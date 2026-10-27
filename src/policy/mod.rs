//! Transaction policy the pool can apply to a block template.
//!
//! The rules are the ones Bitcoin Knots enforces and Bitcoin Core v31.1 does
//! not, so a Core node plus these rules gives Knots-like blocks. Each rule keeps
//! the meaning and the default of its Knots option. The logic is a port of
//! Bitcoin Roots' `src/policy/policy.cpp` and the data-carrier checks in its
//! `src/validation.cpp` (MIT licensed, Copyright The Bitcoin Core developers),
//! which reimplement the Knots behaviour on a Core v31.1 base. Checks that Core
//! already applies (non-standard scripts, dust, the 80-byte stack item limits)
//! are left to the node. Design: `docs/design/template-policy.md`.
//!
//! The embedded-data checks need the script each input executes. With the
//! spent coins (`spent = Some(..)`) it is found exactly, as Knots does; without
//! them it is inferred from the witness shape, which can differ on unusual
//! inputs.

mod inputs;
mod script;
#[cfg(test)]
mod tests;

use bitcoin::{Transaction, TxOut};

use script::{
    datacarrier_bytes, get_op, is_p2sh, is_p2wsh, is_push_only, olga_bytes, push_only_stack,
    witness_program, MAX_SCRIPT_SIZE, OP_0, OP_1, OP_13, OP_16, OP_CHECKMULTISIG, OP_CHECKSIG,
    OP_INVALIDOPCODE, OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4, OP_RETURN,
};

/// Knots' `PARASITE_CAT21_LOCKTIME`.
const CAT21_LOCKTIME: u32 = 21;
/// Core's `MAX_STANDARD_SCRIPTSIG_SIZE`.
const MAX_STANDARD_SCRIPTSIG_SIZE: usize = 1650;
/// Knots' default `-dustrelayfee`, in sat/kvB.
const DUST_RELAY_FEE: u64 = 3000;
/// Core's `MAX_PUBKEYS_PER_MULTISIG`.
const MAX_PUBKEYS_PER_MULTISIG: i64 = 20;

/// The policy settings. Field defaults are Knots 29.3's (`bitcoind -help`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// Knots `-datacarriersize`: the most OP_RETURN script bytes a transaction
    /// may carry, summed over its outputs. Knots' `-datacarrier=0` is 0 here.
    pub datacarrier_size: usize,
    /// Allow more than one OP_RETURN output. Knots has no option and always
    /// rejects; Core allows it.
    pub permit_multiple_op_return: bool,
    /// Knots `-acceptnonstddatacarrier`: allow data embedded in executed
    /// scripts (envelopes, push/`OP_DROP`, Olga, OPNet).
    pub accept_non_std_datacarrier: bool,
    /// Knots `-datacarrierfullcount`: count embedded data toward
    /// `datacarrier_size` together with OP_RETURN data.
    pub datacarrier_full_count: bool,
    /// Knots `-maxscriptsize`: the largest witness stack (serialised), output
    /// script, scriptSig or redeem script allowed.
    pub max_script_size: usize,
    /// Knots `-rejecttokens`: reject Runes and Olga.
    pub reject_tokens: bool,
    /// Knots `-rejectparasites`: reject CAT-21.
    pub reject_parasites: bool,
    /// Knots `-permitbaremultisig`.
    pub permit_bare_multisig: bool,
    /// Knots `-permitbarepubkey`.
    pub permit_bare_pubkey: bool,
    /// Knots `-permitbaredatacarrier`: allow transactions whose only outputs
    /// are data carriers or dust.
    pub permit_bare_datacarrier: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            datacarrier_size: 83,
            permit_multiple_op_return: false,
            accept_non_std_datacarrier: false,
            datacarrier_full_count: true,
            max_script_size: 1650,
            reject_tokens: false,
            reject_parasites: true,
            permit_bare_multisig: false,
            permit_bare_pubkey: false,
            permit_bare_datacarrier: false,
        }
    }
}

/// Why a transaction is declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    Cat21,
    ScriptSigSize,
    ScriptPubKeySize,
    Datacarrier,
    Runes,
    BarePubkey,
    BareMultisig,
    Olga,
    MultiOpReturn,
    BareDatacarrier,
    DatacarrierNonStandard,
    DatacarrierExceeded,
    SpentScriptSize,
    RedeemScriptSize,
    WitnessSize,
}

