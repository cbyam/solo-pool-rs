/// metrics/mod.rs
///
/// Prometheus-compatible metrics.
///
/// `init()` installs the global recorder and returns a handle that the
/// dashboard uses to render the /metrics endpoint.  The HTTP listener is
/// managed by network::dashboard, not here.
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use std::time::Duration;
use tracing::{info, warn};

/// Drop metric series untouched for this long. Worker-labeled series are minted
/// from untrusted names, so without an idle timeout each distinct name leaks a
/// permanent label set in the exporter. 24 h matches the stats-map eviction.
const METRIC_IDLE_TIMEOUT_SECS: u64 = 86_400;

/// Bucket upper bounds for the histogram metrics. Any metric not listed here
/// is exported as a summary.
///
/// Validation is a double SHA-256 plus bookkeeping, so tens of microseconds;
/// the top buckets are there to catch a stall.
const VALIDATION_SECONDS_BUCKETS: &[f64] = &[
    0.000_01, 0.000_025, 0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01,
];
/// Mining sessions run from a reconnect loop's seconds to a week or more.
const CONNECTION_SECONDS_BUCKETS: &[f64] = &[
    1.0, 10.0, 60.0, 300.0, 1_800.0, 3_600.0, 21_600.0, 86_400.0, 604_800.0,
];
/// new / old difficulty, symmetric in log terms around 1 so `le="1"` splits
/// moves down from moves up. 4 is the default `max_retarget_factor`.
const RETARGET_RATIO_BUCKETS: &[f64] = &[0.25, 0.33, 0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// The exporter configuration `init` installs, shared with the tests so they
/// render what production renders.
fn builder() -> Result<PrometheusBuilder, BuildError> {
    PrometheusBuilder::new()
        .idle_timeout(
            MetricKindMask::ALL,
            Some(Duration::from_secs(METRIC_IDLE_TIMEOUT_SECS)),
        )
        .set_buckets_for_metric(
            Matcher::Full("pool_share_validation_duration_seconds".into()),
            VALIDATION_SECONDS_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("pool_connection_duration_seconds".into()),
            CONNECTION_SECONDS_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("pool_vardiff_retarget_ratio".into()),
            RETARGET_RATIO_BUCKETS,
        )
}

pub fn init(addr: &str) -> Option<PrometheusHandle> {
    if addr.is_empty() {
        info!("Prometheus metrics disabled (empty prometheus_addr)");
        return None;
    }
    match builder().and_then(PrometheusBuilder::install_recorder) {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!("Failed to install Prometheus recorder: {e}");
            None
        }
    }
}

// ── Counters & gauges ─────────────────────────────────────────────────────────

/// A connection refused at accept time, before any session work.
/// `reason`: "banned", "rate_limit", "capacity".
pub fn connection_refused(reason: &'static str) {
    counter!("pool_connections_refused_total", "reason" => reason).increment(1);
}

/// An address added to the ban list. `reason` is the short label passed to
/// `BanList::ban`, so a future incident is a panel query, not a journal grep.
pub fn ban(reason: &str) {
    counter!("pool_bans_total", "reason" => reason.to_string()).increment(1);
}

pub fn miner_connected() {
    gauge!("pool_connected_miners").increment(1.0);
}

pub fn miner_disconnected() {
    gauge!("pool_connected_miners").decrement(1.0);
}

/// `difficulty` is the credited difficulty (`Vardiff::credit_for`), so
/// `rate(pool_share_work_total[5m]) * 2^32` is the worker's hashrate on the
/// same basis as `pool_hashrate_estimated_hps`.
///
/// `pool_share_difficulty` is the deprecated summary, kept until 1.0. Its
/// quantiles only repeated the assigned difficulty; the counter keeps its
/// `_sum`, the one part a sampled gauge cannot rebuild.
pub fn share_accepted(difficulty: u64, worker: &str) {
    counter!("pool_shares_accepted_total", "worker" => worker.to_string()).increment(1);
    counter!("pool_share_work_total", "worker" => worker.to_string()).increment(difficulty);
    histogram!("pool_share_difficulty", "worker" => worker.to_string()).record(difficulty as f64);
}

