use grmpl_core::Entity;
use grmpl_ent::EntStore;
use grmpl_lang::{CompiledPackage, GrantSet, ResolvedCapabilityGrant};

const SOURCE: &str = r#"
package actor_package bootstrap 1
entity ACTOR = 7
rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
authority writes { write cursor write timers }
actor ACTOR { inbox inbox cursor cursor authority writes }
form command { "start" -> Start() "spin" -> Spin() }
on inbox parse command {
    match Start() { schedule world_clock at 0 send Spin() to ACTOR }
    match Spin() { schedule world_clock at 1 send Spin() to ACTOR }
}
bootstrap { inbox(ACTOR, 0, ("start")) inbox_seq(ACTOR, 1) }
"#;

fn compile(source: &str) -> Result<CompiledPackage, String> {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    CompiledPackage::compile_with_catalog(source, &store, 100)
}

#[test]
fn actor_declarations_compile_in_stable_name_order_and_resolve_targets() {
    let package = compile(SOURCE).unwrap();
    assert_eq!(package.actors.len(), 1);
    assert_eq!(package.actors[0].name, "ACTOR");
    assert_eq!(package.actors[0].entity, Entity(7));
    assert_eq!(package.authority_requests[0].name, "writes");

    let grants = GrantSet::new()
        .grant_schedule("world_clock", "clock", "timers", "inbox_seq", ["ACTOR"])
        .unwrap();
    let resolved = package.resolve_grants(&grants).unwrap();
    let (_, grant) = resolved.schedules().next().unwrap();
    let ResolvedCapabilityGrant::Schedule { targets, .. } = grant else {
        unreachable!()
    };
    assert_eq!(targets.get("ACTOR").map(|target| target.0), Some(Entity(7)));
}

#[test]
fn actor_schema_sequence_constructor_and_target_errors_fail_compilation() {
    let bad_schema = SOURCE.replace(
        "rel inbox(process: Ent, seq: Int, body: Tuple)",
        "rel inbox(process: Ent, seq: Text, body: Tuple)",
    );
    let error = compile(&bad_schema).err().unwrap();
    assert!(error.contains("bootstrap `inbox`"), "{error}");

    let hole = SOURCE.replace("inbox_seq(ACTOR, 1)", "inbox_seq(ACTOR, 2)");
    let error = compile(&hole).err().unwrap();
    assert!(error.contains("contiguous sequence range"), "{error}");

    let bad_target = SOURCE.replace("send Spin() to ACTOR", "send Spin() to UNKNOWN");
    assert!(compile(&bad_target)
        .err()
        .unwrap()
        .contains("targets undeclared actor `UNKNOWN`"));

    let noninvertible = SOURCE.replace("\"spin\" -> Spin()", "\"spin\" unrecoverable -> Spin()");
    assert!(compile(&noninvertible)
        .err()
        .unwrap()
        .contains("no invertible `Spin` constructor"));
}

#[test]
fn host_schedule_grant_must_allow_every_statically_used_target() {
    let package = compile(SOURCE).unwrap();
    let grants = GrantSet::new()
        .grant_schedule(
            "world_clock",
            "clock",
            "timers",
            "inbox_seq",
            std::iter::empty::<&str>(),
        )
        .unwrap();
    assert!(package
        .resolve_grants(&grants)
        .unwrap_err()
        .contains("disallows target actor `ACTOR`"));
}
