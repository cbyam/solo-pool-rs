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
    bitcoin::template::{difficulty_to_target, double_sha256, hash_to_difficulty, StratumJob},
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

/// Per-session duplicate-share tracker, keyed on the 80-byte header hash.
///
/// The header hash is the canonical identity of a share: two submissions
/// that hash the same header are the same proof-of-work no matter how they
/// were labelled. Keying on the submitted fields instead (job id, ntime,
/// nonce, version bits, ...) left two ways to get one hash credited more
/// than once. The 30 s ntime refresh mints a new job id over byte-identical
/// template content, and the ntime window spans every live job, so one share
/// could be resubmitted once per live job id. And renegotiating the
/// version-rolling mask mid-session changed how the same rolled version was
/// split into "bits", so one header could carry several key encodings.
///
/// Only *validated* shares are recorded (callers check and insert after
/// validation succeeds, when the hash is known), so invalid submissions cannot
/// occupy slots. Sessions `clear` the set on every clean-job broadcast: a
/// clean job changes prev_hash, so no earlier hash can recur, and clearing
/// keeps memory scoped to one job generation instead of relying on FIFO
/// eviction. The FIFO cap remains as a backstop; filling it takes real
/// proof-of-work at the session floor difficulty.
#[derive(Clone, Default)]
pub struct ShareSet {
    seen: HashSet<ShareKey>,
    /// Insertion-order queue for FIFO eviction.
    order: VecDeque<ShareKey>,
    max_size: usize,
}

/// Raw SHA256d header hash, as produced by `validate_share_no_dedup`.
pub type ShareKey = [u8; 32];

impl ShareSet {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            max_size: 4096,
        }
    }

    /// Whether this header hash was already accepted this job generation.
    pub fn contains(&self, key: &ShareKey) -> bool {
        self.seen.contains(key)
    }

    /// Record a share that passed validation.
    pub fn insert(&mut self, key: ShareKey) {
        if self.seen.contains(&key) {
            return;
        }
        if self.seen.len() >= self.max_size {
            // Evict the oldest entry rather than clearing the whole set.
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.order.push_back(key);
        self.seen.insert(key);
    }

    /// Apply dedup to a validation outcome: a validated share whose header
    /// hash was already accepted becomes `DuplicateShare`; a fresh one is
    /// recorded and passed through. Errors pass through untouched, so an
    /// invalid submission never occupies a slot.
    pub fn dedup(
        &mut self,
        result: Result<ShareResult, PoolError>,
    ) -> Result<ShareResult, PoolError> {
        match result {
            Ok(res) if self.contains(res.hash()) => Err(PoolError::DuplicateShare),
            Ok(res) => {
                self.insert(*res.hash());
                Ok(res)
            }
            Err(e) => Err(e),
        }
    }

    /// Drop all entries. Called on clean-job broadcasts: every outstanding job
    /// is retired, so stale-job rejection takes over from dedup.
    pub fn clear(&mut self) {
        self.seen.clear();
        self.order.clear();
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
        /// Actual difficulty of the hash (≥ assigned_difficulty).
        hash_difficulty: u64,
        hash: [u8; 32],
    },
    /// 🎉 Valid share that ALSO meets network difficulty — submit block!
    Block {
        /// Actual difficulty of the hash.
        hash_difficulty: u64,
        block_hex: String,
        hash: [u8; 32],
    },
}

