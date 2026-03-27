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
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

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

    if cfg.json {
        fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(true)
            .init();
    } else {
        fmt().with_env_filter(filter).with_target(true).init();
    }
}
