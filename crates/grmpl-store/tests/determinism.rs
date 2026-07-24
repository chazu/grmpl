//! Determinism guarantees (TKT-72): `read_at` returns tuple-sorted rows and
//! `scan_updates` returns updates in commit order `(edition, counter)`, not the
//! store's physical scan order.

use grmpl_core::{Diff, Edition, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn ent(a: u64, b: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a)), Value::Ent(Entity(b))])
}

#[test]
fn read_at_is_tuple_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    // Commit in a deliberately unsorted order within one edition.
    store
        .commit(&[
            (R, ent(9, 0), 1),
            (R, ent(1, 0), 1),
            (R, ent(5, 0), 1),
            (R, ent(3, 0), 1),
        ])
        .unwrap();

    let rows: Vec<Tuple> = store
        .read_at(R, Edition(1))
        .unwrap()
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let mut want = rows.clone();
    want.sort();
    assert_eq!(rows, want, "read_at must be tuple-sorted");
    assert_eq!(rows, vec![ent(1, 0), ent(3, 0), ent(5, 0), ent(9, 0)]);
}

#[test]
fn scan_updates_is_in_commit_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    // Two same-edition updates whose commit (counter) order is the *reverse* of
    // their tuple order — the sort must follow the counter, not the tuple.
    store.commit(&[(R, ent(9, 0), 1), (R, ent(1, 0), 1)]).unwrap();
    store.commit(&[(R, ent(5, 0), 1)]).unwrap();

    let updates = store.scan_updates(R, Edition::ZERO, Edition(2)).unwrap();
    let seen: Vec<(u64, Diff, Tuple)> = updates
        .into_iter()
        .map(|u| (u.time.edition, u.diff, u.tuple))
        .collect();
    assert_eq!(
        seen,
        vec![
            (1, 1, ent(9, 0)), // edition 1, counter 0
            (1, 1, ent(1, 0)), // edition 1, counter 1
            (2, 1, ent(5, 0)), // edition 2, counter 0
        ]
    );
}
