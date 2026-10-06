//! A relation schema read back from the store: decodes or is refused, and
//! what decodes round-trips.
#![no_main]

use grmpl_core::wire::{decode_schema, encode_schema};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = decode_schema(data) {
        assert_eq!(decode_schema(&encode_schema(&s)).expect("re-encoded schema decodes"), s);
    }
});
