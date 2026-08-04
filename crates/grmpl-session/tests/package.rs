use std::sync::Arc;

use grmpl::{Runtime, Runtime as PublicRuntime};
use grmpl_core::{Authority, DomainId, Edition, Entity, Scope, Tuple, Value, WorldStore};
use grmpl_ent::EntStore;
use grmpl_lang::{CompiledPackage, GrantSet, INSTALL_MARKER_RELATION};

const SOURCE: &str = r#"
package test_world bootstrap 1

entity WORLD = 1
entity ROOM = 10

rel entity_seq(next: Int)
rel rng_state(owner: Ent, state: Int)
rel named(thing: Ent, name: Text)
rel tuning(scale: Float)
rel enabled(value: Bool)
rel nested(value: Tuple)

view world_name() {
    named(WORLD, name)
    yield name
}

requires allocate entities(
    counter: entity_seq,
    first: 1000,
    last: 1999
)

requires random rolls(
    state: rng_state,
    owner: WORLD,
    algorithm: xorshift64star_v1
)

bootstrap {
    entity_seq(1000)
    rng_state(WORLD, 4886718345)
    named(WORLD, "Test World")
    tuning(1.25)
    enabled(true)
    nested((1, 0.25, false))
}
"#;

fn grants() -> GrantSet {
    GrantSet::new()
        .grant_allocate("entities", "entity_seq", 900, 2500)
        .unwrap()
        .grant_random("rolls", "rng_state", Entity(1), "xorshift64star_v1")
        .unwrap()
}

fn open(path: &std::path::Path) -> Arc<dyn WorldStore> {
    Arc::new(EntStore::open(path).unwrap())
}

#[test]
fn bootstrap_is_one_atomic_edition_and_exact_reopen_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path());
        let runtime = Runtime::load_package(Arc::clone(&store), SOURCE, 100, &grants()).unwrap();
        assert_eq!(store.current(), Edition(1));
        assert_eq!(runtime.program().entity("WORLD"), Some(Entity(1)));
        assert_eq!(
            runtime.view("world_name", &[]).unwrap(),
            vec![(grmpl_core::Tuple::from([Value::text("Test World")]), 1)]
        );

        let tuning = runtime.relation("tuning").unwrap();
        assert_eq!(
            store.read_at(tuning, Edition(1)).unwrap(),
            vec![(grmpl_core::Tuple::from([Value::float(1.25).unwrap()]), 1)]
        );
        let marker = runtime.relation(INSTALL_MARKER_RELATION).unwrap();
        let marker_rows = store.read_at(marker, Edition(1)).unwrap();
        assert_eq!(marker_rows.len(), 1);
        assert_eq!(marker_rows[0].1, 1);
    }

    let store = open(dir.path());
    let runtime = PublicRuntime::load_package(Arc::clone(&store), SOURCE, 100, &grants()).unwrap();
    assert_eq!(
        store.current(),
        Edition(1),
        "exact reopen must create no edition"
    );
    assert_eq!(runtime.view("world_name", &[]).unwrap().len(), 1);
    let tuning = runtime.relation("tuning").unwrap();
    assert_eq!(
        store.read_at(tuning, store.current()).unwrap(),
        vec![(Tuple::from([Value::float(1.25).unwrap()]), 1)],
        "a Float bootstrap fact must survive a physical close/reopen"
    );
}

#[test]
fn provisioning_metadata_without_world_commit_resumes_safely() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path());
        let package = CompiledPackage::compile_with_catalog(SOURCE, store.as_ref(), 100).unwrap();
        package
            .program
            .register_schemas(store.as_ref(), store.as_ref(), Edition(1))
            .unwrap();
        assert_eq!(store.current(), Edition::ZERO);
        assert!(store.rel_id(INSTALL_MARKER_RELATION).unwrap().is_some());
    }

    let store = open(dir.path());
    Runtime::load_package(Arc::clone(&store), SOURCE, 100, &grants()).unwrap();
    assert_eq!(store.current(), Edition(1));
}

#[test]
fn marker_mismatch_and_unmarked_history_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    Runtime::load_package(Arc::clone(&store), SOURCE, 100, &grants()).unwrap();

    let changed = SOURCE.replace("tuning(1.25)", "tuning(1.5)");
    let error = Runtime::load_package(Arc::clone(&store), &changed, 100, &grants())
        .err()
        .expect("changed bootstrap must fail")
        .to_string();
    assert!(error.contains("marker mismatch"));
    assert_eq!(store.current(), Edition(1));

    let other = tempfile::tempdir().unwrap();
    let unmarked = open(other.path());
    unmarked
        .commit(&[(
            grmpl_core::RelId(999),
            grmpl_core::Tuple::from([Value::Int(1)]),
            1,
        )])
        .unwrap();
    let error = Runtime::load_package(Arc::clone(&unmarked), SOURCE, 100, &grants())
        .err()
        .expect("unmarked history must fail")
        .to_string();
    assert!(error.contains("no package marker"));
    assert!(error.contains("fresh v4 store"));
}

