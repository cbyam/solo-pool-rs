/// network/session.rs
///
/// Per-miner TCP session state machine.
///
/// Lifecycle:
///   connect → subscribe → authorize → [receive jobs] → [submit shares] → disconnect
///
/// Handles all ASIC extensions:
///   - mining.configure (version-rolling, minimum-difficulty, subscribe-extranonce)
///   - vardiff with async retarget checks
///   - Stale share accounting
///   - Submit latency tracking
///   - Security guards (rate limiting, invalid share counting, message size)
use crate::{
    bitcoin::template::{bits_to_difficulty, StratumJob},
    config::{Config, VardiffConfig},
    error::PoolError,
    metrics,
    mining::{
        engine::{JobBroadcast, TemplateEngine},
        validator::{self, ShareParams, ShareResult, ShareSet},
        vardiff::Vardiff,
    },
    protocol::sv1::{
        AuthorizeParams, ClientMessage, ConfigureParams, ResponseBuilder, StratumRequest,
        SubmitParams, SubscribeParams, SuggestDifficultyParams,
    },
    security::{BanList, SessionGuard},
    stats::PoolStats,
};
use rand::RngCore;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::broadcast,
    task,
};
use tracing::{debug, error, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Session state
// ─────────────────────────────────────────────────────────────────────────────

pub struct Session {
    // Identity
    pub peer: SocketAddr,
    pub worker: Option<String>,
    pub user_agent: Option<String>,
    pub session_id: String,

    // Protocol state
    subscribed: bool,
    authorized: bool,
    version_rolling_enabled: bool,
    version_rolling_mask: u32,
    version_rolling_min_bit_count: Option<u32>,

    // Extranonce
    pub extranonce1: Vec<u8>,
    pub extranonce2_size: usize,

    // Job tracking
    pub current_job: Option<Arc<StratumJob>>,

    // Difficulty & vardiff
    pub difficulty: u64,
    vardiff: Vardiff,
    vardiff_cfg: VardiffConfig,

    // Share dedup
    share_set: ShareSet,

    // Security
    guard: SessionGuard,

    // Stats
    shares_accepted: u64,
    shares_rejected: u64,
    connect_time: Instant,
    stats: Arc<PoolStats>,
}

impl Session {
    pub fn new(
        peer: SocketAddr,
        cfg: &Config,
        extranonce1: Vec<u8>,
        stats: Arc<PoolStats>,
    ) -> Self {
        let initial_diff = cfg.pool.initial_difficulty;
        Self {
            peer,
            worker: None,
            user_agent: None,
            session_id: format!("{:016x}", random_u64()),
            subscribed: false,
            authorized: false,
            version_rolling_enabled: false,
            version_rolling_mask: crate::mining::validator::VERSION_ROLLING_MASK,
            version_rolling_min_bit_count: None,
            extranonce1,
            extranonce2_size: cfg.pool.extranonce2_size,
            current_job: None,
            difficulty: initial_diff,
            vardiff: Vardiff::new(cfg.vardiff.clone(), initial_diff),
            vardiff_cfg: cfg.vardiff.clone(),
            share_set: ShareSet::new(),
            guard: SessionGuard::new(&cfg.security),
            shares_accepted: 0,
            shares_rejected: 0,
            connect_time: Instant::now(),
            stats,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main session loop
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<Config>,
    engine: Arc<TemplateEngine>,
    ban_list: Arc<BanList>,
    stats: Arc<PoolStats>,
) {
    if ban_list.is_banned(&peer.ip()) {
        debug!("Rejected banned IP: {peer}");
        return;
    }

    metrics::miner_connected();
    stats.miner_connected();
    info!("Miner connected: {peer}");

    let extranonce1 = generate_extranonce1(config.pool.extranonce1_size);
    let mut session = Session::new(peer, &config, extranonce1, stats);
    let mut job_rx: tokio::sync::broadcast::Receiver<JobBroadcast> = engine.subscribe();

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line_buf: Vec<u8> = Vec::new();
    let writer = tokio::sync::Mutex::new(writer);

    // Cache current job for later authorize, but do not notify yet.
    if let Some(job) = engine.current_job().await {
        session.current_job = Some(job);
    }

    let idle_timeout = tokio::time::Duration::from_secs(config.pool.idle_timeout_secs);
    let preauth_timeout =
        tokio::time::Duration::from_secs(crate::network::server::HANDSHAKE_TIMEOUT_SECS);
    // Idle tracking must anchor on the last *inbound* message: every completed
    // select! iteration (job broadcasts arrive far more often than the idle
    // timeout) recreates the read-arm future, so a plain timeout() would reset
    // the idle clock and a dead peer that keeps receiving jobs never times out.
    let mut last_inbound = tokio::time::Instant::now();
    // The pre-auth deadline is absolute, measured from connect, not from the
    // last inbound line. Real miners subscribe and authorize within a second.
    // Anchoring it on inbound activity let a peer hold a bounded global slot
    // forever by sending one blank line every few seconds without ever
    // authorizing; enough such peers exhaust max_connections.
    let preauth_deadline = last_inbound + preauth_timeout;

    loop {
        let deadline = if session.authorized {
            last_inbound + idle_timeout
        } else {
            preauth_deadline
        };
        tokio::select! {
            // ── Inbound message from miner ──────────────────────────────────
            line_result = tokio::time::timeout_at(
                deadline,
                read_line_bounded(&mut reader, &mut line_buf, session.guard.max_message_bytes),
            ) => {
                match line_result {
                    Err(_) => {
                        if session.authorized {
                            warn!("Miner {peer} idle timeout — disconnecting");
                        } else {
                            warn!("Miner {peer} did not authorize within {}s — disconnecting", preauth_timeout.as_secs());
                        }
                        break;
                    }
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                        // Line exceeded max_message_bytes mid-read (or invalid UTF-8).
                        warn!("{peer} {e}");
                        ban_list.ban(peer.ip(), "message too large");
                        break;
                    }
                    Ok(Err(e)) => {
                        debug!("Read error from {peer}: {e}");
                        break;
                    }
                    Ok(Ok(None)) => {
                        debug!("Miner {peer} disconnected (EOF)");
                        break;
                    }
                    Ok(Ok(Some(_len))) => {
                        last_inbound = tokio::time::Instant::now();
                        // Take ownership of the completed line and reset the
                        // accumulator here: `read_line_bounded` deliberately does
                        // not clear, so that a line still in flight survives this
                        // `select!` cancelling the read arm.
                        let line = match std::str::from_utf8(&line_buf) {
                            Ok(s) => s.to_owned(),
                            Err(_) => {
                                debug!("Non-UTF8 line from {peer} — disconnecting");
                                break;
                            }
                        };
                        line_buf.clear();

                        tracing::trace!(peer = %peer, raw = %line, "← miner");
                        let response = handle_line(&mut session, &line, &engine).await;

                        match response {
                            HandleResult::Messages(msgs) => {
                                if !send_messages(&writer, peer, msgs).await {
                                    break;
                                }
                            }
                            HandleResult::Disconnect(reason) => {
                                if let Some(worker) = &session.worker {
                                    metrics::miner_disconnect(&reason, worker);
                                }
                                warn!("Disconnecting {peer}: {reason}");
                                break;
                            }
                        }

                        if let Some(new_diff) = session.vardiff.check_retarget() {
                            let old_diff = session.difficulty;
                            session.difficulty = new_diff;
                            if let Some(worker) = &session.worker {
                                metrics::vardiff_retarget(worker, old_diff, new_diff);                                session.stats.update_worker_vardiff(worker, new_diff);                            }
                            let msg = ResponseBuilder::set_difficulty(new_diff);
                            debug!(
                                peer = %session.peer,
                                worker = ?session.worker,
                                difficulty = new_diff,
                                "Sending vardiff update"
                            );
                            if !send_messages(&writer, peer, vec![msg]).await {
                                break;
                            }
                        }

                        let hr_60s  = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(60));
                        let hr_10m  = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(600));
                        let hr_3h   = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(10_800));
                        let hr_24h  = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(86_400));
                        if let Some(worker) = &session.worker {
                            metrics::update_hashrate(hr_10m, worker);
                            session
                                .stats
                                .update_worker_hashrate(worker, hr_60s, hr_10m, hr_3h, hr_24h);
                        }
                    }
                }
            }

            // ── New job broadcast from template engine ──────────────────────
            job_result = job_rx.recv() => {
                match job_result {
                    Ok(JobBroadcast { job, clean }) => {
                        if clean {
                            // A clean job retires every outstanding job; shares
                            // for them are now rejected as stale before dedup,
                            // so the dedup entries have nothing left to guard.
                            session.share_set.clear();
                        }
                        if session.subscribed && session.authorized {
                            let notify = build_notify(&job, clean);
                            session.current_job = Some(job.clone());

                            debug!(
                                peer = %session.peer,
                                worker = ?session.worker,
                                job_id = %job.job_id,
                                difficulty = session.difficulty,
                                clean_jobs = clean,
                                "Sending broadcast mining.notify"
                            );

                            let msgs = vec![
                                ResponseBuilder::set_difficulty(session.difficulty),
                                notify,
                            ];

                            if !send_messages(&writer, peer, msgs).await {
                                break;
                            }

                            metrics::update_job_height(job.height);
                            session.stats.update_height(job.height, job.coinbase_value, job.transactions.len() as u64);

                            if let Ok(net_diff) = bits_to_difficulty(&job.bits) {
                                session.stats.set_network_difficulty(net_diff);
                            }
                        } else {
                            session.current_job = Some(job);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("{peer} missed {n} job broadcasts");
                    }
                    Err(_) => break,
                }
            }
        }
    }

    metrics::miner_disconnected();
    session.stats.miner_disconnected();
    let uptime = session.connect_time.elapsed().as_secs() as f64;
    if let Some(worker) = &session.worker {
        session.stats.mark_worker_offline(worker);
        metrics::connection_duration(worker, uptime);
    }
    info!(
        peer = %peer,
        worker = ?session.worker,
        accepted = session.shares_accepted,
        rejected = session.shares_rejected,
        uptime_secs = uptime,
        "Miner session ended"
    );
}

