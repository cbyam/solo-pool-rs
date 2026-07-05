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

// ─────────────────────────────────────────────────────────────────────────────
// Persistent store for all-time metrics
// ─────────────────────────────────────────────────────────────────────────────

struct StatsStore {
    conn: Mutex<Connection>,
}

impl StatsStore {
    fn open(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
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

        conn.execute(
            "CREATE TABLE IF NOT EXISTS worker_best_shares (
             worker TEXT PRIMARY KEY,
             best_share_difficulty INTEGER NOT NULL
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

    fn load_values(
        &self,
    ) -> Result<(u64, f64, std::collections::HashMap<String, u64>), rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT best_share_difficulty, best_hashrate_hps FROM pool_stats WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        let best_values = if let Some(row) = rows.next()? {
            let best_share_difficulty = row.get::<_, u64>(0)?;
            let best_hashrate_hps = row.get::<_, f64>(1)?;
            (best_share_difficulty, best_hashrate_hps)
        } else {
            (0, 0.0)
        };

        let mut worker_best_shares = std::collections::HashMap::new();
        let mut stmt =
            conn.prepare("SELECT worker, best_share_difficulty FROM worker_best_shares")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let worker = row.get::<_, String>(0)?;
            let difficulty = row.get::<_, u64>(1)?;
            worker_best_shares.insert(worker, difficulty);
        }

        Ok((best_values.0, best_values.1, worker_best_shares))
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
    // Stored as f64::to_bits so we can use AtomicU64
    worker_hashrates_60s: DashMap<String, u64>,
    worker_hashrates_10m: DashMap<String, u64>,
    worker_hashrates_3h: DashMap<String, u64>,
    worker_hashrates_24h: DashMap<String, u64>,
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
        let (store, best_share_difficulty, best_hashrate_hps, worker_best_shares_map) =
            match stats_db_path.filter(|p| !p.is_empty()) {
                Some(path) => match StatsStore::open(&path) {
                    Ok(store) => match store.load_values() {
                        Ok((best_difficulty, best_hps, worker_best_shares_map)) => (
                            Some(store),
                            best_difficulty,
                            best_hps,
                            worker_best_shares_map,
                        ),
                        Err(e) => {
                            warn!("Failed to load stats from DB {}: {e}", path);
                            (None, 0, 0.0, std::collections::HashMap::new())
                        }
                    },
                    Err(e) => {
                        warn!("Failed to open stats DB {}: {e}", path);
                        (None, 0, 0.0, std::collections::HashMap::new())
                    }
                },
                None => (None, 0, 0.0, std::collections::HashMap::new()),
            };

        let worker_best_shares = DashMap::new();
        for (worker, best_share) in worker_best_shares_map {
            worker_best_shares.insert(worker, best_share);
        }

        Arc::new(Self {
            shares_accepted: AtomicU64::new(0),
            shares_rejected: AtomicU64::new(0),
            blocks_found: AtomicU64::new(0),
            connected_miners: AtomicU64::new(0),
            current_height: AtomicU64::new(0),
            current_coinbase_value: AtomicU64::new(0),
            current_block_transaction_count: AtomicU64::new(0),
            best_share_difficulty: AtomicU64::new(best_share_difficulty),
            session_best_share_difficulty: AtomicU64::new(0),
            best_hashrate_hps: AtomicU64::new(best_hashrate_hps.to_bits()),
            session_best_hashrate_hps: AtomicU64::new(0),
            network_hashrate_hps: AtomicU64::new(0),
            network_difficulty: AtomicU64::new(f64::to_bits(0.0)),
            est_difficulty_change_pct: AtomicU64::new(f64::to_bits(f64::NAN)),
            worker_hashrates_60s: DashMap::new(),
            worker_hashrates_10m: DashMap::new(),
            worker_hashrates_3h: DashMap::new(),
            worker_hashrates_24h: DashMap::new(),
            worker_protocol: DashMap::new(),
            worker_last_submit_ts: DashMap::new(),
            worker_best_shares,
            worker_states: DashMap::new(),
            last_block_worker: Mutex::new(None),
            last_block_hash: Mutex::new(None),
            last_block_ts: AtomicU64::new(0),
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

    pub fn miner_connected(&self) {
        self.connected_miners.fetch_add(1, Ordering::Relaxed);
    }

    pub fn miner_disconnected(&self) {
        self.connected_miners.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn share_accepted(&self, difficulty: u64) {
        self.shares_accepted.fetch_add(1, Ordering::Relaxed);

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

    pub fn block_found(&self, worker: &str, hash: &str) {
        self.blocks_found.fetch_add(1, Ordering::Relaxed);
        *self.last_block_worker.lock() = Some(worker.to_string());
        *self.last_block_hash.lock() = Some(hash.to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_block_ts.store(now, Ordering::Relaxed);
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

        let total_10m: f64 = self
            .worker_hashrates_10m
            .iter()
            .map(|e| f64::from_bits(*e.value()))
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
                .map(|e| f64::from_bits(*e.value()))
                .sum();
            store.record_hashrate_snapshot(ts, hps);
        }
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
        let worker_hashrates: Vec<WorkerHashrate> = self
            .worker_hashrates_10m
            .iter()
            .map(|e| {
                let worker = e.key().clone();
                let get = |map: &DashMap<String, u64>| {
                    map.get(&worker)
                        .map(|h| f64::from_bits(*h.value()))
                        .unwrap_or(0.0)
                };
                WorkerHashrate {
                    worker: worker.clone(),
                    last_submit_ts: self
                        .worker_last_submit_ts
                        .get(&worker)
                        .map(|v| *v.value())
                        .unwrap_or(0),
                    hashrate_60s_hps: get(&self.worker_hashrates_60s),
                    hashrate_10m_hps: f64::from_bits(*e.value()),
                    hashrate_3h_hps: get(&self.worker_hashrates_3h),
                    hashrate_24h_hps: get(&self.worker_hashrates_24h),
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
                let get = |map: &DashMap<String, u64>| {
                    map.get(worker)
                        .map(|h| f64::from_bits(*h.value()))
                        .unwrap_or(0.0)
                };
                state.hashrate_60s_hps = get(&self.worker_hashrates_60s);
                state.hashrate_10m_hps = get(&self.worker_hashrates_10m);
                state.hashrate_3h_hps = get(&self.worker_hashrates_3h);
                state.hashrate_24h_hps = get(&self.worker_hashrates_24h);
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

    #[test]
    fn best_hashrate_is_persisted_across_instances() {
        let db_path = make_temp_db();

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
            assert_eq!(stats.snapshot().best_hashrate_hps, 10_000_000_000_000.0);
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(stats.snapshot().best_hashrate_hps, 10_000_000_000_000.0);

        std::fs::remove_file(db_path).ok();
    }

    #[test]
    fn best_share_is_persisted_across_instances() {
        let db_path = make_temp_db();

        {
            let stats = PoolStats::new_with_store(Some(db_path.clone()));
            stats.share_accepted(1_000_000);
            assert_eq!(stats.snapshot().best_share_difficulty, 1_000_000);
            stats.share_accepted(1_500_000);
            assert_eq!(stats.snapshot().best_share_difficulty, 1_500_000);
        }

        let stats = PoolStats::new_with_store(Some(db_path.clone()));
        assert_eq!(stats.snapshot().best_share_difficulty, 1_500_000);

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
}
