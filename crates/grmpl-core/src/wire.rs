//! Canonical serialization of core values and messages.
//!
//! This is *value* serialization (turning `Value`/`Tuple`/`Message` into bytes),
//! not storage layout — but it is the *one* value/tuple encoding for the whole
//! system. Every framing that must put values on a byte channel builds on the
//! shared `encode_tuple`/`decode_tuple` here: the transport and cross-domain
//! router frame a `Message`, and `grmpl-store` frames an on-disk record around
//! this same tuple codec (it no longer keeps a private copy). Pure and
//! dependency-free.
//!
//! ## Format version
//!
//! Every serialized artifact — a `message`, a store `record` — begins with a
//! single [`FORMAT_VERSION`] byte. Decoders reject any other version loudly
//! (`Error::Codec`) rather than silently misreading an evolved layout. Bump the
//! constant whenever the tag set or framing changes so old bytes fail fast; the
//! byte is checked by the shared [`read_version`] helper.
//!
//! * `message = version(1) || inbox(u32, BE) || encoded_tuple(body)`
//! * `encoded_tuple = arity(u32, BE) || value*`

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::patch::Message;
use crate::value::{Entity, RelId, Tuple, Value};

/// The wire/on-disk format version. Prefixes every serialized artifact. Bump
/// on any change to the tag set or framing so stale bytes are rejected rather
/// than misread. All framings (`message`, store `record`) share this byte.
pub const FORMAT_VERSION: u8 = 1;

const TAG_ENT: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;
const TAG_TUPLE: u8 = 5;

/// Push the leading [`FORMAT_VERSION`] byte. Every encoder starts here.
pub fn push_version(out: &mut Vec<u8>) {
    out.push(FORMAT_VERSION);
}

/// Validate the leading format-version byte and return the offset just past it
/// (always 1). Errors loudly on an unknown version or an empty buffer. Every
/// decoder starts here so the version check lives in exactly one place.
pub fn read_version(bytes: &[u8]) -> Result<usize> {
    match bytes.first() {
        Some(&FORMAT_VERSION) => Ok(1),
        Some(other) => Err(Error::Codec(format!(
            "unsupported format version {other} (expected {FORMAT_VERSION})"
        ))),
        None => Err(Error::Codec("empty buffer (no format version)".into())),
    }
}

pub fn encode_message(m: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    push_version(&mut out);
    out.extend_from_slice(&m.inbox.0.to_be_bytes());
    encode_tuple(&m.body, &mut out);
    out
}

pub fn decode_message(bytes: &[u8]) -> Result<Message> {
    let mut pos = read_version(bytes)?;
    let inbox_bytes = bytes
        .get(pos..pos + 4)
        .ok_or_else(|| Error::Codec("message shorter than inbox header".into()))?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(inbox_bytes);
    let inbox = RelId(u32::from_be_bytes(buf));
    pos += 4;
    let (body, pos) = decode_tuple(bytes, pos)?;
    if pos != bytes.len() {
        return Err(Error::Codec("trailing bytes after message body".into()));
    }
    Ok(Message { inbox, body })
}

pub fn encode_tuple(t: &Tuple, out: &mut Vec<u8>) {
    out.extend_from_slice(&(t.arity() as u32).to_be_bytes());
    for v in t.as_slice() {
        encode_value(v, out);
    }
}

pub fn decode_tuple(bytes: &[u8], mut pos: usize) -> Result<(Tuple, usize)> {
    let len = read_u32(bytes, &mut pos)? as usize;
    let mut vals = Vec::with_capacity(len);
    for _ in 0..len {
        let (v, next) = decode_value(bytes, pos)?;
        vals.push(v);
        pos = next;
    }
    Ok((Tuple(Arc::from(vals)), pos))
}

fn encode_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Ent(e) => {
            out.push(TAG_ENT);
            out.extend_from_slice(&e.0.to_be_bytes());
        }
        Value::Int(i) => {
            out.push(TAG_INT);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Text(s) => {
            out.push(TAG_TEXT);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(*b as u8);
        }
        Value::Tuple(elems) => {
            out.push(TAG_TUPLE);
            out.extend_from_slice(&(elems.len() as u32).to_be_bytes());
            for e in elems.iter() {
                encode_value(e, out);
            }
        }
    }
}

fn decode_value(bytes: &[u8], mut pos: usize) -> Result<(Value, usize)> {
    let tag = *bytes.get(pos).ok_or_else(|| Error::Codec("unexpected end (tag)".into()))?;
    pos += 1;
    match tag {
        TAG_ENT => Ok((Value::Ent(Entity(read_u64(bytes, &mut pos)?)), pos)),
        TAG_INT => Ok((Value::Int(read_u64(bytes, &mut pos)? as i64), pos)),
        TAG_TEXT => {
            let len = read_u32(bytes, &mut pos)? as usize;
            let end = pos + len;
            let slice = bytes.get(pos..end).ok_or_else(|| Error::Codec("unexpected end (text)".into()))?;
            let s = std::str::from_utf8(slice).map_err(|e| Error::Codec(e.to_string()))?;
            Ok((Value::text(s), end))
        }
        TAG_BOOL => {
            let b = *bytes.get(pos).ok_or_else(|| Error::Codec("unexpected end (bool)".into()))?;
            Ok((Value::Bool(b != 0), pos + 1))
        }
        TAG_TUPLE => {
            let len = read_u32(bytes, &mut pos)? as usize;
            let mut vals = Vec::with_capacity(len);
            for _ in 0..len {
                let (v, next) = decode_value(bytes, pos)?;
                vals.push(v);
                pos = next;
            }
            Ok((Value::Tuple(Arc::from(vals)), pos))
        }
        other => Err(Error::Codec(format!("unknown value tag {other}"))),
    }
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let end = *pos + 4;
    let slice = bytes.get(*pos..end).ok_or_else(|| Error::Codec("unexpected end (u32)".into()))?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u32::from_be_bytes(buf))
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let end = *pos + 8;
    let slice = bytes.get(*pos..end).ok_or_else(|| Error::Codec("unexpected end (u64)".into()))?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u64::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrips_and_is_version_prefixed() {
        let m = Message {
            inbox: RelId(42),
            body: Tuple::from([Value::Int(7), Value::text("hi"), Value::Bool(true)]),
        };
        let bytes = encode_message(&m);
        assert_eq!(bytes[0], FORMAT_VERSION, "artifact must lead with the version byte");
        assert_eq!(decode_message(&bytes).unwrap(), m);
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let mut bytes = encode_message(&Message {
            inbox: RelId(1),
            body: Tuple::from([Value::Int(0)]),
        });
        bytes[0] = FORMAT_VERSION.wrapping_add(1);
        assert!(decode_message(&bytes).is_err());
    }

    #[test]
    fn read_version_rejects_empty() {
        assert!(read_version(&[]).is_err());
    }
}
