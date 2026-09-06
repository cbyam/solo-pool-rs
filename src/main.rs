use anyhow::{Context, Result};
/// solo-pool-rs — Solo BTC mining pool (Stratum V1 + V2, auto-detected on one port)
///
/// Startup sequence:
///   1. Load config.toml
///   2. Initialise tracing (structured or plain)
///   3. Start Prometheus metrics endpoint
///   4. Connect to Bitcoin Knots RPC (cookie auth)
///   5. Start ZMQ block-notification listener (or RPC poll fallback)
///   6. Bootstrap the template engine and build first job
///   7. Start the TCP accept loop
use solo_pool_rs::{
    bitcoin::{rpc::RpcClient, zmq},
    config, metrics,
    mining::engine::TemplateEngine,
    network, protocol,
    security::BanList,
    settings,
    stats::PoolStats,
};
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
    if let Some(msg) = config::credential_exposure_warning(&cfg_path, &config) {
        tracing::warn!("{msg}");
    }

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

    // ── Bitcoin RPC ───────────────────────────────────────────────────────────
    let rpc =
        Arc::new(RpcClient::new(&config.bitcoin_rpc).context("Connecting to Bitcoin Knots RPC")?);

    // ── Runtime settings (payout address; network detected from the node) ────
    // The node's chain is the source of truth: the payout address must
    // validate against it before any job is built. A previous dashboard save
    // (persisted in the stats DB) overrides the config address.
    let node_chain = rpc
        .chain()
        .context("Querying node chain (getblockchaininfo)")?;
    info!(chain = %node_chain, "Connected node chain detected");
    let runtime_settings = settings::RuntimeSettings::new(&config.pool, &node_chain)?;
    runtime_settings.apply_persisted(stats.load_setting("coinbase_address"));

    // ── Hashrate history recorder (every 10 minutes) ─────────────────────────
    // Sleep first: a sample taken at boot is a zero (no miner has reconnected
    // yet), and the chart drew it as a ramp down to nothing across every
    // restart. Downtime is shown as a gap instead (see chart_json), which the
    // shutdown hook's final sample and the first post-boot sample delimit.
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            let interval = tokio::time::Duration::from_secs(10 * 60);
            loop {
                tokio::time::sleep(interval).await;
                stats.record_hashrate_snapshot();
            }
        });
    }

    // ── Prometheus hashrate refresh for offline workers ──────────────────────
    // The per-worker gauge is pushed from the session loop, so it only moves
    // while a miner is delivering traffic. Re-push decayed values for offline
    // workers so scrapes match the dashboard instead of holding the last
    // live value until the exporter's idle timeout. Pushing stops once the
    // pruner evicts the worker from stats, after which the idle timeout
    // expires the series.
    if prometheus_handle.is_some() {
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                for (worker, hps) in stats.offline_worker_hashrates_10m() {
                    metrics::update_hashrate(hps, &worker);
                }
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
                // bitcoincore-rpc is synchronous. Left unwrapped, a hung node
                // parks a runtime worker for the full transport timeout every
                // 30s — precisely when miners are also struggling.
                let poll = {
                    let rpc = rpc.clone();
                    tokio::task::spawn_blocking(move || {
                        (
                            rpc.network_hashrate(None, None),
                            rpc.estimate_difficulty_change_pct(),
                        )
                    })
                    .await
                };
                match poll {
                    Ok((hashrate, difficulty)) => {
                        match hashrate {
                            Ok(network_hps) => stats.set_network_hashrate(network_hps),
                            Err(e) => tracing::warn!("Failed to poll network hash rate: {e}"),
                        }
                        match difficulty {
                            Ok(pct) => stats.set_est_difficulty_change_pct(pct),
                            Err(e) => tracing::warn!("Failed to estimate difficulty change: {e}"),
                        }
                    }
                    Err(e) => tracing::warn!("Network stats poll task failed: {e}"),
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // ── ZMQ / poll ────────────────────────────────────────────────────────────
    let new_block_rx = zmq::start(&config.zmq, rpc.clone()).await;

    // ── Template engine ───────────────────────────────────────────────────────
    let engine = TemplateEngine::new(rpc.clone(), config.pool.clone(), runtime_settings.clone());

    // Spawn the template refresh loop
    {
        let engine = engine.clone();
        tokio::spawn(engine.run(new_block_rx));
    }

    // Replay any found block whose submission was never confirmed (crash or
    // restart mid-retry). Off the boot path so a slow node cannot delay
    // accepting miners; "duplicate" from the node counts as success.
    {
        let engine = engine.clone();
        tokio::spawn(async move { engine.replay_archived_blocks().await });
    }

    // ── SV2 Noise authority (before the dashboard, which shows the pubkey) ────
    let sv2_authority_pubkey = if config.sv2.enabled {
        let pubkey = protocol::sv2::init_noise_authority(&config.sv2)
            .context("Initialising SV2 Noise authority key")?;
        if config.sv2.persist_authority_key {
            info!("SV2 authority public key: {pubkey} (pin this on the miner to verify pool identity)");
        } else {
            info!("SV2 authority public key: {pubkey} (ephemeral: persist_authority_key = false, changes every restart)");
        }
        Some(pubkey)
    } else {
        None
    };

    // ── Dashboard (after the engine exists: the Settings page triggers a
    //    clean-job refresh through it) ──────────────────────────────────────────
    network::dashboard::start(
        &config.metrics.prometheus_addr,
        stats.clone(),
        prometheus_handle,
        runtime_settings,
        engine.clone(),
        config.metrics.allow_runtime_settings,
        &config.pool.listen_addr,
        config.sv2.enabled,
        sv2_authority_pubkey,
        &config.metrics.allowed_hosts,
    )
    .await;

    // ── Security ──────────────────────────────────────────────────────────────
    let ban_list = BanList::new(config.security.ban_duration_secs);

    // ── TCP server ────────────────────────────────────────────────────────────
    network::server::run(
        config,
        engine.clone(),
        ban_list,
        stats.clone(),
        shutdown_signal(),
    )
    .await?;

    // ── Graceful shutdown ─────────────────────────────────────────────────────
    // The listener has closed. Persist what would otherwise be lost to the
    // ten-minute snapshot cadence, then give any inline block submission a
    // few seconds to reach the node. A block still unconfirmed after that is
    // in the archive and replays on the next boot.
    info!("Shutting down: writing final hashrate snapshot");
    stats.record_hashrate_snapshot();
    if !engine
        .wait_for_inflight_submits(std::time::Duration::from_secs(5))
        .await
    {
        tracing::warn!(
            "Shutdown proceeding with a block submission still in flight; \
             it is archived and will be replayed at next start"
        );
    }
    info!("Shutdown complete");
    Ok(())
}

/// Resolves on SIGTERM (what `systemctl stop` and Docker send) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("Ctrl-C handler failed: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("SIGTERM handler failed: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl-C"),
        _ = terminate => info!("Received SIGTERM"),
    }
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
                .with_ansi(false)
                .init();
        } else {
            fmt()
                .with_env_filter(filter)
                .with_target(true)
                .with_writer(file_appender)
                .with_ansi(false)
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
