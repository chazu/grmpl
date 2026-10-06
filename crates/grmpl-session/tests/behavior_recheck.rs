//! The P12 commit-boundary re-check of stored code, through the runtime.
//!
//! A language handler can store a behavior it was sent: a `form` binds any
//! value, so a message carrying a [`Value::Code`] cell lands in the world when
//! the handler asserts it. (The compiler types a form binding as `Text`, so the
//! carrier column is untyped, and the message is enqueued as a tuple rather
//! than a tokenized line.) The carrier relation is owned, so the Authority law
//! alone admits the write; what must refuse it is the re-check of the stored
//! behavior's own effects against the committing authority. Both runtime
//! commit paths for language processes run it: a driven package's actors and
//! [`Runtime::run_to_idle`].

use std::collections::BTreeMap;
use std::sync::Arc;

use grmpl::{DriveStatus, NamedAuthority, NamedScope, Runtime, RuntimePolicy};
use grmpl_core::{
    Authority, DomainId, Entity, Error, Scope, Tuple, Value, WorldStore,
};
use grmpl_ent::EntStore;
use grmpl_lang::{BehaviorIr, BehaviorOp, ExprIr, GrantSet, PredExpr, StoredBehavior, ValueExpr};
use grmpl_proc::enqueue_seq;

const ACTOR: Entity = Entity(7);

const DRIVEN: &str = r#"
package stored_code bootstrap 1
entity ACTOR = 7
rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel slot(owner: Ent, code)
rel w0(a: Int)
rel w1(a: Int)
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
authority actor_writes { write cursor write slot }
actor ACTOR { inbox inbox cursor cursor authority actor_writes }
form command { "install" stored -> Install(stored) }
on inbox parse command {
    match Install(stored) { assert slot(ACTOR, stored) }
}
bootstrap { inbox_seq(ACTOR, 0) }
"#;

const PLAIN: &str = r#"
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel slot(owner: Ent, code)
rel w0(a: Int)
rel w1(a: Int)
form command { "install" stored -> Install(stored) }
on inbox parse command {
    match Install(stored) { assert slot(self, stored) }
}
"#;

/// A behavior that asserts one row into `relation`.
fn writing(relation: &str) -> Value {
    StoredBehavior::new(
        PredExpr::And(vec![]),
        vec![],
        BehaviorIr::new(vec![BehaviorOp::Assert {
            relation: relation.into(),
            arguments: vec![ExprIr::Value(ValueExpr::Literal(Value::Int(0)))],
        }]),
    )
    .to_value()
}

fn install(runtime: &Runtime, process: Entity, code: Value) {
    enqueue_seq(
        runtime.store(),
        runtime.relation("inbox").unwrap(),
        runtime.relation("inbox_seq").unwrap(),
        process,
        Tuple::from([Value::text("install"), code]),
    )
    .unwrap();
}

fn live(runtime: &Runtime, relation: &str) -> usize {
    let rel = runtime.relation(relation).unwrap();
    runtime
        .store()
        .read_at(rel, runtime.store().current())
        .unwrap()
        .into_iter()
        .filter(|(_, weight)| *weight > 0)
        .count()
}

fn assert_code_rejected(outcome: std::result::Result<impl std::fmt::Debug, Error>) {
    match outcome {
        Err(Error::Authority(message)) => assert!(
            message.contains("stored behavior"),
            "rejected by the code re-check, not the carrier write: {message}"
        ),
        other => panic!("installing code beyond the authority must fail: {other:?}"),
    }
}

fn fresh_store() -> (tempfile::TempDir, Arc<dyn WorldStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
    (dir, Arc::new(store))
}

#[test]
fn a_driven_actor_cannot_install_code_beyond_its_authority() {
    let (_dir, store) = fresh_store();
    let grants = GrantSet::new()
        .grant_schedule("world_clock", "clock", "timers", "inbox_seq", ["ACTOR"])
        .unwrap();
    // The actor owns its carrier relation and `w0`, never `w1`.
    let actor = NamedAuthority::new(
        DomainId(1),
        ["cursor", "slot", "w0"].into_iter().map(NamedScope::whole).collect(),
    );
    let driver = NamedAuthority::new(
        DomainId(1),
        ["clock", "timers", "inbox_seq", "inbox"]
            .into_iter()
            .map(NamedScope::whole)
            .collect(),
    );
    let policy = RuntimePolicy::new(grants, BTreeMap::from([("ACTOR".into(), actor)]), driver);
    let runtime = Runtime::load_driven_package(store, DRIVEN, 100, &policy).unwrap();

    install(&runtime, ACTOR, writing("w1"));
    let before = runtime.store().current();
    assert_code_rejected(runtime.drive_to_idle());
    assert_eq!(runtime.store().current(), before, "a refused install allocates no edition");
    assert_eq!(live(&runtime, "slot"), 0);
    assert_eq!(live(&runtime, "cursor"), 0, "the message stays undelivered");

    // A fresh world, the same actor, code within its authority: it installs.
    let (_dir, store) = fresh_store();
    let runtime = Runtime::load_driven_package(store, DRIVEN, 100, &policy).unwrap();
    install(&runtime, ACTOR, writing("w0"));
    let report = runtime.drive_to_idle().unwrap();
    assert_eq!(report.status, DriveStatus::Idle);
    assert_eq!(report.actor_steps, 1);
    assert_eq!(live(&runtime, "slot"), 1);
}

#[test]
fn a_runtime_process_cannot_install_code_beyond_its_authority() {
    let (_dir, store) = fresh_store();
    let runtime = Runtime::compile(store, PLAIN, 100).unwrap();
    let owns = ["cursor", "slot", "w0"]
        .into_iter()
        .map(|name| Scope::whole(runtime.relation(name).unwrap()))
        .collect();
    let process = runtime
        .process(ACTOR, Authority::new(DomainId(1), owns), "inbox", "cursor")
        .unwrap();

    install(&runtime, ACTOR, writing("w1"));
    let before = runtime.store().current();
    assert_code_rejected(runtime.run_to_idle(&process));
    assert_eq!(runtime.store().current(), before, "a refused install allocates no edition");
    assert_eq!(live(&runtime, "slot"), 0);

    // `Process::step` is the explicit opt-out: the same message commits there.
    // The runtime's own path is what refuses it.
    assert!(process
        .step(runtime.store(), runtime.store())
        .unwrap()
        .is_some());
    assert_eq!(live(&runtime, "slot"), 1);

    install(&runtime, ACTOR, writing("w0"));
    assert_eq!(runtime.run_to_idle(&process).unwrap(), 1);
    assert_eq!(live(&runtime, "slot"), 2);
}