async fn send_messages(
    writer: &tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
    peer: SocketAddr,
    msgs: Vec<String>,
) -> bool {
    let mut w = writer.lock().await;

    for msg in msgs {
        tracing::trace!(peer = %peer, raw = %msg, "→ pool");
        let line = format!("{msg}\n");
        if let Err(e) = w.write_all(line.as_bytes()).await {
            warn!("Write error to {peer}: {e}");
            return false;
        }
    }

    if let Err(e) = w.flush().await {
        warn!("Flush error to {peer}: {e}");
        return false;
    }

    true
}

/// Read one line (up to but excluding `\n`) into `buf`, enforcing `max` bytes
/// *during* the read. Unlike `AsyncBufReadExt::next_line`, which grows an
/// unbounded buffer until a newline arrives, this aborts the moment the
/// accumulated bytes exceed `max` — so a peer cannot stream gigabytes with no
/// newline and exhaust memory before the size check runs.
///
/// Returns `Ok(None)` on EOF with no buffered data, `Ok(Some(len))` with the
/// line bytes in `buf` (trailing `\r` stripped), or an `InvalidData` error when
/// the line exceeds `max`.
///
/// Cancellation safety: this appends to `buf` and never clears it, because bytes
/// are consumed from the `BufReader` before the newline arrives. The session
/// loop races this against job broadcasts in `select!`, so the future is dropped
/// mid-line routinely; clearing here would discard the consumed prefix and
/// corrupt the next parse. **The caller must clear `buf` once it has taken a
/// completed line**, and must not reuse a buffer across different readers.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<usize>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(buf.len())
            });
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            // Enforce the cap here too: a newline early in a single BufReader
            // chunk reaches this branch without the per-iteration check below,
            // so an oversize line ending within one ~8 KiB read would otherwise
            // slip through.
            if buf.len() > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds max message size",
                ));
            }
            return Ok(Some(buf.len()));
        }
        buf.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds max message size",
            ));
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Message dispatch
// ─────────────────────────────────────────────────────────────────────────────

