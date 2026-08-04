use std::sync::Arc;

use grmpl_core::{Entity, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_ent::EntStore;
use grmpl_lang::Program;

const PREFIX: &str = r#"
rel inbox(process: Ent, seq: Int, body: Tuple)
rel result(sum: Float, remainder: Float, comparison: Bool, converted: Float)
form command { "go" -> Go() }
"#;

#[test]
fn named_and_concatenative_arithmetic_lower_to_equivalent_patches() {
    let named = format!(
        r#"{PREFIX}
on inbox parse command {{
    match Go() {{
        let sum = 0.1 + 0.2
        let remainder = 5.5 % 2.0
        let comparison = sum > 0.3
        let converted = float(9007199254740993)
        assert result(sum, remainder, comparison, converted)
    }}
}}
"#
    );
    let concatenative = format!(
        r#"{PREFIX}
on inbox parse command {{
    match Go() [
        0.1 0.2 add
        5.5 2.0 rem
        0.1 0.2 add 0.3 gt
        9007199254740993 to_float
        assert result
    ]
}}
"#
    );
    let named = Arc::new(Program::compile(&named, 1).unwrap());
    let concatenative = Arc::new(Program::compile(&concatenative, 1).unwrap());
    let named_behavior = Program::behavior(&named, "inbox", Entity(1)).unwrap();
    let concat_behavior = Program::behavior(&concatenative, "inbox", Entity(1)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let snapshot = Snapshot::at_current(&store);
    let message = Tuple::from([Value::text("go")]);
    let named_patch = named_behavior(&snapshot, &message).unwrap();
    let concat_patch = concat_behavior(&snapshot, &message).unwrap();
    assert_eq!(named_patch, concat_patch);
    let tuple = &named_patch.asserts[0].tuple;
    assert_eq!(
        tuple.as_slice()[0],
        Value::float(f64::from_bits(0x3fd3_3333_3333_3334)).unwrap()
    );
    assert_eq!(tuple.as_slice()[1], Value::float(1.5).unwrap());
    assert_eq!(
        tuple.as_slice()[3],
        Value::float(f64::from_bits(0x4340_0000_0000_0000)).unwrap()
    );
}

#[test]
fn concatenative_scalar_words_are_type_checked() {
    let source = format!(
        r#"{PREFIX}
on inbox parse command {{
    match Go() [ 1 1.0 add drop ]
}}
"#
    );
    let program = Arc::new(Program::compile(&source, 1).unwrap());
    let error = match Program::behavior(&program, "inbox", Entity(1)) {
        Ok(_) => panic!("mixed arithmetic unexpectedly compiled"),
        Err(error) => error,
    };
    assert!(error.contains("same Int or Float"), "{error}");
}
