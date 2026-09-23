//! Throwaway SV2 **Noise** smoke client — verifies the pool's encrypted Stratum
//! V2 handshake and job delivery end-to-end against a real `getblocktemplate`.
//! Mirrors what a NerdQAxe++ does (initiator that does not verify pool identity).
//!
//!   cargo run --example sv2_smoke_client -- 127.0.0.1:13335
//!
//! Drives: Noise handshake → SetupConnection → OpenExtendedMiningChannel, then
//! reads the pushed OpenExtendedMiningChannelSuccess / NewExtendedMiningJob /
//! SetNewPrevHash and prints a summary. Not part of the shipping pool.
use binary_sv2::{Str0255Owned, U256Owned};
use codec_sv2::{
    Decrypted, Handshake, NoiseDecoder, NoiseEncoder, TransportDecryptState, TransportEncryptState,
};
use common_messages_sv2::{Protocol, SetupConnectionOwned};
use framing_sv2::framing::SerializedFrame;
use mining_sv2::{NewExtendedMiningJob, OpenExtendedMiningChannelOwned, SetNewPrevHash};
use noise_sv2::{Initiator, ELLSWIFT_ENCODING_SIZE, INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HDR: usize = 6;

fn frame_bytes(msg_type: u8, channel_msg: bool, payload: &[u8]) -> Vec<u8> {
    let ext: u16 = if channel_msg { 0x8000 } else { 0 };
    let len = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(HDR + payload.len());
    out.extend_from_slice(&ext.to_le_bytes());
    out.push(msg_type);
    out.extend_from_slice(&len[0..3]);
    out.extend_from_slice(payload);
    out
}

async fn send(
    s: &mut TcpStream,
    enc: &mut NoiseEncoder,
    st: &mut TransportEncryptState,
    mt: u8,
    ch: bool,
    payload: &[u8],
) {
    let frame = SerializedFrame::from_bytes(frame_bytes(mt, ch, payload)).expect("frame");
    let bytes = enc.encode_transport(frame, st).expect("encode");
    s.write_all(bytes.as_ref()).await.unwrap();
    s.flush().await.unwrap();
}

async fn recv(
    s: &mut TcpStream,
    dec: &mut NoiseDecoder,
    st: &mut Option<TransportDecryptState>,
) -> (u8, Vec<u8>) {
    loop {
        match dec.next_transport_frame(st.take().expect("transport state")) {
            Ok(Decrypted::Frame(mut f, state)) => {
                *st = Some(state);
                let mt = f.header().msg_type();
                return (mt, f.payload().to_vec());
            }
            Ok(Decrypted::Incomplete(_, state)) => {
                *st = Some(state);
                let w = dec.writable();
                s.read_exact(w).await.unwrap();
            }
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
}

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:13335".to_string());
    let mut s = TcpStream::connect(&addr).await.expect("connect");
    println!("connected to {addr}");

    // ── Noise handshake (initiator, no identity verification) ────────────────
    let initiator = Initiator::without_pk().expect("initiator");
    let (first, sent) = Handshake::initiator(initiator).step_0().expect("step_0");
    assert_eq!(first.payload().len(), ELLSWIFT_ENCODING_SIZE);
    s.write_all(first.payload()).await.unwrap();
    s.flush().await.unwrap();

    let mut reply = [0u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE];
    s.read_exact(&mut reply).await.unwrap();
    let transport = sent.step_2(reply).expect("step_2");
    println!("Noise handshake complete ✅ (encrypted transport established)");

    let (mut enc_state, dec_state) = transport.split();
    let mut dec_state = Some(dec_state);
    let mut enc = NoiseEncoder::new();
    let mut dec = NoiseDecoder::new();

    // ── SetupConnection ──────────────────────────────────────────────────────
    let setup = SetupConnectionOwned {
        protocol: Protocol::MiningProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0,
        endpoint_host: Str0255Owned::try_from("").unwrap(),
        endpoint_port: 0,
        vendor: Str0255Owned::try_from("smoke").unwrap(),
        hardware_version: Str0255Owned::try_from("").unwrap(),
        firmware: Str0255Owned::try_from("").unwrap(),
        device_id: Str0255Owned::try_from("").unwrap(),
    };
    send(
        &mut s,
        &mut enc,
        &mut enc_state,
        0x00,
        false,
        &binary_sv2::to_bytes(setup).unwrap(),
    )
    .await;
    let (mt, _) = recv(&mut s, &mut dec, &mut dec_state).await;
    println!("← msg_type 0x{mt:02x} (expect 0x01 SetupConnectionSuccess)");
    assert_eq!(mt, 0x01);

    // ── OpenExtendedMiningChannel ────────────────────────────────────────────
    let open = OpenExtendedMiningChannelOwned {
        request_id: 1,
        user_identity: Str0255Owned::try_from("smoke.worker").unwrap(),
        nominal_hash_rate: 1.0e12,
        max_target: U256Owned::from([0xffu8; 32]),
        min_extranonce_size: 8, // NerdQAxe++ asks for 8; pool grants >= this
    };
    send(
        &mut s,
        &mut enc,
        &mut enc_state,
        0x13,
        false,
        &binary_sv2::to_bytes(open).unwrap(),
    )
    .await;

    let mut saw_success = false;
    let mut saw_job = false;
    let mut saw_prevhash = false;
    for _ in 0..6 {
        let (mt, mut payload) = match tokio::time::timeout(
            Duration::from_secs(8),
            recv(&mut s, &mut dec, &mut dec_state),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => break,
        };
        match mt {
            0x14 => {
                saw_success = true;
                println!("← 0x14 OpenExtendedMiningChannelSuccess");
            }
            0x1f => {
                saw_job = true;
                let j: NewExtendedMiningJob = binary_sv2::from_bytes(&mut payload).unwrap();
                println!(
                    "← 0x1f NewExtendedMiningJob: job_id={} version=0x{:08x} vr_allowed={} merkle_path_len={} cb_prefix={}B cb_suffix={}B",
                    j.job_id, j.version, j.version_rolling_allowed, j.merkle_path.len(),
                    j.coinbase_tx_prefix.as_ref().len(), j.coinbase_tx_suffix.as_ref().len(),
                );
            }
            0x20 => {
                saw_prevhash = true;
                let p: SetNewPrevHash = binary_sv2::from_bytes(&mut payload).unwrap();
                println!(
                    "← 0x20 SetNewPrevHash: job_id={} nbits=0x{:08x} prev_hash={}",
                    p.job_id,
                    p.nbits,
                    hex::encode(p.prev_hash.as_ref()),
                );
            }
            other => println!("← 0x{other:02x} (other)"),
        }
        if saw_success && saw_job && saw_prevhash {
            break;
        }
    }

    assert!(saw_success, "no OpenExtendedMiningChannelSuccess");
    assert!(saw_job, "no NewExtendedMiningJob");
    assert!(saw_prevhash, "no SetNewPrevHash");
    println!("\nSV2 Noise smoke test PASSED ✅");
}
