//! **The canopy: interest routing (E4).**
//!
//! A canopy indexes *standing interests* — a watcher's `(relation, key-range)`
//! subscription — and, given a commit's updates, reports which interests a change
//! touched, so a delta is routed only to watchers that care (Xanadu's sensor
//! canopy; grmpl's Attention law: reactivity is a maintained query, not an
//! unrelated callback). Routing is **conservative**: it returns every interest a
//! change *could* affect (a superset), never a subset — so no watcher ever misses
//! a delta (the Snapshot–stream law).
//!
//! Routing is answered by a **measured interval tree** (Gold's CanopyCrum): per
//! relation, the interests are held in an array sorted by low endpoint, augmented
//! with a segment tree of the **maximum high endpoint** over every subrange (the
//! upward `wid`-style summary). A point stab descends only into subranges whose
//! `max-hi` clears the point, so routing is `O(log n + k)` in the number of hit
//! interests rather than `O(n)` over all of them — the same measure-pruning that
//! powers the Fact enfilade's WID range read. Each interest also carries an
//! **endorsement** (a monotone flag-lattice element); [`route_endorsed`] gates
//! delivery on an interest holding every required flag — Gold's authority
//! endorsement, the lattice test `required ⊑ endorsement`.
//!
//! [`route_endorsed`]: Canopy::route_endorsed

use std::collections::BTreeMap;

use grmpl_core::{Diff, RelId, Tuple};

/// A handle to a registered interest.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct InterestId(pub u64);

/// An element of the endorsement flag-lattice — a set of authority flags, ordered
/// by inclusion (`⊑` is subset). The join is union; the top ([`Endorsement::ALL`])
/// holds every flag, so a plainly-registered interest satisfies any requirement.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Endorsement(pub u64);

impl Endorsement {
    /// The lattice top: every flag set. A plain interest carries this, so it
    /// passes any endorsement requirement.
    pub const ALL: Endorsement = Endorsement(u64::MAX);
    /// The lattice bottom: no flags.
    pub const NONE: Endorsement = Endorsement(0);

    /// Does this endorsement dominate `required` — hold every flag it demands
    /// (`required ⊑ self`)? This is the lattice order test routing gates on.
    pub fn dominates(self, required: Endorsement) -> bool {
        self.0 & required.0 == required.0
    }
}

/// One standing interest: rows of `rel` whose key lies in `[lo, hi)`, carrying an
/// endorsement.
#[derive(Clone)]
struct Interest {
    id: InterestId,
    rel: RelId,
    lo: Tuple,
    hi: Tuple,
    endorse: Endorsement,
}

/// The parameters of one stabbing query, bundled so the recursion stays a
/// two-index walk.
struct Stab<'a> {
    point: &'a Tuple,
    min_endorse: Endorsement,
    /// Right bound: candidates are `items[..qr]` (those with `lo ≤ point`).
    qr: usize,
}

/// A per-relation interval index: interests sorted by `lo`, with a segment tree of
/// the maximum `hi` over each subrange for measure-pruned stabbing.
struct RelIndex {
    /// Interests of one relation, sorted ascending by `lo` (so every prefix is
    /// exactly the interests whose low endpoint is ≤ a given point).
    items: Vec<Interest>,
    /// Segment tree (`4·n`) over `items`, `seg[node]` = the maximum `hi` in the
    /// node's subrange — the upward measure that lets a stab skip subranges whose
    /// intervals all end at or before the point.
    seg: Vec<Tuple>,
}

impl RelIndex {
    fn build(mut items: Vec<Interest>) -> RelIndex {
        items.sort_by(|a, b| a.lo.cmp(&b.lo));
        let n = items.len();
        // `hi` values are non-empty tuples; the first item's hi is a valid
        // placeholder for never-queried cells (n==0 is handled by callers).
        let placeholder = items.first().map(|i| i.hi.clone()).unwrap_or_else(|| Tuple::from([]));
        let mut idx = RelIndex { items, seg: vec![placeholder; 4 * n.max(1)] };
        if n > 0 {
            idx.build_seg(0, 0, n);
        }
        idx
    }

    fn build_seg(&mut self, node: usize, sl: usize, sr: usize) -> Tuple {
        if sr - sl == 1 {
            self.seg[node] = self.items[sl].hi.clone();
            return self.seg[node].clone();
        }
        let mid = (sl + sr) / 2;
        let l = self.build_seg(2 * node + 1, sl, mid);
        let r = self.build_seg(2 * node + 2, mid, sr);
        let m = l.max(r);
        self.seg[node] = m.clone();
        m
    }

