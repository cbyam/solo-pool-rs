use crate::metrics;
/// mining/engine.rs
///
/// The TemplateEngine:
///  - Holds the current best StratumJob (updated on each new block)
///  - Broadcasts new jobs to all connected miner sessions
///  - Maintains a job history window for stale-share accounting
///  - Is the single writer to block template state; sessions are read-only
use crate::{
    bitcoin::{rpc::RpcClient, template, zmq::NewBlockReceiver},
    config::PoolConfig,
    error::PoolError,
    settings::RuntimeSettings,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, RwLock},
    task,
};
use tracing::{debug, error, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// How many past jobs to remember for stale-share lookups
const JOB_HISTORY_DEPTH: usize = 8;

/// Channel capacity for new-job broadcasts
const JOB_BROADCAST_CAP: usize = 64;

/// Broadcast payload: the new job plus whether miners should discard current work.
#[derive(Clone, Debug)]
pub struct JobBroadcast {
    pub job: Arc<template::StratumJob>,
    /// true  = new block, miners MUST abandon old work (clean_jobs=true in notify)
    /// false = ntime refresh only, miners MAY continue current work
    pub clean: bool,
}

/// How often to push a new job with refreshed ntime even without a new block.
/// This keeps Avalon/ASIC hardware fed — at 5 TH/s the 32-bit nonce space
/// exhausts in <1ms, so miners need periodic work updates to stay active.
const NTIME_REFRESH_SECS: u64 = 30;

/// In-line submitblock attempts before handing off to the background retrier.
/// Kept small so the miner still gets a timely share response.
const SUBMIT_INLINE_ATTEMPTS: u32 = 3;

/// Background retrier cadence and give-up horizon. Past the deadline the block
/// is almost certainly orphaned, but its hex stays archived on disk either way.
const SUBMIT_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const SUBMIT_RETRY_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct JobEntry {
    pub job: Arc<template::StratumJob>,
    #[allow(dead_code)]
    pub created_at: Instant,
    pub clean: bool, // true = miners should abandon previous work
    pub superseded_by_clean: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// TemplateEngine
// ─────────────────────────────────────────────────────────────────────────────

pub struct TemplateEngine {
    rpc: Arc<RpcClient>,
    pool_cfg: PoolConfig,
    /// Runtime-mutable settings (payout address); read on every refresh so a
    /// dashboard-driven change applies to the next job built.
    settings: Arc<RuntimeSettings>,

    /// Current best job (Arc so sessions can hold a cheap reference)
    current_job: RwLock<Option<Arc<template::StratumJob>>>,

    /// Circular history for stale-share lookups: job_id → JobEntry
    job_history: RwLock<VecDeque<JobEntry>>,

    /// Broadcast channel — sessions subscribe on connect
    job_tx: broadcast::Sender<JobBroadcast>,
}

impl TemplateEngine {
    pub fn new(
        rpc: Arc<RpcClient>,
        pool_cfg: PoolConfig,
        settings: Arc<RuntimeSettings>,
    ) -> Arc<Self> {
        let (job_tx, _) = broadcast::channel(JOB_BROADCAST_CAP);
        Arc::new(Self {
            rpc,
            pool_cfg,
            settings,
            current_job: RwLock::new(None),
            job_history: RwLock::new(VecDeque::with_capacity(JOB_HISTORY_DEPTH)),
            job_tx,
        })
    }

    /// Build and broadcast a fresh clean job immediately — used after a
    /// settings change so miners switch to the new payout address without
    /// waiting for the next block or ntime tick.
    pub async fn force_refresh(&self) {
        self.refresh(true).await;
    }

    /// Subscribe to new-job broadcasts. Call this when a miner session connects.
    pub fn subscribe(&self) -> broadcast::Receiver<JobBroadcast> {
        self.job_tx.subscribe()
    }

    /// Return the current best job, if any.
    pub async fn current_job(&self) -> Option<Arc<template::StratumJob>> {
        self.current_job.read().await.clone()
    }

    /// Look up a job by ID (for stale-share accounting).
    pub async fn find_job(&self, job_id: &str) -> Option<JobEntry> {
        let history = self.job_history.read().await;
        let idx = history.iter().position(|e| e.job.job_id == job_id)?;
        let mut entry = history.get(idx)?.clone();
        entry.superseded_by_clean = history.iter().skip(idx + 1).any(|e| e.clean);
        Some(entry)
    }

    /// Main loop: refresh the template whenever a new block arrives,
    /// and periodically push a ntime-updated job to keep ASIC hardware active.
    pub async fn run(self: Arc<Self>, mut new_block: NewBlockReceiver) {
        // Do an immediate fetch on startup
        self.refresh(true).await;

        let mut ntime_tick = tokio::time::interval(Duration::from_secs(NTIME_REFRESH_SECS));
        ntime_tick.tick().await; // discard the immediate first tick

        loop {
            tokio::select! {
                result = new_block.changed() => {
                    if result.is_err() {
                        warn!("New-block channel closed; stopping template engine");
                        break;
                    }
                    // New block: full GBT refresh, miners must abandon old work
                    self.refresh(true).await;
                    // Reset the ntime timer so we don't send a redundant notify
                    // right after the block notify
                    ntime_tick.reset();
                }
                _ = ntime_tick.tick() => {
                    // Periodic ntime refresh: new job_id + current wall-clock time,
                    // but miners may keep working on current nonce ranges (clean=false)
                    self.refresh(false).await;
                }
            }
        }
    }

    /// Fetch a fresh GBT and push it out to all connected sessions.
    async fn refresh(&self, clean_jobs: bool) {
        // Hard safety gate: never build a job unless the payout address
        // validates against the node's chain. A wrong-network address would
        // still produce a *valid* coinbase script (the script encodes no
        // network), silently mining to a script the operator may not control.
        let Some(coinbase_address) = self.settings.valid_coinbase_address() else {
            warn!(
                network = %self.settings.network(),
                "Mining paused: no valid payout address for the node's network — \
                 set one in the dashboard Settings page"
            );
            return;
        };

        match self.rpc.get_block_template() {
            Ok(gbt) => {
                match template::build_job(
                    &gbt,
                    &coinbase_address,
                    &self.pool_cfg.coinbase_tag,
                    self.pool_cfg.extranonce1_size,
                    self.pool_cfg.extranonce2_size,
                ) {
                    Ok(job) => {
                        let job = Arc::new(job);
                        debug!(
                            height = job.height,
                            job_id = %job.job_id,
                            bits = %job.bits,
                            "New job built"
                        );

                        // Update current job
                        *self.current_job.write().await = Some(job.clone());

                        // Push into history
                        let mut history = self.job_history.write().await;
                        if history.len() >= JOB_HISTORY_DEPTH {
                            history.pop_front();
                        }
                        history.push_back(JobEntry {
                            job: job.clone(),
                            created_at: Instant::now(),
                            clean: clean_jobs,
                            superseded_by_clean: false,
                        });
                        drop(history);

                        // Broadcast — ignore "no receivers" errors (normal before first miner)
                        let receiver_count = self.job_tx.receiver_count();
                        let _ = self.job_tx.send(JobBroadcast {
                            job,
                            clean: clean_jobs,
                        });
                        metrics::job_broadcast(receiver_count);
                    }
                    Err(e) => error!("Failed to build job: {e}"),
                }
            }
            Err(e) => error!("getblocktemplate failed: {e}"),
        }
    }

    /// Submit a found block, guaranteeing it cannot be silently lost: the raw
    /// hex is archived to disk in parallel with the first attempt, transient
    /// RPC failures are retried in-line a few times, and if those fail a
    /// detached background task keeps retrying while the caller reports the
    /// failure.
    pub async fn submit_found_block(
        self: &Arc<Self>,
        height: u64,
        hash_hex: &str,
        block_hex: String,
        worker: &str,
        stats: Arc<crate::stats::PoolStats>,
    ) -> Result<(), PoolError> {
        let block_hex = Arc::new(block_hex);

        // Archive concurrently on a blocking thread — submission is in a race
        // against the rest of the network and must not wait on disk; the
        // archive only matters if submission fails, and it still lands within
        // milliseconds of the submit going out.
        {
            let dir = self.pool_cfg.found_block_dir.clone();
            let hash = hash_hex.to_owned();
            let hex = block_hex.clone();
            task::spawn_blocking(move || archive_found_block(&dir, height, &hash, &hex));
        }

        let mut last_err = None;
        for attempt in 1..=SUBMIT_INLINE_ATTEMPTS {
            match self.try_submit(block_hex.clone()).await {
                Ok(()) => return Ok(()),
                Err(e) if is_permanent_reject(&e) => return Err(e),
                Err(e) => {
                    warn!(
                        "submitblock attempt {attempt}/{SUBMIT_INLINE_ATTEMPTS} \
                         failed for block {hash_hex} (height {height}): {e}"
                    );
                    last_err = Some(e);
                }
            }
            if attempt < SUBMIT_INLINE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
        }

        self.spawn_resubmit_task(
            height,
            hash_hex.to_owned(),
            block_hex,
            worker.to_owned(),
            stats,
        );
        Err(last_err
            .unwrap_or_else(|| PoolError::Other(anyhow::anyhow!("submitblock never attempted"))))
    }

    /// One submitblock attempt, off the async runtime (the RPC client blocks).
    async fn try_submit(&self, block_hex: Arc<String>) -> Result<(), PoolError> {
        let rpc = self.rpc.clone();
        task::spawn_blocking(move || rpc.submit_block(&block_hex))
            .await
            .map_err(|e| PoolError::Other(anyhow::anyhow!("submitblock task panicked: {e}")))?
    }

    /// Keep resubmitting a found block in the background after the in-line
    /// attempts failed — e.g. while bitcoind restarts. `submit_block` treats
    /// "duplicate" as success, so racing an earlier attempt is harmless.
    fn spawn_resubmit_task(
        self: &Arc<Self>,
        height: u64,
        hash_hex: String,
        block_hex: Arc<String>,
        worker: String,
        stats: Arc<crate::stats::PoolStats>,
    ) {
        let engine = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + SUBMIT_RETRY_DEADLINE;
            let mut attempt = SUBMIT_INLINE_ATTEMPTS;
            while Instant::now() < deadline {
                tokio::time::sleep(SUBMIT_RETRY_INTERVAL).await;
                attempt += 1;
                match engine.try_submit(block_hex.clone()).await {
                    Ok(()) => {
                        metrics::block_found();
                        metrics::block_submission_success();
                        // Mirror the inline-success path so the dashboard's
                        // block count / last-block panel agree with Prometheus.
                        stats.block_found(&worker, &hash_hex);
                        info!(
                            "🏆 Block {hash_hex} (height {height}) accepted on \
                             retry attempt {attempt}"
                        );
                        return;
                    }
                    Err(e) if is_permanent_reject(&e) => {
                        error!(
                            "Block {hash_hex} (height {height}) permanently \
                             rejected on retry attempt {attempt}: {e}"
                        );
                        return;
                    }
                    Err(e) => {
                        warn!(
                            "submitblock retry attempt {attempt} for block \
                             {hash_hex} (height {height}) failed: {e}"
                        );
                    }
                }
            }
            error!(
                "Giving up resubmitting block {hash_hex} (height {height}) after {:?}; \
                 its hex remains archived in {} — submit manually with \
                 `bitcoin-cli submitblock`",
                SUBMIT_RETRY_DEADLINE, engine.pool_cfg.found_block_dir
            );
        });
    }
}

/// A consensus-level rejection: the block itself is invalid or outdated, so
/// resubmitting the same bytes can never succeed. Everything else (transport
/// errors, node restarting, unexpected responses) is worth retrying.
fn is_permanent_reject(e: &PoolError) -> bool {
    matches!(e, PoolError::SubmitBlockRejected(_))
}

/// Write the block hex to `<found_block_dir>/block_<height>_<hash>.hex` so the
/// block survives a crash or node outage and can be replayed by hand. Runs on
/// a blocking thread in parallel with submission. Failure is loud but
/// non-fatal — submission proceeds regardless.
fn archive_found_block(dir: &str, height: u64, hash_hex: &str, block_hex: &str) -> Option<PathBuf> {
    let dir = Path::new(dir);
    let path = dir.join(format!("block_{height}_{hash_hex}.hex"));
    let res = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, block_hex));
    match res {
        Ok(()) => {
            info!("Archived found block to {}", path.display());
            Some(path)
        }
        Err(e) => {
            error!(
                "Failed to archive found block {hash_hex} to {}: {e}",
                path.display()
            );
            None
        }
    }
}