#[test]
fn digest_is_independent_of_catalog_ids_and_declaration_order() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let a = open(a_dir.path());
    let b = open(b_dir.path());
    b.register("unrelated", grmpl_core::RelId(900)).unwrap();

    let reordered = SOURCE.replace(
        "rel named(thing: Ent, name: Text)\nrel tuning(scale: Float)",
        "rel tuning(scale: Float)\nrel named(thing: Ent, name: Text)",
    );
    let pa = CompiledPackage::compile_with_catalog(SOURCE, a.as_ref(), 100).unwrap();
    let pb = CompiledPackage::compile_with_catalog(&reordered, b.as_ref(), 100).unwrap();
    assert_eq!(pa.bootstrap_digest, pb.bootstrap_digest);
    assert_ne!(
        pa.program.rel_id("named"),
        pb.program.rel_id("named"),
        "negative control: physical ids should actually differ"
    );
}

#[test]
fn grants_are_required_and_allocation_grants_must_contain_the_requested_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let missing = Runtime::load_package(Arc::clone(&store), SOURCE, 100, &GrantSet::new())
        .err()
        .unwrap()
        .to_string();
    assert!(missing.contains("missing allocate grant `entities`"));
    assert_eq!(store.current(), Edition::ZERO);

    let narrow = GrantSet::new()
        .grant_allocate("entities", "entity_seq", 1000, 1500)
        .unwrap()
        .grant_random("rolls", "rng_state", Entity(1), "xorshift64star_v1")
        .unwrap();
    let error = Runtime::load_package(Arc::clone(&store), SOURCE, 100, &narrow)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("does not contain required counter/range"));
    assert_eq!(store.current(), Edition::ZERO);
}

#[test]
fn invalid_bootstrap_and_resource_seeds_are_compile_errors() {
    let cases = [
        (
            SOURCE.replace("tuning(1.25)", "tuning(1.25)\n    tuning(1.25)"),
            "duplicated",
        ),
        (
            SOURCE.replace("entity ROOM = 10", "entity ROOM = 1000"),
            "allocation range",
        ),
        (
            SOURCE.replace("rng_state(WORLD, 4886718345)", "rng_state(WORLD, 0)"),
            "nonzero bootstrap state",
        ),
        (
            SOURCE.replace("entity_seq(1000)", "entity_seq(1001)"),
            "exactly one bootstrap row",
        ),
    ];
    for (i, (source, expected)) in cases.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let error = CompiledPackage::compile_with_catalog(&source, store.as_ref(), 100)
            .err()
            .unwrap_or_else(|| panic!("case {i} unexpectedly compiled"));
        assert!(error.contains(expected), "case {i}: {error}");
    }
}

const CAPABILITY_SOURCE: &str = r#"
package capability_world bootstrap 1
entity WORLD = 1

rel entity_seq(next: Int)
rel rng_state(owner: Ent, state: Int)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel made(thing: Ent, roll: Int, score: Float)

requires allocate entities(counter: entity_seq, first: 1000, last: 1002)
requires random rolls(state: rng_state, owner: WORLD, algorithm: xorshift64star_v1)

form command {
    "make" -> Make()
}

on inbox parse command {
    match Make() {
        fresh entities as thing
        random rolls below 100 as roll
        let score = float(roll) / 2.0
        if score >= 0.0 {
            assert made(thing, roll, score)
        }
    }
}

bootstrap {
    entity_seq(1000)
    rng_state(WORLD, 1)
}
"#;

#[test]
fn capability_execution_is_replayable_and_seals_each_state_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let runtime =
        Runtime::load_package(Arc::clone(&store), CAPABILITY_SOURCE, 100, &grants()).unwrap();
    let inbox = runtime.relation("inbox").unwrap();
    let cursor = runtime.relation("cursor").unwrap();
    let counter = runtime.relation("entity_seq").unwrap();
    let rng = runtime.relation("rng_state").unwrap();
    let made = runtime.relation("made").unwrap();
    let actor = Entity(1);
    let authority = Authority::new(
        DomainId(1),
        vec![
            Scope::whole(cursor),
            Scope::whole(counter),
            Scope::whole(rng),
            Scope::whole(made),
        ],
    );
    let process = runtime
        .process(actor, authority, "inbox", "cursor")
        .unwrap();
    let input_edition = grmpl_proc::enqueue(
        store.as_ref(),
        inbox,
        actor,
        0,
        Tuple::from([Value::text("make")]),
    )
    .unwrap();

    let first = process
        .patch_at(store.as_ref(), 0, input_edition)
        .unwrap()
        .unwrap();
    let replay = process
        .patch_at(store.as_ref(), 0, input_edition)
        .unwrap()
        .unwrap();
    assert_eq!(
        first, replay,
        "same snapshot and message must reproduce the exact capability patch"
    );
    process
        .step(store.as_ref(), store.as_ref())
        .unwrap()
        .unwrap();

    assert_eq!(
        store.read_at(counter, store.current()).unwrap(),
        vec![(Tuple::from([Value::Int(1001)]), 1)]
    );
    assert_eq!(
        store.read_at(rng, store.current()).unwrap(),
        vec![(Tuple::from([Value::Ent(actor), Value::Int(0x0200_0001)]), 1)]
    );
    assert_eq!(
        store.read_at(made, store.current()).unwrap(),
        vec![(
            Tuple::from([
                Value::Ent(Entity(1000)),
                Value::Int(65),
                Value::float(32.5).unwrap()
            ]),
            1
        )]
    );
}

