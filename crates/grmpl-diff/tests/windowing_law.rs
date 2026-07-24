//! **Windowing-layer law oracle** (TKT-115; P9c design §3, §5, §8.3).
//!
//! A pattern never runs over a live stream — it runs over a *window*, a finite
//! edition-bounded slice of the trace. This file is the acceptance test for the
//! window grammar itself: `window(from, to)` over `scan_updates`, the tumbling
//! and sliding families built on it, and the two materializations a window
//! admits. It is written in the shape the repo convention requires (TKT-112): a
//! **seeded-xorshift randomized-churn law oracle** re-checking every invariant
//! each round, not example unit tests. Every assertion prints its `seed` and
//! `round` so a failure replays directly, and the PRNG is a hand-rolled
//! xorshift64* so there is no `rand`/`proptest` dependency.
//!
//! The laws, per round, over a world under randomized insert/retract churn:
//!
//! 1. **Tumbling partition.** [`tumbling`] windows over `(from, to]` *partition*
//!    the range: they are contiguous (`wᵢ.to == wᵢ₊₁.from`), they start at
//!    `from` and end at `to`, every edition in scope lies in **exactly one** of
//!    them, and concatenating their event slices reproduces the full range's
//!    slice **verbatim, in order** — no loss, no duplication, no overlap, no
//!    reordering.
//!
//! 2. **Sliding coverage.** For [`sliding`] windows with `step ≤ size`, window
//!    *k* is anchored at `from + k·step`, and an update at edition `e` appears
//!    in exactly the windows whose range covers `e`. The oracle does not take
//!    that on faith from the implementation: it recomputes the expected
//!    membership count from *closed-form range arithmetic*
//!    (`⌊(d−1)/step⌋ − ⌈(d−size)/step⌉ + 1` for `d = e − from`) and asserts both
//!    the window count and the number of times the edition's updates actually
//!    surface across slices. Coverage is total (every edition is in ≥ 1 window),
//!    overlap is genuinely shared (a fixed `size=3, step=1` family runs every
//!    round so this can never go vacuous), and `step == size` degenerates to
//!    exactly the tumbling family.
//!
//! 3. **Edition-bounded and ordered.** Windows are pure edition ranges — no
//!    session gaps, no counts (both deferred, §5). A window's event slice
//!    contains exactly the updates whose edition lies in `(from, to]`, in
//!    **commit order** `(edition, counter)`: the oracle asserts it equals the
//!    *submitted batch order* recorded by the generator, edition by edition, so
//!    intra-edition order is pinned too, and re-materializing gives an identical
//!    `Vec` — no scan order or `HashMap` fold order leaks into a window.
//!
//! 4. **Materialization consistency.** The event slice and the consolidated
//!    tuple-set are two readings of one window and must agree — *modulo the
//!    anchor*, which is the honest statement of this law and worth spelling out,
//!    because the naive version is false:
//!
//!    * `consolidate_events(slice)` (the unanchored fold) equals
//!      `eval_delta` over the same range — the event path and the delta path
//!      compute the same multiset.
//!    * `Window::consolidate` equals `snapshot(from) + that fold`, restricted to
//!      the touched tuples — the §4a snapshot anchoring TKT-114 landed.
//!    * On the **snapshot window** `(Edition::ZERO, to]` the anchor is the empty
//!      world, so the two materializations coincide *exactly*, and both equal
//!      `find(q, to)`.
//!
//!    A per-seed counter asserts the anchored and unanchored readings really did
//!    diverge somewhere, so the first two bullets cannot pass vacuously by the
//!    anchor always being zero.
//!
//! 5. **It feeds the engines.** The event slice materialization is accepted by
//!    `grmpl_pattern::DeltaInput` (measure == remaining updates, `Repeat`
//!    terminates consuming the whole window — the §3 "the bound is the window"
//!    guarantee) and the consolidated one by the `&[Value]` slice engine.
//!    `grmpl-pattern` is a **dev-dependency only**: the bright line forbids the
//!    algebra in `grmpl-diff`'s production graph, and `grmpl-diff` reaches the
//!    trace solely through the `TraceStore` **trait**.

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Update, Value};
use grmpl_diff::{
    consolidate_events, eval_delta, eval_snapshot, multiset, sliding, tumbling, window, Query,
    Snapshot, Window,
};
use grmpl_pattern::{DeltaInput, MatchInput, Pattern, VarId};
use grmpl_store::FjallStore;

