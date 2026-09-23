//! protocol/sv2/golden.rs
//!
//! Wire-format tests that do not depend on the SRI crates agreeing with
//! themselves.
//!
//! The other SV2 tests are roundtrips: our encoder against `binary_sv2`'s
//! decoder, or a `binary_sv2`-built device message against our decoder. Both
//! sides of those move together when the SRI crates are upgraded, so they
//! would keep passing even if an upgrade changed the bytes on the wire. Here
//! the expected bytes are written out field by field from the SV2 spec's data
//! types, with nothing from the SRI crates, and our encoders and decoders are
//! checked against them. An SRI upgrade must leave every test here passing
//! unchanged.
use super::{job, messages, *};
use crate::bitcoin::template::StratumJob;

/// Builds a message payload from SV2 spec data types.
#[derive(Default)]
struct Spec(Vec<u8>);

impl Spec {
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(mut self, v: f32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bool(self, v: bool) -> Self {
        self.u8(v as u8)
    }
    /// U256: 32 bytes as they are, no length prefix.
    fn u256(mut self, v: &[u8; 32]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
    /// STR0_255 and B0_32: a one-byte length, then the bytes.
    fn b0_255(mut self, v: &[u8]) -> Self {
        self.0
            .push(u8::try_from(v.len()).expect("fits a one-byte length"));
        self.0.extend_from_slice(v);
        self
    }
    /// B0_64K: a two-byte little-endian length, then the bytes.
    fn b0_64k(mut self, v: &[u8]) -> Self {
        let len = u16::try_from(v.len()).expect("fits a two-byte length");
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(v);
        self
    }
    /// SEQ0_255[U256]: a one-byte count, then the elements.
    fn seq0_255_u256(mut self, v: &[[u8; 32]]) -> Self {
        self.0
            .push(u8::try_from(v.len()).expect("fits a one-byte count"));
        for item in v {
            self.0.extend_from_slice(item);
        }
        self
    }
    /// OPTION[u32], which the spec defines as SEQ0_1[u32].
    fn option_u32(self, v: Option<u32>) -> Self {
        match v {
            None => self.u8(0),
            Some(x) => self.u8(1).u32(x),
        }
    }
    fn done(self) -> Vec<u8> {
        self.0
    }
}

/// 32 distinct bytes starting at `start`, so a reordering shows up.
fn ramp(start: u8) -> [u8; 32] {
    std::array::from_fn(|i| start.wrapping_add(i as u8))
}

#[test]
fn message_types_match_the_spec() {
    assert_eq!(MESSAGE_TYPE_SETUP_CONNECTION, 0x00);
    assert_eq!(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, 0x01);
    assert_eq!(MESSAGE_TYPE_SETUP_CONNECTION_ERROR, 0x02);
    assert_eq!(MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR, 0x12);
    assert_eq!(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL, 0x13);
    assert_eq!(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS, 0x14);
    assert_eq!(MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED, 0x1b);
    assert_eq!(MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS, 0x1c);
    assert_eq!(MESSAGE_TYPE_SUBMIT_SHARES_ERROR, 0x1d);
    assert_eq!(MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB, 0x1f);
    assert_eq!(MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH, 0x20);
    assert_eq!(MESSAGE_TYPE_SET_TARGET, 0x21);
}

#[test]
fn frame_header_is_extension_type_msg_type_and_u24_length() {
    let payload = [0xaa; 0x01_02_03];
    let framed = messages::frame_bytes(0x1f, true, &payload);
    assert_eq!(&framed[..6], &[0x00, 0x80, 0x1f, 0x03, 0x02, 0x01]);
    assert_eq!(&framed[6..], &payload[..]);

    let framed = messages::frame_bytes(0x01, false, &[0xbb, 0xcc]);
    assert_eq!(framed, [0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0xbb, 0xcc]);
}

#[test]
fn setup_connection_replies_match_the_spec_layout() {
    assert_eq!(
        messages::setup_connection_success(2).unwrap(),
        Spec::default().u16(2).u32(0).done()
    );
    assert_eq!(
        messages::setup_connection_error("unsupported-feature-flags", 0b11).unwrap(),
        Spec::default()
            .u32(0b11)
            .b0_255(b"unsupported-feature-flags")
            .done()
    );
}

#[test]
fn channel_replies_match_the_spec_layout() {
    let target = ramp(0x40);
    assert_eq!(
        messages::open_extended_success(7, 3, target, 8, vec![0xde, 0xad, 0xbe, 0xef]).unwrap(),
        Spec::default()
            .u32(7)
            .u32(3)
            .u256(&target)
            .u16(8)
            .b0_255(&[0xde, 0xad, 0xbe, 0xef])
            .u32(0)
            .done()
    );
    assert_eq!(
        messages::open_channel_error_extranonce(7).unwrap(),
        Spec::default()
            .u32(7)
            .b0_255(b"unsupported-min-extranonce-size")
            .done()
    );
    assert_eq!(
        messages::set_target(3, target).unwrap(),
        Spec::default().u32(3).u256(&target).done()
    );
}

#[test]
fn share_replies_match_the_spec_layout() {
    assert_eq!(
        messages::submit_shares_success(3, 41, 0x0102_0304_0506).unwrap(),
        Spec::default()
            .u32(3)
            .u32(41)
            .u32(1)
            .u64(0x0102_0304_0506)
            .done()
    );
    assert_eq!(
        messages::submit_shares_error(3, 42, "stale-share").unwrap(),
        Spec::default().u32(3).u32(42).b0_255(b"stale-share").done()
    );
}

fn sample_job() -> StratumJob {
    StratumJob {
        job_id: "1".into(),
        // Stratum order: each 4-byte word reversed relative to internal order.
        prev_hash: hex::encode(ramp(0x80)),
        coinbase1: vec![0x01, 0x02, 0x03],
        coinbase2: vec![0x04, 0x05, 0x06, 0x07],
        merkle_branch: vec![],
        merkle_branch_raw: vec![ramp(0x10), ramp(0x30)],
        version: 0x2000_0000,
        bits: "1703a30c".into(),
        cur_time: 0x6700_0001,
        min_time: 0x6700_0000,
        height: 900_000,
        network_target: [0; 32],
        coinbase_template: vec![],
        extranonce_offset: 0,
        extranonce1_len: 4,
        extranonce2_len: 8,
        transactions: vec![],
        coinbase_value: 312_500_000,
    }
}

#[test]
fn job_messages_match_the_spec_layout() {
    let j = sample_job();
    let expected = |min_ntime| {
        Spec::default()
            .u32(3)
            .u32(9)
            .option_u32(min_ntime)
            .u32(0x2000_0000)
            .bool(true)
            .seq0_255_u256(&[ramp(0x10), ramp(0x30)])
            .b0_64k(&[0x01, 0x02, 0x03])
            .b0_64k(&[0x04, 0x05, 0x06, 0x07])
            .done()
    };

    // A future job carries no min_ntime; an immediate one carries cur_time.
    let future = job::build_new_extended_job(&j, 3, 9, true).unwrap();
    assert_eq!(messages::encode(future).unwrap(), expected(None));
    let now = job::build_new_extended_job(&j, 3, 9, false).unwrap();
    assert_eq!(messages::encode(now).unwrap(), expected(Some(0x6700_0001)));

    let mut internal = ramp(0x80);
    for word in internal.chunks_mut(4) {
        word.reverse();
    }
    let snph = job::build_set_new_prev_hash(&j, 3, 9).unwrap();
    assert_eq!(
        messages::encode(snph).unwrap(),
        Spec::default()
            .u32(3)
            .u32(9)
            .u256(&internal)
            .u32(0x6700_0001)
            .u32(0x1703_a30c)
            .done()
    );
}

#[test]
fn device_messages_in_the_spec_layout_decode() {
    // SetupConnection as a device sends it: mining protocol, v2 only, no flags.
    let mut setup = Spec::default()
        .u8(0)
        .u16(2)
        .u16(2)
        .u32(0x04)
        .b0_255(b"pool.lan")
        .u16(3335)
        .b0_255(b"bitaxe")
        .b0_255(b"")
        .b0_255(b"v2.9.0")
        .b0_255(b"")
        .done();
    let s = messages::decode_setup_connection(&mut setup).unwrap();
    assert!(matches!(
        s.protocol,
        common_messages_sv2::Protocol::MiningProtocol
    ));
    assert_eq!((s.min_version, s.max_version, s.flags), (2, 2, 0x04));

    let max_target = ramp(0x60);
    let mut open = Spec::default()
        .u32(5)
        .b0_255(b"bc1qexample.worker1")
        .f32(1.2e12)
        .u256(&max_target)
        .u16(4)
        .done();
    let o = messages::decode_open_extended(&mut open).unwrap();
    assert_eq!(o.request_id, 5);
    assert_eq!(o.user_identity, "bc1qexample.worker1");
    assert_eq!(o.nominal_hash_rate, 1.2e12);
    assert_eq!(o.max_target, max_target);
    assert_eq!(o.min_extranonce_size, 4);

    let mut submit = Spec::default()
        .u32(7)
        .u32(42)
        .u32(99)
        .u32(0x1234_5678)
        .u32(0x6700_0002)
        .u32(0x2000_4000)
        .b0_255(&[0x11, 0x22, 0x33, 0x44])
        .done();
    let m = messages::decode_submit_extended(&mut submit).unwrap();
    assert_eq!((m.channel_id, m.sequence_number, m.job_id), (7, 42, 99));
    assert_eq!(
        (m.nonce, m.ntime, m.version),
        (0x1234_5678, 0x6700_0002, 0x2000_4000)
    );
    assert_eq!(m.extranonce, [0x11, 0x22, 0x33, 0x44]);
}
