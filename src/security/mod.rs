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

/// Upper bound on distinct IPs the ban list and the connection rate limiter
/// each track. Both are keyed on the exact peer address, and a peer with a
/// routed IPv6 prefix has an effectively unlimited supply of those, so without
/// a cap either map grows by one entry per connection attempt until the next
/// five-minute prune. Keying on a /64 instead would make one bad device
/// throttle or ban every IPv6 miner on the same LAN segment, which is the
/// wrong trade for a pool that lives on a LAN; a cap keeps exact keys and
/// bounds memory instead. 65k entries is a few MB at most.
const MAX_TRACKED_IPS: usize = 65_536;

struct BanEntry {
    until: Instant,
    #[allow(dead_code)]
    reason: String,
}

pub struct BanList {
    entries: DashMap<IpAddr, BanEntry>,
    ban_duration: Duration,
    max_entries: usize,
}

impl BanList {
    pub fn new(ban_duration_secs: u64) -> Arc<Self> {
        Self::with_capacity_limit(ban_duration_secs, MAX_TRACKED_IPS)
    }

    fn with_capacity_limit(ban_duration_secs: u64, max_entries: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: DashMap::new(),
            ban_duration: Duration::from_secs(ban_duration_secs),
            max_entries,
        })
    }

    pub fn ban(&self, ip: IpAddr, reason: &str) {
        warn!("Banning {ip} for {:?}: {reason}", self.ban_duration);
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(&ip) {
            self.prune();
            if self.entries.len() >= self.max_entries {
                // Still full of live bans: drop the one closest to expiry so
                // the newest offender is always recorded.
                if let Some(victim) = self
                    .entries
                    .iter()
                    .min_by_key(|e| e.until)
                    .map(|e| *e.key())
                {
                    self.entries.remove(&victim);
                }
            }
        }
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
    max_tracked: usize,
}

impl ConnectionRateLimiter {
    pub fn new(max_per_minute: u32) -> Arc<Self> {
        Self::with_capacity_limit(max_per_minute, MAX_TRACKED_IPS)
    }

    fn with_capacity_limit(max_per_minute: u32, max_tracked: usize) -> Arc<Self> {
        Arc::new(Self {
            windows: DashMap::new(),
            max_per_minute,
            max_tracked,
        })
    }

    /// Returns `true` if this connection should be allowed.
    pub fn check_and_record(&self, ip: IpAddr) -> bool {
        // `Instant` is monotonic-since-boot on Linux, so plain subtraction
        // panics when the host has been up for less than the window. `None`
        // means every recorded timestamp is necessarily inside the window.
        let one_minute_ago = Instant::now().checked_sub(Duration::from_secs(60));

        if self.windows.len() >= self.max_tracked && !self.windows.contains_key(&ip) {
            self.prune();
            if self.windows.len() >= self.max_tracked {
                // Every tracked address connected within the last minute and
                // this is yet another new one: that is a flood of fresh
                // sources, and refusing it is the point of the limiter.
                return false;
            }
        }
        let mut entry = self.windows.entry(ip).or_default();

        // Evict old entries
        if let Some(cutoff) = one_minute_ago {
            entry.retain(|&t| t > cutoff);
        }

        if entry.len() >= self.max_per_minute as usize {
            return false;
        }
        entry.push(Instant::now());
        true
    }

    /// Periodic cleanup — drop per-IP windows whose timestamps have all aged out.
    /// Without this the map grows one permanent entry per distinct source IP
    /// (spoofed / IPv6 ranges), since `check_and_record` only trims an entry when
    /// that same IP reconnects. Call from a background task every few minutes.
    pub fn prune(&self) {
        let Some(one_minute_ago) = Instant::now().checked_sub(Duration::from_secs(60)) else {
            return; // host up < 60s — nothing can be stale yet
        };
        self.windows
            .retain(|_, times| times.iter().any(|&t| t > one_minute_ago));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Worker-name validation
// ─────────────────────────────────────────────────────────────────────────────

/// Validate an untrusted worker/user identity before it is stored as a key in
/// the global stats maps and Prometheus metric labels, or rendered on the
/// dashboard. Rejects empty, over-long, and control/whitespace-bearing names so
/// an attacker cannot grow those maps without bound or inject newlines/control
/// characters into logs, metric exposition, or the dashboard HTML.
///
/// `max_len` is a byte cap (128 by default) — comfortably larger than a bech32m
/// payout address plus a `.workername` suffix, so legitimate miners are unaffected.
pub fn validate_worker_name(name: &str, max_len: usize) -> Result<(), crate::error::PoolError> {
    let invalid = |detail: &str| crate::error::PoolError::InvalidParams {
        method: "worker name",
        detail: detail.into(),
    };
    if name.is_empty() {
        return Err(invalid("worker name must not be empty"));
    }
    if name.len() > max_len {
        return Err(invalid("worker name too long"));
    }
    if name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid(
            "worker name must not contain control or whitespace characters",
        ));
    }
    Ok(())
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
    pub max_worker_name_len: usize,
    authorizations: u32,
    max_authorizations: u32,
}

