//! **Non-linear deltas through version compare.**
//!
//! `eval_delta` answered the non-linear operators — `distinct`, `Reduce`,
//! `Iterate` — by recomputing the boundary at *each end* and differencing, which
//! is `O(state)` per pump on exactly the operators the reactive pump drives.
//! Meanwhile the substrate had a subtree-pruned version compare that cost the
//! size of the *edit*, with no caller outside its own tests and no way to reach
//! it from above the bright line at all.
//!
//! Two things changed. `TraceStore::compare` is now the contract (default: read
//! both ends and difference; the Ent: prune shared subtrees), and `distinct` over
//! a base relation is answered from it directly:
//!
//! > `distinct(A@e)(t) = [A@e(t) > 0]`, so the delta at `t` is
//! > `[A@to(t) > 0] − [A@from(t) > 0]`, which is **zero whenever
//! > `A@to(t) == A@from(t)`**. Only tuples whose weight actually changed can
//! > contribute — and that set is exactly what `compare` returns.
//!
//! That is an identity, not an approximation, so these are the tests that
//! matter: **the optimized path must give the same answer as the recompute it
//! replaced**, under churn including the cases that make `distinct` non-linear —
//! weights crossing zero in both directions, weights changing *without* crossing
//! it (which must contribute nothing), and tuples retracted and re-asserted
//! inside one interval (also nothing).
//!
//! Every law runs against two substrates: an `EntStore`, where `compare`,
//! `reader_at` and `touched_since` are all overridden, and a `Plain` wrapper that
//! leaves every one of them at its trait default. Both must agree — the
//! optimization is a substrate concern and may not be observable in the answer.

use grmpl_core::{Diff, Edition, EditionStore, RelId, Result, TraceStore, Tuple, Update, Value};
use grmpl_diff::{eval_delta, eval_snapshot, multiset, Agg, Query};
use grmpl_ent::EntStore;

// ---------------------------------------------------------------------------
// Substrates
// ---------------------------------------------------------------------------

/// A store with **no overrides**: it delegates the four required operations to
/// an `EntStore` and leaves `compare`, `reader_at`, `read_range`,
/// `read_range_on` and `touched_since` at their trait defaults.
struct Plain(EntStore);

impl EditionStore for Plain {
    fn current(&self) -> Edition {
        self.0.current()
    }
}

impl TraceStore for Plain {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.0.commit(updates)
    }
    fn commit_if(
        &self,
        pre: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        self.0.commit_if(pre, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.0.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.0.scan_updates(rel, from, to)
    }
}

