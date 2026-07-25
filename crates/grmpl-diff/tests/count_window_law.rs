//! **Count-window law oracle** (TKT-117; P9c design §5, where count windows are
//! the other deferred shape).
//!
//! A count window is "the last N updates" — the one shape in §5 that is *not* an
//! edition interval, because its boundary generally falls inside an edition. It
//! is therefore a maintained cursor over commit order rather than a `Window`, and
//! the thing that can go wrong with it is eviction: retaining something stale, or
//! dropping something still live.
//!
//! The law is stated against an **independent model** — the oracle keeps its own
//! record of every update it committed, in the order it committed them, and
//! recomputes "the last `min(N, seen)`" by slicing that record. Nothing in the
//! assertions consults `CountWindow` to decide what `CountWindow` should hold.
//!
//! 1. **Contents.** After every arrival the window holds *exactly* the last
//!    `min(N, seen)` updates, in arrival order. That single equality rules out
//!    both failure modes at once: a stale retention makes the window too long or
//!    misordered, and an over-eager eviction makes it too short.
//! 2. **Eviction is one-for-one.** `contents_after = contents_before − evicted +
//!    admitted`, evictions are the *oldest* elements, and once the window is full
//!    `k` arrivals evict exactly `k`. The emitted `CountDelta` is complete: it
//!    accounts for every element that moved.
//! 3. **Slide by one.** Committing one update at a time — the "sliding by one as
//!    each new event arrives" reading — evicts exactly one once full and exactly
//!    zero before, and the window's contents shift by precisely one position.
//! 4. **Seeding and ordering.** Opening at a random historical edition reproduces
//!    the last N as of *that* edition; arrival order is commit order
//!    `(edition, counter)`, including several updates inside one edition; the
//!    window is per base relation.
//!
//! Written in the shape the repo convention requires (TKT-112): **seeded
//! xorshift randomized** churn, invariant re-checked every round, seed printed on
//! every assertion, no `rand`/`proptest` dependency, examples kept only as
//! witnesses.

use grmpl_core::{Edition, EditionStore, RelId, TraceStore, Tuple, Update, Value};
use grmpl_diff::{last_n, CountWindow};
use grmpl_store::FjallStore;

const REL: RelId = RelId(1);
const OTHER: RelId = RelId(2);
const SEEDS: std::ops::Range<u64> = 1..25; // 24 seeds
const ROUNDS: usize = 14;

// ---------------------------------------------------------------------------
// PRNG — SplitMix64 finalisation with the nonzero guard applied *after* mixing,
// so no seed bit is spent on the guard (cf. TKT-141, which sweeps the older
// `Rng(seed ^ K | 1)` spelling out of the existing oracles; that form collapses
// seeds 2n and 2n+1 to one stream and halves declared coverage).
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z })
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

fn tup(k: u64) -> Tuple {
    Tuple::from([Value::Int(k as i64)])
}

/// The oracle's independent record: every update committed to `REL`, in the
/// order it was committed. `CountWindow` is never consulted to build it.
type Arrivals = Vec<Update>;

/// Commit one edition of 1–4 signed updates to `REL`, plus (sometimes) unrelated
/// traffic on a peer relation, and record what `REL` received. Several updates
/// per edition are deliberate: it is what makes "the last N" fall *inside* an
/// edition, which is the whole reason a count window is not a `Window`.
fn churn(store: &FjallStore, rng: &mut Rng, arrivals: &mut Arrivals) {
    let n = 1 + rng.below(4);
    let batch: Vec<(RelId, Tuple, i64)> = (0..n)
        .map(|_| (REL, tup(rng.below(20)), if rng.below(4) == 0 { -1 } else { 1 }))
        .collect();
    let at = store.commit(&batch).unwrap();
    // `scan_updates` is the arrival order by contract; read back exactly this
    // edition's slice so the model records the same order the store will report.
    arrivals.extend(store.scan_updates(REL, Edition(at.0 - 1), at).unwrap());
    if rng.below(3) == 0 {
        store.commit(&[(OTHER, tup(rng.below(5)), 1)]).unwrap();
    }
}

/// The model's answer: the last `min(n, seen)` arrivals, in arrival order.
fn expected(arrivals: &Arrivals, n: usize) -> &[Update] {
    &arrivals[arrivals.len().saturating_sub(n)..]
}

// ---------------------------------------------------------------------------
// Law 1 + 2 — contents, and eviction accounting
// ---------------------------------------------------------------------------

