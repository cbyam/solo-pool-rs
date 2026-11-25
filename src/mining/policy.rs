//! mining/policy.rs
//!
//! Template policy, observe mode (experimental, `[template_policy]`).
//!
//! The observer checks every transaction of a block template against the
//! Knots-style rules in the `tx-datacarrier-policy` crate and records what
//! the rules would decline: per-rule Prometheus counters, a log line, and a
//! snapshot for `/stats`. It changes nothing. The job miners receive and the
//! block a share would assemble are built from the node's template exactly
//! as with the policy off, and the engine runs the observer after the job
//! has been broadcast, off its own task.
//!
//! Design and rollout: `docs/design/template-policy.md`.

use std::collections::{BTreeMap, HashSet};

use bitcoin::{consensus::deserialize, Transaction, Txid};
use serde::Serialize;
use tracing::{debug, info, warn};
use tx_datacarrier_policy::{check, Rule, Rules};

use crate::{bitcoin::rpc::GbtResult, metrics};

/// Label for a transaction that passes every rule itself but spends an output
/// of a declined transaction in the same template. Leaving the parent out of
/// a block leaves the child out too.
pub const DESCENDANT: &str = "descendant";

/// Every value the `rule` metric label can take: each rule's Knots reject
/// reason, then `descendant`.
pub fn rule_labels() -> impl Iterator<Item = &'static str> {
    Rule::ALL
        .into_iter()
        .map(Rule::reason)
        .chain(std::iter::once(DESCENDANT))
}

/// Transactions, weight and fees, summed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub txs: u64,
    pub weight: u64,
    pub fees_sats: u64,
}

impl Tally {
    fn add(&mut self, weight: u64, fee: u64) {
        self.txs += 1;
        self.weight += weight;
        self.fees_sats += fee;
    }
}

/// One declined transaction of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declined {
    txid: Txid,
    rule: &'static str,
    weight: u64,
    fee: u64,
}

/// One rule's share of the current template's declines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleTally {
    pub rule: &'static str,
    #[serde(flatten)]
    pub tally: Tally,
}

/// What the policy makes of the template being mined, for `/stats`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicySnapshot {
    pub mode: &'static str,
    /// Height of the template these figures describe.
    pub height: u64,
    /// The template's transactions, weight and fees, coinbase excluded.
    pub template: Tally,
    /// What the rules would decline from it.
    pub declined: Tally,
    /// The same, split by rule. Each transaction is counted once, under the
    /// first rule it breaks; rules that declined nothing are left out.
    pub declined_by_rule: Vec<RuleTally>,
    /// Distinct declined transactions seen since the pool started.
    pub declined_since_start: Tally,
    /// The `[template_policy]` rule keys set away from their Knots default,
    /// as `key = value`. Empty means the rules are stock Knots 29.3.
    pub non_default_rules: Vec<String>,
}

#[derive(Default)]
struct State {
    /// The tip the `counted` set belongs to.
    prev_hash: String,
    /// Declined transactions already added to the counters. A transaction
    /// sits in every template until it is mined, so without this each one
    /// would be counted again on every 30-second refresh.
    counted: HashSet<Txid>,
    since_start: Tally,
    latest: Option<PolicySnapshot>,
}

pub struct TemplateObserver {
    rules: Rules,
    non_default_rules: Vec<String>,
    state: parking_lot::Mutex<State>,
}

/// The rule keys of `rules` that differ from Knots' defaults, as
/// `key = value` with the `[template_policy]` key names.
pub fn non_default_rules(rules: &Rules) -> Vec<String> {
    let d = Rules::default();
    let mut out = Vec::new();
    macro_rules! diff {
        ($($field:ident),* $(,)?) => {
            $(if rules.$field != d.$field {
                out.push(format!(concat!(stringify!($field), " = {}"), rules.$field));
            })*
        };
    }
    // The keys the [template_policy] section exposes, in its order.
    diff!(
        datacarrier_size,
        permit_multiple_op_return,
        accept_non_std_datacarrier,
        datacarrier_full_count,
        max_script_size,
        reject_tokens,
        reject_parasites,
        permit_bare_multisig,
        permit_bare_pubkey,
        permit_bare_datacarrier,
    );
    out
}

impl TemplateObserver {
    pub fn new(rules: Rules) -> Self {
        Self {
            non_default_rules: non_default_rules(&rules),
            rules,
            state: parking_lot::Mutex::new(State::default()),
        }
    }