const ALLOCATION_RACE_SOURCE: &str = r#"
package allocation_race bootstrap 1
rel entity_seq(next: Int)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel created(thing: Ent, name: Text)
requires allocate entities(counter: entity_seq, first: 1000, last: 1002)
form command { "make" name -> Make(name) }
on inbox parse command {
    match Make(name) {
        fresh entities as thing
        assert created(thing, name)
    }
}
bootstrap { entity_seq(1000) }
"#;

#[test]
fn language_allocation_race_has_one_winner_and_retry_gets_a_distinct_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let grants = GrantSet::new()
        .grant_allocate("entities", "entity_seq", 1000, 1002)
        .unwrap();
    let runtime =
        Runtime::load_package(Arc::clone(&store), ALLOCATION_RACE_SOURCE, 100, &grants).unwrap();
    let inbox = runtime.relation("inbox").unwrap();
    let cursor = runtime.relation("cursor").unwrap();
    let counter = runtime.relation("entity_seq").unwrap();
    let created = runtime.relation("created").unwrap();
    let authority = || {
        Authority::new(
            DomainId(1),
            vec![
                Scope::whole(cursor),
                Scope::whole(counter),
                Scope::whole(created),
            ],
        )
    };
    let alice = runtime
        .process(Entity(10), authority(), "inbox", "cursor")
        .unwrap();
    let bob = runtime
        .process(Entity(11), authority(), "inbox", "cursor")
        .unwrap();
    grmpl_proc::enqueue(
        store.as_ref(),
        inbox,
        Entity(10),
        0,
        Tuple::from([Value::text("make"), Value::text("alice")]),
    )
    .unwrap();
    grmpl_proc::enqueue(
        store.as_ref(),
        inbox,
        Entity(11),
        0,
        Tuple::from([Value::text("make"), Value::text("bob")]),
    )
    .unwrap();
    let contested = store.current();
    let alice_patch = alice
        .patch_at(store.as_ref(), 0, contested)
        .unwrap()
        .unwrap();
    let bob_patch = bob.patch_at(store.as_ref(), 0, contested).unwrap().unwrap();
    assert!(matches!(
        grmpl_proc::commit_patch(
            store.as_ref(),
            store.as_ref(),
            &alice_patch,
            &alice.authority
        )
        .unwrap(),
        grmpl_proc::CommitOutcome::Committed(_)
    ));
    assert_eq!(
        grmpl_proc::commit_patch(store.as_ref(), store.as_ref(), &bob_patch, &bob.authority)
            .unwrap(),
        grmpl_proc::CommitOutcome::Rejected,
        "the stale counter precondition must reject the whole losing creation"
    );
    assert_eq!(
        store.read_at(counter, store.current()).unwrap(),
        vec![(Tuple::from([Value::Int(1001)]), 1)]
    );
    assert_eq!(
        store.read_at(created, store.current()).unwrap(),
        vec![(
            Tuple::from([Value::Ent(Entity(1000)), Value::text("alice")]),
            1
        )]
    );

    assert!(matches!(
        bob.step(store.as_ref(), store.as_ref()).unwrap(),
        Some(grmpl_proc::CommitOutcome::Committed(_))
    ));
    assert_eq!(
        store.read_at(counter, store.current()).unwrap(),
        vec![(Tuple::from([Value::Int(1002)]), 1)]
    );
    let rows = store.read_at(created, store.current()).unwrap();
    assert!(rows.contains(&(
        Tuple::from([Value::Ent(Entity(1000)), Value::text("alice")]),
        1
    )));
    assert!(rows.contains(&(
        Tuple::from([Value::Ent(Entity(1001)), Value::text("bob")]),
        1
    )));

    grmpl_proc::enqueue(
        store.as_ref(),
        inbox,
        Entity(10),
        1,
        Tuple::from([Value::text("make"), Value::text("carol")]),
    )
    .unwrap();
    assert!(matches!(
        alice.step(store.as_ref(), store.as_ref()).unwrap(),
        Some(grmpl_proc::CommitOutcome::Committed(_))
    ));
    assert_eq!(
        store.read_at(counter, store.current()).unwrap(),
        vec![(Tuple::from([Value::Int(1003)]), 1)]
    );

    grmpl_proc::enqueue(
        store.as_ref(),
        inbox,
        Entity(10),
        2,
        Tuple::from([Value::text("make"), Value::text("overflow")]),
    )
    .unwrap();
    let before = store.current();
    let error = alice
        .step(store.as_ref(), store.as_ref())
        .unwrap_err()
        .to_string();
    assert!(error.contains("allocator `entities` exhausted"), "{error}");
    assert_eq!(
        store.current(),
        before,
        "exhaustion must not commit a partial creation"
    );
    assert_eq!(
        alice.position(store.as_ref()).unwrap(),
        2,
        "the exhausted input remains unconsumed"
    );
    assert!(!store
        .read_at(created, store.current())
        .unwrap()
        .iter()
        .any(|(tuple, _)| tuple.as_slice().get(1) == Some(&Value::text("overflow"))));
}