    /// Collect the interests whose `[lo, hi)` contains `q.point`, pushing ids into
    /// `out`. `q.qr` bounds the candidates to those with `lo ≤ point` (a prefix);
    /// the `max-hi` measure prunes subranges that cannot match.
    fn stab(&self, node: usize, sl: usize, sr: usize, q: &Stab, out: &mut Vec<InterestId>) {
        if sl >= q.qr || self.items.is_empty() {
            return;
        }
        // Measure prune: if the whole subrange's greatest `hi` is ≤ point, no
        // interval here reaches past `point`, so none can contain it.
        if self.seg[node] <= *q.point {
            return;
        }
        if sr - sl == 1 {
            // Leaf in-bounds (sl < qr ⇒ lo ≤ point); match iff point < hi.
            let it = &self.items[sl];
            if *q.point < it.hi && it.endorse.dominates(q.min_endorse) {
                out.push(it.id);
            }
            return;
        }
        let mid = (sl + sr) / 2;
        self.stab(2 * node + 1, sl, mid, q, out);
        self.stab(2 * node + 2, mid, sr, q, out);
    }

    /// The ids whose interval contains `point` and whose endorsement dominates
    /// `min_endorse`.
    fn hits(&self, point: &Tuple, min_endorse: Endorsement, out: &mut Vec<InterestId>) {
        // Candidates: the prefix of items with `lo ≤ point`.
        let qr = self.items.partition_point(|i| i.lo <= *point);
        self.stab(0, 0, self.items.len(), &Stab { point, min_endorse, qr }, out);
    }
}

/// The canopy: a set of standing interests over the world, indexed per relation
/// by a measured interval tree.
#[derive(Default)]
pub struct Canopy {
    next: u64,
    interests: Vec<Interest>,
    /// Per-relation interval index, rebuilt on mutation (register/unregister).
    index: BTreeMap<RelId, RelIndex>,
}

impl Canopy {
    pub fn new() -> Canopy {
        Canopy::default()
    }

    /// Register interest in the rows of `rel` in `[lo, hi)`, with full
    /// endorsement (passes any requirement). Returns a handle.
    pub fn register(&mut self, rel: RelId, lo: Tuple, hi: Tuple) -> InterestId {
        self.register_endorsed(rel, lo, hi, Endorsement::ALL)
    }

    /// Register interest carrying an explicit [`Endorsement`] — routing via
    /// [`route_endorsed`](Self::route_endorsed) delivers to it only when its
    /// endorsement dominates the required flags.
    pub fn register_endorsed(&mut self, rel: RelId, lo: Tuple, hi: Tuple, endorse: Endorsement) -> InterestId {
        let id = InterestId(self.next);
        self.next += 1;
        self.interests.push(Interest { id, rel, lo, hi, endorse });
        self.reindex(rel);
        id
    }

    /// Drop a registered interest.
    pub fn unregister(&mut self, id: InterestId) {
        if let Some(pos) = self.interests.iter().position(|i| i.id == id) {
            let rel = self.interests[pos].rel;
            self.interests.remove(pos);
            self.reindex(rel);
        }
    }

    /// Rebuild the interval index for one relation from the interest list.
    fn reindex(&mut self, rel: RelId) {
        let items: Vec<Interest> =
            self.interests.iter().filter(|i| i.rel == rel).cloned().collect();
        if items.is_empty() {
            self.index.remove(&rel);
        } else {
            self.index.insert(rel, RelIndex::build(items));
        }
    }

    /// Number of live interests.
    pub fn len(&self) -> usize {
        self.interests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.interests.is_empty()
    }

    /// The interests touched by `updates` — every interest whose `(rel, [lo, hi))`
    /// contains at least one updated tuple, deduplicated and sorted. A watcher
    /// not in the result is provably unaffected by this commit. Answered by the
    /// measured interval tree in `O(log n + k)` per updated tuple.
    pub fn route(&self, updates: &[(RelId, Tuple, Diff)]) -> Vec<InterestId> {
        self.route_endorsed(updates, Endorsement::NONE)
    }

