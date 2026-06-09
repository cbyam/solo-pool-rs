//! protocol/sv2/noise.rs
//!
//! Stratum V2 Noise transport (pool = responder).
//!
//! SV2 secures the connection with the `Noise_NX_Secp256k1+EllSwift_ChaChaPoly_SHA256`
//! handshake: the initiator (miner) sends a 64-byte ElligatorSwift ephemeral key,
//! the responder (pool) replies with its ephemeral + encrypted static key + a
//! signed certificate, after which all frames are AEAD-encrypted.
//!
//! Devices such as the NerdQAxe++ require this — they will not speak plaintext
//! SV2. We use the SRI `noise_sv2` responder for the handshake and `codec_sv2`'s
//! noise codec for the encrypted transport.
//!
//! The miner does not verify the pool's identity ("No authority pubkey
//! configured" in the device log), so the authority keypair is generated fresh
//! per process — no operator key management is required. (A configurable,
//! persistent authority key can be added later if identity pinning is wanted.)
use anyhow::{anyhow, Result};
use codec_sv2::{NoiseEncoder, StandardNoiseDecoder, State};
use framing_sv2::framing::{Frame, Sv2Frame};
use noise_sv2::{Responder, ELLSWIFT_ENCODING_SIZE};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::sync::{Arc, OnceLock};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    net::TcpStream,
    sync::Mutex,
};
use tracing::warn;

use super::messages;

/// Certificate validity advertised to the miner (seconds). The device does not
/// pin our identity, so this only needs to be comfortably in the future.
const CERT_VALIDITY_SECS: u32 = 365 * 24 * 60 * 60;

/// Phantom message type for the codec generics. The decoder never deserializes
/// into it (we read raw payload bytes) and the encoder is fed already-serialized
/// frames, so any no-lifetime SV2 message that implements the codec bounds works.
type Marker = mining_sv2::SubmitSharesSuccess;
type Decoder = StandardNoiseDecoder<Marker>;
type Encoder = NoiseEncoder<Marker>;

/// Process-wide authority secret key (32 bytes), generated once on first use.
static AUTHORITY_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

fn authority_secret() -> &'static [u8; 32] {
    AUTHORITY_SECRET.get_or_init(|| {
        let secp = Secp256k1::new();
        let (sk, _pk) = secp.generate_keypair(&mut rand::thread_rng());
        sk.secret_bytes()
    })
}

fn io_err(msg: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

/// Run the responder side of the Noise handshake on the raw stream, returning a
/// transport-mode codec [`State`] ready for encrypted framing.
///
/// Wire sequence (no length prefixes — fixed sizes):
///   initiator → 64-byte ElligatorSwift ephemeral key
///   responder → `INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE`-byte reply
pub async fn responder_handshake(stream: &mut TcpStream) -> Result<State> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(authority_secret()).expect("valid authority secret");
    let kp = Keypair::from_secret_key(&secp, &sk);
    let mut responder = Responder::new(kp, CERT_VALIDITY_SECS);

    let mut re_pub = [0u8; ELLSWIFT_ENCODING_SIZE];
    stream.read_exact(&mut re_pub).await?;

    let (response, codec) = responder
        .step_1(re_pub)
        .map_err(|e| anyhow!("noise handshake step_1 failed: {e:?}"))?;

    stream.write_all(&response).await?;
    stream.flush().await?;

    Ok(State::with_transport_mode(codec))
}

/// Encrypted SV2 frame reader (owns the read half + decoder; shares cipher
/// state with the writer via `state`).
pub struct NoiseReader {
    reader: OwnedReadHalf,
    decoder: Decoder,
    state: Arc<Mutex<State>>,
    /// Upper bound on the bytes we will read for a single frame chunk before
    /// rejecting the connection. The SV2 frame header carries an attacker-chosen
    /// u24 length (up to ~16 MB); without this cap the decoder would allocate and
    /// `read_exact` that whole frame (and AEAD-decrypt it) before the post-decode
    /// `check_message_size` ever runs. Set from `security.max_message_bytes` plus
    /// framing/AEAD overhead.
    max_frame: usize,
}

impl NoiseReader {
    pub fn new(reader: OwnedReadHalf, state: Arc<Mutex<State>>, max_frame: usize) -> Self {
        Self {
            reader,
            decoder: Decoder::new(),
            state,
            max_frame,
        }
    }

    /// Read, decrypt and frame one SV2 message, returning `(msg_type, payload)`.
    pub async fn read(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        loop {
            let decoded = {
                let mut st = self.state.lock().await;
                self.decoder.next_frame(&mut st)
            };
            match decoded {
                Ok(Frame::Sv2(mut frame)) => {
                    let header = frame
                        .get_header()
                        .ok_or_else(|| io_err("SV2 frame missing header"))?;
                    let payload = frame.payload().to_vec();
                    return Ok((header.msg_type(), payload));
                }
                Ok(Frame::HandShake(_)) => {
                    return Err(io_err("unexpected handshake frame in transport mode"))
                }
                Err(codec_sv2::Error::MissingBytes(_)) => {
                    let writable = self.decoder.writable();
                    if writable.len() > self.max_frame {
                        return Err(io_err(format!(
                            "SV2 frame too large: {} > {} bytes",
                            writable.len(),
                            self.max_frame
                        )));
                    }
                    self.reader.read_exact(writable).await?;
                }
                Err(e) => return Err(io_err(format!("noise decode error: {e:?}"))),
            }
        }
    }
}

/// Encrypted SV2 frame writer (owns the write half + encoder; shares cipher
/// state with the reader via `state`).
pub struct NoiseWriter {
    writer: OwnedWriteHalf,
    encoder: Encoder,
    state: Arc<Mutex<State>>,
    peer: std::net::SocketAddr,
}

impl NoiseWriter {
    pub fn new(
        writer: OwnedWriteHalf,
        state: Arc<Mutex<State>>,
        peer: std::net::SocketAddr,
    ) -> Self {
        Self {
            writer,
            encoder: Encoder::new(),
            state,
            peer,
        }
    }

    /// Frame, encrypt and write one SV2 message. Returns false on any error so
    /// the session can disconnect (mirrors the SV1 `send_messages` contract).
    pub async fn send(&mut self, msg_type: u8, channel_msg: bool, payload: &[u8]) -> bool {
        tracing::trace!(peer = %self.peer, msg_type, len = payload.len(), "→ sv2 pool (noise)");

        let full = messages::frame_bytes(msg_type, channel_msg, payload);
        let frame: Sv2Frame<Marker, Vec<u8>> = Sv2Frame::from_bytes_unchecked(full);
        let item = Frame::Sv2(frame);

        let encrypted = {
            let mut st = self.state.lock().await;
            match self.encoder.encode(item, &mut st) {
                Ok(b) => b,
                Err(e) => {
                    warn!("SV2 noise encode error to {}: {e:?}", self.peer);
                    return false;
                }
            }
        };

        if let Err(e) = self.writer.write_all(encrypted.as_ref()).await {
            warn!("SV2 write error to {}: {e}", self.peer);
            return false;
        }
        if let Err(e) = self.writer.flush().await {
            warn!("SV2 flush error to {}: {e}", self.peer);
            return false;
        }
        true
    }
}
