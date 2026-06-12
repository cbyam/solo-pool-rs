//! protocol/sv2 — Stratum V2 (Extended Channel) frontend.
//!
//! A protocol-agnostic mining core already exists: [`TemplateEngine`] broadcasts
//! a [`StratumJob`], and [`validator`] reconstructs/validates the header. This
//! module is a second *frontend* over that core, speaking the SV2 mining
//! protocol to devices such as the NerdQAxe++ (AxeOS ≥ v1.0.37).
//!
//! The transport is Noise-encrypted (see [`noise`]) — the pool is the responder.
//! Devices require this; they will not speak plaintext SV2.
//!
//! Lifecycle: Noise handshake → `SetupConnection` → `OpenExtendedMiningChannel`
//! → jobs pushed via `NewExtendedMiningJob` + `SetNewPrevHash` → shares via
//! `SubmitSharesExtended`.
//!
//! SV1 is served by [`crate::network::session`]; the two are auto-detected on a
//! single port in [`crate::network::server`].
mod job;
mod messages;
mod noise;

use crate::{
    bitcoin::template::{bits_to_difficulty, StratumJob},
    config::{Config, VardiffConfig},
    metrics,
    mining::{
        engine::{JobBroadcast, TemplateEngine},
        validator::{self, ShareParams, ShareResult, ShareSet, VERSION_ROLLING_MASK},
        vardiff::Vardiff,
    },
    security::{BanList, SessionGuard},
    stats::PoolStats,
};
use const_sv2::{
    MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH, MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
    MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCES,
    MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR, MESSAGE_TYPE_SETUP_CONNECTION,
    MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, MESSAGE_TYPE_SET_TARGET,
    MESSAGE_TYPE_SUBMIT_SHARES_ERROR, MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
    MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::broadcast, task};
use tracing::{debug, error, info, warn};

use noise::NoiseWriter;

/// Highest SV2 protocol version this pool implements.
const SV2_PROTOCOL_VERSION: u16 = 2;

/// Channel-id allocator (unique per process; channel scope is per-connection).
static CHANNEL_ID: AtomicU32 = AtomicU32::new(1);

// ─────────────────────────────────────────────────────────────────────────────
// Session state
// ─────────────────────────────────────────────────────────────────────────────

struct Sv2Session {
    peer: SocketAddr,
    worker: Option<String>,

    setup_done: bool,
    channel_open: bool,
    channel_id: u32,

    /// Pool-assigned extranonce prefix (length chosen at channel open).
    extranonce_prefix: Vec<u8>,
    /// Bytes the device contributes (granted at channel open from its request).
    extranonce_size: usize,
    /// Total reserved extranonce width in the coinbase (prefix + device bytes).
    extranonce_total: usize,

    difficulty: u64,
    vardiff: Vardiff,
    vardiff_cfg: VardiffConfig,

    share_set: ShareSet,
    guard: SessionGuard,

    /// SV2 job_id (u32) → engine `StratumJob.job_id` (String) for stale lookups.
    job_ids: VecDeque<(u32, String)>,
    next_job_id: u32,
    /// Most recent job broadcast (sent once the channel opens).
    pending_job: Option<Arc<StratumJob>>,

    shares_accepted: u64,
    shares_rejected: u64,
    connect_time: Instant,
    stats: Arc<PoolStats>,
}

impl Sv2Session {
    fn new(
        peer: SocketAddr,
        cfg: &Config,
        extranonce_prefix: Vec<u8>,
        stats: Arc<PoolStats>,
    ) -> Self {
        let initial_diff = cfg.pool.initial_difficulty;
        Self {
            peer,
            worker: None,
            setup_done: false,
            channel_open: false,
            channel_id: 0,
            extranonce_prefix,
            extranonce_size: cfg.pool.extranonce2_size,
            extranonce_total: cfg.pool.extranonce1_size + cfg.pool.extranonce2_size,
            difficulty: initial_diff,
            vardiff: Vardiff::new(cfg.vardiff.clone(), initial_diff),
            vardiff_cfg: cfg.vardiff.clone(),
            share_set: ShareSet::new(),
            guard: SessionGuard::new(&cfg.security),
            job_ids: VecDeque::new(),
            next_job_id: 1,
            pending_job: None,
            shares_accepted: 0,
            shares_rejected: 0,
            connect_time: Instant::now(),
            stats,
        }
    }

