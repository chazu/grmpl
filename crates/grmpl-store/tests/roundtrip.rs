//! M0 acceptance (DESIGN.md §9): a relation round-trips through the fjall store
//! — committed tuples read back at their edition, retractions cancel, and the
//! edition clock survives a reopen.

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_store::FjallStore;

const LOCATED: RelId = RelId(1);

fn located(thing: u64, place: u64) -> (RelId, Tuple, Diff) {
    (
        LOCATED,
        Tuple::from([Value::Ent(Entity(thing)), Value::Ent(Entity(place))]),
        1,
    )
}

fn sorted(mut v: Vec<(Tuple, Diff)>) -> Vec<(Tuple, Diff)> {
    v.sort();
    v
}

#[test]
fn roundtrip_relation_through_fjall() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // Empty world.
    assert_eq!(store.current(), Edition::ZERO);
    assert!(store.read_at(LOCATED, Edition::ZERO).unwrap().is_empty());

    // Commit two facts.
    let e1 = store.commit(&[located(10, 7), located(11, 7)]).unwrap();
    assert_eq!(e1, Edition(1));

    let got = sorted(store.read_at(LOCATED, e1).unwrap());
    assert_eq!(
        got,
        sorted(vec![
            (Tuple::from([Value::Ent(Entity(10)), Value::Ent(Entity(7))]), 1),
            (Tuple::from([Value::Ent(Entity(11)), Value::Ent(Entity(7))]), 1),
        ])
    );

    // A later edition retracts one fact; the sum cancels to zero and it vanishes.
    let e2 = store
        .commit(&[(
            LOCATED,
            Tuple::from([Value::Ent(Entity(10)), Value::Ent(Entity(7))]),
            -1,
        )])
        .unwrap();
    assert_eq!(e2, Edition(2));

    let got = store.read_at(LOCATED, e2).unwrap();
    assert_eq!(
        got,
        vec![(Tuple::from([Value::Ent(Entity(11)), Value::Ent(Entity(7))]), 1)]
    );

    // As-of semantics: reading at the earlier edition still shows both facts.
    let got_e1 = sorted(store.read_at(LOCATED, e1).unwrap());
    assert_eq!(got_e1.len(), 2);
}

#[test]
fn edition_clock_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FjallStore::open(dir.path()).unwrap();
        store.commit(&[located(1, 2)]).unwrap();
        store.commit(&[located(3, 4)]).unwrap();
        assert_eq!(store.current(), Edition(2));
    }
    // Reopen: the edition clock and data persist.
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), Edition(2));
    assert_eq!(store.read_at(LOCATED, Edition(2)).unwrap().len(), 2);
}
