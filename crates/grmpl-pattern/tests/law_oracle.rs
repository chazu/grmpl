//! MatchInput law oracles (TKT-95 rework of the P9a MatchInput abstraction).
//!
//! The `MatchInput` seam is the sole guarantee that `Repeat` terminates: its
//! progress `measure()` is a well-founded natural number that must strictly
//! decrease on every `next()` and hit zero exactly at the end. The in-crate
//! unit tests pin single examples; the steward bar (repo convention in
//! `grmpl-store`/`-diff`/`-proc`) is a seedable-xorshift randomized-churn law
//! oracle that re-checks the invariant every round over random inputs. This
//! file is that oracle for all four `MatchInput` instances — `&[Value]`,
//! `AstInput`, `BytesInput`, and `DeltaInput` (P9c event-mode delta windows) —
//! asserting:
//!
//!   1. **Well-founded measure.** For every cursor, `next()` lowers `measure()`
//!      by exactly the one item consumed, `measure()` reaches zero after exactly
//!      `initial-measure` steps, and `at_end()` holds iff `measure() == 0`.
//!   2. **Repeat terminates.** `Repeat` over random patterns and random inputs
//!      always returns (a non-terminating engine would hang the test), including
//!      the adversarial zero-consuming inner (`Seq([])`) that the measure guard
//!      exists to stop.
//!   3. **Cursor equivalence.** `run == run_in` and `parse == parse_in` for the
//!      `&[Value]` instance, and all four instances agree cell-for-cell when run
//!      over the same underlying sequence of values — for `DeltaInput`, whose
//!      window carries `Update`s rather than `Value`s, "the same sequence" is
//!      its reification, so the window and its reified row must match. That
//!      equivalence is the whole claim of P9c §4b: event mode is *another
//!      cursor* over the one algebra, never a second engine.
//!
//! The final section is the [`DeltaInput`]-specific oracle for TKT-113 (P9c
//! §4b, §8.1): over a random finite window of signed updates it pins (1)
//! well-founded progress (the termination guarantee for matching a delta
//! window), (2) **faithful reification** — each consumed delta is exactly
//! `Value::Tuple([sign, tuple, edition])`, in order, no loss/dup, with the
//! *sign* (not the raw multiplicity) in the first cell — and (3)
//! **determinism** — the same window, and any value-equal but independently
//! *allocated* window, yields an identical reified sequence.
//!
//! Every assertion prints its `seed` so a failure replays directly, and a tiny
//! xorshift64* PRNG keeps the churn reproducible with no external `rand` dep.

use std::sync::Arc;

use grmpl_core::{Time, Tuple, Update, Value};
use grmpl_pattern::{
    reify_delta, AstInput, Bindings, BytesInput, DeltaInput, Form, MatchInput, Pattern, Rule, VarId,
};

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

            // …and the fourth: `DeltaInput` is *just another cursor* over the
            // same algebra (P9c §4b). A delta window carries `Update`s, not
            // `Value`s, so the comparable "same underlying sequence" is its
            // reification — running the pattern over the window must equal
            // running it over that reified row through the plain slice cursor.
            // This is what makes event mode a cursor and not a second engine:
            // no `DeltaInput`-specific matching behaviour may exist.
            let window = rand_window(&mut rng);
            let reified: Vec<Value> = window.iter().map(reify_delta).collect();
            assert_eq!(
                project(pat.run_in(DeltaInput::over(&window))),
                project(pat.run_in(&reified[..])),
                "seed {seed}: delta-window vs its reified slice disagree"
            );
        }
    }
}

// --- DeltaInput (P9c §4b event-mode) law oracle (TKT-113) --------------------

/// A random signed update over the small value domain: a 1..=3-cell tuple, a
/// signed `diff` in `-3..=3`, and an arbitrary edition. A window is any finite
/// `Vec<Update>` — the laws below hold for an arbitrary slice, so the generator
/// does not constrain order or edition monotonicity.
///
/// The `diff` range is deliberately **not** `{+1, -1}`: on the unit range
/// `signum()` is the identity, so an implementation that wrote the raw `diff`
/// verbatim would satisfy the whole oracle. `TraceStore::commit` takes
/// `Diff = i64`, so `scan_updates` can genuinely yield a multiplicity of `+3` or
/// `-2`, and collapsing weight to *sign* is the load-bearing semantic choice of
/// event mode (P9c §4b) — so the generator must produce `|diff| > 1` to pin it.
/// `0` is included because [`reify_delta`]'s contract states the sign is total
/// at a net-zero delta.
fn rand_update(rng: &mut Rng) -> Update {
    let arity = 1 + rng.below(3);
    let cells: Vec<Value> = (0..arity).map(|_| Value::Int(rng.below(VDOM) as i64)).collect();
    let diff = rng.below(7) as i64 - 3;
    let edition = rng.next_u64() % 64;
    Update { tuple: Tuple::new(cells), time: Time::input(edition), diff }
}

/// A random finite window of updates (length `0..=6`).
fn rand_window(rng: &mut Rng) -> Vec<Update> {
    let len = rng.below(7);
    (0..len).map(|_| rand_update(rng)).collect()
}

