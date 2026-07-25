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
//! This is the interest registry + a correct linear stabbing query. A measured
//! interval enfilade for `O(log + k)` routing (and Gold's endorsement flag-lattice
//! for authority) is the faithful refinement; correctness comes first.

use grmpl_core::{Diff, RelId, Tuple};

/// A handle to a registered interest.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct InterestId(pub u64);

/// One standing interest: rows of `rel` whose key lies in `[lo, hi)`.
struct Interest {
    id: InterestId,
    rel: RelId,
    lo: Tuple,
    hi: Tuple,
}

/// The canopy: a set of standing interests over the world.
#[derive(Default)]
pub struct Canopy {
    next: u64,
    interests: Vec<Interest>,
}

impl Canopy {
    pub fn new() -> Canopy {
        Canopy::default()
    }

    /// Register interest in the rows of `rel` in `[lo, hi)`. Returns a handle.
    pub fn register(&mut self, rel: RelId, lo: Tuple, hi: Tuple) -> InterestId {
        let id = InterestId(self.next);
        self.next += 1;
        self.interests.push(Interest { id, rel, lo, hi });
        id
    }

    /// Drop a registered interest.
    pub fn unregister(&mut self, id: InterestId) {
        self.interests.retain(|i| i.id != id);
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
    /// not in the result is provably unaffected by this commit.
    pub fn route(&self, updates: &[(RelId, Tuple, Diff)]) -> Vec<InterestId> {
        let mut hit: Vec<InterestId> = self
            .interests
            .iter()
            .filter(|i| {
                updates
                    .iter()
                    .any(|(rel, tuple, _diff)| *rel == i.rel && i.lo <= *tuple && *tuple < i.hi)
            })
            .map(|i| i.id)
            .collect();
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
}
