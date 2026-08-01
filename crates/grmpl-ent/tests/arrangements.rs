//! **G-9 acceptance: Arrangements — pruning on a trailing column.**
//!
//! The lead-column WID pushdown (E2b) makes an entity-keyed view prune to its
//! key at the source. A predicate on any *other* column had no such path: the
//! trace's primary order says nothing about it, so the only correct answer was
//! read-the-relation-and-filter. Plan v4 listed multi-order Arrangements as the
//! remaining gap, and v5 kept it: "the same primitive replicated per order".
//!
//! An Arrangement is one more measured tree over the same facts, rotated so the
//! column of interest leads — after which pruning on it is the ordinary
//! lead-column range walk. It is built on first use for a column and maintained
//! by every later commit, so a query that never asks about a column never pays
//! for one.
//!
//! The law is exactness: for every column and every span, the Arrangement must
//! return precisely what a full scan-and-filter would.

use grmpl_core::{Diff, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;

const REL: RelId = RelId(1);

fn row(a: i64, b: i64, c: &str) -> Tuple {
    Tuple::from([Value::Int(a), Value::Int(b), Value::text(c)])
}

/// Scan-and-filter — what the default substrate implementation does, and the
/// ground truth an Arrangement must reproduce.
fn by_scan(store: &EntStore, col: usize, lo: &Value, hi: &Value) -> Vec<(Tuple, Diff)> {
    store
        .read_at(REL, store.current())
        .unwrap()
        .into_iter()
        .filter(|(t, _)| t.as_slice().get(col).is_some_and(|v| lo <= v && v < hi))
        .collect()
}

fn sorted(mut v: Vec<(Tuple, Diff)>) -> Vec<(Tuple, Diff)> {
    v.sort();
    v
}

/// Every column, every span: the Arrangement agrees with the scan exactly.
#[test]
fn a_trailing_column_range_matches_a_full_scan() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
    for a in 0..12i64 {
        for b in 0..6i64 {
            updates.push((REL, row(a, b, if b % 2 == 0 { "even" } else { "odd" }), 1));
        }
    }
    store.commit(&updates).unwrap();
    let at = store.current();

    // Column 1 (an Int) over many spans, including empty and out-of-range ones.
    for lo in [-1i64, 0, 2, 5, 6, 99] {
        for hi in [-1i64, 0, 3, 6, 100] {
            let (l, h) = (Value::Int(lo), Value::Int(hi));
            assert_eq!(
                sorted(store.read_range_on(REL, at, 1, &l, &h).unwrap()),
                sorted(by_scan(&store, 1, &l, &h)),
                "column 1 over [{lo},{hi})"
            );
        }
    }
    // Column 2 (Text) — the rotation is column-type agnostic.
    for (lo, hi) in [("even", "odd"), ("a", "z"), ("odd", "odd"), ("z", "zz")] {
        let (l, h) = (Value::text(lo), Value::text(hi));
        assert_eq!(
            sorted(store.read_range_on(REL, at, 2, &l, &h).unwrap()),
            sorted(by_scan(&store, 2, &l, &h)),
            "column 2 over [{lo},{hi})"
        );
    }
    // Column 0 is the primary order; it must agree too.
    let (l, h) = (Value::Int(3), Value::Int(7));
    assert_eq!(
        sorted(store.read_range_on(REL, at, 0, &l, &h).unwrap()),
        sorted(by_scan(&store, 0, &l, &h)),
        "column 0"
    );
}

/// An Arrangement stays correct as the world moves: later commits — asserts and
/// retractions alike — must be reflected in it, not just in the primary order.
#[test]
fn an_arrangement_is_maintained_by_later_commits() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    store
        .commit(&[(REL, row(1, 5, "x"), 1), (REL, row(2, 5, "y"), 1), (REL, row(3, 9, "z"), 1)])
        .unwrap();

    let span = (Value::Int(5), Value::Int(6));
    // First use builds the Arrangement for column 1.
    let at = store.current();
    assert_eq!(store.read_range_on(REL, at, 1, &span.0, &span.1).unwrap().len(), 2);

    // Add a row in the span, and retract one that was in it.
    store.commit(&[(REL, row(4, 5, "w"), 1), (REL, row(1, 5, "x"), -1)]).unwrap();
    let at = store.current();
    assert_eq!(
        sorted(store.read_range_on(REL, at, 1, &span.0, &span.1).unwrap()),
        sorted(by_scan(&store, 1, &span.0, &span.1)),
        "the Arrangement drifted from the primary order"
    );
    assert_eq!(store.read_range_on(REL, at, 1, &span.0, &span.1).unwrap().len(), 2);

    // Retract the rest of the span: the Arrangement must empty out too.
    store.commit(&[(REL, row(2, 5, "y"), -1), (REL, row(4, 5, "w"), -1)]).unwrap();
    let at = store.current();
    assert!(store.read_range_on(REL, at, 1, &span.0, &span.1).unwrap().is_empty());
}

/// An unknown relation, and a column past a row's arity, both answer empty
/// rather than erroring — the operator is total.
#[test]
fn arrangements_are_total() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    store.commit(&[(REL, row(1, 2, "a"), 1)]).unwrap();
    let at = store.current();
    let (l, h) = (Value::Int(i64::MIN), Value::Int(i64::MAX));
    assert!(store.read_range_on(RelId(99), at, 1, &l, &h).unwrap().is_empty());
    assert!(store.read_range_on(REL, at, 7, &l, &h).unwrap().is_empty());
}
