//! **Conformance oracle: `EntStore` ≡ `FjallStore` on the store contract**, both
//! live and **across a reopen** (durability).
//!
//! The ent-native store and the LSM store are driven with the *same* randomized
//! sequence of commit / commit_if / consolidate, and must agree on every
//! observable: the edition clock, watermark, `read_at` (net, sorted) at current
//! and past editions, `scan_updates` (raw, commit-ordered, per-multiplicity),
//! `commit_if` outcomes, and the below-watermark doors — then, after both are
//! closed and reopened from disk, must still agree (the ent rebuilds its state
//! from the persisted enfilades).

use grmpl_core::{Diff, Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;
use grmpl_store::FjallStore;

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

const RELS: [RelId; 3] = [RelId(1), RelId(2), RelId(3)];

fn tup(n: u64) -> Tuple {
    Tuple::from([Value::Int(n as i64)])
}

fn snapshot(s: &dyn TraceStore) -> Vec<Vec<(Tuple, Diff)>> {
    let cur = s.current();
    RELS.iter().map(|r| s.read_at(*r, cur).unwrap()).collect()
}

/// Generate one random op and apply it to *both* stores, asserting they agree on
/// the op's own result (edition / outcome / watermark).
fn drive_one(rng: &mut Rng, a: &dyn TraceStore, b: &dyn TraceStore) {
    match rng.below(10) {
        0..=5 => {
            let n = 1 + rng.below(3);
            let updates: Vec<(RelId, Tuple, Diff)> = (0..n)
                .map(|_| {
                    (
                        RELS[rng.below(3) as usize],
                        tup(rng.below(6)),
                        if rng.below(2) == 0 { 1 } else { -1 },
                    )
                })
                .collect();
            assert_eq!(a.commit(&updates).unwrap(), b.commit(&updates).unwrap());
        }
        6..=8 => {
            let np = rng.below(3);
            let pre: Vec<(RelId, Tuple)> = (0..np)
                .map(|_| (RELS[rng.below(3) as usize], tup(rng.below(6))))
                .collect();
            let upd = vec![(
                RELS[rng.below(3) as usize],
                tup(rng.below(6)),
                if rng.below(2) == 0 { 1 } else { -1 },
            )];
            let oa = a.commit_if(&pre, &upd).unwrap();
            let ob = b.commit_if(&pre, &upd).unwrap();
            assert_eq!(oa.is_some(), ob.is_some(), "commit_if outcome (pre={pre:?})");
        }
        _ => {
            let cur = a.current().0;
            let up = Edition(rng.below(cur + 1));
            assert_eq!(a.consolidate(up).unwrap(), b.consolidate(up).unwrap());
        }
    }
}

/// The full observable agreement check between two stores at their current state.
fn assert_agree(seed: u64, round: usize, rng: &mut Rng, a: &dyn TraceStore, b: &dyn TraceStore) {
    assert_eq!(a.current(), b.current(), "seed {seed} round {round}: current");
    assert_eq!(a.watermark(), b.watermark(), "seed {seed} round {round}: watermark");
    assert_eq!(snapshot(a), snapshot(b), "seed {seed} round {round}: read_at(current)");

    let wm = a.watermark().0;
    let cur = a.current().0;
    let x = wm + rng.below(cur - wm + 1);
    let y = wm + rng.below(cur - wm + 1);
    let (from, to) = (Edition(x.min(y)), Edition(x.max(y)));
    for r in RELS {
        assert_eq!(
            a.scan_updates(r, from, to).unwrap(),
            b.scan_updates(r, from, to).unwrap(),
            "seed {seed} round {round}: scan_updates {r:?} ({},{}]",
            from.0,
            to.0
        );
    }
    let at = Edition(wm + rng.below(cur - wm + 1));
    for r in RELS {
        assert_eq!(
            a.read_at(r, at).unwrap(),
            b.read_at(r, at).unwrap(),
            "seed {seed} round {round}: read_at {r:?} as-of {}",
            at.0
        );
    }
    if wm > 0 {
        let below = Edition(wm - 1);
        assert_eq!(
            a.read_at(RELS[0], below).is_err(),
            b.read_at(RELS[0], below).is_err(),
            "seed {seed} round {round}: read_at door"
        );
    }
}

#[test]
fn ent_store_matches_fjall_under_random_churn() {
    for seed in 1..=16u64 {
        let mut rng = Rng::new(seed);
        let ent = EntStore::new();
        let dir = tempfile::tempdir().unwrap();
        let fj = FjallStore::open(dir.path()).unwrap();
        for round in 0..150 {
            drive_one(&mut rng, &ent, &fj);
            assert_agree(seed, round, &mut rng, &ent, &fj);
        }
    }
}

#[test]
fn gc_collects_orphans_and_preserves_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let fdir = tempfile::tempdir().unwrap();
    let mut rng = Rng::new(777);
    let want = {
        let ent = EntStore::open(dir.path()).unwrap();
        let fj = FjallStore::open(fdir.path()).unwrap();
        // Churn (each commit path-copies its Edition enfilade → orphaned nodes).
        for _ in 0..80 {
            drive_one(&mut rng, &ent, &fj);
        }
        // Consolidate to current (truncates the logs → more orphans), then GC.
        let cur = ent.current();
        ent.consolidate(cur).unwrap();
        fj.consolidate(cur).unwrap();
        let collected = ent.gc().unwrap();
        assert!(collected > 0, "GC collected nothing after churn + consolidate");
        // The ent still matches the LSM after GC (no live node was collected).
        assert_agree(0, 0, &mut rng, &ent, &fj);
        snapshot(&ent)
    };
    // Reopen after GC: the live state is intact (reachable nodes were preserved).
    let ent = EntStore::open(dir.path()).unwrap();
    assert_eq!(snapshot(&ent), want, "GC collected a reachable node — reopen state changed");
}

#[test]
fn ent_store_survives_reopen_matching_fjall() {
    for seed in 1..=8u64 {
        let mut rng = Rng::new(seed ^ 0xDEAD);
        let edir = tempfile::tempdir().unwrap();
        let fdir = tempfile::tempdir().unwrap();
        // Build both durable stores with the same random history, then drop them.
        {
            let ent = EntStore::open(edir.path()).unwrap();
            let fj = FjallStore::open(fdir.path()).unwrap();
            for round in 0..60 {
                drive_one(&mut rng, &ent, &fj);
                assert_agree(seed, round, &mut rng, &ent, &fj);
            }
        }
        // Reopen from disk: the ent rebuilds from its persisted enfilades and must
        // still match the LSM on every observable.
        let ent = EntStore::open(edir.path()).unwrap();
        let fj = FjallStore::open(fdir.path()).unwrap();
        assert_agree(seed, 9999, &mut rng, &ent, &fj);
        // And it keeps working after the reopen.
        for round in 0..30 {
            drive_one(&mut rng, &ent, &fj);
            assert_agree(seed, 10000 + round, &mut rng, &ent, &fj);
        }
    }
}
