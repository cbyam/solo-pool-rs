//! Template policy audit: replay recent blocks through the policy rules and
//! report what each rule would have declined and what it would have cost.
//!
//!   cargo run --release --example template_audit -- \
//!       --url http://127.0.0.1:8332 --cookie ~/.bitcoin/.cookie --blocks 144
//!
//! Each block is read with `getblock <hash> 3`, which includes the coins every
//! input spends, so each transaction is checked twice: exactly (with the
//! coins, as Knots does) and by witness shape (without them, as the pool can
//! do cheaply). The report compares the two. A transaction spending an output
//! of a declined transaction in the same block is declined with it, as the
//! pool would decline it from a template.
//!
//! Rule settings default to Knots 29.3's. `--datacarrier-size <n>` and
//! `--reject-tokens` override the two settings operators most often change.
//! Read-only: the tool sends no transaction and changes nothing on the node.
//! Not part of the shipping pool.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use bitcoin::{consensus::deserialize, Amount, ScriptBuf, Transaction, TxOut, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde_json::{json, Value};
use solo_pool_rs::policy::{check, Rule, Rules};

/// Transactions where exact and shape mode disagree, by (exact, shape)
/// verdict: a count and a few sample txids.
type Disagreements = BTreeMap<(Option<Reason>, Option<Reason>), (u64, Vec<String>)>;

#[derive(Default, Clone, Copy)]
struct Tally {
    txs: u64,
    weight: u64,
    fees: u64,
}

impl Tally {
    fn add(&mut self, weight: u64, fees: u64) {
        self.txs += 1;
        self.weight += weight;
        self.fees += fees;
    }
}

/// What declined a transaction: its own rule, or a declined parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    Rule(Rule),
    Descendant,
}

impl Reason {
    fn label(self) -> &'static str {
        match self {
            Reason::Rule(rule) => rule.reason(),
            Reason::Descendant => "(child of a declined tx)",
        }
    }
}

#[derive(Default)]
struct Mode {
    by_reason: BTreeMap<Reason, Tally>,
    declined: Tally,
    /// Declined share of each block's weight, for the per-block spread.
    block_shares: Vec<f64>,
}

struct Args {
    url: String,
    cookie: PathBuf,
    blocks: u64,
    rules: Rules,
}

fn parse_args() -> Args {
    let mut args = Args {
        url: "http://127.0.0.1:8332".into(),
        cookie: PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".bitcoin/.cookie"),
        blocks: 24,
        rules: Rules::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--url" => args.url = value(),
            "--cookie" => args.cookie = PathBuf::from(value()),
            "--blocks" => args.blocks = value().parse().expect("--blocks takes a number"),
            "--datacarrier-size" => {
                args.rules.datacarrier_size =
                    value().parse().expect("--datacarrier-size takes a number")
            }
            "--reject-tokens" => args.rules.reject_tokens = true,
            other => panic!("unknown argument {other}"),
        }
    }
    args
}

fn sats(btc: &Value) -> u64 {
    btc.as_f64().map(|v| (v * 1e8).round() as u64).unwrap_or(0)
}