    /// [`route`](Self::route) gated by the endorsement lattice: deliver only to
    /// interests whose endorsement **dominates** `required` (holds every required
    /// flag). `Endorsement::NONE` gates nothing, so `route` is this at the lattice
    /// bottom.
    pub fn route_endorsed(&self, updates: &[(RelId, Tuple, Diff)], required: Endorsement) -> Vec<InterestId> {
        let mut hit: Vec<InterestId> = Vec::new();
        for (rel, tuple, _diff) in updates {
            if let Some(idx) = self.index.get(rel) {
                idx.hits(tuple, required, &mut hit);
            }
        }
        hit.sort();
        hit.dedup();
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grmpl_core::Value;

    fn t(n: i64) -> Tuple {
        Tuple::from([Value::Int(n)])
    }

    #[test]
    fn routes_only_to_overlapping_interests() {
        let mut c = Canopy::new();
        // A: rel 1, keys [0, 10);  B: rel 1, keys [10, 20);  C: rel 2, any.
        let a = c.register(RelId(1), t(0), t(10));
        let b = c.register(RelId(1), t(10), t(20));
        let cc = c.register(RelId(2), t(i64::MIN), t(i64::MAX));

        // An update to rel 1 key 5 hits only A.
        assert_eq!(c.route(&[(RelId(1), t(5), 1)]), vec![a]);
        // key 15 hits only B.
        assert_eq!(c.route(&[(RelId(1), t(15), -1)]), vec![b]);
        // rel 2 hits only C.
        assert_eq!(c.route(&[(RelId(2), t(999), 1)]), vec![cc]);
        // A batch touching both ranges of rel 1 hits A and B (not C).
        assert_eq!(c.route(&[(RelId(1), t(3), 1), (RelId(1), t(12), 1)]), vec![a, b]);
        // An update outside every interest routes to no one.
        assert_eq!(c.route(&[(RelId(1), t(50), 1)]), Vec::<InterestId>::new());
        assert_eq!(c.route(&[(RelId(3), t(5), 1)]), Vec::<InterestId>::new());

        // Unregister B: its range no longer routes.
        c.unregister(b);
        assert_eq!(c.route(&[(RelId(1), t(15), 1)]), Vec::<InterestId>::new());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn endorsement_lattice_gates_delivery() {
        let mut c = Canopy::new();
        // Two overlapping interests with different endorsements over rel 1.
        let admin = c.register_endorsed(RelId(1), t(0), t(100), Endorsement(0b11));
        let guest = c.register_endorsed(RelId(1), t(0), t(100), Endorsement(0b01));

        // Ungated: both fire.
        assert_eq!(c.route(&[(RelId(1), t(5), 1)]), vec![admin, guest]);
        // Requiring flag 0b01: both hold it.
        assert_eq!(c.route_endorsed(&[(RelId(1), t(5), 1)], Endorsement(0b01)), vec![admin, guest]);
        // Requiring flag 0b10: only admin holds it.
        assert_eq!(c.route_endorsed(&[(RelId(1), t(5), 1)], Endorsement(0b10)), vec![admin]);
        // Requiring both flags: only admin dominates.
        assert_eq!(c.route_endorsed(&[(RelId(1), t(5), 1)], Endorsement(0b11)), vec![admin]);
        // A point outside the range: nobody, regardless of endorsement.
        assert!(c.route_endorsed(&[(RelId(1), t(200), 1)], Endorsement::NONE).is_empty());
    }

    /// The interval tree must agree with the naive linear stab on every point,
    /// across random interest sets — the measure-pruned routing is a pure speedup,
    /// not a semantic change.
    #[test]
    fn interval_tree_matches_linear_scan_under_random_churn() {
        struct Rng(u64);
        impl Rng {
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

        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut c = Canopy::new();
            // A reference list mirroring the registered intervals.
            let mut refs: Vec<(InterestId, i64, i64)> = Vec::new();

            for _ in 0..30 {
                let a = rng.below(40) as i64 - 5;
                let w = rng.below(12) as i64;
                let (lo, hi) = (a, a + w); // possibly empty when w == 0
                let id = c.register(RelId(1), t(lo), t(hi));
                refs.push((id, lo, hi));

                // Occasionally drop an existing interest.
                if rng.below(4) == 0 && !refs.is_empty() {
                    let victim = rng.below(refs.len() as u64) as usize;
                    let (vid, _, _) = refs.remove(victim);
                    c.unregister(vid);
                }

                // Every point must route identically to the linear oracle.
                for _ in 0..6 {
                    let p = rng.below(50) as i64 - 5;
                    let mut want: Vec<InterestId> =
                        refs.iter().filter(|(_, lo, hi)| *lo <= p && p < *hi).map(|(id, _, _)| *id).collect();
                    want.sort();
                    want.dedup();
                    assert_eq!(c.route(&[(RelId(1), t(p), 1)]), want, "seed {seed}, point {p}");
                }
            }
        }
    }
}
