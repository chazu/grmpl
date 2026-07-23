//! M1 acceptance (DESIGN.md §9, Patch–edition law): `commit` creates exactly
//! one new edition or has no effect.
//!
//! True crash-mid-batch atomicity is delegated to fjall's atomic `batch()` +
//! journal, on which the store deliberately relies (DESIGN.md §4.1). Our layer
//! adds the ordering guarantee that the in-memory edition clock advances *only
//! after* the durable batch commits — so a failed commit leaves no edition and
//! no data. These tests exercise the success path (full effect, one edition,
//! durable) and the no-early-visibility property.

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn fact(a: u64, b: u64) -> (RelId, Tuple, Diff) {
    (R, Tuple::from([Value::Ent(Entity(a)), Value::Ent(Entity(b))]), 1)
}

#[test]
fn one_commit_is_exactly_one_edition_regardless_of_size() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // A commit of five facts advances the clock by exactly one.
    let e = store
        .commit(&[fact(1, 0), fact(2, 0), fact(3, 0), fact(4, 0), fact(5, 0)])
        .unwrap();
    assert_eq!(e, Edition(1));
    assert_eq!(store.current(), Edition(1));

    // All five are visible together at that single edition (no partial edition).
    assert_eq!(store.read_at(R, e).unwrap().len(), 5);

    // The next commit is edition 2, again a single step.
    let e2 = store.commit(&[fact(6, 0)]).unwrap();
    assert_eq!(e2, Edition(2));
}

#[test]
fn no_early_visibility_of_a_later_edition() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    let e1 = store.commit(&[fact(1, 0)]).unwrap();
    let _e2 = store.commit(&[fact(2, 0)]).unwrap();

    // Reading as-of e1 must not see e2's fact (the as-of cut is exact).
    let at_e1 = store.read_at(R, e1).unwrap();
    assert_eq!(at_e1.len(), 1);
    assert_eq!(at_e1[0].0, Tuple::from([Value::Ent(Entity(1)), Value::Ent(Entity(0))]));
}

#[test]
fn multi_update_commit_is_all_or_nothing_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FjallStore::open(dir.path()).unwrap();
        // One commit mixing asserts and a retraction across the same relation.
        store
            .commit(&[fact(1, 0), fact(2, 0), (R, Tuple::from([Value::Ent(Entity(1)), Value::Ent(Entity(0))]), -1)])
            .unwrap();
    }
    // After reopen the whole commit is present: fact(1) asserted then retracted
    // (net zero, absent), fact(2) present. Never a partial subset.
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), Edition(1));
    let rows = store.read_at(R, Edition(1)).unwrap();
    assert_eq!(
        rows,
        vec![(Tuple::from([Value::Ent(Entity(2)), Value::Ent(Entity(0))]), 1)]
    );
}
