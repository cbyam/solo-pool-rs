/// stats.rs
///
/// In-process pool statistics — updated by session tasks, read by the dashboard.
///
/// Uses atomics and DashMap so updates are lock-free from any async task.
use dashmap::DashMap;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
use tracing::{info, warn};

/// Offline workers idle longer than this are evicted from the in-memory stats
/// maps (their persisted best share survives, subject to the cap below). Keeps
/// per-message stats work and dashboard payloads bounded against connections
/// that mint many distinct worker names.
const IDLE_WORKER_EVICT_SECS: u64 = 86_400;

/// Maximum rows kept in `worker_best_shares` (in memory and in SQLite), keeping
/// the highest difficulties. Bounds boot-time load and dashboard growth; a solo
/// operator's real fleet is far below this.
const MAX_WORKER_BEST_SHARES: usize = 512;

/// The trailing windows (seconds) behind the 60s / 10m / 3h / 24h hashrate
/// estimates. Must match the windows the session loops pass to
/// `Vardiff::estimated_hashrate_in_window`.
const HASHRATE_WINDOW_60S: u64 = 60;
const HASHRATE_WINDOW_10M: u64 = 600;
const HASHRATE_WINDOW_3H: u64 = 10_800;
const HASHRATE_WINDOW_24H: u64 = 86_400;

