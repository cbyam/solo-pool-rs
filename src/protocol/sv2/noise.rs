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
//! noise codec for the encrypted transport. The finished handshake splits into
//! independent encrypt and decrypt halves, so the reader task and the session's
//! writer each own theirs and share no lock.
//!
//! The certificate is signed by the pool's authority key. By default that key
//! persists in `[sv2] authority_key_file` so the base58check-encoded public
//! key (logged at startup, shown on the dashboard Settings page) can be pinned
//! in the miner's configuration and survives restarts. Setting
//! `persist_authority_key = false` reverts to a fresh key per process, which
//! any pinning miner will reject after a restart.
use anyhow::{anyhow, Context, Result};
use codec_sv2::{
    Decrypted, Handshake, NoiseDecoder, NoiseEncoder, Transport, TransportDecryptState,
    TransportEncryptState,
};
use framing_sv2::framing::SerializedFrame;
use key_utils::{Secp256k1PublicKey, Secp256k1SecretKey};
use noise_sv2::{Responder, ELLSWIFT_ENCODING_SIZE};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::sync::OnceLock;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    net::TcpStream,
};
use tracing::warn;

use super::messages;
use crate::config::Sv2Config;

/// Process-wide authority key + certificate validity, set once by [`init`] at
/// boot. Falls back to an ephemeral key with the default validity if [`init`]
/// was never called (unit tests, defensive).
struct Authority {
    secret: [u8; 32],
    cert_validity_secs: u32,
}

static AUTHORITY: OnceLock<Authority> = OnceLock::new();

/// Initialise the process-wide Noise authority from config: load the secret
/// key from `authority_key_file` (creating it on first start) when
/// `persist_authority_key` is true, otherwise generate an ephemeral key.
/// Returns the base58check-encoded authority public key (the string a miner
/// pins to verify pool identity).
pub fn init(cfg: &Sv2Config) -> Result<String> {
    let secret = if cfg.persist_authority_key {
        load_or_generate_secret(&crate::config::expand_tilde(&cfg.authority_key_file))?
    } else {
        generate_secret()
    };
    let authority = Authority {
        secret,
        cert_validity_secs: cfg.cert_validity_secs,
    };
    let authority = AUTHORITY.get_or_init(|| authority);
    Ok(encode_public_key(&authority.secret))
}

fn authority() -> &'static Authority {
    AUTHORITY.get_or_init(|| Authority {
        secret: generate_secret(),
        cert_validity_secs: 365 * 24 * 60 * 60,
    })
}

fn generate_secret() -> [u8; 32] {
    let secp = Secp256k1::new();
    let (sk, _pk) = secp.generate_keypair(&mut rand::thread_rng());
    sk.secret_bytes()
}

/// Base58check encoding of the authority public key, in the SRI `key-utils`
/// format miners expect (2-byte version prefix + 32-byte x-only key).
fn encode_public_key(secret: &[u8; 32]) -> String {
    let sk = SecretKey::from_slice(secret).expect("valid authority secret");
    Secp256k1PublicKey::from(Secp256k1SecretKey(sk)).to_string()
}

/// Read the base58check secret key from `path`, or generate one and write it
/// there (owner-only permissions) if the file does not exist yet — the same
/// create-on-first-use pattern as bitcoind's `.cookie`.
fn load_or_generate_secret(path: &std::path::Path) -> Result<[u8; 32]> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key: Secp256k1SecretKey = contents
                .trim()
                .parse()
                .map_err(|e| anyhow!("{e:?}"))
                .with_context(|| {
                    format!(
                        "Parsing SV2 authority key file {} (base58check secret key). \
                         Delete the file to generate a fresh key.",
                        path.display()
                    )
                })?;
            Ok(key.into_bytes())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let secret = generate_secret();
            let sk = SecretKey::from_slice(&secret).expect("valid generated secret");
            let encoded = Secp256k1SecretKey(sk).to_string();
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("Creating directory {}", dir.display()))?;
            }
            write_owner_only(path, &encoded)
                .with_context(|| format!("Writing SV2 authority key file {}", path.display()))?;
            tracing::info!("Generated new SV2 authority key: {}", path.display());
            Ok(secret)
        }
        Err(e) => {
            Err(e).with_context(|| format!("Reading SV2 authority key file {}", path.display()))
        }
    }
}