impl SessionGuard {
    pub fn new(cfg: &SecurityConfig) -> Self {
        Self {
            share_rate: ShareRateLimiter::new(cfg.max_shares_per_sec),
            invalid_shares: InvalidShareCounter::new(cfg.max_invalid_shares),
            max_message_bytes: cfg.max_message_bytes,
            max_worker_name_len: cfg.max_worker_name_len,
            authorizations: 0,
            max_authorizations: cfg.max_authorizations_per_session,
        }
    }

    /// Record one more *distinct* worker identity authorized on this session.
    /// Returns `false` once the configured cap is exceeded — the caller should
    /// disconnect. A cap of 0 disables the limit.
    pub fn record_new_authorization(&mut self) -> bool {
        if self.max_authorizations == 0 {
            return true;
        }
        self.authorizations += 1;
        if self.authorizations > self.max_authorizations {
            warn!(
                "Session exceeded max distinct worker identities ({})",
                self.max_authorizations
            );
            return false;
        }
        true
    }

    /// Enforce message size limit. Returns Err if too large.
    pub fn check_message_size(&self, len: usize) -> Result<(), crate::error::PoolError> {
        if len > self.max_message_bytes {
            Err(crate::error::PoolError::MessageTooLarge { bytes: len })
        } else {
            Ok(())
        }
    }

    /// Validate an untrusted worker/user-identity name against the configured cap.
    pub fn check_worker_name(&self, name: &str) -> Result<(), crate::error::PoolError> {
        validate_worker_name(name, self.max_worker_name_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    /// Distinct addresses from one routed /64, the cheap identity supply the
    /// caps exist for.
    fn v6(n: u64) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(
            ((0x2001_0db8_0000_0000u64 as u128) << 64) | n as u128,
        ))
    }

    #[test]
    fn rate_limiter_refuses_new_sources_once_full() {
        let rl = ConnectionRateLimiter::with_capacity_limit(8, 4);
        for i in 0..4 {
            assert!(rl.check_and_record(v6(i)));
        }
        assert_eq!(rl.windows.len(), 4);
        // A fifth fresh source within the window is refused and not stored.
        assert!(!rl.check_and_record(v6(99)));
        assert_eq!(rl.windows.len(), 4);
        // A source already tracked is still judged on its own window.
        assert!(rl.check_and_record(v6(1)));
    }

    #[test]
    fn ban_list_stays_bounded_and_keeps_the_newest_offender() {
        let bl = BanList::with_capacity_limit(600, 3);
        for i in 0..3 {
            bl.ban(v6(i), "test");
        }
        bl.ban(v6(42), "test");
        assert_eq!(bl.entries.len(), 3);
        assert!(bl.is_banned(&v6(42)), "the newest ban must be recorded");
        // The evicted one is the earliest-expiring, which is the oldest.
        assert!(!bl.is_banned(&v6(0)));
    }

    #[test]
    fn worker_name_accepts_typical_addresses() {
        assert!(validate_worker_name("bc1qexampleaddress", 128).is_ok());
        assert!(validate_worker_name("bc1q...address.nerdqaxe01", 128).is_ok());
    }

    #[test]
    fn worker_name_rejects_empty_and_overlong() {
        assert!(validate_worker_name("", 128).is_err());
        let long = "a".repeat(129);
        assert!(validate_worker_name(&long, 128).is_err());
        assert!(validate_worker_name(&"a".repeat(128), 128).is_ok());
    }

    #[test]
    fn worker_name_rejects_control_and_whitespace() {
        assert!(validate_worker_name("bad\nname", 128).is_err());
        assert!(validate_worker_name("bad name", 128).is_err());
        assert!(validate_worker_name("bad\tname", 128).is_err());
        assert!(validate_worker_name("bad\0name", 128).is_err());
    }

    #[test]
    fn authorization_cap_allows_up_to_max_then_rejects() {
        let cfg = SecurityConfig {
            max_connections_per_ip: 5,
            max_shares_per_sec: 500,
            ban_duration_secs: 0,
            max_invalid_shares: 5,
            max_message_bytes: 4096,
            max_worker_name_len: 128,
            max_authorizations_per_session: 2,
        };
        let mut guard = SessionGuard::new(&cfg);
        assert!(guard.record_new_authorization());
        assert!(guard.record_new_authorization());
        assert!(!guard.record_new_authorization());
    }

    #[test]
    fn authorization_cap_zero_disables_limit() {
        let cfg = SecurityConfig {
            max_connections_per_ip: 5,
            max_shares_per_sec: 500,
            ban_duration_secs: 0,
            max_invalid_shares: 5,
            max_message_bytes: 4096,
            max_worker_name_len: 128,
            max_authorizations_per_session: 0,
        };
        let mut guard = SessionGuard::new(&cfg);
        for _ in 0..1000 {
            assert!(guard.record_new_authorization());
        }
    }

    #[test]
    fn rate_limiter_prune_drops_stale_entries() {
        let rl = ConnectionRateLimiter::new(10);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(rl.check_and_record(ip));
        assert_eq!(rl.windows.len(), 1);
        // Force the recorded timestamp to be older than the 60s window.
        rl.windows
            .get_mut(&ip)
            .unwrap()
            .iter_mut()
            .for_each(|t| *t -= Duration::from_secs(120));
        rl.prune();
        assert_eq!(rl.windows.len(), 0);
    }
}
