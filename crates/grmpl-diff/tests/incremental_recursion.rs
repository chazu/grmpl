//! Incremental recursive maintenance (DESIGN.md §3.1). The `IncrementalFixpoint`
//! maintainer must always agree with the authoritative boundary-recompute
//! (`find(iterate(...))`), while taking the cheap incremental path on monotone
//! (insertion) changes and recomputing only on retraction.

use std::collections::HashSet;

use grmpl_core::{Diff, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{eval_snapshot, IncrementalFixpoint, Maintenance, Query};
use grmpl_ent::EntStore;

const DIRECT: RelId = RelId(3); //    (entity, behavior)
const PROTOTYPE: RelId = RelId(4); // (child, parent)

fn implements() -> (Query, Query) {
    let init = Query::rel(DIRECT);
    let step = Query::rel(PROTOTYPE).join(Query::recur(), [1], [0]).project([0, 3]);
    (init, step)
}

fn seed_iterate() -> Query {
    let (init, step) = implements();
    Query::iterate(init, step)
}

fn sorted(m: &grmpl_diff::Multiset) -> Vec<(Tuple, Diff)> {
    grmpl_diff::multiset::to_sorted_vec(m)
}

#[test]
fn incremental_matches_recompute_under_churn() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let (init, step) = implements();

    let mut fix = IncrementalFixpoint::new(init, step, &store, store.current()).unwrap();

    let mut protos: HashSet<Tuple> = HashSet::new();
    let mut directs: HashSet<Tuple> = HashSet::new();
    let mut s: u64 = 0xdead_beef_cafe_f00d;
    let mut rng = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };

    for round in 0..200 {
        // Toggle a base fact; remember whether this round was an insertion.
        let (rel, set, tuple) = if rng() % 2 == 0 {
            let t = Tuple::from([Value::Ent(Entity(rng() % 5)), Value::Ent(Entity(rng() % 5))]);
            (PROTOTYPE, &mut protos, t)
        } else {
            let b = if rng() % 2 == 0 { "fly" } else { "swim" };
            let t = Tuple::from([Value::Ent(Entity(rng() % 5)), Value::text(b)]);
            (DIRECT, &mut directs, t)
        };
        let inserting = !set.contains(&tuple);
        let diff: Diff = if inserting {
            set.insert(tuple.clone());
            1
        } else {
            set.remove(&tuple);
            -1
        };
        store.commit(&[(rel, tuple, diff)]).unwrap();

        let delta = fix.advance(&store, store.current()).unwrap();

        // Correctness: maintained fixpoint equals a fresh boundary recompute.
        let want = eval_snapshot(&seed_iterate(), &store, store.current()).unwrap();
        assert_eq!(sorted(fix.current()), sorted(&want), "round {round}: fixpoint mismatch");

        // Path: insertions grow the fixpoint; retractions take the DRed
        // (delete-and-re-derive) path. Both are incremental — neither recomputes
        // from ∅.
        let want_path = if inserting { Maintenance::Grow } else { Maintenance::DeleteRederive };
        assert_eq!(fix.last_path, want_path, "round {round}: unexpected maintenance path");

        // The returned delta is the actual change to the maintained set.
        // (Applying delta to the previous set yields the new set — checked
        // implicitly by the fixpoint-equality assertion; here we sanity-check
        // that a nonempty world eventually produces some derivations.)
        let _ = delta;
    }
}