/// Fraction of a trailing window still covered by an estimate computed
/// `elapsed` seconds ago, assuming shares were spread evenly across it.
/// Reaches 0.0 once the whole window postdates the estimate.
fn window_overlap(elapsed: u64, window_secs: u64) -> f64 {
    (1.0 - elapsed as f64 / window_secs as f64).max(0.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistent store for all-time metrics
// ─────────────────────────────────────────────────────────────────────────────

/// Make the stats database readable by the pool's user only. SQLite creates
/// it under the process umask, typically 0644, and it holds the payout
/// address and per-worker history; nothing but this process needs to read it.
#[cfg(unix)]
fn restrict_to_owner(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        warn!("Could not restrict permissions on {path}: {e}");
    }
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &str) {}

struct StatsStore {
    conn: Mutex<Connection>,
}

impl StatsStore {
    fn open(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        restrict_to_owner(path);
        // An operator poking at the file with the sqlite3 CLI holds a write
        // lock for the length of their transaction. Without this, every pool
        // write in that window fails immediately with SQLITE_BUSY (logged,
        // not fatal, but a lost best-share update is still lost). Wait a
        // little instead.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS pool_stats (
             id INTEGER PRIMARY KEY CHECK(id = 1),
             best_share_difficulty INTEGER NOT NULL,
             best_hashrate_hps REAL NOT NULL
             )",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO pool_stats (id, best_share_difficulty, best_hashrate_hps)
             VALUES (1, 0, 0.0)",
            [],
        )?;

        // Round accounting lives on the single pool_stats row. Existing
        // databases predate these columns, so add them in place; SQLite has
        // no ADD COLUMN IF NOT EXISTS, and a duplicate-column error is the
        // expected outcome on every boot after the first.
        for ddl in [
            "ALTER TABLE pool_stats ADD COLUMN round_work REAL NOT NULL DEFAULT 0",
            "ALTER TABLE pool_stats ADD COLUMN round_start_ts INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(e) = conn.execute(ddl, []) {
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
            }
        }

        conn.execute(
            "CREATE TABLE IF NOT EXISTS worker_best_shares (
             worker TEXT PRIMARY KEY,
             best_share_difficulty INTEGER NOT NULL
             )",
            [],
        )?;

        // One row per block this pool found, in the order found. The hash
        // is the key so a block credited twice (inline success racing the
        // background retrier) cannot be counted twice. round_work and
        // network_difficulty are captured at the moment the round closed,
        // so the effort of a past round survives without recomputation.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS found_blocks (
             hash TEXT PRIMARY KEY,
             height INTEGER NOT NULL,
             worker TEXT NOT NULL,
             ts INTEGER NOT NULL,
             round_work REAL NOT NULL,
             network_difficulty REAL NOT NULL
             )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS hashrate_history (
             ts INTEGER PRIMARY KEY,
             hashrate_hps REAL NOT NULL
             )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS settings (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
             )",
            [],
        )?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        // Enforce the row cap at boot so an attacker-inflated table from a
        // previous run is trimmed before load_values pulls it into RAM.
        store.prune_worker_best_shares(MAX_WORKER_BEST_SHARES);
        Ok(store)
    }

    fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }

    fn set_setting(&self, key: &str, value: &str) -> bool {
        match self.conn.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        ) {
            Ok(_) => true,
            Err(e) => {
                warn!("Failed to persist setting {key}: {e}");
                false
            }
        }
    }

    /// Keep only the `keep` highest-difficulty rows in `worker_best_shares`.
    fn prune_worker_best_shares(&self, keep: usize) {
        match self.conn.lock().execute(
            "DELETE FROM worker_best_shares WHERE worker NOT IN (
               SELECT worker FROM worker_best_shares
               ORDER BY best_share_difficulty DESC LIMIT ?1
             )",
            params![keep as i64],
        ) {
            Ok(0) => {}
            Ok(n) => info!("Pruned {n} stale worker_best_shares rows (cap {keep})"),
            Err(e) => warn!("Failed to prune worker_best_shares: {e}"),
        }
    }

    fn load_values(&self) -> Result<LoadedStats, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT best_share_difficulty, best_hashrate_hps, round_work, round_start_ts
             FROM pool_stats WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        let mut loaded = LoadedStats::default();
        if let Some(row) = rows.next()? {
            loaded.best_share_difficulty = row.get::<_, u64>(0)?;
            loaded.best_hashrate_hps = row.get::<_, f64>(1)?;
            loaded.round_work = row.get::<_, f64>(2)?;
            loaded.round_start_ts = row.get::<_, u64>(3)?;
        }

        let mut stmt =
            conn.prepare("SELECT worker, best_share_difficulty FROM worker_best_shares")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let worker = row.get::<_, String>(0)?;
            let difficulty = row.get::<_, u64>(1)?;
            loaded.worker_best_shares.insert(worker, difficulty);
        }

        let mut stmt = conn.prepare(
            "SELECT hash, height, worker, ts, round_work, network_difficulty
             FROM found_blocks ORDER BY ts ASC, height ASC",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            loaded.found_blocks.push(FoundBlock {
                hash: row.get(0)?,
                height: row.get(1)?,
                worker: row.get(2)?,
                ts: row.get(3)?,
                round_work: row.get(4)?,
                network_difficulty: row.get(5)?,
            });
        }

        Ok(loaded)
    }

    fn set_round(&self, round_work: f64, round_start_ts: u64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET round_work = ?1, round_start_ts = ?2 WHERE id = 1",
            params![round_work, round_start_ts],
        ) {
            warn!("Failed to persist round state: {e}");
        }
    }

    fn insert_found_block(&self, block: &FoundBlock) {
        if let Err(e) = self.conn.lock().execute(
            "INSERT OR IGNORE INTO found_blocks
             (hash, height, worker, ts, round_work, network_difficulty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                block.hash,
                block.height,
                block.worker,
                block.ts,
                block.round_work,
                block.network_difficulty
            ],
        ) {
            warn!("Failed to persist found block {}: {e}", block.hash);
        }
    }

    // The `?1 > ...` guards (matching set_worker_best_share) make the writes
    // monotonic at the SQL level: two racing writers can call these out of
    // order, and a stale lower value must not overwrite a higher one already
    // persisted.
    fn set_best_share_difficulty(&self, difficulty: u64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET best_share_difficulty = ?1
             WHERE id = 1 AND ?1 > best_share_difficulty",
            params![difficulty],
        ) {
            warn!("Failed to persist best_share_difficulty: {e}");
        }
    }

    fn set_best_hashrate_hps(&self, hps: f64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET best_hashrate_hps = ?1
             WHERE id = 1 AND ?1 > best_hashrate_hps",
            params![hps],
        ) {
            warn!("Failed to persist best_hashrate_hps: {e}");
        }
    }

    /// Force the persisted watermark to `hps`, ignoring the monotonic guard on
    /// `set_best_hashrate_hps`. Only for an operator-initiated reset: a
    /// watermark recorded from a bad estimate can never be undone otherwise,
    /// because every normal write path refuses to lower it.
    fn force_best_hashrate_hps(&self, hps: f64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET best_hashrate_hps = ?1 WHERE id = 1",
            params![hps],
        ) {
            warn!("Failed to reset best_hashrate_hps: {e}");
        }
    }

    fn record_hashrate_snapshot(&self, ts: u64, hps: f64) {
        let conn = self.conn.lock();
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO hashrate_history (ts, hashrate_hps) VALUES (?1, ?2)",
            params![ts, hps],
        ) {
            warn!("Failed to record hashrate snapshot: {e}");
            return;
        }
        // Prune entries older than 6 months
        let cutoff = ts.saturating_sub(6 * 30 * 24 * 3600);
        let _ = conn.execute(
            "DELETE FROM hashrate_history WHERE ts < ?1",
            params![cutoff],
        );
    }

    fn get_hashrate_history(&self, since_ts: u64) -> Vec<(u64, f64)> {
        let conn = self.conn.lock();
        let mut stmt = match conn
            .prepare("SELECT ts, hashrate_hps FROM hashrate_history WHERE ts >= ?1 ORDER BY ts ASC")
        {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![since_ts], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, f64>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    fn set_worker_best_share(&self, worker: &str, difficulty: u64) {
        if let Err(e) = self.conn.lock().execute(
            "INSERT INTO worker_best_shares (worker, best_share_difficulty) VALUES (?1, ?2)
             ON CONFLICT(worker) DO UPDATE SET best_share_difficulty = excluded.best_share_difficulty
             WHERE excluded.best_share_difficulty > worker_best_shares.best_share_difficulty",
            params![worker, difficulty],
        ) {
            warn!("Failed to persist worker_best_share for {worker}: {e}");
        }
    }
}

/// Everything the store hands back at boot.
#[derive(Default)]
struct LoadedStats {
    best_share_difficulty: u64,
    best_hashrate_hps: f64,
    worker_best_shares: std::collections::HashMap<String, u64>,
    round_work: f64,
    round_start_ts: u64,
    found_blocks: Vec<FoundBlock>,
}

/// A block this pool found, with the round it closed.
#[derive(Clone, Debug, Serialize)]
pub struct FoundBlock {
    pub height: u64,
    pub hash: String,
    pub worker: String,
    pub ts: u64,
    /// Difficulty-work submitted in the round this block closed (the sum of
    /// credited share difficulties since the previous block, or since the
    /// pool first ran).
    pub round_work: f64,
    /// Network difficulty when the block was found; `round_work` over this
    /// is the round's effort, 1.0 being average luck.
    pub network_difficulty: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// PoolStats
// ─────────────────────────────────────────────────────────────────────────────

pub struct PoolStats {
    pub shares_accepted: AtomicU64,
    pub shares_rejected: AtomicU64,
    pub blocks_found: AtomicU64,
    pub connected_miners: AtomicU64,
    pub current_height: AtomicU64,
    pub current_coinbase_value: AtomicU64,
    pub current_block_transaction_count: AtomicU64,
    pub best_share_difficulty: AtomicU64,
    pub session_best_share_difficulty: AtomicU64,
    pub best_hashrate_hps: AtomicU64,
    pub session_best_hashrate_hps: AtomicU64,
    pub network_hashrate_hps: AtomicU64,
    pub network_difficulty: AtomicU64,
    /// Estimated difficulty change (%) at the next retarget, from epoch timestamps.
    /// Stored as f64::to_bits; NaN until first polled / right after a retarget.
    pub est_difficulty_change_pct: AtomicU64,
    pub last_block_worker: Mutex<Option<String>>,
    pub last_block_hash: Mutex<Option<String>>,
    pub last_block_ts: AtomicU64,
    /// Credited share difficulty summed since the last block this pool found
    /// (f64 bits). Divided by network difficulty it is the round's effort.
    /// Persisted on the periodic snapshot tick and when a block closes the
    /// round, so a restart loses at most one tick of work.
    round_work: AtomicU64,
    /// When the current round opened: the last found block, or the first
    /// boot with a stats store. Zero only when there is no store.
    round_start_ts: AtomicU64,
    /// Every block this pool has found, oldest first. Loaded from the store
    /// at boot, so the count and the last-block card survive restarts.
    found_blocks: Mutex<Vec<FoundBlock>>,
    // Stored as f64::to_bits so we can use AtomicU64
    worker_hashrates_60s: DashMap<String, u64>,
    worker_hashrates_10m: DashMap<String, u64>,
    worker_hashrates_3h: DashMap<String, u64>,
    worker_hashrates_24h: DashMap<String, u64>,
    /// When each worker's hashrate estimates were last recomputed. Estimates
    /// only refresh while a session is delivering traffic, so this is the
    /// anchor for decaying an offline worker's frozen values out of each
    /// window (see `worker_hashrates`).
    worker_hashrate_updated_ts: DashMap<String, u64>,
    worker_protocol: DashMap<String, String>,
    worker_last_submit_ts: DashMap<String, u64>,
    worker_best_shares: DashMap<String, u64>,
    worker_states: DashMap<String, WorkerState>,
    start_time: Instant,
    store: Option<StatsStore>,
}

#[derive(Clone, Serialize)]
pub struct WorkerState {
    pub worker: String,
    /// Connection protocol: "sv1" or "sv2".
    pub protocol: String,
    pub online: bool,
    pub current_vardiff: u64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub shares_stale: u64,
    /// Rejected shares broken down by reason ("stale", "duplicate",
    /// "low_difficulty", ...). Keys come from the fixed reason strings at the
    /// reject sites, so cardinality is bounded. Session-lifetime, not persisted.
    pub reject_reasons: BTreeMap<String, u64>,
    pub best_share_difficulty: u64,
    pub active_sessions: u64,
    pub connected_ts: u64,
    pub last_submit_ts: u64,
    pub hashrate_60s_hps: f64,
    pub hashrate_10m_hps: f64,
    pub hashrate_3h_hps: f64,
    pub hashrate_24h_hps: f64,
}

impl PoolStats {
    pub fn new_with_store(stats_db_path: Option<String>) -> Arc<Self> {
        let (store, loaded) = match stats_db_path.filter(|p| !p.is_empty()) {
            Some(path) => match StatsStore::open(&path) {
                Ok(store) => match store.load_values() {
                    Ok(loaded) => (Some(store), loaded),
                    Err(e) => {
                        warn!("Failed to load stats from DB {}: {e}", path);
                        (None, LoadedStats::default())
                    }
                },
                Err(e) => {
                    warn!("Failed to open stats DB {}: {e}", path);
                    (None, LoadedStats::default())
                }
            },
            None => (None, LoadedStats::default()),
        };

        let worker_best_shares = DashMap::new();
        for (worker, best_share) in loaded.worker_best_shares {
            worker_best_shares.insert(worker, best_share);
        }

        // The first boot with a store opens the first round now; without a
        // store there is no round start worth reporting.
        let mut round_start_ts = loaded.round_start_ts;
        if round_start_ts == 0 {
            if let Some(store) = &store {
                round_start_ts = Self::now_secs();
                store.set_round(loaded.round_work, round_start_ts);
            }
        }
        let last_block = loaded.found_blocks.last().cloned();

        Arc::new(Self {
            shares_accepted: AtomicU64::new(0),
            shares_rejected: AtomicU64::new(0),
            blocks_found: AtomicU64::new(loaded.found_blocks.len() as u64),
            connected_miners: AtomicU64::new(0),
            current_height: AtomicU64::new(0),
            current_coinbase_value: AtomicU64::new(0),
            current_block_transaction_count: AtomicU64::new(0),
            best_share_difficulty: AtomicU64::new(loaded.best_share_difficulty),
            session_best_share_difficulty: AtomicU64::new(0),
            best_hashrate_hps: AtomicU64::new(loaded.best_hashrate_hps.to_bits()),
            session_best_hashrate_hps: AtomicU64::new(0),
            network_hashrate_hps: AtomicU64::new(0),
            network_difficulty: AtomicU64::new(f64::to_bits(0.0)),
            est_difficulty_change_pct: AtomicU64::new(f64::to_bits(f64::NAN)),
            worker_hashrates_60s: DashMap::new(),
            worker_hashrates_10m: DashMap::new(),
            worker_hashrates_3h: DashMap::new(),
            worker_hashrates_24h: DashMap::new(),
            worker_hashrate_updated_ts: DashMap::new(),
            worker_protocol: DashMap::new(),
            worker_last_submit_ts: DashMap::new(),
            worker_best_shares,
            worker_states: DashMap::new(),
            last_block_worker: Mutex::new(last_block.as_ref().map(|b| b.worker.clone())),
            last_block_hash: Mutex::new(last_block.as_ref().map(|b| b.hash.clone())),
            last_block_ts: AtomicU64::new(last_block.as_ref().map(|b| b.ts).unwrap_or(0)),
            round_work: AtomicU64::new(loaded.round_work.to_bits()),
            round_start_ts: AtomicU64::new(round_start_ts),
            found_blocks: Mutex::new(loaded.found_blocks),
            start_time: Instant::now(),
            store,
        })
    }

    fn persist_best_share_difficulty(&self, difficulty: u64) {
        if let Some(store) = &self.store {
            store.set_best_share_difficulty(difficulty);
        }
    }

    fn persist_best_hashrate_hps(&self, hps: f64) {
        if let Some(store) = &self.store {
            store.set_best_hashrate_hps(hps);
        }
    }

    /// Clear the all-time best-hashrate watermark, in memory and on disk.
    ///
    /// Operator-initiated only. The watermark is a monotonic maximum, so a
    /// reading produced by a faulty estimate would otherwise stand forever and
    /// no honest measurement could ever reach it. Deliberately not automatic:
    /// silently rewriting an all-time record is not something an upgrade should
    /// do on the operator's behalf.
    ///
    /// Leaves best-share records alone; those are proof of work actually done
    /// and are never invalidated by an estimator change.
    pub fn reset_best_hashrate(&self) {
        self.best_hashrate_hps
            .store(0f64.to_bits(), Ordering::Relaxed);
        self.session_best_hashrate_hps
            .store(0f64.to_bits(), Ordering::Relaxed);
        if let Some(store) = &self.store {
            store.force_best_hashrate_hps(0.0);
        }
        tracing::info!("All-time best-hashrate watermark reset by operator request");
    }

    pub fn miner_connected(&self) {
        self.connected_miners.fetch_add(1, Ordering::Relaxed);
    }

    pub fn miner_disconnected(&self) {
        self.connected_miners.fetch_sub(1, Ordering::Relaxed);
    }

    /// `difficulty` is the hash's actual difficulty (drives the best-share
    /// records); `credited` is the difficulty the share was credited at (see
    /// `Vardiff::credit_for`) and is what the round's effort sums. Summing
    /// hash difficulty instead would let one lucky share add terahashes of
    /// "work" the miner never did.
    pub fn share_accepted(&self, difficulty: u64, credited: u64) {
        self.shares_accepted.fetch_add(1, Ordering::Relaxed);

        let mut prev = self.round_work.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(prev) + credited as f64).to_bits();
            match self.round_work.compare_exchange_weak(
                prev,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => prev = x,
            }
        }

        // CAS loop to track all-time best share
        let mut prev = self.best_share_difficulty.load(Ordering::Relaxed);
        while difficulty > prev {
            match self.best_share_difficulty.compare_exchange_weak(
                prev,
                difficulty,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.persist_best_share_difficulty(difficulty);
                    break;
                }
                Err(x) => prev = x,
            }
        }

        // Session best share
        let mut prev_session_best = self.session_best_share_difficulty.load(Ordering::Relaxed);
        while difficulty > prev_session_best {
            match self.session_best_share_difficulty.compare_exchange_weak(
                prev_session_best,
                difficulty,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => prev_session_best = x,
            }
        }
    }

    pub fn share_rejected(&self) {
        self.shares_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a block this pool found. Closes the current round: its work
    /// and the network difficulty at this moment are stored with the block,
    /// and a new round opens now. Runs after `submitblock` has succeeded,
    /// never on the path to it.
    pub fn block_found(&self, worker: &str, hash: &str, height: u64) {
        // Inline success and the background retrier both credit; the same
        // block must not close two rounds or count twice.
        if self.found_blocks.lock().iter().any(|b| b.hash == hash) {
            return;
        }
        let now = Self::now_secs();
        let round_work = f64::from_bits(self.round_work.swap(0f64.to_bits(), Ordering::Relaxed));
        let block = FoundBlock {
            height,
            hash: hash.to_string(),
            worker: worker.to_string(),
            ts: now,
            round_work,
            network_difficulty: f64::from_bits(self.network_difficulty.load(Ordering::Relaxed)),
        };
        self.blocks_found.fetch_add(1, Ordering::Relaxed);
        *self.last_block_worker.lock() = Some(worker.to_string());
        *self.last_block_hash.lock() = Some(hash.to_string());
        self.last_block_ts.store(now, Ordering::Relaxed);
        self.round_start_ts.store(now, Ordering::Relaxed);
        self.found_blocks.lock().push(block.clone());
        if let Some(store) = &self.store {
            store.insert_found_block(&block);
            store.set_round(0.0, now);
        }
    }

    /// Persist the running round work. Called from the periodic snapshot
    /// tick rather than per share: the value only needs to survive a restart
    /// to within one tick, and a per-share write would cost a SQLite
    /// transaction on the hot path.
    fn persist_round_state(&self) {
        if let Some(store) = &self.store {
            store.set_round(
                f64::from_bits(self.round_work.load(Ordering::Relaxed)),
                self.round_start_ts.load(Ordering::Relaxed),
            );
        }
    }

    pub fn update_height(&self, height: u64, coinbase_value: u64, transaction_count: u64) {
        self.current_height.store(height, Ordering::Relaxed);
        self.current_coinbase_value
            .store(coinbase_value, Ordering::Relaxed);
        self.current_block_transaction_count
            .store(transaction_count, Ordering::Relaxed);
    }

    pub fn update_worker_hashrate(
        &self,
        worker: &str,
        hps_60s: f64,
        hps_10m: f64,
        hps_3h: f64,
        hps_24h: f64,
    ) {
        self.worker_hashrates_60s
            .insert(worker.to_string(), hps_60s.to_bits());
        self.worker_hashrates_10m
            .insert(worker.to_string(), hps_10m.to_bits());
        self.worker_hashrates_3h
            .insert(worker.to_string(), hps_3h.to_bits());
        self.worker_hashrates_24h
            .insert(worker.to_string(), hps_24h.to_bits());
        let now = Self::now_secs();
        self.worker_hashrate_updated_ts
            .insert(worker.to_string(), now);

        // Decayed sum, so a worker that went offline at high hashrate can't
        // keep inflating the pool total (and the best-hashrate watermark).
        let total_10m: f64 = self
            .worker_hashrates_10m
            .iter()
            .map(|e| self.worker_hashrates(e.key(), now).1)
            .sum();

        // Track all-time best (persistent) and session-best (since boot).
        // CAS loops (like share_accepted's best-share tracking) so two racing
        // updaters cannot let a lower value overwrite a higher one that landed
        // between the load and the store.
        let mut prev = self.best_hashrate_hps.load(Ordering::Relaxed);
        while total_10m > f64::from_bits(prev) {
            match self.best_hashrate_hps.compare_exchange_weak(
                prev,
                total_10m.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.persist_best_hashrate_hps(total_10m);
                    break;
                }
                Err(x) => prev = x,
            }
        }

        let mut prev_session = self.session_best_hashrate_hps.load(Ordering::Relaxed);
        while total_10m > f64::from_bits(prev_session) {
            match self.session_best_hashrate_hps.compare_exchange_weak(
                prev_session,
                total_10m.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => prev_session = x,
            }
        }
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// The four windowed hashrate estimates for `worker`: (60s, 10m, 3h, 24h).
    ///
    /// Estimates only refresh while the miner's session is delivering traffic,
    /// so an offline worker's stored values would otherwise freeze at their
    /// last computed level. To keep each window honest, an offline worker's
    /// values decay linearly by the fraction of the window that has passed
    /// since the last recompute: the 60s figure reads zero after a minute
    /// offline, the 24h figure after a day. Online workers are returned as
    /// stored, since their session recomputes on every message.
    fn worker_hashrates(&self, worker: &str, now: u64) -> (f64, f64, f64, f64) {
        let get = |map: &DashMap<String, u64>| {
            map.get(worker)
                .map(|h| f64::from_bits(*h.value()))
                .unwrap_or(0.0)
        };
        let hr_60s = get(&self.worker_hashrates_60s);
        let hr_10m = get(&self.worker_hashrates_10m);
        let hr_3h = get(&self.worker_hashrates_3h);
        let hr_24h = get(&self.worker_hashrates_24h);

        let online = self
            .worker_states
            .get(worker)
            .map(|s| s.online)
            .unwrap_or(false);
        if online {
            return (hr_60s, hr_10m, hr_3h, hr_24h);
        }

        let updated = self
            .worker_hashrate_updated_ts
            .get(worker)
            .map(|v| *v.value())
            .unwrap_or(0);
        let elapsed = now.saturating_sub(updated);
        (
            hr_60s * window_overlap(elapsed, HASHRATE_WINDOW_60S),
            hr_10m * window_overlap(elapsed, HASHRATE_WINDOW_10M),
            hr_3h * window_overlap(elapsed, HASHRATE_WINDOW_3H),
            hr_24h * window_overlap(elapsed, HASHRATE_WINDOW_24H),
        )
    }

    /// Decayed 10m hashrate for every offline worker still tracked in stats.
    ///
    /// The per-worker Prometheus gauge is pushed from the session loop, so it
    /// would otherwise freeze at its last value when a miner disconnects. The
    /// background metrics task re-pushes these decayed values to keep the
    /// scrape surface in line with the dashboard. Online workers are excluded:
    /// their sessions refresh the gauge on every message, and a periodic push
    /// could overwrite a fresher session value with a stale one.
    pub fn offline_worker_hashrates_10m(&self) -> Vec<(String, f64)> {
        let now = Self::now_secs();
        let workers: Vec<String> = self
            .worker_hashrates_10m
            .iter()
            .map(|e| e.key().clone())
            .collect();
        workers
            .into_iter()
            .filter(|w| !self.worker_states.get(w).map(|s| s.online).unwrap_or(false))
            .map(|w| {
                let (_, hr_10m, _, _) = self.worker_hashrates(&w, now);
                (w, hr_10m)
            })
            .collect()
    }

    /// Record the connection protocol ("sv1" / "sv2") for a worker.
    pub fn set_worker_protocol(&self, worker: &str, protocol: &str) {
        self.worker_protocol
            .insert(worker.to_string(), protocol.to_string());
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.protocol = protocol.to_string();
        }
    }

    pub fn mark_worker_submit(&self, worker: &str) {
        let now = Self::now_secs();
        self.worker_last_submit_ts.insert(worker.to_string(), now);
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.last_submit_ts = now;
        }
    }

    pub fn mark_worker_online(&self, worker: &str, current_vardiff: u64) {
        let now = Self::now_secs();
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.online = true;
            state.current_vardiff = current_vardiff;
            state.connected_ts = now;
            state.active_sessions = state.active_sessions.saturating_add(1);
        } else {
            let best_share_difficulty = self
                .worker_best_shares
                .get(worker)
                .map(|v| *v.value())
                .unwrap_or(0);

            let protocol = self
                .worker_protocol
                .get(worker)
                .map(|p| p.value().clone())
                .unwrap_or_else(|| "sv1".to_string());

            self.worker_states.insert(
                worker.to_string(),
                WorkerState {
                    worker: worker.to_string(),
                    protocol,
                    online: true,
                    current_vardiff,
                    shares_accepted: 0,
                    shares_rejected: 0,
                    shares_stale: 0,
                    reject_reasons: BTreeMap::new(),
                    best_share_difficulty,
                    active_sessions: 1,
                    connected_ts: now,
                    last_submit_ts: 0,
                    hashrate_60s_hps: 0.0,
                    hashrate_10m_hps: 0.0,
                    hashrate_3h_hps: 0.0,
                    hashrate_24h_hps: 0.0,
                },
            );
        }
    }

    pub fn mark_worker_offline(&self, worker: &str) {
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            if state.active_sessions > 1 {
                state.active_sessions -= 1;
            } else {
                state.active_sessions = 0;
                state.online = false;
            }
        }
    }

    pub fn update_worker_vardiff(&self, worker: &str, vardiff: u64) {
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.current_vardiff = vardiff;
        } else {
            self.mark_worker_online(worker, vardiff);
        }
    }

    pub fn worker_share_accepted(&self, worker: &str, difficulty: u64) {
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.shares_accepted += 1;
            if difficulty > state.best_share_difficulty {
                state.best_share_difficulty = difficulty;
            }
        }

        let mut entry = self
            .worker_best_shares
            .entry(worker.to_string())
            .or_insert(0);
        if difficulty > *entry {
            *entry = difficulty;
            if let Some(store) = &self.store {
                store.set_worker_best_share(worker, difficulty);
            }
        }
    }

    pub fn worker_share_rejected(&self, worker: &str, reason: &str) {
        if let Some(mut state) = self.worker_states.get_mut(worker) {
            state.shares_rejected += 1;
            *state.reject_reasons.entry(reason.to_string()).or_insert(0) += 1;
            if reason == "stale" {
                state.shares_stale += 1;
            }
        }
    }

    /// Evict offline workers idle past `IDLE_WORKER_EVICT_SECS` from the
    /// in-memory maps, and bound `worker_best_shares` (memory + SQLite) to the
    /// top `MAX_WORKER_BEST_SHARES` by difficulty. Called from the background
    /// pruner task. A reconnecting evicted worker is recreated on authorize;
    /// only its session counters (accepted/rejected this boot) reset.
    pub fn prune_idle_workers(&self) {
        let cutoff = Self::now_secs().saturating_sub(IDLE_WORKER_EVICT_SECS);

        let stale: Vec<String> = self
            .worker_states
            .iter()
            .filter(|e| {
                let s = e.value();
                if s.online {
                    return false;
                }
                let last_submit = self
                    .worker_last_submit_ts
                    .get(e.key())
                    .map(|v| *v.value())
                    .unwrap_or(s.last_submit_ts);
                last_submit.max(s.connected_ts) < cutoff
            })
            .map(|e| e.key().clone())
            .collect();

        for w in &stale {
            self.worker_states.remove(w);
            self.worker_hashrates_60s.remove(w);
            self.worker_hashrates_10m.remove(w);
            self.worker_hashrates_3h.remove(w);
            self.worker_hashrates_24h.remove(w);
            self.worker_hashrate_updated_ts.remove(w);
            self.worker_protocol.remove(w);
            self.worker_last_submit_ts.remove(w);
        }
        if !stale.is_empty() {
            info!("Evicted {} idle offline workers from stats", stale.len());
        }

        // Best shares survive eviction (the dashboard still lists all-time
        // bests), bounded by count so they can't grow without limit.
        if self.worker_best_shares.len() > MAX_WORKER_BEST_SHARES {
            let mut all: Vec<(String, u64)> = self
                .worker_best_shares
                .iter()
                .map(|e| (e.key().clone(), *e.value()))
                .collect();
            all.sort_unstable_by_key(|e| std::cmp::Reverse(e.1));
            for (w, _) in all.drain(MAX_WORKER_BEST_SHARES..) {
                self.worker_best_shares.remove(&w);
            }
            if let Some(store) = &self.store {
                store.prune_worker_best_shares(MAX_WORKER_BEST_SHARES);
            }
        }
    }

    /// Read a persisted runtime setting (dashboard Settings page).
    pub fn load_setting(&self, key: &str) -> Option<String> {
        self.store.as_ref().and_then(|s| s.get_setting(key))
    }

    /// Persist a runtime setting. Returns false when no stats DB is configured
    /// (the value still applies in memory but won't survive a restart).
    pub fn save_setting(&self, key: &str, value: &str) -> bool {
        self.store
            .as_ref()
            .map(|s| s.set_setting(key, value))
            .unwrap_or(false)
    }

    /// Whether a SQLite store backs this instance (settings persistence).
    pub fn has_store(&self) -> bool {
        self.store.is_some()
    }

    pub fn record_hashrate_snapshot(&self) {
        if let Some(store) = &self.store {
            let ts = Self::now_secs();
            let hps: f64 = self
                .worker_hashrates_10m
                .iter()
                .map(|e| self.worker_hashrates(e.key(), ts).1)
                .sum();
            store.record_hashrate_snapshot(ts, hps);
        }
        self.persist_round_state();
    }

    pub fn get_hashrate_history(&self, since_ts: u64) -> Vec<(u64, f64)> {
        self.store
            .as_ref()
            .map(|s| s.get_hashrate_history(since_ts))
            .unwrap_or_default()
    }

    pub fn set_network_hashrate(&self, hps: f64) {
        self.network_hashrate_hps
            .store(hps.to_bits(), Ordering::Relaxed);
    }

    pub fn set_network_difficulty(&self, difficulty: f64) {
        self.network_difficulty
            .store(difficulty.to_bits(), Ordering::Relaxed);
    }

    pub fn set_est_difficulty_change_pct(&self, pct: f64) {
        self.est_difficulty_change_pct
            .store(pct.to_bits(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let now = Self::now_secs();
        let worker_hashrates: Vec<WorkerHashrate> = self
            .worker_hashrates_10m
            .iter()
            .map(|e| {
                let worker = e.key().clone();
                let (hr_60s, hr_10m, hr_3h, hr_24h) = self.worker_hashrates(&worker, now);
                WorkerHashrate {
                    worker: worker.clone(),
                    last_submit_ts: self
                        .worker_last_submit_ts
                        .get(&worker)
                        .map(|v| *v.value())
                        .unwrap_or(0),
                    hashrate_60s_hps: hr_60s,
                    hashrate_10m_hps: hr_10m,
                    hashrate_3h_hps: hr_3h,
                    hashrate_24h_hps: hr_24h,
                }
            })
            .collect();

        let total_hashrate_10m: f64 = worker_hashrates.iter().map(|w| w.hashrate_10m_hps).sum();
        let total_hashrate_60s: f64 = worker_hashrates.iter().map(|w| w.hashrate_60s_hps).sum();
        let total_hashrate_3h: f64 = worker_hashrates.iter().map(|w| w.hashrate_3h_hps).sum();
        let total_hashrate_24h: f64 = worker_hashrates.iter().map(|w| w.hashrate_24h_hps).sum();

        let best_hashrate_hps = f64::from_bits(self.best_hashrate_hps.load(Ordering::Relaxed));

        let mut seen = std::collections::HashSet::new();
        let mut worker_states: Vec<WorkerState> = self
            .worker_states
            .iter()
            .map(|e| {
                let mut state = e.value().clone();
                let worker = e.key();
                seen.insert(worker.clone());
                state.worker = worker.clone();
                let (hr_60s, hr_10m, hr_3h, hr_24h) = self.worker_hashrates(worker, now);
                state.hashrate_60s_hps = hr_60s;
                state.hashrate_10m_hps = hr_10m;
                state.hashrate_3h_hps = hr_3h;
                state.hashrate_24h_hps = hr_24h;
                if let Some(p) = self.worker_protocol.get(worker) {
                    state.protocol = p.value().clone();
                }
                state.last_submit_ts = self
                    .worker_last_submit_ts
                    .get(worker)
                    .map(|v| *v.value())
                    .unwrap_or(0);
                state.best_share_difficulty = self
                    .worker_best_shares
                    .get(worker)
                    .map(|v| *v.value())
                    .unwrap_or(state.best_share_difficulty);
                state
            })
            .collect();

        for entry in self.worker_best_shares.iter() {
            let worker = entry.key();
            if seen.contains(worker) {
                continue;
            }
            worker_states.push(WorkerState {
                worker: worker.clone(),
                protocol: self
                    .worker_protocol
                    .get(worker)
                    .map(|p| p.value().clone())
                    .unwrap_or_else(|| "sv1".to_string()),
                online: false,
                current_vardiff: 0,
                shares_accepted: 0,
                shares_rejected: 0,
                shares_stale: 0,
                reject_reasons: BTreeMap::new(),
                best_share_difficulty: *entry.value(),
                active_sessions: 0,
                connected_ts: 0,
                last_submit_ts: 0,
                hashrate_60s_hps: 0.0,
                hashrate_10m_hps: 0.0,
                hashrate_3h_hps: 0.0,
                hashrate_24h_hps: 0.0,
            });
        }

        StatsSnapshot {
            shares_accepted: self.shares_accepted.load(Ordering::Relaxed),
            shares_rejected: self.shares_rejected.load(Ordering::Relaxed),
            blocks_found: self.blocks_found.load(Ordering::Relaxed),
            connected_miners: self.connected_miners.load(Ordering::Relaxed),
            current_height: self.current_height.load(Ordering::Relaxed),
            current_coinbase_value: self.current_coinbase_value.load(Ordering::Relaxed),
            current_block_transaction_count: self
                .current_block_transaction_count
                .load(Ordering::Relaxed),
            template_version: 0,
            best_share_difficulty: self.best_share_difficulty.load(Ordering::Relaxed),
            session_best_share_difficulty: self
                .session_best_share_difficulty
                .load(Ordering::Relaxed),
            best_hashrate_hps,
            total_hashrate_60s,
            total_hashrate_10m,
            total_hashrate_3h,
            total_hashrate_24h,
            worker_hashrates,
            worker_states,
            network_hashrate_hps: f64::from_bits(self.network_hashrate_hps.load(Ordering::Relaxed)),
            network_difficulty: f64::from_bits(self.network_difficulty.load(Ordering::Relaxed)),
            est_difficulty_change_pct: f64::from_bits(
                self.est_difficulty_change_pct.load(Ordering::Relaxed),
            ),
            uptime_secs: self.start_time.elapsed().as_secs(),
            session_best_hashrate_hps: f64::from_bits(
                self.session_best_hashrate_hps.load(Ordering::Relaxed),
            ),
            last_block_worker: self
                .last_block_worker
                .lock()
                .clone()
                .unwrap_or_else(|| "—".to_string()),
            last_block_hash: self
                .last_block_hash
                .lock()
                .clone()
                .unwrap_or_else(|| "—".to_string()),
            last_block_ts: self.last_block_ts.load(Ordering::Relaxed),
            round_work: f64::from_bits(self.round_work.load(Ordering::Relaxed)),
            round_start_ts: self.round_start_ts.load(Ordering::Relaxed),
            found_blocks: self.found_blocks.lock().clone(),
            template_age_secs: None,
            template_error: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot (serialised as JSON for /stats)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub blocks_found: u64,
    pub connected_miners: u64,
    pub current_height: u64,
    pub current_coinbase_value: u64,
    pub current_block_transaction_count: u64,
    /// Block version of the current template. PoolStats does not track this;
    /// the dashboard fills it from the TemplateEngine (the single writer of
    /// template state) when serving /stats, so it stays 0 elsewhere. Version
    /// bits reflect the node's soft-fork signaling (bit 4 = BIP110/RDTS).
    pub template_version: u32,
    pub best_share_difficulty: u64,
    pub session_best_share_difficulty: u64,
    pub best_hashrate_hps: f64,
    pub total_hashrate_60s: f64,
    pub total_hashrate_10m: f64,
    pub total_hashrate_3h: f64,
    pub total_hashrate_24h: f64,
    pub network_hashrate_hps: f64,
    pub network_difficulty: f64,
    pub est_difficulty_change_pct: f64,
    pub worker_hashrates: Vec<WorkerHashrate>,
    pub worker_states: Vec<WorkerState>,
    pub uptime_secs: u64,
    pub session_best_hashrate_hps: f64,
    pub last_block_worker: String,
    pub last_block_hash: String,
    pub last_block_ts: u64,
    /// Credited share difficulty summed since the last block this pool
    /// found. Over `network_difficulty` it is the current round's effort.
    pub round_work: f64,
    /// When the current round opened (unix seconds); 0 without a stats DB.
    pub round_start_ts: u64,
    /// Every block this pool has found, oldest first.
    pub found_blocks: Vec<FoundBlock>,
    /// Seconds since the template engine last built a job from a fresh
    /// getblocktemplate. PoolStats does not track this; the dashboard fills
    /// it from the TemplateEngine, like `template_version`. None before the
    /// first job.
    pub template_age_secs: Option<u64>,
    /// Why the engine could not build its last job, when it could not.
    /// Filled by the dashboard from the TemplateEngine.
    pub template_error: Option<String>,
}

#[derive(Serialize)]
pub struct WorkerHashrate {
    pub worker: String,
    pub last_submit_ts: u64,
    pub hashrate_60s_hps: f64,
    pub hashrate_10m_hps: f64,
    pub hashrate_3h_hps: f64,
    pub hashrate_24h_hps: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_db() -> String {
        static TEST_DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut path = std::env::temp_dir();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let id = TEST_DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        path.push(format!("solo_pool_rs_stats_test_{}_{}.db", ts, id));
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn stats_database_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let db_path = make_temp_db();
        let _stats = PoolStats::new_with_store(Some(db_path.clone()));
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "stats db must not be group/world readable"
        );
        std::fs::remove_file(&db_path).ok();
    }

    #[test]
    fn resetting_the_hashrate_watermark_beats_the_monotonic_guard() {
        // The watermark only ever moves up, so a reading produced by a faulty
        // estimate stands forever and no honest measurement can reach it. The
        // reset has to defeat the SQL guard and survive a restart.
        let db_path = make_temp_db();

        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            // An absurd reading of the kind the old share-anchored estimator
            // produced, hundreds of times above real hardware.
            stats.update_worker_hashrate("ghost", 2.27e17, 2.27e17, 2.27e17, 2.27e17);
            assert!(stats.snapshot().best_hashrate_hps > 1e17);

            stats.reset_best_hashrate();
            assert_eq!(
                stats.snapshot().best_hashrate_hps,
                0.0,
                "reset must clear the in-memory watermark"
            );
        }

        // And it must not come back on restart.
        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(
            stats.snapshot().best_hashrate_hps,
            0.0,
            "reset must clear the persisted watermark, not just the in-memory one"
        );

        // A later honest reading still sets a fresh watermark.
        stats.update_worker_hashrate("nerdqaxe", 4.0e13, 4.0e13, 4.0e13, 4.0e13);
        assert!(stats.snapshot().best_hashrate_hps > 0.0);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn resetting_the_hashrate_watermark_keeps_best_shares() {
        // Best shares are proof of work actually done; an estimator bug must
        // not be an excuse to drop them.
        let db_path = make_temp_db();
        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        stats.worker_share_accepted("nerdqaxe", 794_568_949_760);
        stats.share_accepted(794_568_949_760, 4096);

        stats.reset_best_hashrate();

        assert_eq!(stats.snapshot().best_share_difficulty, 794_568_949_760);
        let states = stats.snapshot().worker_states;
        assert!(
            states
                .iter()
                .any(|w| w.best_share_difficulty == 794_568_949_760),
            "worker best share must survive a hashrate reset"
        );
        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn best_hashrate_is_persisted_across_instances() {
        let db_path = make_temp_db();
        let expected;

        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            stats.update_worker_hashrate(
                "nerdqaxe",
                6_000_000_000_000.0,
                6_000_000_000_000.0,
                6_000_000_000_000.0,
                6_000_000_000_000.0,
            );
            assert_eq!(stats.snapshot().best_hashrate_hps, 6_000_000_000_000.0);
            stats.update_worker_hashrate(
                "nano",
                4_000_000_000_000.0,
                4_000_000_000_000.0,
                4_000_000_000_000.0,
                4_000_000_000_000.0,
            );
            // Both workers are offline here, so their stored estimates decay by
            // `window_overlap`. If these two updates straddle a wall-clock second
            // the total lands just under 10 TH/s — so assert the watermark is in
            // range and carry the observed value across the restart, rather than
            // pinning an exact figure that depends on second boundaries.
            let live = stats.snapshot().best_hashrate_hps;
            assert!(
                (9_900_000_000_000.0..=10_000_000_000_000.0).contains(&live),
                "unexpected watermark before restart: {live}"
            );
            expected = live;
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(stats.snapshot().best_hashrate_hps, expected);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn best_share_is_persisted_across_instances() {
        let db_path = make_temp_db();

        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            stats.share_accepted(1_000_000, 1_000_000);
            assert_eq!(stats.snapshot().best_share_difficulty, 1_000_000);
            stats.share_accepted(1_500_000, 1_000_000);
            assert_eq!(stats.snapshot().best_share_difficulty, 1_500_000);
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(stats.snapshot().best_share_difficulty, 1_500_000);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn round_work_sums_credited_difficulty_and_a_block_closes_the_round() {
        let stats = PoolStats::new_with_store(None);
        stats.set_network_difficulty(1_000_000.0);
        // Hash difficulty is luck; only the credited difficulty is work.
        stats.share_accepted(900_000_000, 4_096);
        stats.share_accepted(5_000, 4_096);
        let snap = stats.snapshot();
        assert_eq!(snap.round_work, 8_192.0);
        assert_eq!(snap.blocks_found, 0);
        assert!(snap.found_blocks.is_empty());
        assert_eq!(snap.last_block_worker, "—");

        stats.block_found("bitaxe", "00ab", 912_401);
        let snap = stats.snapshot();
        assert_eq!(snap.round_work, 0.0, "a found block opens a fresh round");
        assert_eq!(snap.blocks_found, 1);
        assert_eq!(snap.last_block_worker, "bitaxe");
        assert_eq!(snap.last_block_hash, "00ab");
        assert!(snap.last_block_ts > 0);
        assert_eq!(snap.round_start_ts, snap.last_block_ts);
        let block = &snap.found_blocks[0];
        assert_eq!(block.height, 912_401);
        assert_eq!(
            block.round_work, 8_192.0,
            "the closed round's work rides with the block"
        );
        assert_eq!(block.network_difficulty, 1_000_000.0);
    }

    #[test]
    fn found_blocks_and_round_work_are_persisted_across_instances() {
        let db_path = make_temp_db();

        let first_round_start;
        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            first_round_start = stats.snapshot().round_start_ts;
            assert!(
                first_round_start > 0,
                "first boot with a store opens a round"
            );
            stats.set_network_difficulty(50.0);
            stats.share_accepted(10, 10);
            stats.block_found("w1", "aa", 100);
            stats.share_accepted(7, 7);
            // Round work reaches disk on the snapshot tick, not per share.
            stats.record_hashrate_snapshot();
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        let snap = stats.snapshot();
        assert_eq!(snap.blocks_found, 1);
        assert_eq!(snap.last_block_worker, "w1");
        assert_eq!(snap.last_block_hash, "aa");
        assert_eq!(snap.found_blocks.len(), 1);
        assert_eq!(snap.found_blocks[0].height, 100);
        assert_eq!(snap.found_blocks[0].round_work, 10.0);
        assert_eq!(snap.found_blocks[0].network_difficulty, 50.0);
        assert_eq!(snap.round_work, 7.0);
        assert!(
            snap.round_start_ts >= first_round_start,
            "the round reopened at the block, not at boot"
        );

        // The same block credited again (inline success racing the retrier)
        // is not counted twice, in memory or on disk.
        stats.share_accepted(3, 3);
        stats.block_found("w1", "aa", 100);
        assert_eq!(stats.snapshot().blocks_found, 1);
        assert_eq!(
            stats.snapshot().round_work,
            10.0,
            "a duplicate must not close the round"
        );
        drop(stats);
        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(stats.snapshot().found_blocks.len(), 1);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn prune_evicts_idle_offline_workers_but_not_online_or_recent() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("online", 1_000);
        stats.mark_worker_online("idle", 1_000);
        stats.mark_worker_offline("idle");
        stats.mark_worker_online("recent", 1_000);
        stats.mark_worker_offline("recent");

        // Age "idle" past the TTL; "recent" went offline just now.
        stats.worker_states.get_mut("idle").unwrap().connected_ts =
            PoolStats::now_secs() - IDLE_WORKER_EVICT_SECS - 60;

        stats.prune_idle_workers();

        assert!(stats.worker_states.get("online").is_some());
        assert!(stats.worker_states.get("recent").is_some());
        assert!(stats.worker_states.get("idle").is_none());
    }

    #[test]
    fn worker_rejects_are_counted_per_reason() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("w", 1_000);

        stats.worker_share_rejected("w", "stale");
        stats.worker_share_rejected("w", "stale");
        stats.worker_share_rejected("w", "low_difficulty");
        stats.worker_share_rejected("w", "duplicate");

        let state = stats.worker_states.get("w").unwrap();
        assert_eq!(state.shares_rejected, 4);
        assert_eq!(state.shares_stale, 2);
        assert_eq!(state.reject_reasons.get("stale"), Some(&2));
        assert_eq!(state.reject_reasons.get("low_difficulty"), Some(&1));
        assert_eq!(state.reject_reasons.get("duplicate"), Some(&1));
        assert_eq!(state.reject_reasons.get("invalid"), None);
    }

    #[test]
    fn worker_best_shares_bounded_in_memory_and_on_disk() {
        let db_path = make_temp_db();
        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            for i in 0..MAX_WORKER_BEST_SHARES + 100 {
                stats.worker_share_accepted(&format!("w{i}"), i as u64 + 1);
            }
            stats.prune_idle_workers();
            assert_eq!(stats.worker_best_shares.len(), MAX_WORKER_BEST_SHARES);
            // Highest difficulties survive.
            assert!(stats
                .worker_best_shares
                .get(&format!("w{}", MAX_WORKER_BEST_SHARES + 99))
                .is_some());
            assert!(stats.worker_best_shares.get("w0").is_none());
        }
        // Reopen: boot-time prune + load stay within the cap.
        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert!(stats.worker_best_shares.len() <= MAX_WORKER_BEST_SHARES);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn worker_best_share_is_persisted_across_instances() {
        let db_path = make_temp_db();

        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            stats.mark_worker_online("w1", 1_000);
            stats.worker_share_accepted("w1", 1000);
            stats.worker_share_accepted("w1", 4000);

            let ss = stats.snapshot();
            let w1 = ss.worker_states.iter().find(|w| w.worker == "w1").unwrap();
            assert_eq!(w1.best_share_difficulty, 4000);
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        let ss = stats.snapshot();
        let w1 = ss.worker_states.iter().find(|w| w.worker == "w1").unwrap();
        assert_eq!(w1.best_share_difficulty, 4000);

        std::fs::remove_file(db_path).ok();
    }

    const TH: f64 = 1e12;

    /// Backdate a worker's last hashrate recompute so decay tests don't sleep.
    fn backdate_hashrate_update(stats: &PoolStats, worker: &str, secs_ago: u64) {
        stats
            .worker_hashrate_updated_ts
            .insert(worker.to_string(), PoolStats::now_secs() - secs_ago);
    }

    #[test]
    fn offline_worker_hashrate_decays_out_of_each_window() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("axe", 1024);
        stats.update_worker_hashrate("axe", TH, TH, TH, TH);
        stats.mark_worker_offline("axe");

        // 700s offline: past the 60s and 10m windows, partway into 3h and 24h.
        backdate_hashrate_update(&stats, "axe", 700);

        let ss = stats.snapshot();
        assert_eq!(ss.total_hashrate_60s, 0.0);
        assert_eq!(ss.total_hashrate_10m, 0.0);
        // Tolerances cover the clock ticking between backdating and snapshot.
        let tol_3h = 3.0 * TH / HASHRATE_WINDOW_3H as f64;
        let tol_24h = 3.0 * TH / HASHRATE_WINDOW_24H as f64;
        assert!((ss.total_hashrate_3h - TH * (1.0 - 700.0 / 10_800.0)).abs() < tol_3h);
        assert!((ss.total_hashrate_24h - TH * (1.0 - 700.0 / 86_400.0)).abs() < tol_24h);

        // Per-worker rows decay the same way, in both dashboard shapes.
        let row = &ss.worker_hashrates[0];
        assert_eq!(row.hashrate_60s_hps, 0.0);
        assert_eq!(row.hashrate_10m_hps, 0.0);
        let state = ss.worker_states.iter().find(|w| w.worker == "axe").unwrap();
        assert_eq!(state.hashrate_10m_hps, 0.0);
        assert!(state.hashrate_3h_hps > 0.0);
    }

    #[test]
    fn offline_worker_hashrate_zeroes_after_the_longest_window() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("axe", 1024);
        stats.update_worker_hashrate("axe", TH, TH, TH, TH);
        stats.mark_worker_offline("axe");

        backdate_hashrate_update(&stats, "axe", HASHRATE_WINDOW_24H);

        let ss = stats.snapshot();
        assert_eq!(ss.total_hashrate_60s, 0.0);
        assert_eq!(ss.total_hashrate_10m, 0.0);
        assert_eq!(ss.total_hashrate_3h, 0.0);
        assert_eq!(ss.total_hashrate_24h, 0.0);
    }

    #[test]
    fn online_worker_hashrate_is_not_decayed() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("axe", 1024);
        stats.update_worker_hashrate("axe", TH, TH, TH, TH);

        // Even with a stale recompute timestamp, online workers report the
        // stored values; their session refreshes them on every message.
        backdate_hashrate_update(&stats, "axe", 700);

        let ss = stats.snapshot();
        assert_eq!(ss.total_hashrate_60s, TH);
        assert_eq!(ss.total_hashrate_10m, TH);
        assert_eq!(ss.total_hashrate_3h, TH);
        assert_eq!(ss.total_hashrate_24h, TH);
    }

    #[test]
    fn offline_worker_hashrates_10m_decays_and_skips_online_workers() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("gone", 1024);
        stats.update_worker_hashrate("gone", TH, TH, TH, TH);
        stats.mark_worker_offline("gone");
        stats.mark_worker_online("live", 1024);
        stats.update_worker_hashrate("live", TH, TH, TH, TH);

        // 300s offline: halfway through the 10m window.
        backdate_hashrate_update(&stats, "gone", 300);

        let rows = stats.offline_worker_hashrates_10m();
        assert_eq!(rows.len(), 1);
        let (worker, hps) = &rows[0];
        assert_eq!(worker, "gone");
        let tol = 3.0 * TH / HASHRATE_WINDOW_10M as f64;
        assert!((hps - 0.5 * TH).abs() < tol);

        // Past the window the reported value bottoms out at zero.
        backdate_hashrate_update(&stats, "gone", HASHRATE_WINDOW_10M + 60);
        assert_eq!(stats.offline_worker_hashrates_10m()[0].1, 0.0);
    }

    #[test]
    fn reconnecting_worker_hashrate_resumes_from_fresh_estimates() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("axe", 1024);
        stats.update_worker_hashrate("axe", TH, TH, TH, TH);
        stats.mark_worker_offline("axe");
        backdate_hashrate_update(&stats, "axe", HASHRATE_WINDOW_24H);
        assert_eq!(stats.snapshot().total_hashrate_10m, 0.0);

        stats.mark_worker_online("axe", 1024);
        stats.update_worker_hashrate("axe", 2.0 * TH, 2.0 * TH, 2.0 * TH, 2.0 * TH);

        let ss = stats.snapshot();
        assert_eq!(ss.total_hashrate_10m, 2.0 * TH);
        assert_eq!(ss.total_hashrate_24h, 2.0 * TH);
    }

    #[test]
    fn offline_worker_does_not_inflate_best_hashrate_watermark() {
        let stats = PoolStats::new_with_store(None);
        stats.mark_worker_online("a", 1024);
        stats.update_worker_hashrate("a", TH, TH, TH, TH);
        stats.mark_worker_offline("a");
        backdate_hashrate_update(&stats, "a", HASHRATE_WINDOW_24H);

        // A second worker updating must not see worker "a"'s frozen 1 TH/s in
        // the pool total that feeds the best-hashrate watermark.
        stats.mark_worker_online("b", 1024);
        stats.update_worker_hashrate("b", TH, TH, TH, TH);

        assert_eq!(stats.snapshot().session_best_hashrate_hps, TH);
    }
}
