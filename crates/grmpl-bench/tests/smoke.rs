//! Smoke tests: run every scenario at a tiny size so the harness stays green in
//! CI (`cargo test`) without paying a real benchmark's wall-clock, and assert
//! the invariants each scenario is supposed to expose. Plus a behavioural
//! characterization of the arrangement memo's transient-pointer (ABA) hazard.

use grmpl_bench::scenarios;

#[test]
fn churn_paths_make_progress() {
    let (_d, store) = scenarios::open_store();
    let s = scenarios::churn::raw_commit(&store, 50, 4);
    assert_eq!(s.get("facts"), Some(200.0), "200 facts committed");
    assert!(s.ops_per_sec() > 0.0 && s.ops_per_sec().is_finite());

    let (_d2, store2) = scenarios::open_store();
    let p = scenarios::churn::patch_commit(&store2, 50, 4);
    assert_eq!(p.get("facts"), Some(200.0));
}

#[test]
fn watch_fanout_delivers_to_every_watcher() {
    let s = scenarios::watch::deltastream_fanout(4, 10, 20);
    // Each of the 4 watchers must see all 10 new `a` values.
    assert_eq!(s.get("delivered"), Some(40.0), "4 watchers x 10 deltas");

    let p = scenarios::watch::onwatch_pump(10, 20);
    assert_eq!(p.get("activations"), Some(10.0), "one batch of 10 activations");
}

#[test]
fn precondition_adds_cost_over_the_bare_commit() {
    // Not a timing assertion (too flaky at tiny sizes) — just that both paths
    // run and the precondition path records the history it scanned.
    let base = scenarios::precond::baseline(200, 100);
    assert_eq!(base.get("k"), Some(0.0));
    let cif = scenarios::precond::commit_if_cost(200, 4, 100);
    assert_eq!(cif.get("hist"), Some(200.0));
    assert_eq!(cif.get("k"), Some(4.0));
    assert!(cif.get("ns/precond-row").unwrap() > 0.0);
}

#[test]
fn contention_keeps_exactly_one_winner_per_round() {
    // `race` asserts internally that the counter lands exactly on target and
    // that total wins == increments (no lost or double increment). Drive it with
    // real threads at a small target.
    let s = scenarios::contention::race(4, 200);
    assert_eq!(s.ops, 200, "exactly `target` successful increments");
    let fair = s.get("fair(min/max)").unwrap();
    assert!((0.0..=1.0).contains(&fair));
}

#[test]
fn shared_arrangement_reads_the_base_once() {
    for k in [2u64, 8] {
        let (shared, unshared) = scenarios::arrangement::sharing(k, 30);
        assert_eq!(shared.get("base_reads"), Some(1.0), "shared base read once");
        assert_eq!(
            unshared.get("base_reads"),
            Some(k as f64),
            "unshared base read per reference"
        );
    }
}

#[test]
fn memo_pins_one_entry_per_edition() {
    let s = scenarios::arrangement::memo_footprint(20, 30);
    assert_eq!(s.get("entries"), Some(20.0), "one memo entry per edition");
    assert!(s.get("cached_tuples").unwrap() > 0.0);
}

// --------------------------------------------------------------------------
// Arrangement memo — the transient-pointer (ABA) hazard.
// --------------------------------------------------------------------------

use std::collections::HashMap;

use grmpl_core::{EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{eval_snapshot, eval_snapshot_with, multiset, Arrangements, Query};

const R: RelId = RelId(77);

fn seed(store: &dyn TraceStore, rows: i64) {
    for a in 0..rows {
        store
            .commit(&[(R, Tuple::from([Value::Ent(Entity(a as u64)), Value::Int(a)]), 1)])
            .unwrap();
    }
}

/// The safe invariant: a **fresh** `Arrangements` (what `eval_snapshot` always
/// builds) is correct for every distinct shared query, and reuse across queries
/// whose `Arc`s *coexist* is also correct — distinct live nodes have distinct
/// pointers, so their cache keys never collide.
#[test]
fn fresh_and_coexisting_caches_are_sound() {
    let (_d, store) = scenarios::open_store();
    seed(&store, 25);
    let at = store.current();

    let all = Query::rel(R).filter(|_| true).into_shared();
    let none = Query::rel(R).filter(|_| false).into_shared();

    // Fresh cache per eval — always right.
    let all_v = multiset::to_sorted_vec(&eval_snapshot(&all, &store, at).unwrap());
    let none_v = multiset::to_sorted_vec(&eval_snapshot(&none, &store, at).unwrap());
    assert_eq!(all_v.len(), 25);
    assert!(none_v.is_empty());

    // One cache shared by two *coexisting* queries at the same edition: each
    // keeps its own entry (distinct live pointers), so both stay correct.
    let mut arr: Arrangements = HashMap::new();
    let a2 = multiset::to_sorted_vec(&eval_snapshot_with(&all, &store, at, &mut arr).unwrap());
    let n2 = multiset::to_sorted_vec(&eval_snapshot_with(&none, &store, at, &mut arr).unwrap());
    assert_eq!(a2, all_v, "coexisting reuse: `all` still correct");
    assert_eq!(n2, none_v, "coexisting reuse: `none` still correct");
    assert_eq!(arr.len(), 2, "two distinct live shared nodes → two entries");
}

/// Characterize the hazard behaviourally: reuse one cache across a *dropped*
/// shared query and a fresh, semantically different one. If the freed node's
/// heap address is recycled for the new node (same size class → common), the
/// stale entry is returned at the same edition — the `none` query wrongly yields
/// `all`'s rows. We only assert *when* the recycle is observed (so the test is
/// never flaky); either way it documents why the memo must never outlive the
/// `Query` value that owns its `Arc`s.
#[test]
fn aba_hazard_stale_hit_when_pointer_is_recycled() {
    let (_d, store) = scenarios::open_store();
    seed(&store, 25);
    let at = store.current();

    let mut reproduced = false;
    for _ in 0..200_000 {
        let mut arr: Arrangements = HashMap::new();

        // q1: all rows (non-empty). Evaluate, cache under (ptr(q1), at), drop q1.
        let q1 = Query::rel(R).filter(|_| true).into_shared();
        let r1 = eval_snapshot_with(&q1, &store, at, &mut arr).unwrap();
        assert_eq!(r1.len(), 25);
        drop(q1);

        // q2: same node *shape* (so the same allocation size class), opposite
        // semantics — its true result is empty.
        let q2 = Query::rel(R).filter(|_| false).into_shared();
        let r2 = eval_snapshot_with(&q2, &store, at, &mut arr).unwrap();

        if !r2.is_empty() {
            // Address recycled: the cache handed q2 the stale arrangement keyed
            // by q1's now-freed pointer. That value must be exactly q1's.
            assert_eq!(
                multiset::to_sorted_vec(&r2),
                multiset::to_sorted_vec(&r1),
                "a stale hit must return the recycled node's cached value"
            );
            reproduced = true;
            break;
        }
    }

    // Reproduction is allocator-dependent; document the outcome without making
    // the test's pass/fail hinge on the allocator's recycling behaviour.
    eprintln!(
        "ABA hazard reproduced this run: {reproduced} \
         (the memo is safe only because `eval_snapshot` builds a fresh cache per call)"
    );
}
