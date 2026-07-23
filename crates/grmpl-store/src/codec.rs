//! Physical (de)serialization of values and tuples. This lives entirely below
//! the bright line: the core has no idea how it is stored. Encoding is compact
//! and self-describing (a tag byte per value); it is *not* order-preserving,
//! which is fine because tuples are stored in the record value, not the key
//! (M0/M1 use full-scan consolidation — DESIGN.md §9).

use std::sync::Arc;

use grmpl_core::{Diff, Entity, Error, Result, Tuple, Value};

const TAG_ENT: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;
const TAG_TUPLE: u8 = 5;

/// `value = diff(8, LE) || encoded_tuple`.
pub fn encode_record(diff: Diff, tuple: &Tuple) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&diff.to_le_bytes());
    encode_tuple(tuple, &mut out);
    out
}

pub fn decode_record(bytes: &[u8]) -> Result<(Diff, Tuple)> {
    if bytes.len() < 8 {
        return Err(Error::Codec("record shorter than diff header".into()));
    }
    let mut diff_buf = [0u8; 8];
    diff_buf.copy_from_slice(&bytes[..8]);
    let diff = i64::from_le_bytes(diff_buf);
    let (tuple, pos) = decode_tuple(bytes, 8)?;
    if pos != bytes.len() {
        return Err(Error::Codec("trailing bytes after tuple".into()));
    }
    Ok((diff, tuple))
}

fn encode_tuple(tuple: &Tuple, out: &mut Vec<u8>) {
    let vals = tuple.as_slice();
    out.extend_from_slice(&(vals.len() as u32).to_be_bytes());
    for v in vals {
        encode_value(v, out);
    }
}

fn decode_tuple(bytes: &[u8], mut pos: usize) -> Result<(Tuple, usize)> {
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
    let tag = *bytes
        .get(pos)
        .ok_or_else(|| Error::Codec("unexpected end (tag)".into()))?;
    pos += 1;
    match tag {
        TAG_ENT => {
            let n = read_u64(bytes, &mut pos)?;
            Ok((Value::Ent(Entity(n)), pos))
        }
        TAG_INT => {
            let n = read_u64(bytes, &mut pos)? as i64;
            Ok((Value::Int(n), pos))
        }
        TAG_TEXT => {
            let len = read_u32(bytes, &mut pos)? as usize;
            let end = pos + len;
            let slice = bytes
                .get(pos..end)
                .ok_or_else(|| Error::Codec("unexpected end (text)".into()))?;
            let s = std::str::from_utf8(slice).map_err(|e| Error::Codec(e.to_string()))?;
            pos = end;
            Ok((Value::text(s), pos))
        }
        TAG_BOOL => {
            let b = *bytes
                .get(pos)
                .ok_or_else(|| Error::Codec("unexpected end (bool)".into()))?;
            pos += 1;
            Ok((Value::Bool(b != 0), pos))
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
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| Error::Codec("unexpected end (u32)".into()))?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u32::from_be_bytes(buf))
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let end = *pos + 8;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| Error::Codec("unexpected end (u64)".into()))?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u64::from_be_bytes(buf))
}
