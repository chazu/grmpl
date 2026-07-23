//! Message serialization for the transport is the canonical value serialization
//! from `grmpl-core` (re-exported here so the transport's public API is stable).

pub use grmpl_core::wire::{decode_message, encode_message};
