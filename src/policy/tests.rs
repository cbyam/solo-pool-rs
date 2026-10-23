//! Tests for the template policy rules.
//!
//! Cases marked "Roots" are ported from Bitcoin Roots'
//! `src/test/transaction_tests.cpp` (`test_IsStandard`) and
//! `test/functional/mempool_datacarrier.py` (MIT licensed, Copyright The
//! Bitcoin Core developers), with Roots' expected outcomes. The rest cover
//! what the port adds: finding the executed script without the spent coin,
//! and making sure ordinary spends are never declined.

use bitcoin::hashes::Hash;
use bitcoin::{
    absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
};

use super::*;

const CENT: u64 = 1_000_000;
const MAX_MONEY: u64 = 21_000_000 * 100_000_000;

// ── builders ────────────────────────────────────────────────────────────────

/// A push of `data` with the opcode `CScript << std::vector` would use.
fn push(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    match data.len() {
        n if n < 0x4c => v.push(n as u8),
        n if n <= 0xff => v.extend([0x4c, n as u8]),
        n if n <= 0xffff => {
            v.push(0x4d);
            v.extend((n as u16).to_le_bytes());
        }
        n => {
            v.push(0x4e);
            v.extend((n as u32).to_le_bytes());
        }
    }
    v.extend_from_slice(data);
    v
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn p2pkh() -> Vec<u8> {
    cat(&[&[0x76, 0xa9, 0x14], &[0x11; 20], &[0x88, 0xac]])
}

fn p2wpkh() -> Vec<u8> {
    cat(&[&[0x00, 0x14], &[0x22; 20]])
}

fn p2tr() -> Vec<u8> {
    cat(&[&[0x51, 0x20], &[0x33; 32]])
}

fn compressed_key(fill: u8) -> Vec<u8> {
    cat(&[&[0x02], &[fill; 32]])
}

/// The outline of a 71-byte DER signature with SIGHASH_ALL.
fn der_signature() -> Vec<u8> {
    cat(&[&[0x30, 68], &[0x44; 68], &[0x01]])
}

fn out(sats: u64, spk: Vec<u8>) -> TxOut {
    TxOut {
        value: Amount::from_sat(sats),
        script_pubkey: ScriptBuf::from_bytes(spk),
    }
}

fn input(script_sig: Vec<u8>, witness: Vec<Vec<u8>>) -> TxIn {
    TxIn {
        previous_output: OutPoint {
            txid: Txid::from_byte_array([1; 32]),
            vout: 1,
        },
        script_sig: ScriptBuf::from_bytes(script_sig),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&witness),
    }
}

fn tx(input: Vec<TxIn>, output: Vec<TxOut>) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input,
        output,
    }
}

/// The Roots `test_IsStandard` starting transaction: one input with a 65-byte
/// push, one P2PKH output.
fn roots_base() -> Transaction {
    tx(
        vec![input(push(&[0; 65]), vec![])],
        vec![out(90 * CENT, p2pkh())],
    )
}

fn standard(t: &Transaction, rules: &Rules) -> Result<(), Rule> {
    check_standard(t, rules)
}

// ── Roots: test_IsStandard ──────────────────────────────────────────────────

#[test]
fn roots_base_transaction_is_standard() {
    let rules = Rules {
        permit_bare_pubkey: true,
        ..Rules::default()
    };
    assert_eq!(standard(&roots_base(), &rules), Ok(()));
    assert_eq!(check(&roots_base(), &rules, None), Ok(()));
}

#[test]
fn roots_reject_parasites() {
    let mut t = roots_base();
    t.output = vec![out(674, vec![OP_RETURN]), out(674, p2pkh())];
    t.lock_time = LockTime::from_consensus(21);
    let off = Rules {
        reject_parasites: false,
        ..Rules::default()
    };
    assert_eq!(standard(&t, &off), Ok(()));
    assert_eq!(standard(&t, &Rules::default()), Err(Rule::Cat21));
    t.lock_time = LockTime::ZERO;
    assert_eq!(standard(&t, &Rules::default()), Ok(()));
}

