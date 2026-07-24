//! History: as-of reads, retention, and GC (TKT-78, P6).
//!
//! The store is an append-only edition log, so as-of reads already work; this
//! phase adds a **consolidation watermark**. `consolidate` folds each relation's
//! history at editions ≤ a chosen horizon into a per-relation checkpoint (the
//! reserved edition-0 key range), discards the folded raw updates, and persists
//! the watermark — turning the O(history) as-of read into O(checkpoint + tail).
//!
//! The steward bar for a data law is a randomized-churn *oracle*, not examples:
//! [`consolidation_preserves_as_of_reads_under_random_churn`] churns random
//! commits interleaved with random consolidations and, after every step, checks
//! the load-bearing invariant against an independent model —
//!
//!   * every as-of `read_at(at)` **at or above** the watermark returns exactly
//!     what a full-history store would (checkpoint + tail == full replay),
//!   * every `scan_updates(from, ..)` from at/above the watermark matches, and
//!   * both doors **error** at any edition below the watermark (the history is
//!     genuinely gone, answered loudly rather than wrongly),
//!   * the watermark is monotonic and never exceeds `current`.
//!
//! The example tests below pin the specific corners (reopen durability, no-op /
//! clamped consolidation, the door messages).

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_store::FjallStore;

const RELS: [RelId; 3] = [RelId(1), RelId(2), RelId(3)];

fn ent(a: u64, b: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a)), Value::Ent(Entity(b))])
}

/// Deterministic, seedable PRNG (xorshift64*) — reproducible churn with no
/// external `rand` dependency, matching the `determinism.rs` idiom.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
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

/// Independent model of the whole trace: per rel, the commit-order log of
/// `(edition, tuple, diff)` exactly as submitted.
#[derive(Default)]
struct Model {
    log: HashMap<RelId, Vec<(u64, Tuple, Diff)>>,
}

impl Model {
    fn record(&mut self, rel: RelId, edition: u64, t: &Tuple, d: Diff) {
        self.log.entry(rel).or_default().push((edition, t.clone(), d));
    }

    /// Full-history consolidation of `rel` as-of `at` (what an un-GC'd store
    /// would return): sum every submitted diff at editions ≤ `at`, drop zeros,
    /// tuple-sorted.
    fn read_at(&self, rel: RelId, at: u64) -> Vec<(Tuple, Diff)> {
        let mut acc: HashMap<Tuple, Diff> = HashMap::new();
        if let Some(rel_log) = self.log.get(&rel) {
            for (e, t, d) in rel_log {
                if *e <= at {
                    *acc.entry(t.clone()).or_insert(0) += *d;
                }
            }
        }
        let mut out: Vec<(Tuple, Diff)> = acc.into_iter().filter(|(_, d)| *d != 0).collect();
        out.sort();
        out
    }

    /// The updates of `rel` in `(from, to]`, in commit order.
    fn scan(&self, rel: RelId, from: u64, to: u64) -> Vec<(u64, Tuple, Diff)> {
        self.log
            .get(&rel)
            .map(|l| l.iter().filter(|(e, _, _)| *e > from && *e <= to).cloned().collect())
            .unwrap_or_default()
    }
}

#[test]
fn consolidation_preserves_as_of_reads_under_random_churn() {
    for seed in 1..=32u64 {
        churn_round(seed);
    }
}

