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

/// How far back the share ring buffer is kept for hashrate estimation.
const SHARE_RETENTION: Duration = Duration::from_secs(86_400);

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
    /// Whether a `mining.suggest_difficulty` has already been granted its one
    /// fresh retarget window this session.
    suggest_applied: bool,
}

impl Vardiff {
    pub fn new(cfg: VardiffConfig, initial_difficulty: u64) -> Self {
        Self {
            current: initial_difficulty,
            cfg,
            share_times: VecDeque::with_capacity(8_192),
            last_retarget: Instant::now(),
            shares_since_retarget: 0,
            suggest_applied: false,
        }
    }

    /// Seed the working difficulty from a miner's `mining.suggest_difficulty`
    /// hint, clamped to the configured floor/ceiling, and give it a fresh
    /// retarget window. Returns the applied (clamped) value. Vardiff retains
    /// full authority afterwards — this only sets the starting point.
    pub fn suggest(&mut self, difficulty: u64) -> u64 {
        let clamped = difficulty.clamp(self.cfg.min_difficulty, self.cfg.max_difficulty);
        self.current = clamped;
        // The fresh window is granted once. Clearing `shares_since_retarget`
        // without also moving `last_retarget` would let the next retarget judge
        // an empty share count over a long elapsed and halve the difficulty, so
        // the two move together — but only for the first suggestion. Honouring
        // every suggestion would let a client that re-suggests faster than
        // `retarget_interval_secs` hold `last_retarget` perpetually fresh and
        // pin its difficulty at the floor forever, which is precisely the
        // authority this is documented not to give up.
        if !self.suggest_applied {
            self.suggest_applied = true;
            self.last_retarget = Instant::now();
            self.shares_since_retarget = 0;
        }
        clamped
    }

    /// Record a valid share submission.
    /// `assigned_difficulty` is the difficulty this session had assigned when the share arrived.
    /// This is used to estimate hashrate: H/s ≈ Σ(assigned_diff) × 2³² / elapsed.
    pub fn record_share(&mut self, assigned_difficulty: u64) {
        self.shares_since_retarget += 1;
        let now = Instant::now();
        self.share_times.push_back((now, assigned_difficulty));
        // Evict old entries (keep only the retention window). Compare elapsed
        // durations rather than deriving a `now - RETENTION` cutoff: on platforms
        // where `Instant` is an unsigned counter, subtracting a window wider than
        // the process/host clock underflows and panics. Linux stores a signed
        // timespec and tolerates it, so this is portability hygiene, not a live
        // bug fix.
        while self
            .share_times
            .front()
            .is_some_and(|&(t, _)| now.duration_since(t) > SHARE_RETENTION)
        {
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

        let mut sum_diff: u64 = 0;
        let mut oldest_ts = None;

        // `now.duration_since(ts) <= window` rather than `ts >= now - window`,
        // for the portability reason in `record_share`.
        for &(ts, diff) in self.share_times.iter() {
            if now.duration_since(ts) <= window {
                if oldest_ts.is_none() {
                    // The oldest in-window share defines the start of the
                    // measurement interval, so the work it represents was
                    // finished *before* that interval began. Counting it
                    // inflates the estimate by n/(n-1): double at two shares in
                    // window, +25% at five, negligible once there are hundreds.
                    // The short windows are exactly where n is small.
                    oldest_ts = Some(ts);
                    continue;
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

    #[test]
    fn hashrate_estimate_is_unbiased_for_a_steady_miner() {
        // A miner producing one share of difficulty D every second is doing
        // D * 2^32 hashes per second. Counting the boundary share used to
        // report n/(n-1) of that: +25% here, and double at two shares.
        let mut vd = Vardiff::new(cfg(), 1_000);
        let base = Instant::now();
        for i in 0..5u64 {
            // Oldest first, one second apart, the newest 0s ago.
            let ts = base
                .checked_sub(Duration::from_secs(4 - i))
                .expect("test clock");
            vd.share_times.push_back((ts, 1_000));
        }

        let hps = vd.estimated_hashrate_in_window(Duration::from_secs(100));
        let expected = 1_000.0 * 4_294_967_296.0;
        let ratio = hps / expected;
        assert!(
            (0.98..=1.02).contains(&ratio),
            "estimate off by {ratio:.3}x (got {hps:.0}, want {expected:.0})"
        );
    }

    #[test]
    fn repeated_suggestions_cannot_postpone_retargeting_forever() {
        // A client re-suggesting the floor faster than retarget_interval_secs
        // used to keep last_retarget perpetually fresh, pinning its difficulty
        // at the floor and flooding the pool with cheap shares.
        // Well above the floor, so a zero-share retarget produces a visible
        // halving rather than clamping back to the same value.
        let mut vd = Vardiff::new(cfg(), 100_000);
        assert_eq!(vd.suggest(80_000), 80_000);

        // Age the window past the retarget interval, then suggest again.
        vd.last_retarget = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .expect("test clock");
        assert_eq!(vd.suggest(80_000), 80_000, "value still honoured");

        // The retarget must still be due: the second suggestion must not have
        // reset the clock.
        assert_eq!(
            vd.check_retarget(),
            Some(40_000),
            "a re-suggestion postponed the retarget"
        );
    }

    #[test]
    fn window_wider_than_the_clock_counts_every_share() {
        // Guards the elapsed-duration comparison against regressing to a
        // `now - window` cutoff, which underflows on platforms whose `Instant` is
        // an unsigned counter. Passes either way on Linux — it documents the
        // intended behavior rather than reproducing a Linux failure.
        let mut vd = Vardiff::new(cfg(), 100_000);
        vd.record_share(100_000);
        vd.record_share(100_000);

        let century = Duration::from_secs(86_400 * 365 * 100);
        // Every share falls inside the window, so this is a real (very large)
        // rate rather than the "no data" zero.
        assert!(vd.estimated_hashrate_in_window(century) > 0.0);
    }

    #[test]
    fn suggest_clamps_to_floor_and_ceiling() {
        // cfg(): floor 1024, ceiling 1_000_000_000.
        let mut vd = Vardiff::new(cfg(), 100_000);
        // Below floor → clamped up to the floor (a hostile/buggy suggestion can
        // never push a miner below the share-rate floor).
        assert_eq!(vd.suggest(1), 1024);
        assert_eq!(vd.current, 1024);
        // Above ceiling → clamped down.
        assert_eq!(vd.suggest(5_000_000_000), 1_000_000_000);
        // In range → applied verbatim.
        assert_eq!(vd.suggest(50_000), 50_000);
        assert_eq!(vd.current, 50_000);
    }
}
