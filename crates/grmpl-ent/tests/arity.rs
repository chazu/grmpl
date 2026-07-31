//! **G-1b acceptance: one granfilade record per *run* of tuples, not per tuple.**
//!
//! Before the enfilade had B+ arity, every tree node held one entry and every
//! node was one content-addressed record, so a 1000-row relation was 1000 fjall
//! records — a 16-byte key, child pointers and an LSM index entry *per fact* —
//! and a load was one KV `get` per fact. That constant factor, not any
//! asymptotic gap, is what kept the Ent from replacing the LSM.
//!
//! These tests pin the property down so it cannot silently regress: records grow
//! as `rows / arity`, while the two things arity must **not** cost — within-
//! lineage structural sharing, and exact logical contents — still hold.

use grmpl_core::{Tuple, Value};
use grmpl_ent::granfilade::Granfilade;
use grmpl_ent::measure::Count;
use grmpl_ent::tree::{Tree, B};

type FactTree = Tree<Tuple, i64, Count>;

const ROWS: i64 = 1000;

fn t(n: i64) -> Tuple {
    Tuple::from([Value::Int(n)])
}

fn build(n: i64) -> FactTree {
    let mut tree = FactTree::new();
    for k in 0..n {
        tree = tree.insert(t(k), k * 10);
    }
    tree
}

/// A `ROWS`-entry relation must cost on the order of `ROWS / B` records, not
/// `ROWS`. The floor is what a perfectly-packed tree would need; the ceiling
/// allows for half-full nodes after splits plus the internal levels.
#[test]
fn records_scale_with_runs_not_tuples() {
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open(dir.path()).unwrap();
    let tree = build(ROWS);
    gran.persist(&tree).unwrap();

    let records = gran.node_count().unwrap();
    let perfect = ROWS as usize / B;
    println!(
        "{ROWS} rows at arity {B} -> {records} granfilade records \
         ({:.0}x fewer than one-per-tuple)",
        ROWS as f64 / records as f64
    );
    assert_eq!(tree.len(), ROWS as usize, "the tree must hold every row");
    assert!(
        records <= perfect * 4,
        "expected ~{perfect} records for {ROWS} rows at arity {B}, got {records} \
         — arity is not reaching the granfilade"
    );
    // The pre-arity behaviour was exactly one record per tuple; stay far away.
    assert!(
        records * 8 < ROWS as usize,
        "{records} records for {ROWS} rows is within 8x of one-record-per-tuple"
    );
}

/// Arity must not cost within-lineage structural sharing: an edit on top of a
/// persisted version still writes only the nodes on its path. That is the
/// property forks, cheap history and GC all rest on.
#[test]
fn an_edit_still_writes_only_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open(dir.path()).unwrap();
    let v0 = build(ROWS);
    gran.persist(&v0).unwrap();
    let before = gran.node_count().unwrap();

    let v1 = v0.insert(t(ROWS + 1), 7);
    gran.persist(&v1).unwrap();
    let added = gran.node_count().unwrap() - before;

    assert!(added > 0, "nothing was stored for the new version");
    assert!(
        added <= 4,
        "an edit added {added} records — path copy should touch only the root-to-leaf spine"
    );
    // v0 is untouched by the edit that produced v1.
    assert_eq!(v0.len(), ROWS as usize);
    assert_eq!(v1.len(), ROWS as usize + 1);
}

/// A wide-node tree must round-trip through the granfilade exactly — same
/// entries, same order, same measure — or the shape the content keys address is
/// not the shape that comes back.
#[test]
fn wide_nodes_round_trip_through_the_granfilade() {
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open(dir.path()).unwrap();

    // Several sizes: below one leaf, exactly one split, and deep enough to build
    // internal levels above internal levels.
    for rows in [1i64, (B as i64) - 1, B as i64, (B as i64) + 1, 500, 5_000] {
        let tree = build(rows);
        let ck = gran.persist(&tree).unwrap();
        let back: FactTree = gran.load(ck).unwrap();

        let want: Vec<(Tuple, i64)> = tree.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let got: Vec<(Tuple, i64)> = back.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(got, want, "{rows} rows did not round-trip");
        assert_eq!(back.len(), tree.len(), "{rows} rows: size");
        assert_eq!(back.measure(), tree.measure(), "{rows} rows: measure");

        // Re-persisting the reloaded tree adds nothing: the shape came back
        // identical, so every node hashes to a key already present.
        let before = gran.node_count().unwrap();
        gran.persist(&back).unwrap();
        assert_eq!(
            gran.node_count().unwrap(),
            before,
            "{rows} rows: reload changed the shape, so content keys moved"
        );
    }
}
