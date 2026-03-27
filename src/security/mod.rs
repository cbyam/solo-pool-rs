/// security/mod.rs
///
/// DoS protection layer:
///  - Per-IP connection rate limiting (sliding window)
///  - Per-session share-rate limiting (token bucket)
///  - Invalid-share counting with auto-disconnect
///  - IP ban list with TTL
///  - Maximum message size enforcement (protects JSON parser)
use crate::config::SecurityConfig;
use dashmap::DashMap;
use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::warn;

// ─────────────────────────────────────────────────────────────────────────────
// BanList
// ─────────────────────────────────────────────────────────────────────────────

struct BanEntry {
    until: Instant,
    #[allow(dead_code)]
    reason: String,
}

pub struct BanList {
    entries: DashMap<IpAddr, BanEntry>,
    ban_duration: Duration,
}

impl BanList {
    pub fn new(ban_duration_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            entries: DashMap::new(),
            ban_duration: Duration::from_secs(ban_duration_secs),
        })
    }

    pub fn ban(&self, ip: IpAddr, reason: &str) {
        warn!("Banning {ip} for {:?}: {reason}", self.ban_duration);
        self.entries.insert(
            ip,
            BanEntry {
                until: Instant::now() + self.ban_duration,
                reason: reason.to_string(),
            },
        );
    }

    pub fn is_banned(&self, ip: &IpAddr) -> bool {
        if let Some(entry) = self.entries.get(ip) {
            if Instant::now() < entry.until {
                return true;
            }
        }
        // Clean up expired ban while we're here
        self.entries.remove_if(ip, |_, e| Instant::now() >= e.until);
        false
    }

    /// Periodic cleanup — call from a background task every few minutes.
    pub fn prune(&self) {
        let now = Instant::now();
        self.entries.retain(|_, v| now < v.until);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-IP connection rate limiter (sliding window)
// ─────────────────────────────────────────────────────────────────────────────

pub struct ConnectionRateLimiter {
    /// IP → list of recent connection timestamps
    windows: DashMap<IpAddr, Vec<Instant>>,
    max_per_minute: u32,
}

impl ConnectionRateLimiter {
    pub fn new(max_per_minute: u32) -> Arc<Self> {
        Arc::new(Self {
            windows: DashMap::new(),
            max_per_minute,
        })
    }

    /// Returns `true` if this connection should be allowed.
    pub fn check_and_record(&self, ip: IpAddr) -> bool {
        let one_minute_ago = Instant::now() - Duration::from_secs(60);
        let mut entry = self.windows.entry(ip).or_default();

        // Evict old entries
        entry.retain(|&t| t > one_minute_ago);

        if entry.len() >= self.max_per_minute as usize {
            return false;
        }
        entry.push(Instant::now());
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-session share rate limiter (token bucket)
// ─────────────────────────────────────────────────────────────────────────────

pub struct ShareRateLimiter {
    /// Tokens available (capped at burst = max_per_sec)
    tokens: f64,
    max_per_sec: f64,
    last_refill: Instant,
}

impl ShareRateLimiter {
    pub fn new(max_per_sec: u32) -> Self {
        let rate = max_per_sec as f64;
        Self {
            tokens: rate,
            max_per_sec: rate,
            last_refill: Instant::now(),
        }
    }

    /// Returns `true` if the share can proceed; `false` if rate limited.
    pub fn try_consume(&mut self) -> bool {
        // Refill tokens based on elapsed time
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.max_per_sec).min(self.max_per_sec);
        self.last_refill = Instant::now();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-session invalid share counter
// ─────────────────────────────────────────────────────────────────────────────

pub struct InvalidShareCounter {
    count: u32,
    max: u32,
}

impl InvalidShareCounter {
    pub fn new(max: u32) -> Self {
        Self { count: 0, max }
    }

    /// Returns `true` if the session should be disconnected.
    pub fn record_invalid(&mut self) -> bool {
        if self.max == 0 {
            return false; // disabled
        }
        self.count += 1;
        if self.count >= self.max {
            warn!("Session exceeded max invalid shares ({})", self.max);
            return true;
        }
        false
    }

    #[allow(dead_code)]
    pub fn count(&self) -> u32 {
        self.count
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Convenience guard — holds all security state for one session
// ─────────────────────────────────────────────────────────────────────────────

pub struct SessionGuard {
    pub share_rate: ShareRateLimiter,
    pub invalid_shares: InvalidShareCounter,
    pub max_message_bytes: usize,
}

impl SessionGuard {
    pub fn new(cfg: &SecurityConfig) -> Self {
        Self {
            share_rate: ShareRateLimiter::new(cfg.max_shares_per_sec),
            invalid_shares: InvalidShareCounter::new(cfg.max_invalid_shares),
            max_message_bytes: cfg.max_message_bytes,
        }
    }

    /// Enforce message size limit. Returns Err if too large.
    pub fn check_message_size(&self, len: usize) -> Result<(), crate::error::PoolError> {
        if len > self.max_message_bytes {
            Err(crate::error::PoolError::MessageTooLarge { bytes: len })
        } else {
            Ok(())
        }
    }
}
