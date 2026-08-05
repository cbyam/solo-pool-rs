/// bitcoin/zmq.rs
///
/// Listens on the Bitcoin Knots ZMQ `hashblock` socket.
/// On new block notification, triggers a GBT refresh via the template engine.
/// Falls back to RPC polling when ZMQ is unavailable or misconfigured.
use crate::config::ZmqConfig;
use crate::metrics;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

/// Sends a unit signal every time a new block is detected.
pub type NewBlockSender = watch::Sender<u64>;
pub type NewBlockReceiver = watch::Receiver<u64>;

/// Backoff before re-subscribing after the ZMQ stream errors or closes.
const ZMQ_RECONNECT_SECS: u64 = 5;

/// How long ZMQ may be silent before a poll-detected tip change is treated as
/// "ZMQ missed it". Blocks are ~10 minutes apart and a healthy subscription
/// delivers within milliseconds of the tip moving, so a minute of silence around
/// a confirmed tip change means the subscription is not delivering.
const ZMQ_SILENCE_SECS: u64 = 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Start the ZMQ listener (or polling fallback).
/// Returns a `watch::Receiver` that fires whenever the chain tip advances.
pub async fn start(cfg: &ZmqConfig, rpc: Arc<crate::bitcoin::rpc::RpcClient>) -> NewBlockReceiver {
    let (tx, rx) = watch::channel(0u64);
    let endpoint = cfg.hashblock_endpoint.clone();
    let poll_fallback = cfg.poll_fallback;
    let poll_interval_ms = cfg.poll_interval_ms;

    // Wall-clock second of the last ZMQ hashblock message; 0 until one arrives.
    // The poll uses it to tell "ZMQ delivered and I am confirming" from "ZMQ is
    // silently dead and I am the only thing noticing blocks".
    let zmq_last_seen = Arc::new(AtomicU64::new(0));

    // ZMQ listener, with reconnect. A transient receive error must not retire
    // ZMQ for the process lifetime.
    {
        let tx = tx.clone();
        let zmq_last_seen = Arc::clone(&zmq_last_seen);
        tokio::spawn(async move {
            loop {
                if let Err(e) =
                    run_zmq_listener(&endpoint, tx.clone(), Arc::clone(&zmq_last_seen)).await
                {
                    warn!("ZMQ listener failed ({e}); reconnecting in {ZMQ_RECONNECT_SECS}s");
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(ZMQ_RECONNECT_SECS)).await;
            }
        });
    }

    // Poll safety net, always on.
    //
    // ZMQ's connect() is lazy: it returns Ok for an endpoint with nothing
    // listening (wrong port, node without -zmqpubhashblock, firewall) and the
    // subscription then blocks forever without ever erroring. A fallback gated
    // on the listener returning Err therefore cannot engage in exactly the case
    // it exists for. Rather than trying to prove the socket is live, poll
    // unconditionally at a slow cadence and let the watch channel dedupe: the
    // poll only signals when the tip hash actually changes, so when ZMQ is
    // healthy this costs one getbestblockhash per interval and nothing else.
    if poll_fallback {
        tokio::spawn(run_poll_fallback(rpc, poll_interval_ms, tx, zmq_last_seen));
    } else {
        warn!(
            "[zmq] poll_fallback is disabled — a silently dead ZMQ subscription \
             will go undetected and new blocks will only be noticed by the \
             periodic template refresh"
        );
    }

    rx
}

async fn run_zmq_listener(
    endpoint: &str,
    tx: NewBlockSender,
    zmq_last_seen: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let ctx = tmq::Context::new();
    let mut sub = tmq::subscribe(&ctx)
        .connect(endpoint)?
        .subscribe(b"hashblock")?;

    info!("ZMQ listener connected to {endpoint}");
    let mut seq: u64 = 0;

    loop {
        match sub.next().await {
            Some(Ok(_multipart)) => {
                seq += 1;
                zmq_last_seen.store(now_secs(), Ordering::Relaxed);
                debug!("ZMQ: hashblock notification #{seq}");
                let _ = tx.send(seq);
            }
            Some(Err(e)) => {
                return Err(anyhow::anyhow!("ZMQ receive error: {e}"));
            }
            None => {
                return Err(anyhow::anyhow!("ZMQ stream closed"));
            }
        }
    }
}

async fn run_poll_fallback(
    rpc: Arc<crate::bitcoin::rpc::RpcClient>,
    poll_interval_ms: u64,
    tx: NewBlockSender,
    zmq_last_seen: Arc<AtomicU64>,
) {
    info!("Starting RPC tip poll ({}ms interval)", poll_interval_ms);
    let mut last_hash = String::new();
    let mut seq: u64 = 0;
    let interval = tokio::time::Duration::from_millis(poll_interval_ms);

    loop {
        // bitcoincore-rpc is synchronous; run it on the blocking pool so a hung
        // node cannot pin a runtime worker for the transport timeout.
        let rpc = Arc::clone(&rpc);
        match tokio::task::spawn_blocking(move || rpc.best_block_hash()).await {
            Ok(Ok(hash)) => {
                if hash != last_hash {
                    // Skip the first observation: that is the poll learning the
                    // current tip at startup, not a missed notification.
                    if !last_hash.is_empty() {
                        let silence =
                            now_secs().saturating_sub(zmq_last_seen.load(Ordering::Relaxed));
                        if zmq_last_seen.load(Ordering::Relaxed) == 0 || silence > ZMQ_SILENCE_SECS
                        {
                            metrics::rpc_fallback_used();
                            warn!(
                                "Tip changed but ZMQ has been silent — the hashblock \
                                 subscription is not delivering. Check zmq.hashblock_endpoint \
                                 and that the node runs with -zmqpubhashblock."
                            );
                        }
                    }
                    debug!("Poll: new block hash {hash}");
                    last_hash = hash;
                    seq += 1;
                    let _ = tx.send(seq);
                }
            }
            Ok(Err(e)) => warn!("Poll RPC error: {e}"),
            Err(e) => warn!("Poll task failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}
