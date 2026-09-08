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

#[allow(dead_code)]
pub fn zmq_reconnect() {
    counter!("pool_zmq_reconnects_total").increment(1);
}

pub fn rpc_fallback_used() {
    counter!("pool_rpc_fallback_used_total").increment(1);
}

pub fn vardiff_retarget(worker: &str, old_diff: u64, new_diff: u64) {
    gauge!("pool_worker_difficulty", "worker" => worker.to_string()).set(new_diff as f64);
    histogram!("pool_vardiff_change_ratio").record(new_diff as f64 / old_diff as f64);
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

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

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