    /// Allocate an SV2 job_id and remember its engine job-id mapping.
    fn assign_job_id(&mut self, engine_job_id: String) -> u32 {
        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.wrapping_add(1);
        if self.next_job_id == 0 {
            self.next_job_id = 1;
        }
        self.job_ids.push_back((id, engine_job_id));
        while self.job_ids.len() > 16 {
            self.job_ids.pop_front();
        }
        id
    }

    fn engine_job_id(&self, sv2_job_id: u32) -> Option<String> {
        self.job_ids
            .iter()
            .find(|(j, _)| *j == sv2_job_id)
            .map(|(_, s)| s.clone())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Session entry point
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

    info!("SV2 miner connected: {peer}");

    // ── Noise handshake (pool = responder) ───────────────────────────────────
    // Bounded by the short handshake deadline so a peer cannot hold the
    // connection (and its bounded global slot) open mid-handshake — the
    // session-loop idle timeout only starts once we reach transport mode.
    let mut stream = stream;
    let handshake_timeout = Duration::from_secs(crate::network::server::HANDSHAKE_TIMEOUT_SECS);
    let state = match tokio::time::timeout(
        handshake_timeout,
        noise::responder_handshake(&mut stream),
    )
    .await
    {
        Err(_) => {
            warn!("SV2 {peer} Noise handshake timed out");
            return;
        }
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            warn!("SV2 {peer} Noise handshake failed: {e}");
            return;
        }
    };

    // Only count the miner once the secure channel is established.
    metrics::miner_connected();
    stats.miner_connected();

    let extranonce_prefix =
        crate::network::session::generate_extranonce1(config.pool.extranonce1_size);
    let mut session = Sv2Session::new(peer, &config, extranonce_prefix, stats);
    let mut job_rx: broadcast::Receiver<JobBroadcast> = engine.subscribe();

    // Shared cipher state; reader (own task) decrypts, writer (this task) encrypts.
    let state = Arc::new(tokio::sync::Mutex::new(state));
    let (reader_half, writer_half) = stream.into_split();
    // Allow the configured message size plus SV2 framing + Noise AEAD overhead.
    let max_frame = config.security.max_message_bytes.saturating_add(1024);
    let mut nreader = noise::NoiseReader::new(reader_half, state.clone(), max_frame);
    let mut writer = NoiseWriter::new(writer_half, state, peer);

    // read_exact into the codec buffer is not cancel-safe, so frames are read in
    // a dedicated task and forwarded over a channel the main loop can select on.
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(32);
    let reader_task = tokio::spawn(async move {
        loop {
            match nreader.read().await {
                Ok(msg) => {
                    if inbound_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!("SV2 reader ended: {e}");
                    break;
                }
            }
        }
    });

    // Cache the current job so we can push it as soon as the channel opens.
    if let Some(job) = engine.current_job().await {
        session.pending_job = Some(job);
    }

    let idle_timeout = Duration::from_secs(config.pool.idle_timeout_secs);
    let preauth_timeout = Duration::from_secs(crate::network::server::HANDSHAKE_TIMEOUT_SECS);

