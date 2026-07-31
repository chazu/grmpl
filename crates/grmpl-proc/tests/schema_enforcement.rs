//! P1 (TKT-73): commit-boundary arity/type enforcement, applied beside the
//! Authority check. A world write whose relation has a registered schema must
//! conform (arity + column types); a relation with no registered schema passes
//! (schemas are opt-in). Enforcement holds on both commit paths — `commit_patch`
//! and `Domain::commit`.

use std::collections::HashMap;

use grmpl_core::{
    Authority, Column, DomainId, Entity, Error, Fact, Patch, RelId, Schema,
    Scope, Ty, Tuple, Value,
};
use grmpl_proc::{commit_patch, CommitOutcome, Domain};

const LOCATED: RelId = RelId(1); // (thing: Ent, place: Ent)
const A: DomainId = DomainId(1);

fn located_schema() -> Schema {
    Schema::new(vec![Column::new("thing", Ty::Ent), Column::new("place", Ty::Ent)])
}

fn authority() -> Authority {
    Authority::new(A, vec![Scope::whole(LOCATED)])
}

fn assert_located(t: Tuple) -> Patch {
    Patch::new().assert(Fact::new(LOCATED, t))
}

#[test]
fn well_typed_write_commits() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        let good = assert_located(Tuple::from([Value::Ent(Entity(100)), Value::Ent(Entity(7))]));
        let out = commit_patch(store, store, &good, &authority()).unwrap();
        assert!(matches!(out, CommitOutcome::Committed(_)));
    });
}

#[test]
fn wrong_type_is_rejected_at_commit() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        // `place` should be Ent; supply an Int.
        let bad = assert_located(Tuple::from([Value::Ent(Entity(100)), Value::Int(7)]));
        let err = commit_patch(store, store, &bad, &authority()).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "expected Schema error, got {err:?}");

        // Nothing was written (no effect).
        assert!(store.read_at(LOCATED, store.current()).unwrap().is_empty());
    });
}

#[test]
fn wrong_arity_is_rejected_at_commit() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        let bad = assert_located(Tuple::from([Value::Ent(Entity(100))])); // arity 1, want 2
        let err = commit_patch(store, store, &bad, &authority()).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "got {err:?}");
    });
}

#[test]
fn unregistered_relation_is_unchecked() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        // No schema registered for LOCATED: any shape/arity commits (opt-in).
        let odd = assert_located(Tuple::from([Value::Int(1), Value::text("x"), Value::Bool(true)]));
        let out = commit_patch(store, store, &odd, &authority()).unwrap();
        assert!(matches!(out, CommitOutcome::Committed(_)));
    });
}

#[test]
fn retracts_are_also_checked() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        let bad = Patch::new().retract(Fact::new(
            LOCATED,
            Tuple::from([Value::Ent(Entity(100)), Value::Int(7)]),
        ));
        let err = commit_patch(store, store, &bad, &authority()).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "got {err:?}");
    });
}

#[test]
fn authority_is_still_enforced_beside_schema() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        // Write is well-typed but outside the (empty) authority domain.
        let unowned = Authority::new(A, vec![]);
        let good = assert_located(Tuple::from([Value::Ent(Entity(100)), Value::Ent(Entity(7))]));
        let err = commit_patch(store, store, &good, &unowned).unwrap_err();
        assert!(matches!(err, Error::Authority(_)), "authority must still be enforced, got {err:?}");
    });
}

#[test]
fn domain_commit_enforces_schema() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.world();
        store.put_schema(LOCATED, &located_schema(), grmpl_core::Edition(1)).unwrap();

        let net = grmpl_transport::InProcessNet::new();
        let tx = net.endpoint(A);
        let domain = Domain {
            id: A,
            store,
            transport: &tx,
            routes: HashMap::new(),
            outbox: RelId(90),
            outseq: RelId(91),
            seen: RelId(92),
        };

        let bad = assert_located(Tuple::from([Value::Int(1), Value::Int(2)]));
        let err = domain.commit(&bad, &authority(), store).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "Domain::commit must enforce schema, got {err:?}");
    });
}