impl Rule {
    /// Knots' reject reason. Knots prefixes the input checks with
    /// `bad-txns-input-` and the witness check with `bad-witness-`.
    pub fn reason(self) -> &'static str {
        match self {
            Rule::Cat21 => "parasite-cat21",
            Rule::ScriptSigSize => "scriptsig-size",
            Rule::ScriptPubKeySize => "scriptpubkey-size",
            Rule::Datacarrier => "datacarrier",
            Rule::Runes => "tokens-runes",
            Rule::BarePubkey => "bare-pubkey",
            Rule::BareMultisig => "bare-multisig",
            Rule::Olga => "tokens-olga",
            Rule::MultiOpReturn => "multi-op-return",
            Rule::BareDatacarrier => "bare-datacarrier",
            Rule::DatacarrierNonStandard => "txn-datacarrier-nonstandard",
            Rule::DatacarrierExceeded => "txn-datacarrier-exceeded",
            Rule::SpentScriptSize => "script-size",
            Rule::RedeemScriptSize => "scriptcheck-size",
            Rule::WitnessSize => "witness-size",
        }
    }

    /// The `[template_policy]` key that controls this rule.
    pub fn key(self) -> &'static str {
        match self {
            Rule::Cat21 => "reject_parasites",
            Rule::Runes | Rule::Olga => "reject_tokens",
            Rule::BarePubkey => "permit_bare_pubkey",
            Rule::BareMultisig => "permit_bare_multisig",
            Rule::MultiOpReturn => "permit_multiple_op_return",
            Rule::BareDatacarrier => "permit_bare_datacarrier",
            Rule::Datacarrier => "datacarrier_size",
            Rule::DatacarrierNonStandard => "accept_non_std_datacarrier",
            Rule::DatacarrierExceeded => "datacarrier_full_count",
            Rule::ScriptSigSize
            | Rule::ScriptPubKeySize
            | Rule::SpentScriptSize
            | Rule::RedeemScriptSize
            | Rule::WitnessSize => "max_script_size",
        }
    }
}

/// Check one transaction. `spent` holds the coins its inputs spend, in input
/// order, when they are known. Returns the first rule it breaks, in the order
/// Knots checks them.
pub fn check(tx: &Transaction, rules: &Rules, spent: Option<&[TxOut]>) -> Result<(), Rule> {
    if tx.is_coinbase() {
        return Ok(());
    }
    check_standard(tx, rules)?;
    check_datacarrier(tx, rules, spent)?;
    check_inputs(tx, rules, spent)?;
    check_witnesses(tx, rules, spent)
}

fn spent_coin(spent: Option<&[TxOut]>, index: usize) -> Option<&TxOut> {
    spent.and_then(|coins| coins.get(index))
}

/// The Knots additions to `IsStandardTx`.
pub(crate) fn check_standard(tx: &Transaction, rules: &Rules) -> Result<(), Rule> {
    if rules.reject_parasites && tx.lock_time.to_consensus_u32() == CAT21_LOCKTIME {
        return Err(Rule::Cat21);
    }

    let scriptsig_limit = MAX_STANDARD_SCRIPTSIG_SIZE.min(rules.max_script_size);
    if tx
        .input
        .iter()
        .any(|i| i.script_sig.len() > scriptsig_limit)
    {
        return Err(Rule::ScriptSigSize);
    }

    let mut datacarrier_left = rules.datacarrier_size;
    let mut data_outputs = 0usize;
    let mut monetary_outputs = 0usize;
    let outputs = tx.output.len();
    for (i, out) in tx.output.iter().enumerate().rev() {
        let spk = out.script_pubkey.as_bytes();
        if spk.len() > rules.max_script_size {
            return Err(Rule::ScriptPubKeySize);
        }
        let kind = output_kind(spk);
        if !is_dust(out) && kind != OutputKind::NullData {
            monetary_outputs += 1;
        }
        match kind {
            OutputKind::NullData => {
                if spk.len() > datacarrier_left {
                    return Err(Rule::Datacarrier);
                }
                datacarrier_left -= spk.len();
                if rules.reject_tokens && spk.len() > 2 && spk[1] == OP_13 {
                    return Err(Rule::Runes);
                }
                data_outputs += 1;
            }
            OutputKind::PubKey if !rules.permit_bare_pubkey => return Err(Rule::BarePubkey),
            OutputKind::Multisig if !rules.permit_bare_multisig => return Err(Rule::BareMultisig),
            OutputKind::P2wsh if rules.reject_tokens && olga_bytes(spk, outputs - i) != 0 => {
                return Err(Rule::Olga)
            }
            _ => {}
        }
    }

    if data_outputs > 1 && !rules.permit_multiple_op_return {
        return Err(Rule::MultiOpReturn);
    }
    if monetary_outputs == 0 && data_outputs > 0 && !rules.permit_bare_datacarrier {
        return Err(Rule::BareDatacarrier);
    }
    Ok(())
}

