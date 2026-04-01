/// bitcoin/template.rs
///
/// Converts a raw GBT result into a Stratum job, including:
///  - Coinbase transaction construction (BIP34 height, extranonce, tag, reward output)
///  - SegWit witness commitment output
///  - Merkle branch computation for mining.notify
///  - prev_hash byte-reversal (Core → Stratum format)
///  - Job ID management
use super::rpc::GbtResult;
use crate::error::PoolError;
use bitcoin::{
    blockdata::transaction::{OutPoint, Transaction, TxIn, TxOut},
    consensus::encode::serialize,
    Amount, ScriptBuf, Sequence, Witness,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────────────────────────────────────
// Job ID counter
// ─────────────────────────────────────────────────────────────────────────────

static JOB_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn next_job_id() -> String {
    format!("{:08x}", JOB_COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ─────────────────────────────────────────────────────────────────────────────
// StratumJob — everything a miner session needs
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StratumJob {
    /// Unique job identifier (hex string, sent in mining.notify)
    pub job_id: String,

    /// Previous block hash in Stratum byte order (8 groups of 4 bytes, each reversed)
    pub prev_hash: String,

    /// Serialized coinbase part 1: everything before the extranonce placeholder
    pub coinbase1: Vec<u8>,

    /// Serialized coinbase part 2: everything after the extranonce placeholder
    pub coinbase2: Vec<u8>,

    /// Merkle branch hashes (hex) for mining.notify
    pub merkle_branch: Vec<String>,

    /// Block version (may include version-rolling mask bits)
    pub version: u32,

    /// Compact target from GBT (nbits)
    pub bits: String,

    /// Template time advertised to miners in mining.notify
    pub cur_time: u32,

    /// Block height (for BIP34 and logging)
    pub height: u64,

    /// Network target as 32 bytes (derived from bits)
    pub network_target: [u8; 32],

    /// Full serialized coinbase (for block assembly after share submit)
    /// extranonce1 + extranonce2 slots are zero until filled by `assemble_coinbase`
    pub coinbase_template: Vec<u8>,

    /// Byte offsets of the extranonce field inside `coinbase_template`
    pub extranonce_offset: usize,
    pub extranonce1_len: usize,
    pub extranonce2_len: usize,

    /// All transaction data (for block assembly)
    pub transactions: Vec<Vec<u8>>,
}

impl StratumJob {
    /// Assemble the full coinbase by splicing extranonce1 + extranonce2.
    pub fn assemble_coinbase(&self, extranonce1: &[u8], extranonce2: &[u8]) -> Vec<u8> {
        let mut cb = self.coinbase_template.clone();
        let off = self.extranonce_offset;
        cb[off..off + self.extranonce1_len].copy_from_slice(extranonce1);
        cb[off + self.extranonce1_len..off + self.extranonce1_len + self.extranonce2_len]
            .copy_from_slice(extranonce2);
        cb
    }

    /// Compute the merkle root given a fully assembled coinbase.
    pub fn merkle_root(&self, coinbase: &[u8]) -> [u8; 32] {
        let cb_hash = double_sha256(coinbase);
        let mut hash = cb_hash;
        for branch in &self.merkle_branch {
            let branch_bytes = hex::decode(branch).expect("valid merkle branch hex");
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(&hash);
            combined[32..].copy_from_slice(&branch_bytes);
            hash = double_sha256(&combined);
        }
        hash
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Job builder
// ─────────────────────────────────────────────────────────────────────────────

/// Build a `StratumJob` from a GBT result.
///
/// `coinbase_address`   — P2WPKH/P2PKH address receiving the block reward
/// `coinbase_tag`       — arbitrary UTF-8 tag embedded in the coinbase scriptSig
/// `extranonce1_len`    — bytes reserved for pool-assigned extranonce1
/// `extranonce2_len`    — bytes reserved for miner-chosen extranonce2
pub fn build_job(
    gbt: &GbtResult,
    coinbase_address: &str,
    coinbase_tag: &str,
    extranonce1_len: usize,
    extranonce2_len: usize,
) -> Result<StratumJob, PoolError> {
    let job_id = next_job_id();

    // ── 1. Build coinbase transaction ─────────────────────────────────────────
    let (coinbase_bytes, extranonce_offset) = build_coinbase(
        gbt.height,
        gbt.coinbase_value,
        coinbase_address,
        coinbase_tag,
        extranonce1_len,
        extranonce2_len,
        gbt.default_witness_commitment.as_deref(),
    )?;

    // ── 2. Split coinbase around extranonce placeholder ───────────────────────
    let coinbase1 = coinbase_bytes[..extranonce_offset].to_vec();
    let en_total = extranonce1_len + extranonce2_len;
    let coinbase2 = coinbase_bytes[extranonce_offset + en_total..].to_vec();

    // ── 3. Merkle branch (from tx hashes, excluding coinbase) ─────────────────
    let tx_txids: Vec<[u8; 32]> = gbt
        .transactions
        .iter()
        .map(|tx| {
            let mut b = hex::decode(&tx.txid).unwrap_or_default();
            b.reverse(); // txid in internal byte order
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b[..32.min(b.len())]);
            arr
        })
        .collect();

    let merkle_branch = compute_merkle_branch(&tx_txids);

    // ── 4. Stratum-format prev_hash (byte-swap each 4-byte word) ─────────────
    let prev_hash_stratum = stratum_prev_hash(&gbt.prev_hash)?;

    // ── 5. Network target from bits ───────────────────────────────────────────
    let network_target = bits_to_target(&gbt.bits)?;

    Ok(StratumJob {
        job_id,
        prev_hash: prev_hash_stratum,
        coinbase1,
        coinbase2,
        merkle_branch,
        version: gbt.version,
        bits: gbt.bits.clone(),
        cur_time: gbt.cur_time,
        height: gbt.height,
        network_target,
        coinbase_template: coinbase_bytes,
        extranonce_offset,
        extranonce1_len,
        extranonce2_len,
        transactions: gbt.transactions.iter().map(|t| t.data.clone()).collect(),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Coinbase construction
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `(serialized_coinbase, extranonce_offset)`.
///
/// Coinbase scriptSig layout:
///   [BIP34 height push] [tag bytes] [extranonce1 placeholder] [extranonce2 placeholder]
///
/// The extranonce placeholder region is zeroed; callers fill it at submit time.
fn build_coinbase(
    height: u64,
    reward: u64,
    address: &str,
    tag: &str,
    extranonce1_len: usize,
    extranonce2_len: usize,
    witness_commitment: Option<&str>,
) -> Result<(Vec<u8>, usize), PoolError> {
    // ── scriptSig ─────────────────────────────────────────────────────────────
    let height_script = encode_bip34_height(height);
    let tag_bytes = tag.as_bytes();

    // We need to know the offset before building the script, so we compute
    // where the extranonce will land inside the serialized transaction.
    //
    // CoinbaseTx layout (simplified):
    //   4  version
    //   1  marker (segwit) or ...
    //   varint  vin count = 1
    //   36 outpoint (32 hash + 4 index)
    //   varint  scriptSig len
    //   N  scriptSig content   ← extranonce is inside here
    //   4  sequence
    //   ...outputs...
    //
    // We build a "dummy" scriptSig, serialize the tx, then locate the
    // extranonce bytes by searching for a known sentinel.

    const SENTINEL: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    let en_total = extranonce1_len + extranonce2_len;

    let mut script_sig_content = Vec::new();
    script_sig_content.extend_from_slice(&height_script);
    script_sig_content.extend_from_slice(tag_bytes);
    // Extranonce placeholder — sentinel + zeros
    script_sig_content.extend_from_slice(&SENTINEL);
    script_sig_content.resize(script_sig_content.len() + en_total.saturating_sub(4), 0x00);

    let script_sig = ScriptBuf::from_bytes(script_sig_content);

    // ── Inputs ────────────────────────────────────────────────────────────────
    let coinbase_input = TxIn {
        previous_output: OutPoint::null(), // all-zeros (coinbase)
        script_sig,
        sequence: Sequence::MAX,
        witness: Witness::default(),
    };

    // ── Outputs ───────────────────────────────────────────────────────────────
    let reward_script = address_to_script(address)?;
    let reward_output = TxOut {
        value: Amount::from_sat(reward),
        script_pubkey: reward_script,
    };

    let mut outputs = vec![reward_output];

    // SegWit witness commitment (OP_RETURN)
    if let Some(wc_hex) = witness_commitment {
        // GBT's default_witness_commitment is already the full scriptPubKey
        // (OP_RETURN OP_36 0xaa21a9ed <32-byte-hash>), so use it as-is.
        let wc_script_bytes = hex::decode(wc_hex)
            .map_err(|e| PoolError::Other(anyhow::anyhow!("witness commitment hex: {e}")))?;
        outputs.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(wc_script_bytes),
        });
    }

    // ── Assemble transaction (non-segwit serialisation for coinbase) ──────────
    let tx = Transaction {
        version: bitcoin::transaction::Version(1),
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![coinbase_input],
        output: outputs,
    };

    let serialized = serialize(&tx);

    // ── Locate extranonce offset inside the serialized bytes ──────────────────
    let offset = find_bytes(&serialized, &SENTINEL).ok_or_else(|| {
        PoolError::Other(anyhow::anyhow!("Extranonce sentinel not found in coinbase"))
    })?;

    // Replace sentinel with zeros
    let mut final_bytes = serialized;
    for i in offset..offset + en_total.min(final_bytes.len() - offset) {
        final_bytes[i] = 0x00;
    }

    Ok((final_bytes, offset))
}

/// Encode block height as a minimal CScript push for BIP34.
fn encode_bip34_height(height: u64) -> Vec<u8> {
    if height == 0 {
        return vec![0x01, 0x00];
    }
    let mut n = height;
    let mut bytes = Vec::new();
    while n > 0 {
        bytes.push((n & 0xff) as u8);
        n >>= 8;
    }
    // If high bit set, add 0x00 to avoid sign-bit interpretation
    if bytes.last().is_some_and(|&b| b & 0x80 != 0) {
        bytes.push(0x00);
    }
    let mut result = vec![bytes.len() as u8];
    result.extend_from_slice(&bytes);
    result
}

/// Convert a Bitcoin address string to its scriptPubKey bytes.
fn address_to_script(address: &str) -> Result<ScriptBuf, PoolError> {
    use std::str::FromStr;
    let addr = bitcoin::Address::from_str(address)
        .map_err(|e| PoolError::Other(anyhow::anyhow!("Invalid coinbase address: {e}")))?;
    // require_network can be skipped for solo pool (trusts config)
    Ok(addr.assume_checked().script_pubkey())
}

// ─────────────────────────────────────────────────────────────────────────────
// Merkle branch
// ─────────────────────────────────────────────────────────────────────────────

/// Compute the Stratum merkle branch for the coinbase transaction.
///
/// `txids` must contain every non-coinbase txid in internal byte order.
/// The returned hashes are the coinbase path siblings, from leaf upward.
pub fn compute_merkle_branch(txids: &[[u8; 32]]) -> Vec<String> {
    if txids.is_empty() {
        return vec![];
    }

    let mut branch = Vec::new();
    let mut path_index = 0usize; // coinbase is always leaf 0
    let mut level: Vec<Option<[u8; 32]>> = Vec::with_capacity(txids.len() + 1);
    level.push(None); // placeholder for the unknown coinbase hash
    level.extend(txids.iter().copied().map(Some));

    while level.len() > 1 {
        if level.len() % 2 != 0 {
            let last = *level.last().expect("non-empty merkle level");
            level.push(last);
        }

        let sibling_index = if path_index % 2 == 0 {
            path_index + 1
        } else {
            path_index - 1
        };

        if let Some(sibling) = level[sibling_index] {
            branch.push(hex::encode(sibling));
        }

        let mut next_level = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks(2) {
            match (pair[0], pair[1]) {
                (Some(left), Some(right)) => {
                    let mut buf = [0u8; 64];
                    buf[..32].copy_from_slice(&left);
                    buf[32..].copy_from_slice(&right);
                    next_level.push(Some(double_sha256(&buf)));
                }
                _ => next_level.push(None),
            }
        }

        level = next_level;
        path_index /= 2;
    }

    branch
}

// ─────────────────────────────────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────────────────────────────────

/// Double SHA-256
pub fn double_sha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    second.into()
}

/// Convert compact `bits` (hex string) to a 32-byte big-endian target.
pub fn bits_to_target(bits_hex: &str) -> Result<[u8; 32], PoolError> {
    let bits = u32::from_str_radix(bits_hex, 16)
        .map_err(|_| PoolError::Other(anyhow::anyhow!("Invalid bits: {bits_hex}")))?;
    compact_to_target(bits)
}

/// Convert compact `bits` (hex string) to a network difficulty value.
pub fn bits_to_difficulty(bits_hex: &str) -> Result<f64, PoolError> {
    let bits = u32::from_str_radix(bits_hex, 16)
        .map_err(|_| PoolError::Other(anyhow::anyhow!("Invalid bits: {bits_hex}")))?;
    let exponent = ((bits >> 24) & 0xff) as i32;
    let mantissa = (bits & 0x007f_ffff) as u64;
    if mantissa == 0 {
        return Err(PoolError::Other(anyhow::anyhow!("Invalid bits mantissa = 0")));
    }

    // difficulty = diff1_target / current_target
    // diff1_target = 0x00ffff * 2^208
    // current_target  = mantissa * 2^(8*(exponent-3))
    // => difficulty = (0x00ffff / mantissa) * 2^(232 - 8*exponent)
    let diff1_const = 0x00ffffu64 as f64;
    let exponent_factor = 232.0 - (8.0 * exponent as f64);
    let diff = diff1_const / mantissa as f64 * 2f64.powf(exponent_factor);
    Ok(diff)
}

/// Convert a network difficulty to a 32-byte share target using exact integer division.
///
/// difficulty_1_target = 0x00000000FFFF0000000000000000000000000000000000000000000000000000
pub fn difficulty_to_target(difficulty: u64) -> [u8; 32] {
    const DIFF1_TARGET: [u8; 32] = [
        0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];

    if difficulty <= 1 {
        return DIFF1_TARGET;
    }

    div_be_u256_by_u64(&DIFF1_TARGET, difficulty)
}

fn compact_to_target(bits: u32) -> Result<[u8; 32], PoolError> {
    let exponent = ((bits >> 24) & 0xff) as usize;
    let mantissa = bits & 0x007f_ffff;

    if mantissa == 0 {
        return Ok([0u8; 32]);
    }

    let mut target = [0u8; 32];
    if exponent <= 3 {
        let value = mantissa >> (8 * (3 - exponent));
        let bytes = value.to_be_bytes();
        target[28..32].copy_from_slice(&bytes);
        return Ok(target);
    }

    let shift = exponent - 3;
    if shift > 29 {
        return Err(PoolError::Other(anyhow::anyhow!(
            "bits overflow target width: {bits:08x}"
        )));
    }

    let mantissa_bytes = [
        ((mantissa >> 16) & 0xff) as u8,
        ((mantissa >> 8) & 0xff) as u8,
        (mantissa & 0xff) as u8,
    ];
    let offset = 32 - 3 - shift;
    target[offset..offset + 3].copy_from_slice(&mantissa_bytes);
    Ok(target)
}

fn div_be_u256_by_u64(value: &[u8; 32], divisor: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut rem: u128 = 0;

    for (i, byte) in value.iter().enumerate() {
        let accum = (rem << 8) | (*byte as u128);
        out[i] = (accum / divisor as u128) as u8;
        rem = accum % divisor as u128;
    }

    out
}

/// Reverse byte order within each 4-byte word of the previous block hash.
/// Bitcoin Core returns hashes in internal byte order; Stratum wants word-swapped.
pub fn stratum_prev_hash(core_hex: &str) -> Result<String, PoolError> {
    let mut bytes = hex::decode(core_hex)
        .map_err(|_| PoolError::Other(anyhow::anyhow!("Invalid prev_hash hex")))?;
    if bytes.len() != 32 {
        return Err(PoolError::Other(anyhow::anyhow!(
            "prev_hash must be 32 bytes"
        )));
    }
    for chunk in bytes.chunks_mut(4) {
        chunk.reverse();
    }
    Ok(hex::encode(bytes))
}

/// Locate the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bip34_height_encoding() {
        // Height 0 → 0x01 0x00
        assert_eq!(encode_bip34_height(0), vec![0x01, 0x00]);
        // Height 1 → 0x01 0x01
        assert_eq!(encode_bip34_height(1), vec![0x01, 0x01]);
        // Height 128 → needs 2 bytes (sign-bit extension)
        let h128 = encode_bip34_height(128);
        assert_eq!(h128.len(), 3); // len byte + 0x80 + 0x00
    }

    #[test]
    fn test_bits_to_target_mainnet_genesis() {
        // Genesis bits: 0x1d00ffff
        let target = bits_to_target("1d00ffff").unwrap();
        assert_eq!(&target[..6], &[0, 0, 0, 0, 0xff, 0xff]);
    }

    #[test]
    fn test_difficulty_to_target_diff1() {
        let t = difficulty_to_target(1);
        assert!(t[4] > 0, "diff-1 target should be non-zero around byte 4");
    }

    #[test]
    fn test_stratum_prev_hash_roundtrip() {
        let original = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let swapped = stratum_prev_hash(original).unwrap();
        assert_eq!(swapped.len(), 64); // still 32 bytes
        assert_ne!(swapped, original); // should differ
    }

    #[test]
    fn test_empty_merkle_branch() {
        let branch = compute_merkle_branch(&[]);
        assert!(branch.is_empty());
    }

    #[test]
    fn test_merkle_branch_for_three_transactions() {
        let tx1 = [0x11u8; 32];
        let tx2 = [0x22u8; 32];
        let branch = compute_merkle_branch(&[tx1, tx2]);
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[0], hex::encode(tx1));

        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&tx2);
        buf[32..].copy_from_slice(&tx2);
        assert_eq!(branch[1], hex::encode(double_sha256(&buf)));
    }
}
