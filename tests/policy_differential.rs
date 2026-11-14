//! Differential test of the template policy classifier against a node that
//! enforces the same policy: Bitcoin Knots, or Bitcoin Roots.
//!
//! Each vector is a transaction that is standard under Core's own rules and
//! sits at, or just past, one of the classifier's limits. The node judges it
//! with `testmempoolaccept`; the classifier (`solo_pool_rs::policy::check`)
//! judges it twice, with the spent coins (exact mode) and without (shape
//! mode). All three verdicts must agree, reject reason included, under three
//! node configurations: Knots' defaults, the deployed node's settings, and a
//! permissive set that turns the optional rules off. A node reject reason the
//! classifier does not know fails the test: either the vector is not
//! standard for some other reason, or the node has a rule the classifier
//! lacks.
//!
//! The mainnet audit (`examples/template_audit.rs`) shows what the rules do on
//! real traffic. This test shows they still match the node after a Knots or
//! Roots release. See "Library crate" in docs/design/template-policy.md.
//!
//! Not covered, because Core's own rules refuse the transaction first at
//! these settings: `scriptsig-size` and `scriptcheck-size` (Core's 1,650-byte
//! scriptSig limit), and `script-size` (spending a coin with a scriptPubKey
//! over 1,650 bytes means spending a non-standard output).
//!
//! Ignored by default (needs a Knots or Roots `bitcoind`):
//!
//! ```text
//! BITCOIND=/path/to/knots/bitcoind \
//!     cargo test --release --test policy_differential -- --ignored --nocapture
//! ```
//!
//! On any other node it skips, except under CI (`$CI` set), where it fails,
//! so a misconfigured job cannot pass by skipping.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bitcoin::address::NetworkUnchecked;
use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::secp256k1::{Keypair, PublicKey, Secp256k1, SecretKey};
use bitcoin::taproot::{LeafVersion, TaprootBuilder};
use bitcoin::{
    absolute::LockTime, transaction::Version, Address, Amount, Network, OutPoint, ScriptBuf,
    Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde_json::{json, Value};
use solo_pool_rs::policy::{check, Rule, Rules};

/// Every rule, to map a node reject reason back to one.
const ALL_RULES: [Rule; 15] = [
    Rule::Cat21,
    Rule::ScriptSigSize,
    Rule::ScriptPubKeySize,
    Rule::Datacarrier,
    Rule::Runes,
    Rule::BarePubkey,
    Rule::BareMultisig,
    Rule::Olga,
    Rule::MultiOpReturn,
    Rule::BareDatacarrier,
    Rule::DatacarrierNonStandard,
    Rule::DatacarrierExceeded,
    Rule::SpentScriptSize,
    Rule::RedeemScriptSize,
    Rule::WitnessSize,
];

/// The rules the vectors must make the node reject at least once, across
/// all configurations.
const COVERED: [Rule; 12] = [
    Rule::Cat21,
    Rule::ScriptPubKeySize,
    Rule::Datacarrier,
    Rule::Runes,
    Rule::BarePubkey,
    Rule::BareMultisig,
    Rule::Olga,
    Rule::MultiOpReturn,
    Rule::BareDatacarrier,
    Rule::DatacarrierNonStandard,
    Rule::DatacarrierExceeded,
    Rule::WitnessSize,
];

const FEE: u64 = 50_000;
const CRAFTED_VALUE: u64 = 100_000;
const SMALL_VALUE: u64 = 20_000;

// ─────────────────────────────────────────────────────────────────────────────
// Node
// ─────────────────────────────────────────────────────────────────────────────

fn bitcoind_bin() -> String {
    std::env::var("BITCOIND").unwrap_or_else(|_| "bitcoind".into())
}

/// The node's version line, or `None` if it cannot run.
fn node_version() -> Option<String> {
    let out = Command::new(bitcoind_bin()).arg("-version").output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
}

struct Node {
    child: Child,
    rpc: Client,
    wallet: Client,
}

impl Node {
    fn start(datadir: &Path, extra: &[&str]) -> Self {
        std::fs::create_dir_all(datadir).expect("datadir");
        let rpc_port = free_port();
        let child = Command::new(bitcoind_bin())
            .arg("-regtest")
            .arg(format!("-datadir={}", datadir.display()))
            .arg(format!("-rpcport={rpc_port}"))
            .arg(format!("-port={}", free_port()))
            .args([
                "-listen=0",
                "-rpcbind=127.0.0.1",
                "-rpcallowip=127.0.0.1",
                "-server=1",
                "-fallbackfee=0.0001",
                "-acceptnonstdtxn=0",
            ])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bitcoind");
        let url = format!("http://127.0.0.1:{rpc_port}");
        let cookie = datadir.join("regtest/.cookie");
        let deadline = Instant::now() + Duration::from_secs(30);
        let rpc = loop {
            let ready = Client::new(&url, Auth::CookieFile(cookie.clone()))
                .ok()
                .filter(|c| c.call::<Value>("getblockchaininfo", &[]).is_ok());
            if let Some(c) = ready {
                break c;
            }
            assert!(
                Instant::now() < deadline,
                "bitcoind RPC did not become ready"
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        let wallet =
            Client::new(&format!("{url}/wallet/w"), Auth::CookieFile(cookie)).expect("client");
        Node { child, rpc, wallet }
    }

    fn call(&self, method: &str, params: &[Value]) -> Value {
        self.rpc
            .call(method, params)
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    fn wallet_call(&self, method: &str, params: &[Value]) -> Value {
        self.wallet
            .call(method, params)
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    fn new_spk(&self) -> ScriptBuf {
        let addr = self.wallet_call("getnewaddress", &[json!(""), json!("bech32")]);
        addr.as_str()
            .expect("address")
            .parse::<Address<NetworkUnchecked>>()
            .expect("parse address")
            .require_network(Network::Regtest)
            .expect("regtest address")
            .script_pubkey()
    }

    fn sign(&self, tx: &Transaction) -> Transaction {
        let signed = self.wallet_call("signrawtransactionwithwallet", &[json!(serialize_hex(tx))]);
        assert_eq!(signed["complete"], json!(true), "signing failed: {signed}");
        deserialize_hex(signed["hex"].as_str().expect("hex")).expect("signed tx")
    }

    /// The node's verdict: `Ok` or its reject reason.
    fn verdict(&self, tx: &Transaction) -> Result<(), String> {
        let r = self.call("testmempoolaccept", &[json!([serialize_hex(tx)])]);
        let r = &r[0];
        if r["allowed"] == json!(true) {
            Ok(())
        } else {
            Err(r["reject-reason"].as_str().unwrap_or("?").to_string())
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.rpc.call::<Value>("stop", &[]);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The rule behind a node reject reason. Knots prefixes the input checks with
/// `bad-txns-input-` and the witness check with `bad-witness-`.
///
/// Knots 29 (on a Core 29 base) checks `-datacarriersize` per output inside
/// `IsStandard` and reports the generic `scriptpubkey`; Roots (on Core 31)
/// reports `datacarrier`. So `scriptpubkey` counts as `datacarrier` only when
/// the classifier itself says `datacarrier`, and any other `scriptpubkey`
/// rejection still fails the test.
fn rule_for_reason(reason: &str, exact: &Result<(), Rule>) -> Option<Rule> {
    if reason == "scriptpubkey" && *exact == Err(Rule::Datacarrier) {
        return Some(Rule::Datacarrier);
    }
    ALL_RULES.into_iter().find(|r| {
        let base = r.reason();
        reason == base
            || reason == format!("bad-txns-input-{base}")
            || reason == format!("bad-witness-{base}")
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Scripts
// ─────────────────────────────────────────────────────────────────────────────

const OP_0: u8 = 0x00;
const OP_PUSHDATA1: u8 = 0x4c;
const OP_1: u8 = 0x51;
const OP_13: u8 = 0x5d;
const OP_NOP: u8 = 0x61;
const OP_IF: u8 = 0x63;
const OP_ENDIF: u8 = 0x68;
const OP_RETURN: u8 = 0x6a;
const OP_DROP: u8 = 0x75;
const OP_CHECKSIG: u8 = 0xac;
const OP_CHECKMULTISIG: u8 = 0xae;

fn push(script: &mut Vec<u8>, data: &[u8]) {
    match data.len() {
        0 => script.push(OP_0),
        n @ 1..=75 => script.push(n as u8),
        n @ 76..=255 => script.extend([OP_PUSHDATA1, n as u8]),
        n => panic!("push of {n} bytes not needed here"),
    }
    script.extend_from_slice(data);
}

/// An OP_RETURN script exactly `len` bytes long, made of direct pushes.
fn op_return(len: usize) -> ScriptBuf {
    let mut s = vec![OP_RETURN];
    while s.len() < len {
        let left = len - s.len();
        // A direct push of n bytes takes n + 1; a 1-byte remainder is OP_0.
        let n = if left > 76 { 75 } else { left - 1 };
        push(&mut s, &vec![0x42; n]);
    }
    assert_eq!(s.len(), len);
    ScriptBuf::from_bytes(s)
}

/// `OP_FALSE OP_IF <data> OP_ENDIF OP_1`, an inscription-style envelope.
fn envelope(data_len: usize) -> ScriptBuf {
    let mut s = vec![OP_0, OP_IF];
    push(&mut s, &vec![0x6f; data_len]);
    s.extend([OP_ENDIF, OP_1]);
    ScriptBuf::from_bytes(s)
}

fn pubkey(seed: u8) -> PublicKey {
    let secp = Secp256k1::new();
    PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[seed; 32]).expect("key"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Vectors
// ─────────────────────────────────────────────────────────────────────────────

struct Vector {
    name: &'static str,
    tx: Transaction,
    spent: Vec<TxOut>,
}

/// A coin the test can spend without the wallet: its outpoint, output and
/// the witness that spends it.
struct Crafted {
    name: &'static str,
    outpoint: OutPoint,
    txout: TxOut,
    witness: Witness,
}

/// Script-path spends to fund: (vector name, scriptPubKey, spending witness).
fn crafted_scripts() -> Vec<(&'static str, ScriptBuf, Witness)> {
    let p2wsh = |name, ws: ScriptBuf| {
        let spk = ScriptBuf::new_p2wsh(&ws.wscript_hash());
        (name, spk, Witness::from_slice(&[ws.as_bytes()]))
    };
    let secp = Secp256k1::new();
    let internal = Keypair::from_seckey_slice(&secp, &[1; 32])
        .expect("key")
        .x_only_public_key()
        .0;
    let tapscript = |name, leaf: ScriptBuf| {
        let info = TaprootBuilder::new()
            .add_leaf(0, leaf.clone())
            .expect("leaf")
            .finalize(&secp, internal)
            .expect("finalize");
        let control = info
            .control_block(&(leaf.clone(), LeafVersion::TapScript))
            .expect("control block");
        let spk = ScriptBuf::new_p2tr_tweaked(info.output_key());
        (
            name,
            spk,
            Witness::from_slice(&[leaf.as_bytes(), &control.serialize()]),
        )
    };
    // A witness of [leaf, 33-byte control block] serialises to
    // 1 + (3 + leaf) + (1 + 33) bytes once the leaf is 253 bytes or more.
    let nop_leaf = |witness_size: usize| {
        let mut s = vec![OP_NOP; witness_size - 38 - 1];
        s.push(OP_1);
        ScriptBuf::from_bytes(s)
    };
    let mut push_drop = Vec::new();
    push(&mut push_drop, &[0x70; 20]);
    push_drop.extend([OP_DROP, OP_1]);
    vec![
        p2wsh("p2wsh-op-true", ScriptBuf::from_bytes(vec![OP_1])),
        p2wsh("p2wsh-envelope-20", envelope(20)),
        p2wsh("p2wsh-push-drop", ScriptBuf::from_bytes(push_drop)),
        tapscript("tapscript-op-true", ScriptBuf::from_bytes(vec![OP_1])),
        // An envelope of d bytes pushed with OP_PUSHDATA1 counts d + 5 bytes:
        // 78 is exactly 83, the default limit, and 79 is one past it.
        tapscript("tapscript-envelope-78", envelope(78)),
        tapscript("tapscript-envelope-79", envelope(79)),
        tapscript("tapscript-witness-1650", nop_leaf(1650)),
        tapscript("tapscript-witness-1651", nop_leaf(1651)),
    ]
}

struct Funds {
    big: (OutPoint, TxOut),
    small: (OutPoint, TxOut),
    crafted: Vec<Crafted>,
}

/// Mine spendable coins and one funding transaction that creates every
/// crafted coin, a small wallet coin and a large wallet coin.
fn fund(node: &Node) -> Funds {
    node.call("createwallet", &[json!("w")]);
    let mine_spk = node.new_spk();
    let mine_addr = Address::from_script(&mine_spk, Network::Regtest).expect("address");
    node.call(
        "generatetoaddress",
        &[json!(101), json!(mine_addr.to_string())],
    );
    let unspent = node.wallet_call("listunspent", &[json!(100)]);
    let coinbase = unspent
        .as_array()
        .and_then(|a| a.iter().find(|u| u["amount"] == json!(50.0)))
        .expect("mature coinbase");
    let coinbase_out = OutPoint {
        txid: coinbase["txid"]
            .as_str()
            .expect("txid")
            .parse()
            .expect("txid"),
        vout: coinbase["vout"].as_u64().expect("vout") as u32,
    };

    let scripts = crafted_scripts();
    let mut outputs: Vec<TxOut> = scripts
        .iter()
        .map(|(_, spk, _)| TxOut {
            value: Amount::from_sat(CRAFTED_VALUE),
            script_pubkey: spk.clone(),
        })
        .collect();
    let small_spk = node.new_spk();
    let big_spk = node.new_spk();
    let used = CRAFTED_VALUE * scripts.len() as u64 + SMALL_VALUE + FEE;
    outputs.push(TxOut {
        value: Amount::from_sat(SMALL_VALUE),
        script_pubkey: small_spk,
    });
    outputs.push(TxOut {
        value: Amount::from_sat(50 * 100_000_000 - used),
        script_pubkey: big_spk,
    });
    let funding = node.sign(&unsigned(vec![coinbase_out], outputs, LockTime::ZERO));
    node.call("sendrawtransaction", &[json!(serialize_hex(&funding))]);
    node.call(
        "generatetoaddress",
        &[json!(1), json!(mine_addr.to_string())],
    );

    let txid: Txid = funding.compute_txid();
    let at = |vout: usize| {
        (
            OutPoint {
                txid,
                vout: vout as u32,
            },
            funding.output[vout].clone(),
        )
    };
    let n = scripts.len();
    Funds {
        big: at(n + 1),
        small: at(n),
        crafted: scripts
            .into_iter()
            .enumerate()
            .map(|(i, (name, _, witness))| {
                let (outpoint, txout) = at(i);
                Crafted {
                    name,
                    outpoint,
                    txout,
                    witness,
                }
            })
            .collect(),
    }
}

fn unsigned(inputs: Vec<OutPoint>, outputs: Vec<TxOut>, lock_time: LockTime) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time,
        input: inputs
            .into_iter()
            .map(|previous_output| TxIn {
                previous_output,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output: outputs,
    }
}

fn vectors(node: &Node, funds: &Funds) -> Vec<Vector> {
    let change_spk = node.new_spk();
    // Spend the large wallet coin to `extra` outputs plus change.
    let wallet_spend = |name, extra: Vec<TxOut>, lock_time: u32| {
        let paid: u64 = extra.iter().map(|o| o.value.to_sat()).sum();
        let mut outputs = extra;
        outputs.push(TxOut {
            value: Amount::from_sat(funds.big.1.value.to_sat() - paid - FEE),
            script_pubkey: change_spk.clone(),
        });
        let tx = node.sign(&unsigned(
            vec![funds.big.0],
            outputs,
            LockTime::from_consensus(lock_time),
        ));
        Vector {
            name,
            tx,
            spent: vec![funds.big.1.clone()],
        }
    };
    let data = |spk: ScriptBuf| TxOut {
        value: Amount::ZERO,
        script_pubkey: spk,
    };
    let paying = |spk: ScriptBuf, sats: u64| TxOut {
        value: Amount::from_sat(sats),
        script_pubkey: spk,
    };

    let mut runes = vec![OP_RETURN, OP_13];
    push(&mut runes, &[0x14, 0x01, 0x02, 0x03]);
    let mut p2pk = Vec::new();
    push(&mut p2pk, &pubkey(2).serialize());
    p2pk.push(OP_CHECKSIG);
    let mut multisig = vec![OP_1];
    push(&mut multisig, &pubkey(3).serialize());
    push(&mut multisig, &pubkey(4).serialize());
    multisig.extend([OP_1 + 1, OP_CHECKMULTISIG]);
    // Olga: a P2WSH output whose "hash" starts with a payload length and
    // "stamp:".
    let mut olga = vec![OP_0, 0x20, 0x00, 0x0a];
    olga.extend_from_slice(b"stamp:");
    olga.resize(34, 0x00);

    let mut v = vec![
        wallet_spend("plain-payment", vec![], 0),
        wallet_spend("op-return-42", vec![data(op_return(42))], 0),
        wallet_spend("op-return-43", vec![data(op_return(43))], 0),
        wallet_spend("op-return-83", vec![data(op_return(83))], 0),
        wallet_spend("op-return-84", vec![data(op_return(84))], 0),
        wallet_spend("op-return-1650", vec![data(op_return(1650))], 0),
        wallet_spend("op-return-1651", vec![data(op_return(1651))], 0),
        wallet_spend(
            "two-op-returns",
            vec![data(op_return(10)), data(op_return(10))],
            0,
        ),
        wallet_spend("runes", vec![data(ScriptBuf::from_bytes(runes))], 0),
        wallet_spend(
            "bare-pubkey",
            vec![paying(ScriptBuf::from_bytes(p2pk), 10_000)],
            0,
        ),
        wallet_spend(
            "bare-multisig-1-of-2",
            vec![paying(ScriptBuf::from_bytes(multisig), 10_000)],
            0,
        ),
        wallet_spend(
            "olga-stamp",
            vec![paying(ScriptBuf::from_bytes(olga), 10_000)],
            0,
        ),
        wallet_spend("cat21-locktime-21", vec![], 21),
        wallet_spend("locktime-22", vec![], 22),
    ];

    // Only an OP_RETURN output: the whole small coin is the fee.
    let bare = node.sign(&unsigned(
        vec![funds.small.0],
        vec![data(op_return(20))],
        LockTime::ZERO,
    ));
    v.push(Vector {
        name: "bare-datacarrier",
        tx: bare,
        spent: vec![funds.small.1.clone()],
    });

    for c in &funds.crafted {
        let mut tx = unsigned(
            vec![c.outpoint],
            vec![paying(change_spk.clone(), CRAFTED_VALUE - FEE)],
            LockTime::ZERO,
        );
        tx.input[0].witness = c.witness.clone();
        v.push(Vector {
            name: c.name,
            tx,
            spent: vec![c.txout.clone()],
        });
    }
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// The test
// ─────────────────────────────────────────────────────────────────────────────

struct Config {
    name: &'static str,
    args: &'static [&'static str],
    rules: Rules,
}

fn configs() -> Vec<Config> {
    vec![
        Config {
            name: "defaults",
            args: &[],
            rules: Rules::default(),
        },
        Config {
            name: "deployed",
            args: &["-datacarriersize=42", "-rejecttokens=1"],
            rules: Rules {
                datacarrier_size: 42,
                reject_tokens: true,
                ..Rules::default()
            },
        },
        Config {
            name: "permissive",
            args: &[
                "-acceptnonstddatacarrier=1",
                "-permitbaremultisig=1",
                "-permitbarepubkey=1",
                "-permitbaredatacarrier=1",
                "-rejectparasites=0",
            ],
            rules: Rules {
                accept_non_std_datacarrier: true,
                permit_bare_multisig: true,
                permit_bare_pubkey: true,
                permit_bare_datacarrier: true,
                reject_parasites: false,
                ..Rules::default()
            },
        },
    ]
}

fn label(v: &Result<(), Rule>) -> String {
    match v {
        Ok(()) => "accept".into(),
        Err(rule) => rule.reason().into(),
    }
}

#[test]
#[ignore = "needs a Knots or Roots bitcoind; run with: cargo test --release --test policy_differential -- --ignored --nocapture"]
fn classifier_matches_node_policy() {
    let version = node_version();
    let supported = version
        .as_deref()
        .is_some_and(|v| v.contains("Knots") || v.contains("Roots"));
    if !supported {
        let msg = format!(
            "`{}` is not a Knots or Roots node ({})",
            bitcoind_bin(),
            version.as_deref().unwrap_or("does not run")
        );
        if std::env::var_os("CI").is_some() {
            panic!("{msg}");
        }
        eprintln!("SKIP: {msg}");
        return;
    }
    let version = version.unwrap_or_default();
    eprintln!("node: {version}");

    let root = std::env::temp_dir().join(format!("policy-differential-{}", std::process::id()));
    let mut failures = Vec::new();
    let mut node_rejected = BTreeSet::new();
    for config in configs() {
        let datadir: PathBuf = root.join(config.name);
        let node = Node::start(&datadir, config.args);
        let funds = fund(&node);
        eprintln!("\n[{}] {}", config.name, config.args.join(" "));
        eprintln!(
            "  {:<24} {:<30} {:<30} {:<30}",
            "vector", "node", "exact", "shape"
        );
        for v in vectors(&node, &funds) {
            let node_verdict = node.verdict(&v.tx);
            let exact = check(&v.tx, &config.rules, Some(&v.spent));
            let shape = check(&v.tx, &config.rules, None);
            let node_rule = match &node_verdict {
                Ok(()) => Ok(()),
                Err(reason) => match rule_for_reason(reason, &exact) {
                    Some(rule) => {
                        node_rejected.insert(rule);
                        Err(rule)
                    }
                    None => {
                        failures.push(format!(
                            "[{}] {}: node rejected with `{reason}`, which no rule covers \
                             (vector not otherwise standard, or a rule the classifier lacks)",
                            config.name, v.name
                        ));
                        continue;
                    }
                },
            };
            let node_label = node_verdict.clone().err().unwrap_or("accept".into());
            let mark = if exact == node_rule && shape == node_rule {
                ""
            } else {
                "  <-- MISMATCH"
            };
            eprintln!(
                "  {:<24} {:<30} {:<30} {:<30}{mark}",
                v.name,
                node_label,
                label(&exact),
                label(&shape)
            );
            if !mark.is_empty() {
                failures.push(format!(
                    "[{}] {}: node {node_label}, exact {}, shape {}",
                    config.name,
                    v.name,
                    label(&exact),
                    label(&shape)
                ));
            }
        }
        drop(node);
        let _ = std::fs::remove_dir_all(&datadir);
    }
    let _ = std::fs::remove_dir_all(&root);

    let missing: Vec<&str> = COVERED
        .iter()
        .filter(|r| !node_rejected.contains(r))
        .map(|r| r.reason())
        .collect();
    if !missing.is_empty() {
        failures.push(format!(
            "no vector made the node reject under: {}",
            missing.join(", ")
        ));
    }
    assert!(
        failures.is_empty(),
        "{version}: {} disagreement(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
