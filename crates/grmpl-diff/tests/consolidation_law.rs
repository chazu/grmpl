//! **State-mode consolidation law oracle** (TKT-114; P9c design §4a, §7 test 2,
//! §8.2).
//!
//! State mode is "match the window's net contents": consolidate the edition
//! window `(from, to]` — fold `Σ diff` per tuple, drop zeros, sort — and hand
//! the survivors to the *existing* `&[Value]` slice engine. It needs no new
//! `MatchInput` instance: retraction is absorbed by **arithmetic, upstream of
//! the algebra**. This file is the acceptance test for that claim, in the shape
//! the repo convention requires (TKT-112): a **seeded-xorshift randomized-churn
//! law oracle** re-checking every invariant each round, not example unit tests.
//! Every assertion prints its `seed` and `round` so a failure replays directly,
//! and the PRNG is a hand-rolled xorshift64* so there is no `rand`/`proptest`
//! dependency.
//!
//! The laws, per round, over a world under randomized multiplicity churn:
//!
//! 1. **Consolidation ≡ snapshot/delta.** `consolidate_window(q, from, to)`
//!    equals `eval_snapshot(q, to)` restricted to the tuples `eval_delta(q,
//!    from, to)` touched. The implementation computes `anchor + delta`; the
//!    oracle recomputes the same value by the *other* route (`find` at `to`),
//!    so agreement is a real cross-check. Its degenerate case is checked
//!    separately and is the sharpest form: the **snapshot window**
//!    `(Edition::ZERO, to]` must equal `q.find(to)` exactly — design §5's claim
//!    that state mode unifies with today's tuple matching. A third, randomly
//!    chosen earlier anchor exercises overlapping/nested windows.
//!
//! 2. **Retraction absorbed by arithmetic.** A `+k` then `−k` of the same tuple
//!    *within* the window cancels to zero and is dropped — the pattern never
//!    sees it. Multiplicities fold: `+a` at one edition and `+b` at another
//!    surface as the single weight `a+b`. Both are injected every round with
//!    freshly-namespaced tuples, so neither law can go vacuous. Cancellation is
//!    injected a *second* way, on a tuple that was **already present** before
//!    the window: it must still be absent, even though it is present in the
//!    world at `to`. That is the touched-set boundary — these are the window's
//!    *net contents*, "what the window did", not the whole world — and the
//!    fresh-tuple injection alone cannot pin it, since with a zero anchor both
//!    readings coincide.
//!
//! 3. **Snapshot anchoring** — the load-bearing correctness trap of §4a. A
//!    tuple asserted *before* `from` and retracted *inside* the window must not
//!    produce a spurious negative row: the in-window retract resolves against
//!    the anchor to weight zero. Every round injects exactly that shape and
//!    asserts (a) the **unanchored** consolidation really does emit the
//!    negative row — the built-in negative control, so "just return the raw
//!    delta" is a mutant this oracle kills — and (b) the anchored consolidation
//!    emits no row at all. A per-seed counter asserts the control fired.
//!
//! 4. **Determinism.** Rows come back strictly tuple-ascending and identical
//!    across repeated evaluation, so no `HashMap` fold order leaks into the
//!    result (`to_sorted_vec`).
//!
//! 5. **It feeds the existing engine.** The consolidated window converts to the
//!    flat `&[Value]` sequence `Pattern::run_in` already matches: length equals
//!    the summed multiplicity, `Repeat` terminates over it with a parse that
//!    consumes the whole window (the §3 "the bound is the window" guarantee),
//!    and a literal pattern over a present row matches it. `grmpl-pattern` is a
//!    **dev-dependency only** — the bright line forbids the algebra in
//!    `grmpl-diff`'s production graph.
//!
//! Laws 1, 3(no-negative-rows), 4 and 5 run over two queries — a bare relation
//! and a `filter`+`distinct` (a non-linear boundary, where `eval_delta`
//! recomputes rather than passes through). The tuple-level injections of laws 2
//! and 3 are asserted against the bare relation, where multiplicity is not
//! collapsed by `distinct`.

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{
    consolidate_window, eval_delta, eval_snapshot, multiset, ConsolidatedWindow, Query, Snapshot,
};
use grmpl_pattern::{MatchInput, Pattern, VarId};
use grmpl_store::FjallStore;

