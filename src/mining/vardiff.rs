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

/// The share record a worker's hashrate windows are computed from, detached
/// from the session that collected it.
///
/// A session's `Vardiff` dies with its connection. Without this, a miner
/// restart (a new TCP session seconds after the old one closed) began every
/// window from an empty record, so the 3h and 24h columns dropped to the
/// 60s figure and took hours to recover, while the shares those windows are
/// meant to cover had all been accepted and were merely forgotten. The
/// session hands its history to the stats collector on disconnect, keyed by
/// worker, and the worker's next session takes it back on authorize. Only
/// the estimate inputs travel; retarget state stays with the session.
pub struct ShareHistory {
    share_times: VecDeque<(Instant, u64)>,
    started_at: Instant,
}

impl ShareHistory {
    /// Rebuild a record from persisted `(unix_ts, credited_difficulty)` rows,
    /// oldest first, for a worker returning after a pool restart.
    ///
    /// The observation anchor is the earliest row: the worker was certainly
    /// being observed from then on, and a session's own anchor sits within
    /// one share interval of its first share anyway. Rows past retention are
    /// skipped. Returns `None` when nothing usable remains, so the caller
    /// starts fresh rather than adopting an empty record with a stale anchor.
    pub fn from_unix_rows(rows: &[(u64, u64)], now_unix: u64) -> Option<Self> {
        let now = Instant::now();
        let share_times: VecDeque<(Instant, u64)> = rows
            .iter()
            .filter_map(|&(ts, diff)| {
                let age = Duration::from_secs(now_unix.saturating_sub(ts));
                if age > SHARE_RETENTION {
                    return None;
                }
                now.checked_sub(age).map(|at| (at, diff))
            })
            .collect();
        let started_at = share_times.front()?.0;
        Some(Self {
            share_times,
            started_at,
        })
    }
}

pub struct Vardiff {
    cfg: VardiffConfig,
    /// Ring buffer of (arrival_time, assigned_difficulty) for hashrate estimation.
    /// Each entry stores the session's assigned difficulty at the time the share was accepted.
    share_times: VecDeque<(Instant, u64)>,
    last_retarget: Instant,
    /// Current difficulty assigned to this session
    pub current: u64,
    /// The difficulty assigned before the most recent change, so a share the
    /// miner started before a retarget is still credited with the target it
    /// was mined against rather than falling through to the floor.
    previous: u64,
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
            previous: initial_difficulty,
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
        self.set_current(clamped);
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

    fn set_current(&mut self, difficulty: u64) {
        if difficulty != self.current {
            self.previous = self.current;
            self.current = difficulty;
        }
    }

    /// The difficulty to credit a validated share with in the hashrate
    /// estimate, given the difficulty its hash actually reached.
    ///
    /// A share is evidence of `D × 2³²` expected hashes only for the target
    /// `D` the miner was working against, so the credit must be a threshold
    /// the miner was aiming at, never the hash's own difficulty: the expected
    /// difficulty of a hash that clears `D` is far above `D`, and crediting it
    /// would inflate the estimate many times over.
    ///
    /// Shares are accepted at the pool floor because some hardware fixes its
    /// threshold at connect time and never follows `set_difficulty`. For such
    /// a device the assigned difficulty is not what it mined against, and
    /// crediting it anyway (as the pool used to) overstated the device by
    /// assigned/floor, up to 256× with the default range, and pushed that
    /// figure into the persisted all-time best-hashrate watermark. So: a hash
    /// that clears the assigned difficulty is credited with it; one that only
    /// clears the previous assignment (in-flight work across a retarget) is
    /// credited with that; anything else can only be known to have cleared the
    /// floor, and is credited with the floor.
    pub fn credit_for(&self, hash_difficulty: u64) -> u64 {
        if hash_difficulty >= self.current {
            self.current
        } else if hash_difficulty >= self.previous {
            self.previous
        } else {
            self.cfg.min_difficulty
        }
    }

    /// Record a valid share submission.
    /// `assigned_difficulty` is the difficulty credited to this share (see
    /// `credit_for`). This is used to estimate hashrate:
    /// H/s ≈ Σ(credited_diff) × 2³² / elapsed.
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

    /// Detach the share record for handoff to the worker's next session,
    /// leaving this session with an empty record anchored now.
    pub fn take_share_history(&mut self) -> ShareHistory {
        ShareHistory {
            share_times: std::mem::take(&mut self.share_times),
            started_at: std::mem::replace(&mut self.started_at, Instant::now()),
        }
    }

