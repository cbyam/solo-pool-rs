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
};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, warn};

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

    /// Current best job (Arc so sessions can hold a cheap reference)
    current_job: RwLock<Option<Arc<template::StratumJob>>>,

    /// Circular history for stale-share lookups: job_id → JobEntry
    job_history: RwLock<VecDeque<JobEntry>>,

    /// Broadcast channel — sessions subscribe on connect
    job_tx: broadcast::Sender<JobBroadcast>,
}

impl TemplateEngine {
    pub fn new(rpc: Arc<RpcClient>, pool_cfg: PoolConfig) -> Arc<Self> {
        let (job_tx, _) = broadcast::channel(JOB_BROADCAST_CAP);
        Arc::new(Self {
            rpc,
            pool_cfg,
            current_job: RwLock::new(None),
            job_history: RwLock::new(VecDeque::with_capacity(JOB_HISTORY_DEPTH)),
            job_tx,
        })
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
        match self.rpc.get_block_template() {
            Ok(gbt) => {
                match template::build_job(
                    &gbt,
                    &self.pool_cfg.coinbase_address,
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

    /// Submit a complete block to Bitcoin Core.
    pub fn submit_block(&self, block_hex: &str) -> Result<(), PoolError> {
        self.rpc.submit_block(block_hex)
    }
}
