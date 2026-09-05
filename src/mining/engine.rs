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
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
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

/// How many past jobs to remember for stale-share lookups.
///
/// Each entry pins a whole `StratumJob`, including `transactions` — the full
/// raw tx data needed to assemble a block if a winning share arrives against
/// that job. On mainnet that is megabytes per entry, so this is a memory knob
/// as much as a correctness one, and it must stay small enough for the
/// single-board machines this pool targets.
///
/// At `NTIME_REFRESH_SECS` this covers roughly the last four minutes. Work
/// older than that is rejected as job-not-found, which is accounted as a stale
/// share and deliberately does NOT count toward the invalid-share disconnect
/// counter (see `handle_submit`) — that accounting, not the window size, was
/// what made an aged-out job disconnect an otherwise healthy miner.
const JOB_HISTORY_DEPTH: usize = 8;

/// Channel capacity for new-job broadcasts
const JOB_BROADCAST_CAP: usize = 64;

/// Whether the chain tip moved between the job being replaced and the new one.
///
/// A prevhash change must retire outstanding work even when the caller asked for
/// a non-clean refresh. The periodic ntime tick calls `refresh(false)`, so a tip
/// change first noticed there — a missed or late ZMQ signal — would otherwise go
/// out as "you may keep your current work". Miners would keep grinding the dead
/// prevhash, their shares would keep validating against the retired job, and a
/// block found on it would be rejected by the node.
///
/// No current job (first refresh after boot) counts as moved: there is no
/// outstanding work to preserve, and a clean job is the correct opening state.
fn tip_moved(current_prev_hash: Option<&str>, new_prev_hash: &str) -> bool {
    current_prev_hash != Some(new_prev_hash)
}

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

    /// Inline `submitblock` attempts currently in progress. Shutdown waits
    /// for this to drain so a block found seconds before `systemctl stop`
    /// still reaches the node.
    inflight_submits: AtomicUsize,
}

/// RAII count of an in-progress inline submission.
struct InflightSubmit<'a>(&'a AtomicUsize);