/// Sibling scratch file for an in-progress key write.
fn key_tmp_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Write the key so that `path` only ever holds a complete key or nothing.
///
/// The contents go to a sibling `.tmp` (owner-only, fsynced) and are then
/// linked into place. A crash between create and write used to leave a
/// zero-length or partial file at `path`, which parses as a corrupt key and
/// fails every later boot until someone deletes it by hand. `hard_link`
/// rather than `rename` so an existing key is never overwritten: the caller
/// only writes after seeing NotFound, and this keeps that check honest
/// against a racing writer.
#[cfg(unix)]
fn write_owner_only(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = key_tmp_path(path);
    // A leftover from an earlier interrupted write is not a key; clear it so
    // create_new below succeeds.
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        link_into_place(&tmp, path)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Publish `tmp` at `path` without overwriting. `hard_link` refuses an
/// existing target atomically; on a filesystem that cannot link (some
/// container bind mounts) fall back to `rename`, which the kernel still
/// applies atomically.
fn link_into_place(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => std::fs::rename(tmp, path),
    }
}

#[cfg(not(unix))]
fn write_owner_only(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let tmp = key_tmp_path(path);
    let _ = std::fs::remove_file(&tmp);
    let result =
        std::fs::write(&tmp, format!("{contents}\n")).and_then(|_| link_into_place(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Generic reader/handshake failure. Deliberately not `InvalidData`: the SV2
/// session bans on that kind, and a decrypt failure or a truncated stream is
/// a broken peer, not an abusive one.
fn io_err(msg: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(msg.to_string())
}

/// The one reader failure that earns a ban: a frame larger than the
/// configured maximum. Same kind the SV1 line reader uses for an oversize
/// line, so both protocols take the same path in the session.
fn oversize_err(at_least: usize, max_frame: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("SV2 frame too large: at least {at_least} > {max_frame} bytes"),
    )
}

/// Run the responder side of the Noise handshake on the raw stream, returning
/// the [`Transport`] to split into encrypted reader and writer halves.
///
/// Wire sequence (no length prefixes — fixed sizes):
///   initiator → 64-byte ElligatorSwift ephemeral key
///   responder → `INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE`-byte reply
pub async fn responder_handshake(stream: &mut TcpStream) -> Result<Transport> {
    let auth = authority();
    responder_handshake_with(stream, &auth.secret, auth.cert_validity_secs).await
}

async fn responder_handshake_with(
    stream: &mut TcpStream,
    secret: &[u8; 32],
    cert_validity_secs: u32,
) -> Result<Transport> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(secret).expect("valid authority secret");
    let kp = Keypair::from_secret_key(&secp, &sk);
    let responder = Responder::new(kp, cert_validity_secs);

    let mut re_pub = [0u8; ELLSWIFT_ENCODING_SIZE];
    stream.read_exact(&mut re_pub).await?;

    let (response, transport) = Handshake::responder(responder)
        .step_1(re_pub)
        .map_err(|e| anyhow!("noise handshake step_1 failed: {e:?}"))?;

    stream.write_all(response.payload()).await?;
    stream.flush().await?;

    Ok(transport)
}

/// Encrypted SV2 frame reader (owns the read half, the decoder and the
/// decrypt half of the transport).
pub struct NoiseReader {
    reader: OwnedReadHalf,
    decoder: NoiseDecoder,
    /// Handed to the decoder for each step and handed back with its result.
    /// A decode error consumes it, and the connection cannot be read again.
    state: Option<TransportDecryptState>,
    /// Upper bound on the encrypted bytes one frame may take before the
    /// connection is refused. The SV2 frame header carries an attacker-chosen
    /// u24 length (up to ~16 MB); without this cap the reader would pull in and
    /// AEAD-decrypt that whole frame before the post-decode
    /// `check_message_size` ever runs. Set from `security.max_message_bytes`
    /// plus framing/AEAD overhead.
    max_frame: usize,
    /// Encrypted bytes already read toward the frame being decoded.
    frame_read: usize,
}

impl NoiseReader {
    pub fn new(reader: OwnedReadHalf, state: TransportDecryptState, max_frame: usize) -> Self {
        Self {
            reader,
            decoder: NoiseDecoder::new(),
            state: Some(state),
            max_frame,
            frame_read: 0,
        }
    }

    /// Read, decrypt and frame one SV2 message, returning `(msg_type, payload)`.
    pub async fn read(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        loop {
            let state = self
                .state
                .take()
                .ok_or_else(|| io_err("noise transport unusable after a decode error"))?;
            match self.decoder.next_transport_frame(state) {
                Ok(Decrypted::Frame(mut frame, state)) => {
                    self.state = Some(state);
                    self.frame_read = 0;
                    let msg_type = frame.header().msg_type();
                    return Ok((msg_type, frame.payload().to_vec()));
                }
                Ok(Decrypted::Incomplete(missing, state)) => {
                    self.state = Some(state);
                    // `missing` is the next read, at most one 64 KB chunk,
                    // not the length the header declared, so the guard counts
                    // what the frame has taken so far. Below one chunk it
                    // refuses on the header alone, as before; above it, within
                    // one chunk of the cap. Either way it refuses before
                    // `writable()` grows the buffer for the read.
                    let total = self.frame_read.saturating_add(missing);
                    if total > self.max_frame {
                        return Err(oversize_err(total, self.max_frame));
                    }
                    let writable = self.decoder.writable();
                    self.reader.read_exact(writable).await?;
                    self.frame_read = total;
                }
                Err(e) => return Err(io_err(format!("noise decode error: {e:?}"))),
            }
        }
    }
}

/// Encrypted SV2 frame writer (owns the write half, the encoder and the
/// encrypt half of the transport).
pub struct NoiseWriter {
    writer: OwnedWriteHalf,
    encoder: NoiseEncoder,
    state: TransportEncryptState,
    peer: std::net::SocketAddr,
}

impl NoiseWriter {
    pub fn new(
        writer: OwnedWriteHalf,
        state: TransportEncryptState,
        peer: std::net::SocketAddr,
    ) -> Self {
        Self {
            writer,
            encoder: NoiseEncoder::new(),
            state,
            peer,
        }
    }

    /// Frame, encrypt and write one SV2 message. Returns false on any error so
    /// the session can disconnect (mirrors the SV1 `send_messages` contract).
    pub async fn send(&mut self, msg_type: u8, channel_msg: bool, payload: &[u8]) -> bool {
        tracing::trace!(peer = %self.peer, msg_type, len = payload.len(), "→ sv2 pool (noise)");

        let full = messages::frame_bytes(msg_type, channel_msg, payload);
        // Checks the header against the bytes, which catches a payload too
        // long for the u24 length field.
        let frame = match SerializedFrame::from_bytes(full) {
            Ok(f) => f,
            Err(e) => {
                warn!(
                    "SV2 frame to {} does not match its header: {e:?}",
                    self.peer
                );
                return false;
            }
        };

        let encrypted = match self.encoder.encode_transport(frame, &mut self.state) {
            Ok(b) => b,
            Err(e) => {
                warn!("SV2 noise encode error to {}: {e:?}", self.peer);
                return false;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use noise_sv2::{Initiator, INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE};
    use tokio::net::TcpListener;

    fn authority_pubkey_bytes(secret: &[u8; 32]) -> [u8; 32] {
        let sk = SecretKey::from_slice(secret).unwrap();
        Secp256k1PublicKey::from(Secp256k1SecretKey(sk)).into_bytes()
    }

    /// Drive a full handshake against `responder_handshake_with`, acting as a
    /// miner. `now_offset_secs` shifts the initiator's clock when it checks the
    /// certificate validity window (a stand-in for device clock skew).
    async fn initiator_handshake(
        mut initiator: Box<Initiator>,
        responder_secret: [u8; 32],
        cert_validity_secs: u32,
        now_offset_secs: i64,
    ) -> Result<(), noise_sv2::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            responder_handshake_with(&mut sock, &responder_secret, cert_validity_secs)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let first = initiator.step_0().unwrap();
        client.write_all(&first).await.unwrap();
        let mut reply = [0u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE];
        client.read_exact(&mut reply).await.unwrap();

        let now = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + now_offset_secs) as u32;
        let result = initiator.step_2_with_now(reply, now).map(|_| ());
        server.await.unwrap().unwrap();
        result
    }

    #[tokio::test]
    async fn pinning_initiator_accepts_the_pool_certificate() {
        let secret = generate_secret();
        let initiator = Initiator::from_raw_k(authority_pubkey_bytes(&secret)).unwrap();
        initiator_handshake(initiator, secret, 3600, 0)
            .await
            .expect("handshake with the correct pinned authority key");
    }

    #[tokio::test]
    async fn pinning_initiator_rejects_a_wrong_authority_key() {
        let secret = generate_secret();
        let other = generate_secret();
        let initiator = Initiator::from_raw_k(authority_pubkey_bytes(&other)).unwrap();
        let err = initiator_handshake(initiator, secret, 3600, 0)
            .await
            .expect_err("certificate signed by a different authority must be rejected");
        assert!(matches!(err, noise_sv2::Error::InvalidCertificate(_)));
    }

    #[tokio::test]
    async fn non_pinning_initiator_connects_without_the_key() {
        let secret = generate_secret();
        let initiator = Initiator::without_pk().unwrap();
        initiator_handshake(initiator, secret, 3600, 0)
            .await
            .expect("handshake without identity pinning");
    }

    #[tokio::test]
    async fn certificate_outside_its_validity_window_is_rejected() {
        // Initiator clock 60 s past a 1 s validity window — well outside the
        // 10 s clock-drift leeway SRI's verifier grants (stratum issue #2015).
        let secret = generate_secret();
        let initiator = Initiator::from_raw_k(authority_pubkey_bytes(&secret)).unwrap();
        let err = initiator_handshake(initiator, secret, 1, 60)
            .await
            .expect_err("expired certificate must be rejected");
        assert!(matches!(err, noise_sv2::Error::InvalidCertificate(_)));
    }

    /// Bring up a real Noise transport between a client `NoiseWriter` and a
    /// pool-side `NoiseReader` with the given frame cap.
    pub(crate) async fn transport_pair(max_frame: usize) -> (NoiseWriter, NoiseReader) {
        let secret = generate_secret();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let transport = responder_handshake_with(&mut sock, &secret, 3600)
                .await
                .unwrap();
            (sock, transport)
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let initiator = Initiator::without_pk().unwrap();
        let (first, sent) = Handshake::initiator(initiator).step_0().unwrap();
        client.write_all(first.payload()).await.unwrap();
        let mut reply = [0u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE];
        client.read_exact(&mut reply).await.unwrap();
        let (client_enc, _client_dec) = sent.step_2(reply).unwrap().split();
        let (_client_rx, client_tx) = client.into_split();
        let writer = NoiseWriter::new(client_tx, client_enc, addr);

        let (sock, transport) = server.await.unwrap();
        let (_server_enc, server_dec) = transport.split();
        let (server_rx, _server_tx) = sock.into_split();
        let reader = NoiseReader::new(server_rx, server_dec, max_frame);
        (writer, reader)
    }

    #[tokio::test]
    async fn frame_within_cap_is_delivered() {
        let (mut writer, mut reader) = transport_pair(4096).await;
        let payload = vec![0xAB; 1000];
        assert!(writer.send(0x1F, true, &payload).await);
        let (msg_type, got) = reader.read().await.unwrap();
        assert_eq!(msg_type, 0x1F);
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn frame_over_cap_is_refused_as_invalid_data() {
        // The header declares a payload far over max_frame. The reader must
        // reject on the declared length with the kind the session bans on,
        // rather than allocating and reading the payload first.
        let (mut writer, mut reader) = transport_pair(4096).await;
        let payload = vec![0xAB; 200_000];
        assert!(writer.send(0x1F, true, &payload).await);
        let err = reader.read().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn a_cap_above_one_chunk_still_refuses_an_oversize_frame() {
        // The decoder asks for at most one 64 KB chunk per read, so a cap
        // above that cannot be checked against a single request. The reader
        // counts what the frame has taken instead and refuses once it passes
        // the cap, well before the 200 KB frame has been read.
        let (mut writer, mut reader) = transport_pair(100_000).await;
        let payload = vec![0xAB; 200_000];
        assert!(writer.send(0x1F, true, &payload).await);
        let err = reader.read().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn multi_chunk_frames_within_the_cap_are_each_delivered() {
        // Each 80 KB frame spans two chunks and fits the cap on its own, but
        // two of them together do not: the count must restart per frame.
        let (mut writer, mut reader) = transport_pair(100_000).await;
        let first = vec![0x11; 80_000];
        let second = vec![0x22; 80_000];
        assert!(writer.send(0x1F, true, &first).await);
        assert!(writer.send(0x20, true, &second).await);
        assert_eq!(reader.read().await.unwrap(), (0x1F, first));
        assert_eq!(reader.read().await.unwrap(), (0x20, second));
    }

    fn temp_key_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("solo-pool-noise-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn authority_key_file_round_trips() {
        let path = temp_key_path("roundtrip.key");
        std::fs::remove_file(&path).ok();

        let first = load_or_generate_secret(&path).unwrap();
        let second = load_or_generate_secret(&path).unwrap();
        assert_eq!(first, second, "second load must return the persisted key");

        // The file holds one base58check line in the SRI key-utils format.
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Secp256k1SecretKey = contents.trim().parse().unwrap();
        assert_eq!(parsed.into_bytes(), first);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be owner-only");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn key_write_leaves_no_scratch_file_and_survives_a_stale_one() {
        let path = temp_key_path("atomic.key");
        let tmp = key_tmp_path(&path);
        std::fs::remove_file(&path).ok();
        // Debris from a write interrupted before it reached the key path.
        std::fs::write(&tmp, "partial").unwrap();

        let secret = load_or_generate_secret(&path).unwrap();
        assert!(!tmp.exists(), "scratch file must be gone after a write");
        assert!(path.exists());
        // The key that landed is the one returned, not the debris.
        assert_eq!(load_or_generate_secret(&path).unwrap(), secret);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn key_write_never_overwrites_an_existing_file() {
        // Simulates a racing writer landing between the NotFound check and
        // the write: the existing key must win and the call must fail.
        let path = temp_key_path("existing.key");
        std::fs::write(&path, "keep-me\n").unwrap();
        let err = write_owner_only(&path, "new-key").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep-me\n");
        assert!(!key_tmp_path(&path).exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupt_authority_key_file_fails_loudly() {
        let path = temp_key_path("corrupt.key");
        std::fs::write(&path, "not-a-key\n").unwrap();
        let err = load_or_generate_secret(&path).unwrap_err();
        assert!(err.to_string().contains("Parsing SV2 authority key file"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn encoded_public_key_parses_in_the_key_utils_format() {
        let secret = generate_secret();
        let encoded = encode_public_key(&secret);
        let parsed: Secp256k1PublicKey = encoded.parse().unwrap();
        assert_eq!(parsed.into_bytes(), authority_pubkey_bytes(&secret));
    }
}