/// A named way to build a fresh store: tests take a factory rather than a store,
/// so every seed starts from an empty world.
type Substrate = (&'static str, fn() -> Box<dyn TraceStore>);

/// A fresh store of each substrate.
fn substrates() -> Vec<Substrate> {
    vec![
        ("ent", || Box::new(EntStore::new())),
        ("defaults", || Box::new(Plain(EntStore::new()))),
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A self-contained xorshift64* — deterministic per seed, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn in_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
}

const SEEDS: std::ops::Range<u64> = 1..17;
const R: RelId = RelId(1);
const S: RelId = RelId(2);

fn row(a: i64, b: i64) -> Tuple {
    Tuple::from([Value::Int(a), Value::Int(b)])
}

/// `snapshot(to) − snapshot(from)` computed the slow, obviously-correct way —
/// the independent model every law here is checked against.
fn boundary_difference(
    q: &Query,
    store: &dyn TraceStore,
    from: Edition,
    to: Edition,
) -> Vec<(Tuple, Diff)> {
    let mut out = eval_snapshot(q, store, to).unwrap();
    for (t, d) in &eval_snapshot(q, store, from).unwrap() {
        multiset::add(&mut out, t.clone(), -d);
    }
    multiset::strip_zeros(&mut out);
    multiset::to_sorted_vec(&out)
}

fn delta(q: &Query, store: &dyn TraceStore, from: Edition, to: Edition) -> Vec<(Tuple, Diff)> {
    multiset::to_sorted_vec(&eval_delta(q, store, from, to).unwrap())
}

/// Commit randomized churn into `rel`, returning every edition boundary — so a
/// law can be checked over *every* interval, not just the last one. A lagging
/// watch cursor produces exactly those wide intervals.
fn churn(store: &dyn TraceStore, rng: &mut Rng, rel: RelId, rounds: u64) -> Vec<Edition> {
    let mut marks = vec![store.current()];
    for _ in 0..rounds {
        let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for _ in 0..rng.in_range(1, 5) {
            let t = row(rng.in_range(0, 7) as i64, rng.in_range(0, 3) as i64);
            // Weights in [-2, 2] \ {0}: enough to cross zero, and enough to
            // change a weight without crossing it.
            let d = [-2i64, -1, 1, 2][rng.in_range(0, 3) as usize];
            ups.push((rel, t, d));
        }
        store.commit(&ups).unwrap();
        marks.push(store.current());
    }
    marks
}

/// Assert a query's delta equals its boundary difference over every interval.
fn matches_over_every_interval(
    q: &Query,
    store: &dyn TraceStore,
    marks: &[Edition],
    ctx: &str,
) {
    for (i, &from) in marks.iter().enumerate() {
        for &to in &marks[i..] {
            assert_eq!(
                delta(q, store, from, to),
                boundary_difference(q, store, from, to),
                "{ctx}: delta over ({}, {}]",
                from.0,
                to.0
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Laws
// ---------------------------------------------------------------------------

/// **The law.** `Δdistinct` from the version compare equals
/// `distinct(to) − distinct(from)`, over randomized churn, at every interval.
#[test]
fn distinct_delta_equals_the_boundary_difference_under_churn() {
    for (name, make) in substrates() {
        for seed in SEEDS {
            let store = make();
            let mut rng = Rng::new(seed);
            let rounds = rng_rounds(&mut rng);
            let marks = churn(&*store, &mut rng, R, rounds);
            matches_over_every_interval(
                &Query::rel(R).distinct(),
                &*store,
                &marks,
                &format!("{name} seed={seed}"),
            );
        }
    }
}

fn rng_rounds(rng: &mut Rng) -> u64 {
    rng.in_range(8, 18)
}

/// The same law for the **range-restricted** base shapes, whose boundary is the
/// relation's state restricted to a span — so the compare result has to be
/// filtered by exactly the predicate the snapshot applies. An off-by-one in that
/// filter would show up here and nowhere else.
#[test]
fn range_restricted_distinct_delta_matches_under_churn() {
    let ranged =
        Query::RangeRel { rel: R, lo: row(2, i64::MIN), hi: row(5, i64::MIN) }.distinct();
    let on_col =
        Query::RangeRelOn { rel: R, col: 1, lo: Value::Int(1), hi: Value::Int(3) }.distinct();

    for (name, make) in substrates() {
        for seed in SEEDS {
            let store = make();
            let mut rng = Rng::new(seed ^ 0xBEEF);
            let rounds = rng_rounds(&mut rng);
            let marks = churn(&*store, &mut rng, R, rounds);
            for (label, q) in [("RangeRel", &ranged), ("RangeRelOn", &on_col)] {
                matches_over_every_interval(
                    q,
                    &*store,
                    &marks,
                    &format!("{name} seed={seed} {label}"),
                );
            }
        }
    }
}

/// A `distinct` over a **compound** input keeps the boundary recompute, because
/// its boundary is not a relation's state. Checked so the fast path's guard
/// cannot silently swallow a shape it does not apply to.
#[test]
fn distinct_over_a_compound_input_still_matches() {
    for (name, make) in substrates() {
        let store = make();
        let q = Query::rel(R).join(Query::rel(S), vec![1], vec![0]).distinct();

        let mut marks = vec![store.current()];
        for k in 0..6i64 {
            store.commit(&[(R, row(k, k % 3), 1), (S, row(k % 3, k), 1)]).unwrap();
            marks.push(store.current());
        }
        // Retract some, so the compound boundary genuinely moves both ways.
        for k in 0..3i64 {
            store.commit(&[(R, row(k, k % 3), -1)]).unwrap();
            marks.push(store.current());
        }
        matches_over_every_interval(&q, &*store, &marks, name);
    }
}

/// The `quiet` short-circuit must never invent an empty delta. Over an interval
/// in which the view's *own* relations were untouched — while another relation
/// churned hard — every non-linear operator must still agree with its boundary
/// difference (which is empty); over an interval in which they *were* touched it
/// must not fire.
#[test]
fn the_quiet_short_circuit_agrees_with_the_boundary() {
    let distinct = Query::rel(R).distinct();
    let reduce = Query::rel(R).reduce(vec![0], Agg::Count);
    let iterate = Query::iterate(Query::rel(R), Query::recur());

    for (name, make) in substrates() {
        let store = make();
        for k in 0..5i64 {
            store.commit(&[(R, row(k, k), 1)]).unwrap();
        }
        let quiet_from = store.current();
        // Churn a relation the views do not read.
        for k in 0..10i64 {
            store.commit(&[(S, row(k, k), 1)]).unwrap();
        }
        let quiet_to = store.current();

        for q in [&distinct, &reduce, &iterate] {
            assert!(
                delta(q, &*store, quiet_from, quiet_to).is_empty(),
                "{name}: an untouched view has an empty delta"
            );
            assert_eq!(
                delta(q, &*store, quiet_from, quiet_to),
                boundary_difference(q, &*store, quiet_from, quiet_to),
                "{name}: …and that agrees with the boundary difference"
            );
        }

        // Now touch R: the short-circuit must not fire.
        store.commit(&[(R, row(99, 99), 1), (R, row(0, 0), -1)]).unwrap();
        let noisy_to = store.current();
        for q in [&distinct, &reduce, &iterate] {
            let d = delta(q, &*store, quiet_from, noisy_to);
            assert!(!d.is_empty(), "{name}: a touched view has a real delta");
            assert_eq!(
                d,
                boundary_difference(q, &*store, quiet_from, noisy_to),
                "{name}: and it is the boundary difference"
            );
        }
    }
}

/// **The `compare` contract.** It reports exactly the tuples whose net weight
/// differs, with both weights, **tuple-sorted** — the Determinism invariant, so
/// the answer cannot depend on physical scan order. Checked against `read_at` at
/// both ends, on both substrates, so the Ent's pruned descent and the generic
/// default are held to one specification.
#[test]
fn compare_reports_exactly_the_changed_tuples_sorted() {
    for (name, make) in substrates() {
        for seed in SEEDS {
            let store = make();
            let mut rng = Rng::new(seed ^ 0xC0FFEE);
            let rounds = rng_rounds(&mut rng);
            let marks = churn(&*store, &mut rng, R, rounds);

            for (i, &a) in marks.iter().enumerate() {
                for &b in &marks[i..] {
                    let got = store.compare(R, a, b).unwrap();

                    // Sorted, and no tuple twice.
                    let keys: Vec<&Tuple> = got.iter().map(|(t, _, _)| t).collect();
                    let mut sorted = keys.clone();
                    sorted.sort();
                    sorted.dedup();
                    assert_eq!(
                        keys, sorted,
                        "{name} seed={seed}: compare must be tuple-sorted and distinct"
                    );

                    // Exactly the tuples whose weight differs, with both weights.
                    let at = |e: Edition| -> std::collections::BTreeMap<Tuple, Diff> {
                        store
                            .read_at(R, e)
                            .unwrap()
                            .into_iter()
                            .filter(|(_, d)| *d != 0)
                            .collect()
                    };
                    let (wa, wb) = (at(a), at(b));
                    let all: std::collections::BTreeSet<Tuple> =
                        wa.keys().chain(wb.keys()).cloned().collect();
                    let want: Vec<(Tuple, Diff, Diff)> = all
                        .into_iter()
                        .map(|t| {
                            let x = *wa.get(&t).unwrap_or(&0);
                            let y = *wb.get(&t).unwrap_or(&0);
                            (t, x, y)
                        })
                        .filter(|(_, x, y)| x != y)
                        .collect();
                    assert_eq!(
                        got, want,
                        "{name} seed={seed}: compare over ({}, {}]",
                        a.0, b.0
                    );
                }
            }
        }
    }
}

/// A comparison against an edition below the consolidation watermark must error
/// at the same door `read_at` does — the fast path is not a way around the P6
/// guard.
#[test]
fn compare_below_the_watermark_errors_at_the_door() {
    for (name, make) in substrates() {
        let store = make();
        for k in 0..5i64 {
            store.commit(&[(R, row(k, k), 1)]).unwrap();
        }
        let early = store.current();
        store.commit(&[(R, row(9, 9), 1)]).unwrap();
        let now = store.current();
        if store.consolidate(now).unwrap().0 == 0 {
            continue; // a store that retains full history closes no door
        }
        assert!(
            store.compare(R, early, now).is_err(),
            "{name}: comparing below the watermark must error"
        );
        assert!(
            eval_delta(&Query::rel(R).distinct(), &*store, early, now).is_err(),
            "{name}: and so must a delta that would use it"
        );
    }
}

// ---------------------------------------------------------------------------
// Routing: is the fast path actually taken?
// ---------------------------------------------------------------------------

/// A store that counts which primitive the engine reached for. Correctness tests
/// prove the answers agree; this proves the *route*, which no amount of agreeing
/// can — a fast path that silently fell back would pass every law above.
struct Counting {
    inner: EntStore,
    reads: std::sync::atomic::AtomicUsize,
    compares: std::sync::atomic::AtomicUsize,
}

impl Counting {
    fn new() -> Counting {
        Counting {
            inner: EntStore::new(),
            reads: std::sync::atomic::AtomicUsize::new(0),
            compares: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn taken(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.reads.load(Relaxed), self.compares.load(Relaxed))
    }
    fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.reads.store(0, Relaxed);
        self.compares.store(0, Relaxed);
    }
}

impl EditionStore for Counting {
    fn current(&self) -> Edition {
        self.inner.current()
    }
}

impl TraceStore for Counting {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.inner.commit(updates)
    }
    fn commit_if(
        &self,
        pre: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        self.inner.commit_if(pre, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.inner.scan_updates(rel, from, to)
    }
    fn compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<(Tuple, Diff, Diff)>> {
        self.compares.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.compare(rel, a, b)
    }
}

/// **The routing law.** A `distinct` over a base relation is answered by one
/// `compare` and **no boundary read at all**; a `distinct` over a compound input
/// still reads both boundaries. Both must hold, or the fast path is either not
/// firing or firing where it does not apply.
#[test]
fn a_base_relation_distinct_is_answered_by_compare_not_by_reading_boundaries() {
    let store = Counting::new();
    for k in 0..20i64 {
        store.commit(&[(R, row(k, k % 4), 1), (S, row(k % 4, k), 1)]).unwrap();
    }
    let from = store.current();
    store.commit(&[(R, row(99, 0), 1), (R, row(0, 0), -1)]).unwrap();
    let to = store.current();

    // Base relation: one compare, zero reads. Neither boundary is materialized.
    store.reset();
    let d = delta(&Query::rel(R).distinct(), &store, from, to);
    assert_eq!(
        store.taken(),
        (0, 1),
        "a base-relation distinct costs one compare and no boundary read"
    );
    assert_eq!(d, boundary_difference(&Query::rel(R).distinct(), &store, from, to));

    // Range-restricted base shapes take the same route.
    for q in [
        Query::RangeRel { rel: R, lo: row(0, i64::MIN), hi: row(50, i64::MIN) }.distinct(),
        Query::RangeRelOn { rel: R, col: 1, lo: Value::Int(0), hi: Value::Int(2) }.distinct(),
    ] {
        store.reset();
        delta(&q, &store, from, to);
        assert_eq!(store.taken(), (0, 1), "a range-restricted distinct also routes through compare");
    }

    // Seen through a `Shared` node, which is transparent to delta computation:
    // `distinct(Shared(Rel))` is still a distinct over a base relation.
    store.reset();
    delta(&Query::rel(R).into_shared().distinct(), &store, from, to);
    assert_eq!(
        store.taken(),
        (0, 1),
        "a `Shared` wrapper does not hide the base relation from the fast path"
    );

    // A compound input does not qualify: its boundary is not a relation's state,
    // so both ends are recomputed and `compare` is not reached.
    store.reset();
    delta(
        &Query::rel(R).join(Query::rel(S), vec![1], vec![0]).distinct(),
        &store,
        from,
        to,
    );
    let (reads, compares) = store.taken();
    assert_eq!(compares, 0, "a compound distinct must not use the base-relation shortcut");
    assert!(reads > 0, "it recomputes both boundaries instead");
}
