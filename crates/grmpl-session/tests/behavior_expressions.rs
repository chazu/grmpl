use std::sync::Arc;

use grmpl::Runtime;
use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Scope, TraceStore, Tuple, Value, WorldStore,
};
use grmpl_ent::EntStore;
use grmpl_proc::enqueue;

const SOURCE: &str = r#"
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel result(i: Int, f: Float, b: Bool)

form command {
    "calc" -> Calc()
    "short" -> Short()
    "fault" -> Fault()
}

on inbox parse command {
    match Calc() {
        let i = max(1, 10 - 3)
        let f = 0.1 + 0.2
        let b = i == 7 && f > 0.3
        if b {
            assert result(i, f, b)
        } else {
            assert result(0, 0.0, false)
        }
    }
    match Short() {
        let safe = false && (1 / 0 == 0)
        assert result(1, 1.5, safe)
    }
    match Fault() {
        let impossible = 9223372036854775807 + 1
        assert result(impossible, 0.0, false)
    }
}
"#;

#[test]
fn arithmetic_control_flow_and_fault_cursor_law() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir.path()).unwrap());
    let runtime = Runtime::compile(Arc::clone(&store), SOURCE, 100).unwrap();
    let inbox = runtime.relation("inbox").unwrap();
    let cursor = runtime.relation("cursor").unwrap();
    let result = runtime.relation("result").unwrap();
    let actor = Entity(7);
    let process = runtime
        .process(
            actor,
            Authority::new(
                DomainId(1),
                vec![Scope::whole(cursor), Scope::whole(result)],
            ),
            "inbox",
            "cursor",
        )
        .unwrap();

    for (seq, command) in ["calc", "short", "fault"].into_iter().enumerate() {
        enqueue(
            store.as_ref(),
            inbox,
            actor,
            seq as i64,
            Tuple::from([Value::text(command)]),
        )
        .unwrap();
    }

    process
        .step(store.as_ref(), store.as_ref())
        .unwrap()
        .unwrap();
    process
        .step(store.as_ref(), store.as_ref())
        .unwrap()
        .unwrap();
    let rows = store.read_at(result, store.current()).unwrap();
    assert!(rows.contains(&(
        Tuple::from([
            Value::Int(7),
            Value::float(f64::from_bits(0x3fd3_3333_3333_3334)).unwrap(),
            Value::Bool(true),
        ]),
        1,
    )));
    assert!(
        rows.contains(&(
            Tuple::from([
                Value::Int(1),
                Value::float(1.5).unwrap(),
                Value::Bool(false)
            ]),
            1,
        )),
        "false && rhs must not evaluate the division by zero"
    );

    let before = store.current();
    let error = process.step(store.as_ref(), store.as_ref()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("actor 7 inbox sequence 2"), "{message}");
    assert!(message.contains("arithmetic fault in `add`"), "{message}");
    assert_eq!(
        store.current(),
        before,
        "a deterministic behavior fault allocates no edition"
    );
    assert_eq!(
        process.position(store.as_ref()).unwrap(),
        2,
        "faulting input remains unconsumed"
    );
}

#[test]
fn ill_typed_expressions_are_rejected_before_execution() {
    let cases = [
        ("let x = 1 + 1.0", "same Int or Float"),
        ("let x = true + false", "same Int or Float"),
        ("let x = 1 < true", "same concrete type"),
        ("let x = unknown + 1", "unbound local"),
        ("let x = 1 let x = 2", "bound more than once"),
        (
            "if true { let branch_only = 1 } let x = branch_only",
            "unbound local",
        ),
    ];
    for (body, expected) in cases {
        let source = format!(
            "rel inbox(process: Ent, seq: Int, body: Tuple)\n\
             form command {{ \"go\" -> Go() }}\n\
             on inbox parse command {{ match Go() {{ {body} }} }}"
        );
        let program = Arc::new(grmpl_lang::Program::compile(&source, 1).unwrap());
        let error = match grmpl_lang::Program::behavior(&program, "inbox", Entity(1)) {
            Ok(_) => panic!("`{body}` unexpectedly type-checked"),
            Err(error) => error,
        };
        assert!(error.contains(expected), "`{body}`: {error}");
    }
}

#[test]
fn non_finite_float_faults_without_consuming_the_message() {
    let source = r#"
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel result(value: Float)
form command { "overflow" -> Overflow() }
on inbox parse command {
    match Overflow() {
        let impossible = 1e308 * 1e308
        assert result(impossible)
    }
}
"#;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir.path()).unwrap());
    let runtime = Runtime::compile(Arc::clone(&store), source, 100).unwrap();
    let inbox = runtime.relation("inbox").unwrap();
    let cursor = runtime.relation("cursor").unwrap();
    let result = runtime.relation("result").unwrap();
    let actor = Entity(8);
    let process = runtime
        .process(
            actor,
            Authority::new(
                DomainId(1),
                vec![Scope::whole(cursor), Scope::whole(result)],
            ),
            "inbox",
            "cursor",
        )
        .unwrap();
    enqueue(
        store.as_ref(),
        inbox,
        actor,
        0,
        Tuple::from([Value::text("overflow")]),
    )
    .unwrap();

    let before = store.current();
    let error = process
        .step(store.as_ref(), store.as_ref())
        .unwrap_err()
        .to_string();
    assert!(error.contains("actor 8 inbox sequence 0"), "{error}");
    assert!(error.contains("arithmetic fault in `mul`"), "{error}");
    assert_eq!(store.current(), before);
    assert_eq!(process.position(store.as_ref()).unwrap(), 0);
    assert!(store.read_at(result, store.current()).unwrap().is_empty());
}

#[test]
fn float_behavior_results_survive_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let result = {
        let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir.path()).unwrap());
        let runtime = Runtime::compile(Arc::clone(&store), SOURCE, 100).unwrap();
        let inbox = runtime.relation("inbox").unwrap();
        let cursor = runtime.relation("cursor").unwrap();
        let result = runtime.relation("result").unwrap();
        let actor = Entity(9);
        let process = runtime
            .process(
                actor,
                Authority::new(
                    DomainId(1),
                    vec![Scope::whole(cursor), Scope::whole(result)],
                ),
                "inbox",
                "cursor",
            )
            .unwrap();
        enqueue(
            store.as_ref(),
            inbox,
            actor,
            0,
            Tuple::from([Value::text("calc")]),
        )
        .unwrap();
        process
            .step(store.as_ref(), store.as_ref())
            .unwrap()
            .unwrap();
        result
    };

    let reopened = EntStore::open(dir.path()).unwrap();
    let rows = reopened.read_at(result, reopened.current()).unwrap();
    assert!(rows.contains(&(
        Tuple::from([
            Value::Int(7),
            Value::float(f64::from_bits(0x3fd3_3333_3333_3334)).unwrap(),
            Value::Bool(true),
        ]),
        1,
    )));
}
