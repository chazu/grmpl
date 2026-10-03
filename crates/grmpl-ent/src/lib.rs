//! # grmpl-ent
//!
//! The **Ent**: grmpl's authoritative substrate as a family of persistent,
//! measured, versioned trees over a shared content-addressed node store (the
//! [`granfilade`]), modeled on Xanadu Gold's `Ent`.
//!
//! The trees ([`tree::Tree`]) are path-copied B+trees with monoidal subtree
//! measures and, as in Gold, a **dsp on every pointer**: nodes keep their keys in
//! a local frame, so [`relocate`](tree::Tree::relocate) is `O(1)` and
//! [`graft`](tree::Tree::graft) — the virtual copy behind
//! [`EntStore::instance_template`] — costs `O(log n)` new nodes. What a
//! displacement does to a key is [`Displace`].
//!
//! **Everything is in the Ent.** The granfilade has one mutable slot, its root
//! record, linking to the branch DAG and the branch enfilade; each branch's
//! state — its relations' versions, logs and Arrangements, its context enfilade,
//! its canopy — is trees linked from there. Nodes page in on demand, so opening
//! a world reads a couple of frames whatever its size. See
//! `docs/ENT-AND-XANADU.md` for how this compares with Xanadu's `Ent` and what
//! is still missing.

pub mod canopy;
pub mod context;
pub mod dag;
pub mod dsp;
pub mod granfilade;
pub mod measure;
pub mod store;
pub mod tree;