    /// Adopt a previous session's share record ahead of this session's own.
    ///
    /// The observation anchor moves back to the earlier of the two starts, so
    /// a window the previous session had already filled reports at full
    /// width immediately instead of waiting out `MIN_OBSERVATION` and then
    /// dividing by the new session's age. Entries past `SHARE_RETENTION` are
    /// dropped on the way in; a window that the gap outlasted simply finds
    /// no shares inside it, which is the correct reading for that outage.
    pub fn restore_share_history(&mut self, history: ShareHistory) {
        let now = Instant::now();
        let mut merged: VecDeque<(Instant, u64)> = history
            .share_times
            .into_iter()
            .filter(|&(t, _)| now.duration_since(t) <= SHARE_RETENTION)
            .collect();
        merged.extend(self.share_times.drain(..));
        self.share_times = merged;
        self.started_at = self.started_at.min(history.started_at);
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
                self.set_current(new_diff);
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
            self.set_current(new_diff);
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
    fn credit_is_the_threshold_the_share_cleared_never_the_hash() {
        let mut vd = Vardiff::new(cfg(), 100_000);
        // Clears the assignment: credited with the assignment, not the hash.
        assert_eq!(vd.credit_for(100_000), 100_000);
        assert_eq!(vd.credit_for(u64::MAX), 100_000);
        // Below the assignment, above the floor: this miner is not following
        // set_difficulty, so only the floor is known to have been targeted.
        assert_eq!(vd.credit_for(50_000), 1024);
        assert_eq!(vd.credit_for(1024), 1024);
        // After a raise, work started under the old assignment is still
        // credited with the old assignment rather than the floor.
        vd.set_current(400_000);
        assert_eq!(vd.credit_for(400_000), 400_000);
        assert_eq!(vd.credit_for(100_000), 100_000);
        assert_eq!(vd.credit_for(99_999), 1024);
    }

    #[test]
    fn floor_only_miner_is_estimated_at_the_floor() {
        // A device fixed at the floor keeps submitting floor shares while
        // vardiff raises its assignment. Crediting the assignment made its
        // hashrate read assignment/floor times too high.
        let mut vd = Vardiff::new(cfg(), 262_144);
        let ts = Instant::now() - Duration::from_secs(600);
        vd.started_at = ts;
        // Shares strictly inside the window: the first one sits a second in
        // from the edge so clock skew cannot drop it.
        for i in 0..40u64 {
            let credited = vd.credit_for(1500); // clears 1024, not 262_144
            vd.share_times
                .push_back((ts + Duration::from_secs(1 + i * 14), credited));
        }
        let hps = vd.estimated_hashrate_in_window(Duration::from_secs(600));
        let expected = 40.0 * 1024.0 * 4_294_967_296.0 / 600.0;
        assert!(
            (hps - expected).abs() / expected < 0.01,
            "{hps} vs {expected}"
        );
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

    #[test]
    fn share_history_carries_the_long_windows_across_a_reconnect() {
        // Three hours of steady shares, then the miner restarts: the old
        // session hands off its record and a brand-new session takes it.
        let diff = 100_000;
        let three_hours = Duration::from_secs(10_800);
        let mut old = aged(diff, 720, Duration::from_secs(15), three_hours);
        let before = old.estimated_hashrate_in_window(three_hours);
        assert!(before > 0.0);

        let history = old.take_share_history();
        // The closing session is left empty rather than double-counted.
        assert_eq!(old.estimated_hashrate_in_window(three_hours), 0.0);

        let mut new = Vardiff::new(cfg(), diff);
        // Before the handoff a fresh session reports nothing at all.
        assert_eq!(new.estimated_hashrate_in_window(three_hours), 0.0);
        new.restore_share_history(history);

        let after = new.estimated_hashrate_in_window(three_hours);
        assert!(
            (after - before).abs() / before < 0.01,
            "{after} vs {before}"
        );
        // Retarget state did not travel: the new session still owes a full
        // interval before it may adjust difficulty.
        assert!(new.check_retarget().is_none());
    }

    #[test]
    fn restored_history_drops_shares_past_retention_and_keeps_new_ones() {
        let diff = 100_000;
        let mut old = Vardiff::new(cfg(), diff);
        let now = Instant::now();
        old.started_at = now.checked_sub(Duration::from_secs(90_000)).unwrap();
        // One share outside retention, one well inside.
        old.share_times.push_back((
            now - Duration::from_secs(SHARE_RETENTION.as_secs() + 60),
            diff,
        ));
        old.share_times
            .push_back((now - Duration::from_secs(60), diff));
        let history = old.take_share_history();

        let mut new = Vardiff::new(cfg(), diff);
        new.record_share(diff);
        new.restore_share_history(history);

        assert_eq!(new.share_times.len(), 2);
        // Oldest first, so the restored share precedes this session's own.
        assert!(new.share_times[0].0 < new.share_times[1].0);
        assert!(new.started_at <= now - Duration::from_secs(90_000));
    }

    #[test]
    fn persisted_rows_rebuild_a_full_width_window() {
        // Three hours of 15-second shares as they would come back from the
        // share log, anchored at the earliest row: the 3h window reports at
        // full width straight away instead of dividing by a fresh session's
        // age.
        let diff = 100_000u64;
        let now_unix = 1_800_000_000u64;
        let rows: Vec<(u64, u64)> = (0..720u64)
            .map(|i| (now_unix - 10_800 + 1 + i * 15, diff))
            .collect();
        let history = ShareHistory::from_unix_rows(&rows, now_unix).expect("rows inside retention");
        let mut vd = Vardiff::new(cfg(), diff);
        vd.restore_share_history(history);

        let hps = vd.estimated_hashrate_in_window(Duration::from_secs(10_800));
        let expected = 720.0 * diff as f64 * 4_294_967_296.0 / 10_800.0;
        assert!(
            (hps - expected).abs() / expected < 0.01,
            "{hps} vs {expected}"
        );

        // Rows past retention are ignored, and nothing usable means no record.
        let stale = [(now_unix - SHARE_RETENTION.as_secs() - 1, diff)];
        assert!(ShareHistory::from_unix_rows(&stale, now_unix).is_none());
        assert!(ShareHistory::from_unix_rows(&[], now_unix).is_none());
    }
}