#[test]
fn roots_reject_tokens_runes() {
    let mut t = roots_base();
    t.output = vec![out(674, vec![OP_RETURN, OP_13, OP_0]), out(674, p2pkh())];
    let on = Rules {
        reject_tokens: true,
        reject_parasites: false,
        ..Rules::default()
    };
    let off = Rules {
        reject_tokens: false,
        ..on.clone()
    };
    assert_eq!(standard(&t, &off), Ok(()));
    assert_eq!(standard(&t, &on), Err(Rule::Runes));
    // At least one data push is needed after OP_13 to match.
    t.output[0] = out(674, vec![OP_RETURN, OP_13]);
    assert_eq!(standard(&t, &on), Ok(()));
}

#[test]
fn roots_reject_tokens_olga() {
    let header = cat(&[
        &[0x00, 0x20],
        &[0x00, 0x3e, b's', b't', b'a', b'm', b'p', b':'],
        &[0; 24],
    ]);
    let on = Rules {
        reject_tokens: true,
        ..Rules::default()
    };
    let mut t = roots_base();
    t.output = vec![out(674, header.clone())];
    // Missing a second output, so not Olga.
    assert_eq!(standard(&t, &on), Ok(()));
    t.output.push(out(1000, header.clone()));
    assert_eq!(standard(&t, &on), Err(Rule::Olga));
    t.output.push(out(1000, header));
    assert_eq!(standard(&t, &on), Err(Rule::Olga));
    assert_eq!(standard(&t, &Rules::default()), Ok(()));
}

#[test]
fn roots_datacarrier_limit_is_83_bytes() {
    let rules = Rules {
        permit_bare_datacarrier: true,
        ..Rules::default()
    };
    let mut spk = vec![OP_RETURN];
    spk.resize(83, OP_0);
    let mut t = roots_base();
    t.output = vec![out(90 * CENT, spk.clone())];
    assert_eq!(standard(&t, &rules), Ok(()));
    spk.push(OP_0);
    t.output = vec![out(90 * CENT, spk)];
    assert_eq!(standard(&t, &rules), Err(Rule::Datacarrier));
}

#[test]
fn roots_datacarrier_payload_can_be_encoded_any_way() {
    let rules = Rules {
        permit_bare_datacarrier: true,
        ..Rules::default()
    };
    let mut t = roots_base();
    let encodings = [
        cat(&[&[OP_RETURN], &push(&[])]),
        cat(&[&[OP_RETURN], &push(&[0x00]), &push(&[0x01])]),
        // OP_RESERVED, -1, 0, a push, then 2 through 16.
        cat(&[
            &[OP_RETURN, 0x50, 0x4f, 0x00],
            &push(&[0x01]),
            &(0x52..=0x60).collect::<Vec<u8>>(),
        ]),
        cat(&[
            &[OP_RETURN, 0x00],
            &push(&[0x01]),
            &[0x52],
            &push(&[0xff; 36]),
        ]),
        vec![OP_RETURN],
    ];
    for spk in encodings {
        t.output = vec![out(0, spk.clone())];
        assert_eq!(standard(&t, &rules), Ok(()), "{}", hex::encode(&spk));
    }
}

