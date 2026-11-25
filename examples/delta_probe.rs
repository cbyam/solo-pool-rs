//! Template policy delta probe: what a `prioritisetransaction` delta does on
//! the operator's node besides block building. See "What the delta changes
//! besides block building" in docs/design/template-policy.md.
//!
//!   cargo run --release --example delta_probe -- \
//!       --bitcoind /path/to/bitcoind --workdir /tmp/delta-probe-big --mode big
//!
//! It starts three regtest nodes, connected peer <-> pool <-> relay:
//!
//! - pool: the operator's node. Sets the deltas, `-maxmempool=5`.
//! - peer: no deltas, default mempool. Mines, and stands in for the network.
//! - relay: carries the same delta as pool and forwards a replacement to it
//!   over P2P, the way a replacement normally arrives.
//!
//! `--mode big` uses the design's delta of -1,000,000,003 sats. `--mode zero`
//! uses minus the transaction's fee, so the modified fee is exactly 0.
//! On Knots it also sends the -1e17 priority delta and sets
//! `-blockprioritysize=0`, as the pool would.
//!
//! `--node-arg <arg>` (repeatable) passes an option to all three nodes, for
//! example `--node-arg -blockprioritysize=0` on Roots.
//!
//! The workdir must be empty or absent. Ports `--port-base` to `--port-base +
//! 21` are used (default 18700). Results go to `<workdir>/results.json`.
//! `bitcoind` defaults to `$BITCOIND`, else `bitcoind` on the PATH.
//! Regtest only. Not part of the shipping pool.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde_json::{json, Map, Value};

const BIG_DELTA: i64 = -1_000_000_003;
const KNOTS_PRIORITY_DELTA: f64 = -1e17;
/// Value of each coin the fan-out creates, in sats.
const COIN: i64 = 30_000_000;
const WALLET: &str = "w";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Big,
    Zero,
}

impl Mode {
    fn delta(self, fee: i64) -> i64 {
        match self {
            Mode::Big => BIG_DELTA,
            Mode::Zero => -fee,
        }
    }
}

struct Args {
    bitcoind: String,
    workdir: PathBuf,
    mode: Mode,
    port_base: u16,
    node_args: Vec<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        bitcoind: std::env::var("BITCOIND").unwrap_or_else(|_| "bitcoind".into()),
        workdir: std::env::temp_dir().join("delta-probe"),
        mode: Mode::Big,
        port_base: 18700,
        node_args: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--bitcoind" => args.bitcoind = value(),
            "--workdir" => args.workdir = PathBuf::from(value()),
            "--mode" => {
                args.mode = match value().as_str() {
                    "big" => Mode::Big,
                    "zero" => Mode::Zero,
                    other => usage(&format!("unknown mode {other}")),
                }
            }
            "--port-base" => args.port_base = value().parse().unwrap_or_else(|_| usage("bad port")),
            "--node-arg" => args.node_args.push(value()),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    args
}

fn usage(msg: &str) -> ! {
    eprintln!("{msg}");
    eprintln!(
        "usage: delta_probe [--bitcoind PATH] [--workdir DIR] [--mode big|zero] [--port-base N] [--node-arg ARG]..."
    );
    std::process::exit(2);
}

struct Node {
    name: &'static str,
    bitcoind: String,
    dir: PathBuf,
    p2p: u16,
    rpc: u16,
    extra: Vec<String>,
    child: Option<Child>,
}

impl Node {
    fn new(name: &'static str, a: &Args, offset: u16, extra: &[&str]) -> Self {
        Node {
            name,
            bitcoind: a.bitcoind.clone(),
            dir: a.workdir.join(name),
            p2p: a.port_base + offset,
            rpc: a.port_base + offset + 1,
            extra: extra
                .iter()
                .map(|s| s.to_string())
                .chain(a.node_args.iter().cloned())
                .collect(),
            child: None,
        }
    }

