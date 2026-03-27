/// protocol/sv2_stub.rs
///
/// SV2 migration shim — placeholder interfaces that will be wired to the
/// `stratum-mining/stratum` SRI crates when your miners support SV2.
///
/// The trait `StratumFrontend` abstracts over SV1 and SV2 so that the
/// mining engine (template engine, validator, vardiff) needs no changes
/// when you flip the protocol layer.
use crate::bitcoin::template::StratumJob;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Protocol abstraction trait
// ─────────────────────────────────────────────────────────────────────────────

/// Implemented by both the SV1 session handler and (eventually) the SV2 handler.
/// The mining core only calls methods on this trait — it is protocol-agnostic.
#[allow(dead_code)]
#[async_trait::async_trait]
pub trait StratumFrontend: Send + Sync {
    /// Send a new job to the miner.
    async fn send_job(&self, job: Arc<StratumJob>, clean: bool);

    /// Update the miner's share difficulty.
    async fn set_difficulty(&self, difficulty: u64);

    /// Worker name (for logging / metrics).
    fn worker_name(&self) -> &str;
}

// ─────────────────────────────────────────────────────────────────────────────
// SV2 migration notes (to be removed once wired up)
// ─────────────────────────────────────────────────────────────────────────────
//
// Phase 1 (current): SV1 session implements StratumFrontend.
//
// Phase 2 (SV2 ready):
//   1. Add to Cargo.toml:
//        sv1-api     = { git = "https://github.com/stratum-mining/stratum", package = "sv1-api" }
//        stratum-core = { git = "https://github.com/stratum-mining/stratum" }
//
//   2. Introduce an SV2Session struct that:
//        - Handles the Noise handshake (codec-sv2 / noise-sv2)
//        - Speaks the Mining Protocol (subprotocols/mining-sv2)
//        - Implements StratumFrontend
//
//   3. For miners that can't be upgraded, deploy the SRI Translator Proxy
//      (sv2-apps/miner-apps/translator) in front of this pool:
//
//         Legacy ASICs → [Translator Proxy (SV1→SV2)] → [solo-pool-rs SV2 listener]
//
//   4. The server.rs accept loop can detect the protocol from the first bytes
//      of the TCP stream (SV2 starts with a Noise handshake, SV1 is plain JSON).
//
// Key SRI crates for SV2:
//   - codec-sv2       : Noise-encrypted framing
//   - binary-sv2      : Binary message encoding
//   - parsers-sv2     : High-level message parsing
//   - handlers_sv2    : Trait-based message dispatching
//   - channels-sv2    : Standard/Extended channel management
//   - stratum-translation : V1 ↔ V2 coinbase/job translation
