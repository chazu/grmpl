//! **Conformance oracle: `EntStore` ≡ `FjallStore` on the store contract.**
//!
//! The ent-native store and the LSM store are driven with the *same* randomized
//! sequence of commit / commit_if / consolidate, and after every op must agree on
//! every observable: the edition clock, the watermark, `read_at` (net, sorted) at
//! current and past editions, `scan_updates` (raw, commit-ordered, per-multiplicity)
//! over random ranges, `commit_if` outcomes, and the below-watermark edition doors.
//! This is the E1 acceptance in miniature — proving the enfilade family reproduces
//! the store's semantics exactly, so the semantic core runs over it unchanged.

use grmpl_core::{Diff, Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;
use grmpl_store::FjallStore;

/// Deterministic xorshift64* — the repo's seeded-oracle idiom.
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

/// The full net state of every relation at the current edition.
fn snapshot(s: &dyn TraceStore) -> Vec<Vec<(Tuple, Diff)>> {
    let cur = s.current();
    RELS.iter().map(|r| s.read_at(*r, cur).unwrap()).collect()
}

#[test]
fn ent_store_matches_fjall_under_random_churn() {
    for seed in 1..=16u64 {
        let mut rng = Rng::new(seed);
        let ent = EntStore::new();
        let dir = tempfile::tempdir().unwrap();
        let fj = FjallStore::open(dir.path()).unwrap();

        for round in 0..150 {
            match rng.below(10) {
                0..=5 => {
                    // commit: 1..=3 updates
                    let n = 1 + rng.below(3);
                    let updates: Vec<(RelId, Tuple, Diff)> = (0..n)
                        .map(|_| {
                            let r = RELS[rng.below(3) as usize];
                            let t = tup(rng.below(6));
                            let d = if rng.below(2) == 0 { 1 } else { -1 };
                            (r, t, d)
                        })
                        .collect();
                    let e1 = ent.commit(&updates).unwrap();
                    let e2 = fj.commit(&updates).unwrap();
                    assert_eq!(e1, e2, "seed {seed} round {round}: commit edition");
                }
                6..=8 => {
                    // commit_if: 0..=2 preconditions + one update
                    let np = rng.below(3);
                    let pre: Vec<(RelId, Tuple)> = (0..np)
                        .map(|_| (RELS[rng.below(3) as usize], tup(rng.below(6))))
                        .collect();
                    let upd = vec![(
                        RELS[rng.below(3) as usize],
                        tup(rng.below(6)),
                        if rng.below(2) == 0 { 1 } else { -1 },
                    )];
                    let o1 = ent.commit_if(&pre, &upd).unwrap();
                    let o2 = fj.commit_if(&pre, &upd).unwrap();
                    assert_eq!(
                        o1.is_some(),
                        o2.is_some(),
                        "seed {seed} round {round}: commit_if outcome (pre={pre:?})"
                    );
                }
                _ => {
                    // consolidate up to a random edition ≤ current
                    let cur = ent.current().0;
                    let up = Edition(rng.below(cur + 1));
                    let w1 = ent.consolidate(up).unwrap();
                    let w2 = fj.consolidate(up).unwrap();
                    assert_eq!(w1, w2, "seed {seed} round {round}: consolidate watermark");
                }
            }

            // --- observable agreement after every op ---
            assert_eq!(ent.current(), fj.current(), "seed {seed} round {round}: current");
            assert_eq!(ent.watermark(), fj.watermark(), "seed {seed} round {round}: watermark");
            assert_eq!(
                snapshot(&ent),
                snapshot(&fj),
                "seed {seed} round {round}: read_at(current) diverged"
            );

            let wm = ent.watermark().0;
            let cur = ent.current().0;

            // scan_updates over a random range ≥ watermark: raw, ordered, equal.
            let a = wm + rng.below(cur - wm + 1);
            let b = wm + rng.below(cur - wm + 1);
            let (from, to) = (Edition(a.min(b)), Edition(a.max(b)));
            for r in RELS {
                assert_eq!(
                    ent.scan_updates(r, from, to).unwrap(),
                    fj.scan_updates(r, from, to).unwrap(),
                    "seed {seed} round {round}: scan_updates {r:?} ({},{}]",
                    from.0,
                    to.0
                );
            }

            // read_at as-of a random past edition ≥ watermark.
            let at = Edition(wm + rng.below(cur - wm + 1));
            for r in RELS {
                assert_eq!(
                    ent.read_at(r, at).unwrap(),
                    fj.read_at(r, at).unwrap(),
                    "seed {seed} round {round}: read_at {r:?} as-of {}",
                    at.0
                );
            }

            // Edition doors: below the watermark, both must error identically.
            if wm > 0 {
                let below = Edition(wm - 1);
                assert_eq!(
                    ent.read_at(RELS[0], below).is_err(),
                    fj.read_at(RELS[0], below).is_err(),
                    "seed {seed} round {round}: read_at door"
                );
                assert_eq!(
                    ent.scan_updates(RELS[0], below, ent.current()).is_err(),
                    fj.scan_updates(RELS[0], below, fj.current()).is_err(),
                    "seed {seed} round {round}: scan_updates door"
                );
            }
        }
    }
}