/// The window holds exactly the last `min(N, seen)` updates in arrival order
/// after **every** advance, and the emitted `CountDelta` accounts completely for
/// how it got there. Anti-vacuity counters assert the window actually filled and
/// actually evicted — a run that never reached capacity would check nothing.
#[test]
fn count_window_holds_exactly_the_last_n_in_arrival_order() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed);
        let mut arrivals = Arrivals::new();

        // Some history before opening, so the seed path is exercised too.
        for _ in 0..rng.below(4) {
            churn(&store, &mut rng, &mut arrivals);
        }
        let n = 1 + rng.below(9) as usize;
        let mut w = CountWindow::open(&store, REL, n, store.current()).unwrap();
        assert_eq!(
            w.events(),
            expected(&arrivals, n),
            "seed {seed}: opening did not seed the last {n} updates as of the opening edition"
        );
        assert_eq!(w.seen(), arrivals.len() as u64, "seed {seed}: seed count wrong");

        let mut ever_full = 0usize;
        let mut ever_evicted = 0usize;

        for round in 0..ROUNDS {
            let before: Vec<Update> = w.events().to_vec();
            let before_len = before.len();
            for _ in 0..(1 + rng.below(3)) {
                churn(&store, &mut rng, &mut arrivals);
            }
            let delta = w.advance_to(&store, store.current()).unwrap();

            // LAW 1 — contents, against the model.
            assert_eq!(
                w.events(),
                expected(&arrivals, n),
                "seed {seed} round {round}: the window is not the last {n} updates in arrival \
                 order (holds {}, {} seen)",
                w.len(),
                arrivals.len()
            );
            assert_eq!(
                w.len(),
                n.min(arrivals.len()),
                "seed {seed} round {round}: len() != min(capacity, seen)"
            );
            assert_eq!(w.seen(), arrivals.len() as u64, "seed {seed} round {round}: seen() drifted");
            assert_eq!(w.capacity(), n);
            assert_eq!(w.at(), store.current());

            // LAW 2 — the delta accounts for every element that moved. Stated
            // over `before ++ admitted`, not over `before` alone: when one
            // advance brings more than `n` updates, the surplus taken off the
            // oldest end reaches *into the admissions*, and an update that
            // arrived and was immediately displaced is genuinely both admitted
            // and evicted.
            let mut combined = before.clone();
            combined.extend(delta.admitted.iter().cloned());
            assert!(
                combined.starts_with(&delta.evicted),
                "seed {seed} round {round}: evictions were not taken from the *oldest* end of \
                 (contents ++ arrivals)"
            );
            assert_eq!(
                &combined[delta.evicted.len()..],
                w.events(),
                "seed {seed} round {round}: (contents_before ++ admitted) − evicted != \
                 contents_after — the CountDelta does not account for the advance"
            );
            assert_eq!(
                delta.evicted.len(),
                (before_len + delta.admitted.len()).saturating_sub(n),
                "seed {seed} round {round}: evicted {} to admit {} into a window of {n} holding \
                 {before_len} — eviction must be exactly the surplus, no more and no less",
                delta.evicted.len(),
                delta.admitted.len()
            );

            if w.len() == n {
                ever_full += 1;
            }
            ever_evicted += delta.evicted.len();

            // A re-advance to the same edition is a no-op, not a re-admission.
            let again = w.advance_to(&store, store.current()).unwrap();
            assert!(
                again.admitted.is_empty() && again.evicted.is_empty(),
                "seed {seed} round {round}: re-advancing to the same edition moved elements"
            );
            assert_eq!(w.events(), expected(&arrivals, n));
        }

        assert!(
            ever_full > 0,
            "seed {seed}: the window never reached capacity, so eviction went untested"
        );
        assert!(
            ever_evicted > 0,
            "seed {seed}: nothing was ever evicted, so the law was checked on a growing window \
             only"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 3 — sliding by one
// ---------------------------------------------------------------------------

/// The "sliding by one as each new event arrives" reading, stated exactly:
/// feeding one update at a time, the window grows by one until it is full and
/// thereafter shifts by exactly one — one admitted, one evicted, and the retained
/// part is the previous contents minus its head.
#[test]
fn count_window_slides_by_one_per_arrival() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x511D_E111);
        let mut arrivals = Arrivals::new();

        let n = 2 + rng.below(6) as usize;
        let mut w = CountWindow::open(&store, REL, n, Edition::ZERO).unwrap();
        assert!(w.is_empty(), "seed {seed}: a window opened at ⊥ starts empty");
        assert_eq!(w.span(), None, "seed {seed}: an empty window spans nothing");

        for i in 0..(2 * n + 6) {
            let before: Vec<Update> = w.events().to_vec();
            // Exactly one update, so "arrival" and "advance" coincide.
            let at = store.commit(&[(REL, tup(i as u64), 1)]).unwrap();
            arrivals.extend(store.scan_updates(REL, Edition(at.0 - 1), at).unwrap());

            let delta = w.advance_to(&store, at).unwrap();
            assert_eq!(delta.admitted.len(), 1, "seed {seed} step {i}: one commit, one admission");

            if before.len() < n {
                assert!(
                    delta.evicted.is_empty(),
                    "seed {seed} step {i}: the window was not full ({} of {n}) yet something was \
                     evicted",
                    before.len()
                );
                assert_eq!(w.len(), before.len() + 1, "seed {seed} step {i}: it should have grown");
            } else {
                assert_eq!(
                    delta.evicted.len(),
                    1,
                    "seed {seed} step {i}: a full window must evict exactly one per arrival"
                );
                assert_eq!(
                    delta.evicted[0], before[0],
                    "seed {seed} step {i}: the evicted update is not the oldest one"
                );
                assert_eq!(w.len(), n, "seed {seed} step {i}: a full window must stay at capacity");
                assert_eq!(
                    &w.events()[..n - 1],
                    &before[1..],
                    "seed {seed} step {i}: the retained part is not the previous contents shifted \
                     by one — something stale was kept or something live was dropped"
                );
            }
            assert_eq!(
                w.events(),
                expected(&arrivals, n),
                "seed {seed} step {i}: contents diverged from the model"
            );

            // The reported span brackets the contents, and is honest about being
            // a report rather than a recoverable window.
            let span = w.span().expect("non-empty");
            for u in w.events() {
                assert!(
                    span.contains(Edition(u.time.edition)),
                    "seed {seed} step {i}: span {span:?} excludes an update it holds"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Law 4 — seeding, intra-edition order, per-relation scope
// ---------------------------------------------------------------------------

/// Opening at an arbitrary historical edition reproduces the last N *as of that
/// edition* — the window is a function of the trace prefix, not of when it was
/// constructed. Advancing from there converges on the same contents as a window
/// that watched the whole trace, so seeding and maintenance agree.
#[test]
fn opening_at_any_edition_agrees_with_having_watched_all_along() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x09E0_5EED);
        let mut arrivals = Arrivals::new();
        let mut stops = vec![Edition::ZERO];

        for _ in 0..10 {
            churn(&store, &mut rng, &mut arrivals);
            stops.push(store.current());
        }
        let n = 1 + rng.below(7) as usize;

        // Watched all along.
        let mut watcher = CountWindow::open(&store, REL, n, Edition::ZERO).unwrap();
        for e in &stops {
            watcher.advance_to(&store, *e).unwrap();
        }

        // Opened cold at each historical edition: the contents at that edition
        // must match the model's prefix, and advancing to the end must converge.
        for (i, e) in stops.iter().enumerate() {
            let mut cold = CountWindow::open(&store, REL, n, *e).unwrap();
            let prefix: Arrivals = arrivals
                .iter()
                .filter(|u| Edition(u.time.edition) <= *e)
                .cloned()
                .collect();
            assert_eq!(
                cold.events(),
                expected(&prefix, n),
                "seed {seed}: opening at stop {i} (edition {}) did not reproduce the last {n} \
                 updates as of that edition",
                e.0
            );
            cold.advance_to(&store, store.current()).unwrap();
            assert_eq!(
                cold.events(),
                watcher.events(),
                "seed {seed}: a window opened at stop {i} and advanced does not agree with one \
                 that watched all along"
            );
        }

        assert_eq!(
            watcher.events(),
            expected(&arrivals, n),
            "seed {seed}: the watcher diverged from the model"
        );

        // Per base relation: the peer's traffic never enters this window.
        for u in watcher.events() {
            assert!(
                arrivals.contains(u),
                "seed {seed}: the window admitted an update that was never committed to REL"
            );
        }
        assert_eq!(watcher.rel(), REL);
    }
}

