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
    bitcoin::template::StratumJob,
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
        SubmitParams, SubscribeParams,
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::broadcast,
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
    let mut lines = BufReader::new(reader).lines();
    let writer = tokio::sync::Mutex::new(writer);

    // Cache current job for later authorize, but do not notify yet.
    if let Some(job) = engine.current_job().await {
        session.current_job = Some(job);
    }

    let idle_timeout = tokio::time::Duration::from_secs(config.pool.idle_timeout_secs);

    loop {
        tokio::select! {
            // ── Inbound message from miner ──────────────────────────────────
            line_result = tokio::time::timeout(idle_timeout, lines.next_line()) => {
                match line_result {
                    Err(_) => {
                        warn!("Miner {peer} idle timeout — disconnecting");
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
                    Ok(Ok(Some(line))) => {
                        if let Err(e) = session.guard.check_message_size(line.len()) {
                            warn!("{peer} {e}");
                            ban_list.ban(peer.ip(), "message too large");
                            break;
                        }

                        tracing::trace!(peer = %peer, raw = %line, "← miner");
                        let response = handle_line(&mut session, &line, &engine, &ban_list).await;

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
                                metrics::vardiff_retarget(worker, old_diff, new_diff);
                            }
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

                        let hr_60s = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(60));
                        let hr_3h = session.vardiff.estimated_hashrate_in_window(std::time::Duration::from_secs(10800));
                        let hr_5m = session.vardiff.estimated_hashrate();
                        if let Some(worker) = &session.worker {
                            metrics::update_hashrate(hr_5m, worker);
                            session
                                .stats
                                .update_worker_hashrate(worker, hr_60s, hr_3h, hr_5m);
                        }
                    }
                }
            }

            // ── New job broadcast from template engine ──────────────────────
            job_result = job_rx.recv() => {
                match job_result {
                    Ok(JobBroadcast { job, clean }) => {
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
                            session.stats.update_height(job.height);
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
    ban_list: &Arc<BanList>,
) -> HandleResult {
    let req = match StratumRequest::parse(line) {
        Ok(r) => r,
        Err(e) => {
            debug!("Parse error from {}: {e}", session.peer);
            return HandleResult::Disconnect(format!("parse error: {e}"));
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
        ClientMessage::Submit(params) => {
            if !session.guard.share_rate.try_consume() {
                metrics::share_rejected("rate_limited", session.worker.as_deref().unwrap_or("?"));
                ban_list.ban(session.peer.ip(), "share rate exceeded");
                return HandleResult::Disconnect("rate limited".into());
            }
            handle_submit(session, &req, params, engine).await
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
    let worker = session.worker.as_deref().unwrap_or("?");

    if !session.authorized {
        metrics::share_rejected("unauthorized", worker);
        session.stats.share_rejected();
        return HandleResult::Messages(vec![ResponseBuilder::err(
            &req.id,
            PoolError::NotAuthorized.to_stratum_error(),
        )]);
    }

    let submit_start = Instant::now();

    let job_entry = match engine.find_job(&params.job_id).await {
        Some(entry) => entry,
        None => {
            metrics::share_rejected("job_not_found", worker);
            session.stats.share_rejected();
            if session.guard.invalid_shares.record_invalid() {
                return HandleResult::Disconnect("too many invalid shares".into());
            }
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

    debug!(
        worker = worker,
        job_id = %params.job_id,
        extranonce2 = %hex::encode(&params.extranonce2),
        ntime = %format!("{:08x}", params.ntime),
        nonce = %format!("{:08x}", params.nonce),
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
    match validator::validate_share(
        &share_params,
        &job_entry.job,
        &job_entry,
        &session.extranonce1,
        accept_difficulty,
        &mut session.share_set,
    ) {
        Ok(ShareResult::Valid { difficulty, hash }) => {
            let validation_duration_ms = validation_start.elapsed().as_millis() as f64;
            metrics::share_validation_time(validation_duration_ms);

            let latency_ms = submit_start.elapsed().as_millis();
            debug!(
                worker = worker,
                job = %params.job_id,
                hash = %hex::encode(hash),
                diff = difficulty,
                latency_ms = latency_ms,
                "Share accepted"
            );
            session.shares_accepted += 1;
            session.vardiff.record_share(session.difficulty);
            metrics::share_accepted(difficulty, worker);
            session.stats.share_accepted(difficulty);
            session.stats.mark_worker_submit(worker);
            HandleResult::Messages(vec![ResponseBuilder::ok(
                &req.id,
                serde_json::Value::Bool(true),
            )])
        }

        Ok(ShareResult::Block { block_hex, hash }) => {
            let validation_duration_ms = validation_start.elapsed().as_millis() as f64;
            metrics::share_validation_time(validation_duration_ms);

            let submit_result = engine.submit_block(&block_hex);
            match submit_result {
                Ok(_) => {
                    metrics::block_found();
                    metrics::block_submission_success();
                    session.stats.block_found();
                    session.shares_accepted += 1;
                    session.vardiff.record_share(session.difficulty);
                    session.stats.mark_worker_submit(worker);
                    info!(
                        "🏆 Block submitted! worker={worker} hash={}",
                        hex::encode(hash)
                    );
                    HandleResult::Messages(vec![ResponseBuilder::ok(
                        &req.id,
                        serde_json::Value::Bool(true),
                    )])
                }
                Err(e) => {
                    metrics::block_submission_failure(&format!("{:?}", e));
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

fn generate_extranonce1(size: usize) -> Vec<u8> {
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