    /// The latest template's figures; None before the first template.
    pub fn snapshot(&self) -> Option<PolicySnapshot> {
        self.state.lock().latest.clone()
    }

    /// Check one template and record the result. Blocking (CPU only): the
    /// engine calls it from the blocking pool.
    pub fn observe(&self, gbt: &GbtResult) {
        let (template, declined) = classify(gbt, &self.rules);

        let mut by_rule: BTreeMap<&'static str, Tally> = BTreeMap::new();
        let mut total = Tally::default();
        for d in &declined {
            by_rule.entry(d.rule).or_default().add(d.weight, d.fee);
            total.add(d.weight, d.fee);
        }

        // The lock is held to the end so two templates cannot interleave
        // their counter updates.
        let mut state = self.state.lock();
        if state.prev_hash != gbt.prev_hash {
            // New tip. Mined transactions are gone for good. The ones still
            // waiting are in this template's declined list, so keeping only
            // those bounds the set without counting them twice.
            let still: HashSet<Txid> = declined.iter().map(|d| d.txid).collect();
            state.counted.retain(|txid| still.contains(txid));
            state.prev_hash = gbt.prev_hash.clone();
        }

        let mut new = Tally::default();
        for d in &declined {
            if state.counted.insert(d.txid) {
                metrics::template_declined(d.rule, d.weight, d.fee);
                state.since_start.add(d.weight, d.fee);
                new.add(d.weight, d.fee);
                debug!(txid = %d.txid, rule = d.rule, weight = d.weight, fee_sats = d.fee,
                    "Template policy (observe): transaction would be declined");
            }
        }
        metrics::template_policy_current(total.txs, total.weight, total.fees_sats);

        if new.txs > 0 {
            let rules = by_rule
                .iter()
                .map(|(rule, t)| format!("{rule}={}", t.txs))
                .collect::<Vec<_>>()
                .join(" ");
            info!(
                height = gbt.height,
                new_txs = new.txs,
                declined_txs = total.txs,
                declined_weight = total.weight,
                declined_fees_sats = total.fees_sats,
                template_txs = template.txs,
                rules = %rules,
                "Template policy (observe): template holds transactions the rules \
                 would decline; template unchanged"
            );
        }

        state.latest = Some(PolicySnapshot {
            mode: "observe",
            height: gbt.height,
            template,
            declined: total,
            declined_by_rule: by_rule
                .into_iter()
                .map(|(rule, tally)| RuleTally { rule, tally })
                .collect(),
            declined_since_start: state.since_start,
            non_default_rules: self.non_default_rules.clone(),
        });
    }
}

