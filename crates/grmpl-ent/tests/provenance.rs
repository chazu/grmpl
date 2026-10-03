//! **The spanfilade at the store: every instance knows its template.**
//!
//! `instance_template` is a graft, and a graft points one way. The branch's
//! spanfilade records each one twice — by source block and by target block —
//! so the store answers both of Green's questions: where was this template
//! copied to, and where did this instance come from. Pinned here:
//!
//! 1. **Both directions**, and a chain of copies followed back to its origin.
//! 2. **It is part of the root**: it survives a reopen.
//! 3. **It is part of a branch**: a fork into the past keeps only the grafts
//!    made by then, and the two branches then diverge.
//! 4. **It is history**: retracting an instance's facts does not erase the
//!    record that the copy was made.

use grmpl_core::{Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;

const ROOMS: RelId = RelId(1);
const EXITS: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

/// A three-room template in `[1000, 1010)`.
fn seed(store: &EntStore) {
    store
        .commit(&[
            (ROOMS, Tuple::from([ent(1_000), Value::text("hall")]), 1),
            (ROOMS, Tuple::from([ent(1_001), Value::text("vault")]), 1),
            (ROOMS, Tuple::from([ent(1_002), Value::text("stair")]), 1),
            (EXITS, Tuple::from([ent(1_000), ent(1_001)]), 1),
            (EXITS, Tuple::from([ent(1_001), ent(1_002)]), 1),
        ])
        .unwrap();
}

fn targets(store: &EntStore, lo: u64, hi: u64, at: Edition) -> Vec<u64> {
    store.copies_of(lo, hi, at).iter().map(|g| g.target.0).collect()
}

#[test]
fn a_template_knows_its_instances_and_an_instance_its_template() {
    let store = EntStore::new();
    seed(&store);
    let first = store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 1_000).unwrap();
    let second = store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 2_000).unwrap();
    // A copy of the first instance, not of the template.
    let third = store.instance_template(&[ROOMS, EXITS], 2_000, 2_010, 7_000).unwrap();
    let now = store.current();

    assert_eq!(targets(&store, 1_000, 1_010, now), vec![2_000, 3_000]);
    assert_eq!(targets(&store, 2_000, 2_010, now), vec![9_000]);
    let back = store.sources_of(9_000, 9_010, now);
    assert_eq!(back.len(), 1);
    assert_eq!((back[0].source, back[0].shift(), back[0].edition), ((2_000, 2_010), 7_000, third));
    assert_eq!(back[0].rels, vec![ROOMS, EXITS]);

    // The vault in the copy of the copy started as the template's vault.
    let (origin, chain) = store.origin_of(Entity(9_001), now);
    assert_eq!(origin, Entity(1_001));
    assert_eq!(chain.iter().map(|g| g.edition).collect::<Vec<_>>(), vec![third, first]);
    let (origin, chain) = store.origin_of(Entity(3_002), now);
    assert_eq!((origin, chain[0].edition), (Entity(1_002), second));
    // As of the first instancing, the second had not happened.
    assert_eq!(targets(&store, 1_000, 1_010, first), vec![2_000]);
    assert_eq!(store.origin_of(Entity(3_002), first), (Entity(3_002), vec![]));
}

#[test]
fn provenance_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let at = {
        let store = EntStore::open(dir.path()).unwrap();
        seed(&store);
        store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 5_000).unwrap()
    };
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(targets(&store, 1_000, 1_010, at), vec![6_000]);
    assert_eq!(store.origin_of(Entity(6_002), at).0, Entity(1_002));
}

#[test]
fn a_fork_into_the_past_keeps_only_the_grafts_made_by_then() {
    let store = EntStore::new();
    seed(&store);
    let first = store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 1_000).unwrap();
    store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 2_000).unwrap();

    let fork = store.fork_at(first).unwrap();
    assert_eq!(targets(&fork, 1_000, 1_010, fork.current()), vec![2_000]);
    // The fork instances into a block of its own at the edition the parent
    // used for its second instance. Each branch sees only its own.
    let mine = fork.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 4_000).unwrap();
    assert_eq!(mine, store.current(), "the two grafts share an edition number");
    assert_eq!(targets(&fork, 1_000, 1_010, mine), vec![2_000, 5_000]);
    assert_eq!(targets(&store, 1_000, 1_010, store.current()), vec![2_000, 3_000]);
}

#[test]
fn retracting_an_instance_does_not_erase_that_it_was_made() {
    let store = EntStore::new();
    seed(&store);
    let made = store.instance_template(&[ROOMS, EXITS], 1_000, 1_010, 1_000).unwrap();
    let rows: Vec<_> = [ROOMS, EXITS]
        .into_iter()
        .flat_map(|rel| {
            store.read_at(rel, made).unwrap().into_iter().filter_map(move |(t, n)| {
                matches!(t.as_slice()[0], Value::Ent(e) if e.0 >= 2_000).then_some((rel, t, -n))
            })
        })
        .collect();
    store.commit(&rows).unwrap();
    assert!(store.search_at(ROOMS, store.current(), &[(0, 2_000, 2_010)]).unwrap().is_empty());
    assert_eq!(targets(&store, 1_000, 1_010, store.current()), vec![2_000]);
}