/// Knots' data-carrier check in `MemPoolAccept::PreChecks`.
pub(crate) fn check_datacarrier(
    tx: &Transaction,
    rules: &Rules,
    spent: Option<&[TxOut]>,
) -> Result<(), Rule> {
    if !rules.datacarrier_full_count && rules.accept_non_std_datacarrier {
        return Ok(());
    }
    let (mut standard, mut nonstandard) = (0usize, 0usize);
    for (i, txin) in tx.input.iter().enumerate() {
        let script = match spent_coin(spent, i) {
            Some(coin) => inputs::executed_script_exact(coin.script_pubkey.as_bytes(), txin),
            None => inputs::executed_script_by_shape(txin),
        };
        let stack = inputs::witness_stack(txin);
        let (s, n) = datacarrier_bytes(&script, 0, Some(&stack));
        standard = standard.wrapping_add(s);
        nonstandard = nonstandard.wrapping_add(n);
    }
    let outputs = tx.output.len();
    for (i, out) in tx.output.iter().enumerate().rev() {
        let (s, n) = datacarrier_bytes(out.script_pubkey.as_bytes(), outputs - i, None);
        standard = standard.wrapping_add(s);
        nonstandard = nonstandard.wrapping_add(n);
    }

    if nonstandard != 0 && !rules.accept_non_std_datacarrier {
        return Err(Rule::DatacarrierNonStandard);
    }
    if rules.datacarrier_full_count && standard.wrapping_add(nonstandard) > rules.datacarrier_size {
        return Err(Rule::DatacarrierExceeded);
    }
    Ok(())
}

/// The Knots additions to `AreInputsStandard` that need no sigop counting.
/// They read the spent coin, so they only run when it is known.
pub(crate) fn check_inputs(
    tx: &Transaction,
    rules: &Rules,
    spent: Option<&[TxOut]>,
) -> Result<(), Rule> {
    for (i, txin) in tx.input.iter().enumerate() {
        let Some(coin) = spent_coin(spent, i) else {
            continue;
        };
        let spk = coin.script_pubkey.as_bytes();
        if spk.len() > rules.max_script_size {
            return Err(Rule::SpentScriptSize);
        }
        if is_p2sh(spk) && is_push_only(txin.script_sig.as_bytes(), 0) {
            if let Some(redeem) =
                push_only_stack(txin.script_sig.as_bytes()).and_then(|mut s| s.pop())
            {
                if redeem.len() > rules.max_script_size {
                    return Err(Rule::RedeemScriptSize);
                }
            }
        }
    }
    Ok(())
}

/// The Knots addition to `IsWitnessStandard`: the whole witness stack against
/// `max_script_size`.
pub(crate) fn check_witnesses(
    tx: &Transaction,
    rules: &Rules,
    spent: Option<&[TxOut]>,
) -> Result<(), Rule> {
    for (i, txin) in tx.input.iter().enumerate() {
        if txin.witness.is_empty() {
            continue;
        }
        if let Some(coin) = spent_coin(spent, i) {
            // Knots checks only witness spends; a witness on anything else is
            // invalid and left to the node.
            let mut program = coin.script_pubkey.as_bytes().to_vec();
            if is_p2sh(&program) {
                match push_only_stack(txin.script_sig.as_bytes()).and_then(|mut s| s.pop()) {
                    Some(redeem) => program = redeem,
                    None => continue,
                }
            }
            if witness_program(&program).is_none() {
                continue;
            }
        }
        if witness_serialized_size(&inputs::witness_stack(txin)) > rules.max_script_size {
            return Err(Rule::WitnessSize);
        }
    }
    Ok(())
}

/// Core's `GetSerializeSize` of a witness stack.
fn witness_serialized_size(stack: &[&[u8]]) -> usize {
    compact_size_len(stack.len())
        + stack
            .iter()
            .map(|item| compact_size_len(item.len()) + item.len())
            .sum::<usize>()
}

