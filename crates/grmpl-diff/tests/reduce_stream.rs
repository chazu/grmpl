//! P2 acceptance (DESIGN.md §9, Snapshot–stream law) for `Reduce`:
//! `initial + Σ deltas = find(current)` under a randomized sequence of asserts
//! and retractions, for each aggregate (`Count`/`Sum`/`Min`/`Max`). Because
//! `Reduce` is non-linear and stateless in v1, its delta is a boundary
//! recompute; the churn drives group *creation*, *update*, and *emptying*
//! (a retraction removing a group's last member) — the cases that separate a
//! correct aggregate delta from a broken one. Every round is also checked
//! against an **independent model** that recomputes each aggregate from the set
//! of present base tuples directly, so the test is a genuine law oracle rather
//! than a self-consistency check.

use std::collections::{HashMap, HashSet};

use grmpl_core::{Diff, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{multiset, Agg, Query, Snapshot};
use grmpl_store::FjallStore;

const MEASURED: RelId = RelId(1); // (group: Ent, value: Int)

fn row(g: u64, v: i64) -> Tuple {
    Tuple::from([Value::Ent(Entity(g)), Value::Int(v)])
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

/// Independent model: recompute the expected `reduce(key=[group], agg)` result
/// directly from the present base tuples, as a tuple-sorted `(group, agg)` vec.
fn model(present: &HashSet<Tuple>, agg: Agg) -> Vec<(Tuple, Diff)> {
    let mut groups: HashMap<Value, Vec<i64>> = HashMap::new();
    for t in present {
        let g = t.as_slice()[0].clone();
        let v = match t.as_slice()[1] {
            Value::Int(n) => n,
            _ => unreachable!(),
        };
        groups.entry(g).or_default().push(v);
    }
    let mut out: Vec<(Tuple, Diff)> = groups
        .into_iter()
        .map(|(g, vs)| {
            let a = match agg {
                Agg::Count => Value::Int(vs.len() as i64),
                Agg::Sum(_) => Value::Int(vs.iter().sum()),
                Agg::Min(_) => Value::Int(*vs.iter().min().unwrap()),
                Agg::Max(_) => Value::Int(*vs.iter().max().unwrap()),
            };
            (Tuple::from([g, a]), 1)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn reduce_tracks_find_under_random_churn() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // One maintained stream per aggregate, all over the same base relation,
    // grouping by column 0 and folding column 1.
    let aggs = [
        ("count", Agg::Count),
        ("sum", Agg::Sum(1)),
        ("min", Agg::Min(1)),
        ("max", Agg::Max(1)),
    ];
    let queries: Vec<Query> =
        aggs.iter().map(|(_, a)| Query::rel(MEASURED).reduce([0], *a)).collect();

    let mut streams: Vec<_> = queries.iter().map(|q| q.watch(&store, store.current())).collect();
    let mut accs: Vec<multiset::Multiset> = vec![multiset::Multiset::new(); aggs.len()];
    for (i, s) in streams.iter_mut().enumerate() {
        for (t, d) in s.poll().unwrap() {
            multiset::add(&mut accs[i], t, d); // initial (empty world)
        }
    }

    // Track present base facts so we only assert absent ones / retract present
    // ones, keeping base weights in {0,1} (a realistic world).
    let mut present: HashSet<Tuple> = HashSet::new();
    let mut rng = Lcg(0x0d1a_5eed_face_b00c);

    for round in 0..300 {
        // A small overlapping domain: group 0..3 forces multi-member groups and
        // repeated values so Min/Max ties and Sum cancellation are exercised.
        let tuple = row(rng.below(3), rng.below(4) as i64);
        let diff: Diff = if present.contains(&tuple) {
            present.remove(&tuple);
            -1
        } else {
            present.insert(tuple.clone());
            1
        };
        store.commit(&[(MEASURED, tuple, diff)]).unwrap();

        for (i, (name, agg)) in aggs.iter().enumerate() {
            // Fold the maintained deltas in.
            for (t, d) in streams[i].poll().unwrap() {
                multiset::add(&mut accs[i], t, d);
            }
            multiset::strip_zeros(&mut accs[i]);

            // Law 1: maintained result equals a fresh evaluation at current.
            let want = queries[i].find(&Snapshot::at_current(&store)).unwrap();
            let got = multiset::to_sorted_vec(&accs[i]);
            assert_eq!(got, want, "[{name}] snapshot-stream law violated at round {round}");

            // Law 2: that evaluation matches the independent model.
            let expected = model(&present, *agg);
            assert_eq!(want, expected, "[{name}] find disagrees with model at round {round}");
        }
    }
}

#[test]
fn reduce_inside_iterate_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // An `Iterate` whose step contains a `Reduce` is an ill-formed plan.
    let step = Query::recur().reduce([0], Agg::Count);
    let q = Query::iterate(Query::rel(MEASURED), step);

    let err = q.find(&Snapshot::at_current(&store)).unwrap_err();
    assert!(matches!(err, grmpl_core::Error::Query(_)), "expected Error::Query, got {err:?}");
}
