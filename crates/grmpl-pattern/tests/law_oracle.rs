//! MatchInput law oracles (TKT-95 rework of the P9a MatchInput abstraction).
//!
//! The `MatchInput` seam is the sole guarantee that `Repeat` terminates: its
//! progress `measure()` is a well-founded natural number that must strictly
//! decrease on every `next()` and hit zero exactly at the end. The in-crate
//! unit tests pin single examples; the steward bar (repo convention in
//! `grmpl-store`/`-diff`/`-proc`) is a seedable-xorshift randomized-churn law
//! oracle that re-checks the invariant every round over random inputs. This
//! file is that oracle for all three `MatchInput` instances — `&[Value]`,
//! `AstInput`, and `BytesInput` — asserting:
//!
//!   1. **Well-founded measure.** For every cursor, `next()` lowers `measure()`
//!      by exactly the one item consumed, `measure()` reaches zero after exactly
//!      `initial-measure` steps, and `at_end()` holds iff `measure() == 0`.
//!   2. **Repeat terminates.** `Repeat` over random patterns and random inputs
//!      always returns (a non-terminating engine would hang the test), including
//!      the adversarial zero-consuming inner (`Seq([])`) that the measure guard
//!      exists to stop.
//!   3. **Cursor equivalence.** `run == run_in` and `parse == parse_in` for the
//!      `&[Value]` instance, and the three instances agree cell-for-cell when
//!      run over the same underlying sequence of values.
//!
//! Every assertion prints its `seed` so a failure replays directly, and a tiny
//! xorshift64* PRNG keeps the churn reproducible with no external `rand` dep.

use std::sync::Arc;

use grmpl_core::Value;
use grmpl_pattern::{AstInput, Bindings, BytesInput, Form, MatchInput, Pattern, Rule, VarId};

/// Deterministic, seedable PRNG (xorshift64*) — same idiom as the store
/// determinism oracle; reproducible churn with no `rand`/`proptest` dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point of xorshift.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> usize {
        (self.next_u64() % n) as usize
    }
}

// Small value domain, drawn from `Value::Int(0..VDOM)`, so literals collide with
// captured bytes: a `BytesInput` surfaces each byte as `Value::Int(byte)`, so a
// slice of the same `Value::Int`s and an `AstInput::forest` of that slice run the
// *same* pattern over the *same* items — letting the three instances be compared
// cell-for-cell. Kept tiny so matches happen but result sets stay bounded.
const VDOM: u64 = 4;

/// A random byte string over the small domain (length `0..=6`).
fn rand_bytes(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(7);
    (0..len).map(|_| rng.below(VDOM) as u8).collect()
}

/// The same byte string as a flat `Value::Int` row — the shape a `BytesInput`
/// surfaces, so a slice / `AstInput::forest` over it is directly comparable.
fn as_value_row(bytes: &[u8]) -> Vec<Value> {
    bytes.iter().map(|&b| Value::Int(i64::from(b))).collect()
}

/// A random pattern over `VarId(0..3)` and `Value::Int(0..VDOM)` literals.
///
/// Deliberately includes empty `Seq(vec![])` (a zero-consuming match) and
/// `Repeat`, so `Repeat` over a non-progressing inner — the exact case the
/// well-founded measure exists to stop — is generated and must still terminate.
fn rand_pattern(rng: &mut Rng, depth: u32) -> Pattern {
    // At depth 0, only leaves (bounds the tree and the result blow-up).
    let arms = if depth == 0 { 2 } else { 6 };
    match rng.below(arms) {
        0 => Pattern::Lit(Value::Int(rng.below(VDOM) as i64)),
        1 => Pattern::Bind(VarId(rng.below(3) as u32)),
        2 => {
            // Seq of 0..=2 sub-patterns — 0 is the zero-consuming case.
            let n = rng.below(3);
            Pattern::Seq((0..n).map(|_| rand_pattern(rng, depth - 1)).collect())
        }
        3 => {
            // Choice of 1..=2 alternatives.
            let n = 1 + rng.below(2);
            Pattern::Choice((0..n).map(|_| rand_pattern(rng, depth - 1)).collect())
        }
        4 => Pattern::Repeat(Box::new(rand_pattern(rng, depth - 1))),
        _ => {
            // Guard whose predicate is deterministic given the bindings, so it
            // is identical across instances run over the same values.
            let want = VarId(rng.below(3) as u32);
            let inner = rand_pattern(rng, depth - 1);
            Pattern::Guard(
                Box::new(inner),
                Arc::new(move |b: &Bindings| b.contains_key(&want)),
            )
        }
    }
}

// --- Invariant 1: well-founded progress measure -----------------------------

/// Walk a cursor to exhaustion, asserting the measure law at every step.
fn assert_well_founded<I: MatchInput>(cursor: I, seed: u64, what: &str) {
    let initial = cursor.measure();
    let mut cur = cursor;
    let mut steps = 0usize;
    loop {
        let m = cur.measure();
        assert_eq!(
            cur.at_end(),
            m == 0,
            "seed {seed} [{what}]: at_end() must equal (measure()==0)"
        );
        match cur.next() {
            Some((_, rest)) => {
                assert!(m > 0, "seed {seed} [{what}]: next() Some while measure()==0");
                assert!(!cur.at_end(), "seed {seed} [{what}]: next() Some at at_end()");
                assert_eq!(
                    rest.measure(),
                    m - 1,
                    "seed {seed} [{what}]: next() must lower measure by exactly one"
                );
                cur = rest;
                steps += 1;
            }
            None => {
                assert_eq!(m, 0, "seed {seed} [{what}]: next() None while measure()>0");
                break;
            }
        }
    }
    assert_eq!(
        steps, initial,
        "seed {seed} [{what}]: measure must bottom out in exactly initial-measure steps"
    );
}

