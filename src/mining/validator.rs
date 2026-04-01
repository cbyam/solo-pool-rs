/// mining/validator.rs
///
/// Share and block validation logic:
///  - Reconstruct the 80-byte block header from share parameters
///  - Verify the double-SHA256 hash meets the share target
///  - Detect duplicate shares (per-session set)
///  - Detect network-difficulty hits (BLOCK FOUND!)
///  - Stale share detection (job not current)
///  - Version-rolling validation (BIP320 mask enforcement)
use crate::{
    bitcoin::template::{difficulty_to_target, double_sha256, StratumJob},
    error::PoolError,
    mining::engine::JobEntry,
};

use std::collections::{HashSet, VecDeque};

// ─────────────────────────────────────────────────────────────────────────────
// BIP320 version-rolling mask
// ─────────────────────────────────────────────────────────────────────────────

/// Only these bits are allowed to be modified by the miner (BIP320).
/// 0x1FFFE000 = bits 13–28 (16 bits of version space)
pub const VERSION_ROLLING_MASK: u32 = 0x1FFF_E000;

// ─────────────────────────────────────────────────────────────────────────────
// Share duplicate tracker
// ─────────────────────────────────────────────────────────────────────────────

/// Per-session duplicate-share tracker.
/// Stores (job_id, extranonce2_hex, ntime, nonce) tuples.
/// Bounded to prevent memory exhaustion — evicts the oldest entry when full.
pub struct ShareSet {
    seen: HashSet<ShareKey>,
    /// Insertion-order queue for FIFO eviction.
    order: VecDeque<ShareKey>,
    max_size: usize,
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct ShareKey {
    job_id: String,
    extranonce2: Vec<u8>,
    ntime: u32,
    nonce: u32,
    version_bits: u32,
}

impl ShareSet {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            max_size: 4096,
        }
    }

    /// Returns true if this share has been seen before (duplicate).
    pub fn check_and_insert(
        &mut self,
        job_id: &str,
        extranonce2: &[u8],
        ntime: u32,
        nonce: u32,
        version_bits: u32,
    ) -> bool {
        let key = ShareKey {
            job_id: job_id.to_string(),
            extranonce2: extranonce2.to_vec(),
            ntime,
            nonce,
            version_bits,
        };
        if self.seen.contains(&key) {
            return true; // duplicate
        }
        if self.seen.len() >= self.max_size {
            // Evict the oldest entry rather than clearing the whole set.
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.seen.insert(key);
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Share submission parameters
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ShareParams {
    #[allow(dead_code)]
    pub worker: String,
    pub job_id: String,
    pub extranonce2: Vec<u8>,
    pub ntime: u32,
    pub nonce: u32,
    /// BIP320: miner-submitted version bits
    pub version_bits: Option<u32>,
    /// Per-session negotiated version-rolling mask
    pub version_rolling_mask: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation result
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ShareResult {
    /// Valid share meeting pool difficulty — keep mining
    Valid {
        assigned_difficulty: u64,
        hash: [u8; 32],
    },
    /// 🎉 Valid share that ALSO meets network difficulty — submit block!
    Block {
        assigned_difficulty: u64,
        block_hex: String,
        hash: [u8; 32],
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Core validation function
// ─────────────────────────────────────────────────────────────────────────────

/// Validate a share submission.
///
/// Returns:
///   - `Ok(ShareResult::Valid)` — good share
///   - `Ok(ShareResult::Block)` — block found, submit immediately
///   - `Err(PoolError::*)` — rejected share with reason
pub fn validate_share(
    params: &ShareParams,
    job: &StratumJob,
    job_entry: &JobEntry,
    extranonce1: &[u8],
    session_difficulty: u64,
    share_set: &mut ShareSet,
) -> Result<ShareResult, PoolError> {
    // ── 1. Stale job check ────────────────────────────────────────────────────
    if job_entry.superseded_by_clean {
        return Err(PoolError::StaleJob(params.job_id.clone()));
    }

    // ── 2. Duplicate share check ──────────────────────────────────────────────
    if share_set.check_and_insert(
        &params.job_id,
        &params.extranonce2,
        params.ntime,
        params.nonce,
        params.version_bits.unwrap_or(0),
    ) {
        return Err(PoolError::DuplicateShare);
    }

    // ── 3. ntime validation — pool acceptance policy, not consensus ──────────
    // Bitcoin consensus allows any ntime ≥ median-time-past. This window
    // (cur_time..cur_time+7200) is a tighter pool-side drift limit.
    if params.ntime < job.cur_time || params.ntime > job.cur_time.saturating_add(7200) {
        return Err(PoolError::InvalidParams {
            method: "mining.submit",
            detail: format!(
                "ntime out of range: submitted={} template_curtime={}",
                params.ntime, job.cur_time
            ),
        });
    }

    // ── 4. Assemble coinbase ──────────────────────────────────────────────────
    let coinbase = job.assemble_coinbase(extranonce1, &params.extranonce2);
    let _coinbase_hash = double_sha256(&coinbase);

    // ── 5. Compute merkle root ────────────────────────────────────────────────
    let merkle_root = job.merkle_root(&coinbase);

    // ── 6. Resolve version (with optional BIP320 rolling) ────────────────────
    let version = resolve_version(
        job.version,
        params.version_bits,
        params.version_rolling_mask,
    )?;

    // ── 7. Assemble 80-byte block header ─────────────────────────────────────
    let header = build_header(
        version,
        &job.prev_hash,
        &merkle_root,
        params.ntime,
        &job.bits,
        params.nonce,
    )?;

    // ── 8. Double-SHA256 of header ────────────────────────────────────────────
    let hash = double_sha256(&header);

    // ── 9. Check hash meets pool share target ─────────────────────────────────
    let share_target = difficulty_to_target(session_difficulty);
    if !meets_target(&hash, &share_target) {
        let mut hash_be = hash;
        hash_be.reverse();
        tracing::warn!(
            hash_le = %hex::encode(hash),
            hash_be = %hex::encode(hash_be),
            share_target = %hex::encode(share_target),
            session_difficulty = session_difficulty,
            "Share failed target check"
        );
        return Err(PoolError::LowDifficulty);
    }

    // ── 10. Check if hash also meets network target (BLOCK FOUND!) ────────────
    if meets_target(&hash, &job.network_target) {
        let block_hex = assemble_block_hex(&header, &coinbase, &job.transactions);
        tracing::info!(
            "🎉 BLOCK FOUND! height={} hash={}",
            job.height,
            hex::encode(hash)
        );
        return Ok(ShareResult::Block {
            assigned_difficulty: session_difficulty,
            block_hex,
            hash,
        });
    }
    Ok(ShareResult::Valid {
        assigned_difficulty: session_difficulty,
        hash,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Apply BIP320 version-rolling: only modify bits allowed by the mask.
fn resolve_version(
    base_version: u32,
    rolling_bits: Option<u32>,
    negotiated_mask: Option<u32>,
) -> Result<u32, PoolError> {
    let mask = negotiated_mask.unwrap_or(VERSION_ROLLING_MASK);
    match rolling_bits {
        Some(bits) => {
            if bits & !mask != 0 {
                return Err(PoolError::InvalidParams {
                    method: "mining.submit",
                    detail: format!(
                        "version bits outside negotiated mask: bits={bits:08x} mask={mask:08x}"
                    ),
                });
            }
            Ok((base_version & !mask) | (bits & mask))
        }
        None => Ok(base_version),
    }
}

/// Build an 80-byte block header.
///
/// Layout (all little-endian):
///   4  version
///   32 prev_block  (Stratum-format → must reverse back to internal order)
///   32 merkle_root
///   4  ntime
///   4  nbits
///   4  nonce
fn build_header(
    version: u32,
    stratum_prev_hash: &str,
    merkle_root: &[u8; 32],
    ntime: u32,
    bits_hex: &str,
    nonce: u32,
) -> Result<[u8; 80], PoolError> {
    let mut header = [0u8; 80];

    // version (LE)
    header[..4].copy_from_slice(&version.to_le_bytes());

    // prev_hash: un-stratum it (reverse each 4-byte word back)
    let prev_bytes = hex::decode(stratum_prev_hash).map_err(|_| PoolError::InvalidHeader)?;
    let mut prev_internal = prev_bytes.clone();
    for chunk in prev_internal.chunks_mut(4) {
        chunk.reverse();
    }
    header[4..36].copy_from_slice(&prev_internal);

    // merkle root (LE — bitcoin's internal byte order)
    header[36..68].copy_from_slice(merkle_root);

    // ntime (LE)
    header[68..72].copy_from_slice(&ntime.to_le_bytes());

    // nbits (from hex, stored LE)
    let bits = u32::from_str_radix(bits_hex, 16).map_err(|_| PoolError::InvalidHeader)?;
    header[72..76].copy_from_slice(&bits.to_le_bytes());

    // nonce (LE)
    header[76..80].copy_from_slice(&nonce.to_le_bytes());

    Ok(header)
}

/// Serialise the complete block as hex for submitblock.
fn assemble_block_hex(header: &[u8; 80], coinbase: &[u8], transactions: &[Vec<u8>]) -> String {
    let mut block = Vec::with_capacity(
        80 + coinbase.len() + transactions.iter().map(|t| t.len()).sum::<usize>() + 16,
    );
    block.extend_from_slice(header);

    // Transaction count varint
    let tx_count = 1 + transactions.len(); // coinbase + rest
    block.extend_from_slice(&encode_varint(tx_count as u64));

    // Coinbase first
    block.extend_from_slice(coinbase);

    // All other transactions
    for tx in transactions {
        block.extend_from_slice(tx);
    }

    hex::encode(block)
}

/// Check `hash < target` (both 32-byte big-endian).
pub fn meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    let mut hash_be = *hash;
    hash_be.reverse();
    hash_be <= *target
}

fn encode_varint(n: u64) -> Vec<u8> {
    if n < 0xfd {
        vec![n as u8]
    } else if n <= 0xffff {
        let mut v = vec![0xfd];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= 0xffff_ffff {
        let mut v = vec![0xfe];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![0xff];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_meets_target_lower() {
        // hash is raw SHA256d output (LE, byte[0]=LSB).
        // meets_target reverses it to BE before comparing with the BE target.
        // Significant byte at position 29 → becomes position 2 in BE → 0x01 < target[2]=0x02
        let mut hash = [0u8; 32];
        hash[29] = 0x01;
        let target = [
            0x00, 0x00, 0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(meets_target(&hash, &target));
    }

    #[test]
    fn test_meets_target_higher() {
        // Significant byte 0x03 at position 29 → BE position 2 → 0x03 > target[2]=0x02
        let mut hash = [0u8; 32];
        hash[29] = 0x03;
        let target = [
            0x00, 0x00, 0x02, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(!meets_target(&hash, &target));
    }

    #[test]
    fn test_version_rolling_mask() {
        let base: u32 = 0x2000_0000;
        let miner_bits: u32 = 0x0001_E000; // within mask
        let result = (base & !VERSION_ROLLING_MASK) | (miner_bits & VERSION_ROLLING_MASK);
        assert_eq!(result & !VERSION_ROLLING_MASK, base & !VERSION_ROLLING_MASK);
        assert_eq!(
            result & VERSION_ROLLING_MASK,
            miner_bits & VERSION_ROLLING_MASK
        );
    }

    #[test]
    fn test_duplicate_share_detection() {
        let mut ss = ShareSet::new();
        assert!(!ss.check_and_insert("job1", b"en2", 12345, 999, 0));
        assert!(ss.check_and_insert("job1", b"en2", 12345, 999, 0));
        // Different nonce should not be a duplicate
        assert!(!ss.check_and_insert("job1", b"en2", 12345, 1000, 0));
        // Different version bits should also not be a duplicate
        assert!(!ss.check_and_insert("job1", b"en2", 12345, 1000, 0x2000));
    }

    #[test]
    fn test_varint_encoding() {
        assert_eq!(encode_varint(0xfc), vec![0xfc]);
        assert_eq!(encode_varint(0xfd), vec![0xfd, 0xfd, 0x00]);
        assert_eq!(encode_varint(0x1234), vec![0xfd, 0x34, 0x12]);
    }
}
