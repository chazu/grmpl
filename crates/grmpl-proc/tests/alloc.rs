//! `Alloc` — the replay-safe entity-id counter (P3).

use grmpl_core::{EditionStore, Entity, NoSchemas, Patch, RelId, Scope, TraceStore, Tuple, Value};
use grmpl_core::{Authority, DomainId, Fact};
use grmpl_proc::{commit_patch, Alloc, CommitOutcome};
use grmpl_store::FjallStore;

const SEQ: RelId = RelId(30);
const THING: RelId = RelId(1); // (id, tag) — something to name with allocated ids

fn store() -> (FjallStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    (store, dir)
}

fn auth() -> Authority {
    Authority::new(DomainId(1), vec![Scope::whole(SEQ), Scope::whole(THING)])
}

#[test]
fn allocates_distinct_ids_and_persists_the_counter() {
    let (store, _dir) = store();

    // First allocation from an absent counter starts at the base.
    let mut a = Alloc::read(&store, SEQ, 1000).unwrap();
    let x = a.fresh();
    let y = a.fresh();
    assert_eq!((x, y), (Entity(1000), Entity(1001)));
    assert_eq!(a.allocated(), 2);

    let patch = a.seal(
        Patch::new()
            .assert(Fact::new(THING, Tuple::from([Value::Ent(x), Value::text("x")])))
            .assert(Fact::new(THING, Tuple::from([Value::Ent(y), Value::text("y")]))),
    );
    assert!(matches!(
        commit_patch(&store, &NoSchemas, &patch, &auth()).unwrap(),
        CommitOutcome::Committed(_)
    ));

    // The counter now reads 1002, held with weight exactly 1 (no leftover rows).
    let at = store.current();
    let counter: Vec<_> =
        store.read_at(SEQ, at).unwrap().into_iter().filter(|(_, d)| *d != 0).collect();
    assert_eq!(counter, vec![(Tuple::from([Value::Int(1002)]), 1)]);

    // A fresh allocator picks up where the last left off — no id reuse.
    let mut b = Alloc::read(&store, SEQ, 1000).unwrap();
    assert_eq!(b.fresh(), Entity(1002));
}

#[test]
fn seal_is_a_noop_when_nothing_was_allocated() {
    let (store, _dir) = store();
    let a = Alloc::read(&store, SEQ, 1000).unwrap();
    let patch = a.seal(Patch::new());
    assert_eq!(patch, Patch::new(), "no allocation -> no counter write");
}

#[test]
fn allocation_is_replay_safe_from_a_fixed_edition() {
    // Re-running from the same committed edition reproduces the same ids: the id
    // is a pure function of the counter it reads, nothing external.
    let (store, _dir) = store();
    store.commit(&[(SEQ, Tuple::from([Value::Int(1000)]), 1)]).unwrap();
    let pinned = store.current();

    let ids_a: Vec<Entity> = {
        let mut a = Alloc::read(&store, SEQ, 1000).unwrap();
        vec![a.fresh(), a.fresh(), a.fresh()]
    };
    // The store is unchanged (we never committed), so a replay reads the same
    // counter and hands out the identical ids.
    assert_eq!(store.current(), pinned);
    let ids_b: Vec<Entity> = {
        let mut b = Alloc::read(&store, SEQ, 1000).unwrap();
        vec![b.fresh(), b.fresh(), b.fresh()]
    };
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a, vec![Entity(1000), Entity(1001), Entity(1002)]);
}
