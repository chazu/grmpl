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
//! Routing is answered by a **measured interval enfilade** (Gold's CanopyCrum):
//! the interests live in the same persistent measured [`Tree`] as everything else
//! in the plex, keyed by `(rel, lo, id)` so one relation's interests are a
//! contiguous span in low-endpoint order, and carrying an upward measure of the
//! **maximum high endpoint** plus the **join of the endorsements** beneath each
//! node. A point stab prunes any subtree whose `max-hi` does not clear the point
//! — the same WID pruning that powers the Fact enfilade's range read — and any
//! subtree whose joined endorsement cannot satisfy the requirement.
//!
//! It is an enfilade rather than an array-plus-segment-tree (G-4) so that
//! registering an interest is an `O(log n)` persistent insert instead of an
//! `O(n log n)` rebuild, and so the canopy **versions, persists and GCs with the
//! rest of the world** rather than sitting beside it in memory.
//!
//! Each interest carries an **endorsement** (a monotone flag-lattice element);
//! [`route_endorsed`] gates delivery on an interest holding every required flag —
//! Gold's authority endorsement, the lattice test `required ⊑ endorsement`.
//!
//! [`route_endorsed`]: Canopy::route_endorsed

use grmpl_core::{Diff, RelId, Tuple};

use crate::measure::Measure;
use crate::tree::{NodeRef, Tree};

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

/// The upward measure of a canopy subtree: the greatest `hi` endpoint under it,
/// and the join (union) of every endorsement under it.
///
/// `max-hi` is what makes a stab prune — a subtree whose intervals all end at or
/// before the point cannot contain it — and the endorsement join is what lets a
/// gated stab skip a subtree none of whose interests could satisfy the
/// requirement. Both are monoids, so they ride the ordinary enfilade.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reach {
    /// The greatest `hi` under this subtree; `None` for the empty subtree.
    pub max_hi: Option<Tuple>,
    /// The union of endorsements under this subtree.
    pub endorse: Endorsement,
}

/// The key of a canopy entry: `(relation, low endpoint, interest id)`. Ordering
/// by `(rel, lo)` puts one relation's interests in a contiguous, low-endpoint-
/// ordered span; the id breaks ties so two identical intervals both survive.
pub type InterestKey = (u32, Tuple, u64);

/// What an interest binds: its half-open upper endpoint and its endorsement.
pub type InterestVal = (Tuple, Endorsement);

impl Measure<InterestKey, InterestVal> for Reach {
    fn empty() -> Self {
        Reach { max_hi: None, endorse: Endorsement::NONE }
    }
    fn entry(_k: &InterestKey, v: &InterestVal) -> Self {
        Reach { max_hi: Some(v.0.clone()), endorse: v.1 }
    }
    fn combine(&self, right: &Self) -> Self {
        let max_hi = match (&self.max_hi, &right.max_hi) {
            (None, r) => r.clone(),
            (l, None) => l.clone(),
            (Some(l), Some(r)) => Some(l.max(r).clone()),
        };
        Reach { max_hi, endorse: Endorsement(self.endorse.0 | right.endorse.0) }
    }
}

/// The canopy enfilade: standing interests as a persistent measured tree.
pub type CanopyEnf = Tree<InterestKey, InterestVal, Reach>;

/// An endorsement persists as its flag word, so a canopy can be written to the
/// granfilade like any other enfilade.
impl crate::granfilade::Persist for Endorsement {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
    }
    fn decode(bytes: &[u8], pos: usize) -> grmpl_core::Result<(Self, usize)> {
        let end = pos + 8;
        let b = bytes
            .get(pos..end)
            .ok_or_else(|| grmpl_core::Error::Codec("canopy: truncated endorsement".into()))?;
        Ok((Endorsement(u64::from_be_bytes(b.try_into().unwrap())), end))
    }
}

/// The canopy: standing interests over the world, held in one measured enfilade.
#[derive(Default, Clone)]
pub struct Canopy {
    next: u64,
    enf: CanopyEnf,
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
    /// endorsement dominates the required flags. `O(log n)`, persistent: the
    /// prior canopy version is unchanged and shares every untouched node.
    pub fn register_endorsed(
        &mut self,
        rel: RelId,
        lo: Tuple,
        hi: Tuple,
        endorse: Endorsement,
    ) -> InterestId {
        let id = InterestId(self.next);
        self.next += 1;
        self.enf = self.enf.insert((rel.0, lo, id.0), (hi, endorse));
        id
    }