// ---------------------------------------------------------------------------
// The pure kernel, and ill-formed input
// ---------------------------------------------------------------------------

/// `last_n` is the arithmetic the eviction rule is stated over, so it carries the
/// boundary cases on its own: fewer items than `n`, exactly `n`, more than `n`,
/// and `n` at zero (empty, not a panic).
#[test]
fn last_n_is_the_tail_of_at_most_n() {
    for seed in 1..33u64 {
        let mut rng = Rng::new(seed ^ 0x1A57_0000);
        for round in 0..12 {
            let len = rng.below(12) as usize;
            let items: Vec<u64> = (0..len as u64).collect();
            let n = rng.below(14) as usize;
            let got = last_n(&items, n);
            assert_eq!(
                got.len(),
                n.min(len),
                "seed {seed} round {round}: last_n({len} items, {n}) has {} items",
                got.len()
            );
            assert!(
                items.ends_with(got),
                "seed {seed} round {round}: last_n returned something other than a suffix"
            );
        }
    }
    assert!(last_n::<u8>(&[], 4).is_empty());
    assert!(last_n(&[1, 2, 3], 0).is_empty());
    assert_eq!(last_n(&[1, 2, 3], 9), &[1, 2, 3]);
}

/// A readable statement of the whole shape, with several updates inside one
/// edition — the case that makes a count window not an edition window at all.
#[test]
fn witness_the_boundary_falls_inside_an_edition() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // One edition carrying four updates, into a window of three.
    let at = store
        .commit(&[(REL, tup(1), 1), (REL, tup(2), 1), (REL, tup(3), 1), (REL, tup(4), 1)])
        .unwrap();
    let mut w = CountWindow::open(&store, REL, 3, at).unwrap();
    assert_eq!(w.len(), 3, "the last three of four updates committed in one edition");
    assert_eq!(
        w.events().iter().map(|u| u.tuple.clone()).collect::<Vec<_>>(),
        vec![tup(2), tup(3), tup(4)],
        "commit order within the edition decides which three survive"
    );
    // There is no `from` making `(from, at]` hold exactly these three — the whole
    // edition is one indivisible interval — which is why this is not a `Window`.
    let span = w.span().unwrap();
    assert_eq!(span.to(), at);
    assert_eq!(
        span.events(&store, REL).unwrap().len(),
        4,
        "the reported span brackets the contents but re-materializes all four: a count window's \
         boundary is not recoverable as an edition interval"
    );

    let next = store.commit(&[(REL, tup(5), 1)]).unwrap();
    let d = w.advance_to(&store, next).unwrap();
    assert_eq!(d.admitted.len(), 1);
    assert_eq!(d.evicted.iter().map(|u| u.tuple.clone()).collect::<Vec<_>>(), vec![tup(2)]);
    assert_eq!(
        w.events().iter().map(|u| u.tuple.clone()).collect::<Vec<_>>(),
        vec![tup(3), tup(4), tup(5)]
    );
    assert_eq!(w.seen(), 5);
}

