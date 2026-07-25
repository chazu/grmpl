//! # grmpl-ent
//!
//! The **Ent**: grmpl's authoritative substrate as a coordinated family of
//! persistent, measured, versioned *enfilades* over a shared granfilade — the
//! goal being parity with Xanadu Gold's `Ent`
//! (see [`docs/ENT-MIGRATION-PLAN.md`](../../../docs/ENT-MIGRATION-PLAN.md) and
//! [`docs/ENT-AND-XANADU.md`](../../../docs/ENT-AND-XANADU.md)).
//!
//! This crate is being built bottom-up. Present: the enfilade primitive — a
//! persistent measured weight-balanced [`tree::Tree`] with WID range measures.
//! Next: the granfilade (content-interned node store on fjall), then the Edition
//! and Fact enfilades implementing the `grmpl-core` store traits.

pub mod measure;
pub mod store;
pub mod tree;

pub use measure::{Count, Measure};
pub use store::EntStore;
pub use tree::Tree;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Deterministic xorshift64* — the repo's seeded-oracle idiom (no `rand`).
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
            self.next() % n
        }
    }

    type T = Tree<i64, i64, Count>;

    fn count_in(map: &BTreeMap<i64, i64>, lo: i64, hi: i64) -> u64 {
        map.range(lo..hi).count() as u64
    }

    /// Black-height-ish sanity: a weight-balanced tree of n entries has depth
    /// O(log n). We assert the in-order walk is sorted and length matches — the
    /// balance itself is exercised by keeping operations O(log n) under churn.
    #[test]
    fn ordered_and_sized() {
        let mut t = T::new();
        for k in [5, 2, 8, 1, 9, 3, 7, 0, 6, 4] {
            t = t.insert(k, k * 10);
        }
        assert_eq!(t.len(), 10);
        let ks: Vec<i64> = t.iter().map(|(k, _)| *k).collect();
        assert_eq!(ks, (0..10).collect::<Vec<_>>());
        for k in 0..10 {
            assert_eq!(t.get(&k), Some(&(k * 10)));
        }
        assert_eq!(t.get(&99), None);
    }

    #[test]
    fn persistence_shares_old_versions() {
        let v0 = T::new().insert(1, 1).insert(2, 2).insert(3, 3);
        let v1 = v0.insert(4, 4).remove(&2);
        // v0 is unchanged by operations that produced v1.
        assert_eq!(v0.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(v1.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![1, 3, 4]);
        assert_eq!(v0.len(), 3);
        assert_eq!(v1.len(), 3);
    }

    #[test]
    fn measure_is_the_fold_and_range_is_wid_pruned() {
        let mut t = T::new();
        for k in 0..50i64 {
            t = t.insert(k, k);
        }
        assert_eq!(t.measure(), Count(50));
        // WID range measure equals the true count over the span, for every span.
        for lo in [-5, 0, 7, 20, 49, 60] {
            for hi in [-5, 0, 8, 21, 50, 100] {
                let want = if lo < hi { (lo.max(0)..hi.min(50)).count() as u64 } else { 0 };
                assert_eq!(t.measure_range(&lo, &hi), Count(want), "range [{lo},{hi})");
            }
        }
    }

    /// The seeded law oracle: random insert/remove churn, every round the tree
    /// must agree with a `BTreeMap` reference on ordering, membership, size, total
    /// measure, and WID range measure — across 24 seeds.
    #[test]
    fn tree_matches_btreemap_under_random_churn() {
        for seed in 1..=24u64 {
            let mut rng = Rng::new(seed);
            let mut t = T::new();
            let mut r: BTreeMap<i64, i64> = BTreeMap::new();
            // Keep a couple of old versions to check persistence/immutability.
            let mut snapshots: Vec<(T, BTreeMap<i64, i64>)> = Vec::new();

            for round in 0..200 {
                let k = rng.below(40) as i64;
                if rng.below(3) == 0 {
                    t = t.remove(&k);
                    r.remove(&k);
                } else {
                    let v = rng.next() as i64;
                    t = t.insert(k, v);
                    r.insert(k, v);
                }
                if round % 40 == 0 {
                    snapshots.push((t.clone(), r.clone()));
                }

                // Ordering + membership + size.
                let tk: Vec<(i64, i64)> = t.iter().map(|(k, v)| (*k, *v)).collect();
                let rk: Vec<(i64, i64)> = r.iter().map(|(k, v)| (*k, *v)).collect();
                assert_eq!(tk, rk, "seed {seed} round {round}: contents diverged");
                assert_eq!(t.len(), r.len(), "seed {seed} round {round}: size");
                assert_eq!(t.measure(), Count(r.len() as u64), "seed {seed}: total measure");

                // WID range measure vs truth, several spans.
                for _ in 0..4 {
                    let a = rng.below(50) as i64 - 5;
                    let b = rng.below(50) as i64 - 5;
                    let (lo, hi) = (a.min(b), a.max(b));
                    assert_eq!(
                        t.measure_range(&lo, &hi),
                        Count(count_in(&r, lo, hi)),
                        "seed {seed} round {round}: range [{lo},{hi})"
                    );
                }
            }
            // Old snapshots are immutable and still correct.
            for (ts, rs) in &snapshots {
                let tk: Vec<i64> = ts.iter().map(|(k, _)| *k).collect();
                let rk: Vec<i64> = rs.keys().copied().collect();
                assert_eq!(tk, rk, "seed {seed}: a retained snapshot mutated");
            }
        }
    }
}