const RNG_RACE_SOURCE: &str = r#"
package rng_race bootstrap 1
entity WORLD = 1
rel rng_state(owner: Ent, state: Int)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel drawn(process: Ent, roll: Int)
requires random rolls(state: rng_state, owner: WORLD, algorithm: xorshift64star_v1)
form command { "draw" -> Draw() }
on inbox parse command {
    match Draw() {
        random rolls below 100 as roll
        assert drawn(self, roll)
    }
}
bootstrap { rng_state(WORLD, 1) }
"#;

#[test]
fn random_stream_contention_retries_from_the_committed_successor() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let grants = GrantSet::new()
        .grant_random("rolls", "rng_state", Entity(1), "xorshift64star_v1")
        .unwrap();
    let runtime = Runtime::load_package(Arc::clone(&store), RNG_RACE_SOURCE, 100, &grants).unwrap();
    let inbox = runtime.relation("inbox").unwrap();
    let cursor = runtime.relation("cursor").unwrap();
    let rng = runtime.relation("rng_state").unwrap();
    let drawn = runtime.relation("drawn").unwrap();
    let authority = || {
        Authority::new(
            DomainId(1),
            vec![Scope::whole(cursor), Scope::whole(rng), Scope::whole(drawn)],
        )
    };
    let alice = runtime
        .process(Entity(10), authority(), "inbox", "cursor")
        .unwrap();
    let bob = runtime
        .process(Entity(11), authority(), "inbox", "cursor")
        .unwrap();
    for actor in [Entity(10), Entity(11)] {
        grmpl_proc::enqueue(
            store.as_ref(),
            inbox,
            actor,
            0,
            Tuple::from([Value::text("draw")]),
        )
        .unwrap();
    }

    let contested = store.current();
    let alice_patch = alice
        .patch_at(store.as_ref(), 0, contested)
        .unwrap()
        .unwrap();
    let bob_patch = bob.patch_at(store.as_ref(), 0, contested).unwrap().unwrap();
    assert!(matches!(
        grmpl_proc::commit_patch(
            store.as_ref(),
            store.as_ref(),
            &alice_patch,
            &alice.authority
        )
        .unwrap(),
        grmpl_proc::CommitOutcome::Committed(_)
    ));
    assert_eq!(
        grmpl_proc::commit_patch(store.as_ref(), store.as_ref(), &bob_patch, &bob.authority)
            .unwrap(),
        grmpl_proc::CommitOutcome::Rejected
    );
    assert_eq!(
        store.read_at(rng, store.current()).unwrap(),
        vec![(
            Tuple::from([Value::Ent(Entity(1)), Value::Int(0x0200_0001)]),
            1
        )]
    );
    assert_eq!(
        store.read_at(drawn, store.current()).unwrap(),
        vec![(Tuple::from([Value::Ent(Entity(10)), Value::Int(65)]), 1)],
        "the rejected draw must leave no result-dependent fact"
    );

    assert!(matches!(
        bob.step(store.as_ref(), store.as_ref()).unwrap(),
        Some(grmpl_proc::CommitOutcome::Committed(_))
    ));
    assert_eq!(
        store.read_at(rng, store.current()).unwrap(),
        vec![(
            Tuple::from([Value::Ent(Entity(1)), Value::Int(0x0004_0040_0080_2801)]),
            1
        )]
    );
    let rows = store.read_at(drawn, store.current()).unwrap();
    assert!(rows.contains(&(Tuple::from([Value::Ent(Entity(10)), Value::Int(65)]), 1)));
    assert!(rows.contains(&(Tuple::from([Value::Ent(Entity(11)), Value::Int(17)]), 1)));
}
