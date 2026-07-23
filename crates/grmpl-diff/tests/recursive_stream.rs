//! M3 acceptance (DESIGN.md §9): recursive Snapshot–stream **including deletion**.
//!
//! `implements` is the transitive closure of behaviour over the prototype chain:
//!
//! ```text
//! implements(e,b) :- direct(e,b)
//! implements(e,b) :- prototype(e,p), implements(p,b)
//! ```
//!
//! The deletion case — retracting a prototype edge and watching the derived
//! `implements` facts disappear — is what separates a correct recursive delta
//! from a broken one.

use std::collections::HashSet;

use grmpl_core::{Diff, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{multiset, DeltaStream, Multiset, Query, Snapshot};
use grmpl_store::FjallStore;

const DIRECT: RelId = RelId(3); //    (entity, behavior)
const PROTOTYPE: RelId = RelId(4); // (child, parent)

fn direct(e: u64, b: &str) -> (RelId, Tuple, Diff) {
    (DIRECT, Tuple::from([Value::Ent(Entity(e)), Value::text(b)]), 1)
}
fn proto(child: u64, parent: u64) -> (RelId, Tuple, Diff) {
    (PROTOTYPE, Tuple::from([Value::Ent(Entity(child)), Value::Ent(Entity(parent))]), 1)
}
fn neg(u: (RelId, Tuple, Diff)) -> (RelId, Tuple, Diff) {
    (u.0, u.1, -u.2)
}
fn imp(e: u64, b: &str) -> (Tuple, Diff) {
    (Tuple::from([Value::Ent(Entity(e)), Value::text(b)]), 1)
}

/// `implements` = fixpoint of `distinct(direct ∪ (prototype ⋈ recur))`.
/// join layout: [0]child [1]parent [2]entity(=parent) [3]behavior → project (child, behavior).
fn implements_query() -> Query {
    let step = Query::rel(PROTOTYPE)
        .join(Query::recur(), [1], [0])
        .project([0, 3]);
    Query::iterate(Query::rel(DIRECT), step)
}

fn commit_and_check(
    store: &FjallStore,
    q: &Query,
    stream: &mut DeltaStream<'_>,
    acc: &mut Multiset,
    updates: &[(RelId, Tuple, Diff)],
) {
    store.commit(updates).unwrap();
    for (t, d) in stream.poll().unwrap() {
        multiset::add(acc, t, d);
    }
    multiset::strip_zeros(acc);
    let want = q.find(&Snapshot::at_current(store)).unwrap();
    assert_eq!(multiset::to_sorted_vec(acc), want, "recursive snapshot-stream law violated");
}

#[test]
fn implements_transitive_closure_with_edge_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let q = implements_query();

    let mut stream = q.watch(&store, store.current());
    let mut acc = Multiset::new();
    for (t, d) in stream.poll().unwrap() {
        multiset::add(&mut acc, t, d); // initial (empty)
    }

    // Build chain 1 -> 2 -> 3 with a behavior at the root.
    commit_and_check(&store, &q, &mut stream, &mut acc, &[direct(3, "fly")]);
    commit_and_check(&store, &q, &mut stream, &mut acc, &[proto(2, 3)]);
    commit_and_check(&store, &q, &mut stream, &mut acc, &[proto(1, 2)]);

    // Now all three entities implement "fly" (1 and 2 by delegation).
    let mut want = vec![imp(1, "fly"), imp(2, "fly"), imp(3, "fly")];
    want.sort();
    assert_eq!(q.find(&Snapshot::at_current(&store)).unwrap(), want);

    // Delete the 2 -> 3 edge: 1 and 2 can no longer reach the behavior.
    commit_and_check(&store, &q, &mut stream, &mut acc, &[neg(proto(2, 3))]);

    // Only the root retains "fly"; the two derived facts were retracted.
    assert_eq!(
        q.find(&Snapshot::at_current(&store)).unwrap(),
        vec![imp(3, "fly")]
    );
    // And the maintained stream agrees (law already asserted inside the helper).
    assert_eq!(multiset::to_sorted_vec(&acc), vec![imp(3, "fly")]);
}

#[test]
fn recursive_watch_tracks_find_under_churn() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let q = implements_query();

    let mut stream = q.watch(&store, store.current());
    let mut acc = Multiset::new();
    for (t, d) in stream.poll().unwrap() {
        multiset::add(&mut acc, t, d);
    }

    let mut protos: HashSet<Tuple> = HashSet::new();
    let mut directs: HashSet<Tuple> = HashSet::new();

    // xorshift PRNG, dependency-free.
    let mut s: u64 = 0x9e3779b97f4a7c15;
    let mut rng = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };

    for _round in 0..200 {
        if rng() % 2 == 0 {
            // toggle a prototype edge among entities 0..5 (cycles allowed).
            let c = rng() % 5;
            let p = rng() % 5;
            let t = Tuple::from([Value::Ent(Entity(c)), Value::Ent(Entity(p))]);
            let d = if protos.remove(&t) { -1 } else { protos.insert(t.clone()); 1 };
            store.commit(&[(PROTOTYPE, t, d)]).unwrap();
        } else {
            // toggle a direct behavior.
            let e = rng() % 5;
            let b = rng() % 2;
            let t = Tuple::from([Value::Ent(Entity(e)), Value::text(if b == 0 { "fly" } else { "swim" })]);
            let d = if directs.remove(&t) { -1 } else { directs.insert(t.clone()); 1 };
            store.commit(&[(DIRECT, t, d)]).unwrap();
        }

        for (t, d) in stream.poll().unwrap() {
            multiset::add(&mut acc, t, d);
        }
        multiset::strip_zeros(&mut acc);

        let want = q.find(&Snapshot::at_current(&store)).unwrap();
        assert_eq!(multiset::to_sorted_vec(&acc), want, "law violated at round {_round}");
    }
}