const REL: RelId = RelId(1);
const SEEDS: std::ops::Range<u64> = 1..25; // 24 seeds
const ROUNDS: usize = 12;

/// Deterministic, seedable PRNG (xorshift64*) — the repo idiom; reproducible
/// churn with no external dependency.
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

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn tup(a: i64, b: u64) -> Tuple {
    Tuple::from([Value::Int(a), Value::Ent(Entity(b))])
}

/// The world the churn maintains, so the generator can keep every weight
/// non-negative (a realistic trace) and still produce multiplicities > 1.
type World = HashMap<Tuple, Diff>;

fn apply(world: &mut World, t: &Tuple, d: Diff) {
    let e = world.entry(t.clone()).or_insert(0);
    *e += d;
}

fn weight(world: &World, t: &Tuple) -> Diff {
    world.get(t).copied().unwrap_or(0)
}

/// Commit one update and mirror it into the model world.
fn commit1(store: &FjallStore, world: &mut World, t: &Tuple, d: Diff) {
    store.commit(&[(REL, t.clone(), d)]).unwrap();
    apply(world, t, d);
}

/// A batch of 1–3 random churn updates over a small overlapping tuple domain,
/// committed as one edition. Magnitudes run to 3 (so `|diff| > 1` folds are
/// exercised) and retractions never drive a weight below zero.
fn churn(store: &FjallStore, world: &mut World, rng: &mut Rng) {
    let n = 1 + rng.below(3);
    let mut batch: Vec<(RelId, Tuple, Diff)> = Vec::new();
    let mut pending = World::new();
    for _ in 0..n {
        let t = tup(rng.below(4) as i64, rng.below(3));
        let mag = 1 + rng.below(3) as Diff;
        let held = weight(world, &t) + weight(&pending, &t);
        let d = if held >= mag && rng.below(2) == 0 { -mag } else { mag };
        apply(&mut pending, &t, d);
        batch.push((REL, t, d));
    }
    store.commit(&batch).unwrap();
    for (_, t, d) in &batch {
        apply(world, t, *d);
    }
}

/// The unanchored consolidation the design warns about: fold the window's own
/// deltas in isolation. Used only as the oracle's **negative control**.
fn naive_window(q: &Query, store: &FjallStore, from: Edition, to: Edition) -> Vec<(Tuple, Diff)> {
    let mut d = eval_delta(q, store, from, to).unwrap();
    multiset::strip_zeros(&mut d);
    multiset::to_sorted_vec(&d)
}

/// Law 1: the window equals `find(q, to)` restricted to the tuples it touched —
/// recomputed by the snapshot route, not the `anchor + delta` route the
/// implementation takes.
fn assert_equals_snapshot_restricted(
    q: &Query,
    store: &FjallStore,
    cw: &ConsolidatedWindow,
    what: &str,
) {
    let delta = eval_delta(q, store, cw.from(), cw.to()).unwrap();
    let snap_to = eval_snapshot(q, store, cw.to()).unwrap();
    let mut want = multiset::Multiset::new();
    for (t, d) in &delta {
        if *d != 0 {
            multiset::add(&mut want, t.clone(), snap_to.get(t).copied().unwrap_or(0));
        }
    }
    multiset::strip_zeros(&mut want);
    assert_eq!(
        cw.rows(),
        multiset::to_sorted_vec(&want).as_slice(),
        "{what}: consolidated window != find(to) restricted to the touched tuples"
    );
}

