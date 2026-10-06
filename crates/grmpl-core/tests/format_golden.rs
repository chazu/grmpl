//! **Golden fixtures for the wire format.** "Bump `FORMAT_VERSION` on any
//! change to the tag set or framing" would otherwise hold by discipline alone:
//! a round-trip test cannot see an encoding that changed on both sides at
//! once. This one encodes a fixed corpus (every value tag, a tuple, a message,
//! a schema of every column type, and the content hash) and compares it with
//! `tests/golden/v{FORMAT_VERSION}.txt`.
//!
//! Change the encoding under the same version and it fails; bump the version
//! and it asks for a new file, leaving the old ones as the format's history.
//! `GRMPL_BLESS=1` writes the file instead of comparing.

use std::fmt::Write as _;
use std::path::PathBuf;

use grmpl_core::schema::{Column, Schema, Ty};
use grmpl_core::wire::{self, FORMAT_VERSION};
use grmpl_core::{sha256, Entity, Message, RelId, Tuple, Value};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    wire::encode_value(v, &mut out);
    out
}

/// One line per artifact, `name: <hex>`, in a fixed order.
fn corpus() -> String {
    let nested = Value::Tuple(
        [Value::Ent(Entity(7)), Value::Tuple([Value::text("in"), Value::Bool(false)].into())].into(),
    );
    let tuple = Tuple::from([Value::Ent(Entity(1)), Value::Int(-2), Value::text("lamp"), nested.clone()]);
    let message = Message { inbox: RelId(42), body: tuple.clone() };
    let schema = Schema::new(
        [
            ("thing", Ty::Ent),
            ("since", Ty::Int),
            ("ratio", Ty::Float),
            ("label", Ty::Text),
            ("flag", Ty::Bool),
            ("body", Ty::Tuple),
            ("blob", Ty::Bytes),
            ("code", Ty::Code),
            ("free", Ty::Any),
        ]
        .into_iter()
        .map(|(name, ty)| Column::new(name, ty))
        .collect(),
    );
    let mut tuple_bytes = Vec::new();
    wire::encode_tuple(&tuple, &mut tuple_bytes);

    let artifacts: Vec<(&str, Vec<u8>)> = vec![
        ("value.ent", value(&Value::Ent(Entity(0x0102_0304_0506_0708)))),
        ("value.int", value(&Value::Int(-1))),
        ("value.float", value(&Value::float(-1.5).unwrap())),
        ("value.float.zero", value(&Value::float(-0.0).unwrap())),
        ("value.text", value(&Value::text("héllo"))),
        ("value.bool.true", value(&Value::Bool(true))),
        ("value.bool.false", value(&Value::Bool(false))),
        ("value.bytes", value(&Value::bytes([0u8, 1, 0xfe, 0xff]))),
        ("value.code", value(&Value::code([9u8, 8, 7]))),
        ("value.tuple", value(&nested)),
        ("value.tuple.empty", value(&Value::Tuple([].into()))),
        ("tuple", tuple_bytes),
        ("message", wire::encode_message(&message)),
        ("schema", wire::encode_schema(&schema)),
        ("schema.empty", wire::encode_schema(&Schema::new(vec![]))),
        ("sha256.empty", sha256(b"").to_vec()),
        ("sha256.grmpl", sha256(b"grmpl: a differential, relational substrate").to_vec()),
    ];
    let mut out = format!("format: {FORMAT_VERSION}\n");
    for (name, bytes) in artifacts {
        writeln!(out, "{name}: {}", hex(&bytes)).unwrap();
    }
    out
}

#[test]
fn the_encoding_is_the_one_this_format_version_fixed() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/v{FORMAT_VERSION}.txt"));
    let got = corpus();
    if std::env::var_os("GRMPL_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let Ok(want) = std::fs::read_to_string(&path) else {
        panic!("no golden file for format v{FORMAT_VERSION}: bless it with GRMPL_BLESS=1");
    };
    for (want, got) in want.lines().zip(got.lines()) {
        assert_eq!(
            got, want,
            "encoding changed under the same FORMAT_VERSION: bump it (see CLAUDE.md), then bless with GRMPL_BLESS=1"
        );
    }
    assert_eq!(
        got.lines().count(),
        want.lines().count(),
        "encoding changed under the same FORMAT_VERSION: bump it (see CLAUDE.md), then bless with GRMPL_BLESS=1"
    );
}