#[test]
fn roots_multiple_carriers_need_explicit_permission() {
    let data40 = hex::decode(
        "04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38",
    )
    .unwrap();
    let generous = Rules {
        datacarrier_size: 100_000,
        permit_bare_datacarrier: true,
        ..Rules::default()
    };
    let permitted = Rules {
        permit_multiple_op_return: true,
        ..generous.clone()
    };
    let pairs = [
        (
            cat(&[&[OP_RETURN], &push(&data40)]),
            cat(&[&[OP_RETURN], &push(&data40)]),
        ),
        (cat(&[&[OP_RETURN], &push(&data40)]), vec![OP_RETURN]),
        (vec![OP_RETURN], vec![OP_RETURN]),
    ];
    for (a, b) in pairs {
        let mut t = roots_base();
        t.output = vec![out(0, a), out(0, b)];
        assert_eq!(standard(&t, &generous), Err(Rule::MultiOpReturn));
        assert_eq!(standard(&t, &permitted), Ok(()));
    }

    // Two 83-byte carriers: the aggregate limit applies across outputs.
    let data80 = data40.repeat(2);
    let carrier = cat(&[&[OP_RETURN], &push(&data80)]);
    let mut t = roots_base();
    t.output = vec![out(0, carrier.clone()), out(0, carrier.clone())];
    let total = carrier.len() * 2;
    assert_eq!(
        standard(
            &t,
            &Rules {
                permit_bare_datacarrier: true,
                ..Rules::default()
            }
        ),
        Err(Rule::Datacarrier)
    );
    assert_eq!(standard(&t, &permitted), Ok(()));
    let exact = Rules {
        datacarrier_size: total,
        ..permitted.clone()
    };
    assert_eq!(standard(&t, &exact), Ok(()));
    let short = Rules {
        datacarrier_size: total - 1,
        ..permitted
    };
    assert_eq!(standard(&t, &short), Err(Rule::Datacarrier));
}

#[test]
fn roots_bare_datacarrier_needs_permission() {
    let mut t = roots_base();
    t.output = vec![out(90 * CENT, vec![OP_RETURN])];
    assert_eq!(standard(&t, &Rules::default()), Err(Rule::BareDatacarrier));
    let rules = Rules {
        permit_bare_datacarrier: true,
        ..Rules::default()
    };
    assert_eq!(standard(&t, &rules), Ok(()));
}

#[test]
fn roots_bare_multisig() {
    let one_of_one = cat(&[
        &[OP_1],
        &push(&compressed_key(0x55)),
        &[OP_1, OP_CHECKMULTISIG],
    ]);
    let mut t = roots_base();
    t.output = vec![out(MAX_MONEY, one_of_one)];
    let rules = Rules {
        permit_bare_multisig: true,
        ..Rules::default()
    };
    assert_eq!(standard(&t, &rules), Ok(()));
    assert_eq!(standard(&t, &Rules::default()), Err(Rule::BareMultisig));
}

#[test]
fn bare_pubkey() {
    let p2pk = cat(&[&push(&compressed_key(0x66)), &[OP_CHECKSIG]]);
    let mut t = roots_base();
    t.output = vec![out(90 * CENT, p2pk)];
    assert_eq!(standard(&t, &Rules::default()), Err(Rule::BarePubkey));
    let rules = Rules {
        permit_bare_pubkey: true,
        ..Rules::default()
    };
    assert_eq!(standard(&t, &rules), Ok(()));
}

// ── Roots: mempool_datacarrier.py ───────────────────────────────────────────

/// A self-transfer spending P2WSH(OP_TRUE), as the functional test's
/// MiniWallet does, plus an OP_RETURN carrying `data` (no push for `None`).
fn null_data_tx(data: Option<&[u8]>) -> Transaction {
    let carrier = match data {
        Some(d) => cat(&[&[OP_RETURN], &push(d)]),
        None => vec![OP_RETURN],
    };
    tx(
        vec![input(vec![], vec![vec![OP_1]])],
        vec![out(49_000, p2wpkh()), out(0, carrier)],
    )
}

