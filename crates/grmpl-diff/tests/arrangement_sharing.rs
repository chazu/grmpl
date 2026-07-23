//! Arrangement sharing (DESIGN.md §3.2): a sub-DAG marked with `into_shared`
//! evaluates once per edition no matter how many queries reference it. Proven
//! with a store wrapper that counts base-relation reads.

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value,
};
use grmpl_diff::{eval_snapshot, eval_snapshot_with, Arrangements, Query};
use grmpl_store::FjallStore;

/// Delegates to a real store but tallies `read_at` calls per relation.
struct CountingStore {
    inner: FjallStore,
    reads: Mutex<HashMap<RelId, usize>>,
}

impl CountingStore {
    fn new(inner: FjallStore) -> Self {
        CountingStore { inner, reads: Mutex::new(HashMap::new()) }
    }
    fn reads(&self, rel: RelId) -> usize {
        *self.reads.lock().unwrap().get(&rel).unwrap_or(&0)
    }
    fn reset(&self) {
        self.reads.lock().unwrap().clear();
    }
}

impl EditionStore for CountingStore {
    fn current(&self) -> Edition {
        self.inner.current()
    }
}

impl TraceStore for CountingStore {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.inner.commit(updates)
    }
    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        self.inner.commit_if(preconditions, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        *self.reads.lock().unwrap().entry(rel).or_default() += 1;
        self.inner.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.inner.scan_updates(rel, from, to)
    }
}

const R: RelId = RelId(1);

fn seed(store: &CountingStore) {
    store
        .commit(&[
            (R, Tuple::from([Value::Ent(Entity(1)), Value::Ent(Entity(9))]), 1),
            (R, Tuple::from([Value::Ent(Entity(2)), Value::Ent(Entity(9))]), 1),
        ])
        .unwrap();
    store.reset(); // ignore the reads the commit path may incur
}

#[test]
fn shared_subdag_is_read_once_within_a_query() {
    let dir = tempfile::tempdir().unwrap();
    let store = CountingStore::new(FjallStore::open(dir.path()).unwrap());
    seed(&store);
    let at = store.current();

    // A self-join through a *shared* base reads R once.
    let base = Query::rel(R).into_shared();
    let shared_q = base.clone().join(base.clone(), [1], [1]);
    eval_snapshot(&shared_q, &store, at).unwrap();
    assert_eq!(store.reads(R), 1, "shared base evaluated once");

    // The same shape without sharing reads R twice.
    store.reset();
    let unshared_q = Query::rel(R).join(Query::rel(R), [1], [1]);
    eval_snapshot(&unshared_q, &store, at).unwrap();
    assert_eq!(store.reads(R), 2, "unshared base evaluated per reference");
}

#[test]
fn shared_arrangement_spans_multiple_queries() {
    let dir = tempfile::tempdir().unwrap();
    let store = CountingStore::new(FjallStore::open(dir.path()).unwrap());
    seed(&store);
    let at = store.current();

    let base = Query::rel(R).into_shared();
    let q1 = base.clone().project([0]);
    let q2 = base.clone().project([1]);

    // Evaluated through one shared arrangement cache: R read once total.
    let mut arr = Arrangements::new();
    let r1 = eval_snapshot_with(&q1, &store, at, &mut arr).unwrap();
    let r2 = eval_snapshot_with(&q2, &store, at, &mut arr).unwrap();
    assert_eq!(store.reads(R), 1, "one arrangement shared across two queries");

    // Sanity: results are still correct.
    assert_eq!(r1.len(), 2); // two distinct entities in column 0
    assert_eq!(r2.values().sum::<i64>(), 2); // both rows share place 9

    // Without the shared cache (independent evaluations) R is read twice.
    store.reset();
    eval_snapshot(&q1, &store, at).unwrap();
    eval_snapshot(&q2, &store, at).unwrap();
    assert_eq!(store.reads(R), 2);
}
