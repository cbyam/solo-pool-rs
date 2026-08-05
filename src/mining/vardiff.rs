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

/// Shortest observation period that yields a reportable hashrate.
///
/// The estimate divides accumulated share work by how long the session has been
/// observed, so a session only milliseconds old would divide by ~0 and report an
/// astronomical rate. Because the all-time watermark is a monotonic maximum
/// persisted to SQLite, one such reading poisons it permanently. Report nothing
/// until there is a real time base to divide by.
const MIN_OBSERVATION: Duration = Duration::from_secs(30);

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
    /// When this session started, so the hashrate estimate can divide by the
    /// period actually observed rather than by the gap between two shares.
    started_at: Instant,
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
            started_at: Instant::now(),
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
    ///
    /// Divides the share work accumulated inside `window` by the length of the
    /// period actually observed: the whole window once the session is at least
    /// that old, and the session's age before then.
    ///
    /// The denominator deliberately does NOT come from the span between the
    /// first and last share. Anchoring on shares makes the time base collapse
    /// whenever a session submits a few shares close together and then goes
    /// quiet: two shares 155us apart in a ten-minute window produced a reading
    /// of ~2.3e17 H/s, which is how an all-time watermark ends up hundreds of
    /// times above anything the hardware can produce. A fixed observation
    /// period cannot collapse, so the estimate is bounded by the work actually
    /// proven, and it can only read high if the shares were really submitted.
    ///
    /// Returns 0.0 before `MIN_OBSERVATION` has elapsed, and 0.0 when no shares
    /// fall inside the window.
    pub fn estimated_hashrate_in_window(&self, window: std::time::Duration) -> f64 {
        let now = std::time::Instant::now();

        // Observation period: the window, or the whole session if it is younger.
        let observed = now.duration_since(self.started_at).min(window);
        if observed < MIN_OBSERVATION.min(window) {
            return 0.0;
        }

        // Every share inside the window counts. The n/(n-1) correction that a
        // share-anchored interval needs does not apply here: the period is
        // fixed independently of when the shares landed, so counting all of
        // them is unbiased.
        let sum_diff: u64 = self
            .share_times
            .iter()
            .filter(|&&(ts, _)| now.duration_since(ts) <= window)
            .map(|&(_, diff)| diff)
            .sum();

        if sum_diff == 0 {
            return 0.0;
        }

        // Standard Bitcoin hashrate formula: difficulty × 2³² hashes per share
        (sum_diff as f64 * 4_294_967_296.0) / observed.as_secs_f64()
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

    /// Build a Vardiff whose session is old enough to report, with `shares`
    /// entries of difficulty `diff` placed `spacing` apart ending now.
    fn aged(diff: u64, shares: u64, spacing: Duration, age: Duration) -> Vardiff {
        let mut vd = Vardiff::new(cfg(), diff);
        vd.started_at = Instant::now().checked_sub(age).expect("test clock");
        for i in 0..shares {
            let back = spacing * ((shares - 1 - i) as u32);
            let ts = Instant::now().checked_sub(back).expect("test clock");
            vd.share_times.push_back((ts, diff));
        }
        vd
    }

    #[test]
    fn a_burst_of_shares_cannot_produce_an_absurd_hashrate() {
        // The real failure that poisoned a production all-time watermark: a
        // session submitted two shares a fraction of a millisecond apart and
        // then went quiet. Anchoring the denominator on those shares gave
        // ~2.3e17 H/s, roughly 800x above anything the hardware could do, and
        // the monotonic watermark kept it forever.
        let vd = aged(
            4096,
            2,
            Duration::from_micros(155),
            Duration::from_secs(600),
        );
        let hps = vd.estimated_hashrate_in_window(Duration::from_secs(600));

        // Two shares of difficulty 4096 over ten minutes is ~59 MH/s.
        let expected = 2.0 * 4096.0 * 4_294_967_296.0 / 600.0;
        assert!(
            (hps - expected).abs() / expected < 0.01,
            "expected ~{expected:.3e} H/s, got {hps:.3e}"
        );
        assert!(
            hps < 1e12,
            "a two-share burst must not report terahashes: {hps:.3e}"
        );
    }

    #[test]
    fn no_estimate_until_there_is_a_time_base() {
        // A session milliseconds old would divide by ~0. Report nothing until
        // the observation period is real.
        let vd = aged(4096, 5, Duration::from_millis(1), Duration::from_millis(50));
        assert_eq!(
            vd.estimated_hashrate_in_window(Duration::from_secs(600)),
            0.0
        );
    }

    #[test]
    fn hashrate_estimate_is_unbiased_for_a_steady_miner() {
        // A miner producing one share of difficulty D every second is doing
        // D * 2^32 hashes per second. Counting the boundary share used to
        // report n/(n-1) of that: +25% here, and double at two shares.
        // 60 shares of difficulty 1000, one per second, over a 60s window that
        // the session has fully covered: exactly 1000 * 2^32 H/s.
        let vd = aged(
            1_000,
            60,
            Duration::from_secs(1),
            Duration::from_secs(3_600),
        );
        let hps = vd.estimated_hashrate_in_window(Duration::from_secs(60));
        let expected = 60.0 * 1_000.0 * 4_294_967_296.0 / 60.0;
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
        let vd = aged(100_000, 2, Duration::from_secs(1), Duration::from_secs(120));

        let century = Duration::from_secs(86_400 * 365 * 100);
        // Both shares fall inside the window, and the denominator is the
        // session's age rather than the century, so this is a real rate rather
        // than the "no data" zero.
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