#[test]
fn roots_null_data_limits() {
    let size = |n| Rules {
        datacarrier_size: n,
        ..Rules::default()
    };
    // -datacarriersize=83: 80 bytes of data fit, 81 do not.
    assert_eq!(
        check(&null_data_tx(Some(&[7; 80])), &size(83), None),
        Ok(())
    );
    assert_eq!(
        check(&null_data_tx(Some(&[7; 81])), &size(83), None),
        Err(Rule::Datacarrier)
    );
    // -datacarrier=0, as datacarrier_size = 0: even a bare OP_RETURN fails.
    for data in [None, Some(&[][..]), Some(&[7][..])] {
        assert_eq!(
            check(&null_data_tx(data), &size(0), None),
            Err(Rule::Datacarrier)
        );
    }
    // -datacarriersize=2: OP_RETURN and OP_RETURN OP_0 fit, one data byte does not.
    assert_eq!(check(&null_data_tx(None), &size(2), None), Ok(()));
    assert_eq!(check(&null_data_tx(Some(&[])), &size(2), None), Ok(()));
    assert_eq!(
        check(&null_data_tx(Some(&[7])), &size(2), None),
        Err(Rule::Datacarrier)
    );
}

/// The functional test's taproot carrier: a script-path spend of
/// `<520 bytes> OP_DROP OP_TRUE` with a single-leaf control block.
fn taproot_carrier() -> Transaction {
    let script = cat(&[&push(&[b'x'; 520]), &[0x75, OP_1]]);
    let control = cat(&[&[0xc0], &[0x77; 32]]);
    tx(
        vec![input(vec![], vec![script, control])],
        vec![out(90_000, p2tr())],
    )
}

#[test]
fn roots_taproot_carrier() {
    let t = taproot_carrier();
    let spent = [out(100_000, p2tr())];
    // len(CScript([b"x" * 520, OP_DROP])): the push, its 3-byte header, OP_DROP.
    let carrier_size = 3 + 520 + 1;
    for coins in [None, Some(&spent[..])] {
        assert_eq!(
            check(&t, &Rules::default(), coins),
            Err(Rule::DatacarrierNonStandard)
        );
        let full_count_off = Rules {
            accept_non_std_datacarrier: true,
            datacarrier_full_count: false,
            datacarrier_size: 0,
            ..Rules::default()
        };
        assert_eq!(check(&t, &full_count_off, coins), Ok(()));
        let short = Rules {
            accept_non_std_datacarrier: true,
            datacarrier_size: carrier_size - 1,
            ..Rules::default()
        };
        assert_eq!(check(&t, &short, coins), Err(Rule::DatacarrierExceeded));
        let exact = Rules {
            accept_non_std_datacarrier: true,
            datacarrier_size: carrier_size,
            ..Rules::default()
        };
        assert_eq!(check(&t, &exact, coins), Ok(()));
    }
}

// ── inscriptions and the witness-size rule ──────────────────────────────────

/// An ord-style inscription reveal: `<key> OP_CHECKSIG OP_FALSE OP_IF "ord" 1
/// "text/plain" 0 <body> OP_ENDIF`, spent by script path, optionally with an
/// annex.
fn inscription(body_len: usize, annex: bool) -> Transaction {
    let mut script = cat(&[&push(&[0x88; 32]), &[OP_CHECKSIG, OP_0, 0x63]]);
    script.extend(push(b"ord"));
    script.extend(push(&[1]));
    script.extend(push(b"text/plain"));
    script.push(OP_0);
    for chunk in vec![b'i'; body_len].chunks(520) {
        script.extend(push(chunk));
    }
    script.push(0x68);
    let mut witness = vec![vec![0x99; 64], script, cat(&[&[0xc1], &[0x77; 32]])];
    if annex {
        witness.push(vec![0x50, 0x01]);
    }
    tx(vec![input(vec![], witness)], vec![out(546, p2tr())])
}

#[test]
fn inscriptions_are_declined_with_and_without_the_coin() {
    let spent = [out(10_000, p2tr())];
    for annex in [false, true] {
        let t = inscription(100, annex);
        assert_eq!(
            check(&t, &Rules::default(), None),
            Err(Rule::DatacarrierNonStandard)
        );
        assert_eq!(
            check(&t, &Rules::default(), Some(&spent)),
            Err(Rule::DatacarrierNonStandard)
        );
        // Counted toward the limit when embedded data is otherwise allowed.
        let counted = Rules {
            accept_non_std_datacarrier: true,
            ..Rules::default()
        };
        assert_eq!(check(&t, &counted, None), Err(Rule::DatacarrierExceeded));
    }
}