const REL: RelId = RelId(1);
const SEEDS: std::ops::Range<u64> = 1..25; // 24 seeds
const ROUNDS: usize = 10;

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

/// The world the churn maintains, so the generator keeps every weight
/// non-negative (a realistic trace) while still producing multiplicities > 1.
type World = HashMap<Tuple, Diff>;

/// The generator's record of what it committed, in submit order, per edition —
/// the oracle's independent notion of commit order `(edition, counter)`.
type Log = Vec<(Edition, Vec<(Tuple, Diff)>)>;

fn apply(world: &mut World, t: &Tuple, d: Diff) {
    *world.entry(t.clone()).or_insert(0) += d;
}

fn weight(world: &World, t: &Tuple) -> Diff {
    world.get(t).copied().unwrap_or(0)
}

/// Commit one edition of 1–3 random churn updates over a small overlapping tuple
/// domain. Magnitudes run to 3 (so `|diff| > 1` folds are exercised) and
/// retractions never drive a weight below zero.
fn churn_edition(store: &FjallStore, world: &mut World, rng: &mut Rng, log: &mut Log) {
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
    let at = store.commit(&batch).unwrap();
    for (_, t, d) in &batch {
        apply(world, t, *d);
    }
    log.push((at, batch.into_iter().map(|(_, t, d)| (t, d)).collect()));
}

/// The updates the generator committed in `(from, to]`, in submit order — the
/// oracle's expectation for a window's event slice, computed without consulting
/// the store at all.
fn expected_events(log: &Log, w: &Window) -> Vec<(Tuple, Diff)> {
    log.iter()
        .filter(|(at, _)| w.contains(*at))
        .flat_map(|(_, batch)| batch.iter().cloned())
        .collect()
}

fn as_pairs(ups: &[Update]) -> Vec<(Tuple, Diff)> {
    ups.iter().map(|u| (u.tuple.clone(), u.diff)).collect()
}

/// How many `sliding(from, to, size, step)` windows contain edition `e`,
/// computed from range arithmetic alone: window *k* covers
/// `(from + k·step, from + k·step + size]`, so with `d = e − from` the covering
/// `k` run from `⌈(d−size)/step⌉` to `⌊(d−1)/step⌋`.
fn covering_count(from: Edition, e: Edition, size: u64, step: u64) -> u64 {
    let d = e.0 - from.0;
    assert!(d >= 1, "edition {} is not above the anchor {}", e.0, from.0);
    let k_max = (d - 1) / step;
    let k_min = if d <= size { 0 } else { (d - size).div_ceil(step) };
    k_max + 1 - k_min
}

/// Law 1: the tumbling family partitions `(from, to]`.
fn assert_tumbling_partitions(
    store: &FjallStore,
    w: &Window,
    full: &[Update],
    size: u64,
    what: &str,
) {
    let ws = tumbling(w.from(), w.to(), size).unwrap();
    if w.is_empty() {
        assert!(ws.is_empty(), "{what}: an empty range must yield no tumbling windows");
        return;
    }
    assert_eq!(ws.first().unwrap().from(), w.from(), "{what}: tumbling does not start at `from`");
    assert_eq!(ws.last().unwrap().to(), w.to(), "{what}: tumbling does not end at `to`");
    assert!(
        ws.windows(2).all(|p| p[0].to() == p[1].from()),
        "{what}: tumbling windows are not contiguous (a gap or an overlap)"
    );
    assert!(
        ws.iter().all(|x| x.span() <= size && !x.is_empty()),
        "{what}: a tumbling window is empty or longer than the size"
    );

    // Every edition in scope is in exactly one window.
    for e in (w.from().0 + 1)..=w.to().0 {
        let n = ws.iter().filter(|x| x.contains(Edition(e))).count();
        assert_eq!(n, 1, "{what}: edition {e} lies in {n} tumbling windows, not exactly 1");
    }

    // Concatenating the slices reproduces the range's slice verbatim: no loss,
    // no duplication, and the commit order survives the split.
    let mut concat: Vec<Update> = Vec::new();
    for x in &ws {
        concat.extend(x.events(store, REL).unwrap());
    }
    assert_eq!(
        as_pairs(&concat),
        as_pairs(full),
        "{what}: concatenated tumbling slices != the whole window's slice"
    );
}