    /// Drop a registered interest. `O(n)` only in the number of interests on the
    /// same relation span, since the id is the key's last column.
    pub fn unregister(&mut self, id: InterestId) {
        let key = self
            .enf
            .iter()
            .find(|(k, _)| k.2 == id.0)
            .map(|(k, _)| k.clone());
        if let Some(k) = key {
            self.enf = self.enf.remove(&k);
        }
    }

    /// Number of live interests.
    pub fn len(&self) -> usize {
        self.enf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.enf.is_empty()
    }

    /// The enfilade backing this canopy — a persistent measured tree, so it can
    /// be versioned and persisted with the rest of the plex.
    pub fn enfilade(&self) -> &CanopyEnf {
        &self.enf
    }

    /// The interests touched by `updates` — every interest whose `(rel, [lo, hi))`
    /// contains at least one updated tuple, deduplicated and sorted. A watcher
    /// not in the result is provably unaffected by this commit.
    pub fn route(&self, updates: &[(RelId, Tuple, Diff)]) -> Vec<InterestId> {
        self.route_endorsed(updates, Endorsement::NONE)
    }

    /// [`route`](Self::route) gated by the endorsement lattice: deliver only to
    /// interests whose endorsement **dominates** `required` (holds every required
    /// flag). `Endorsement::NONE` gates nothing, so `route` is this at the lattice
    /// bottom.
    pub fn route_endorsed(
        &self,
        updates: &[(RelId, Tuple, Diff)],
        required: Endorsement,
    ) -> Vec<InterestId> {
        let mut hit: Vec<InterestId> = Vec::new();
        for (rel, tuple, _diff) in updates {
            self.stab(&self.enf, rel.0, tuple, required, &mut hit);
        }
        hit.sort();
        hit.dedup();
        hit
    }

    /// Collect the interests of `rel` whose interval contains `point`, pruning on
    /// the upward measure: a subtree whose greatest `hi` does not clear the point
    /// holds nothing that can contain it, and a subtree whose joined endorsement
    /// lacks a required flag holds nothing that can receive it.
    fn stab(
        &self,
        t: &CanopyEnf,
        rel: u32,
        point: &Tuple,
        required: Endorsement,
        out: &mut Vec<InterestId>,
    ) {
        let node = match t.node() {
            None => return,
            Some(n) => n,
        };
        // WID prune: nothing under here reaches past the point…
        let m = t.measure();
        if m.max_hi.as_ref().is_none_or(|h| h <= point) {
            return;
        }
        // …and nothing under here carries the flags the requirement demands.
        if !m.endorse.dominates(required) {
            return;
        }
        match node {
            NodeRef::Leaf(entries) => {
                for ((r, lo, id), (hi, endorse)) in entries {
                    if *r == rel && lo <= point && point < hi && endorse.dominates(required) {
                        out.push(InterestId(*id));
                    }
                }
            }
            NodeRef::Internal(keys, children) => {
                for (i, child) in children.iter().enumerate() {
                    // Skip children whose whole span lies past this relation's
                    // interests, or whose low endpoints all exceed the point.
                    if i > 0 {
                        let (kr, klo, _) = &keys[i - 1];
                        if *kr > rel || (*kr == rel && klo > point) {
                            break;
                        }
                    }
                    if i < keys.len() {
                        let (kr, _, _) = &keys[i];
                        if *kr < rel {
                            continue;
                        }
                    }
                    self.stab(child, rel, point, required, out);
                }
            }
        }
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

    /// The canopy is a real enfilade: it versions like one (an older canopy is
    /// unaffected by a later registration) and it **persists** like one, through
    /// the same granfilade as the Fact and Edition enfilades.
    #[test]
    fn the_canopy_versions_and_persists_like_any_enfilade() {
        let mut c = Canopy::new();
        let a = c.register(RelId(1), t(0), t(10));
        let v0 = c.enfilade().clone();

        let b = c.register(RelId(1), t(5), t(20));
        // The retained older version is untouched by the later registration —
        // this is a persistent structure, not a mutable index.
        assert_eq!(v0.len(), 1, "an older canopy version mutated");
        assert_eq!(c.len(), 2);
        assert_eq!(c.route(&[(RelId(1), t(7), 1)]), vec![a, b]);

        // …and it round-trips through the granfilade.
        let dir = tempfile::tempdir().unwrap();
        let gran = crate::granfilade::Granfilade::open(dir.path()).unwrap();
        let ck = gran.persist(c.enfilade()).unwrap();
        let back: CanopyEnf = gran.load(ck).unwrap();
        let want: Vec<_> = c.enfilade().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let got: Vec<_> = back.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want, "the canopy did not survive the granfilade");
        assert_eq!(back.measure(), c.enfilade().measure(), "its upward measure did not");
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
