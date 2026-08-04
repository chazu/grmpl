use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use grmpl::{DriveStatus, NamedAuthority, NamedScope, Runtime, RuntimePolicy};
use grmpl_core::{DomainId, Entity, Tuple, Value, WorldStore};
use grmpl_ent::EntStore;
use grmpl_lang::GrantSet;

const ACTOR: Entity = Entity(7);

const PATROL_SOURCE: &str = r#"
package actor_patrol bootstrap 1

entity ACTOR = 7

rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel state(actor: Ent, count: Int)
rel seen(actor: Ent, text: Text)

requires schedule world_clock(
    clock: clock,
    timers: timers,
    sequences: inbox_seq
)

authority actor_writes {
    write cursor
    write state
    write seen
    write timers
}

actor ACTOR {
    inbox inbox
    cursor cursor
    authority actor_writes
}

form actor_command {
    "start" -> Start()
    "tick" -> Tick()
    "echo" word -> Echo(word)
}

on inbox parse actor_command {
    match Start() {
        schedule world_clock at 5 send Tick() to ACTOR
        schedule world_clock at 5 send Echo("hello") to ACTOR
    }
    match Tick() {
        find state(ACTOR, count)
        let next = count + 1
        retract state(ACTOR, count)
        assert state(ACTOR, next)
    }
    match Echo(word) {
        assert seen(ACTOR, word)
    }
}

bootstrap {
    inbox(ACTOR, 0, ("start"))
    inbox_seq(ACTOR, 1)
    state(ACTOR, 0)
}
"#;

const SPIN_SOURCE: &str = r#"
package actor_spin bootstrap 1
entity ACTOR = 7
rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel state(actor: Ent, count: Int)
rel seen(actor: Ent, text: Text)
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
authority actor_writes { write cursor write timers }
actor ACTOR { inbox inbox cursor cursor authority actor_writes }
form command { "start" -> Start() "spin" -> Spin() }
on inbox parse command {
    match Start() { schedule world_clock at 0 send Spin() to ACTOR }
    match Spin() { schedule world_clock at 0 send Spin() to ACTOR }
}
bootstrap { inbox(ACTOR, 0, ("start")) inbox_seq(ACTOR, 1) }
"#;

const FAULT_SOURCE: &str = r#"
package actor_fault bootstrap 1
entity ACTOR = 7
rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel state(actor: Ent, count: Int)
rel seen(actor: Ent, text: Text)
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
authority actor_writes { write cursor write timers }
actor ACTOR { inbox inbox cursor cursor authority actor_writes }
form command { "start" -> Start() "fault" -> Fault() }
on inbox parse command {
    match Start() { schedule world_clock at 0 send Fault() to ACTOR }
    match Fault() { let impossible = 1 / 0 }
}
bootstrap { inbox(ACTOR, 0, ("start")) inbox_seq(ACTOR, 1) }
"#;

const ORDER_SOURCE: &str = r#"
package actor_order bootstrap 1
entity A = 1
entity B = 2
rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
rel permit(actor: Ent, next: Ent)
rel done(value: Bool)
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
authority writes { write cursor write permit write done }
actor A { inbox inbox cursor cursor authority writes }
actor B { inbox inbox cursor cursor authority writes }
form command { "go" -> Go() }
on inbox parse command {
    match Go() {
        find permit(self, next)
        expect permit(self, next)
        retract permit(self, next)
        if self == A {
            assert permit(B, B)
        } else {
            assert done(true)
        }
    }
}
bootstrap {
    inbox(A, 0, ("go"))
    inbox(B, 0, ("go"))
    inbox_seq(A, 1)
    inbox_seq(B, 1)
    permit(A, B)
}
"#;

fn open(path: &std::path::Path) -> Arc<dyn WorldStore> {
    Arc::new(EntStore::open(path).unwrap())
}

fn policy(targets: &[&str]) -> RuntimePolicy {
    let grants = GrantSet::new()
        .grant_schedule(
            "world_clock",
            "clock",
            "timers",
            "inbox_seq",
            targets.iter().copied(),
        )
        .unwrap();
    let actor = NamedAuthority::new(
        DomainId(1),
        ["cursor", "state", "seen", "timers"]
            .into_iter()
            .map(NamedScope::whole)
            .collect(),
    );
    let driver = NamedAuthority::new(
        DomainId(1),
        ["clock", "timers", "inbox_seq", "inbox"]
            .into_iter()
            .map(NamedScope::whole)
            .collect(),
    );
    RuntimePolicy::new(grants, BTreeMap::from([("ACTOR".into(), actor)]), driver)
}

fn live_rows(runtime: &Runtime, relation: &str) -> Vec<Tuple> {
    let rel = runtime.relation(relation).unwrap();
    runtime
        .store()
        .read_at(rel, runtime.store().current())
        .unwrap()
        .into_iter()
        .filter(|(_, weight)| *weight > 0)
        .map(|(tuple, _)| tuple)
        .collect()
}

