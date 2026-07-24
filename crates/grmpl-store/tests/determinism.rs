//! Determinism guarantees (TKT-72): `read_at` returns tuple-sorted rows and
//! `scan_updates` returns updates in commit order `(edition, counter)`, not the
//! store's physical scan order.

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn ent(a: u64, b: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a)), Value::Ent(Entity(b))])
}

#[test]
fn read_at_is_tuple_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    // Commit in a deliberately unsorted order within one edition.
    store
        .commit(&[
            (R, ent(9, 0), 1),
            (R, ent(1, 0), 1),
            (R, ent(5, 0), 1),
            (R, ent(3, 0), 1),
        ])
        .unwrap();

    let rows: Vec<Tuple> = store
        .read_at(R, Edition(1))
        .unwrap()
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let mut want = rows.clone();
    want.sort();
    assert_eq!(rows, want, "read_at must be tuple-sorted");
    assert_eq!(rows, vec![ent(1, 0), ent(3, 0), ent(5, 0), ent(9, 0)]);
}

#[test]
fn scan_updates_is_in_commit_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    // Two same-edition updates whose commit (counter) order is the *reverse* of
    // their tuple order — the sort must follow the counter, not the tuple.
    store.commit(&[(R, ent(9, 0), 1), (R, ent(1, 0), 1)]).unwrap();
    store.commit(&[(R, ent(5, 0), 1)]).unwrap();

    let updates = store.scan_updates(R, Edition::ZERO, Edition(2)).unwrap();
    let seen: Vec<(u64, Diff, Tuple)> = updates
        .into_iter()
        .map(|u| (u.time.edition, u.diff, u.tuple))
        .collect();
    assert_eq!(
        seen,
        vec![
            (1, 1, ent(9, 0)), // edition 1, counter 0
            (1, 1, ent(1, 0)), // edition 1, counter 1
            (2, 1, ent(5, 0)), // edition 2, counter 0
        ]
    );
}

// --- Randomized-churn property tests (the law oracles) -----------------------
//
// The two example tests above pin *one* interleaving each. The steward bar for
// the determinism laws (CLAUDE.md §Determinism) is that they hold under
// *arbitrary* write order, so these tests churn many random commits and check
// the invariants after every round, against an independent oracle:
//
//   * `read_at` is always tuple-sorted, and equals a from-scratch consolidation
//     (multiset sum) of every update up to `at` — so the sort is not just
//     ordered but *correct*.
//   * `scan_updates` is always in commit order `(edition, counter)`: editions
//     never regress, and within an edition the updates appear in exactly the
//     order they were submitted to `commit` (the counter) — regardless of the
//     store's physical fjall scan order.
//
// A tiny xorshift64* PRNG keeps the churn reproducible with no external `rand`
// dependency; every assertion prints its `seed` so a failure replays directly.

/// Deterministic, seedable PRNG (xorshift64*). Reproducible churn without
/// pulling a `rand`/`proptest` dependency into the store's test harness.
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
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

const RELS: [RelId; 3] = [RelId(1), RelId(2), RelId(3)];

#[test]
fn read_at_and_scan_updates_hold_under_random_churn() {
    for seed in 1..=24u64 {
        churn_round(seed);
    }
}

fn churn_round(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let mut rng = Rng::new(seed);

    // Independent oracle: per rel, the exact commit-order log of (edition,
    // tuple, diff) as *submitted*. Small key/diff spaces force real
    // cancellation (a tuple asserted then retracted must vanish from read_at).
    let mut log: HashMap<RelId, Vec<(u64, Tuple, Diff)>> = HashMap::new();

    let rounds = 20 + rng.below(30); // 20..=49 commits
    for _ in 0..rounds {
        // A random batch, in random submit order, across all three rels.
        let batch_len = 1 + rng.below(6);
        let mut batch: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for _ in 0..batch_len {
            let rel = RELS[rng.below(RELS.len() as u64) as usize];
            let t = ent(rng.below(6), rng.below(3));
            let diff: Diff = if rng.below(2) == 0 { 1 } else { -1 };
            batch.push((rel, t, diff));
        }

        let ed = store.commit(&batch).unwrap();
        // Record into the oracle in submit order — this *is* commit/counter order.
        for (rel, t, d) in &batch {
            log.entry(*rel).or_default().push((ed.0, t.clone(), *d));
        }

        for rel in RELS {
            let empty = Vec::new();
            let rel_log = log.get(&rel).unwrap_or(&empty);

            // --- Invariant 1: read_at tuple-sorted AND a correct consolidation.
            let rows = store.read_at(rel, ed).unwrap();
            let mut sorted = rows.clone();
            sorted.sort();
            assert_eq!(rows, sorted, "seed {seed}: read_at not tuple-sorted for {rel:?}");

            let mut acc: HashMap<Tuple, Diff> = HashMap::new();
            for (_e, t, d) in rel_log {
                *acc.entry(t.clone()).or_insert(0) += *d;
            }
            let mut want: Vec<(Tuple, Diff)> =
                acc.into_iter().filter(|(_, d)| *d != 0).collect();
            want.sort();
            assert_eq!(rows, want, "seed {seed}: read_at consolidation wrong for {rel:?}");

            // --- Invariant 2: scan_updates in commit order (edition, counter).
            let ups = store.scan_updates(rel, Edition::ZERO, ed).unwrap();
            let mut prev_edition = 0u64;
            for u in &ups {
                assert!(
                    u.time.edition >= prev_edition,
                    "seed {seed}: scan_updates edition regressed for {rel:?}"
                );
                prev_edition = u.time.edition;
            }
            // Full oracle: exact submit order (which encodes the intra-edition
            // counter) is reproduced regardless of physical scan order.
            let got: Vec<(u64, Tuple, Diff)> = ups
                .iter()
                .map(|u| (u.time.edition, u.tuple.clone(), u.diff))
                .collect();
            assert_eq!(&got, rel_log, "seed {seed}: scan_updates not in commit order for {rel:?}");
        }
    }
}
