//! **Decoders against hostile bytes.** Messages arrive from other domains and
//! stored cells from players, so every `wire` decoder must answer `Ok` or
//! `Err(Error::Codec)` on *any* input: never panic, never let a count read from
//! the input size an allocation, never recurse as deep as the input says.
//!
//! The fixed cases pin the two attacks that used to abort the process (a deep
//! tuple, a huge count); the mutation law throws seeded bit flips, overwrites,
//! truncations and insertions at valid encodings of every artifact, and asserts
//! that whatever decodes re-encodes to bytes that decode to the same thing.

use std::panic::{catch_unwind, AssertUnwindSafe};

use grmpl_core::schema::{Column, Schema, Ty};
use grmpl_core::wire::{self, MAX_DEPTH};
use grmpl_core::{Entity, Error, Message, RelId, Tuple, Value};

/// Deterministic xorshift64*, the crate-wide test idiom.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One to four random edits: a bit flipped, a byte or a length-sized word
/// overwritten (with the values that break counts), a tail cut off, a byte
/// inserted.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(b.len());
        match rng.below(5) {
            0 if !b.is_empty() => b[at] ^= 1 << rng.below(8),
            1 if !b.is_empty() => b[at] = rng.next() as u8,
            2 if b.len() >= 4 => {
                let word: u32 = [0, 1, 0x7FFF_FFFF, 0xFFFF_FFFF, rng.next() as u32][rng.below(5)];
                let at = at.min(b.len() - 4);
                b[at..at + 4].copy_from_slice(&word.to_be_bytes());
            }
            3 => b.truncate(at),
            _ => b.insert(at, rng.next() as u8),
        }
    }
    b
}

fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Int(7), |v, _| Value::Tuple([v].into()))
}

fn every_value() -> Vec<Value> {
    vec![
        Value::Ent(Entity(42)),
        Value::Int(-3),
        Value::float(2.5).unwrap(),
        Value::text("lamp"),
        Value::Bool(true),
        Value::bytes([0u8, 1, 255]),
        Value::code([9u8, 8, 7]),
        Value::Tuple([Value::Int(1), Value::Tuple([Value::text("x"), Value::Bool(false)].into())].into()),
        nested(MAX_DEPTH),
    ]
}

#[test]
fn a_deep_tuple_is_refused_not_a_stack_overflow() {
    // A one-cell tuple whose cell is a one-cell tuple value, a million deep:
    // this aborted the process before the nesting limit.
    let mut bytes = 1u32.to_be_bytes().to_vec();
    for _ in 0..1_000_000 {
        bytes.push(5);
        bytes.extend_from_slice(&1u32.to_be_bytes());
    }
    assert!(matches!(wire::decode_tuple(&bytes, 0), Err(Error::Codec(_))));
}

#[test]
fn values_nest_to_max_depth_and_no_further() {
    for (depth, ok) in [(MAX_DEPTH, true), (MAX_DEPTH + 1, false)] {
        let v = nested(depth);
        let mut bytes = Vec::new();
        wire::encode_value(&v, &mut bytes);
        match wire::decode_value(&bytes, 0) {
            Ok((back, end)) => {
                assert!(ok, "a value {depth} deep decoded");
                assert_eq!((back, end), (v, bytes.len()));
            }
            Err(e) => {
                assert!(!ok, "a value {depth} deep was refused: {e}");
                assert!(matches!(e, Error::Codec(_)));
            }
        }
    }
}

