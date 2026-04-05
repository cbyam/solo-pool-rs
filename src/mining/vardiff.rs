/// mining/vardiff.rs
///
/// Per-session variable difficulty (vardiff).
///
/// Algorithm:
///   - Track share submission timestamps in a sliding window
///   - At each retarget interval, compute actual share rate vs target
///   - Scale difficulty proportionally, clamped by min/max and max_factor
///   - Return the new difficulty so the caller can send `set_difficulty`
use crate::config::VardiffConfig;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub struct Vardiff {
    cfg: VardiffConfig,
    /// Ring buffer of (arrival_time, assigned_difficulty) for hashrate estimation.
    /// Each entry stores the session's assigned difficulty at the time the share was accepted.
    share_times: VecDeque<(Instant, u64)>,
    last_retarget: Instant,
    /// Current difficulty assigned to this session
    pub current: u64,
    /// Number of valid shares since last retarget
    shares_since_retarget: u64,
}

impl Vardiff {
    pub fn new(cfg: VardiffConfig, initial_difficulty: u64) -> Self {
        Self {
            current: initial_difficulty,
            cfg,
            share_times: VecDeque::with_capacity(8_192),
            last_retarget: Instant::now(),
            shares_since_retarget: 0,
        }
    }

    /// Record a valid share submission.
    /// `assigned_difficulty` is the difficulty this session had assigned when the share arrived.
    /// This is used to estimate hashrate: H/s ≈ Σ(assigned_diff) × 2³² / elapsed.
    pub fn record_share(&mut self, assigned_difficulty: u64) {
        self.shares_since_retarget += 1;
        let now = Instant::now();
        self.share_times.push_back((now, assigned_difficulty));
        // Evict old entries (keep only last 24 hours)
        let cutoff = now - Duration::from_secs(86_400);
        while self.share_times.front().is_some_and(|&(t, _)| t < cutoff) {
            self.share_times.pop_front();
        }
    }

    /// Check if a retarget is due. Returns `Some(new_difficulty)` when the
    /// difficulty should change.
    pub fn check_retarget(&mut self) -> Option<u64> {
        let elapsed = self.last_retarget.elapsed().as_secs_f64();
        let interval = self.cfg.retarget_interval_secs as f64;

        if elapsed < interval {
            return None;
        }

        let shares = self.shares_since_retarget;
        self.shares_since_retarget = 0;
        self.last_retarget = Instant::now();

        if shares == 0 {
            // No shares in this window — halve difficulty so a slow/paused miner
            // gets an easier target on reconnect, flooring at min_difficulty.
            let new_diff = (self.current / 2).max(self.cfg.min_difficulty);
            if new_diff != self.current {
                self.current = new_diff;
                return Some(new_diff);
            }
            return None;
        }

        // Actual seconds per share during this window
        let actual_sps = elapsed / shares as f64;
        let target_sps = self.cfg.target_share_time_secs as f64;

        // Scale: if shares came in too fast (actual_sps < target_sps), raise difficulty
        let ratio = target_sps / actual_sps;

        // Clamp ratio to ±max_retarget_factor
        let factor = self.cfg.max_retarget_factor;
        let clamped_ratio = ratio.clamp(1.0 / factor, factor);

        let new_diff_f = self.current as f64 * clamped_ratio;
        let new_diff = (new_diff_f as u64).clamp(self.cfg.min_difficulty, self.cfg.max_difficulty);

        // Only emit if meaningfully different (>5% change)
        let pct_change = ((new_diff as f64 - self.current as f64) / self.current as f64).abs();
        if pct_change > 0.05 && new_diff != self.current {
            tracing::debug!(
                old = self.current,
                new = new_diff,
                actual_sps = format!("{:.1}", actual_sps),
                "vardiff retarget"
            );
            self.current = new_diff;
            Some(new_diff)
        } else {
            None
        }
    }

    /// Estimated hashrate in H/s over an arbitrary lookback `window`.
    /// Returns 0.0 if fewer than two shares are present (not enough data to measure a rate).
    pub fn estimated_hashrate_in_window(&self, window: std::time::Duration) -> f64 {
        if self.share_times.len() < 2 {
            return 0.0;
        }

        let now = std::time::Instant::now();
        let cutoff = now - window;

        let mut sum_diff: u64 = 0;
        let mut oldest_ts = None;

        for &(ts, diff) in self.share_times.iter() {
            if ts >= cutoff {
                if oldest_ts.is_none() {
                    oldest_ts = Some(ts);
                }
                sum_diff += diff;
            }
        }

        let oldest_ts = match oldest_ts {
            Some(ts) => ts,
            None => return 0.0,
        };

        let elapsed = now.duration_since(oldest_ts).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }

        // Standard Bitcoin hashrate formula: difficulty × 2³² hashes per share
        (sum_diff as f64 * 4_294_967_296.0) / elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VardiffConfig {
        VardiffConfig {
            target_share_time_secs: 15,
            retarget_interval_secs: 60,
            min_difficulty: 1024,
            max_difficulty: 1_000_000_000,
            max_retarget_factor: 4.0,
        }
    }

    #[test]
    fn no_retarget_before_interval() {
        let mut vd = Vardiff::new(cfg(), 100_000);
        for _ in 0..10 {
            vd.record_share(100_000);
        }
        // No retarget should happen immediately
        assert!(vd.check_retarget().is_none());
    }

    #[test]
    fn zero_shares_halves_difficulty() {
        let mut vd = Vardiff::new(cfg(), 100_000);
        // Force the last retarget to be far in the past
        vd.last_retarget = Instant::now() - Duration::from_secs(120);
        let result = vd.check_retarget();
        assert_eq!(result, Some(50_000));
    }
}