pub fn share_rejected(reason: &str, worker: &str) {
    counter!(
        "pool_shares_rejected_total",
        "reason" => reason.to_string(),
        "worker" => worker.to_string()
    )
    .increment(1);
    // Also track per-worker rejected shares for efficiency monitoring
    counter!(
        "pool_worker_shares_rejected_total",
        "worker" => worker.to_string()
    )
    .increment(1);
}

/// Recorded as fractional milliseconds. Validation runs well under a
/// millisecond, so whole milliseconds recorded almost every share as 0.
///
/// `pool_share_validation_duration_ms` is the deprecated summary, kept until
/// 1.0; the histogram in seconds replaces it.
pub fn share_validation_time(elapsed: Duration) {
    histogram!("pool_share_validation_duration_ms").record(elapsed.as_secs_f64() * 1000.0);
    histogram!("pool_share_validation_duration_seconds").record(elapsed.as_secs_f64());
}

/// A session ended. Counted once in `pool_connections_closed_total` by how far
/// it got: `no_auth` never named a worker, `no_shares` authorized but never
/// submitted, `shares` submitted at least one share, accepted or not.
///
/// Only `shares` sessions go into `pool_connection_duration_seconds`. cgminer
/// and Avalon firmware open side connections that authorize and close within
/// a second, and counting them pulled every Avalon's duration toward zero.
///
/// `pool_connection_duration_secs` is the deprecated summary, kept until 1.0.
/// It still records every authorized session, as it always has.
pub fn session_closed(worker: Option<&str>, shares_submitted: u64, elapsed: Duration) {
    let Some(worker) = worker else {
        counter!("pool_connections_closed_total", "reason" => "no_auth").increment(1);
        return;
    };
    histogram!("pool_connection_duration_secs", "worker" => worker.to_string())
        .record(elapsed.as_secs_f64());
    if shares_submitted == 0 {
        counter!("pool_connections_closed_total", "reason" => "no_shares").increment(1);
        return;
    }
    counter!("pool_connections_closed_total", "reason" => "shares").increment(1);
    histogram!("pool_connection_duration_seconds", "worker" => worker.to_string())
        .record(elapsed.as_secs_f64());
}

pub fn miner_disconnect(reason: &str, worker: &str) {
    counter!(
        "pool_miner_disconnects_total",
        "reason" => reason.to_string(),
        "worker" => worker.to_string()
    )
    .increment(1);
}

pub fn block_submission_success() {
    counter!("pool_block_submissions_success_total").increment(1);
}

pub fn block_submission_failure(reason: &'static str) {
    counter!(
        "pool_block_submissions_failed_total",
        "reason" => reason
    )
    .increment(1);
}

pub fn job_broadcast(miners_count: usize) {
    gauge!("pool_job_broadcast_miners").set(miners_count as f64);
    counter!("pool_job_broadcasts_total").increment(1);
}

pub fn zmq_reconnect() {
    counter!("pool_zmq_reconnects_total").increment(1);
}

pub fn rpc_fallback_used() {
    counter!("pool_rpc_fallback_used_total").increment(1);
}

pub fn vardiff_retarget(worker: &str, old_diff: u64, new_diff: u64) {
    gauge!("pool_worker_difficulty", "worker" => worker.to_string()).set(new_diff as f64);
    let ratio = new_diff as f64 / old_diff as f64;
    // pool_vardiff_change_ratio is the deprecated summary, kept until 1.0.
    histogram!("pool_vardiff_change_ratio").record(ratio);
    histogram!("pool_vardiff_retarget_ratio").record(ratio);
    // The ratio is exported as a summary over a short rolling window, and
    // retargets are sparse enough that its quantiles mostly read 0. A counter
    // makes the retarget rate itself something a dashboard can plot.
    let direction = if new_diff > old_diff { "up" } else { "down" };
    counter!(
        "pool_vardiff_retargets_total",
        "worker" => worker.to_string(),
        "direction" => direction
    )
    .increment(1);
}

pub fn block_found() {
    counter!("pool_blocks_found_total").increment(1);
}

pub fn update_hashrate(hps: f64, worker: &str) {
    gauge!("pool_hashrate_estimated_hps", "worker" => worker.to_string()).set(hps);
}

pub fn update_job_height(height: u64) {
    gauge!("pool_job_height").set(height as f64);
}