/// Law 2: the sliding family covers `(from, to]`, sharing overlaps exactly as
/// the range arithmetic says. Returns the number of updates that landed in more
/// than one window (the anti-vacuity signal for overlap).
fn assert_sliding_covers(
    store: &FjallStore,
    w: &Window,
    log: &Log,
    size: u64,
    step: u64,
    what: &str,
) -> usize {
    let ws = sliding(w.from(), w.to(), size, step).unwrap();
    if w.is_empty() {
        assert!(ws.is_empty(), "{what}: an empty range must yield no sliding windows");
        return 0;
    }
    for (k, x) in ws.iter().enumerate() {
        assert_eq!(
            x.from().0,
            w.from().0 + k as u64 * step,
            "{what}: sliding window {k} is not anchored at from + k·step"
        );
        assert_eq!(
            x.to().0,
            (x.from().0 + size).min(w.to().0),
            "{what}: sliding window {k} does not end at min(anchor + size, to)"
        );
        assert!(x.to() <= w.to(), "{what}: sliding window {k} escapes the range");
    }
    if step == size {
        assert_eq!(
            ws,
            tumbling(w.from(), w.to(), size).unwrap(),
            "{what}: step == size must degenerate to the tumbling family"
        );
    }

    let slices: Vec<Vec<Update>> = ws.iter().map(|x| x.events(store, REL).unwrap()).collect();

    let mut shared = 0usize;
    for (at, batch) in log.iter().filter(|(at, _)| w.contains(*at)) {
        let want = covering_count(w.from(), *at, size, step);
        assert!(want >= 1, "{what}: edition {} is covered by no window", at.0);
        let got = ws.iter().filter(|x| x.contains(*at)).count() as u64;
        assert_eq!(
            got, want,
            "{what}: edition {} lies in {got} sliding windows, range arithmetic says {want}",
            at.0
        );
        // …and the updates really do surface that many times, not just the
        // bounds: `want` copies of the edition's whole batch, in order.
        let occurrences: usize = slices
            .iter()
            .map(|slice| slice.iter().filter(|u| u.time.edition == at.0).count())
            .sum();
        assert_eq!(
            occurrences,
            want as usize * batch.len(),
            "{what}: edition {}'s {} updates surfaced {occurrences} times across slices, \
             expected {want} × {}",
            at.0,
            batch.len(),
            batch.len()
        );
        if want > 1 {
            shared += batch.len();
        }
    }
    shared
}

/// Law 3: a window's event slice is exactly the trace restricted to its edition
/// range, in commit order, deterministically.
fn assert_edition_bounded_and_ordered(store: &FjallStore, w: &Window, log: &Log, what: &str) {
    let full = w.events(store, REL).unwrap();
    assert!(
        full.iter().all(|u| w.contains(Edition(u.time.edition))),
        "{what}: an update outside (from, to] surfaced in the window"
    );
    assert!(
        full.windows(2).all(|p| p[0].time.edition <= p[1].time.edition),
        "{what}: the event slice is not in edition order"
    );
    // Against the generator's own record: same updates, same intra-edition
    // submit order — commit order (edition, counter), not scan order.
    assert_eq!(
        as_pairs(&full),
        expected_events(log, w),
        "{what}: the event slice is not the committed batches in commit order"
    );
    // Re-materializing gives an identical Vec (no HashMap/scan-order leak).
    assert_eq!(full, w.events(store, REL).unwrap(), "{what}: re-materialization differed");
}