#[test]
fn large_witness_is_declined_whatever_it_holds() {
    let t = inscription(2_000, false);
    let only_size = Rules {
        accept_non_std_datacarrier: true,
        datacarrier_full_count: false,
        ..Rules::default()
    };
    assert_eq!(check(&t, &only_size, None), Err(Rule::WitnessSize));
    let roomy = Rules {
        max_script_size: 100_000,
        ..only_size
    };
    assert_eq!(check(&t, &roomy, None), Ok(()));
}

// ── ordinary spends are never declined ──────────────────────────────────────

/// A compressed key whose bytes parse as `<2-byte push> OP_DROP ...`. Read as a
/// script it looks like embedded data; it must be recognised as a key.
fn tricky_key() -> Vec<u8> {
    cat(&[&[0x02, 0xaa, 0xbb, 0x75], &[0x11; 29]])
}

#[test]
fn ordinary_spends_pass_by_shape() {
    let redeem_2_of_3 = cat(&[
        &[0x52],
        &push(&compressed_key(1)),
        &push(&compressed_key(2)),
        &push(&compressed_key(3)),
        &[0x53, OP_CHECKMULTISIG],
    ]);
    let cases: Vec<(&str, TxIn)> = vec![
        ("p2wpkh", input(vec![], vec![der_signature(), tricky_key()])),
        (
            "p2sh-p2wpkh",
            input(push(&p2wpkh()), vec![der_signature(), tricky_key()]),
        ),
        ("p2tr key path", input(vec![], vec![vec![0x99; 64]])),
        (
            "p2pkh",
            input(
                cat(&[&push(&der_signature()), &push(&tricky_key())]),
                vec![],
            ),
        ),
        (
            "p2sh 2-of-3",
            input(
                cat(&[
                    &[OP_0],
                    &push(&der_signature()),
                    &push(&der_signature()),
                    &push(&redeem_2_of_3),
                ]),
                vec![],
            ),
        ),
        (
            "p2wsh 2-of-3",
            input(
                vec![],
                vec![
                    vec![],
                    der_signature(),
                    der_signature(),
                    redeem_2_of_3.clone(),
                ],
            ),
        ),
        (
            "p2tr script path",
            input(
                vec![],
                vec![
                    vec![0x99; 64],
                    cat(&[&push(&[0x88; 32]), &[OP_CHECKSIG]]),
                    cat(&[&[0xc0], &[0x77; 32]]),
                ],
            ),
        ),
    ];
    for (name, txin) in cases {
        let t = tx(vec![txin], vec![out(50_000, p2wpkh())]);
        assert_eq!(check(&t, &Rules::default(), None), Ok(()), "{name}");
    }
}

#[test]
fn exact_mode_agrees_on_ordinary_spends() {
    let t = tx(
        vec![input(vec![], vec![der_signature(), tricky_key()])],
        vec![out(50_000, p2wpkh())],
    );
    assert_eq!(
        check(&t, &Rules::default(), Some(&[out(60_000, p2wpkh())])),
        Ok(())
    );
}

#[test]
fn coinbase_is_never_checked() {
    let mut t = null_data_tx(Some(&[7; 200]));
    t.input = vec![TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuf::from_bytes(vec![0x03, 0x01, 0x02, 0x03]),
        sequence: Sequence::MAX,
        witness: Witness::new(),
    }];
    assert_eq!(check(&t, &Rules::default(), None), Ok(()));
}

#[test]
fn every_rule_names_its_reason_and_key() {
    let all = [
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
    for rule in all {
        assert!(!rule.reason().is_empty());
        assert!(!rule.key().is_empty());
    }
}