/// Touch every pool-wide series so the exporter's 24 h idle timeout, which
/// exists to bound the `worker` label, never expires them. Without this,
/// `pool_blocks_found_total` vanished a day after a block and came back at 0,
/// and `pool_connected_miners` vanished when no miner churned for a day. A
/// zero-sized update still counts: metrics-util bumps a series' generation on
/// every call. It also registers the counters at 0 from boot, so `increase()`
/// and absence alerts work before the first event. Label values are the
/// fixed sets used at the call sites.
pub fn keep_pool_series_alive() {
    for name in [
        "pool_blocks_found_total",
        "pool_block_submissions_success_total",
        "pool_job_broadcasts_total",
        "pool_zmq_reconnects_total",
        "pool_rpc_fallback_used_total",
    ] {
        counter!(name).increment(0);
    }
    for reason in ["capacity", "banned", "rate_limit"] {
        counter!("pool_connections_refused_total", "reason" => reason).increment(0);
    }
    counter!("pool_bans_total", "reason" => "message too large").increment(0);
    for reason in ["no_auth", "no_shares", "shares"] {
        counter!("pool_connections_closed_total", "reason" => reason).increment(0);
    }
    for reason in [
        "rpc_error",
        "rejected",
        "template_unavailable",
        "io",
        "other",
    ] {
        counter!("pool_block_submissions_failed_total", "reason" => reason).increment(0);
    }
    for name in [
        "pool_connected_miners",
        "pool_job_height",
        "pool_job_broadcast_miners",
    ] {
        gauge!(name).increment(0.0);
    }
}

/// Per-worker liveness, re-set on a timer for every worker the pool still
/// knows (up to 24 h after it goes offline), so alert rules on
/// `pool_worker_online == 0` or on the age of
/// `pool_worker_last_share_timestamp_seconds` keep working while the worker
/// is down. Once stats evict the worker the series is no longer touched and
/// expires like any worker series.
pub fn worker_liveness(worker: &str, online: bool, last_share_ts: u64) {
    gauge!("pool_worker_online", "worker" => worker.to_string()).set(if online {
        1.0
    } else {
        0.0
    });
    if last_share_ts > 0 {
        gauge!("pool_worker_last_share_timestamp_seconds", "worker" => worker.to_string())
            .set(last_share_ts as f64);
    }
}

