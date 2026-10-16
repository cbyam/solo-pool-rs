//! Finding the script an input executes.
//!
//! With the spent coin, [`executed_script_exact`] is a port of Bitcoin Roots'
//! `GetScriptForTransactionInput` (`src/policy/policy.cpp`, MIT licensed,
//! Copyright The Bitcoin Core developers). Without it, [`executed_script_by_shape`]
//! infers the input type from the witness and scriptSig. The shape rules must
//! recognise key spends explicitly: an ordinary public key parsed as a script
//! can look like embedded data.

use std::borrow::Cow;

use bitcoin::TxIn;

use super::script::{is_p2sh, push_only_stack, witness_program};

/// Taproot's annex tag (BIP 341).
const ANNEX_TAG: u8 = 0x50;

/// The witness stack as byte slices.
pub(crate) fn witness_stack(txin: &TxIn) -> Vec<&[u8]> {
    txin.witness.iter().collect()
}

/// The script this input executes, given the scriptPubKey of the coin it
/// spends. Empty where Roots returns an empty script (a key spend, or an
/// input that would fail validation).
pub(crate) fn executed_script_exact<'a>(prev_spk: &'a [u8], txin: &'a TxIn) -> Cow<'a, [u8]> {
    let mut script: Cow<'a, [u8]> = Cow::Borrowed(prev_spk);
    let mut p2sh = false;
    if is_p2sh(prev_spk) {
        match push_only_stack(txin.script_sig.as_bytes()) {
            Some(mut stack) if !stack.is_empty() => {
                script = Cow::Owned(stack.pop().expect("stack is not empty"));
                p2sh = true;
            }
            _ => return Cow::Borrowed(&[]),
        }
    }

    let Some((version, program)) = witness_program(&script) else {
        // For P2SH the executed script is the redeem script; for anything else
        // Roots returns the scriptSig, since the scriptPubKey is not part of
        // this transaction.
        return if p2sh {
            script
        } else {
            Cow::Borrowed(txin.script_sig.as_bytes())
        };
    };
    let stack = witness_stack(txin);

    if version == 0 && program.len() == 32 {
        return match stack.last() {
            Some(witness_script) => Cow::Borrowed(*witness_script),
            None => Cow::Borrowed(&[]),
        };
    }

    if version == 1 && program.len() == 32 && !p2sh {
        let mut stack = &stack[..];
        if stack.len() >= 2 && stack.last().is_some_and(|a| a.first() == Some(&ANNEX_TAG)) {
            stack = &stack[..stack.len() - 1];
        }
        if stack.len() >= 2 {
            return Cow::Borrowed(stack[stack.len() - 2]);
        }
    }

    Cow::Borrowed(&[])
}

/// The script this input most likely executes, inferred without the spent
/// coin. Empty for key spends.
pub(crate) fn executed_script_by_shape(txin: &TxIn) -> Cow<'_, [u8]> {
    let witness = witness_stack(txin);
    if !witness.is_empty() {
        // Taproot script path: an optional annex, then a control block, then
        // the tapscript. A segwit v0 witness script can never start with
        // 0xc0 or 0xc1 (those bytes are invalid opcodes and the first opcode
        // always executes), so a control-block shape is unambiguous.
        let mut stack = &witness[..];
        if stack.len() >= 2 && stack.last().is_some_and(|a| a.first() == Some(&ANNEX_TAG)) {
            stack = &stack[..stack.len() - 1];
        }
        if stack.len() >= 2 && is_control_block(stack[stack.len() - 1]) {
            return Cow::Borrowed(stack[stack.len() - 2]);
        }
        // P2WPKH (bare or P2SH-wrapped): a signature and a compressed key.
        if witness.len() == 2 && is_compressed_pubkey(witness[1]) {
            return Cow::Borrowed(&[]);
        }
        // Taproot key path: a single Schnorr signature.
        if witness.len() == 1 && matches!(witness[0].len(), 64 | 65) {
            return Cow::Borrowed(&[]);
        }
        // Otherwise P2WSH (bare or P2SH-wrapped): the last item is the
        // witness script.
        return Cow::Borrowed(witness[witness.len() - 1]);
    }

    // Legacy. A P2SH redeem script is the last scriptSig push; anything else
    // (P2PKH, P2PK, bare multisig) executes the push-only scriptSig, which
    // never carries counted data, so it is treated as no script.
    let Some(mut pushes) = push_only_stack(txin.script_sig.as_bytes()) else {
        return Cow::Borrowed(&[]);
    };
    match pushes.pop() {
        Some(last) if !last.is_empty() && !is_pubkey(&last) && !is_der_signature(&last) => {
            Cow::Owned(last)
        }
        _ => Cow::Borrowed(&[]),
    }
}

/// A BIP 341 control block for a tapscript leaf: 33 + 32m bytes, m ≤ 128,
/// leaf version 0xc0.
fn is_control_block(item: &[u8]) -> bool {
    item.len() >= 33
        && (item.len() - 33).is_multiple_of(32)
        && (item.len() - 33) / 32 <= 128
        && item[0] & 0xfe == 0xc0
}

fn is_compressed_pubkey(item: &[u8]) -> bool {
    item.len() == 33 && matches!(item[0], 0x02 | 0x03)
}

/// Core's `CPubKey::ValidSize` on its own: a compressed or uncompressed key.
fn is_pubkey(item: &[u8]) -> bool {
    match item.first() {
        Some(0x02 | 0x03) => item.len() == 33,
        Some(0x04 | 0x06 | 0x07) => item.len() == 65,
        _ => false,
    }
}

/// The outline of a DER ECDSA signature with its sighash byte.
fn is_der_signature(item: &[u8]) -> bool {
    (9..=73).contains(&item.len()) && item[0] == 0x30 && item[1] as usize == item.len() - 3
}