    loop {
        // Until the channel opens (worker identified), hold the connection to
        // the short handshake deadline — mirrors the SV1 pre-auth timeout.
        let read_timeout = if session.channel_open {
            idle_timeout
        } else {
            preauth_timeout
        };
        tokio::select! {
            // ── Inbound (decrypted) SV2 message ─────────────────────────────
            inbound = tokio::time::timeout(read_timeout, inbound_rx.recv()) => {
                let (msg_type, mut payload) = match inbound {
                    Err(_) => { warn!("SV2 miner {peer} idle timeout — disconnecting"); break; }
                    Ok(None) => { debug!("SV2 {peer} reader closed"); break; }
                    Ok(Some(m)) => m,
                };

                if let Err(e) = session.guard.check_message_size(payload.len()) {
                    warn!("{peer} {e}");
                    ban_list.ban(peer.ip(), "message too large");
                    break;
                }
                tracing::trace!(peer = %peer, msg_type, len = payload.len(), "← sv2 miner");

                match handle_message(&mut session, &mut writer, msg_type, &mut payload, &engine, &ban_list).await {
                    Flow::Continue => {}
                    Flow::Disconnect(reason) => {
                        if let Some(worker) = &session.worker {
                            metrics::miner_disconnect(&reason, worker);
                        }
                        warn!("Disconnecting SV2 {peer}: {reason}");
                        break;
                    }
                }

                // Vardiff retarget → SetTarget
                if session.channel_open {
                    if let Some(new_diff) = session.vardiff.check_retarget() {
                        let old_diff = session.difficulty;
                        session.difficulty = new_diff;
                        if let Some(worker) = &session.worker {
                            metrics::vardiff_retarget(worker, old_diff, new_diff);
                            session.stats.update_worker_vardiff(worker, new_diff);
                        }
                        let target = job::difficulty_to_sv2_target(new_diff);
                        debug!(peer = %peer, worker = ?session.worker, difficulty = new_diff, "Sending SV2 set_target");
                        match messages::set_target(session.channel_id, target) {
                            Ok(p) => if !writer.send(MESSAGE_TYPE_SET_TARGET, true, &p).await { break; },
                            Err(e) => { error!("encode set_target: {e}"); break; }
                        }
                    }

                    // Worker hashrate stats (mirrors SV1 cadence)
                    let hr_60s = session.vardiff.estimated_hashrate_in_window(Duration::from_secs(60));
                    let hr_10m = session.vardiff.estimated_hashrate_in_window(Duration::from_secs(600));
                    let hr_3h  = session.vardiff.estimated_hashrate_in_window(Duration::from_secs(10_800));
                    let hr_24h = session.vardiff.estimated_hashrate_in_window(Duration::from_secs(86_400));
                    if let Some(worker) = &session.worker {
                        metrics::update_hashrate(hr_10m, worker);
                        session.stats.update_worker_hashrate(worker, hr_60s, hr_10m, hr_3h, hr_24h);
                    }
                }
            }

            // ── New job broadcast from the template engine ──────────────────
            job_result = job_rx.recv() => {
                match job_result {
                    Ok(JobBroadcast { job, clean }) => {
                        if session.channel_open {
                            // clean (new block): future-job + SetNewPrevHash.
                            // ntime refresh: immediate job on the existing prev-hash.
                            if !send_job(&mut session, &mut writer, &job, clean, peer).await {
                                break;
                            }
                            metrics::update_job_height(job.height);
                            session.stats.update_height(job.height, job.coinbase_value, job.transactions.len() as u64);
                            if let Ok(net_diff) = bits_to_difficulty(&job.bits) {
                                session.stats.set_network_difficulty(net_diff);
                            }
                        }
                        session.pending_job = Some(job);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => warn!("SV2 {peer} missed {n} job broadcasts"),
                    Err(_) => break,
                }
            }
        }
    }

    reader_task.abort();
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
        "SV2 miner session ended"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Message dispatch
// ─────────────────────────────────────────────────────────────────────────────

enum Flow {
    Continue,
    Disconnect(String),
}

async fn handle_message(
    session: &mut Sv2Session,
    writer: &mut NoiseWriter,
    msg_type: u8,
    payload: &mut [u8],
    engine: &Arc<TemplateEngine>,
    ban_list: &Arc<BanList>,
) -> Flow {
    // One token per inbound frame, not just submits: setup/open-channel floods
    // are as cheap to send as shares and feed the same per-message stats work,
    // so they share the same bucket (mirrors the SV1 dispatch).
    if !session.guard.share_rate.try_consume() {
        metrics::share_rejected("rate_limited", session.worker.as_deref().unwrap_or("?"));
        ban_list.ban(session.peer.ip(), "message rate exceeded");
        return Flow::Disconnect("rate limited".into());
    }

    match msg_type {
        MESSAGE_TYPE_SETUP_CONNECTION => handle_setup_connection(session, writer, payload).await,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL => {
            handle_open_extended(session, writer, payload).await
        }
        MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED => {
            handle_submit(session, writer, payload, engine).await
        }
        other => {
            // UpdateChannel / CloseChannel / etc. — not required for plaintext
            // extended-channel mining; log and continue.
            debug!(peer = %session.peer, msg_type = other, "Ignoring unhandled SV2 message");
            Flow::Continue
        }
    }
}

async fn handle_setup_connection(
    session: &mut Sv2Session,
    writer: &mut NoiseWriter,
    payload: &mut [u8],
) -> Flow {
    let setup = match messages::decode_setup_connection(payload) {
        Ok(s) => s,
        Err(e) => return Flow::Disconnect(format!("bad SetupConnection: {e}")),
    };
    if setup.min_version > SV2_PROTOCOL_VERSION {
        return Flow::Disconnect(format!(
            "unsupported SV2 version range {}..{}",
            setup.min_version, setup.max_version
        ));
    }
    let used_version = SV2_PROTOCOL_VERSION.min(setup.max_version);
    session.setup_done = true;
    debug!(peer = %session.peer, used_version, "SV2 SetupConnection");

    match messages::setup_connection_success(used_version) {
        Ok(p) => {
            if !writer
                .send(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, false, &p)
                .await
            {
                return Flow::Disconnect("write".into());
            }
            Flow::Continue
        }
        Err(e) => Flow::Disconnect(format!("encode SetupConnectionSuccess: {e}")),
    }
}

async fn handle_open_extended(
    session: &mut Sv2Session,
    writer: &mut NoiseWriter,
    payload: &mut [u8],
) -> Flow {
    if !session.setup_done {
        return Flow::Disconnect("OpenExtendedMiningChannel before SetupConnection".into());
    }
    let open = match messages::decode_open_extended(payload) {
        Ok(o) => o,
        Err(e) => return Flow::Disconnect(format!("bad OpenExtendedMiningChannel: {e}")),
    };

    if let Err(e) = session.guard.check_worker_name(&open.user_identity) {
        return Flow::Disconnect(format!("invalid user_identity: {e}"));
    }

    // Only a *new* identity counts against the cap or touches the stats maps
    // (mirrors the SV1 authorize path).
    let is_new_identity = session.worker.as_deref() != Some(open.user_identity.as_str());
    if is_new_identity {
        if !session.guard.record_new_authorization() {
            return Flow::Disconnect("too many worker identities".into());
        }
        if let Some(prev) = session.worker.take() {
            session.stats.mark_worker_offline(&prev);
        }
    }

    // Grant the device its requested extranonce out of the coinbase's reserved
    // total; the remaining bytes become the pool prefix. This is independent of
    // the SV1 split, so SV1 miners (e.g. the Avalon Nano) keep their smaller
    // extranonce2 while an SV2 device gets the larger size it needs.
    let requested = open.min_extranonce_size as usize;
    if requested > session.extranonce_total {
        warn!(
            peer = %session.peer,
            requested,
            total = session.extranonce_total,
            "SV2 min_extranonce_size exceeds total reserved extranonce — rejecting; \
             raise [pool] extranonce sizes so extranonce1_size + extranonce2_size >= {requested}"
        );
        match messages::open_channel_error_extranonce(open.request_id) {
            Ok(p) => {
                let _ = writer
                    .send(MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR, false, &p)
                    .await;
            }
            Err(e) => error!("encode OpenMiningChannelError: {e}"),
        }
        return Flow::Continue;
    }

    // Grant exactly what the device asked (min 1), leaving the rest as prefix.
    let granted = requested.max(1);
    let prefix_len = session.extranonce_total - granted;
    session.extranonce_size = granted;
    session.extranonce_prefix = crate::network::session::generate_extranonce1(prefix_len);
    debug!(
        peer = %session.peer,
        granted,
        prefix_len,
        "SV2 extranonce split"
    );

    let channel_id = CHANNEL_ID.fetch_add(1, Ordering::Relaxed);
    session.channel_id = channel_id;
    session.worker = Some(open.user_identity.clone());

    // Target from current difficulty, clamped to be no easier than the device's
    // declared `max_target` (SV2: assigned target MUST be ≤ max_target).
    let mut target = job::difficulty_to_sv2_target(session.difficulty);
    if !job::sv2_target_le(&target, &open.max_target) {
        warn!(
            peer = %session.peer,
            "SV2 device max_target easier than initial difficulty target; clamping to max_target"
        );
        target = open.max_target;
    }

    if is_new_identity {
        session
            .stats
            .mark_worker_online(&open.user_identity, session.difficulty);
        session
            .stats
            .set_worker_protocol(&open.user_identity, "sv2");
    }
    info!(peer = %session.peer, worker = %open.user_identity, channel_id, "SV2 extended channel opened");

    match messages::open_extended_success(
        open.request_id,
        channel_id,
        target,
        session.extranonce_size as u16,
        session.extranonce_prefix.clone(),
    ) {
        Ok(p) => {
            if !writer
                .send(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCES, false, &p)
                .await
            {
                return Flow::Disconnect("write".into());
            }
        }
        Err(e) => return Flow::Disconnect(format!("encode OpenExtendedMiningChannelSuccess: {e}")),
    }

    session.channel_open = true;

    // Push the current job immediately (future-job + SetNewPrevHash).
    if let Some(job) = session.pending_job.clone() {
        if !send_job(session, writer, &job, true, session.peer).await {
            return Flow::Disconnect("write".into());
        }
    } else {
        warn!(peer = %session.peer, "SV2 channel opened but no job available yet");
    }

    Flow::Continue
}

async fn handle_submit(
    session: &mut Sv2Session,
    writer: &mut NoiseWriter,
    payload: &mut [u8],
    engine: &Arc<TemplateEngine>,
) -> Flow {
    if !session.channel_open {
        return Flow::Disconnect("SubmitShares before channel open".into());
    }
    let submit = match messages::decode_submit_extended(payload) {
        Ok(s) => s,
        Err(e) => return Flow::Disconnect(format!("bad SubmitSharesExtended: {e}")),
    };
    let worker = session.worker.clone().unwrap_or_else(|| "?".to_string());

    // The device must send exactly the granted extranonce size; the coinbase
    // splice depends on it. Guard so a malformed length is a clean reject, not a
    // panic in the validation task.
    if submit.extranonce.len() != session.extranonce_size {
        warn!(
            worker = %worker,
            got = submit.extranonce.len(),
            expected = session.extranonce_size,
            "SV2 submit has wrong extranonce length — rejecting"
        );
        metrics::share_rejected("bad_extranonce", &worker);
        session.stats.share_rejected();
        if session.guard.invalid_shares.record_invalid() {
            return Flow::Disconnect("too many invalid shares".into());
        }
        return reject(
            session,
            writer,
            submit.sequence_number,
            "bad-extranonce-size",
        )
        .await;
    }

    let submit_start = Instant::now();

    // Map SV2 job_id → engine job-id → JobEntry (stale handling as in SV1).
    let job_entry = match session.engine_job_id(submit.job_id) {
        Some(engine_id) => engine.find_job(&engine_id).await,
        None => None,
    };
    let job_entry = match job_entry {
        Some(e) => e,
        None => {
            metrics::share_rejected("job_not_found", &worker);
            session.stats.share_rejected();
            if session.guard.invalid_shares.record_invalid() {
                return Flow::Disconnect("too many invalid shares".into());
            }
            return reject(session, writer, submit.sequence_number, "stale-job").await;
        }
    };

    let mask = VERSION_ROLLING_MASK;
    let share_params = ShareParams {
        worker: worker.clone(),
        job_id: job_entry.job.job_id.clone(),
        extranonce2: submit.extranonce.clone(),
        ntime: submit.ntime,
        nonce: submit.nonce,
        // The device sends the full version; pass only the masked (rolled) bits.
        version_bits: Some(submit.version & mask),
        version_rolling_mask: Some(mask),
    };

    // Duplicate detection (same key as SV1).
    if session.share_set.check_and_insert(
        &share_params.job_id,
        &submit.extranonce,
        submit.ntime,
        submit.nonce,
        submit.version & mask,
    ) {
        metrics::share_rejected("duplicate", &worker);
        session.stats.share_rejected();
        session.stats.worker_share_rejected(&worker);
        if session.guard.invalid_shares.record_invalid() {
            return Flow::Disconnect("too many invalid shares".into());
        }
        return reject(session, writer, submit.sequence_number, "duplicate-share").await;
    }

    // Accept any share meeting the configured floor — matches SV1 policy so a
    // fixed hardware submission threshold isn't penalised by vardiff raises.
    let accept_difficulty = session.vardiff_cfg.min_difficulty;

    let validation_start = Instant::now();
    let extranonce1 = session.extranonce_prefix.clone();
    let share_set = std::mem::take(&mut session.share_set);
    let job_entry_cloned = job_entry.clone();
    let validation = task::spawn_blocking(move || {
        let result = validator::validate_share_no_dedup(
            &share_params,
            &job_entry_cloned.job,
            &job_entry_cloned,
            &extranonce1,
            accept_difficulty,
        );
        (share_set, result)
    })
    .await;

    let validation_result = match validation {
        Ok((share_set, result)) => {
            session.share_set = share_set;
            result
        }
        Err(e) => {
            session.share_set = ShareSet::new();
            error!("SV2 share validation task failed: {e}");
            return Flow::Disconnect("internal error".into());
        }
    };

    match validation_result {
        Ok(ShareResult::Valid {
            assigned_difficulty,
            hash_difficulty,
            hash,
        }) => {
            metrics::share_validation_time(validation_start.elapsed().as_millis() as f64);
            debug!(
                worker = %worker, hash = %hex::encode(hash), diff = assigned_difficulty,
                hash_diff = hash_difficulty, latency_ms = submit_start.elapsed().as_millis(),
                "SV2 share accepted"
            );
            session.shares_accepted += 1;
            session.vardiff.record_share(session.difficulty);
            metrics::share_accepted(assigned_difficulty, &worker);
            session.stats.share_accepted(hash_difficulty);
            session
                .stats
                .worker_share_accepted(&worker, hash_difficulty);
            session.stats.mark_worker_submit(&worker);
            accept(session, writer, submit.sequence_number).await
        }

        Ok(ShareResult::Block {
            hash_difficulty,
            block_hex,
            hash,
        }) => {
            metrics::share_validation_time(validation_start.elapsed().as_millis() as f64);
            let block_hash_hex = hex::encode(hash);
            match engine
                .submit_found_block(job_entry.job.height, &block_hash_hex, block_hex)
                .await
            {
                Ok(_) => {
                    metrics::block_found();
                    metrics::block_submission_success();
                    session.stats.block_found(&worker, &hex::encode(hash));
                    session.shares_accepted += 1;
                    session.vardiff.record_share(session.difficulty);
                    session.stats.share_accepted(hash_difficulty);
                    session
                        .stats
                        .worker_share_accepted(&worker, hash_difficulty);
                    session.stats.mark_worker_submit(&worker);
                    info!(
                        "🏆 Block submitted (SV2)! worker={worker} hash={}",
                        hex::encode(hash)
                    );
                    accept(session, writer, submit.sequence_number).await
                }
                Err(e) => {
                    metrics::block_submission_failure(e.submit_failure_label());
                    error!("SV2 submitblock failed: {e}");
                    reject(
                        session,
                        writer,
                        submit.sequence_number,
                        "block-submit-failed",
                    )
                    .await
                }
            }
        }

        Err(e) => {
            metrics::share_validation_time(validation_start.elapsed().as_millis() as f64);
            let reason = match &e {
                crate::error::PoolError::StaleJob(_) => "stale",
                crate::error::PoolError::DuplicateShare => "duplicate",
                crate::error::PoolError::LowDifficulty => "low_difficulty",
                _ => "invalid",
            };
            warn!(worker = %worker, reason, nonce = %format!("{:08x}", submit.nonce), "SV2 share rejected: {e}");
            metrics::share_rejected(reason, &worker);
            session.stats.share_rejected();
            session.stats.worker_share_rejected(&worker);
            if let crate::error::PoolError::StaleJob(_) = e {
                session.stats.worker_share_stale(&worker);
            }
            session.shares_rejected += 1;

            let is_malicious = !matches!(
                e,
                crate::error::PoolError::LowDifficulty | crate::error::PoolError::StaleJob(_)
            );
            if is_malicious && session.guard.invalid_shares.record_invalid() {
                return Flow::Disconnect("too many invalid shares".into());
            }
            reject(session, writer, submit.sequence_number, reason).await
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Outbound helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Send a job to the device. When `future` (new block / initial), announce a
/// future `NewExtendedMiningJob` then a `SetNewPrevHash` that activates it;
/// otherwise send an immediate job on the current prev-hash (ntime refresh).
async fn send_job(
    session: &mut Sv2Session,
    writer: &mut NoiseWriter,
    job: &Arc<StratumJob>,
    future: bool,
    peer: SocketAddr,
) -> bool {
    let sv2_job_id = session.assign_job_id(job.job_id.clone());

    let new_job = match job::build_new_extended_job(job, session.channel_id, sv2_job_id, future) {
        Ok(j) => j,
        Err(e) => {
            error!("build NewExtendedMiningJob: {e}");
            return false;
        }
    };
    let payload = match messages::encode(new_job) {
        Ok(p) => p,
        Err(e) => {
            error!("encode NewExtendedMiningJob: {e}");
            return false;
        }
    };
    debug!(peer = %peer, sv2_job_id, future, clean = future, "Sending SV2 NewExtendedMiningJob");
    if !writer
        .send(MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB, true, &payload)
        .await
    {
        return false;
    }

    if future {
        let snph = match job::build_set_new_prev_hash(job, session.channel_id, sv2_job_id) {
            Ok(s) => s,
            Err(e) => {
                error!("build SetNewPrevHash: {e}");
                return false;
            }
        };
        let payload = match messages::encode(snph) {
            Ok(p) => p,
            Err(e) => {
                error!("encode SetNewPrevHash: {e}");
                return false;
            }
        };
        if !writer
            .send(MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH, true, &payload)
            .await
        {
            return false;
        }
    }
    true
}

async fn accept(session: &Sv2Session, writer: &mut NoiseWriter, seq: u32) -> Flow {
    match messages::submit_shares_success(session.channel_id, seq, session.difficulty) {
        Ok(p) => {
            if !writer
                .send(MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS, true, &p)
                .await
            {
                return Flow::Disconnect("write".into());
            }
            Flow::Continue
        }
        Err(e) => Flow::Disconnect(format!("encode SubmitSharesSuccess: {e}")),
    }
}

async fn reject(session: &Sv2Session, writer: &mut NoiseWriter, seq: u32, code: &str) -> Flow {
    match messages::submit_shares_error(session.channel_id, seq, code) {
        Ok(p) => {
            if !writer
                .send(MESSAGE_TYPE_SUBMIT_SHARES_ERROR, true, &p)
                .await
            {
                return Flow::Disconnect("write".into());
            }
            Flow::Continue
        }
        Err(e) => Flow::Disconnect(format!("encode SubmitSharesError: {e}")),
    }
}