fn compact_size_len(n: usize) -> usize {
    match n {
        0..=0xfc => 1,
        0xfd..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

/// The parts of Core's `Solver` these rules distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    NullData,
    PubKey,
    Multisig,
    P2wsh,
    Other,
}

fn output_kind(spk: &[u8]) -> OutputKind {
    if is_p2wsh(spk) {
        OutputKind::P2wsh
    } else if !spk.is_empty() && spk[0] == OP_RETURN && is_push_only(spk, 1) {
        OutputKind::NullData
    } else if is_pay_to_pubkey(spk) {
        OutputKind::PubKey
    } else if is_multisig(spk) {
        OutputKind::Multisig
    } else {
        OutputKind::Other
    }
}

/// Core's `CPubKey::ValidSize`.
fn valid_pubkey_size(key: &[u8]) -> bool {
    match key.first() {
        Some(0x02 | 0x03) => key.len() == 33,
        Some(0x04 | 0x06 | 0x07) => key.len() == 65,
        _ => false,
    }
}

/// Core's `MatchPayToPubkey`.
fn is_pay_to_pubkey(spk: &[u8]) -> bool {
    (spk.len() == 67 && spk[0] == 65 && spk[66] == OP_CHECKSIG && valid_pubkey_size(&spk[1..66]))
        || (spk.len() == 35
            && spk[0] == 33
            && spk[34] == OP_CHECKSIG
            && valid_pubkey_size(&spk[1..34]))
}

/// Core's `MatchMultisig`.
fn is_multisig(spk: &[u8]) -> bool {
    if spk.last() != Some(&OP_CHECKMULTISIG) {
        return false;
    }
    let mut pc = 0;
    let Some(first) = get_op(spk, &mut pc) else {
        return false;
    };
    let Some(required) = script_number(first.opcode, first.data, 1, MAX_PUBKEYS_PER_MULTISIG)
    else {
        return false;
    };
    let mut keys = 0i64;
    let (last_opcode, last_data) = loop {
        match get_op(spk, &mut pc) {
            Some(op) if valid_pubkey_size(op.data) => keys += 1,
            Some(op) => break (op.opcode, op.data),
            None => break (OP_INVALIDOPCODE, &[][..]),
        }
    };
    let Some(total) = script_number(last_opcode, last_data, required, MAX_PUBKEYS_PER_MULTISIG)
    else {
        return false;
    };
    keys == total && pc + 1 == spk.len()
}

/// Core's `GetScriptNumber`, used by `MatchMultisig`.
fn script_number(opcode: u8, data: &[u8], min: i64, max: i64) -> Option<i64> {
    let count = if (OP_1..=OP_16).contains(&opcode) {
        (opcode - (OP_1 - 1)) as i64
    } else if opcode > OP_0 && opcode <= OP_PUSHDATA4 {
        if !is_minimal_push(data, opcode) {
            return None;
        }
        minimal_script_num(data)?
    } else {
        return None;
    };
    (min..=max).contains(&count).then_some(count)
}

/// Core's `CheckMinimalPush`.
fn is_minimal_push(data: &[u8], opcode: u8) -> bool {
    match data.len() {
        0 => opcode == OP_0,
        1 if (1..=16).contains(&data[0]) || data[0] == 0x81 => false,
        n if n <= 75 => opcode as usize == n,
        n if n <= 255 => opcode == OP_PUSHDATA1,
        n if n <= 65535 => opcode == OP_PUSHDATA2,
        _ => true,
    }
}

/// Core's `CScriptNum(data, fRequireMinimal = true).getint()`.
fn minimal_script_num(data: &[u8]) -> Option<i64> {
    if data.len() > 4 {
        return None;
    }
    if let Some(&last) = data.last() {
        if last & 0x7f == 0 && (data.len() <= 1 || data[data.len() - 2] & 0x80 == 0) {
            return None;
        }
    }
    let mut value = 0i64;
    for (i, &b) in data.iter().enumerate() {
        value |= (b as i64) << (8 * i);
    }
    if let Some(&last) = data.last() {
        if last & 0x80 != 0 {
            return Some(-(value & !(0x80i64 << (8 * (data.len() - 1)))));
        }
    }
    Some(value)
}

/// Core's `IsDust` at Knots' default dust relay fee.
fn is_dust(out: &TxOut) -> bool {
    let spk = out.script_pubkey.as_bytes();
    if (!spk.is_empty() && spk[0] == OP_RETURN) || spk.len() > MAX_SCRIPT_SIZE {
        return false;
    }
    let mut size = 8 + compact_size_len(spk.len()) + spk.len();
    size += if witness_program(spk).is_some() {
        32 + 4 + 1 + 107 / 4 + 4
    } else {
        32 + 4 + 1 + 107 + 4
    };
    let threshold = (DUST_RELAY_FEE * size as u64).div_ceil(1000);
    out.value.to_sat() < threshold
}
