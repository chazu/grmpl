//! **Arrangement memory.** The shared-arrangement memo (`grmpl-diff`'s
//! `Arrangements`) caches a sub-DAG's value keyed by
//! `(Arc::as_ptr(node) as usize, edition)`. Two things to measure, one to warn
//! about:
//!
//! * **Compute saved.** A sub-DAG referenced `k` times behind `into_shared`
//!   evaluates *once* per edition, not `k` times. We count base reads (a wrapper
//!   store tallies `read_at`) to prove `1` vs `k`, and time both.
//! * **Memory pinned.** Reusing one `Arrangements` across `e` editions holds `e`
//!   full `Multiset` snapshots alive at once — the cache trades memory for
//!   compute. We report entries and total cached tuples: the memo's footprint.
//! * **ABA hazard (documented, not benchmarked).** The key is a *transient* heap
//!   pointer. A cache outliving the `Query` value that owns the `Arc` is unsound:
//!   a freed node's address can be reused by a *different* `Query` node, and at
//!   the same edition the stale entry would be returned for the wrong sub-DAG.
//!   This is why the memo is only ever built fresh per top-level evaluation
//!   (`eval_snapshot`) and never persisted — the property the smoke test pins,
//!   and the constraint the P13 statefulness work must design around (a stable,
//!   non-pointer arrangement identity).

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value,
};
use grmpl_diff::{eval_snapshot, eval_snapshot_with, Arrangements, Query};
use grmpl_ent::EntStore;

use crate::harness::{measure, Report, Sample};
use crate::scenarios::open_store;

const B: RelId = RelId(50);

/// A `TraceStore` that delegates to the ent but tallies `read_at` calls — the
/// witness that a shared arrangement is evaluated once, not once per reference.
pub struct CountingStore {
    inner: EntStore,
    reads: Mutex<usize>,
}

impl CountingStore {
    pub fn new(inner: EntStore) -> CountingStore {
        CountingStore { inner, reads: Mutex::new(0) }
    }
    pub fn reads(&self) -> usize {
        *self.reads.lock().unwrap()
    }
    pub fn reset(&self) {
        *self.reads.lock().unwrap() = 0;
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
        *self.reads.lock().unwrap() += 1;
        self.inner.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.inner.scan_updates(rel, from, to)
    }
}

fn base_row(a: i64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a as u64)), Value::Int(a)])
}

/// `distinct(base ∪ base ∪ … )` — `k` references to `base`, unioned. When `base`
/// carries `into_shared`, the `k` references collapse to one evaluation.
fn fan(base: Query, k: u64) -> Query {
    let mut q = base.clone();
    for _ in 1..k {
        q = q.union(base.clone());
    }
    q.distinct()
}

/// Seed `B` with `rows` distinct rows in a counting store.
fn seeded(rows: u64) -> (tempfile::TempDir, CountingStore) {
    let (dir, inner) = open_store();
    for a in 0..rows as i64 {
        inner.commit(&[(B, base_row(a), 1)]).unwrap();
    }
    (dir, CountingStore::new(inner))
}

/// Evaluate a `k`-way fan with and without `into_shared`; report reads + time.
pub fn sharing(k: u64, rows: u64) -> (Sample, Sample) {
    let (_d, store) = seeded(rows);
    let at = store.current();

    let shared_q = fan(Query::rel(B).into_shared(), k);
    store.reset();
    let shared = measure(format!("shared    k={k}"), 1, || {
        eval_snapshot(&shared_q, &store, at).unwrap();
    });
    let shared_reads = store.reads();

    let unshared_q = fan(Query::rel(B), k);
    store.reset();
    let unshared = measure(format!("unshared  k={k}"), 1, || {
        eval_snapshot(&unshared_q, &store, at).unwrap();
    });
    let unshared_reads = store.reads();

    (
        shared.note("base_reads", shared_reads as f64).note("k", k as f64),
        unshared.note("base_reads", unshared_reads as f64).note("k", k as f64),
    )
}

/// Reuse one `Arrangements` across `editions` editions of a shared query and
/// report the memo's footprint: entries pinned and total tuples cached.
pub fn memo_footprint(editions: u64, rows: u64) -> Sample {
    let (_d, store) = seeded(rows);
    let shared_q = fan(Query::rel(B).into_shared(), 3);

    let mut arr: Arrangements = HashMap::new();
    let s = measure("memo across editions", editions, || {
        for e in 0..editions as i64 {
            // Move the world so each iteration is a distinct edition/key.
            store.inner.commit(&[(B, base_row(1_000_000 + e), 1)]).unwrap();
            let at = store.current();
            eval_snapshot_with(&shared_q, &store, at, &mut arr).unwrap();
        }
    });

    let entries = arr.len();
    let cached_tuples: usize = arr.values().map(|m| m.len()).sum();
    s.note("entries", entries as f64).note("cached_tuples", cached_tuples as f64)
}

/// Arrangement compute-saving and memo footprint.
pub fn report(rows: u64, editions: u64) -> Report {
    let mut rep = Report::new("Arrangement memo (compute saved vs memory pinned)");
    for &k in &[2u64, 8, 32] {
        let (shared, unshared) = sharing(k, rows);
        rep.push(unshared);
        rep.push(shared);
    }
    rep.push(memo_footprint(editions, rows));
    rep
}
