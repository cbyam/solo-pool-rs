/// mining/vardiff.rs
///
/// Per-session variable difficulty (vardiff).
///
/// Algorithm:
///   - Track share submission timestamps in a sliding window
///   - At each retarget interval, sum the work the accepted shares proved over
///     a trailing window of about twenty target share times and derive the
///     difficulty at which that rate would yield one share per target interval
///   - Leave the difficulty alone while that lands inside a hysteresis band
///     around the current value; otherwise step halfway toward it (in ratio
///     terms), or all the way when it is far off, clamped by min/max and
///     max_factor
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

/// How far the observed share rate may drift from the target, as a ratio in
/// either direction, before a retarget moves the difficulty.
///
/// Share arrivals are Poisson, so an observed rate scatters around the true
/// one even for a miner that is already converged, and the 5% deadband this
/// replaced fired on most windows. Each firing is a `set_difficulty` the miner
/// has to act on. Within this band the difficulty is close enough that the
/// share time stays between two thirds and one and a half times the target.
const RETARGET_HYSTERESIS: f64 = 1.5;

/// How much history a retarget judges, in target share times.
///
/// One retarget interval holds only a handful of shares (six at a 15s target
/// and 90s interval), and a rate read off six Poisson arrivals is off by about
/// 40% one standard deviation of the time. Judged on that alone, the band
/// above was crossed on over a third of windows and the move that followed
/// was usually reversed by the next one. Twenty shares' worth of history
/// brings the scatter to about 22%. The evidence is summed credited work, so
/// shares mined before a difficulty change inside the window still count at
/// what they proved.
const RETARGET_WINDOW_SHARES: u64 = 20;

