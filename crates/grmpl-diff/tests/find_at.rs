//! `find q at E` as-of, paired with the P1 schema-at-edition surface (TKT-78,
//! P6). A [`Snapshot`] pinned at an arbitrary edition evaluates a query as it
//! stood *then* (already true for an append-only log), and now also reports the
//! column schema *in force then* via [`Snapshot::schema`] — so an as-of read
//! sees the typing of its own era, not today's. Both are subject to the P6
//! consolidation watermark: a snapshot below the horizon cannot be read.

use grmpl_core::{
    Column, Edition, Entity, RelId, Schema, SchemaCatalog, TraceStore, Tuple, Ty, Value,
};
use grmpl_diff::{Query, Snapshot};
use grmpl_ent::EntStore;

const R: RelId = RelId(1);

fn row(a: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a))])
}

#[test]
fn find_and_schema_are_both_as_of_the_pinned_edition() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    // A one-column schema at edition 1, additively evolved to two columns later.
    let v1 = Schema::new(vec![Column::new("a", Ty::Ent)]);
    let v2 = Schema::new(vec![Column::new("a", Ty::Ent), Column::new("b", Ty::Int)]);

    store.commit(&[(R, row(1), 1)]).unwrap(); // edition 1
    store.put_schema(R, &v1, Edition(1)).unwrap();
    store.commit(&[(R, row(2), 1)]).unwrap(); // edition 2
    store.commit(&[(R, row(3), 1)]).unwrap(); // edition 3
    store.put_schema(R, &v2, Edition(3)).unwrap();

    let q = Query::rel(R);

    // `find q at E`: the world *as it stood* at each edition.
    assert_eq!(q.find(&Snapshot::new(&store, Edition(1))).unwrap(), vec![(row(1), 1)]);
    assert_eq!(
        q.find(&Snapshot::new(&store, Edition(3))).unwrap(),
        vec![(row(1), 1), (row(2), 1), (row(3), 1)]
    );

    // schema-at-edition rides the same pinned edition: v1 before edition 3, v2 at/after.
    let s1 = Snapshot::new(&store, Edition(1));
    let s2 = Snapshot::new(&store, Edition(2));
    let s3 = Snapshot::new(&store, Edition(3));
    assert_eq!(s1.schema(&store, R).unwrap(), Some(v1.clone()));
    assert_eq!(s2.schema(&store, R).unwrap(), Some(v1), "v2 not yet in force at edition 2");
    assert_eq!(s3.schema(&store, R).unwrap(), Some(v2));
    // A relation with no registered schema reads as untyped (None), as-of anything.
    assert_eq!(s3.schema(&store, RelId(999)).unwrap(), None);
}

#[test]
fn as_of_find_survives_consolidation_and_errors_below_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    store.commit(&[(R, row(1), 1)]).unwrap(); // e1
    store.commit(&[(R, row(2), 1)]).unwrap(); // e2
    store.commit(&[(R, row(1), -1)]).unwrap(); // e3: net {2}
    store.commit(&[(R, row(3), 1)]).unwrap(); // e4: net {2,3}

    // Collapse history through edition 2.
    assert_eq!(store.consolidate(Edition(2)).unwrap(), Edition(2));

    let q = Query::rel(R);
    // `find q at E` at/above the watermark still answers from checkpoint + tail.
    assert_eq!(
        q.find(&Snapshot::new(&store, Edition(2))).unwrap(),
        vec![(row(1), 1), (row(2), 1)]
    );
    assert_eq!(
        q.find(&Snapshot::new(&store, Edition(4))).unwrap(),
        vec![(row(2), 1), (row(3), 1)]
    );
    // Below the watermark the intermediate state is gone — the find errors at the door.
    assert!(q.find(&Snapshot::new(&store, Edition(1))).is_err());
}
