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

/// How long after the poll notices a new tip the subscription gets to deliver
/// its own notification before the poll declares it silent. The hashblock
/// message leaves the node through its validation callback queue, which can
/// lag the RPC-visible tip by seconds when the queue is busy (a rawtx
/// publisher on the same node shares it), so a one-second poll wins the race
/// now and then on a perfectly healthy subscription. A subscription that has
/// not spoken this long after a confirmed tip change is not delivering.
const ZMQ_GRACE_SECS: u64 = 10;

/// What the poll concludes about a tip change it noticed at `observed_at`.
#[derive(Debug, PartialEq, Eq)]
enum ZmqVerdict {
    /// ZMQ has spoken since the poll's observation: the subscription is alive
    /// and the poll merely won the race for this block.
    Delivered,
    /// Still inside the grace period; keep watching.
    Pending,
    /// The grace period passed with no ZMQ message.
    Silent,
}

/// How far before the poll's observation a ZMQ message still counts as being
/// about the same tip change.
///
/// The poll learns of a block up to one interval after it connects, while ZMQ
/// is stamped as it arrives, so a healthy subscription is routinely stamped
/// *earlier* than the observation it belongs to. Both stamps are whole
/// seconds, which costs another second at the boundary. Without this slack the
/// verdict is a coin flip on every block: measured on prod, ZMQ beat the poll
/// by ~0.5 s on 5 of 6 blocks and the warning fired on 43 of 97 tip changes
/// while the node had in fact delivered every one.
fn poll_slack_secs(poll_interval_ms: u64) -> u64 {
    poll_interval_ms.div_ceil(1_000) + 1
}

fn zmq_verdict(zmq_last_seen: u64, observed_at: u64, now: u64, slack: u64) -> ZmqVerdict {
    // A message stamped up to `slack` seconds before the poll's observation is
    // the notification for this tip change, not a stale one: the poll cannot
    // notice a block sooner than ZMQ can announce it.
    if zmq_last_seen + slack >= observed_at {
        ZmqVerdict::Delivered
    } else if now.saturating_sub(observed_at) > ZMQ_GRACE_SECS {
        ZmqVerdict::Silent
    } else {
        ZmqVerdict::Pending
    }
}

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
    // A tip change the poll saw first, awaiting ZMQ's own notification.
    let mut awaiting_zmq: Option<u64> = None;

    loop {
        if let Some(observed_at) = awaiting_zmq {
            match zmq_verdict(
                zmq_last_seen.load(Ordering::Relaxed),
                observed_at,
                now_secs(),
                poll_slack_secs(poll_interval_ms),
            ) {
                ZmqVerdict::Pending => {}
                ZmqVerdict::Delivered => awaiting_zmq = None,
                ZmqVerdict::Silent => {
                    awaiting_zmq = None;
                    metrics::rpc_fallback_used();
                    warn!(
                        "Tip changed {ZMQ_GRACE_SECS}s ago and ZMQ never delivered it — the \
                         hashblock subscription is not delivering. Check \
                         zmq.hashblock_endpoint and that the node runs with -zmqpubhashblock."
                    );
                }
            }
        }

        // bitcoincore-rpc is synchronous; run it on the blocking pool so a hung
        // node cannot pin a runtime worker for the transport timeout.
        let rpc = Arc::clone(&rpc);
        match tokio::task::spawn_blocking(move || rpc.best_block_hash()).await {
            Ok(Ok(hash)) => {
                if hash != last_hash {
                    // Skip the first observation: that is the poll learning the
                    // current tip at startup, not a missed notification. For
                    // every later change, give ZMQ a grace period to deliver
                    // before concluding anything; the verdict is taken at the
                    // top of the loop. The signal below goes out regardless,
                    // so a lagging or dead subscription costs no job latency.
                    if !last_hash.is_empty() && awaiting_zmq.is_none() {
                        awaiting_zmq = Some(now_secs());
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

#[cfg(test)]
mod tests {
    use super::{poll_slack_secs, zmq_verdict, ZmqVerdict, ZMQ_GRACE_SECS};

    /// Slack for the default 1000 ms poll.
    const SLACK: u64 = 2;

    #[test]
    fn zmq_beating_the_poll_is_delivered() {
        // Same second, whichever came first.
        assert_eq!(
            zmq_verdict(1_000, 1_000, 1_000, SLACK),
            ZmqVerdict::Delivered
        );
        // The block connected at t=999.9: ZMQ stamped 999, the poll's next tick
        // noticed at t=1000.1 and stamped 1000. ZMQ delivered *first*, so this
        // is the subscription working, not failing. Asserting Pending here is
        // what made the warning fire on roughly half of all blocks.
        assert_eq!(zmq_verdict(999, 1_000, 1_000, SLACK), ZmqVerdict::Delivered);
        assert_eq!(
            zmq_verdict(999, 1_000, 1_000 + ZMQ_GRACE_SECS + 1, SLACK),
            ZmqVerdict::Delivered,
            "and it stays delivered once the grace has elapsed"
        );
    }

    #[test]
    fn zmq_lagging_the_poll_inside_the_grace_is_delivered() {
        // The poll won the race at t=1000; ZMQ delivered at t=1003. Before it
        // did, the verdict must stay pending, never silent.
        assert_eq!(zmq_verdict(400, 1_000, 1_002, SLACK), ZmqVerdict::Pending);
        assert_eq!(
            zmq_verdict(1_003, 1_000, 1_004, SLACK),
            ZmqVerdict::Delivered
        );
    }

    #[test]
    fn no_zmq_message_after_the_grace_is_silent() {
        let observed = 1_000;
        assert_eq!(
            zmq_verdict(400, observed, observed + ZMQ_GRACE_SECS, SLACK),
            ZmqVerdict::Pending,
            "the boundary second is still inside the grace"
        );
        assert_eq!(
            zmq_verdict(400, observed, observed + ZMQ_GRACE_SECS + 1, SLACK),
            ZmqVerdict::Silent
        );
        // Never delivered at all since boot (last_seen 0) is the same case.
        assert_eq!(
            zmq_verdict(0, observed, observed + ZMQ_GRACE_SECS + 1, SLACK),
            ZmqVerdict::Silent
        );
    }

    #[test]
    fn slack_covers_a_whole_poll_interval() {
        // The slack has to track poll_interval_ms: at 5000 ms, documented in
        // config.toml.example as the low-call-volume setting, the poll can
        // notice a tip five seconds after ZMQ announced it. A fixed one-second
        // tolerance would call that silent on most blocks.
        assert_eq!(poll_slack_secs(1_000), 2);
        assert_eq!(poll_slack_secs(5_000), 6);
        assert_eq!(poll_slack_secs(1_500), 3);

        let slack = poll_slack_secs(5_000);
        assert_eq!(
            zmq_verdict(995, 1_000, 1_020, slack),
            ZmqVerdict::Delivered,
            "ZMQ spoke one poll interval before the poll noticed"
        );
        // A genuinely dead subscription is still caught at any interval.
        assert_eq!(zmq_verdict(900, 1_000, 1_020, slack), ZmqVerdict::Silent);
    }
}