/// Observed/target ratio beyond which a retarget takes the whole step at once.
///
/// Inside it the step is the square root of the ratio: half the distance in
/// log terms. The next retarget sees much of the same history and finishes
/// the move if the evidence still supports it, while a move that was noise is
/// not amplified into a full swing the other way. A miner three or more times
/// off target is not noise, and waiting on it only delays recovery.
const FULL_STEP_BEYOND: f64 = 3.0;

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
        // The fresh interval is granted once, so the suggested value is mined
        // for a full interval before it is first judged. Honouring every
        // suggestion would let a client that re-suggests faster than
        // `retarget_interval_secs` hold `last_retarget` perpetually fresh and
        // pin its difficulty at the floor forever, which is precisely the
        // authority this is documented not to give up.
        if !self.suggest_applied {
            self.suggest_applied = true;
            self.last_retarget = Instant::now();
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

    fn retarget_interval(&self) -> Duration {
        Duration::from_secs(self.cfg.retarget_interval_secs)
    }

    /// When the next retarget is due. Sessions sleep until this instant and
    /// then call `check_retarget`, so a miner that has gone quiet is still
    /// judged on schedule: silence is evidence too.
    pub fn retarget_due(&self) -> Instant {
        self.last_retarget + self.retarget_interval()
    }

    /// Check if a retarget is due. Returns `Some(new_difficulty)` when the
    /// difficulty should change.
    ///
    /// The miner is judged on the work its shares proved over a trailing
    /// window of `RETARGET_WINDOW_SHARES` target share times (at least one
    /// retarget interval), not on how many shares landed since the last
    /// retarget: a share credited with difficulty `D` is evidence of `D × 2³²`
    /// hashes whatever the session's assignment was at the time. The
    /// difficulty that would turn the observed rate into one share per
    /// `target_share_time_secs` is `Σcredited × target / observed`, the same
    /// estimate `estimated_hashrate_in_window` reports, scaled by the target.
    /// A window with no shares is judged as if a single share at the current
    /// difficulty had landed at its very end: that is the highest rate the
    /// silence is consistent with, so the drop it produces is the smallest
    /// the evidence supports, further limited by `max_retarget_factor`.
    pub fn check_retarget(&mut self) -> Option<u64> {
        // Durations, not floats: the session sleeps until `retarget_due`, and
        // once it wakes this must agree that the retarget is due, or the
        // session would wake again at once and spin.
        if self.last_retarget.elapsed() < self.retarget_interval() {
            return None;
        }
        self.last_retarget = Instant::now();

        let lookback = Duration::from_secs(
            self.cfg
                .target_share_time_secs
                .saturating_mul(RETARGET_WINDOW_SHARES),
        )
        .max(self.retarget_interval());
        let (work, observed) = self.work_in_window(lookback);
        let observed = observed.as_secs_f64();
        if observed <= 0.0 {
            return None;
        }

        let target_sps = self.cfg.target_share_time_secs as f64;
        let proven = if work == 0 {
            self.current as f64
        } else {
            work as f64
        };
        let ideal = proven * target_sps / observed;
        let ratio = ideal / self.current as f64;

        // Inside the band the difference is noise, not drift: leave the
        // miner's work alone.
        if (1.0 / RETARGET_HYSTERESIS..=RETARGET_HYSTERESIS).contains(&ratio) {
            return None;
        }

        let step = if (1.0 / FULL_STEP_BEYOND..=FULL_STEP_BEYOND).contains(&ratio) {
            ratio.sqrt()
        } else {
            ratio
        };
        let factor = self.cfg.max_retarget_factor;
        let new_diff_f = self.current as f64 * step.clamp(1.0 / factor, factor);
        let new_diff =
            (new_diff_f.round() as u64).clamp(self.cfg.min_difficulty, self.cfg.max_difficulty);

        if new_diff == self.current {
            return None;
        }
        tracing::debug!(
            old = self.current,
            new = new_diff,
            observed_ratio = format!("{ratio:.2}"),
            window_secs = observed as u64,
            "vardiff retarget"
        );
        self.set_current(new_diff);
        Some(new_diff)
    }

    /// Credited work inside the trailing `window`, and the length of the
    /// period it was observed over: the whole window once the record is at
    /// least that old, and the record's age before then.
    fn work_in_window(&self, window: Duration) -> (u64, Duration) {
        let now = Instant::now();
        let observed = now.duration_since(self.started_at).min(window);
        let work = self
            .share_times
            .iter()
            .filter(|&&(ts, _)| now.duration_since(ts) <= window)
            .fold(0u64, |sum, &(_, diff)| sum.saturating_add(diff));
        (work, observed)
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
        // Every share inside the window counts. The n/(n-1) correction that a
        // share-anchored interval needs does not apply here: the period is
        // fixed independently of when the shares landed, so counting all of
        // them is unbiased.
        let (sum_diff, observed) = self.work_in_window(window);
        if observed < MIN_OBSERVATION.min(window) {
            return 0.0;
        }

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
    fn zero_shares_drop_by_what_the_silence_proves() {
        // 120s without a share against a 15s target: even one share landing
        // right now would put the miner at an eighth of the rate the
        // difficulty assumes, so the window supports a drop to 12_500. The
        // per-step factor limits it to a quarter.
        let mut vd = window(100_000, 0, 0, 120);
        assert_eq!(vd.check_retarget(), Some(25_000));

        // A window no longer than the target says nothing: a single share
        // could have landed at its end at exactly the target rate.
        let mut vd = Vardiff::new(
            VardiffConfig {
                retarget_interval_secs: 15,
                ..cfg()
            },
            100_000,
        );
        vd.last_retarget = Instant::now() - Duration::from_secs(15);
        vd.started_at = vd.last_retarget;
        assert_eq!(vd.check_retarget(), None);
    }

    /// A session that started `secs` ago and has not been retargeted since,
    /// with `shares` credited entries of difficulty `credited` recorded inside
    /// that time. Shorter than the trailing retarget window, so the whole
    /// session is what gets judged.
    fn window(current: u64, credited: u64, shares: u64, secs: u64) -> Vardiff {
        let mut vd = Vardiff::new(cfg(), current);
        vd.last_retarget = Instant::now() - Duration::from_secs(secs);
        vd.started_at = vd.last_retarget;
        for _ in 0..shares {
            vd.record_share(credited);
        }
        vd
    }

    #[test]
    fn a_converged_miner_is_left_alone_inside_the_band() {
        // cfg(): 15s target, 60s interval, so four shares is exactly on
        // target. Three and five are ordinary Poisson scatter for a miner
        // that is already right; the old 5% deadband moved it both times.
        for shares in 3..=5 {
            let mut vd = window(100_000, 100_000, shares, 60);
            assert_eq!(
                vd.check_retarget(),
                None,
                "{shares} shares moved the difficulty"
            );
        }
        // Outside the band it moves halfway in ratio terms: two shares in
        // 60s is one per 30s, half the target rate, so the step is 1/√2.
        let mut vd = window(100_000, 100_000, 2, 60);
        assert_eq!(vd.check_retarget(), Some(70_711));
        // Eight shares in 60s is one per 7.5s, double the rate: √2.
        let mut vd = window(100_000, 100_000, 8, 60);
        assert_eq!(vd.check_retarget(), Some(141_421));
    }

    #[test]
    fn a_miner_far_off_target_takes_the_whole_step() {
        // Sixteen shares in 60s is four times the target rate: far enough
        // that it is not noise, so the step is the full ratio.
        let mut vd = window(100_000, 100_000, 16, 60);
        assert_eq!(vd.check_retarget(), Some(400_000));
    }

    #[test]
    fn one_quiet_interval_does_not_undo_a_converged_miner() {
        // Five minutes on target at 15s per share, except that the latest 90s
        // brought only two shares. Judged on that interval alone, one share
        // per 45s, the difficulty would be cut to a third and raised back
        // once the luck evened out. The trailing window holds 16 shares in
        // 300s, 0.8 of the target rate, inside the band.
        let mut vd = Vardiff::new(cfg(), 100_000);
        let now = Instant::now();
        vd.started_at = now - Duration::from_secs(600);
        vd.last_retarget = now - Duration::from_secs(90);
        for secs in (100..=295).rev().step_by(15) {
            vd.share_times
                .push_back((now - Duration::from_secs(secs), 100_000));
        }
        vd.share_times
            .push_back((now - Duration::from_secs(60), 100_000));
        vd.share_times
            .push_back((now - Duration::from_secs(10), 100_000));
        assert_eq!(vd.share_times.len(), 16);
        assert_eq!(vd.check_retarget(), None);
    }

    #[test]
    fn a_retarget_pushes_the_next_one_a_full_interval_out() {
        // Sessions sleep until retarget_due and then call check_retarget.
        // Whether or not it moves the difficulty, the due time must move a
        // full interval on, or the session would wake again at once.
        for shares in [0, 4] {
            let mut vd = window(100_000, 100_000, shares, 60);
            assert!(vd.retarget_due() <= Instant::now());
            vd.check_retarget();
            assert!(vd.retarget_due() >= Instant::now() + Duration::from_secs(59));
            assert_eq!(vd.check_retarget(), None);
        }
    }

    #[test]
    fn a_reconnecting_miner_is_retargeted_on_its_carried_history() {
        // A miner that ran at 100_000 reconnects and starts again at a low
        // initial difficulty. Its first retarget is judged on the adopted
        // record, which already shows the real rate, rather than on the few
        // shares of the new session.
        let history = aged(
            100_000,
            80,
            Duration::from_secs(15),
            Duration::from_secs(1_200),
        )
        .take_share_history();
        let mut vd = Vardiff::new(cfg(), 4096);
        vd.restore_share_history(history);
        vd.last_retarget = Instant::now() - Duration::from_secs(60);
        // Twenty-four times too easy: a full step, limited to a quarter.
        assert_eq!(vd.check_retarget(), Some(16_384));
    }

    #[test]
    fn a_window_across_a_raise_is_judged_by_credited_work() {
        // The difficulty was just raised 25_000 -> 100_000 and the miner's
        // in-flight work keeps landing at 25_000 for a while: four shares
        // credited at 25_000, then four at 100_000, in one 60s window. That
        // is 500_000 of work, one 100_000 share per 12s, inside the band.
        // Counting shares saw eight in 60s and doubled the difficulty again.
        let mut vd = window(100_000, 25_000, 4, 60);
        for _ in 0..4 {
            vd.record_share(100_000);
        }
        assert_eq!(vd.check_retarget(), None);
    }

    #[test]
    fn a_floor_pinned_device_is_brought_down_to_its_real_rate() {
        // Hardware that ignores set_difficulty keeps submitting floor shares,
        // credited at the floor. Forty of them in 60s is 40_960 of work,
        // a 10_240 difficulty at the target rate. The count-based retarget
        // saw forty shares and drove such a device to the ceiling.
        let mut vd = window(262_144, 1024, 40, 60);
        // Limited to a quarter per step; the next steps continue down.
        assert_eq!(vd.check_retarget(), Some(65_536));
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
        // drop rather than clamping back to the same value.
        let mut vd = Vardiff::new(cfg(), 100_000);
        assert_eq!(vd.suggest(80_000), 80_000);

        // Age the window past the retarget interval, then suggest again.
        vd.last_retarget = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .expect("test clock");
        vd.started_at = vd.last_retarget;
        assert_eq!(vd.suggest(80_000), 80_000, "value still honoured");

        // The retarget must still be due: the second suggestion must not have
        // reset the clock.
        // 120s of silence against a 15s target, limited to a quarter.
        assert_eq!(
            vd.check_retarget(),
            Some(20_000),
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