    fn start(&mut self) {
        fs::create_dir_all(&self.dir).expect("create datadir");
        let mut args = vec![
            "-regtest".to_string(),
            format!("-datadir={}", self.dir.display()),
            format!("-port={}", self.p2p),
            format!("-bind=127.0.0.1:{}", self.p2p),
            format!("-rpcport={}", self.rpc),
        ];
        args.extend(
            [
                "-listenonion=0",
                "-dnsseed=0",
                "-fixedseeds=0",
                "-rpcbind=127.0.0.1",
                "-rpcallowip=127.0.0.1",
                "-server=1",
                "-fallbackfee=0.0001",
                "-printtoconsole=0",
                "-logtimemicros=1",
                "-debug=cmpctblock",
                "-debug=mempool",
                "-debug=mempoolrej",
                "-rpcthreads=8",
            ]
            .map(String::from),
        );
        args.extend(self.extra.iter().cloned());
        self.child = Some(
            Command::new(&self.bitcoind)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn bitcoind"),
        );
        let ready = wait_until(Duration::from_secs(60), || {
            self.call("getblockchaininfo", &[]).is_ok()
        });
        assert!(ready, "{} did not start", self.name);
    }

    fn client(&self, wallet: bool) -> Result<Client, String> {
        let mut url = format!("http://127.0.0.1:{}", self.rpc);
        if wallet {
            url.push_str(&format!("/wallet/{WALLET}"));
        }
        Client::new(&url, Auth::CookieFile(self.dir.join("regtest/.cookie")))
            .map_err(|e| e.to_string())
    }

    fn call(&self, method: &str, params: &[Value]) -> Result<Value, String> {
        self.client(false)?
            .call(method, params)
            .map_err(|e| e.to_string())
    }

    fn rpc(&self, method: &str, params: &[Value]) -> Value {
        self.call(method, params)
            .unwrap_or_else(|e| panic!("{} {method}: {e}", self.name))
    }

    fn wallet(&self, method: &str, params: &[Value]) -> Value {
        self.client(true)
            .and_then(|c| c.call(method, params).map_err(|e| e.to_string()))
            .unwrap_or_else(|e| panic!("{} {method}: {e}", self.name))
    }

