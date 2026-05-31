//! protocol/sv2/messages.rs
//!
//! Stratum V2 frame I/O and message (de)serialization for the plaintext
//! mining protocol.
//!
//! Every SV2 message is a 6-byte frame header followed by a binary payload:
//!   - `extension_type` : u16 LE (bit 15 = `channel_msg`, lower 15 bits = ext)
//!   - `msg_type`       : u8
//!   - `msg_length`     : u24 LE (payload length, header excluded)
//!
//! We use `binary_sv2` for the (hard) payload encoding and `framing_sv2::Header`
//! for the header size constant; the per-frame read/write over tokio is done
//! here directly (no Noise — see the SV2 dependency note in Cargo.toml).
use anyhow::{anyhow, Result};
use binary_sv2::{Str0255, B032, U256};
use common_messages_sv2::{Protocol, SetupConnection, SetupConnectionSuccess};
use framing_sv2::header::Header;
use mining_sv2::{
    OpenExtendedMiningChannel, OpenExtendedMiningChannelSuccess, OpenMiningChannelError, SetTarget,
    SubmitSharesError, SubmitSharesExtended, SubmitSharesSuccess,
};

/// Standard mining protocol — no negotiated extension.
const EXT_TYPE: u16 = 0x0000;
/// `channel_msg` bit (MSB of the 16-bit `extension_type` field).
const CHANNEL_MSG_BIT: u16 = 0x8000;

// ─────────────────────────────────────────────────────────────────────────────
// Framing
// ─────────────────────────────────────────────────────────────────────────────

