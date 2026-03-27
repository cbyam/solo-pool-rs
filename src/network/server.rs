/// network/server.rs
///
/// TCP accept loop.
///
/// Responsibilities:
///  - Bind the listener socket
///  - Enforce per-IP connection rate limits before spawning a session
///  - Check the ban list on accept
///  - Track total active connections (bounded by max_connections)
///  - Spawn one tokio task per miner connection
use crate::{
    config::Config,
    mining::engine::TemplateEngine,
    security::{BanList, ConnectionRateLimiter},
    stats::PoolStats,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub async fn run(
    config: Arc<Config>,
    engine: Arc<TemplateEngine>,
    ban_list: Arc<BanList>,
    stats: Arc<PoolStats>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&config.pool.listen_addr).await?;
    info!("Stratum V1 listener on {}", config.pool.listen_addr);

    let conn_limiter = ConnectionRateLimiter::new(config.security.max_connections_per_ip);
    let active_count = Arc::new(AtomicUsize::new(0));
    let max_connections = config.pool.max_connections;

    // Background ban-list pruner
    {
        let bl = ban_list.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                bl.prune();
            }
        });
    }

    loop {
        let (stream, peer) = listener.accept().await?;

        // ── Global connection cap ────────────────────────────────────────────
        let reserved = loop {
            let current = active_count.load(Ordering::Relaxed);
            if current >= max_connections {
                warn!("Connection limit reached ({max_connections}), dropping {peer}");
                break false;
            }
            match active_count.compare_exchange(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break true,
                Err(_) => continue,
            }
        };
        if !reserved {
            continue;
        }

        // ── IP ban check ─────────────────────────────────────────────────────
        if ban_list.is_banned(&peer.ip()) {
            warn!("Rejected banned IP: {peer}");
            active_count.fetch_sub(1, Ordering::Relaxed);
            continue;
        }

        // ── Per-IP rate limit ────────────────────────────────────────────────
        if !conn_limiter.check_and_record(peer.ip()) {
            warn!("Connection rate limit exceeded for {}", peer.ip());
            ban_list.ban(peer.ip(), "connection rate limit exceeded");
            active_count.fetch_sub(1, Ordering::Relaxed);
            continue;
        }

        // ── TCP tuning ───────────────────────────────────────────────────────
        // Keep-alive so we detect dead miners without waiting for idle_timeout
        if let Err(e) = stream.set_nodelay(true) {
            warn!("TCP_NODELAY failed for {peer}: {e}");
        }

        // ── Spawn session task ───────────────────────────────────────────────
        let config = config.clone();
        let engine = engine.clone();
        let ban_list = ban_list.clone();
        let stats = stats.clone();
        let active_count = active_count.clone();

        tokio::spawn(async move {
            crate::network::session::run(stream, peer, config, engine, ban_list, stats).await;
            active_count.fetch_sub(1, Ordering::Relaxed);
        });
    }
}