pub use canopy::{Canopy, Endorsement, InterestId};
pub use context::{ContextEnf, Scope};
pub use dag::{Branch, BranchId, Dag};
pub use dsp::Displace;
pub use granfilade::{Granfilade, Persist};
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

    /// The in-order walk is sorted and the length matches; balance itself is
    /// checked structurally by `Tree::check` in the churn oracles below.
    #[test]
    fn ordered_and_sized() {
        let mut t = T::new();
        for k in [5, 2, 8, 1, 9, 3, 7, 0, 6, 4] {
            t = t.insert(k, k * 10);
        }
        assert_eq!(t.len(), 10);
        let ks: Vec<i64> = t.iter().map(|(k, _)| k).collect();
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
        assert_eq!(v0.iter().map(|(k, _)| k).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(v1.iter().map(|(k, _)| k).collect::<Vec<_>>(), vec![1, 3, 4]);
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
                let tk: Vec<(i64, i64)> = t.iter().map(|(k, v)| (k, *v)).collect();
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
                let tk: Vec<i64> = ts.iter().map(|(k, _)| k).collect();
                let rk: Vec<i64> = rs.keys().copied().collect();
                assert_eq!(tk, rk, "seed {seed}: a retained snapshot mutated");
            }
        }
    }

    // ---- displaced trees: relocate, split, join, graft ---------------------

    use grmpl_core::{Entity, Tuple, Value};

    type F = Tree<Tuple, i64, Count>;
    type Oracle = BTreeMap<Tuple, i64>;

    /// A three-column key with two entity cells, so a displacement has to move
    /// more than the lead column.
    fn key(e: u64, n: i64) -> Tuple {
        Tuple::from([Value::Ent(Entity(e)), Value::Int(n), Value::Ent(Entity(e + 7))])
    }

    fn lead(e: u64) -> Tuple {
        Tuple::from([Value::Ent(Entity(e))])
    }

    fn contents(t: &F) -> Vec<(Tuple, i64)> {
        t.iter().map(|(k, v)| (k, *v)).collect()
    }

    fn oracle_contents(r: &Oracle) -> Vec<(Tuple, i64)> {
        r.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Every read path against the oracle: contents, size, point lookups, range
    /// collection and measure, emptiness probes, and as-of lookup.
    fn agree(t: &F, r: &Oracle, rng: &mut Rng, ctx: &str) {
        t.check();
        assert_eq!(contents(t), oracle_contents(r), "{ctx}: contents");
        assert_eq!(t.len(), r.len(), "{ctx}: size");
        assert_eq!(t.measure(), Count(r.len() as u64), "{ctx}: measure");
        for _ in 0..6 {
            let k = key(rng.below(2600), rng.below(3) as i64);
            assert_eq!(t.get(&k), r.get(&k), "{ctx}: get {k:?}");
            let want = r.range(..=k.clone()).next_back().map(|(k, v)| (k.clone(), *v));
            assert_eq!(t.last_le(&k).map(|(k, v)| (k, *v)), want, "{ctx}: last_le");
            let (a, b) = (rng.below(2600), rng.below(2600));
            let (lo, hi) = (lead(a.min(b)), lead(a.max(b)));
            let span: Vec<(Tuple, i64)> =
                r.range(lo.clone()..hi.clone()).map(|(k, v)| (k.clone(), *v)).collect();
            assert_eq!(t.range_collect(&lo, &hi), span, "{ctx}: range");
            assert_eq!(t.measure_range(&lo, &hi), Count(span.len() as u64), "{ctx}: range measure");
            assert_eq!(t.any_in(&lo, &hi), !span.is_empty(), "{ctx}: any_in");
        }
    }

    #[test]
    fn relocation_moves_every_key_and_shares_the_root() {
        let mut t = F::new();
        let mut r = Oracle::new();
        for e in 0..300u64 {
            t = t.insert(key(e, 0), e as i64);
            r.insert(key(e, 0), e as i64);
        }
        let moved = t.relocate(1000);
        assert!(moved.dsp() == 1000 && t.dsp() == 0);
        let mut want: Oracle = r.iter().map(|(k, v)| (k.displace(1000), *v)).collect();
        let mut rng = Rng::new(7);
        agree(&moved, &want, &mut rng, "relocated");
        // Edits to the relocated tree land in its coordinates and leave the
        // original alone.
        let edited = moved.insert(key(1500, 1), -1).remove(&key(1010, 0));
        want.insert(key(1500, 1), -1);
        want.remove(&key(1010, 0));
        agree(&edited, &want, &mut rng, "edited relocation");
        agree(&t, &r, &mut rng, "original after relocation");
        // A relocated tree persists in normalized form and reloads equal.
        let dir = tempfile::tempdir().unwrap();
        let gran = crate::Granfilade::open(dir.path()).unwrap();
        let ck = gran.persist(&edited).unwrap();
        let back: F = gran.load(ck).unwrap();
        agree(&back, &want, &mut rng, "reloaded relocation");
    }

    /// **The displaced-tree law oracle.** Random insert / remove / graft /
    /// split+join churn on entity-keyed trees, checked after every step against a
    /// `BTreeMap` that performs each graft as an explicit copy — and every
    /// structural invariant checked through the displaced frames. Old versions
    /// must stay intact, and `diff` between any two must match the oracle's.
    #[test]
    fn displaced_tree_matches_btreemap_under_graft_split_join_churn() {
        for seed in 1..=40u64 {
            let mut rng = Rng::new(seed);
            let mut t = F::new();
            let mut r = Oracle::new();
            let mut snapshots: Vec<(F, Oracle)> = Vec::new();
            for round in 0..300 {
                let ctx = format!("seed {seed} round {round}");
                match rng.below(12) {
                    0..=5 => {
                        let k = key(rng.below(300), rng.below(3) as i64);
                        let v = rng.next() as i64;
                        t = t.insert(k.clone(), v);
                        r.insert(k, v);
                    }
                    6..=7 => {
                        let k = key(rng.below(300), rng.below(3) as i64);
                        t = t.remove(&k);
                        r.remove(&k);
                    }
                    8..=9 => {
                        // Copy a block of leads `[e0, e0 + w)` by `by`. Targets
                        // land in a few fixed blocks, so later grafts sometimes
                        // collide with earlier ones and must be refused.
                        let e0 = rng.below(300);
                        let w = 1 + rng.below(120);
                        // Mostly upward into a few fixed blocks; sometimes a short
                        // hop down, which wraps the id space for a low block and
                        // must then be refused.
                        let by = [1000, 1400, 1800, 2200, 50, 450, -150][rng.below(7) as usize];
                        let (lo, hi) = (lead(e0), lead(e0 + w));
                        let (tlo, thi) = (lo.displace(by), hi.displace(by));
                        let copy: Vec<(Tuple, i64)> = r
                            .range(lo.clone()..hi.clone())
                            .map(|(k, v)| (k.displace(by), *v))
                            .collect();
                        let wraps = tlo >= thi;
                        let free = wraps || r.range(tlo..thi).next().is_none();
                        let got = t.graft(&lo, &hi, by);
                        if copy.is_empty() {
                            assert!(got.is_some(), "{ctx}: empty graft refused");
                        } else if wraps {
                            assert!(got.is_none(), "{ctx}: a wrapping graft was accepted");
                        } else if free {
                            t = got.unwrap_or_else(|| panic!("{ctx}: graft into a free block refused"));
                            r.extend(copy);
                        } else {
                            assert!(got.is_none(), "{ctx}: graft over an occupied block accepted");
                        }
                    }
                    _ => {
                        let at = key(rng.below(2600), rng.below(3) as i64);
                        let (a, b) = t.split(&at);
                        a.check();
                        b.check();
                        assert!(a.iter().all(|(k, _)| k < at), "{ctx}: left half reaches the cut");
                        assert!(b.iter().all(|(k, _)| k >= at), "{ctx}: right half below the cut");
                        assert_eq!(a.len() + b.len(), t.len(), "{ctx}: split lost entries");
                        t = F::join(&a, &b);
                    }
                }
                agree(&t, &r, &mut rng, &ctx);
                if round % 60 == 0 {
                    snapshots.push((t.clone(), r.clone()));
                }
            }
            for (i, (ts, rs)) in snapshots.iter().enumerate() {
                assert_eq!(contents(ts), oracle_contents(rs), "seed {seed}: snapshot {i} mutated");
                let mut want: Vec<(Tuple, Option<i64>, Option<i64>)> = Vec::new();
                let keys: std::collections::BTreeSet<&Tuple> = rs.keys().chain(r.keys()).collect();
                for k in keys {
                    let (a, b) = (rs.get(k).copied(), r.get(k).copied());
                    if a != b {
                        want.push((k.clone(), a, b));
                    }
                }
                assert_eq!(ts.diff(&t), want, "seed {seed}: diff from snapshot {i}");
            }
        }
    }

    /// A graft shares the copied span: persisting the grafted version adds
    /// `O(log n)` nodes — the new spines along the cuts — however large the span
    /// is, where copying the facts one by one would add `O(span / B)`.
    #[test]
    fn graft_persists_in_log_n_new_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let gran = crate::Granfilade::open(dir.path()).unwrap();
        let mut t = F::new();
        for e in 0..20_000u64 {
            t = t.insert(key(e, 0), 1);
        }
        gran.persist(&t).unwrap();
        let before = gran.node_count().unwrap();
        // Copy 10 000 facts (some 300 leaves) to a fresh block.
        let grafted = t.graft(&lead(5_000), &lead(15_000), 100_000).unwrap();
        assert_eq!(grafted.len(), 30_000);
        let ck = gran.persist(&grafted).unwrap();
        let added = gran.node_count().unwrap() - before;
        assert!(added <= 16, "graft added {added} nodes; a copy would add hundreds");
        // And it reads back as the copy.
        let back: F = gran.load(ck).unwrap();
        back.check();
        assert_eq!(back.get(&key(112_345, 0)), Some(&1));
        assert_eq!(back.get(&key(115_000, 0)), None);
        assert_eq!(back.measure_range(&lead(105_000), &lead(115_000)), Count(10_000));
    }
}
