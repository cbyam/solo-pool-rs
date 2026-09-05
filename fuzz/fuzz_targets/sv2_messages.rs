#![no_main]
//! One decrypted Stratum V2 payload from an unauthenticated peer, routed to
//! the decoder the first byte selects. These run before any channel is open,
//! so every byte is attacker-chosen. Must never panic.
use libfuzzer_sys::fuzz_target;
use solo_pool_rs::protocol::sv2::messages::{
    decode_open_extended, decode_setup_connection, decode_submit_extended,
};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    let mut payload = payload.to_vec();
    match selector % 3 {
        0 => {
            let _ = decode_setup_connection(&mut payload);
        }
        1 => {
            let _ = decode_open_extended(&mut payload);
        }
        _ => {
            let _ = decode_submit_extended(&mut payload);
        }
    }
});