impl<'a> InflightSubmit<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InflightSubmit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
            inflight_submits: AtomicUsize::new(0),
        })
    }

    /// Wait up to `timeout` for inline block submissions to finish. Called on
    /// shutdown; returns whether the count reached zero.
    pub async fn wait_for_inflight_submits(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.inflight_submits.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Resubmit every archived block whose acceptance was never confirmed.
    ///
    /// `submit_found_block` archives the hex before the first attempt and
    /// moves the file to `submitted/` once the node has it. Anything still at
    /// the top level when the pool boots is a block whose confirmation this
    /// process never saw: a crash mid-submit, a restart while the two-hour
    /// retrier was still trying, or a node that was down. `submitblock` on a
    /// block the node already has answers "duplicate", which counts as
    /// success, so the replay can be dumb and safe. Blocks the node rejects
    /// outright move to `rejected/` so they are not retried forever.
    ///
    /// Replayed blocks are not credited to the dashboard's block counter:
    /// most will be duplicates of blocks that were counted before the
    /// restart, and the counter is in-memory only.
    pub async fn replay_archived_blocks(self: &Arc<Self>) {
        let dir = self.pool_cfg.found_block_dir.clone();
        let pending = {
            let dir = dir.clone();
            task::spawn_blocking(move || list_archived_blocks(&dir))
                .await
                .unwrap_or_default()
        };
        if pending.is_empty() {
            return;
        }
        warn!(
            "{} archived block(s) in {dir} were never confirmed submitted; replaying",
            pending.len()
        );
        for path in pending {
            let (height, hash_hex) =
                parse_archive_name(&path).unwrap_or_else(|| (0, path.display().to_string()));
            let block_hex = match std::fs::read_to_string(&path) {
                Ok(h) => Arc::new(h.trim().to_string()),
                Err(e) => {
                    error!("Cannot read archived block {}: {e}", path.display());
                    continue;
                }
            };
            match self.try_submit(block_hex.clone()).await {
                Ok(()) => {
                    info!(
                        "Replayed archived block {hash_hex} (height {height}): \
                         node has it"
                    );
                    file_archived_block(&dir, &path, "submitted");
                }
                Err(e) if is_permanent_reject(&e) || is_node_side_error(&e) => {
                    error!(
                        "Archived block {hash_hex} (height {height}) rejected on \
                         replay: {e}; filing under rejected/"
                    );
                    file_archived_block(&dir, &path, "rejected");
                }
                Err(e) => {
                    warn!(
                        "Replay of archived block {hash_hex} (height {height}) \
                         failed: {e}; retrying in the background"
                    );
                    self.spawn_resubmit_task(height, hash_hex, block_hex, None);
                }
            }
        }
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

                        // Update current job. The write guard is held across the
                        // compare-and-set so two concurrent refreshes cannot both
                        // conclude the tip is unchanged.
                        let mut current = self.current_job.write().await;
                        let tip_changed = tip_moved(
                            current.as_ref().map(|c| c.prev_hash.as_str()),
                            &job.prev_hash,
                        );
                        let clean_jobs = clean_jobs || tip_changed;
                        *current = Some(job.clone());
                        drop(current);

                        if tip_changed {
                            info!(
                                height = job.height,
                                prev_hash = %job.prev_hash,
                                "Chain tip changed — broadcasting clean job"
                            );
                        }

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

        let _inflight = InflightSubmit::new(&self.inflight_submits);
        let mut last_err = None;
        for attempt in 1..=SUBMIT_INLINE_ATTEMPTS {
            match self.try_submit(block_hex.clone()).await {
                Ok(()) => {
                    // After the submit, never before it: the archive file is
                    // moved aside so a later boot does not replay it.
                    self.mark_archived_submitted(height, hash_hex);
                    return Ok(());
                }
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
            Some((worker.to_owned(), stats)),
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

    /// Move the archive file for a confirmed block into `submitted/` on a
    /// blocking thread. Never awaited on the block-found path.
    fn mark_archived_submitted(&self, height: u64, hash_hex: &str) {
        let dir = self.pool_cfg.found_block_dir.clone();
        let path = archive_path(&dir, height, hash_hex);
        task::spawn_blocking(move || {
            // The archive write runs on its own blocking thread and can land a
            // moment after a fast submit; give it a beat before giving up. A
            // file that is still missed here is simply replayed on the next
            // boot, where the node answers "duplicate".
            if !path.exists() {
                std::thread::sleep(Duration::from_secs(1));
            }
            if path.exists() {
                file_archived_block(&dir, &path, "submitted");
            }
        });
    }

    /// Keep resubmitting a found block in the background after the in-line
    /// attempts failed — e.g. while bitcoind restarts. `submit_block` treats
    /// "duplicate" as success, so racing an earlier attempt is harmless.
    /// `credit` is the worker and stats to credit on success; `None` for a
    /// boot-time replay, which must not count a block twice.
    fn spawn_resubmit_task(
        self: &Arc<Self>,
        height: u64,
        hash_hex: String,
        block_hex: Arc<String>,
        credit: Option<(String, Arc<crate::stats::PoolStats>)>,
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
                        engine.mark_archived_submitted(height, &hash_hex);
                        if let Some((worker, stats)) = &credit {
                            metrics::block_found();
                            metrics::block_submission_success();
                            // Mirror the inline-success path so the dashboard's
                            // block count / last-block panel agree with Prometheus.
                            stats.block_found(worker, &hash_hex);
                        }
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

/// The node answered with a JSON-RPC error object (for example -22, block
/// decode failed) rather than the call failing in transport. Only the
/// boot-time replay treats this as permanent: hex the pool built itself never
/// fails to decode, but a truncated or hand-edited archive file would, and
/// retrying it for two hours on every boot helps nobody.
fn is_node_side_error(e: &PoolError) -> bool {
    matches!(
        e,
        PoolError::Rpc(bitcoincore_rpc::Error::JsonRpc(
            bitcoincore_rpc::jsonrpc::Error::Rpc(_)
        ))
    )
}

/// `<found_block_dir>/block_<height>_<hash>.hex`
fn archive_path(dir: &str, height: u64, hash_hex: &str) -> PathBuf {
    Path::new(dir).join(format!("block_{height}_{hash_hex}.hex"))
}

/// Recover `(height, hash)` from an archive file name; `None` if the name
/// was not written by `archive_path`.
fn parse_archive_name(path: &Path) -> Option<(u64, String)> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".hex")?;
    let rest = stem.strip_prefix("block_")?;
    let (height, hash) = rest.split_once('_')?;
    let height = height.parse().ok()?;
    (hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| (height, hash.to_string()))
}

/// Archive files at the top level of `dir`: blocks whose submission was never
/// confirmed. `submitted/` and `rejected/` hold the ones already dealt with.
fn list_archived_blocks(dir: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "hex"))
        .collect();
    paths.sort();
    paths
}

/// Move an archive file into `<dir>/<bucket>/`. Failure is logged; the file
/// stays where it is and is retried on the next boot.
fn file_archived_block(dir: &str, path: &Path, bucket: &str) {
    let target_dir = Path::new(dir).join(bucket);
    let Some(name) = path.file_name() else { return };
    let target = target_dir.join(name);
    if let Err(e) =
        std::fs::create_dir_all(&target_dir).and_then(|_| std::fs::rename(path, &target))
    {
        warn!(
            "Could not move archived block {} to {}: {e}",
            path.display(),
            target.display()
        );
    }
}

/// Write the block hex to `<found_block_dir>/block_<height>_<hash>.hex` so the
/// block survives a crash or node outage and is replayed on the next boot if
/// its acceptance is never confirmed. Runs on a blocking thread in parallel
/// with submission. Failure is loud but non-fatal — submission proceeds
/// regardless.
fn archive_found_block(dir: &str, height: u64, hash_hex: &str, block_hex: &str) -> Option<PathBuf> {
    let path = archive_path(dir, height, hash_hex);
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

#[cfg(test)]
mod tests {
    use super::{
        archive_found_block, archive_path, file_archived_block, list_archived_blocks,
        parse_archive_name, tip_moved,
    };

    const H: &str = "00000000000000000001b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f607";

    fn temp_dir(name: &str) -> String {
        let d =
            std::env::temp_dir().join(format!("solo-pool-archive-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn archive_names_round_trip() {
        let p = archive_path("d", 961_633, H);
        assert_eq!(parse_archive_name(&p), Some((961_633, H.to_string())));
        for bad in [
            "notes.txt",
            "block_x_y.hex",
            "block_1_abc.hex",
            "1_hash.hex",
        ] {
            assert_eq!(parse_archive_name(std::path::Path::new(bad)), None, "{bad}");
        }
    }

    #[test]
    fn only_unconfirmed_archives_are_listed_for_replay() {
        // A boot sees the top-level files only; a block filed under
        // submitted/ or rejected/ is done and must never be replayed.
        let dir = temp_dir("list");
        let a = archive_found_block(&dir, 1, H, "aa").unwrap();
        let b = archive_found_block(&dir, 2, H, "bb").unwrap();
        std::fs::write(std::path::Path::new(&dir).join("README"), "not a block").unwrap();
        assert_eq!(list_archived_blocks(&dir), vec![a.clone(), b.clone()]);

        file_archived_block(&dir, &a, "submitted");
        file_archived_block(&dir, &b, "rejected");
        assert!(list_archived_blocks(&dir).is_empty());
        assert!(std::path::Path::new(&dir)
            .join("submitted")
            .join(a.file_name().unwrap())
            .exists());
        assert_eq!(
            std::fs::read_to_string(
                std::path::Path::new(&dir)
                    .join("rejected")
                    .join(b.file_name().unwrap())
            )
            .unwrap(),
            "bb"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_a_missing_directory_is_empty_not_fatal() {
        assert!(list_archived_blocks("/nonexistent/solo-pool-archive").is_empty());
    }

    const A: &str = "00000000000000000001b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f607";
    const B: &str = "00000000000000000002c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718";

    #[test]
    fn tip_change_forces_a_clean_job_even_on_a_non_clean_refresh() {
        // The 30s ntime tick calls refresh(false). If ZMQ missed the block, this
        // is the only thing standing between miners and a dead prevhash.
        assert!(tip_moved(Some(A), B), "a moved tip must retire old work");
        assert!(!tip_moved(Some(A), A), "an unchanged tip must not");
    }

    /// Mirrors how `refresh` combines the caller's request with the tip check.
    fn clean_for(requested: bool, current: Option<&str>, new: &str) -> bool {
        requested || tip_moved(current, new)
    }

    #[test]
    fn clean_escalation_is_one_way() {
        // The ntime tick asks for a non-clean refresh; a tip change overrides it.
        assert!(clean_for(false, Some(A), B), "ntime tick must escalate");
        // A steady tip on the ntime tick stays non-clean, so miners keep working.
        assert!(
            !clean_for(false, Some(A), A),
            "steady tip must not escalate"
        );
        // An explicit clean request is never downgraded.
        assert!(clean_for(true, Some(A), A), "explicit clean is preserved");
    }

    #[test]
    fn first_job_after_boot_is_clean() {
        assert!(tip_moved(None, A));
    }
}