    fn has(&self, txid: &str) -> bool {
        self.rpc("getrawmempool", &[])
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t == txid))
    }

    fn mempool(&self) -> Vec<String> {
        strings(&self.rpc("getrawmempool", &[]))
    }

    /// This node's `getprioritisedtransactions` entry for txid, or "cleared".
    fn delta_entry(&self, txid: &str) -> Value {
        self.rpc("getprioritisedtransactions", &[])
            .get(txid)
            .cloned()
            .unwrap_or_else(|| json!("cleared"))
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("regtest/debug.log")).unwrap_or_default()
    }

    fn peers(&self) -> usize {
        self.rpc("getpeerinfo", &[]).as_array().map_or(0, Vec::len)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = self.call("stop", &[]);
        let stopped = wait_until(Duration::from_secs(60), || {
            matches!(child.try_wait(), Ok(Some(_)))
        });
        if !stopped {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

struct Probe {
    pool: Node,
    peer: Node,
    relay: Node,
    knots: bool,
    mode: Mode,
    coins: Vec<(String, u64)>,
}

impl Probe {
    fn newaddr(&self) -> String {
        str_of(
            &self
                .peer
                .wallet("getnewaddress", &[json!(""), json!("bech32")]),
        )
    }

    fn coin(&mut self) -> (String, u64) {
        self.coins.pop().expect("ran out of coins")
    }

    /// Build and sign a transaction with the peer's wallet. Every input
    /// signals replaceability.
    fn sign(&self, inputs: &[(String, u64)], outputs: Map<String, Value>) -> String {
        let ins: Vec<Value> = inputs
            .iter()
            .map(|(txid, vout)| json!({"txid": txid, "vout": vout, "sequence": 0xFFFF_FFFDu32}))
            .collect();
        let raw = self.peer.rpc(
            "createrawtransaction",
            &[json!(ins), Value::Object(outputs)],
        );
        let signed = self.peer.wallet("signrawtransactionwithwallet", &[raw]);
        assert_eq!(signed["complete"], json!(true), "signing failed: {signed}");
        str_of(&signed["hex"])
    }

    fn decode(&self, hex: &str) -> Value {
        self.peer.rpc("decoderawtransaction", &[json!(hex)])
    }

    fn txid(&self, hex: &str) -> String {
        str_of(&self.decode(hex)["txid"])
    }

    /// Pay `payouts` from `coin` at `rate` sat/vB, the rest to `change`.
    fn build_at_rate(
        &self,
        coin: &(String, u64),
        payouts: &Map<String, Value>,
        paid: i64,
        change: &str,
        rate: i64,
    ) -> String {
        let mut fee = 0;
        let mut tx = String::new();
        for _ in 0..3 {
            let mut outs = payouts.clone();
            outs.insert(change.into(), json!(btc(COIN - paid - fee)));
            tx = self.sign(std::slice::from_ref(coin), outs);
            let vsize = self.decode(&tx)["vsize"].as_i64().expect("vsize");
            if fee >= rate * vsize {
                break;
            }
            fee = rate * vsize + 2;
        }
        tx
    }

    /// Send to the peer (must accept) and then the pool node. Returns whether
    /// the pool node accepted it.
    fn send_both(&self, hex: &str) -> bool {
        let mut pool_ok = true;
        for node in [&self.peer, &self.pool] {
            if let Err(e) = node.call("sendrawtransaction", &[json!(hex)]) {
                if e.contains("already") {
                    continue;
                }
                assert!(node.name == "pool", "peer refused a transaction: {e}");
                pool_ok = false;
            }
        }
        pool_ok
    }

    fn prioritise(&self, node: &Node, txid: &str, fee_delta: i64) {
        // On Knots the priority delta takes the fee delta's sign, so a
        // cancelling call undoes both.
        let priority = if self.knots {
            json!(KNOTS_PRIORITY_DELTA.abs() * fee_delta.signum() as f64)
        } else {
            json!(0)
        };
        node.rpc(
            "prioritisetransaction",
            &[json!(txid), priority, json!(fee_delta)],
        );
    }

    /// Mine on the peer and wait for the pool node to follow.
    fn mine(&self, n: u64) -> Vec<String> {
        let addr = self.newaddr();
        let hashes = strings(&self.peer.rpc("generatetoaddress", &[json!(n), json!(addr)]));
        let last = hashes.last().cloned().expect("no block hashes");
        let synced = wait_until(Duration::from_secs(120), || {
            self.pool.rpc("getbestblockhash", &[]) == json!(last)
        });
        assert!(synced, "pool node did not reach {last}");
        hashes
    }
}

fn main() {
    let args = parse_args();
    if fs::read_dir(&args.workdir).is_ok_and(|mut d| d.next().is_some()) {
        usage(&format!("{} is not empty", args.workdir.display()));
    }
    let version = Command::new(&args.bitcoind)
        .arg("-version")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_else(|e| usage(&format!("cannot run {}: {e}", args.bitcoind)));
    let knots = version.contains("Knots");
    let extra: &[&str] = if knots {
        &["-blockprioritysize=0"]
    } else {
        &[]
    };
    let pool_extra: Vec<&str> = ["-maxmempool=5"].iter().chain(extra).copied().collect();

    let mut probe = Probe {
        pool: Node::new("pool", &args, 0, &pool_extra),
        peer: Node::new("peer", &args, 10, extra),
        relay: Node::new("relay", &args, 20, extra),
        knots,
        mode: args.mode,
        coins: Vec::new(),
    };
    let mut results = Map::new();
    results.insert("node".into(), json!(version));
    results.insert("node_args".into(), json!(args.node_args));
    results.insert(
        "mode".into(),
        json!(if args.mode == Mode::Big {
            "big"
        } else {
            "zero"
        }),
    );
    run(&mut probe, &mut results);
    let out = serde_json::to_string_pretty(&Value::Object(results)).expect("serialize");
    fs::write(args.workdir.join("results.json"), &out).expect("write results.json");
    println!("{out}");
}

fn run(p: &mut Probe, results: &mut Map<String, Value>) {
    p.pool.start();
    p.peer.start();
    p.relay.start();
    for other in [&p.peer, &p.relay] {
        let addr = format!("127.0.0.1:{}", other.p2p);
        p.pool.rpc("addnode", &[json!(addr), json!("onetry")]);
    }
    let peered = wait_until(Duration::from_secs(60), || {
        p.pool.peers() >= 2 && p.peer.peers() >= 1 && p.relay.peers() >= 1
    });
    assert!(peered, "nodes did not connect");
    let info = p.pool.rpc("getmempoolinfo", &[]);
    results.insert(
        "mempoolinfo_pool".into(),
        json!({
            "maxmempool": info["maxmempool"],
            "minrelaytxfee": info["minrelaytxfee"],
            "incrementalrelayfee": info["incrementalrelayfee"],
        }),
    );

    p.peer.rpc("createwallet", &[json!(WALLET)]);
    p.mine(110);
    fan_out(p);
    eprintln!("{} coins ready", p.coins.len());

    let r = replacement_by_rpc(p);
    eprintln!("replacement: {r}");
    results.insert("1_replacement".into(), r);
    let r = replacement_by_p2p(p);
    eprintln!("replacement p2p: {r}");
    results.insert("1b_replacement_p2p".into(), r);
    let (evict, refusal) = eviction_and_refusal(p);
    results.insert("2_eviction".into(), evict);
    results.insert("3_refusal_on_return".into(), refusal);
}

/// Split one mature coinbase into 150 coins of `COIN` sats.
fn fan_out(p: &mut Probe) {
    let unspent = p.peer.wallet("listunspent", &[json!(100)]);
    let cb = unspent
        .as_array()
        .and_then(|a| a.iter().find(|u| sats(&u["amount"]) == 50 * 100_000_000))
        .cloned()
        .expect("no mature coinbase");
    let mut outs = Map::new();
    for _ in 0..150 {
        outs.insert(p.newaddr(), json!(btc(COIN)));
    }
    outs.insert(
        p.newaddr(),
        json!(btc(50 * 100_000_000 - 150 * COIN - 100_000)),
    );
    let input = (str_of(&cb["txid"]), cb["vout"].as_u64().expect("vout"));
    let fan = p.sign(&[input], outs);
    p.peer.rpc("sendrawtransaction", &[json!(fan)]);
    p.mine(1);
    let unspent = p.peer.wallet("listunspent", &[json!(1)]);
    p.coins = unspent
        .as_array()
        .expect("listunspent")
        .iter()
        .filter(|u| sats(&u["amount"]) == COIN)
        .map(|u| (str_of(&u["txid"]), u["vout"].as_u64().expect("vout")))
        .collect();
}

const A_FEE: i64 = 10_000;
const B_FEE: i64 = 300;

/// Original A and a low-fee replacement B spending the same coin.
fn replacement_pair(p: &mut Probe) -> (String, String) {
    let c = p.coin();
    let a = p.sign(
        std::slice::from_ref(&c),
        one_output(p.newaddr(), COIN - A_FEE),
    );
    let b = p.sign(&[c], one_output(p.newaddr(), COIN - B_FEE));
    (a, b)
}

/// 1. The replacement is sent to the pool node by RPC.
fn replacement_by_rpc(p: &mut Probe) -> Value {
    let (a, b) = replacement_pair(p);
    let (a_txid, b_txid) = (p.txid(&a), p.txid(&b));
    p.send_both(&a);
    let mut r = Map::new();
    r.insert("A_fee_sat".into(), json!(A_FEE));
    r.insert("B_fee_sat".into(), json!(B_FEE));
    r.insert("B_vsize".into(), p.decode(&b)["vsize"].clone());
    r.insert("pool_B_before_delta".into(), tma(&p.pool, &b));
    p.prioritise(&p.pool, &a_txid, p.mode.delta(A_FEE));
    r.insert("delta_on_A".into(), p.pool.delta_entry(&a_txid));
    r.insert("pool_B_after_delta".into(), tma(&p.pool, &b));
    r.insert("peer_B".into(), tma(&p.peer, &b));
    let sent = match p.pool.call("sendrawtransaction", &[json!(b)]) {
        Ok(_) => "accepted".to_string(),
        Err(e) => e,
    };
    r.insert("pool_send_B".into(), json!(sent));
    r.insert(
        "delta_on_A_after_replacement".into(),
        p.pool.delta_entry(&a_txid),
    );
    sleep(Duration::from_secs(20));
    r.insert(
        "after_20s".into(),
        json!({
            "pool_has_A": p.pool.has(&a_txid),
            "pool_has_B": p.pool.has(&b_txid),
            "peer_has_A": p.peer.has(&a_txid),
            "peer_has_B": p.peer.has(&b_txid),
        }),
    );
    // The peer mines A, which the pool node replaced.
    let blk = p.mine(1).remove(0);
    r.insert("peer_mines_A".into(), reconstruct(&p.pool, &blk));
    r.insert(
        "delta_on_A_after_A_confirmed".into(),
        p.pool.delta_entry(&a_txid),
    );
    r.insert(
        "pool_rejected_A_from_a_peer".into(),
        json!(rejected_from_peer(&p.pool, &a_txid)),
    );
    Value::Object(r)
}

/// 1b. The relay node, carrying the same delta, forwards the replacement to
/// the pool node over P2P.
fn replacement_by_p2p(p: &mut Probe) -> Value {
    let (a, b) = replacement_pair(p);
    let (a_txid, b_txid) = (p.txid(&a), p.txid(&b));
    for node in [&p.peer, &p.pool, &p.relay] {
        node.rpc("sendrawtransaction", &[json!(a)]);
    }
    p.prioritise(&p.pool, &a_txid, p.mode.delta(A_FEE));
    p.prioritise(&p.relay, &a_txid, p.mode.delta(A_FEE));
    p.relay.rpc("sendrawtransaction", &[json!(b)]);
    let t0 = Instant::now();
    let arrived = wait_until(Duration::from_secs(120), || p.pool.has(&b_txid));
    assert!(arrived, "replacement never reached the pool node");
    let accepted_from_peer = p.pool.log().lines().any(|l| {
        l.contains("AcceptToMemoryPool: peer=") && l.contains(&format!("accepted {b_txid}"))
    });
    let mut r = Map::new();
    r.insert(
        "B_reached_pool_after_s".into(),
        json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0),
    );
    r.insert(
        "pool_accepted_B_from_peer".into(),
        json!(accepted_from_peer),
    );
    r.insert(
        "delta_on_A_after_replacement".into(),
        p.pool.delta_entry(&a_txid),
    );
    r.insert("peer_has_A".into(), json!(p.peer.has(&a_txid)));
    let blk = p.mine(1).remove(0);
    r.insert("peer_mines_A".into(), reconstruct(&p.pool, &blk));
    r.insert(
        "pool_rejected_A_from_a_peer".into(),
        json!(rejected_from_peer(&p.pool, &a_txid)),
    );
    Value::Object(r)
}

/// Eviction from a full mempool (check 2), refusal on return (check 3), and
/// the compact-block cost when the peer mines what the pool node evicted.
fn eviction_and_refusal(p: &mut Probe) -> (Value, Value) {
    // U: unwanted parent at ~50 sat/vB with a 100 sat/vB child C.
    // L: a lone unwanted transaction of the same shape and feerate.
    let u_fee: i64 = 50 * 141;
    let c_fee: i64 = 100 * 110;
    let coin = p.coin();
    let u_change = p.newaddr();
    let mut outs = one_output(p.newaddr(), 10_000_000);
    outs.insert(u_change.clone(), json!(btc(COIN - 10_000_000 - u_fee)));
    let u_hex = p.sign(&[coin], outs);
    let u = p.decode(&u_hex);
    let u_txid = str_of(&u["txid"]);
    let u_vout = u["vout"]
        .as_array()
        .and_then(|v| {
            v.iter()
                .find(|o| o["scriptPubKey"]["address"] == json!(u_change))
        })
        .and_then(|o| o["n"].as_u64())
        .expect("change output");
    p.send_both(&u_hex);
    let c_hex = p.sign(
        &[(u_txid.clone(), u_vout)],
        one_output(p.newaddr(), COIN - 10_000_000 - u_fee - c_fee),
    );
    let c = p.decode(&c_hex);
    let c_txid = str_of(&c["txid"]);
    p.send_both(&c_hex);
    let coin = p.coin();
    let mut outs = one_output(p.newaddr(), 10_000_000);
    outs.insert(p.newaddr(), json!(btc(COIN - 10_000_000 - u_fee)));
    let l_hex = p.sign(&[coin], outs);
    let l_txid = p.txid(&l_hex);
    p.send_both(&l_hex);
    p.prioritise(&p.pool, &u_txid, p.mode.delta(u_fee));
    p.prioritise(&p.pool, &l_txid, p.mode.delta(u_fee));

    let mut r = Map::new();
    let feerate = |fee: i64, tx: &Value| {
        let vsize = tx["vsize"].as_f64().expect("vsize");
        (fee as f64 / vsize * 10.0).round() / 10.0
    };
    r.insert("U_feerate".into(), json!(feerate(u_fee, &u)));
    r.insert("C_feerate".into(), json!(feerate(c_fee, &c)));
    r.insert("delta_on_U".into(), p.pool.delta_entry(&u_txid));
    r.insert("delta_on_L".into(), p.pool.delta_entry(&l_txid));
    sleep(Duration::from_secs(6)); // past GBT's 5 s cache window
    let gbt = p
        .pool
        .rpc("getblocktemplate", &[json!({"rules": ["segwit"]})]);
    let in_gbt = |txid: &str| {
        gbt["transactions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t["txid"] == json!(txid)))
    };
    r.insert(
        "pool_template_has".into(),
        json!({"U": in_gbt(&u_txid), "C": in_gbt(&c_txid), "L": in_gbt(&l_txid)}),
    );

    // Fill with 5 sat/vB transactions of 2,000 outputs until the pool node
    // evicts U and L, or 60 transactions have gone in.
    let descriptors = p.peer.wallet("listdescriptors", &[]);
    let desc = descriptors["descriptors"]
        .as_array()
        .and_then(|a| {
            a.iter().find(|d| {
                d["desc"].as_str().is_some_and(|s| s.starts_with("wpkh("))
                    && d["internal"] != json!(true)
            })
        })
        .map(|d| d["desc"].clone())
        .expect("wpkh descriptor");
    let fill_addrs = strings(
        &p.peer
            .rpc("deriveaddresses", &[desc, json!([2000, 2000 + 2000 * 60])]),
    );
    let fill_rate = 5;
    let (mut fills, mut refused) = (Vec::new(), 0);
    let (mut u_evicted_at, mut l_evicted_at) = (None, None);
    for i in 0..60 {
        let coin = p.coin();
        let mut payouts = Map::new();
        for a in &fill_addrs[i * 2000..(i + 1) * 2000] {
            payouts.insert(a.clone(), json!(btc(5000)));
        }
        let change = p.newaddr();
        let tx = p.build_at_rate(&coin, &payouts, 2000 * 5000, &change, fill_rate);
        fills.push(p.txid(&tx));
        if !p.send_both(&tx) {
            refused += 1;
        }
        let now = p.pool.mempool();
        if l_evicted_at.is_none() && !now.contains(&l_txid) {
            l_evicted_at = Some(i + 1);
        }
        if u_evicted_at.is_none() && !now.contains(&u_txid) {
            u_evicted_at = Some(i + 1);
        }
        if u_evicted_at.is_some() && l_evicted_at.is_some() {
            break;
        }
    }
    let info = p.pool.rpc("getmempoolinfo", &[]);
    let pool_mp = p.pool.mempool();
    let peer_mp = p.peer.mempool();
    let missing: Vec<String> = peer_mp
        .iter()
        .filter(|t| !pool_mp.contains(t))
        .cloned()
        .collect();
    r.insert("fill_feerate".into(), json!(fill_rate));
    r.insert("fills_sent_until_U_evicted".into(), json!(u_evicted_at));
    r.insert("fills_sent_until_L_evicted".into(), json!(l_evicted_at));
    r.insert("fills_refused_by_pool".into(), json!(refused));
    r.insert(
        "pool_mempool".into(),
        json!({"size": info["size"], "usage": info["usage"], "mempoolminfee": info["mempoolminfee"]}),
    );
    for (name, txid) in [("U", &u_txid), ("C", &c_txid), ("L", &l_txid)] {
        r.insert(format!("{name}_in_pool"), json!(pool_mp.contains(txid)));
    }
    r.insert(
        "fills_missing_from_pool".into(),
        json!(fills.iter().filter(|t| !pool_mp.contains(t)).count()),
    );
    r.insert("pool_missing_vs_peer".into(), json!(missing.len()));
    r.insert("delta_on_U_after_fill".into(), p.pool.delta_entry(&u_txid));
    r.insert("delta_on_L_after_fill".into(), p.pool.delta_entry(&l_txid));
    eprintln!("eviction: {}", Value::Object(r.clone()));

    // 3. Refusal on return: rebroadcast while the delta stands, after
    // cancelling it, and after setting it again.
    let mut refusal = Map::new();
    for (name, hex, txid) in [("U", &u_hex, &u_txid), ("L", &l_hex, &l_txid)] {
        let delta = p.mode.delta(u_fee);
        refusal.insert(format!("{name}_with_delta"), tma(&p.pool, hex));
        p.prioritise(&p.pool, txid, -delta);
        refusal.insert(
            format!("{name}_delta_after_cancel"),
            p.pool.delta_entry(txid),
        );
        refusal.insert(format!("{name}_delta_cancelled"), tma(&p.pool, hex));
        p.prioritise(&p.pool, txid, delta);
        refusal.insert(format!("{name}_delta_reapplied"), tma(&p.pool, hex));
    }
    eprintln!("refusal: {}", Value::Object(refusal.clone()));

    // The peer mines U, C and L.
    let blk = p.mine(1).remove(0);
    let block = p.peer.rpc("getblock", &[json!(blk)]);
    let block_txids: Vec<String> = strings(&block["tx"]).into_iter().skip(1).collect();
    let has = |t: &String| block_txids.contains(t);
    r.insert(
        "peer_block".into(),
        json!({
            "txs": block_txids.len(),
            "has_U": has(&u_txid), "has_C": has(&c_txid), "has_L": has(&l_txid),
            "block_txs_pool_lacked": block_txids.iter().filter(|t| missing.contains(t)).count(),
            "reconstruct": reconstruct(&p.pool, &blk),
        }),
    );
    // Control: mine the rest, then a block of three small transactions both
    // nodes hold.
    for _ in 0..6 {
        if p.peer.mempool().is_empty() {
            break;
        }
        p.mine(1);
    }
    for _ in 0..3 {
        let coin = p.coin();
        let tx = p.sign(&[coin], one_output(p.newaddr(), COIN - 2000));
        p.send_both(&tx);
    }
    let blk = p.mine(1).remove(0);
    r.insert("control_block".into(), reconstruct(&p.pool, &blk));
    eprintln!("eviction and blocks: {}", Value::Object(r.clone()));
    (Value::Object(r), Value::Object(refusal))
}

fn tma(node: &Node, hex: &str) -> Value {
    let r = node.rpc("testmempoolaccept", &[json!([hex])]);
    let r = &r[0];
    json!({"allowed": r["allowed"], "reason": r["reject-reason"], "details": r["reject-details"]})
}

/// The pool node's compact-block log lines for a block, and the time from
/// receiving the compact block to reconstructing it.
fn reconstruct(node: &Node, block: &str) -> Value {
    let log = node.log();
    let find = |needle: &str| {
        log.lines()
            .find(|l| l.contains(needle) && l.contains(block))
            .map(str::to_string)
    };
    let init = find("Initialized PartiallyDownloadedBlock for block");
    let done = find("Successfully reconstructed block");
    let mut r = Map::new();
    if let (Some(i), Some(d)) = (&init, &done) {
        if let (Some(a), Some(b)) = (log_seconds(i), log_seconds(d)) {
            r.insert(
                "init_to_reconstructed_ms".into(),
                json!(((b - a) * 100_000.0).round() / 100.0),
            );
        }
    }
    match done {
        Some(d) => r.insert("line".into(), json!(d)),
        None => r.insert(
            "note".into(),
            json!("no reconstruction line for this block (full block delivery?)"),
        ),
    };
    Value::Object(r)
}

/// True if the node logged a P2P rejection of txid. A rejected transaction
/// also goes into the compact-block extra pool, which would explain an
/// extra-pool hit.
fn rejected_from_peer(node: &Node, txid: &str) -> bool {
    node.log()
        .lines()
        .any(|l| l.contains(txid) && l.contains(" from peer=") && l.contains("was not accepted"))
}

/// Seconds since midnight from a `-logtimemicros` timestamp
/// (`2026-11-14T20:03:33.175441Z ...`).
fn log_seconds(line: &str) -> Option<f64> {
    let time = line.get(11..26)?;
    let mut parts = time.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

fn one_output(addr: String, value: i64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(addr, json!(btc(value)));
    m
}

/// Sats as a BTC amount string, which bitcoind accepts in place of a number.
fn btc(sats: i64) -> String {
    format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}

fn sats(amount: &Value) -> i64 {
    (amount.as_f64().expect("amount") * 1e8).round() as i64
}

fn str_of(v: &Value) -> String {
    v.as_str().expect("string").to_string()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().expect("array").iter().map(str_of).collect()
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}
