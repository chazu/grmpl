//! Canonical serialization of core values and messages.
//!
//! This is *value* serialization (turning `Value`/`Tuple`/`Message` into bytes),
//! not storage layout — the on-disk physical encoding stays in `grmpl-store`.
//! It lives here so every crate that must put a `Message` on a wire (the
//! transport, the cross-domain router) shares one encoding. Pure and
//! dependency-free. `message = inbox(u32, BE) || encoded_tuple(body)`.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::patch::Message;
use crate::value::{Entity, RelId, Tuple, Value};

const TAG_ENT: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;
const TAG_TUPLE: u8 = 5;

pub fn encode_message(m: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&m.inbox.0.to_be_bytes());
    encode_tuple(&m.body, &mut out);
    out
}

pub fn decode_message(bytes: &[u8]) -> Result<Message> {
    if bytes.len() < 4 {
        return Err(Error::Codec("message shorter than inbox header".into()));
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[..4]);
    let inbox = RelId(u32::from_be_bytes(buf));
    let (body, pos) = decode_tuple(bytes, 4)?;
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