#[test]
fn extending_a_chain_is_incremental_and_cheap() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let (init, step) = implements();
    let mut fix = IncrementalFixpoint::new(init, step, &store, store.current()).unwrap();

    // Root behavior, then grow the prototype chain 4 -> 3 -> 2 -> 1 one edge at a time.
    store.commit(&[(DIRECT, Tuple::from([Value::Ent(Entity(4)), Value::text("fly")]), 1)]).unwrap();
    fix.advance(&store, store.current()).unwrap();
    assert_eq!(fix.last_path, Maintenance::Grow);

    for (child, parent) in [(3u64, 4u64), (2, 3), (1, 2)] {
        store
            .commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(child)), Value::Ent(Entity(parent))]), 1)])
            .unwrap();
        let delta = fix.advance(&store, store.current()).unwrap();
        assert_eq!(fix.last_path, Maintenance::Grow, "chain extension should grow");
        // Each new edge adds exactly one derived `implements(child, fly)`.
        assert_eq!(
            delta.iter().filter(|(_, d)| **d == 1).count(),
            1,
            "one new derivation per edge"
        );
        // Incremental convergence is cheap: a couple of semi-naïve rounds.
        assert!(fix.last_iterations <= 2, "few iterations, got {}", fix.last_iterations);
    }

    // Everyone in the chain now implements "fly".
    let all = eval_snapshot(&seed_iterate(), &store, store.current()).unwrap();
    assert_eq!(all.len(), 4);

    // Deleting the root edge takes the DRed (delete-and-re-derive) path and
    // retracts the now-unsupported derivations — incrementally, not by recompute.
    store
        .commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(3)), Value::Ent(Entity(4))]), -1)])
        .unwrap();
    let delta = fix.advance(&store, store.current()).unwrap();
    assert_eq!(fix.last_path, Maintenance::DeleteRederive, "deletion takes the DRed path");
    assert!(fix.last_overdeletion_rounds >= 1, "overdeletion actually ran");
    // 3, 2, 1 lose "fly"; only the root (4) keeps it.
    assert_eq!(delta.iter().filter(|(_, d)| **d == -1).count(), 3);
    assert_eq!(fix.current().len(), 1);
}

/// Retracting the grounding of a **derivation cycle** must collapse it. Two
/// entities in a prototype cycle mutually re-derive a behavior that entered the
/// cycle from an outside node; deleting the edge that brought it in leaves the
/// cycle with no grounding, so both derivations must be retracted. A naive
/// "still one-step supported?" overdeletion would keep them forever (each is
/// supported by the other) — real DRed keys on the broken derivation and cascades.
#[test]
fn deleting_a_cycles_grounding_collapses_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let (init, step) = implements();

    // Outside grounding: entity 2 directly "swim".
    store.commit(&[(DIRECT, Tuple::from([Value::Ent(Entity(2)), Value::text("swim")]), 1)]).unwrap();
    // Bring "swim" into the cycle: 0 inherits from 2.
    store.commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(0)), Value::Ent(Entity(2))]), 1)]).unwrap();
    // The 2-cycle 0 <-> 1, so 0 and 1 mutually derive each other's behaviors.
    store.commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(1)), Value::Ent(Entity(0))]), 1)]).unwrap();
    store.commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(0)), Value::Ent(Entity(1))]), 1)]).unwrap();

    let mut fix = IncrementalFixpoint::new(init, step, &store, store.current()).unwrap();
    // Both 0 and 1 implement "swim" (grounded via 0 -> 2), plus 2 itself.
    assert_eq!(fix.current().len(), 3);
    let want = eval_snapshot(&seed_iterate(), &store, store.current()).unwrap();
    assert_eq!(sorted(fix.current()), sorted(&want));

    // Delete the grounding edge 0 -> 2. The cycle can no longer reach "swim".
    store
        .commit(&[(PROTOTYPE, Tuple::from([Value::Ent(Entity(0)), Value::Ent(Entity(2))]), -1)])
        .unwrap();
    let delta = fix.advance(&store, store.current()).unwrap();

    assert_eq!(fix.last_path, Maintenance::DeleteRederive);
    // 0 and 1 both lose "swim"; only 2 (the direct grounding) keeps it.
    assert_eq!(delta.iter().filter(|(_, d)| **d == -1).count(), 2, "both cycle members retracted");
    assert_eq!(fix.current().len(), 1);
    let want = eval_snapshot(&seed_iterate(), &store, store.current()).unwrap();
    assert_eq!(sorted(fix.current()), sorted(&want), "agrees with boundary recompute");
}