enum HandleResult {
    Messages(Vec<String>),
    Disconnect(String),
}

async fn handle_line(
    session: &mut Session,
    line: &str,
    engine: &Arc<TemplateEngine>,
) -> HandleResult {
    // Blank / whitespace-only lines (e.g. firmware keepalive newlines) aren't
    // errors — ignore them instead of letting the empty string fail JSON parsing
    // and drop the connection. They cost nothing to handle, so they draw no
    // rate-limit token either: a talkative but honest device must not pay for
    // keepalives. A blank-line flood stays bounded by the pre-auth deadline
    // and the bounded line reader.
    if line.trim().is_empty() {
        return HandleResult::Messages(vec![]);
    }

    // One token per inbound message, not just submits: authorize/configure/
    // subscribe floods are as cheap to send as shares and feed the same
    // per-message stats work, so they share the same bucket. Disconnect, do
    // not ban: a reconnecting miner flushing queued shares after a pool
    // restart trips this too, and the disconnect is already the defense.
    if !session.guard.share_rate.try_consume() {
        metrics::share_rejected("rate_limited", session.worker.as_deref().unwrap_or("?"));
        return HandleResult::Disconnect("rate limited".into());
    }

    let req = match StratumRequest::parse(line) {
        Ok(r) => r,
        Err(e) => {
            // Keep the detailed serde message in the log, but emit a *bounded*
            // disconnect reason for the metric label: the serde error embeds the
            // line/column and varies with input, so using it as a Prometheus
            // label lets untrusted bytes mint unbounded time series.
            debug!("Parse error from {}: {e}", session.peer);
            return HandleResult::Disconnect("parse error".into());
        }
    };

    let msg = match ClientMessage::from_request(&req) {
        Ok(m) => m,
        Err(e) => {
            return HandleResult::Messages(vec![ResponseBuilder::err(
                &req.id,
                e.to_stratum_error(),
            )]);
        }
    };

    match msg {
        ClientMessage::Configure(params) => handle_configure(session, &req, params),
        ClientMessage::Subscribe(params) => handle_subscribe(session, &req, params),
        ClientMessage::Authorize(params) => handle_authorize(session, &req, params, engine).await,
        ClientMessage::Submit(params) => handle_submit(session, &req, params, engine).await,
        ClientMessage::SuggestDifficulty(params) => {
            handle_suggest_difficulty(session, &req, params)
        }
        ClientMessage::Unknown(method) => {
            debug!("Unknown method from {}: {method}", session.peer);
            HandleResult::Messages(vec![ResponseBuilder::err(
                &req.id,
                PoolError::UnknownMethod(method).to_stratum_error(),
            )])
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler implementations
// ─────────────────────────────────────────────────────────────────────────────

fn handle_configure(
    session: &mut Session,
    req: &StratumRequest,
    params: ConfigureParams,
) -> HandleResult {
    if params.version_rolling {
        let negotiated = params
            .version_rolling_mask
            .map(|m| m & session.version_rolling_mask)
            .unwrap_or(session.version_rolling_mask);

        let negotiated_bits = negotiated.count_ones();
        let min_bits_ok = params
            .version_rolling_min_bit_count
            .map(|min_bits| negotiated_bits >= min_bits)
            .unwrap_or(true);

        session.version_rolling_enabled = min_bits_ok && negotiated != 0;
        session.version_rolling_mask = negotiated;
        session.version_rolling_min_bit_count = params.version_rolling_min_bit_count;

        if session.version_rolling_enabled {
            debug!(
                "{} version-rolling enabled, mask={:08x}",
                session.peer, negotiated
            );
        } else {
            debug!(
                "{} version-rolling not enabled: negotiated mask {:08x} does not satisfy requested minimum bit count {:?}",
                session.peer, negotiated, params.version_rolling_min_bit_count
            );
        }
    }

    // Always include minimum-difficulty so that Avalon Nano / cgminer-based firmware
    // knows the pool share threshold. The Nano 3 uses this field to configure its
    // hardware submission threshold; without it, it defaults to network difficulty
    // and never submits pool-level shares.
    let configured_min_diff = Some(session.difficulty);

    HandleResult::Messages(vec![ResponseBuilder::configure(
        &req.id,
        session.version_rolling_enabled,
        session.version_rolling_mask,
        configured_min_diff,
        params.subscribe_extranonce,
    )])
}

fn handle_suggest_difficulty(
    session: &mut Session,
    req: &StratumRequest,
    params: SuggestDifficultyParams,
) -> HandleResult {
    // Seed vardiff from the hint, clamped to the configured floor/ceiling so a
    // (buggy or hostile) suggestion can never drop a miner below the share-rate
    // floor. Vardiff owns the difficulty from here.
    let applied = session.vardiff.suggest(params.difficulty);
    session.difficulty = applied;
    debug!(
        peer = %session.peer,
        suggested = params.difficulty,
        applied,
        floor = session.vardiff_cfg.min_difficulty,
        ceiling = session.vardiff_cfg.max_difficulty,
        "Applied mining.suggest_difficulty"
    );

    let mut msgs = vec![ResponseBuilder::ok(&req.id, serde_json::Value::Bool(true))];
    // If the miner is already receiving work, push the new target now; otherwise
    // the set_difficulty sent during subscribe/authorize will carry the seed.
    if session.subscribed {
        msgs.push(ResponseBuilder::set_difficulty(applied));
    }
    HandleResult::Messages(msgs)
}

fn handle_subscribe(
    session: &mut Session,
    req: &StratumRequest,
    params: SubscribeParams,
) -> HandleResult {
    session.subscribed = true;
    session.user_agent = params.user_agent.clone();
    debug!(
        peer = %session.peer,
        user_agent = ?params.user_agent,
        "Subscribed"
    );

    HandleResult::Messages(vec![ResponseBuilder::subscribe(
        &req.id,
        &session.session_id,
        &hex::encode(&session.extranonce1),
        session.extranonce2_size,
    )])
}

async fn handle_authorize(
    session: &mut Session,
    req: &StratumRequest,
    params: AuthorizeParams,
    engine: &Arc<TemplateEngine>,
) -> HandleResult {
    if !session.subscribed {
        return HandleResult::Messages(vec![ResponseBuilder::err(
            &req.id,
            PoolError::NotSubscribed.to_stratum_error(),
        )]);
    }

    if let Err(e) = session.guard.check_worker_name(&params.worker) {
        warn!(peer = %session.peer, "Rejected worker name: {e}");
        return HandleResult::Messages(vec![ResponseBuilder::err(&req.id, e.to_stratum_error())]);
    }

    // Only a *new* identity counts against the cap or touches the stats maps;
    // re-authorizing the same name (some firmware does on reconnect-in-place)
    // must not inflate active_sessions or the authorization count.
    if session.worker.as_deref() != Some(params.worker.as_str()) {
        if !session.guard.record_new_authorization() {
            return HandleResult::Disconnect("too many worker identities".into());
        }
        if let Some(prev) = session.worker.take() {
            session.stats.mark_worker_offline(&prev);
        }
        session
            .stats
            .mark_worker_online(&params.worker, session.difficulty);
        session.stats.set_worker_protocol(&params.worker, "sv1");
    }
    session.authorized = true;
    session.worker = Some(params.worker.clone());

    info!(
        peer = %session.peer,
        worker = %params.worker,
        "Authorized"
    );

    let mut msgs = Vec::new();
    msgs.push(ResponseBuilder::ok(&req.id, serde_json::Value::Bool(true)));

    let diff_msg = ResponseBuilder::set_difficulty(session.difficulty);
    msgs.push(diff_msg);

    let current_job = if let Some(job) = session.current_job.clone() {
        Some(job)
    } else {
        engine.current_job().await
    };

    if let Some(job) = current_job {
        session.current_job = Some(job.clone());

        debug!(
            peer = %session.peer,
            worker = %params.worker,
            job_id = %job.job_id,
            difficulty = session.difficulty,
            clean_jobs = true,
            "Sending initial mining.notify after authorize"
        );

        msgs.push(build_notify(&job, true));
    } else {
        warn!(
            peer = %session.peer,
            worker = %params.worker,
            "Authorized but no current job available"
        );
    }

    HandleResult::Messages(msgs)
}

async fn handle_submit(
    session: &mut Session,
    req: &StratumRequest,
    params: SubmitParams,
    engine: &Arc<TemplateEngine>,
) -> HandleResult {
    let worker_owned = session.worker.clone().unwrap_or_else(|| "?".to_string());
    let worker: &str = &worker_owned;

    if !session.authorized {
        metrics::share_rejected("unauthorized", worker);
        session.stats.share_rejected();
        return HandleResult::Messages(vec![ResponseBuilder::err(
            &req.id,
            PoolError::NotAuthorized.to_stratum_error(),
        )]);
    }

    // The miner must send exactly the extranonce2 width it was given at
    // subscribe. `assemble_coinbase` returns the template with the extranonce
    // region still zeroed when the widths disagree, which means a wrong-length
    // submit reconstructs a coinbase the miner can predict: one low-difficulty
    // share can then be replayed under unlimited distinct extranonce2 values,
    // each hashing identically, each passing the target check, and each getting
    // a different dedup key. It also turns mis-sized firmware into a stream of
    // "low difficulty" rejects rather than a diagnosable error. The SV2 path
    // already guards this; SV1 did not.
    if params.extranonce2.len() != session.extranonce2_size {
        warn!(
            peer = %session.peer,
            worker = worker,
            got = params.extranonce2.len(),
            expected = session.extranonce2_size,
            "Submit has wrong extranonce2 length — rejecting"
        );
        metrics::share_rejected("bad_extranonce", worker);
        session.stats.share_rejected();
        session
            .stats
            .worker_share_rejected(worker, "bad_extranonce");
        if session.guard.invalid_shares.record_invalid() {
            return HandleResult::Disconnect("too many invalid shares".into());
        }
        return HandleResult::Messages(vec![ResponseBuilder::err(
            &req.id,
            PoolError::BadExtranonceSize {
                got: params.extranonce2.len(),
                expected: session.extranonce2_size,
            }
            .to_stratum_error(),
        )]);
    }

    let submit_start = Instant::now();

    let job_entry = match engine.find_job(&params.job_id).await {
        Some(entry) => entry,
        None => {
            // Not counted toward the invalid-share disconnect counter, for the
            // same reason a superseded job is not: the miner submitted work
            // that simply is not current any more, which is expected rather
            // than malicious. A job that aged out of the history window is if
            // anything more benign than one explicitly retired by a clean job,
            // yet this path used to be the only one of the two that counted —
            // so a miner legitimately continuing on a non-clean job was
            // disconnected after max_invalid_shares (default 5) submissions.
            // The miner already gets the same StaleJob error either way.
            metrics::share_rejected("job_not_found", worker);
            session.stats.share_rejected();
            session.stats.worker_share_rejected(worker, "job_not_found");
            return HandleResult::Messages(vec![ResponseBuilder::err(
                &req.id,
                PoolError::StaleJob(params.job_id.clone()).to_stratum_error(),
            )]);
        }
    };

    let share_params = ShareParams {
        worker: worker.to_string(),
        job_id: params.job_id.clone(),
        extranonce2: params.extranonce2.clone(),
        ntime: params.ntime,
        nonce: params.nonce,
        version_bits: if session.version_rolling_enabled {
            params.version_bits
        } else {
            None
        },
        version_rolling_mask: if session.version_rolling_enabled {
            Some(session.version_rolling_mask)
        } else {
            None
        },
    };

    let extranonce2_hex = hex::encode(&params.extranonce2);
    let ntime_hex = format!("{:08x}", params.ntime);
    let nonce_hex = format!("{:08x}", params.nonce);

    debug!(
        worker = worker,
        job_id = %params.job_id,
        extranonce2 = %extranonce2_hex,
        ntime = %ntime_hex,
        nonce = %nonce_hex,
        version_bits = ?params.version_bits,
        session_version_rolling = session.version_rolling_enabled,
        session_mask = %format!("{:08x}", session.version_rolling_mask),
        "Validating submitted share"
    );

    // Accept any share meeting the configured floor (min_difficulty), not the
    // current vardiff level.  For Avalon/cgminer hardware the hardware threshold
    // is set once via minimum-difficulty at configure time and never changes, so
    // subsequent set_difficulty raises cannot reduce its submission rate.
    // Validating against session.difficulty would only produce spurious rejects.
    let accept_difficulty = session.vardiff_cfg.min_difficulty;

    let validation_start = Instant::now();
    let job_height = job_entry.job.height;
    let extranonce1 = session.extranonce1.clone();
    let job_entry = job_entry.clone();
    let validation = task::spawn_blocking(move || {
        validator::validate_share_no_dedup(
            &share_params,
            &job_entry.job,
            &job_entry,
            &extranonce1,
            accept_difficulty,
        )
    })
    .await;

    let validation_result = match validation {
        Ok(result) => result,
        Err(e) => {
            error!("Share validation task failed: {e}");
            return HandleResult::Disconnect("internal error".into());
        }
    };
    // Dedup on the header hash, known only now. A replay of an accepted
    // share, under any job id or version-bits encoding, becomes
    // DuplicateShare here and is handled by the Err arm below.
    let validation_result = session.share_set.dedup(validation_result);
    match validation_result {
        Ok(ShareResult::Valid {
            assigned_difficulty,
            hash_difficulty,
            hash,
        }) => {
            let validation_duration_ms = validation_start.elapsed().as_millis() as f64;
            metrics::share_validation_time(validation_duration_ms);

            let latency_ms = submit_start.elapsed().as_millis();
            debug!(
                worker = worker,
                job = %params.job_id,
                hash = %validator::hash_display_hex(&hash),
                diff = assigned_difficulty,
                hash_diff = hash_difficulty,
                latency_ms = latency_ms,
                "Share accepted"
            );
            session.shares_accepted += 1;
            let credited = session.vardiff.credit_for(hash_difficulty);
            session.vardiff.record_share(credited);
            metrics::share_accepted(credited, worker);
            session.stats.share_accepted(hash_difficulty);
            session.stats.worker_share_accepted(worker, hash_difficulty);
            session.stats.mark_worker_submit(worker);
            HandleResult::Messages(vec![ResponseBuilder::ok(
                &req.id,
                serde_json::Value::Bool(true),
            )])
        }

        Ok(ShareResult::Block {
            hash_difficulty,
            block_hex,
            hash,
        }) => {
            let validation_duration_ms = validation_start.elapsed().as_millis() as f64;
            metrics::share_validation_time(validation_duration_ms);

            let block_hash_hex = validator::hash_display_hex(&hash);
            let submit_result = engine
                .submit_found_block(
                    job_height,
                    &block_hash_hex,
                    block_hex,
                    worker,
                    session.stats.clone(),
                )
                .await;
            match submit_result {
                Ok(_) => {
                    metrics::block_found();
                    metrics::block_submission_success();
                    session.stats.block_found(worker, &block_hash_hex);
                    session.shares_accepted += 1;
                    let credited = session.vardiff.credit_for(hash_difficulty);
                    session.vardiff.record_share(credited);
                    session.stats.share_accepted(hash_difficulty);
                    session.stats.worker_share_accepted(worker, hash_difficulty);
                    session.stats.mark_worker_submit(worker);
                    info!(
                        "🏆 Block submitted! worker={worker} hash={}",
                        block_hash_hex
                    );
                    HandleResult::Messages(vec![ResponseBuilder::ok(
                        &req.id,
                        serde_json::Value::Bool(true),
                    )])
                }
                Err(e) => {
                    metrics::block_submission_failure(e.submit_failure_label());
                    error!("submitblock failed: {e}");
                    HandleResult::Messages(vec![ResponseBuilder::err(
                        &req.id,
                        e.to_stratum_error(),
                    )])
                }
            }
        }

        Err(e) => {
            let validation_duration_ms = validation_start.elapsed().as_millis() as f64;
            metrics::share_validation_time(validation_duration_ms);

            let reason = match &e {
                PoolError::StaleJob(_) => "stale",
                PoolError::DuplicateShare => "duplicate",
                PoolError::LowDifficulty => "low_difficulty",
                _ => "invalid",
            };
            warn!(
                worker = worker,
                reason = reason,
                job_id = %params.job_id,
                extranonce2 = %hex::encode(&params.extranonce2),
                ntime = %format!("{:08x}", params.ntime),
                nonce = %format!("{:08x}", params.nonce),
                version_bits = ?params.version_bits,
                "Share rejected: {e}"
            );
            metrics::share_rejected(reason, worker);
            session.stats.share_rejected();
            session.stats.worker_share_rejected(worker, reason);
            session.shares_rejected += 1;

            // Low-difficulty shares are expected during vardiff transitions — the miner
            // has in-flight work at the old difficulty. Don't count them as malicious.
            let is_malicious = !matches!(e, PoolError::LowDifficulty | PoolError::StaleJob(_));
            if is_malicious && session.guard.invalid_shares.record_invalid() {
                return HandleResult::Disconnect("too many invalid shares".into());
            }

            HandleResult::Messages(vec![ResponseBuilder::err(&req.id, e.to_stratum_error())])
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────────────────────────────────

fn build_notify(job: &Arc<StratumJob>, clean: bool) -> String {
    ResponseBuilder::notify(
        &job.job_id,
        &job.prev_hash,
        &hex::encode(&job.coinbase1),
        &hex::encode(&job.coinbase2),
        &job.merkle_branch,
        job.version,
        &job.bits,
        job.cur_time,
        clean,
    )
}

static EXTRANONCE1_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn generate_extranonce1(size: usize) -> Vec<u8> {
    let counter = EXTRANONCE1_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .to_be_bytes();
    if size <= counter.len() {
        return counter[counter.len() - size..].to_vec();
    }

    let mut buf = vec![0u8; size];
    let start = size - counter.len();
    buf[start..].copy_from_slice(&counter);
    buf
}

fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::read_line_bounded;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_lines_and_strips_crlf() {
        let data = b"hello\r\nworld\n";
        let mut r = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        // The caller owns clearing between lines (see the cancellation-safety
        // note on `read_line_bounded`).
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 64).await.unwrap(),
            Some(5)
        );
        assert_eq!(&buf, b"hello");
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 64).await.unwrap(),
            Some(5)
        );
        assert_eq!(&buf, b"world");
        buf.clear();
        assert_eq!(read_line_bounded(&mut r, &mut buf, 64).await.unwrap(), None);
    }

    #[tokio::test]
    async fn partial_line_survives_a_cancelled_read() {
        // The session loop runs this inside `select!` against job broadcasts, so
        // the read future is dropped whenever a notify wins the race. A submit
        // split across TCP segments must not lose the bytes already consumed
        // from the BufReader.
        let mock = tokio_test::io::Builder::new()
            .read(b"{\"id\":1,\"me")
            .wait(std::time::Duration::from_millis(50))
            .read(b"thod\":\"mining.submit\"}\n")
            .build();
        let mut r = BufReader::new(mock);
        let mut buf = Vec::new();

        // First attempt: consumes the first segment, then is cancelled while
        // awaiting the rest — exactly what a concurrent job broadcast does.
        tokio::select! {
            biased;
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            _ = read_line_bounded(&mut r, &mut buf, 8192) => {
                panic!("read should still have been pending");
            }
        }

        // Second attempt: must return the whole line, not just the tail.
        let n = read_line_bounded(&mut r, &mut buf, 8192).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            "{\"id\":1,\"method\":\"mining.submit\"}",
            "bytes consumed before cancellation were dropped"
        );
        assert_eq!(n, Some(33));
    }

    #[tokio::test]
    async fn rejects_line_exceeding_max_without_buffering_all() {
        // 10k bytes with no newline; cap is 16. Must error, not buffer everything.
        let data = vec![b'a'; 10_000];
        let mut r = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        let err = read_line_bounded(&mut r, &mut buf, 16).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // The accumulator never grew far past the cap (bounded by the BufReader
        // chunk size), proving we abort mid-stream rather than reading all 10k.
        assert!(
            buf.len() <= 16 + 8192,
            "buffer grew unbounded: {}",
            buf.len()
        );
    }

    #[tokio::test]
    async fn rejects_oversize_line_ending_within_one_chunk() {
        // A newline-terminated line larger than the cap, delivered in a single
        // BufReader chunk so it hits the newline-found branch. Must still error.
        let mut data = vec![b'a'; 100];
        data.push(b'\n');
        let mut r = BufReader::new(&data[..]);
        let mut buf = Vec::new();

        let err = read_line_bounded(&mut r, &mut buf, 16).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn accepts_line_exactly_at_max() {
        let mut data = vec![b'a'; 16];
        data.push(b'\n');
        let mut r = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 16).await.unwrap(),
            Some(16)
        );
    }
}