/// Laws 4 and 5: determinism, and that the result really feeds the v1 engine.
fn assert_deterministic_and_matchable(
    q: &Query,
    store: &FjallStore,
    cw: &ConsolidatedWindow,
    what: &str,
) {
    // 4. Stable tuple-sorted order, zero-free, and stable across re-evaluation
    //    (a second `HashMap` fold with a different iteration order).
    assert!(
        cw.rows().windows(2).all(|w| w[0].0 < w[1].0),
        "{what}: rows are not strictly tuple-ascending (determinism)"
    );
    assert!(
        cw.rows().iter().all(|(_, d)| *d != 0),
        "{what}: a zero-weight row survived strip_zeros"
    );
    let again = consolidate_window(q, store, cw.from(), cw.to()).unwrap();
    assert_eq!(*cw, again, "{what}: re-evaluating the same window gave a different result");

    // 5. The `&[Value]` feed: one `Value::Tuple` per present row, repeated by
    //    multiplicity, matchable by the existing engine.
    let vals = cw.values();
    let total: i64 = cw.rows().iter().map(|(_, d)| (*d).max(0)).sum();
    assert_eq!(vals.len(), total as usize, "{what}: values() != summed multiplicity");
    assert!(
        vals.iter().all(|v| matches!(v, Value::Tuple(_))),
        "{what}: values() must surface each row as a Value::Tuple"
    );
    // `Repeat` terminates over a window (§3) and can consume all of it.
    let all = Pattern::Repeat(Box::new(Pattern::Bind(VarId(0))));
    assert!(
        all.run_in(&vals[..]).iter().any(|(_, rest)| rest.at_end()),
        "{what}: Repeat over the window has no parse consuming it to the end"
    );
    // A present row is matchable as a literal. The engine matches at the
    // cursor, so the head of the sequence — the least present row — matches
    // exactly once and consumes exactly one value.
    if let Some(head) = vals.first() {
        let lit = Pattern::Lit(head.clone());
        let parses = lit.run_in(&vals[..]);
        assert_eq!(parses.len(), 1, "{what}: a literal over the window head did not match");
        assert_eq!(
            parses[0].1.len(),
            vals.len() - 1,
            "{what}: a literal match consumed other than one value"
        );
    }
}