#[test]
fn progress_measure_is_well_founded_for_all_instances() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let bytes = rand_bytes(&mut rng);
            let row = as_value_row(&bytes);

            // All three instances over the same underlying sequence.
            assert_well_founded(&row[..], seed, "slice");
            assert_well_founded(AstInput::forest(&row), seed, "ast-forest");
            let blob = Value::bytes(&bytes);
            assert_well_founded(BytesInput::of(&blob).unwrap(), seed, "bytes");
        }
    }
}

// --- Invariant 2: Repeat always terminates ----------------------------------

#[test]
fn repeat_always_terminates_under_random_inputs() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..30 {
            let inner = rand_pattern(&mut rng, 3);
            let star = Pattern::Repeat(Box::new(inner));
            let bytes = rand_bytes(&mut rng);
            let row = as_value_row(&bytes);
            let blob = Value::bytes(&bytes);

            // Merely *returning* proves termination; a non-progressing Repeat
            // would loop forever. Also sanity-check the results are well-formed:
            // the zero-repetition is always present and no branch grew the input.
            for (what, results) in [
                ("slice", star.run_in(&row[..]).into_iter().map(|(b, r)| (b, r.measure())).collect::<Vec<_>>()),
                ("ast", star.run_in(AstInput::forest(&row)).into_iter().map(|(b, r)| (b, r.measure())).collect()),
                ("bytes", star.run_in(BytesInput::of(&blob).unwrap()).into_iter().map(|(b, r)| (b, r.measure())).collect()),
            ] {
                assert!(!results.is_empty(), "seed {seed} [{what}]: Repeat drops the zero-rep");
                for (_, rem) in &results {
                    assert!(*rem <= row.len(), "seed {seed} [{what}]: Repeat remainder grew");
                }
            }
        }
    }
}

#[test]
fn repeat_over_zero_consuming_inner_terminates() {
    // The adversarial case the measure guard exists for: an inner that matches
    // with no consumption (empty Seq, and a Choice that can pick it). Without the
    // `rest2.measure() >= rest.measure()` progress guard this would loop forever.
    let row = as_value_row(&[1, 2, 3]);
    for inner in [
        Pattern::Seq(vec![]),
        Pattern::Choice(vec![Pattern::Seq(vec![]), Pattern::Bind(VarId(0))]),
        Pattern::Repeat(Box::new(Pattern::Seq(vec![]))),
    ] {
        let star = Pattern::Repeat(Box::new(inner));
        // If this returns at all, termination holds.
        let results = star.run_in(&row[..]);
        assert!(!results.is_empty());
    }
}

// --- Invariant 3: cursor equivalence ----------------------------------------

/// Project a result set to `(bindings, remaining-measure)` so the three
/// instances (whose cursor types differ) become comparable. `Bindings` is a
/// `HashMap`, whose `PartialEq` is order-independent; the engine is structural,
/// so equal inputs yield equal result *order* too.
fn project<I: MatchInput>(results: Vec<(Bindings, I)>) -> Vec<(Bindings, usize)> {
    results.into_iter().map(|(b, r)| (b, r.measure())).collect()
}

#[test]
fn run_and_parse_agree_with_the_generic_engine() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let pat = rand_pattern(&mut rng, 3);
            let bytes = rand_bytes(&mut rng);
            let row = as_value_row(&bytes);

            // `run` is `run_in` at `I = &[Value]` — they must coincide exactly.
            assert_eq!(
                pat.run(&row),
                pat.run_in(&row[..]),
                "seed {seed}: run != run_in"
            );

            // A form whose ctor echoes a binding, so `parse` carries the match.
            let form = Form::new(vec![Rule::new(pat.clone(), |b: &Bindings| {
                b.get(&VarId(0)).cloned().unwrap_or(Value::Int(-1))
            })]);
            assert_eq!(
                form.parse(&row),
                form.parse_in(&row[..]),
                "seed {seed}: parse != parse_in"
            );
            assert_eq!(
                form.parse_all(&row),
                form.parse_all_in(&row[..]),
                "seed {seed}: parse_all != parse_all_in"
            );
        }
    }
}

#[test]
fn instances_agree_over_the_same_values() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let pat = rand_pattern(&mut rng, 3);
            let bytes = rand_bytes(&mut rng);
            let row = as_value_row(&bytes);
            let blob = Value::bytes(&bytes);

            // The same algebra over the same items via three different cursors.
            let via_slice = project(pat.run_in(&row[..]));
            let via_ast = project(pat.run_in(AstInput::forest(&row)));
            let via_bytes = project(pat.run_in(BytesInput::of(&blob).unwrap()));

            assert_eq!(via_slice, via_ast, "seed {seed}: slice vs ast-forest disagree");
            assert_eq!(via_slice, via_bytes, "seed {seed}: slice vs bytes disagree");
        }
    }
}
