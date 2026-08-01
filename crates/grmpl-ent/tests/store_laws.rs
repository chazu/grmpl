//! **The store contract, stated absolutely against the Ent.**
//!
//! These laws used to live in `grmpl-store/tests` and were checked against the
//! LSM; the Ent was checked *differentially* against it. Deleting the LSM would
//! therefore have deleted the laws too, leaving only "the two agree" — with
//! nothing to agree with. So they are restated here as absolute properties, each
//! against an independent model rather than against another store:
//!
//! * **Determinism** — `read_at` is tuple-sorted; `scan_updates` is raw,
//!   per-multiplicity, and in exact commit order.
//! * **Patch–edition** — one commit is exactly one edition regardless of size, a
//!   later edition is never visible early, and a multi-update commit is
//!   all-or-nothing across a reopen.
//! * **History** — consolidation preserves every as-of read at or above the
//!   watermark, the watermark is monotonic and clamped to `current`, and both it
//!   and the checkpoint survive a reopen.
//! * **Forks** — a fork is logically identical to its source at the cut, evolves
//!   independently, and replaying the same commands onto both reconverges.
//!
//! Identity here is **logical** — `read_at` / `scan_updates` — never node bytes,
//! as `DESIGN.md` and plan v5 §G-2b define it.

use std::collections::BTreeMap;

use grmpl_core::{Diff, Edition, EditionStore, RelId, Time, TraceStore, Tuple, Update, Value};
use grmpl_ent::EntStore;

const A: RelId = RelId(1);
const B: RelId = RelId(2);
const RELS: [RelId; 2] = [A, B];

fn t(a: i64, b: i64) -> Tuple {
    Tuple::from([Value::Int(a), Value::Int(b)])
}

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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// An independent full-history model: every update ever committed, in order.
#[derive(Default)]
struct Model {
    log: Vec<(u64, RelId, Tuple, Diff)>,
}

impl Model {
    fn apply(&mut self, edition: u64, updates: &[(RelId, Tuple, Diff)]) {
        for (rel, tup, d) in updates {
            self.log.push((edition, *rel, tup.clone(), *d));
        }
    }
    /// The net contents of `rel` at `at`, tuple-sorted with zero weights absent.
    fn read_at(&self, rel: RelId, at: u64) -> Vec<(Tuple, Diff)> {
        let mut net: BTreeMap<Tuple, Diff> = BTreeMap::new();
        for (e, r, tup, d) in &self.log {
            if *r == rel && *e <= at {
                *net.entry(tup.clone()).or_insert(0) += d;
            }
        }
        net.into_iter().filter(|(_, w)| *w != 0).collect()
    }
    /// The raw updates of `rel` in `(from, to]`, in commit order.
    fn scan(&self, rel: RelId, from: u64, to: u64) -> Vec<Update> {
        self.log
            .iter()
            .filter(|(e, r, _, _)| *r == rel && *e > from && *e <= to)
            .map(|(e, _, tup, d)| Update { tuple: tup.clone(), time: Time::input(*e), diff: *d })
            .collect()
    }
}

// --- Determinism ------------------------------------------------------------

#[test]
fn read_at_is_tuple_sorted_and_scan_updates_is_in_commit_order() {
    let store = EntStore::new();
    // Commit deliberately out of key order, and assert/retract/assert the same
    // tuple so the log must keep all three, not a net.
    store
        .commit(&[(A, t(5, 0), 1), (A, t(1, 0), 1), (A, t(3, 0), 1)])
        .unwrap();
    store.commit(&[(A, t(3, 0), -1)]).unwrap();
    store.commit(&[(A, t(3, 0), 1)]).unwrap();

    let rows = store.read_at(A, store.current()).unwrap();
    let keys: Vec<Tuple> = rows.iter().map(|(k, _)| k.clone()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "read_at must be tuple-sorted");

    let scan = store.scan_updates(A, Edition::ZERO, store.current()).unwrap();
    assert_eq!(scan.len(), 5, "scan_updates must be raw and per-multiplicity");
    let diffs: Vec<Diff> = scan.iter().filter(|u| u.tuple == t(3, 0)).map(|u| u.diff).collect();
    assert_eq!(diffs, vec![1, -1, 1], "assert;retract;assert is three entries in order");
}

#[test]
fn read_at_and_scan_updates_match_the_model_under_random_churn() {
    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        let store = EntStore::new();
        let mut model = Model::default();

        for round in 0..80 {
            let n = 1 + rng.below(4);
            let updates: Vec<(RelId, Tuple, Diff)> = (0..n)
                .map(|_| {
                    (
                        RELS[rng.below(2) as usize],
                        t(rng.below(6) as i64, rng.below(3) as i64),
                        if rng.below(2) == 0 { 1 } else { -1 },
                    )
                })
                .collect();
            let e = store.commit(&updates).unwrap();
            model.apply(e.0, &updates);

            let cur = store.current().0;
            let at = rng.below(cur + 1);
            for r in RELS {
                assert_eq!(
                    store.read_at(r, Edition(at)).unwrap(),
                    model.read_at(r, at),
                    "seed {seed} round {round}: read_at({r:?}, {at})"
                );
                let x = rng.below(cur + 1);
                let y = rng.below(cur + 1);
                let (from, to) = (x.min(y), x.max(y));
                assert_eq!(
                    store.scan_updates(r, Edition(from), Edition(to)).unwrap(),
                    model.scan(r, from, to),
                    "seed {seed} round {round}: scan_updates({r:?}, {from}, {to}]"
                );
            }
        }
    }
}