fn churn_round(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let mut rng = Rng::new(seed);
    let mut model = Model::default();
    let mut watermark: u64 = 0;

    let rounds = 24 + rng.below(24); // 24..=47 steps
    for _ in 0..rounds {
        // Small key/diff spaces force real cancellation (assert-then-retract must
        // vanish from a checkpoint, not linger as a stale +1/-1 pair).
        let batch_len = 1 + rng.below(6);
        let mut batch: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for _ in 0..batch_len {
            let rel = RELS[rng.below(RELS.len() as u64) as usize];
            let t = ent(rng.below(5), rng.below(3));
            let diff: Diff = if rng.below(2) == 0 { 1 } else { -1 };
            batch.push((rel, t, diff));
        }
        let ed = store.commit(&batch).unwrap();
        for (rel, t, d) in &batch {
            model.record(*rel, ed.0, t, *d);
        }
        let current = ed.0;

        // Roughly every third step, consolidate to a random horizon (which may be
        // in the future, at, or below the watermark — all must behave).
        if rng.below(3) == 0 {
            let horizon = rng.below(current + 3); // 0..=current+2, exercising the clamp
            let got = store.consolidate(Edition(horizon)).unwrap();
            let expected = horizon.min(current).max(watermark); // clamp + monotonic
            assert_eq!(
                got.0, expected,
                "seed {seed}: consolidate({horizon}) at current {current}, wm {watermark}"
            );
            watermark = expected;
        }

        assert_eq!(store.watermark().0, watermark, "seed {seed}: watermark drifted");
        assert!(watermark <= current, "seed {seed}: watermark {watermark} > current {current}");

        for rel in RELS {
            // Door: everything at/above the watermark reads exactly as full history.
            for at in watermark..=current {
                let rows = store.read_at(rel, Edition(at)).unwrap();
                assert_eq!(
                    rows,
                    model.read_at(rel, at),
                    "seed {seed}: read_at({at}) != full-history for {rel:?} (wm {watermark})"
                );
                // read_at is always tuple-sorted (checkpoint + tail merge).
                let mut sorted = rows.clone();
                sorted.sort();
                assert_eq!(rows, sorted, "seed {seed}: read_at({at}) unsorted for {rel:?}");
            }

            // Door: scan_updates from at/above the watermark matches the model.
            for from in watermark..=current {
                let ups = store.scan_updates(rel, Edition(from), Edition(current)).unwrap();
                let got: Vec<(u64, Tuple, Diff)> =
                    ups.iter().map(|u| (u.time.edition, u.tuple.clone(), u.diff)).collect();
                assert_eq!(
                    got,
                    model.scan(rel, from, current),
                    "seed {seed}: scan_updates({from},{current}) != model for {rel:?}"
                );
            }

            // Door: below the watermark, both reads error (history is gone).
            if watermark > 0 {
                assert!(
                    store.read_at(rel, Edition(watermark - 1)).is_err(),
                    "seed {seed}: read_at below wm {watermark} must error for {rel:?}"
                );
                assert!(
                    store.scan_updates(rel, Edition(watermark - 1), Edition(current)).is_err(),
                    "seed {seed}: scan from below wm {watermark} must error for {rel:?}"
                );
            }
        }
    }
}

// --- Corner cases -----------------------------------------------------------

#[test]
fn watermark_and_checkpoint_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let r = RelId(7);
    {
        let store = FjallStore::open(dir.path()).unwrap();
        store.commit(&[(r, ent(1, 0), 1), (r, ent(2, 0), 1)]).unwrap();
        store.commit(&[(r, ent(1, 0), -1), (r, ent(3, 0), 1)]).unwrap(); // net: {2,3}
        store.commit(&[(r, ent(4, 0), 1)]).unwrap();
        let wm = store.consolidate(Edition(2)).unwrap();
        assert_eq!(wm.0, 2);
    }
    // Reopen: watermark and the checkpoint (the consolidated state as-of 2) must
    // both be durable, and later reads still correct.
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.watermark().0, 2, "watermark not persisted");
    // read at the watermark == checkpoint alone.
    assert_eq!(store.read_at(r, Edition(2)).unwrap(), vec![(ent(2, 0), 1), (ent(3, 0), 1)]);
    // read at current == checkpoint + tail.
    assert_eq!(
        store.read_at(r, Edition(3)).unwrap(),
        vec![(ent(2, 0), 1), (ent(3, 0), 1), (ent(4, 0), 1)]
    );
    // Below the watermark still errors after reopen.
    assert!(store.read_at(r, Edition(1)).is_err());
}

#[test]
fn consolidate_is_monotonic_and_clamped_to_current() {
    let dir = tempfile::tempdir().unwrap();
    let r = RelId(1);
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(r, ent(1, 0), 1)]).unwrap();
    store.commit(&[(r, ent(2, 0), 1)]).unwrap(); // current == 2

    // Clamp to current: asking for the future consolidates only what exists.
    assert_eq!(store.consolidate(Edition(999)).unwrap().0, 2);
    // Monotonic: a lower (or equal) horizon is a no-op, not a rollback.
    assert_eq!(store.consolidate(Edition(1)).unwrap().0, 2);
    assert_eq!(store.consolidate(Edition(0)).unwrap().0, 2);
    assert_eq!(store.watermark().0, 2);
    // The whole state still reads correctly from the checkpoint.
    assert_eq!(store.read_at(r, Edition(2)).unwrap(), vec![(ent(1, 0), 1), (ent(2, 0), 1)]);
}

#[test]
fn consolidate_on_empty_store_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.consolidate(Edition(5)).unwrap().0, 0); // clamped to current == 0
    assert_eq!(store.watermark().0, 0);
    assert!(store.read_at(RelId(1), Edition(0)).unwrap().is_empty());
}
