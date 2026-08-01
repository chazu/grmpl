//! M2 acceptance (DESIGN.md §9, Snapshot–stream law):
//! `initial + Σ deltas = find(current)` under a randomized sequence of asserts
//! and retractions, for a query that joins, projects, and de-duplicates. The
//! join's deletion path (a retraction destroying a match) is the case that
//! separates a correct differential join from a broken one.

use std::collections::HashSet;

use grmpl_core::{Diff, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{multiset, Query, Snapshot};
use grmpl_ent::EntStore;

const LOCATED: RelId = RelId(1); // (thing, place)
const NAMED: RelId = RelId(2); //  (thing, name)

fn ent(t: u64, p: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(t)), Value::Ent(Entity(p))])
}

/// A small deterministic PRNG so the test is reproducible without a dep.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn watch_tracks_find_under_random_churn() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    // q = distinct( located ⋈_thing named  →  project (place, name) )
    // concat layout after join: [0]=thing_l [1]=place [2]=thing_r [3]=name
    let q = Query::rel(LOCATED)
        .join(Query::rel(NAMED), [0], [0])
        .project([1, 3])
        .distinct();

    let mut stream = q.watch(&store, store.current());
    let mut acc = multiset::Multiset::new();
    for (t, d) in stream.poll().unwrap() {
        multiset::add(&mut acc, t, d); // initial (empty world)
    }

    // Track present facts so we only assert absent ones / retract present ones,
    // keeping base weights in {0,1} (a realistic world).
    let mut located: HashSet<Tuple> = HashSet::new();
    let mut named: HashSet<Tuple> = HashSet::new();
    let mut rng = Lcg(0x1234_5678_9abc_def0);

    for _round in 0..250 {
        // Choose a relation and a random tuple from a small overlapping domain.
        let use_located = rng.below(2) == 0;
        let (rel, set, tuple) = if use_located {
            let t = ent(rng.below(4), rng.below(3)); // thing 0..4, place 0..3
            (LOCATED, &mut located, t)
        } else {
            let t = ent(rng.below(4), rng.below(3)); // thing 0..4, name 0..3
            (NAMED, &mut named, t)
        };

        let diff: Diff = if set.contains(&tuple) {
            set.remove(&tuple);
            -1
        } else {
            set.insert(tuple.clone());
            1
        };
        store.commit(&[(rel, tuple, diff)]).unwrap();

        // Fold the maintained deltas in.
        for (t, d) in stream.poll().unwrap() {
            multiset::add(&mut acc, t, d);
        }
        multiset::strip_zeros(&mut acc);

        // The law: maintained result equals a fresh evaluation at current.
        let want = q.find(&Snapshot::at_current(&store)).unwrap();
        let got = multiset::to_sorted_vec(&acc);
        assert_eq!(got, want, "snapshot-stream law violated at round {_round}");
    }
}