// --- Patch–edition ----------------------------------------------------------

#[test]
fn one_commit_is_exactly_one_edition_regardless_of_size() {
    let store = EntStore::new();
    let before = store.current().0;
    let big: Vec<(RelId, Tuple, Diff)> = (0..500i64).map(|k| (A, t(k, 0), 1)).collect();
    let e = store.commit(&big).unwrap();
    assert_eq!(e.0, before + 1, "a 500-update commit is still one edition");
    assert_eq!(store.current(), e);
    // Every one of them is visible at that edition, and none before it.
    assert_eq!(store.read_at(A, e).unwrap().len(), 500);
    assert!(store.read_at(A, Edition(before)).unwrap().is_empty());
}

#[test]
fn a_multi_update_commit_is_all_or_nothing_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let e = {
        let store = EntStore::open(dir.path()).unwrap();
        store
            .commit(&[(A, t(1, 1), 1), (A, t(2, 2), 1), (B, t(3, 3), 1)])
            .unwrap()
    };
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), e, "the edition clock did not survive the reopen");
    assert_eq!(store.read_at(A, e).unwrap().len(), 2, "part of the commit was lost");
    assert_eq!(store.read_at(B, e).unwrap().len(), 1);
}

// --- History ----------------------------------------------------------------

#[test]
fn consolidation_preserves_every_as_of_read_at_or_above_the_watermark() {
    for seed in 1..=16u64 {
        let mut rng = Rng::new(seed ^ 0xC0FFEE);
        let store = EntStore::new();
        let mut model = Model::default();

        for _ in 0..40 {
            let updates: Vec<(RelId, Tuple, Diff)> = (0..1 + rng.below(3))
                .map(|_| (RELS[rng.below(2) as usize], t(rng.below(5) as i64, 0), 1))
                .collect();
            let e = store.commit(&updates).unwrap();
            model.apply(e.0, &updates);

            if rng.below(4) == 0 {
                let cur = store.current().0;
                let wm_before = store.watermark().0;
                let target = rng.below(cur + 3);
                let wm = store.consolidate(Edition(target)).unwrap();
                assert!(wm.0 >= wm_before, "seed {seed}: watermark went backwards");
                assert!(wm.0 <= cur, "seed {seed}: watermark passed current");

                // Every read at or above the watermark is untouched by the cut.
                for at in wm.0..=cur {
                    for r in RELS {
                        assert_eq!(
                            store.read_at(r, Edition(at)).unwrap(),
                            model.read_at(r, at),
                            "seed {seed}: read_at({r:?}, {at}) after consolidating to {}",
                            wm.0
                        );
                    }
                }
                // …and below it the door is shut, rather than answering wrongly.
                if wm.0 > 0 {
                    assert!(store.read_at(A, Edition(wm.0 - 1)).is_err(), "the door must close");
                }
            }
        }
    }
}

#[test]
fn the_watermark_survives_a_reopen_and_is_a_noop_on_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let (wm, cur) = {
        let store = EntStore::open(dir.path()).unwrap();
        assert_eq!(store.consolidate(Edition(5)).unwrap(), Edition(0), "empty consolidate");
        for k in 0..10i64 {
            store.commit(&[(A, t(k, 0), 1)]).unwrap();
        }
        let wm = store.consolidate(Edition(6)).unwrap();
        assert_eq!(wm, Edition(6));
        // Clamped to current: asking beyond it cannot invent history.
        assert_eq!(store.consolidate(Edition(9_999)).unwrap(), store.current());
        (store.watermark(), store.current())
    };
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.watermark(), wm, "the watermark did not survive the reopen");
    assert_eq!(store.current(), cur);
    assert_eq!(store.read_at(A, cur).unwrap().len(), 10, "the checkpoint did not survive");
}

// --- Forks ------------------------------------------------------------------

/// The logical projection — how identity is defined (plan v5 §G-2b).
fn projection(store: &EntStore) -> Vec<Vec<Update>> {
    let (from, to) = (store.watermark(), store.current());
    RELS.iter().map(|r| store.scan_updates(*r, from, to).unwrap()).collect()
}

#[test]
fn a_fork_is_identical_at_the_cut_then_evolves_independently() {
    for seed in 1..=8u64 {
        let mut rng = Rng::new(seed ^ 0xF0);
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        for _ in 0..20 {
            let updates: Vec<(RelId, Tuple, Diff)> = (0..1 + rng.below(3))
                .map(|_| (RELS[rng.below(2) as usize], t(rng.below(8) as i64, 0), 1))
                .collect();
            store.commit(&updates).unwrap();
        }

        let fork = store.fork_at(store.current()).unwrap();
        assert_eq!(projection(&store), projection(&fork), "seed {seed}: fork differs at the cut");

        // Replaying the *same* commands onto both reconverges them.
        let replay: Vec<(RelId, Tuple, Diff)> =
            (0..5).map(|k| (A, t(1000 + k, 0), 1)).collect();
        for target in [&store, &fork] {
            for u in &replay {
                target.commit(std::slice::from_ref(u)).unwrap();
            }
        }
        assert_eq!(projection(&store), projection(&fork), "seed {seed}: replay diverged");
        assert_eq!(store.current(), fork.current());

        // Diverging one leaves the other alone.
        fork.commit(&[(B, t(7777, 0), 1)]).unwrap();
        assert_ne!(projection(&store), projection(&fork));
        assert!(store
            .read_at(B, store.current())
            .unwrap()
            .iter()
            .all(|(k, _)| *k != t(7777, 0)));
    }
}