/// Law 4: the event slice and the consolidated tuple-set agree, modulo the
/// anchor. Returns true if the anchored and unanchored readings differed here
/// (the anti-vacuity signal for anchoring).
fn assert_materializations_agree(store: &FjallStore, w: &Window, full: &[Update], what: &str) -> bool {
    let q = Query::rel(REL);
    let folded = consolidate_events(full);

    // The event path and the delta path compute the same multiset.
    let delta = eval_delta(&q, store, w.from(), w.to()).unwrap();
    assert_eq!(
        folded,
        multiset::to_sorted_vec(&delta),
        "{what}: folding the event slice != eval_delta over the same range"
    );

    // State mode is that fold anchored at `from`, restricted to touched tuples.
    let anchor = eval_snapshot(&q, store, w.from()).unwrap();
    let mut want = multiset::Multiset::new();
    for (t, d) in &folded {
        multiset::add(&mut want, t.clone(), anchor.get(t).copied().unwrap_or(0) + d);
    }
    multiset::strip_zeros(&mut want);
    let want = multiset::to_sorted_vec(&want);
    let cw = w.consolidate(&q, store).unwrap();
    assert_eq!(
        cw.rows(),
        want.as_slice(),
        "{what}: the consolidated window != snapshot(from) + fold(event slice)"
    );
    assert_eq!(cw.from(), w.from(), "{what}: consolidated window lost its anchor");
    assert_eq!(cw.to(), w.to(), "{what}: consolidated window lost its far end");

    folded != want
}

#[test]
fn windowing_layer_law() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed);
        let mut world = World::new();
        let mut log = Log::new();
        let mut anchors: Vec<Edition> = vec![Edition::ZERO];

        // Anti-vacuity counters, asserted at the end of the seed.
        let mut nonempty_windows = 0usize; // a window with updates in it
        let mut overlap_shared = 0usize; // an update in ≥ 2 sliding windows
        let mut anchored_differed = 0usize; // the anchor actually mattered

        for round in 0..ROUNDS {
            for _ in 0..(1 + rng.below(3)) {
                churn_edition(&store, &mut world, &mut rng, &mut log);
                anchors.push(store.current());
            }
            let to = store.current();
            let from = anchors[rng.below(anchors.len() as u64) as usize];
            let w = window(from, to).unwrap();
            let size = 1 + rng.below(4);
            let step = 1 + rng.below(size);
            let ctx = format!(
                "seed {seed} round {round} window ({}, {}] size {size} step {step}",
                from.0, to.0
            );

            let full = w.events(&store, REL).unwrap();
            if !full.is_empty() {
                nonempty_windows += 1;
            }

            // 3. Edition-bounded, commit-ordered, deterministic.
            assert_edition_bounded_and_ordered(&store, &w, &log, &ctx);

            // 1. Tumbling partitions the range.
            assert_tumbling_partitions(&store, &w, &full, size, &ctx);

            // 2. Sliding covers it, sharing overlaps by the range arithmetic —
            //    once with the random (size, step), and once with a fixed
            //    overlapping family so the overlap law can never go vacuous.
            overlap_shared += assert_sliding_covers(&store, &w, &log, size, step, &ctx);
            overlap_shared += assert_sliding_covers(&store, &w, &log, 3, 1, &format!("{ctx} [3/1]"));

            // 4. The two materializations agree, modulo the anchor.
            if assert_materializations_agree(&store, &w, &full, &ctx) {
                anchored_differed += 1;
            }

            // 4b. The snapshot window (ZERO, to] is where they coincide exactly,
            //     and both are `find(q, to)` — design §5's claim that state mode
            //     unifies with today's tuple matching.
            let q = Query::rel(REL);
            let snap_w = window(Edition::ZERO, to).unwrap();
            let snap_events = snap_w.events(&store, REL).unwrap();
            let snap_cw = snap_w.consolidate(&q, &store).unwrap();
            assert_eq!(
                snap_cw.rows(),
                consolidate_events(&snap_events).as_slice(),
                "{ctx}: on the snapshot window the two materializations must coincide"
            );
            assert_eq!(
                snap_cw.rows(),
                q.find(&Snapshot::new(&store, to)).unwrap().as_slice(),
                "{ctx}: the snapshot window must equal find(q, to)"
            );

            // 5. The event slice feeds the event-mode cursor: its measure is the
            //    remaining update count and `Repeat` terminates over it having
            //    consumed the whole window (§3 — the bound is the window).
            let input = DeltaInput::over(&full);
            assert_eq!(input.measure(), full.len(), "{ctx}: DeltaInput measure != window length");
            let all = Pattern::Repeat(Box::new(Pattern::Bind(VarId(0))));
            assert!(
                all.run_in(input).iter().any(|(_, rest)| rest.at_end()),
                "{ctx}: Repeat over the event slice has no parse consuming it to the end"
            );

            // The degenerate empty window `(to, to]`: in scope for nothing, and
            // every materialization of it is empty.
            let empty = window(to, to).unwrap();
            assert!(empty.is_empty() && empty.span() == 0, "{ctx}: (to, to] is not empty");
            assert!(empty.events(&store, REL).unwrap().is_empty(), "{ctx}: empty window has events");
            assert!(
                empty.consolidate(&q, &store).unwrap().is_empty(),
                "{ctx}: empty window has consolidated contents"
            );
            assert!(tumbling(to, to, size).unwrap().is_empty(), "{ctx}: empty range tumbled");
        }

        assert!(nonempty_windows > 0, "seed {seed}: every window was empty — the laws were vacuous");
        assert!(
            overlap_shared > 0,
            "seed {seed}: no update ever landed in two sliding windows, so the overlap law \
             proved nothing"
        );
        assert!(
            anchored_differed > 0,
            "seed {seed}: the anchored and unanchored readings never diverged, so the \
             materialization law never distinguished them"
        );
    }
}