#[test]
fn a_huge_count_reserves_nothing_it_cannot_fill() {
    // Each claims four billion elements in a few bytes. Reserving that many
    // would abort on the allocation; each must fail on the bytes it lacks.
    let huge = u32::MAX.to_be_bytes();
    let tuple = [&huge[..], &[2, 0, 0, 0, 0, 0, 0, 0, 1]].concat();
    assert!(matches!(wire::decode_tuple(&tuple, 0), Err(Error::Codec(_))));
    for tag in [3u8, 5, 6, 7] {
        let value = [&[tag][..], &huge].concat();
        assert!(matches!(wire::decode_value(&value, 0), Err(Error::Codec(_))), "tag {tag}");
    }
    let schema = [&[wire::FORMAT_VERSION][..], &huge, &[1, 0, 0, 0, 1, b'a']].concat();
    assert!(matches!(wire::decode_schema(&schema), Err(Error::Codec(_))));
    let message = [&[wire::FORMAT_VERSION][..], &[0, 0, 0, 1], &huge].concat();
    assert!(matches!(wire::decode_message(&message), Err(Error::Codec(_))));
}

/// What decodes from mutated bytes must be a value: encoding it and decoding
/// again gives it back. Run under `catch_unwind`, so a panic fails the law
/// with the seed and the bytes that caused it.
fn law<T: PartialEq + std::fmt::Debug>(
    name: &str,
    seeds: &[Vec<u8>],
    decode: impl Fn(&[u8]) -> grmpl_core::Result<T>,
    encode: impl Fn(&T) -> Vec<u8>,
) {
    for (i, seed) in seeds.iter().enumerate() {
        assert!(decode(seed).is_ok(), "{name}: seed {i} does not decode");
    }
    let mut rng = Rng::new(name.len() as u64);
    for iter in 0..4_000 {
        let pick = rng.below(seeds.len());
        let bytes = mutate(&mut rng, &seeds[pick]);
        let got = catch_unwind(AssertUnwindSafe(|| decode(&bytes)))
            .unwrap_or_else(|_| panic!("{name}: iteration {iter} panicked on {bytes:02x?}"));
        if let Ok(v) = got {
            let again = encode(&v);
            let back = decode(&again).unwrap_or_else(|e| panic!("{name}: iteration {iter}: re-encoding {v:?} does not decode: {e}"));
            assert_eq!(back, v, "{name}: iteration {iter}: not a round trip");
        }
    }
}

#[test]
fn mutated_values_decode_or_err_and_never_panic() {
    let seeds: Vec<Vec<u8>> = every_value()
        .iter()
        .map(|v| {
            let mut b = Vec::new();
            wire::encode_value(v, &mut b);
            b
        })
        .collect();
    law(
        "value",
        &seeds,
        |b| wire::decode_value(b, 0).map(|(v, _)| v),
        |v| {
            let mut b = Vec::new();
            wire::encode_value(v, &mut b);
            b
        },
    );
}

#[test]
fn mutated_tuples_and_messages_decode_or_err_and_never_panic() {
    let tuple = Tuple::new(every_value());
    let mut bytes = Vec::new();
    wire::encode_tuple(&tuple, &mut bytes);
    let encode = |t: &Tuple| {
        let mut b = Vec::new();
        wire::encode_tuple(t, &mut b);
        b
    };
    law("tuple", &[bytes, encode(&Tuple::from([]))], |b| wire::decode_tuple(b, 0).map(|(t, _)| t), encode);

    let messages: Vec<Vec<u8>> = [Tuple::new(every_value()), Tuple::from([Value::Int(1)])]
        .into_iter()
        .map(|body| wire::encode_message(&Message { inbox: RelId(9), body }))
        .collect();
    law("message", &messages, wire::decode_message, wire::encode_message);
}

#[test]
fn mutated_schemas_decode_or_err_and_never_panic() {
    let schema = Schema::new(
        [Ty::Ent, Ty::Int, Ty::Float, Ty::Text, Ty::Bool, Ty::Tuple, Ty::Bytes, Ty::Code, Ty::Any]
            .into_iter()
            .enumerate()
            .map(|(i, ty)| Column::new(format!("c{i}"), ty))
            .collect(),
    );
    let seeds = [wire::encode_schema(&schema), wire::encode_schema(&Schema::new(vec![]))];
    law("schema", &seeds, wire::decode_schema, wire::encode_schema);
}