/// Check every transaction of a template. Returns the template's own totals
/// and the transactions the rules decline, in template order.
///
/// The spent coins are not known here, so the classifier infers each input's
/// executed script from the witness shape (see the crate documentation and
/// the audit results in the design doc).
fn classify(gbt: &GbtResult, rules: &Rules) -> (Tally, Vec<Declined>) {
    let mut template = Tally::default();
    let mut declined = Vec::new();
    let mut declined_ids: HashSet<Txid> = HashSet::new();

    for entry in &gbt.transactions {
        template.add(entry.weight, entry.fee);
        let tx: Transaction = match deserialize(&entry.data) {
            Ok(tx) => tx,
            Err(e) => {
                // The node built this template, so this should not happen.
                // The job is built from the raw bytes and is unaffected.
                warn!(txid = %entry.txid, "Template policy: cannot decode a template transaction: {e}");
                continue;
            }
        };
        // A template lists parents before children, so one pass finds every
        // descendant, however deep the chain.
        let rule = match check(&tx, rules, None) {
            Err(rule) => rule.reason(),
            Ok(())
                if tx
                    .input
                    .iter()
                    .any(|i| declined_ids.contains(&i.previous_output.txid)) =>
            {
                DESCENDANT
            }
            Ok(()) => continue,
        };
        let txid = tx.compute_txid();
        declined_ids.insert(txid);
        declined.push(Declined {
            txid,
            rule,
            weight: entry.weight,
            fee: entry.fee,
        });
    }
    (template, declined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitcoin::rpc::GbtTransaction;
    use bitcoin::{
        absolute::LockTime, consensus::serialize, hashes::Hash, script::PushBytesBuf,
        transaction::Version, Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness,
    };

    /// A P2WPKH-shaped spend of `prev`, paying one P2WPKH output and any
    /// extra outputs given.
    fn tx_spending(prev: OutPoint, extra_outputs: Vec<TxOut>) -> Transaction {
        let mut output = vec![TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::from_bytes([vec![0x00, 0x14], vec![0x11; 20]].concat()),
        }];
        output.extend(extra_outputs);
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[vec![0x30; 71], vec![0x02; 33]]),
            }],
            output,
        }
    }

    fn outpoint(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout: 0,
        }
    }

    fn op_return(len: usize) -> TxOut {
        let data = PushBytesBuf::try_from(vec![0xAB; len]).unwrap();
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(data),
        }
    }

    fn entry(tx: &Transaction, fee: u64) -> GbtTransaction {
        GbtTransaction {
            data: serialize(tx),
            txid: tx.compute_txid().to_string(),
            hash: tx.compute_wtxid().to_string(),
            fee,
            weight: tx.weight().to_wu(),
        }
    }

    fn template(prev_hash: &str, txs: &[(&Transaction, u64)]) -> GbtResult {
        GbtResult {
            version: 0x2000_0000,
            prev_hash: prev_hash.to_string(),
            bits: "207fffff".into(),
            cur_time: 0,
            min_time: 0,
            height: 102,
            coinbase_value: 0,
            transactions: txs.iter().map(|(tx, fee)| entry(tx, *fee)).collect(),
            longpoll_id: None,
            default_witness_commitment: None,
            rules: vec![],
        }
    }

    /// A plain payment, a 200-byte OP_RETURN, a child of that, and a
    /// grandchild: the template every test below starts from.
    fn sample() -> (Transaction, Transaction, Transaction, Transaction) {
        let plain = tx_spending(outpoint(1), vec![]);
        let big = tx_spending(outpoint(2), vec![op_return(200)]);
        let child = tx_spending(
            OutPoint {
                txid: big.compute_txid(),
                vout: 0,
            },
            vec![],
        );
        let grandchild = tx_spending(
            OutPoint {
                txid: child.compute_txid(),
                vout: 0,
            },
            vec![],
        );
        (plain, big, child, grandchild)
    }

    #[test]
    fn declines_the_rule_breaker_and_its_descendants_only() {
        let (plain, big, child, grandchild) = sample();
        let gbt = template(
            "aa",
            &[(&plain, 100), (&big, 700), (&child, 30), (&grandchild, 5)],
        );
        let (all, declined) = classify(&gbt, &Rules::default());

        assert_eq!(all.txs, 4);
        assert_eq!(all.fees_sats, 835);
        let got: Vec<_> = declined.iter().map(|d| (d.txid, d.rule, d.fee)).collect();
        assert_eq!(
            got,
            vec![
                (big.compute_txid(), Rule::Datacarrier.reason(), 700),
                (child.compute_txid(), DESCENDANT, 30),
                (grandchild.compute_txid(), DESCENDANT, 5),
            ]
        );
    }

    #[test]
    fn a_clean_template_declines_nothing() {
        let (plain, ..) = sample();
        let small = tx_spending(outpoint(3), vec![op_return(40)]);
        let gbt = template("aa", &[(&plain, 100), (&small, 100)]);
        let (all, declined) = classify(&gbt, &Rules::default());
        assert_eq!(all.txs, 2);
        assert!(declined.is_empty());
    }

    #[test]
    fn the_configured_rules_are_the_ones_applied() {
        // 60 bytes of data passes Knots' default 83 and fails the deployed
        // node's 42.
        let tx = tx_spending(outpoint(4), vec![op_return(60)]);
        let gbt = template("aa", &[(&tx, 100)]);
        assert!(classify(&gbt, &Rules::default()).1.is_empty());
        let mut strict = Rules::default();
        strict.datacarrier_size = 42;
        assert_eq!(classify(&gbt, &strict).1.len(), 1);
    }

    #[test]
    fn a_transaction_is_counted_once_however_many_templates_hold_it() {
        let (plain, big, child, _) = sample();
        let observer = TemplateObserver::new(Rules::default());
        assert!(observer.snapshot().is_none());

        // The same template three times: two ntime refreshes on one tip.
        let gbt = template("aa", &[(&plain, 100), (&big, 700), (&child, 30)]);
        for _ in 0..3 {
            observer.observe(&gbt);
        }
        let snap = observer.snapshot().unwrap();
        assert_eq!(snap.mode, "observe");
        assert_eq!(snap.height, 102);
        assert_eq!(snap.template.txs, 3);
        assert_eq!(snap.declined.txs, 2);
        assert_eq!(snap.declined.fees_sats, 730);
        assert_eq!(snap.declined_since_start.txs, 2);
        assert_eq!(
            snap.declined_by_rule
                .iter()
                .map(|r| (r.rule, r.tally.txs))
                .collect::<Vec<_>>(),
            vec![(Rule::Datacarrier.reason(), 1), (DESCENDANT, 1)]
        );

        // A new tip that did not mine them: still in the template, still
        // counted once.
        let gbt = template("bb", &[(&plain, 100), (&big, 700), (&child, 30)]);
        observer.observe(&gbt);
        assert_eq!(observer.snapshot().unwrap().declined_since_start.txs, 2);

        // A different declined transaction arrives: one more.
        let other = tx_spending(outpoint(9), vec![op_return(300)]);
        let gbt = template("bb", &[(&plain, 100), (&big, 700), (&other, 50)]);
        observer.observe(&gbt);
        let snap = observer.snapshot().unwrap();
        assert_eq!(snap.declined.txs, 2, "current template: big and other");
        assert_eq!(snap.declined_since_start.txs, 3);
        assert_eq!(snap.declined_since_start.fees_sats, 780);

        // Everything mined: the template is clean and the totals stay.
        let gbt = template("cc", &[(&plain, 100)]);
        observer.observe(&gbt);
        let snap = observer.snapshot().unwrap();
        assert_eq!(snap.declined, Tally::default());
        assert!(snap.declined_by_rule.is_empty());
        assert_eq!(snap.declined_since_start.txs, 3);
    }

    #[test]
    fn an_undecodable_transaction_is_skipped_not_fatal() {
        let (plain, big, ..) = sample();
        let mut gbt = template("aa", &[(&plain, 100), (&big, 700)]);
        gbt.transactions[0].data = vec![0xFF, 0x00];
        let (all, declined) = classify(&gbt, &Rules::default());
        assert_eq!(all.txs, 2);
        assert_eq!(declined.len(), 1);
    }

    #[test]
    fn snapshot_json_shape() {
        let (plain, big, ..) = sample();
        let observer = TemplateObserver::new(Rules::default());
        observer.observe(&template("aa", &[(&plain, 100), (&big, 700)]));
        let json = serde_json::to_value(observer.snapshot().unwrap()).unwrap();
        assert_eq!(json["mode"], "observe");
        assert_eq!(json["template"]["txs"], 2);
        assert_eq!(json["declined"]["fees_sats"], 700);
        assert_eq!(json["declined_by_rule"][0]["rule"], "datacarrier");
        assert_eq!(json["declined_by_rule"][0]["txs"], 1);
        assert_eq!(json["declined_since_start"]["txs"], 1);
        assert_eq!(json["non_default_rules"], serde_json::json!([]));
    }

    #[test]
    fn non_default_rules_lists_only_the_keys_changed_from_knots() {
        assert!(non_default_rules(&Rules::default()).is_empty());

        // The deployed node's settings.
        let mut deployed = Rules::default();
        deployed.datacarrier_size = 42;
        deployed.reject_tokens = true;
        assert_eq!(
            non_default_rules(&deployed),
            ["datacarrier_size = 42", "reject_tokens = true"]
        );

        // Every key the config section exposes is reported when changed.
        let d = Rules::default();
        let mut all = d.clone();
        all.datacarrier_size += 1;
        all.permit_multiple_op_return = !d.permit_multiple_op_return;
        all.accept_non_std_datacarrier = !d.accept_non_std_datacarrier;
        all.datacarrier_full_count = !d.datacarrier_full_count;
        all.max_script_size += 1;
        all.reject_tokens = !d.reject_tokens;
        all.reject_parasites = !d.reject_parasites;
        all.permit_bare_multisig = !d.permit_bare_multisig;
        all.permit_bare_pubkey = !d.permit_bare_pubkey;
        all.permit_bare_datacarrier = !d.permit_bare_datacarrier;
        assert_eq!(non_default_rules(&all).len(), 10);

        // And it reaches the snapshot.
        let observer = TemplateObserver::new(deployed);
        observer.observe(&template("aa", &[]));
        assert_eq!(observer.snapshot().unwrap().non_default_rules.len(), 2);
    }
}
