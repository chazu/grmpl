//! A bare value and a tuple, as framings embed them: each decodes or is
//! refused, and what decodes round-trips.
#![no_main]

use grmpl_core::wire::{decode_tuple, decode_value, encode_tuple, encode_value};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((v, _)) = decode_value(data, 0) {
        let mut again = Vec::new();
        encode_value(&v, &mut again);
        assert_eq!(decode_value(&again, 0).expect("re-encoded value decodes"), (v, again.len()));
    }
    if let Ok((t, _)) = decode_tuple(data, 0) {
        let mut again = Vec::new();
        encode_tuple(&t, &mut again);
        assert_eq!(decode_tuple(&again, 0).expect("re-encoded tuple decodes"), (t, again.len()));
    }
});
