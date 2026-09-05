#![no_main]
//! One Stratum V1 line from an unauthenticated peer, as `handle_line` sees it
//! after the bounded reader: JSON parse, then method dispatch and parameter
//! extraction for every known method. Must never panic.
use libfuzzer_sys::fuzz_target;
use solo_pool_rs::protocol::sv1::{ClientMessage, StratumRequest};

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(req) = StratumRequest::parse(line) {
        let _ = ClientMessage::from_request(&req);
    }
});