/// Serialize one plaintext SV2 frame: the 6-byte header followed by `payload`.
///
/// The encrypted transport ([`super::noise`]) feeds this to the Noise codec,
/// which encrypts the header and chunks/encrypts the payload before it hits the
/// wire. `channel_msg` sets the channel bit (MSB) of the `extension_type` field.
pub fn frame_bytes(msg_type: u8, channel_msg: bool, payload: &[u8]) -> Vec<u8> {
    let ext = EXT_TYPE | if channel_msg { CHANNEL_MSG_BIT } else { 0 };
    let len = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(Header::SIZE + payload.len());
    out.extend_from_slice(&ext.to_le_bytes());
    out.push(msg_type);
    out.extend_from_slice(&len[0..3]);
    out.extend_from_slice(payload);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Inbound decode (extract owned values so nothing borrows the payload buffer)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SetupConn {
    pub min_version: u16,
    pub max_version: u16,
    #[allow(dead_code)]
    pub flags: u32,
}

/// Decode `SetupConnection`, rejecting any non-mining sub-protocol.
pub fn decode_setup_connection(payload: &mut [u8]) -> Result<SetupConn> {
    let m: SetupConnection =
        binary_sv2::from_bytes(payload).map_err(|e| anyhow!("decode SetupConnection: {e:?}"))?;
    if !matches!(m.protocol, Protocol::MiningProtocol) {
        return Err(anyhow!("unsupported sub-protocol: {:?}", m.protocol));
    }
    Ok(SetupConn {
        min_version: m.min_version,
        max_version: m.max_version,
        flags: m.flags,
    })
}

#[derive(Debug)]
pub struct OpenExtended {
    pub request_id: u32,
    pub user_identity: String,
    #[allow(dead_code)]
    pub nominal_hash_rate: f32,
    /// Easiest target the device will accept (SV2 little-endian U256).
    pub max_target: [u8; 32],
    pub min_extranonce_size: u16,
}

pub fn decode_open_extended(payload: &mut [u8]) -> Result<OpenExtended> {
    let m: OpenExtendedMiningChannel = binary_sv2::from_bytes(payload)
        .map_err(|e| anyhow!("decode OpenExtendedMiningChannel: {e:?}"))?;
    let user_identity = String::from_utf8_lossy(m.user_identity.inner_as_ref()).into_owned();
    let mut max_target = [0u8; 32];
    max_target.copy_from_slice(m.max_target.inner_as_ref());
    Ok(OpenExtended {
        request_id: m.request_id,
        user_identity,
        nominal_hash_rate: m.nominal_hash_rate,
        max_target,
        min_extranonce_size: m.min_extranonce_size,
    })
}

#[derive(Debug)]
pub struct SubmitExtended {
    #[allow(dead_code)]
    pub channel_id: u32,
    pub sequence_number: u32,
    pub job_id: u32,
    pub nonce: u32,
    pub ntime: u32,
    pub version: u32,
    pub extranonce: Vec<u8>,
}

pub fn decode_submit_extended(payload: &mut [u8]) -> Result<SubmitExtended> {
    let m: SubmitSharesExtended = binary_sv2::from_bytes(payload)
        .map_err(|e| anyhow!("decode SubmitSharesExtended: {e:?}"))?;
    Ok(SubmitExtended {
        channel_id: m.channel_id,
        sequence_number: m.sequence_number,
        job_id: m.job_id,
        nonce: m.nonce,
        ntime: m.ntime,
        version: m.version,
        extranonce: m.extranonce.inner_as_ref().to_vec(),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Outbound encode (payload bytes only — caller frames with write_frame)
// ─────────────────────────────────────────────────────────────────────────────

/// Serialize any SV2 message struct into its payload bytes.
pub fn encode<T: binary_sv2::Serialize + binary_sv2::GetSize>(msg: T) -> Result<Vec<u8>> {
    binary_sv2::to_bytes(msg).map_err(|e| anyhow!("SV2 encode: {e:?}"))
}

pub fn setup_connection_success(used_version: u16) -> Result<Vec<u8>> {
    // flags = 0: no constraints (version rolling allowed, extended channels ok).
    encode(SetupConnectionSuccess {
        used_version,
        flags: 0,
    })
}

pub fn open_extended_success(
    request_id: u32,
    channel_id: u32,
    target_le: [u8; 32],
    extranonce_size: u16,
    extranonce_prefix: Vec<u8>,
) -> Result<Vec<u8>> {
    encode(OpenExtendedMiningChannelSuccess {
        request_id,
        channel_id,
        target: U256::from(target_le),
        extranonce_size,
        extranonce_prefix: B032::try_from(extranonce_prefix)
            .map_err(|e| anyhow!("extranonce_prefix: {e:?}"))?,
        group_channel_id: 0,
    })
}

pub fn open_channel_error_extranonce(request_id: u32) -> Result<Vec<u8>> {
    encode(OpenMiningChannelError::unsupported_extranonce_size(
        request_id,
    ))
}

pub fn set_target(channel_id: u32, target_le: [u8; 32]) -> Result<Vec<u8>> {
    encode(SetTarget {
        channel_id,
        maximum_target: U256::from(target_le),
    })
}

pub fn submit_shares_success(
    channel_id: u32,
    last_sequence_number: u32,
    shares_sum: u64,
) -> Result<Vec<u8>> {
    encode(SubmitSharesSuccess {
        channel_id,
        last_sequence_number,
        new_submits_accepted_count: 1,
        new_shares_sum: shares_sum,
    })
}

pub fn submit_shares_error(channel_id: u32, sequence_number: u32, code: &str) -> Result<Vec<u8>> {
    encode(SubmitSharesError {
        channel_id,
        sequence_number,
        error_code: Str0255::try_from(code.to_string())
            .map_err(|e| anyhow!("error_code: {e:?}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_bytes_has_well_formed_header() {
        let payload = vec![0xde, 0xad, 0xbe, 0xef, 0x01];

        // Channel message (e.g. NewExtendedMiningJob).
        let framed = frame_bytes(0x1f, true, &payload);
        let header = Header::from_bytes(&framed[..Header::SIZE]).unwrap();
        assert_eq!(header.msg_type(), 0x1f);
        assert!(header.channel_msg(), "channel bit must be set");
        assert_eq!(header.ext_type_without_channel_msg(), 0x0000);
        assert_eq!(&framed[Header::SIZE..], &payload[..]);

        // Non-channel message: channel bit clear.
        let framed = frame_bytes(0x01, false, &payload);
        let header = Header::from_bytes(&framed[..Header::SIZE]).unwrap();
        assert_eq!(header.msg_type(), 0x01);
        assert!(!header.channel_msg());
    }

    #[test]
    fn setup_connection_decode_accepts_mining_and_rejects_others() {
        let mk = |protocol: Protocol| {
            let m = SetupConnection {
                protocol,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: Str0255::try_from(String::new()).unwrap(),
                endpoint_port: 0,
                vendor: Str0255::try_from("bitaxe".to_string()).unwrap(),
                hardware_version: Str0255::try_from(String::new()).unwrap(),
                firmware: Str0255::try_from(String::new()).unwrap(),
                device_id: Str0255::try_from(String::new()).unwrap(),
            };
            binary_sv2::to_bytes(m).unwrap()
        };

        let mut mining = mk(Protocol::MiningProtocol);
        let decoded = decode_setup_connection(&mut mining).unwrap();
        assert_eq!(decoded.min_version, 2);
        assert_eq!(decoded.max_version, 2);

        let mut jd = mk(Protocol::JobDeclarationProtocol);
        assert!(decode_setup_connection(&mut jd).is_err());
    }

    #[test]
    fn open_extended_decode_extracts_identity_and_target() {
        let max_target = [0xffu8; 32];
        let m = OpenExtendedMiningChannel {
            request_id: 5,
            user_identity: Str0255::try_from("bc1qexample.worker1".to_string()).unwrap(),
            nominal_hash_rate: 1.2e12,
            max_target: U256::from(max_target),
            min_extranonce_size: 4,
        };
        let mut bytes = binary_sv2::to_bytes(m).unwrap();
        let decoded = decode_open_extended(&mut bytes).unwrap();
        assert_eq!(decoded.request_id, 5);
        assert_eq!(decoded.user_identity, "bc1qexample.worker1");
        assert_eq!(decoded.min_extranonce_size, 4);
        assert_eq!(decoded.max_target, max_target);
    }

    #[test]
    fn submit_shares_extended_decode_extracts_fields() {
        // Build a SubmitSharesExtended exactly as a device would, serialize it,
        // and confirm our decoder recovers every field (validates endianness via
        // binary_sv2 and our extranonce extraction).
        let extranonce = vec![0x11u8, 0x22, 0x33, 0x44];
        let msg = SubmitSharesExtended {
            channel_id: 7,
            sequence_number: 42,
            job_id: 99,
            nonce: 0x1234_5678,
            ntime: 0x6500_0000,
            version: 0x2000_2000,
            extranonce: B032::try_from(extranonce.clone()).unwrap(),
        };
        let mut bytes = binary_sv2::to_bytes(msg).unwrap();
        let decoded = decode_submit_extended(&mut bytes).unwrap();
        assert_eq!(decoded.channel_id, 7);
        assert_eq!(decoded.sequence_number, 42);
        assert_eq!(decoded.job_id, 99);
        assert_eq!(decoded.nonce, 0x1234_5678);
        assert_eq!(decoded.ntime, 0x6500_0000);
        assert_eq!(decoded.version, 0x2000_2000);
        assert_eq!(decoded.extranonce, extranonce);
    }
}