/// 1 while a configured stats database is taking writes, 0 while it is not.
/// Not exported when no database is configured.
pub fn stats_store_ok(ok: bool) {
    gauge!("pool_stats_store_ok").set(if ok { 1.0 } else { 0.0 });
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use metrics_util::MetricKindMask;
    use std::time::Duration;

    /// The keep-alive has to survive the same idle timeout the pool installs,
    /// while a worker series nobody touches still expires.
    #[test]
    fn touched_pool_series_outlive_the_idle_timeout_and_worker_series_do_not() {
        let recorder = PrometheusBuilder::new()
            .idle_timeout(MetricKindMask::ALL, Some(Duration::from_millis(300)))
            .build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::block_found();
            super::share_accepted(4096, "rig1");
        });
        let first = handle.render();
        assert!(first.contains("pool_blocks_found_total 1"), "{first}");
        assert!(first.contains("worker=\"rig1\""), "{first}");

        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(150));
            metrics::with_local_recorder(&recorder, super::keep_pool_series_alive);
            let _ = handle.render();
        }
        let later = handle.render();
        assert!(
            later.contains("pool_blocks_found_total 1"),
            "kept-alive counter expired or reset:\n{later}"
        );
        assert!(
            !later.contains("worker=\"rig1\""),
            "untouched worker series should have expired:\n{later}"
        );
    }

    /// The `metrics` facade these helpers emit through and the exporter that
    /// serves /metrics must resolve to one version of the crate.
    ///
    /// When they split, nothing complains. Bumping `metrics` to 0.24 while the
    /// exporter still pulled 0.23 gave two global registries: every macro here
    /// wrote to one, the exporter read the other, the build stayed green, all
    /// tests passed, and /metrics served 200 with an empty body until someone
    /// happened to look. Rendering through the real helpers is the only check
    /// that fails when that happens.
    #[test]
    fn the_exporter_can_read_what_these_helpers_emit() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::miner_connected();
            super::connection_refused("rate_limit");
        });

        let rendered = handle.render();
        for expected in ["pool_connected_miners", "pool_connections_refused_total"] {
            assert!(
                rendered.contains(expected),
                "exporter rendered nothing for {expected}, which this module just \
                 emitted — the facade and the exporter are probably on different \
                 versions of `metrics`. Rendered:\n{rendered}"
            );
        }
    }

    /// Durations keep their fractional part. Both used to be truncated to
    /// whole units before recording, so a 74,605-share run summed to 2 ms and
    /// every short session summed to 0 s.
    #[test]
    fn durations_are_recorded_with_their_fraction() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::share_validation_time(Duration::from_micros(250));
            super::share_validation_time(Duration::from_micros(250));
            super::session_closed(Some("rig1"), 1, Duration::from_millis(1_500));
        });

        let rendered = handle.render();
        assert!(
            rendered.contains("pool_share_validation_duration_ms_sum 0.5\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("pool_connection_duration_secs_sum{worker=\"rig1\"} 1.5\n"),
            "{rendered}"
        );
    }

    /// The replacement metrics render as Prometheus histograms with the
    /// buckets `init` configures, while the deprecated names stay summaries.
    #[test]
    fn replacement_metrics_are_histograms_and_old_names_stay_summaries() {
        let recorder = super::builder().unwrap().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::share_validation_time(Duration::from_micros(40));
            super::session_closed(Some("rig1"), 3, Duration::from_secs(7_200));
            super::vardiff_retarget("rig1", 4096, 2048);
        });

        let rendered = handle.render();
        for expected in [
            "# TYPE pool_share_validation_duration_seconds histogram",
            "pool_share_validation_duration_seconds_bucket{le=\"0.000025\"} 0\n",
            "pool_share_validation_duration_seconds_bucket{le=\"0.00005\"} 1\n",
            "# TYPE pool_connection_duration_seconds histogram",
            "pool_connection_duration_seconds_bucket{worker=\"rig1\",le=\"3600\"} 0\n",
            "pool_connection_duration_seconds_bucket{worker=\"rig1\",le=\"21600\"} 1\n",
            "# TYPE pool_vardiff_retarget_ratio histogram",
            "pool_vardiff_retarget_ratio_bucket{le=\"0.33\"} 0\n",
            "pool_vardiff_retarget_ratio_bucket{le=\"0.5\"} 1\n",
            "# TYPE pool_share_validation_duration_ms summary",
            "# TYPE pool_connection_duration_secs summary",
            "# TYPE pool_vardiff_change_ratio summary",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
    }

    /// Accepted work sums credited difficulty and matches the deprecated
    /// summary's `_sum`, the basis for hashrate in PromQL.
    #[test]
    fn accepted_work_counts_credited_difficulty() {
        let recorder = super::builder().unwrap().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::share_accepted(4096, "rig1");
            super::share_accepted(1024, "rig1");
        });

        let rendered = handle.render();
        for expected in [
            "# TYPE pool_share_work_total counter",
            "pool_share_work_total{worker=\"rig1\"} 5120\n",
            "pool_share_difficulty_sum{worker=\"rig1\"} 5120\n",
            "pool_shares_accepted_total{worker=\"rig1\"} 2\n",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
    }

    /// Sessions that never submitted a share are counted, not timed, so the
    /// side connections Avalon firmware opens do not drag its durations
    /// toward zero. The deprecated summary keeps its old behaviour.
    #[test]
    fn sessions_without_shares_are_counted_but_not_timed() {
        let recorder = super::builder().unwrap().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            super::session_closed(None, 0, Duration::from_millis(50));
            super::session_closed(Some("avalon"), 0, Duration::from_millis(120));
            super::session_closed(Some("avalon"), 0, Duration::from_millis(80));
            super::session_closed(Some("avalon"), 40, Duration::from_secs(600));
        });

        let rendered = handle.render();
        for expected in [
            "pool_connections_closed_total{reason=\"no_auth\"} 1\n",
            "pool_connections_closed_total{reason=\"no_shares\"} 2\n",
            "pool_connections_closed_total{reason=\"shares\"} 1\n",
            "pool_connection_duration_seconds_count{worker=\"avalon\"} 1\n",
            "pool_connection_duration_seconds_sum{worker=\"avalon\"} 600\n",
            "pool_connection_duration_secs_count{worker=\"avalon\"} 3\n",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
    }
}
