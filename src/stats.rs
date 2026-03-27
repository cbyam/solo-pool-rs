/// stats.rs
///
/// In-process pool statistics — updated by session tasks, read by the dashboard.
///
/// Uses atomics and DashMap so updates are lock-free from any async task.
use dashmap::DashMap;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
use tracing::warn;

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

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn load_values(&self) -> Result<(u64, f64), rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT best_share_difficulty, best_hashrate_hps FROM pool_stats WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let best_share_difficulty = row.get::<_, u64>(0)?;
            let best_hashrate_hps = row.get::<_, f64>(1)?;
            Ok((best_share_difficulty, best_hashrate_hps))
        } else {
            Ok((0, 0.0))
        }
    }

    fn set_best_share_difficulty(&self, difficulty: u64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET best_share_difficulty = ?1 WHERE id = 1",
            params![difficulty],
        ) {
            warn!("Failed to persist best_share_difficulty: {e}");
        }
    }

    fn set_best_hashrate_hps(&self, hps: f64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE pool_stats SET best_hashrate_hps = ?1 WHERE id = 1",
            params![hps],
        ) {
            warn!("Failed to persist best_hashrate_hps: {e}");
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
    pub best_share_difficulty: AtomicU64,
    pub best_hashrate_hps: AtomicU64,
    pub session_best_hashrate_hps: AtomicU64,
    pub last_block_worker: Mutex<Option<String>>,
    pub last_block_hash: Mutex<Option<String>>,
    pub last_block_ts: AtomicU64,
    // Stored as f64::to_bits so we can use AtomicU64
    worker_hashrates_5m: DashMap<String, u64>,
    worker_hashrates_60s: DashMap<String, u64>,
    worker_hashrates_3h: DashMap<String, u64>,
    worker_last_submit_ts: DashMap<String, u64>,
    start_time: Instant,
    store: Option<StatsStore>,
}

impl PoolStats {
    pub fn new_with_store(stats_db_path: Option<String>) -> Arc<Self> {
        let (store, best_share_difficulty, best_hashrate_hps) =
            match stats_db_path.filter(|p| !p.is_empty()) {
                Some(path) => match StatsStore::open(&path) {
                    Ok(store) => match store.load_values() {
                        Ok((best_difficulty, best_hps)) => (Some(store), best_difficulty, best_hps),
                        Err(e) => {
                            warn!("Failed to load stats from DB {}: {e}", path);
                            (None, 0, 0.0)
                        }
                    },
                    Err(e) => {
                        warn!("Failed to open stats DB {}: {e}", path);
                        (None, 0, 0.0)
                    }
                },
                None => (None, 0, 0.0),
            };

        Arc::new(Self {
            shares_accepted: AtomicU64::new(0),
            shares_rejected: AtomicU64::new(0),
            blocks_found: AtomicU64::new(0),
            connected_miners: AtomicU64::new(0),
            current_height: AtomicU64::new(0),
            best_share_difficulty: AtomicU64::new(best_share_difficulty),
            best_hashrate_hps: AtomicU64::new(best_hashrate_hps.to_bits()),
            session_best_hashrate_hps: AtomicU64::new(0),
            worker_hashrates_5m: DashMap::new(),
            worker_hashrates_60s: DashMap::new(),
            worker_hashrates_3h: DashMap::new(),
            worker_last_submit_ts: DashMap::new(),
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
        // CAS loop to track best share
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

    pub fn update_height(&self, height: u64) {
        self.current_height.store(height, Ordering::Relaxed);
    }

    pub fn update_worker_hashrate(&self, worker: &str, hps_60s: f64, hps_3h: f64, hps_5m: f64) {
        self.worker_hashrates_60s
            .insert(worker.to_string(), hps_60s.to_bits());
        self.worker_hashrates_3h
            .insert(worker.to_string(), hps_3h.to_bits());
        self.worker_hashrates_5m
            .insert(worker.to_string(), hps_5m.to_bits());

        let total_hashrate_hps: f64 = self
            .worker_hashrates_5m
            .iter()
            .map(|e| f64::from_bits(*e.value()))
            .sum();

        // Track all-time best (persistent) and session-best (since boot)
        let prev_best = f64::from_bits(self.best_hashrate_hps.load(Ordering::Relaxed));
        if total_hashrate_hps > prev_best {
            self.best_hashrate_hps
                .store(total_hashrate_hps.to_bits(), Ordering::Relaxed);
            self.persist_best_hashrate_hps(total_hashrate_hps);
        }

        let prev_session_best =
            f64::from_bits(self.session_best_hashrate_hps.load(Ordering::Relaxed));
        if total_hashrate_hps > prev_session_best {
            self.session_best_hashrate_hps
                .store(total_hashrate_hps.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn mark_worker_submit(&self, worker: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.worker_last_submit_ts.insert(worker.to_string(), now);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let worker_hashrates: Vec<WorkerHashrate> = self
            .worker_hashrates_5m
            .iter()
            .map(|e| {
                let worker = e.key().clone();
                let hashrate_60s_hps = self
                    .worker_hashrates_60s
                    .get(&worker)
                    .map(|h| f64::from_bits(*h.value()))
                    .unwrap_or(0.0);
                let hashrate_3h_hps = self
                    .worker_hashrates_3h
                    .get(&worker)
                    .map(|h| f64::from_bits(*h.value()))
                    .unwrap_or(0.0);
                WorkerHashrate {
                    worker: worker.clone(),
                    last_submit_ts: self
                        .worker_last_submit_ts
                        .get(&worker)
                        .map(|v| *v.value())
                        .unwrap_or(0),
                    hashrate_60s_hps,
                    hashrate_3h_hps,
                    hashrate_5m_hps: f64::from_bits(*e.value()),
                }
            })
            .collect();

        let total_hashrate_hps: f64 = worker_hashrates.iter().map(|w| w.hashrate_5m_hps).sum();
        let total_hashrate_60s: f64 = worker_hashrates.iter().map(|w| w.hashrate_60s_hps).sum();
        let total_hashrate_3h: f64 = worker_hashrates.iter().map(|w| w.hashrate_3h_hps).sum();

        let best_hashrate_hps = f64::from_bits(self.best_hashrate_hps.load(Ordering::Relaxed));

        StatsSnapshot {
            shares_accepted: self.shares_accepted.load(Ordering::Relaxed),
            shares_rejected: self.shares_rejected.load(Ordering::Relaxed),
            blocks_found: self.blocks_found.load(Ordering::Relaxed),
            connected_miners: self.connected_miners.load(Ordering::Relaxed),
            current_height: self.current_height.load(Ordering::Relaxed),
            best_share_difficulty: self.best_share_difficulty.load(Ordering::Relaxed),
            best_hashrate_hps,
            total_hashrate_hps,
            total_hashrate_60s,
            total_hashrate_3h,
            worker_hashrates,
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
    pub best_share_difficulty: u64,
    pub best_hashrate_hps: f64,
    pub total_hashrate_hps: f64,
    pub total_hashrate_60s: f64,
    pub total_hashrate_3h: f64,
    pub worker_hashrates: Vec<WorkerHashrate>,
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
    pub hashrate_3h_hps: f64,
    pub hashrate_5m_hps: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_db() -> String {
        let mut path = std::env::temp_dir();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        path.push(format!("solo_pool_rs_stats_test_{}.db", ts));
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
            );
            assert_eq!(stats.snapshot().best_hashrate_hps, 6_000_000_000_000.0);
            stats.update_worker_hashrate(
                "nano",
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
}