#[test]
fn committed_time_drives_form_rendered_messages_without_player_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let runtime =
        Runtime::load_driven_package(store, PATROL_SOURCE, 100, &policy(&["ACTOR"])).unwrap();

    let start = runtime.drive_to_idle().unwrap();
    assert_eq!(start.status, DriveStatus::Idle);
    assert_eq!((start.actor_steps, start.timers_fired), (1, 0));

    runtime.record_sample(4, 10).unwrap();
    assert_eq!(runtime.drive_to_idle().unwrap().committed, 0);
    assert_eq!(
        live_rows(&runtime, "state"),
        vec![Tuple::from([Value::Ent(ACTOR), Value::Int(0)])]
    );

    runtime.record_sample(5, 11).unwrap();
    let due = runtime.drive_to_idle().unwrap();
    assert_eq!(due.status, DriveStatus::Idle);
    assert_eq!((due.actor_steps, due.timers_fired), (2, 2));
    assert_eq!(
        live_rows(&runtime, "state"),
        vec![Tuple::from([Value::Ent(ACTOR), Value::Int(1)])]
    );
    assert_eq!(
        live_rows(&runtime, "seen"),
        vec![Tuple::from([Value::Ent(ACTOR), Value::text("hello")])]
    );
    let before = runtime.store().current();
    assert!(runtime.record_sample(4, 12).is_err());
    assert_eq!(runtime.store().current(), before);
}

#[test]
fn reopen_between_schedule_fire_and_attention_resumes_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    {
        let runtime =
            Runtime::load_driven_package(open(dir.path()), PATROL_SOURCE, 100, &policy(&["ACTOR"]))
                .unwrap();
        let report = runtime
            .drive_with_fuel(NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert_eq!(report.status, DriveStatus::FuelExhausted);
        assert_eq!((report.actor_steps, report.timers_fired), (1, 0));
    }
    {
        let runtime =
            Runtime::load_driven_package(open(dir.path()), PATROL_SOURCE, 100, &policy(&["ACTOR"]))
                .unwrap();
        runtime.record_sample(5, 0).unwrap();
        let report = runtime
            .drive_with_fuel(NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert_eq!((report.actor_steps, report.timers_fired), (0, 1));
    }
    let runtime =
        Runtime::load_driven_package(open(dir.path()), PATROL_SOURCE, 100, &policy(&["ACTOR"]))
            .unwrap();
    let report = runtime.drive_to_idle().unwrap();
    assert_eq!((report.actor_steps, report.timers_fired), (2, 1));
    assert_eq!(
        live_rows(&runtime, "state"),
        vec![Tuple::from([Value::Ent(ACTOR), Value::Int(1)])]
    );
    assert_eq!(runtime.drive_to_idle().unwrap().committed, 0);
}

#[test]
fn fuel_bounds_an_immediate_self_scheduling_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let runtime =
        Runtime::load_driven_package(open(dir.path()), SPIN_SOURCE, 100, &policy(&["ACTOR"]))
            .unwrap();
    runtime.record_sample(0, 0).unwrap();
    let first = runtime
        .drive_with_fuel(NonZeroUsize::new(5).unwrap())
        .unwrap();
    assert_eq!(first.status, DriveStatus::FuelExhausted);
    assert_eq!(first.committed, 5);
    let second = runtime
        .drive_with_fuel(NonZeroUsize::new(3).unwrap())
        .unwrap();
    assert_eq!(second.status, DriveStatus::FuelExhausted);
    assert_eq!(second.committed, 3);
}

#[test]
fn actor_fault_is_visible_sticky_and_does_not_advance_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let runtime =
        Runtime::load_driven_package(open(dir.path()), FAULT_SOURCE, 100, &policy(&["ACTOR"]))
            .unwrap();
    runtime.record_sample(0, 0).unwrap();
    let first = runtime.drive_to_idle().unwrap();
    assert!(matches!(
        first.status,
        DriveStatus::ActorFault {
            actor: ACTOR,
            sequence: 1,
            ..
        }
    ));
    assert_eq!((first.actor_steps, first.timers_fired), (1, 1));
    let second = runtime.drive_to_idle().unwrap();
    assert!(matches!(
        second.status,
        DriveStatus::ActorFault { sequence: 1, .. }
    ));
    assert_eq!(second.committed, 0);
}

#[test]
fn missing_target_grant_and_authority_fail_before_driving() {
    let dir = tempfile::tempdir().unwrap();
    let error = Runtime::load_driven_package(open(dir.path()), PATROL_SOURCE, 100, &policy(&[]))
        .err()
        .expect("target grant must be required");
    assert!(error.contains("disallows target actor `ACTOR`"));

    let dir = tempfile::tempdir().unwrap();
    let mut missing = policy(&["ACTOR"]);
    missing.actor_authorities.clear();
    let error = Runtime::load_driven_package(open(dir.path()), PATROL_SOURCE, 100, &missing)
        .err()
        .expect("actor authority must be required");
    assert!(error.contains("missing host authority for actor `ACTOR`"));
}

#[test]
fn ready_actors_run_in_canonical_inbox_entity_sequence_order() {
    let dir = tempfile::tempdir().unwrap();
    let grants = GrantSet::new()
        .grant_schedule("world_clock", "clock", "timers", "inbox_seq", ["A", "B"])
        .unwrap();
    let actor = NamedAuthority::new(
        DomainId(1),
        ["cursor", "permit", "done"]
            .into_iter()
            .map(NamedScope::whole)
            .collect(),
    );
    let driver = NamedAuthority::new(
        DomainId(1),
        ["clock", "timers", "inbox_seq", "inbox"]
            .into_iter()
            .map(NamedScope::whole)
            .collect(),
    );
    let policy = RuntimePolicy::new(
        grants,
        BTreeMap::from([("A".into(), actor.clone()), ("B".into(), actor)]),
        driver,
    );
    let runtime =
        Runtime::load_driven_package(open(dir.path()), ORDER_SOURCE, 100, &policy).unwrap();
    let report = runtime.drive_to_idle().unwrap();
    assert_eq!(report.status, DriveStatus::Idle);
    assert_eq!(report.actor_steps, 2);
    assert_eq!(
        live_rows(&runtime, "done"),
        vec![Tuple::from([Value::Bool(true)])]
    );
}
