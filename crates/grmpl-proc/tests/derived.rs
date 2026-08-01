//! **G-8 acceptance: the Derived enfilade — materialized views that survive a
//! reopen.**
//!
//! Plan v4 §1.5 lists Derived enfilades as a first-class member of the family
//! and calls them grmpl's differential extension of the `Ent`. They were neither
//! built nor listed as deferred: what existed was an in-memory arrangement memo
//! keyed by `(Arc::as_ptr, edition)` that could not outlive a single advance, so
//! a view's incremental state died with the process.
//!
//! A materialized view is now an ordinary relation the engine maintains, which
//! puts it in the Fact enfilade and gives it everything that comes with one:
//! versioned by edition, structurally shared, persisted with the commit that
//! produced it, GC-reachable, and readable as-of.
//!
//! The law: after any interleaving of churn and refreshes, the derived relation
//! equals `find(view, cursor)` — the view as-of the frontier it has folded in.
//! It may lag; it may not disagree.

use std::collections::BTreeMap;

use grmpl_core::{
    Authority, DomainId, Entity, NoSchemas, RelId, Scope, TraceStore, Tuple, Value,
};
use grmpl_diff::{eval_snapshot, multiset, Query};
use grmpl_proc::Materialized;

const BASE: RelId = RelId(60);
const DERIVED: RelId = RelId(61);
const CURSOR: RelId = RelId(62);
const OTHER: RelId = RelId(63);
const KEY: Entity = Entity(900);

/// `distinct(a)` of `BASE` — a view whose deltas genuinely cancel, so the
/// derived relation must net rather than accumulate.
fn view() -> Query {
    Query::rel(BASE).project([0]).distinct()
}

fn materialized() -> Materialized {
    Materialized {
        view: view(),
        into: DERIVED,
        key: KEY,
        cursor_rel: CURSOR,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(DERIVED), Scope::whole(CURSOR)],
        ),
    }
}

fn churn(store: &dyn TraceStore, rel: RelId, a: i64, b: i64, diff: i64) {
    store
        .commit(&[(rel, Tuple::from([Value::Int(a), Value::Int(b)]), diff)])
        .unwrap();
}

/// The derived relation's live rows as `a -> weight`.
fn derived_rows(store: &dyn TraceStore) -> BTreeMap<i64, i64> {
    let mut m = BTreeMap::new();
    for (t, d) in store.read_at(DERIVED, store.current()).unwrap() {
        if let [Value::Int(a)] = t.as_slice() {
            *m.entry(*a).or_insert(0) += d;
        }
    }
    m.retain(|_, w| *w != 0);
    m
}

/// Ground truth: the view evaluated from scratch at `at`.
fn truth(store: &dyn TraceStore, at: grmpl_core::Edition) -> BTreeMap<i64, i64> {
    let snap = eval_snapshot(&view(), store, at).unwrap();
    let mut m = BTreeMap::new();
    for (t, w) in multiset::to_sorted_vec(&snap) {
        if let [Value::Int(a)] = t.as_slice() {
            *m.entry(*a).or_insert(0) += w;
        }
    }
    m.retain(|_, w| *w != 0);
    m
}

/// The core law, over an interleaving of churn and refreshes on every substrate.
#[test]
fn a_materialized_view_equals_the_view_at_its_cursor() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let m = materialized();
        m.install(store, &NoSchemas).unwrap();

        let mut st = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            st ^= st >> 12;
            st ^= st << 25;
            st ^= st >> 27;
            st
        };

        for round in 0..60 {
            // Small value space so rows genuinely cross zero and cancel.
            let a = (next() % 5) as i64;
            let b = (next() % 3) as i64;
            let diff = if next().is_multiple_of(2) { 1 } else { -1 };
            churn(store, BASE, a, b, diff);
            // Sometimes churn a relation the view does not read, so the routing
            // gate is exercised on a live materialization.
            if next().is_multiple_of(3) {
                churn(store, OTHER, a, b, 1);
            }
            if !next().is_multiple_of(4) {
                m.refresh(store, &NoSchemas).unwrap();
                let cursor = m.cursor(store).unwrap().unwrap();
                assert_eq!(
                    derived_rows(store),
                    truth(store, cursor),
                    "{}: round {round}: the materialization disagrees with the view at its cursor",
                    c.name
                );
            }
        }

        // Drain, then it must equal the view at current exactly.
        m.refresh(store, &NoSchemas).unwrap();
        let cursor = m.cursor(store).unwrap().unwrap();
        assert_eq!(derived_rows(store), truth(store, cursor), "{}: after drain", c.name);
        assert!(!derived_rows(store).is_empty(), "{}: vacuous — nothing materialized", c.name);
    });
}

/// **The point of the enfilade**: the materialization is durable. Close the
/// world, reopen it, and the derived rows *and* the maintenance cursor are still
/// there — nothing is recomputed, because the substrate persisted them.
#[test]
fn a_materialization_survives_a_reopen_without_recomputing() {
    grmpl_conformance::for_each_world(|w| {
        let (rows_before, cursor_before) = {
            let store = w.open();
            let store = store.as_ref();
            let m = materialized();
            m.install(store, &NoSchemas).unwrap();
            for k in 0..12i64 {
                churn(store, BASE, k % 5, k, 1);
            }
            m.refresh(store, &NoSchemas).unwrap();
            (derived_rows(store), m.cursor(store).unwrap())
        }; // dropped — the crash

        let store = w.open();
        let store = store.as_ref();
        let m = materialized();
        assert_eq!(derived_rows(store), rows_before, "{}: derived rows lost", w.name);
        assert_eq!(m.cursor(store).unwrap(), cursor_before, "{}: cursor lost", w.name);
        assert!(!rows_before.is_empty(), "vacuous");

        // A refresh straight after the reopen does nothing: the cursor is already
        // at the frontier, so there is no interval to fold and no recomputation.
        assert_eq!(m.refresh(store, &NoSchemas).unwrap(), 0, "{}: recomputed on reopen", w.name);

        // Maintenance simply continues from where it stopped.
        churn(store, BASE, 99, 0, 1);
        assert!(m.refresh(store, &NoSchemas).unwrap() > 0, "{}: did not resume", w.name);
        let cursor = m.cursor(store).unwrap().unwrap();
        assert_eq!(derived_rows(store), truth(store, cursor), "{}: diverged after resume", w.name);
    });
}

/// A derived relation is an ordinary relation: it can be read as-of a past
/// edition, and queried and joined like any other.
#[test]
fn a_derived_relation_is_an_ordinary_relation() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let m = materialized();
        m.install(store, &NoSchemas).unwrap();

        churn(store, BASE, 1, 0, 1);
        m.refresh(store, &NoSchemas).unwrap();
        let after_first = store.current();

        churn(store, BASE, 2, 0, 1);
        m.refresh(store, &NoSchemas).unwrap();

        // As-of: the earlier edition still shows only what was materialized then.
        let early = m.rows(store, after_first).unwrap();
        assert_eq!(early.len(), 1, "{}: as-of read of the derived relation", c.name);
        assert_eq!(m.rows(store, store.current()).unwrap().len(), 2, "{}: current", c.name);

        // And it composes: a view over the derived relation evaluates normally.
        let over_derived = Query::rel(DERIVED).filter(|t| t.as_slice()[0] == Value::Int(2));
        let snap = eval_snapshot(&over_derived, store, store.current()).unwrap();
        assert_eq!(multiset::to_sorted_vec(&snap).len(), 1, "{}: query over derived", c.name);
    });
}
