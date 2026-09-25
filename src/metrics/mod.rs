/// metrics/mod.rs
///
/// Prometheus-compatible metrics.
///
/// `init()` installs the global recorder and returns a handle that the
/// dashboard uses to render the /metrics endpoint.  The HTTP listener is
/// managed by network::dashboard, not here.
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use tracing::{info, warn};

/// Drop metric series untouched for this long. Worker-labeled series are minted
/// from untrusted names, so without an idle timeout each distinct name leaks a
/// permanent label set in the exporter. 24 h matches the stats-map eviction.
const METRIC_IDLE_TIMEOUT_SECS: u64 = 86_400;

pub fn init(addr: &str) -> Option<PrometheusHandle> {
    if addr.is_empty() {
        info!("Prometheus metrics disabled (empty prometheus_addr)");
        return None;
    }
    match PrometheusBuilder::new()
        .idle_timeout(
            MetricKindMask::ALL,
            Some(std::time::Duration::from_secs(METRIC_IDLE_TIMEOUT_SECS)),
        )
        .install_recorder()
    {
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

pub fn share_accepted(difficulty: u64, worker: &str) {
    counter!("pool_shares_accepted_total", "worker" => worker.to_string()).increment(1);
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

pub fn share_validation_time(duration_ms: f64) {
    histogram!("pool_share_validation_duration_ms").record(duration_ms);
}

pub fn connection_duration(worker: &str, duration_secs: f64) {
    histogram!("pool_connection_duration_secs", "worker" => worker.to_string())
        .record(duration_secs);
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
    histogram!("pool_vardiff_change_ratio").record(new_diff as f64 / old_diff as f64);
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
}