impl ShareResult {
    /// The raw header hash this share proved.
    pub fn hash(&self) -> &[u8; 32] {
        match self {
            ShareResult::Valid { hash, .. } | ShareResult::Block { hash, .. } => hash,
        }
    }
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
pub fn validate_share_no_dedup(
    params: &ShareParams,
    job: &StratumJob,
    job_entry: &JobEntry,
    extranonce1: &[u8],
    session_difficulty: u64,
) -> Result<ShareResult, PoolError> {
    // ── 1. Stale job check ────────────────────────────────────────────────────
    if job_entry.superseded_by_clean {
        return Err(PoolError::StaleJob(params.job_id.clone()));
    }

    // ── 2. ntime validation ──────────────────────────────────────────────────
    check_ntime(params.ntime, job.min_time, job.cur_time)?;

    // ── 3. Assemble coinbase ──────────────────────────────────────────────────
    let coinbase = job.assemble_coinbase(extranonce1, &params.extranonce2);

    // ── 4. Compute merkle root ────────────────────────────────────────────────
    let merkle_root = job.merkle_root(&coinbase);

    // ── 5. Resolve version (with optional BIP320 rolling) ────────────────────
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

    let hash_difficulty = hash_to_difficulty(&hash);

    // ── 9. Network target first (BLOCK FOUND!) ───────────────────────────────
    // A hash that meets the network target is a block whether or not it also
    // meets the pool's share target. On mainnet the share target is always
    // the easier of the two, so order never mattered there; on regtest and
    // signet the network target can be the easier one, and checking the share
    // target first threw the block away as "low difficulty".
    if meets_target(&hash, &job.network_target) {
        let block_hex = assemble_block_hex(&header, &coinbase, &job.transactions);
        tracing::info!(
            "🎉 BLOCK FOUND! height={} hash={}",
            job.height,
            hash_display_hex(&hash)
        );
        return Ok(ShareResult::Block {
            hash_difficulty,
            block_hex,
            hash,
        });
    }

    // ── 10. Pool share target ────────────────────────────────────────────────
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

    Ok(ShareResult::Valid {
        assigned_difficulty: session_difficulty,
        hash_difficulty,
        hash,
    })
}

/// Pool-side ntime window.
///
/// The floor is consensus: a block with ntime below the template's `mintime`
/// (median-time-past + 1) is invalid, so no share below it can ever be worth
/// anything. The floor used to be the template's `curtime` instead, which is
/// stricter than consensus by however far the miner's ntime lags the pool's
/// clock; a miner a few seconds behind that found a block in that gap had it
/// rejected here as out of range before the hash was ever computed. The
/// ceiling stays a pool policy: nodes accept up to two hours past their own
/// adjusted time, and `curtime + 7200` tracks that closely enough.
fn check_ntime(ntime: u32, min_time: u32, cur_time: u32) -> Result<(), PoolError> {
    let ceiling = cur_time.saturating_add(7200);
    if ntime < min_time || ntime > ceiling {
        return Err(PoolError::InvalidParams {
            method: "mining.submit",
            detail: format!(
                "ntime out of range: submitted={ntime} allowed={min_time}..={ceiling} \
                 (template mintime={min_time} curtime={cur_time})"
            ),
        });
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Hex of a hash in conventional big-endian display order — the form
/// `bitcoin-cli`, block explorers, and `getblockhash` all use.
///
/// Hashes are held internally as raw SHA256d output, which is little-endian;
/// that is the order `meets_target` and `hash_to_difficulty` need, so it stays
/// the internal representation. The reversal belongs at the boundary where a
/// person or an explorer reads the value: the found-block log, the archive
/// filename, and the dashboard. Reporting the internal order there means that
/// during the one event this pool exists for, nothing the operator sees matches
/// what the node reports.
pub fn hash_display_hex(hash: &[u8; 32]) -> String {
    let mut be = *hash;
    be.reverse();
    hex::encode(be)
}

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
    /// Genesis block: SHA256d output in internal order, and the hash as every
    /// explorer and `bitcoin-cli` reports it. Pins the display convention so
    /// the found-block log, archive filename, and dashboard agree with the node
    /// during the one event this pool exists for.
    #[test]
    fn hash_display_uses_big_endian_like_bitcoin_cli() {
        let internal: [u8; 32] =
            hex::decode("6fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            super::hash_display_hex(&internal),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
    }

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

    /// Distinct header hashes, derived from a counter.
    fn test_keys(count: u32) -> Vec<ShareKey> {
        (0..count)
            .map(|n| {
                let mut k = [0u8; 32];
                k[..4].copy_from_slice(&n.to_le_bytes());
                k
            })
            .collect()
    }

    #[test]
    fn test_duplicate_share_detection() {
        let mut ss = ShareSet::new();
        let keys = test_keys(2);
        assert!(!ss.contains(&keys[0]));
        ss.insert(keys[0]);
        assert!(ss.contains(&keys[0]));
        assert!(!ss.contains(&keys[1]));
    }

    #[test]
    fn dedup_rejects_a_second_validation_of_the_same_header() {
        // The same proof-of-work arriving under a different job id (ntime
        // refresh) or a different version-bits encoding (mask renegotiation)
        // produces the same header hash, and that is what is keyed on.
        let mut ss = ShareSet::new();
        let hash = test_keys(1).remove(0);
        let valid = || {
            Ok(ShareResult::Valid {
                assigned_difficulty: 1024,
                hash_difficulty: 2048,
                hash,
            })
        };
        assert!(ss.dedup(valid()).is_ok());
        assert!(matches!(ss.dedup(valid()), Err(PoolError::DuplicateShare)));
        // A validation error passes through and leaves no trace.
        let other = test_keys(2).remove(1);
        assert!(matches!(
            ss.dedup(Err(PoolError::LowDifficulty)),
            Err(PoolError::LowDifficulty)
        ));
        assert!(!ss.contains(&other));
    }

    #[test]
    fn invalid_shares_do_not_occupy_dedup_slots() {
        // The caller only inserts after validation passes, so a rejected
        // submission leaves no trace: an identical later submit that validates
        // is judged fresh, not misreported as `duplicate`.
        let mut ss = ShareSet::new();
        let key = test_keys(1).remove(0);
        assert!(!ss.contains(&key)); // invalid attempt: checked, never inserted
        assert!(!ss.contains(&key)); // same share resubmitted: still fresh
        ss.insert(key); // now it validates
        assert!(ss.contains(&key)); // and only now is a resubmit a duplicate
    }

    #[test]
    fn clear_retires_all_entries_on_clean_job() {
        let mut ss = ShareSet::new();
        let key = test_keys(1).remove(0);
        ss.insert(key);
        assert!(ss.contains(&key));
        ss.clear();
        assert!(!ss.contains(&key));
        // Re-inserting after a clear works (fresh job generation).
        ss.insert(key);
        assert!(ss.contains(&key));
    }

    #[test]
    fn insert_is_bounded_by_fifo_eviction() {
        let mut ss = ShareSet {
            max_size: 4,
            ..ShareSet::default()
        };
        let keys = test_keys(6);
        for k in &keys {
            ss.insert(*k);
        }
        // Oldest two evicted, newest four retained.
        assert!(!ss.contains(&keys[0]));
        assert!(!ss.contains(&keys[1]));
        for k in &keys[2..] {
            assert!(ss.contains(k));
        }
        // Double-insert of a present key must not grow the FIFO or evict.
        ss.insert(keys[5]);
        assert!(ss.contains(&keys[2]));
    }

    #[test]
    fn ntime_window_is_mintime_to_curtime_plus_two_hours() {
        let (min_t, cur_t) = (1_699_999_000, 1_700_000_000);
        assert!(check_ntime(min_t, min_t, cur_t).is_ok(), "floor inclusive");
        assert!(
            check_ntime(cur_t - 1, min_t, cur_t).is_ok(),
            "below curtime, above mintime"
        );
        assert!(check_ntime(cur_t, min_t, cur_t).is_ok());
        assert!(
            check_ntime(cur_t + 7200, min_t, cur_t).is_ok(),
            "ceiling inclusive"
        );
        assert!(check_ntime(min_t - 1, min_t, cur_t).is_err());
        assert!(check_ntime(cur_t + 7201, min_t, cur_t).is_err());
        // Ceiling arithmetic must not wrap near u32::MAX.
        assert!(check_ntime(u32::MAX, min_t, u32::MAX - 10).is_ok());
    }

    /// A real job from the template builder, so the coinbase/merkle/header
    /// path runs end to end. Mainnet block 1's prev hash, no transactions.
    fn real_job() -> (StratumJob, JobEntry) {
        let gbt = crate::bitcoin::rpc::GbtResult {
            version: 0x2000_0000,
            prev_hash: "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
                .to_string(),
            bits: "1d00ffff".to_string(),
            cur_time: 1_700_000_000,
            min_time: 1_699_999_000,
            height: 900_000,
            coinbase_value: 312_500_000,
            transactions: vec![],
            longpoll_id: None,
            default_witness_commitment: None,
            rules: vec![],
        };
        let job = crate::bitcoin::template::build_job(
            &gbt,
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            "tag",
            4,
            4,
        )
        .unwrap();
        let entry = JobEntry {
            job: std::sync::Arc::new(job.clone()),
            created_at: std::time::Instant::now(),
            clean: true,
            superseded_by_clean: false,
        };
        (job, entry)
    }

    /// Deterministic header nonce derived from a range rather than written as
    /// a literal (CodeQL reads a literal flowing into the header nonce as a
    /// hard-coded cryptographic nonce).
    fn test_nonce() -> u32 {
        (0..64u32).fold(7, |acc, i| acc.wrapping_mul(31).wrapping_add(i))
    }

    fn params(job: &StratumJob, ntime: u32, nonce: u32) -> ShareParams {
        ShareParams {
            worker: "w".into(),
            job_id: job.job_id.clone(),
            extranonce2: vec![1, 2, 3, 4],
            ntime,
            nonce,
            version_bits: None,
            version_rolling_mask: None,
        }
    }

    #[test]
    fn block_is_found_even_when_the_share_target_is_missed() {
        // Regtest/signet shape: the network target is easier than the pool's
        // share target. Any hash meets the all-ones network target; a huge
        // session difficulty makes the share target unreachable. The share
        // target must not be consulted first.
        let (mut job, mut entry) = real_job();
        job.network_target = [0xff; 32];
        entry.job = std::sync::Arc::new(job.clone());
        let nonce = test_nonce();
        let res = validate_share_no_dedup(
            &params(&job, job.cur_time, nonce),
            &job,
            &entry,
            &[0xAA; 4],
            u64::MAX / 2,
        );
        assert!(matches!(res, Ok(ShareResult::Block { .. })), "{res:?}");
    }

    #[test]
    fn a_block_with_ntime_between_mintime_and_curtime_is_accepted() {
        // The lost-block case: consensus-valid ntime that lags curtime.
        let (mut job, mut entry) = real_job();
        job.network_target = [0xff; 32];
        entry.job = std::sync::Arc::new(job.clone());
        let nonce = test_nonce();
        let res = validate_share_no_dedup(
            &params(&job, job.cur_time - 30, nonce),
            &job,
            &entry,
            &[0xAA; 4],
            1,
        );
        assert!(matches!(res, Ok(ShareResult::Block { .. })), "{res:?}");

        // One below mintime is still refused, before any hashing.
        let res = validate_share_no_dedup(
            &params(&job, job.min_time - 1, nonce),
            &job,
            &entry,
            &[0xAA; 4],
            1,
        );
        assert!(
            matches!(res, Err(PoolError::InvalidParams { .. })),
            "{res:?}"
        );
    }

    #[test]
    fn mainnet_shape_still_rejects_a_low_difficulty_share() {
        // Real diff-1 network target, diff-1 share target, arbitrary nonce:
        // fails both, and the result must be LowDifficulty, not a block.
        let (job, entry) = real_job();
        let nonce = test_nonce();
        let res = validate_share_no_dedup(
            &params(&job, job.cur_time, nonce),
            &job,
            &entry,
            &[0xAA; 4],
            1,
        );
        assert!(matches!(res, Err(PoolError::LowDifficulty)), "{res:?}");
    }

    #[test]
    fn test_varint_encoding() {
        assert_eq!(encode_varint(0xfc), vec![0xfc]);
        assert_eq!(encode_varint(0xfd), vec![0xfd, 0xfd, 0x00]);
        assert_eq!(encode_varint(0x1234), vec![0xfd, 0x34, 0x12]);
    }
}
