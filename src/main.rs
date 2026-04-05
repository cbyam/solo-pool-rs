/// solo-pool-rs — Solo BTC mining pool (Stratum V1, SV2-ready)
///
/// Startup sequence:
///   1. Load config.toml
///   2. Initialise tracing (structured or plain)
///   3. Start Prometheus metrics endpoint
///   4. Connect to Bitcoin Knots RPC (cookie auth)
///   5. Start ZMQ block-notification listener (or RPC poll fallback)
///   6. Bootstrap the template engine and build first job
///   7. Start the TCP accept loop
mod bitcoin;
mod config;
mod error;
mod metrics;
mod mining;
mod network;
mod protocol;
mod security;
mod stats;

use crate::{
    bitcoin::{rpc::RpcClient, zmq},
    mining::engine::TemplateEngine,
    security::BanList,
    stats::PoolStats,
};
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Config ────────────────────────────────────────────────────────────────
    let mut args = std::env::args().skip(1);
    let cfg_path = match args.next() {
        Some(a) if a == "--config" => args.next().unwrap_or_else(|| "config.toml".to_string()),
        Some(a) => a,
        None => "config.toml".to_string(),
    };

    let config = Arc::new(
        config::load(&cfg_path).with_context(|| format!("Loading config from '{cfg_path}'"))?,
    );

    // ── Logging ───────────────────────────────────────────────────────────────
    init_tracing(&config.logging);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        listen  = %config.pool.listen_addr,
        address = %config.pool.coinbase_address,
        "solo-pool-rs starting"
    );

    // ── Metrics ───────────────────────────────────────────────────────────────
    let prometheus_handle = metrics::init(&config.metrics.prometheus_addr);

    // ── Pool stats (HTTP dashboard snapshot + in-memory state)
    // Supports optional SQLite persistence for all-time best values.
    let stats = PoolStats::new_with_store(config.metrics.stats_db_path.clone());
    network::dashboard::start(
        &config.metrics.prometheus_addr,
        stats.clone(),
        prometheus_handle,
    )
    .await;

    // ── Bitcoin RPC ───────────────────────────────────────────────────────────
    let rpc =
        Arc::new(RpcClient::new(&config.bitcoin_rpc).context("Connecting to Bitcoin Knots RPC")?);

    // ── Hashrate history recorder (every 10 minutes) ─────────────────────────
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            let interval = tokio::time::Duration::from_secs(10 * 60);
            loop {
                stats.record_hashrate_snapshot();
                tokio::time::sleep(interval).await;
            }
        });
    }

    // ── Network hash rate poll ───────────────────────────────────────────────
    {
        let stats = stats.clone();
        let rpc = rpc.clone();
        tokio::spawn(async move {
            let interval = tokio::time::Duration::from_secs(30);
            loop {
                match rpc.network_hashrate(None, None) {
                    Ok(network_hps) => stats.set_network_hashrate(network_hps),
                    Err(e) => tracing::warn!("Failed to poll network hash rate: {e}"),
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // ── ZMQ / poll ────────────────────────────────────────────────────────────
    let new_block_rx = zmq::start(&config.zmq, rpc.clone()).await;

    // ── Template engine ───────────────────────────────────────────────────────
    let engine = TemplateEngine::new(rpc.clone(), config.pool.clone());

    // Spawn the template refresh loop
    {
        let engine = engine.clone();
        tokio::spawn(engine.run(new_block_rx));
    }

    // ── Security ──────────────────────────────────────────────────────────────
    let ban_list = BanList::new(config.security.ban_duration_secs);

    // ── TCP server ────────────────────────────────────────────────────────────
    network::server::run(config, engine, ban_list, stats).await?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tracing initialisation
// ─────────────────────────────────────────────────────────────────────────────

fn init_tracing(cfg: &config::LoggingConfig) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_new(&cfg.level).unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(log_dir) = &cfg.log_dir {
        use std::path::PathBuf;
        use tracing_appender::rolling::{RollingFileAppender, Rotation};

        // Expand ~ if present
        let log_dir_path = if let Some(stripped) = log_dir.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                PathBuf::from(home).join(stripped)
            } else {
                PathBuf::from(log_dir)
            }
        } else {
            PathBuf::from(log_dir)
        };

        // Create directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&log_dir_path) {
            eprintln!(
                "Failed to create log directory {}: {}",
                log_dir_path.display(),
                e
            );
            std::process::exit(1);
        }

        let file_appender =
            RollingFileAppender::new(Rotation::DAILY, log_dir_path, "solo-pool-rs.log");

        if cfg.json {
            fmt()
                .json()
                .with_env_filter(filter)
                .with_current_span(true)
                .with_writer(file_appender)
                .init();
        } else {
            fmt()
                .with_env_filter(filter)
                .with_target(true)
                .with_writer(file_appender)
                .init();
        }
    } else if cfg.json {
        fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(true)
            .with_ansi(false)
            .init();
    } else {
        fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_ansi(false)
            .init();
    }
}