#[test]
fn state_mode_consolidation_law() {
    // q1: the bare relation — plain state mode, multiplicity preserved.
    let q1 = Query::rel(REL);
    // q2: a non-linear boundary, where `eval_delta` recomputes rather than
    //     passing a child delta through, and weights collapse to presence.
    let q2 = Query::rel(REL)
        .filter(|t| matches!(t.as_slice()[0], Value::Int(n) if n % 2 == 0))
        .distinct();

    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed);
        let mut world = World::new();
        let mut anchors: Vec<Edition> = vec![Edition::ZERO];
        // Anti-vacuity: how many rounds the *unanchored* control actually
        // emitted a spurious negative row. If this stays zero the anchoring law
        // proved nothing, so it is asserted at the end of the seed.
        let mut control_fired = 0usize;

        // The pre-window assert that the next round retracts in-window: the
        // §4a caveat generator. Namespaced away from the churn domain so no
        // other commit can touch it.
        let mut victim: Option<(Tuple, Diff)> = None;

        // A tuple that is present *throughout*, which every window churns and
        // then un-churns. It pins the touched-set boundary: cancelling changes
        // to an already-present tuple leave it out of the window's contents,
        // even though it never leaves the world.
        let carry_t = tup(3_000, seed % 7);
        let carry_w: Diff = 2;
        commit1(&store, &mut world, &carry_t, carry_w);

        for round in 0..ROUNDS {
            let ctx = format!("seed {seed} round {round}");

            // ---- before the window ------------------------------------------
            churn(&store, &mut world, &mut rng);
            let from = store.current();
            anchors.push(from);

            // ---- inside the window ------------------------------------------
            churn(&store, &mut world, &mut rng);

            // Law 2a: assert then retract the same tuple within the window.
            let cancel_t = tup(1_000 + round as i64, seed % 7);
            let k = 1 + rng.below(3) as Diff;
            commit1(&store, &mut world, &cancel_t, k);
            commit1(&store, &mut world, &cancel_t, -k);

            // Law 2c: the same cancellation on an *already present* tuple —
            // the touched-set boundary. It stays in the world at `to`, but the
            // window did nothing net to it, so it is not window content.
            let j = 1 + rng.below(3) as Diff;
            commit1(&store, &mut world, &carry_t, j);
            commit1(&store, &mut world, &carry_t, -j);

            // Law 2b: a multiplicity folded across two editions.
            let mult_t = tup(2_000 + round as i64, seed % 7);
            let (m1, m2) = (1 + rng.below(3) as Diff, 1 + rng.below(3) as Diff);
            commit1(&store, &mut world, &mult_t, m1);
            commit1(&store, &mut world, &mult_t, m2);

            // Law 3: retract, inside the window, a tuple asserted before it.
            let retracted = victim.take();
            if let Some((v, w)) = &retracted {
                assert_eq!(weight(&world, v), *w, "{ctx}: victim weight drifted before retraction");
                commit1(&store, &mut world, v, -w);
            }
            // This round's multiplicity tuple becomes the next round's victim:
            // by then it is a strictly pre-window assert.
            victim = Some((mult_t.clone(), m1 + m2));

            let to = store.current();

            // ---- laws --------------------------------------------------------
            for (name, q) in [("q1(rel)", &q1), ("q2(filter+distinct)", &q2)] {
                let what = format!("{ctx} {name}");
                let cw = consolidate_window(q, &store, from, to).unwrap();

                // 1. Consolidation ≡ snapshot/delta, over three windows: this
                //    round's, the degenerate snapshot window, and a random
                //    earlier anchor (overlapping / nested).
                assert_equals_snapshot_restricted(q, &store, &cw, &what);

                let full = consolidate_window(q, &store, Edition::ZERO, to).unwrap();
                let find_to = q.find(&Snapshot::new(&store, to)).unwrap();
                assert_eq!(
                    full.rows(),
                    find_to.as_slice(),
                    "{what}: the snapshot window (ZERO, to] must equal find(q, to)"
                );

                let earlier = anchors[rng.below(anchors.len() as u64) as usize];
                let wide = consolidate_window(q, &store, earlier, to).unwrap();
                assert_equals_snapshot_restricted(q, &store, &wide, &format!("{what} wide"));

                // 3. Anchoring: no anchored row is negative (this world only
                //    ever holds non-negative weights and neither query
                //    negates), while the unanchored control does go negative.
                assert!(
                    cw.rows().iter().all(|(_, d)| *d > 0),
                    "{what}: anchored consolidation emitted a non-positive row: {:?}",
                    cw.rows()
                );
                if naive_window(q, &store, from, to).iter().any(|(_, d)| *d < 0) {
                    control_fired += 1;
                }

                // 4, 5.
                assert_deterministic_and_matchable(q, &store, &cw, &what);
            }

            // Tuple-level injections, asserted against the bare relation where
            // `distinct` does not collapse multiplicity.
            let cw = consolidate_window(&q1, &store, from, to).unwrap();
            assert!(
                !cw.rows().iter().any(|(t, _)| *t == cancel_t),
                "{ctx}: a +{k}/-{k} pair within the window survived consolidation"
            );
            assert_eq!(
                cw.rows().iter().find(|(t, _)| *t == mult_t).map(|(_, d)| *d),
                Some(m1 + m2),
                "{ctx}: multiplicity did not fold to {m1}+{m2}"
            );
            assert!(
                !cw.rows().iter().any(|(t, _)| *t == carry_t),
                "{ctx}: a +{j}/-{j} pair on an already-present tuple became window content \
                 (the window's net contents are what it changed, not the whole world)"
            );
            assert_eq!(
                q1.find(&Snapshot::new(&store, to))
                    .unwrap()
                    .iter()
                    .find(|(t, _)| *t == carry_t)
                    .map(|(_, d)| *d),
                Some(carry_w),
                "{ctx}: the carried tuple should still be present at `to` (generator bug)"
            );
            if let Some((v, w)) = &retracted {
                // The negative control, pinned to the exact tuple: unanchored
                // consolidation emits the spurious `-w`...
                assert_eq!(
                    naive_window(&q1, &store, from, to)
                        .iter()
                        .find(|(t, _)| t == v)
                        .map(|(_, d)| *d),
                    Some(-w),
                    "{ctx}: the unanchored control did not produce the spurious negative \
                     (the anchoring law would be vacuous)"
                );
                // ...and anchoring resolves it to nothing at all.
                assert!(
                    !cw.rows().iter().any(|(t, _)| t == v),
                    "{ctx}: a pre-window assert retracted in-window surfaced as a row"
                );
                assert!(
                    !q1.find(&Snapshot::new(&store, to))
                        .unwrap()
                        .iter()
                        .any(|(t, _)| t == v),
                    "{ctx}: the retracted tuple is still present at `to` (generator bug)"
                );
            }
        }

        assert!(
            control_fired > 0,
            "seed {seed}: the unanchored control never went negative, so the \
             snapshot-anchoring law was never exercised"
        );
    }
}

