//! Forks v1 (P10): copy the whole domain, bit-identical, then evolve
//! independently. Cost is O(domain) — the physical store, not O(history).
//!
//! Fork identity / clock semantics are deferred to P15; v1 is the flat copy.

use grmpl_core::{
    Catalog, Column, Edition, EditionStore, Entity, RelId, Schema, SchemaCatalog, TraceStore,
    Tuple, Ty, Value,
};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn ent(a: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a))])
}

fn thing_schema() -> Schema {
    Schema::new(vec![Column::new("thing", Ty::Ent)])
}

/// A store with a few editions of churn, a catalog binding and a schema.
fn seeded() -> (FjallStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(R, ent(1), 1), (R, ent(2), 1)]).unwrap(); // e1
    store.commit(&[(R, ent(3), 1), (R, ent(1), -1)]).unwrap(); // e2 → {2,3}
    store.register("things", R).unwrap();
    store.put_schema(R, &thing_schema(), Edition(1)).unwrap();
    (store, dir)
}

#[test]
fn fork_is_a_bit_identical_copy() {
    let (store, _dir) = seeded();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();

    // The whole physical store copied byte for byte.
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "a fork is bit-identical to its source at the moment of the copy"
    );

    // …and every observable surface agrees: clock, watermark, data, catalog,
    // schema.
    assert_eq!(fork.current(), store.current());
    assert_eq!(TraceStore::watermark(&fork), TraceStore::watermark(&store));
    assert_eq!(
        fork.read_at(R, Edition(2)).unwrap(),
        vec![(ent(2), 1), (ent(3), 1)]
    );
    assert_eq!(
        fork.read_at(R, Edition(1)).unwrap(),
        store.read_at(R, Edition(1)).unwrap()
    );
    assert_eq!(Catalog::rel_id(&fork, "things").unwrap(), Some(R));
    assert_eq!(
        SchemaCatalog::schema(&fork, R).unwrap(),
        Some(thing_schema())
    );
}

#[test]
fn fork_evolves_independently_then_replay_reconverges() {
    let (store, _dir) = seeded();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();
    let base_edition = store.current();

    // Commit only to the source: the fork does not move.
    store.commit(&[(R, ent(9), 1)]).unwrap();
    assert_eq!(fork.current(), base_edition, "the fork's clock is its own");
    assert_ne!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "independent evolution diverges the two stores"
    );

    // Replaying the *same* commit onto the fork reconverges it bit-identically —
    // the checkpoint is a fork point, and identical inputs reproduce the world.
    fork.commit(&[(R, ent(9), 1)]).unwrap();
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "identical commits on a fork reproduce the source bit for bit"
    );
    assert_eq!(fork.current(), store.current());
}

#[test]
fn fork_does_not_disturb_the_source() {
    let (store, _dir) = seeded();
    let before: Vec<(String, Vec<u8>, Vec<u8>)> = store.canonical_dump().unwrap();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();

    // A write to the fork must not touch the source.
    fork.commit(&[(R, ent(42), 1)]).unwrap();
    assert_eq!(
        store.canonical_dump().unwrap(),
        before,
        "forking then writing the fork leaves the source untouched"
    );
}