fn main() {
    let args = parse_args();
    let rpc = Client::new(&args.url, Auth::CookieFile(args.cookie.clone())).expect("RPC client");
    let tip = rpc.get_block_count().expect("getblockcount");
    let first = tip + 1 - args.blocks.min(tip + 1);

    let mut exact = Mode::default();
    let mut shape = Mode::default();
    let mut total = Tally::default();
    let mut disagreements = Disagreements::new();

    for height in first..=tip {
        let hash = rpc.get_block_hash(height).expect("getblockhash");
        let block: Value = rpc
            .call("getblock", &[json!(hash.to_string()), json!(3)])
            .expect("getblock verbosity 3");
        let txs = block["tx"].as_array().expect("block tx list");

        let mut declined_exact: HashSet<Txid> = HashSet::new();
        let mut declined_shape: HashSet<Txid> = HashSet::new();
        let (mut block_weight, mut exact_weight, mut shape_weight) = (0u64, 0u64, 0u64);

        // The coinbase is never subject to policy.
        for entry in txs.iter().skip(1) {
            let raw = hex::decode(entry["hex"].as_str().expect("tx hex")).expect("tx hex decode");
            let tx: Transaction = deserialize(&raw).expect("tx decode");
            let weight = entry["weight"]
                .as_u64()
                .unwrap_or_else(|| tx.weight().to_wu());
            let fee = sats(&entry["fee"]);
            total.add(weight, fee);
            block_weight += weight;

            let coins: Vec<TxOut> = entry["vin"]
                .as_array()
                .expect("vin list")
                .iter()
                .map(|vin| TxOut {
                    value: Amount::from_sat(sats(&vin["prevout"]["value"])),
                    script_pubkey: ScriptBuf::from_bytes(
                        hex::decode(vin["prevout"]["scriptPubKey"]["hex"].as_str().unwrap_or(""))
                            .expect("prevout script hex"),
                    ),
                })
                .collect();

            let has_declined_parent = |declined: &HashSet<Txid>| {
                tx.input
                    .iter()
                    .any(|i| declined.contains(&i.previous_output.txid))
            };
            let verdict = |result: Result<(), Rule>, declined: &HashSet<Txid>| match result {
                Err(rule) => Some(Reason::Rule(rule)),
                Ok(()) if has_declined_parent(declined) => Some(Reason::Descendant),
                Ok(()) => None,
            };
            let by_exact = verdict(check(&tx, &args.rules, Some(&coins)), &declined_exact);
            let by_shape = verdict(check(&tx, &args.rules, None), &declined_shape);

            let txid = tx.compute_txid();
            for (reason, mode, declined, share) in [
                (by_exact, &mut exact, &mut declined_exact, &mut exact_weight),
                (by_shape, &mut shape, &mut declined_shape, &mut shape_weight),
            ] {
                if let Some(reason) = reason {
                    mode.by_reason.entry(reason).or_default().add(weight, fee);
                    mode.declined.add(weight, fee);
                    declined.insert(txid);
                    *share += weight;
                }
            }
            if by_exact != by_shape {
                let slot = disagreements.entry((by_exact, by_shape)).or_default();
                slot.0 += 1;
                if slot.1.len() < 5 {
                    slot.1.push(txid.to_string());
                }
            }
        }

        if block_weight > 0 {
            exact
                .block_shares
                .push(exact_weight as f64 / block_weight as f64);
            shape
                .block_shares
                .push(shape_weight as f64 / block_weight as f64);
        }
        eprint!(
            "\rread block {height} ({}/{})",
            height - first + 1,
            tip - first + 1
        );
    }
    eprintln!();

    report(&args, first, tip, &total, &exact, &shape, &disagreements);
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn spread(shares: &[f64]) -> String {
    if shares.is_empty() {
        return "n/a".into();
    }
    let mut s = shares.to_vec();
    s.sort_by(f64::total_cmp);
    let at = |q: f64| 100.0 * s[((s.len() - 1) as f64 * q).round() as usize];
    format!(
        "min {:.1}%  median {:.1}%  max {:.1}%",
        at(0.0),
        at(0.5),
        at(1.0)
    )
}

fn report(
    args: &Args,
    first: u64,
    tip: u64,
    total: &Tally,
    exact: &Mode,
    shape: &Mode,
    disagreements: &Disagreements,
) {
    let r = &args.rules;
    println!(
        "Template policy audit, blocks {first}..={tip} ({} blocks)",
        tip - first + 1
    );
    println!(
        "Rules: datacarrier_size={} accept_non_std_datacarrier={} datacarrier_full_count={} \
         max_script_size={} reject_tokens={} reject_parasites={} permit_bare_multisig={} \
         permit_bare_pubkey={} permit_bare_datacarrier={} permit_multiple_op_return={}",
        r.datacarrier_size,
        r.accept_non_std_datacarrier,
        r.datacarrier_full_count,
        r.max_script_size,
        r.reject_tokens,
        r.reject_parasites,
        r.permit_bare_multisig,
        r.permit_bare_pubkey,
        r.permit_bare_datacarrier,
        r.permit_multiple_op_return
    );
    println!(
        "Transactions checked: {} ({} WU, {:.8} BTC in fees; coinbases excluded)\n",
        total.txs,
        total.weight,
        total.fees as f64 / 1e8
    );

    for (name, mode) in [
        ("Exact (spent coins known)", exact),
        ("Shape (no coin lookup)", shape),
    ] {
        println!("{name}");
        println!(
            "  {:<30} {:>8} {:>7} {:>12} {:>7} {:>14} {:>7}",
            "rule", "txs", "%txs", "weight", "%wt", "fees (sat)", "%fees"
        );
        for (reason, t) in &mode.by_reason {
            println!(
                "  {:<30} {:>8} {:>6.2}% {:>12} {:>6.2}% {:>14} {:>6.2}%",
                reason.label(),
                t.txs,
                pct(t.txs, total.txs),
                t.weight,
                pct(t.weight, total.weight),
                t.fees,
                pct(t.fees, total.fees)
            );
        }
        let d = &mode.declined;
        println!(
            "  {:<30} {:>8} {:>6.2}% {:>12} {:>6.2}% {:>14} {:>6.2}%",
            "declined, all rules",
            d.txs,
            pct(d.txs, total.txs),
            d.weight,
            pct(d.weight, total.weight),
            d.fees,
            pct(d.fees, total.fees)
        );
        println!(
            "  declined share of block weight: {}\n",
            spread(&mode.block_shares)
        );
    }

    let disagreeing: u64 = disagreements.values().map(|(n, _)| n).sum();
    println!("Exact and shape disagree on {disagreeing} transactions");
    let show = |r: &Option<Reason>| r.map_or("passes", |r| r.label());
    for ((by_exact, by_shape), (n, samples)) in disagreements {
        println!(
            "  exact {:<28} shape {:<28} {n}",
            show(by_exact),
            show(by_shape)
        );
        for txid in samples {
            println!("    {txid}");
        }
    }
    println!(
        "\nFees are what the declined transactions paid. With deprioritising the node \
         refills their space with the next-best transactions, so the net cost is lower."
    );
}
