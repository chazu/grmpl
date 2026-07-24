//! Physical framing of a stored record. This lives entirely below the bright
//! line: the core has no idea how it is stored. The *value/tuple* encoding is
//! not duplicated here — it is the one canonical codec from
//! `grmpl_core::wire`, shared with the message wire so there is a single
//! serialization to reason about (M0/M1 use full-scan consolidation —
//! DESIGN.md §9). This module only adds the record framing around it:
//!
//! `record = version(1) || diff(8, LE) || encoded_tuple`
//!
//! The leading version byte is `grmpl_core::wire::FORMAT_VERSION`; a record
//! written under a different version is rejected rather than misread.

use grmpl_core::wire;
use grmpl_core::{Diff, Error, Result, Tuple};

/// `record = version(1) || diff(8, LE) || encoded_tuple`.
pub fn encode_record(diff: Diff, tuple: &Tuple) -> Vec<u8> {
    let mut out = Vec::new();
    wire::push_version(&mut out);
    out.extend_from_slice(&diff.to_le_bytes());
    wire::encode_tuple(tuple, &mut out);
    out
}

pub fn decode_record(bytes: &[u8]) -> Result<(Diff, Tuple)> {
    let pos = wire::read_version(bytes)?;
    let diff_bytes = bytes
        .get(pos..pos + 8)
        .ok_or_else(|| Error::Codec("record shorter than diff header".into()))?;
    let mut diff_buf = [0u8; 8];
    diff_buf.copy_from_slice(diff_bytes);
    let diff = i64::from_le_bytes(diff_buf);
    let (tuple, end) = wire::decode_tuple(bytes, pos + 8)?;
    if end != bytes.len() {
        return Err(Error::Codec("trailing bytes after tuple".into()));
    }
    Ok((diff, tuple))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grmpl_core::{Entity, Value};

    #[test]
    fn record_roundtrips_and_is_version_prefixed() {
        let tuple = Tuple::from([Value::Ent(Entity(3)), Value::text("x")]);
        let bytes = encode_record(-2, &tuple);
        assert_eq!(bytes[0], wire::FORMAT_VERSION);
        assert_eq!(decode_record(&bytes).unwrap(), (-2, tuple));
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let mut bytes = encode_record(1, &Tuple::from([Value::Int(0)]));
        bytes[0] = wire::FORMAT_VERSION.wrapping_add(1);
        assert!(decode_record(&bytes).is_err());
    }
}