// ---- witnesses -----------------------------------------------------------

/// The §4a caveat in three commits: a tuple asserted *before* the window and
/// retracted *inside* it. Unanchored consolidation reports `(t, −1)` — a tuple
/// "present −1 times", which is not a thing. Anchored consolidation reports
/// nothing, matching `find` at `to`.
#[test]
fn witness_pre_window_retraction_never_goes_negative() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let q = Query::rel(REL);
    let t = tup(7, 1);

    store.commit(&[(REL, t.clone(), 1)]).unwrap(); // before the window
    let from = store.current();
    store.commit(&[(REL, t.clone(), -1)]).unwrap(); // inside it
    let to = store.current();

    assert_eq!(naive_window(&q, &store, from, to), vec![(t.clone(), -1)]);

    let cw = consolidate_window(&q, &store, from, to).unwrap();
    assert!(cw.is_empty(), "anchored window should be empty, got {:?}", cw.rows());
    assert!(cw.values().is_empty());
    assert!(q.find(&Snapshot::new(&store, to)).unwrap().is_empty());
}

/// The snapshot window of §5: `(ZERO, E]` *is* `find(q, E)`, and its `values()`
/// are matched by the ordinary slice engine — state mode unifying with today's
/// tuple matching, with a within-window cancellation absorbed on the way.
#[test]
fn witness_snapshot_window_is_find_and_feeds_the_slice_engine() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let q = Query::rel(REL);
    let (a, b, gone) = (tup(1, 1), tup(2, 2), tup(3, 3));

    store.commit(&[(REL, a.clone(), 2)]).unwrap(); // multiplicity 2
    store.commit(&[(REL, gone.clone(), 1)]).unwrap();
    store.commit(&[(REL, b.clone(), 1)]).unwrap();
    store.commit(&[(REL, gone.clone(), -1)]).unwrap(); // cancels within the window
    let to = store.current();

    let cw = consolidate_window(&q, &store, Edition::ZERO, to).unwrap();
    assert_eq!(cw.rows(), &[(a.clone(), 2), (b.clone(), 1)][..]);
    assert_eq!(cw.rows(), q.find(&Snapshot::new(&store, to)).unwrap().as_slice());

    // Multiplicity 2 means the tuple appears twice in the matched sequence.
    let vals = cw.values();
    assert_eq!(
        vals,
        vec![
            Value::Tuple(a.0.clone()),
            Value::Tuple(a.0.clone()),
            Value::Tuple(b.0.clone()),
        ]
    );

    // The existing `&[Value]` engine matches it with no knowledge of signs.
    let p = Pattern::Seq(vec![
        Pattern::Lit(Value::Tuple(a.0.clone())),
        Pattern::Lit(Value::Tuple(a.0.clone())),
        Pattern::Bind(VarId(0)),
    ]);
    let parses = p.run_in(&vals[..]);
    assert_eq!(parses.len(), 1);
    assert!(parses[0].1.at_end());
    assert_eq!(parses[0].0.get(&VarId(0)), Some(&Value::Tuple(b.0.clone())));
}