// ---- witnesses -----------------------------------------------------------

/// Tumbling in the small: five editions, size 2 → `(0,2] (2,4] (4,5]`. Each
/// update lands in exactly one window and the concatenation is the whole range,
/// in order — the partition law with the randomness taken out.
#[test]
fn witness_tumbling_partitions_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    for i in 1..=5 {
        store.commit(&[(REL, tup(i, 1), 1)]).unwrap();
    }
    let to = store.current();

    let ws = tumbling(Edition::ZERO, to, 2).unwrap();
    assert_eq!(
        ws.iter().map(|w| (w.from().0, w.to().0)).collect::<Vec<_>>(),
        vec![(0, 2), (2, 4), (4, 5)]
    );

    let slices: Vec<Vec<i64>> = ws
        .iter()
        .map(|w| {
            w.events(&store, REL)
                .unwrap()
                .iter()
                .map(|u| match u.tuple.as_slice()[0] {
                    Value::Int(n) => n,
                    _ => unreachable!(),
                })
                .collect()
        })
        .collect();
    assert_eq!(slices, vec![vec![1, 2], vec![3, 4], vec![5]]);

    // The last window is the clamped tail: one edition, not two.
    assert_eq!(ws.last().unwrap().span(), 1);
}

/// Sliding in the small: size 3, step 1 over five editions. The update at
/// edition 3 is shared by three windows — overlap is real, not a rounding of the
/// bounds — and a `step > size` family, which would drop editions into no window
/// at all, is rejected rather than silently losing them.
#[test]
fn witness_sliding_shares_overlaps_and_rejects_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    for i in 1..=5 {
        store.commit(&[(REL, tup(i, 1), 1)]).unwrap();
    }
    let to = store.current();

    let ws = sliding(Edition::ZERO, to, 3, 1).unwrap();
    assert_eq!(
        ws.iter().map(|w| (w.from().0, w.to().0)).collect::<Vec<_>>(),
        vec![(0, 3), (1, 4), (2, 5), (3, 5), (4, 5)]
    );
    // Edition 3 is covered by windows 0, 1 and 2 — and by nothing else.
    assert_eq!(ws.iter().filter(|w| w.contains(Edition(3))).count(), 3);
    assert_eq!(covering_count(Edition::ZERO, Edition(3), 3, 1), 3);
    // Every edition in scope is covered at least once.
    for e in 1..=5 {
        assert!(ws.iter().any(|w| w.contains(Edition(e))), "edition {e} uncovered");
    }

    // Ill-formed families are rejected, not silently answered.
    assert!(sliding(Edition::ZERO, to, 2, 3).is_err(), "step > size must be rejected");
    assert!(sliding(Edition::ZERO, to, 0, 1).is_err(), "size 0 must be rejected");
    assert!(sliding(Edition::ZERO, to, 2, 0).is_err(), "step 0 must be rejected");
    assert!(window(to, Edition::ZERO).is_err(), "inverted bounds must be rejected");
}
