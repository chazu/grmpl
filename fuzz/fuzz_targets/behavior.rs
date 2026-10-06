//! A stored behavior, player-supplied code: decodes or is refused, and what
//! decodes round-trips.
#![no_main]

use grmpl_lang::behavior::{decode_behavior, encode_behavior};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = decode_behavior(data) {
        assert_eq!(decode_behavior(&encode_behavior(&b)).expect("re-encoded behavior decodes"), b);
    }
});