/// Retractions are ordinary arrivals: a count window is **event mode**, so a
/// `−1` occupies a slot exactly as a `+1` does rather than cancelling one.
#[test]
fn witness_a_retraction_occupies_a_slot_rather_than_cancelling_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(REL, tup(1), 1)]).unwrap();
    let at = store.commit(&[(REL, tup(1), -1)]).unwrap();

    let w = CountWindow::open(&store, REL, 4, at).unwrap();
    assert_eq!(w.len(), 2, "the assert and its retraction are two events, not zero");
    assert_eq!(w.events()[0].diff, 1);
    assert_eq!(w.events()[1].diff, -1);
}

/// Ill-formed use is an error, not a silently degenerate answer.
#[test]
fn witness_ill_formed_count_window_use_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(REL, tup(1), 1)]).unwrap();
    let early = store.current();
    store.commit(&[(REL, tup(2), 1)]).unwrap();

    assert!(
        matches!(
            CountWindow::open(&store, REL, 0, early),
            Err(grmpl_core::Error::Query(_))
        ),
        "a count window of zero holds nothing and can never be advanced meaningfully"
    );

    let mut w = CountWindow::open(&store, REL, 2, early).unwrap();
    w.advance_to(&store, store.current()).unwrap();
    assert!(
        matches!(w.advance_to(&store, early), Err(grmpl_core::Error::Query(_))),
        "a count window advances with the trace: retreating is an ill-formed plan"
    );
}
