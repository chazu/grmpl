//! A message from another domain: decodes or is refused, and what decodes
//! round-trips.
#![no_main]

use grmpl_core::wire::{decode_message, encode_message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = decode_message(data) {
        assert_eq!(decode_message(&encode_message(&m)).expect("re-encoded message decodes"), m);
    }
});