/// Walk a `DeltaInput` to exhaustion, collecting each reified value in order.
fn drain(window: &[Update]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cur = DeltaInput::over(window);
    while let Some((v, rest)) = cur.next() {
        out.push(v);
        cur = rest;
    }
    out
}

/// **Law 1 — well-founded progress.** The termination guarantee for matching a
/// delta window: `measure()` strictly decreases by one per `next()`, bottoms out
/// in exactly `initial-measure` steps, never goes negative, never stalls, and
/// `at_end()` holds iff `measure() == 0`. Reuses the shared cursor walker.
#[test]
fn delta_input_progress_measure_is_well_founded() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let window = rand_window(&mut rng);
            assert_well_founded(DeltaInput::over(&window), seed, "delta");
        }
    }
}

/// **Law 1 (corollary) — `Repeat` terminates over a delta window.** Because the
/// window is finite and the measure is well-founded, `Repeat` of a random
/// pattern over the reified deltas always returns; a live/unbounded stream would
/// hang here — which is exactly why P9c forbids matching one (§3).
#[test]
fn repeat_over_delta_window_terminates() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..30 {
            let inner = rand_pattern(&mut rng, 3);
            let star = Pattern::Repeat(Box::new(inner));
            let window = rand_window(&mut rng);
            // Returning at all proves termination; also sanity-check no branch
            // grew the input and the zero-repetition is always present.
            let results = star.run_in(DeltaInput::over(&window));
            assert!(!results.is_empty(), "seed {seed} [delta]: Repeat drops the zero-rep");
            for (_, rest) in &results {
                assert!(rest.measure() <= window.len(), "seed {seed} [delta]: Repeat remainder grew");
            }
        }
    }
}

/// **Law 2 — faithful reification.** Each consumed delta is exactly
/// `Value::Tuple([sign, tuple, edition])` per §4b, one value per update, in the
/// window's order, with no loss and no duplication. The `iter` sub-coordinate of
/// `Time` is intentionally not surfaced (it is not a window axis).
#[test]
fn delta_input_reification_is_faithful_and_in_order() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let window = rand_window(&mut rng);
            let yielded = drain(&window);

            // One value per update — no loss, no dup — and index-for-index order.
            assert_eq!(
                yielded.len(),
                window.len(),
                "seed {seed}: reified count != window length"
            );
            for (i, (u, v)) in window.iter().zip(&yielded).enumerate() {
                // The cursor's value is exactly the reification helper's output.
                assert_eq!(*v, reify_delta(u), "seed {seed} [{i}]: cursor value != reify_delta");
                let Value::Tuple(cells) = v else {
                    panic!("seed {seed} [{i}]: reified delta must be a Value::Tuple");
                };
                assert_eq!(cells.len(), 3, "seed {seed} [{i}]: reified delta must be a 3-tuple");
                assert_eq!(
                    cells[0],
                    Value::Int(u.diff.signum()),
                    "seed {seed} [{i}]: sign field must be diff.signum()"
                );
                assert_eq!(
                    cells[1],
                    Value::Tuple(u.tuple.0.clone()),
                    "seed {seed} [{i}]: tuple field must be the update's cells"
                );
                assert_eq!(
                    cells[2],
                    Value::Int(u.time.edition as i64),
                    "seed {seed} [{i}]: edition field must be time.edition"
                );
            }
        }
    }
}

/// A value-equal copy of a window whose tuple cells are **freshly allocated**.
///
/// `Update::clone` (and so `Vec<Update>::clone`) is *not* enough to test content
/// purity: `Tuple` is `Tuple(Arc<[Value]>)`, so cloning only bumps the refcount
/// and hands back the *same* allocation. Rebuilding through
/// `Tuple::new(cells.to_vec())` forces a distinct backing buffer, so a
/// reification that keyed off an address (this repo has a confirmed `Arc`
/// pointer-identity ABA hazard on memo keys) would visibly diverge here.
fn realloc_window(window: &[Update]) -> Vec<Update> {
    window
        .iter()
        .map(|u| Update {
            tuple: Tuple::new(u.tuple.0.to_vec()),
            time: u.time,
            diff: u.diff,
        })
        .collect()
}

/// **Law 3 — determinism.** The reified sequence is a pure function of the
/// window: the same slice always yields the identical sequence, and a fresh
/// window built from value-equal (but distinctly-allocated) updates yields the
/// same result too — reification depends on content, never on address or any
/// scan-order-dependent state.
#[test]
fn delta_input_reified_sequence_is_deterministic() {
    for seed in 1..=32u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..40 {
            let window = rand_window(&mut rng);
            let once = drain(&window);
            let twice = drain(&window);
            assert_eq!(once, twice, "seed {seed}: same window yielded different sequences");

            // A value-equal, independently-*allocated* window must match too.
            let fresh = realloc_window(&window);
            for (u, v) in window.iter().zip(&fresh) {
                assert_eq!(u, v, "seed {seed}: realloc_window changed a value");
                assert!(
                    !Arc::ptr_eq(&u.tuple.0, &v.tuple.0),
                    "seed {seed}: realloc_window must not share the cells allocation"
                );
            }
            assert_eq!(once, drain(&fresh), "seed {seed}: reification is not content-pure");
        }
    }
}
