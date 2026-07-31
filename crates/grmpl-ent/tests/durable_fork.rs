//! **G-6 acceptance: a durable fork shares nodes, and the DagWood survives.**
//!
//! `fork_at` used to return a store with no granfilade at all: forking a durable
//! world silently produced an in-memory one, and the branch graph was rebuilt
//! empty on every `open`, so a reopened store had forgotten its forks. Both made
//! the Ent's headline capability — `O(edit)` virtual copy of a whole world —
//! unavailable to anything that had to survive a restart.
//!
//! All branches now live in **one** granfilade with their roots namespaced by
//! branch. So a fork writes roots pointing at nodes that are already there, and
//! writes **no nodes at all** — where the LSM's fork copies `O(state)` bytes.

use grmpl_core::{Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{Dag, EntStore};

const REL: RelId = RelId(1);
const OTHER: RelId = RelId(2);

fn t(n: i64) -> Tuple {
    Tuple::from([Value::Int(n)])
}

fn seed(store: &EntStore, rows: i64) {
    let updates: Vec<_> = (0..rows).map(|k| (REL, t(k), 1)).collect();
    store.commit(&updates).unwrap();
}

/// The defining property: forking a 5000-row world adds **no** stored nodes,
/// because the fork's roots name nodes the granfilade already holds.
#[test]
fn forking_a_durable_world_writes_roots_not_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    seed(&store, 5_000);

    let encoded_before = store.frames_encoded();
    let fork = store.fork_at(store.current()).unwrap();

    assert_eq!(
        store.frames_encoded(),
        encoded_before,
        "the fork encoded node frames — it copied instead of sharing"
    );
    assert_ne!(fork.branch_id(), store.branch_id(), "a fork is a new branch");

    // The fork sees the whole world it was cut from.
    assert_eq!(fork.read_at(REL, fork.current()).unwrap().len(), 5_000);
}

/// The two branches evolve independently, and neither disturbs the other.
#[test]
fn branches_diverge_without_disturbing_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    seed(&store, 10);
    let cut = store.current();
    let fork = store.fork_at(cut).unwrap();

    store.commit(&[(OTHER, Tuple::from([Value::text("parent")]), 1)]).unwrap();
    fork.commit(&[(OTHER, Tuple::from([Value::text("child")]), 1)]).unwrap();

    let parent_rows = store.read_at(OTHER, store.current()).unwrap();
    let child_rows = fork.read_at(OTHER, fork.current()).unwrap();
    assert_eq!(parent_rows, vec![(Tuple::from([Value::text("parent")]), 1)]);
    assert_eq!(child_rows, vec![(Tuple::from([Value::text("child")]), 1)]);

    // Both still hold the shared pre-fork state.
    assert_eq!(store.read_at(REL, store.current()).unwrap().len(), 10);
    assert_eq!(fork.read_at(REL, fork.current()).unwrap().len(), 10);
}

/// A fork survives a reopen: its roots and its place in the DagWood are durable,
/// so the branch can be picked up again by id.
#[test]
fn a_fork_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (branch, cut) = {
        let store = EntStore::open(dir.path()).unwrap();
        seed(&store, 20);
        let cut = store.current();
        let fork = store.fork_at(cut).unwrap();
        fork.commit(&[(OTHER, Tuple::from([Value::text("only on the fork")]), 1)]).unwrap();
        (fork.branch_id(), cut)
    };

    // Reopen the world, then pick the branch back up by id — the fork is a
    // first-class, durable world. (Both branches share one granfilade handle:
    // the directory lock is exclusive, which is exactly why one node store
    // holding every branch is the right shape.)
    let root = EntStore::open(dir.path()).unwrap();
    let fork = root.branch(branch).unwrap();
    assert_eq!(fork.read_at(REL, fork.current()).unwrap().len(), 20, "shared state lost");
    assert_eq!(
        fork.read_at(OTHER, fork.current()).unwrap(),
        vec![(Tuple::from([Value::text("only on the fork")]), 1)],
        "the fork's own commits did not survive"
    );

    // The parent branch is untouched by the fork's divergence.
    assert_eq!(root.current(), cut, "the parent's clock moved");
    assert!(root.read_at(OTHER, root.current()).unwrap().is_empty());

    // And the branch graph itself persisted: the fork still knows its ancestry.
    assert_eq!(fork.dag().parent(branch), Some((Dag::ROOT, cut.0)));
    assert!(fork.descends_from(&root, cut), "ancestry did not survive the reopen");
}

/// GC must root from **every** branch, or collecting the parent's orphans would
/// take the fork's shared nodes with it.
#[test]
fn gc_preserves_every_branch() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    seed(&store, 200);
    let fork = store.fork_at(store.current()).unwrap();

    // Churn the parent so it orphans plenty of its own nodes, then collect.
    for k in 200..400i64 {
        store.commit(&[(REL, t(k), 1)]).unwrap();
    }
    store.consolidate(store.current()).unwrap();
    store.gc().unwrap();

    // The fork's state is intact — its nodes were reachable from its own roots.
    assert_eq!(fork.read_at(REL, fork.current()).unwrap().len(), 200);
    let branch = fork.branch_id();
    drop(fork);
    drop(store);
    let fork = EntStore::open_branch(dir.path(), branch).unwrap();
    assert_eq!(
        fork.read_at(REL, fork.current()).unwrap().len(),
        200,
        "GC collected nodes the fork still referenced"
    );
}

/// Forking below the current edition cuts at that edition, and the door still
/// guards anything below the watermark.
#[test]
fn forking_at_a_past_edition_cuts_there() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    for k in 0..10i64 {
        store.commit(&[(REL, t(k), 1)]).unwrap();
    }
    let fork = store.fork_at(Edition(4)).unwrap();
    assert_eq!(fork.current(), Edition(4));
    assert_eq!(fork.read_at(REL, Edition(4)).unwrap().len(), 4);

    store.consolidate(Edition(6)).unwrap();
    assert!(store.fork_at(Edition(2)).is_err(), "fork below the watermark must error");
}
