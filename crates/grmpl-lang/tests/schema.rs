//! P1 (TKT-73): the typed `rel` surface grammar, named-column resolution, and
//! the bridge from a compiled program to the durable schema registry.
//!
//! * `rel r(a: Ty, b)` parses into named, typed columns; an unannotated column
//!   defaults to `Ty::Any`.
//! * `Program::resolve_column` turns a column *name* into a tuple index.
//! * `Program::register_schemas` records each relation's schema into the
//!   durable registry, after which the commit boundary enforces arity/type.

use grmpl_core::{
    Authority, Column, Entity, Error, Fact, Patch, Schema, Scope, Tuple, Ty, Value,
};
use grmpl_lang::Program;
use grmpl_proc::{commit_patch, CommitOutcome};

/// `Program` intentionally has no `Debug`, so `unwrap_err` is unavailable —
/// recover the error string directly.
fn compile_err(src: &str) -> String {
    match Program::compile(src, 1) {
        Err(e) => e,
        Ok(_) => panic!("expected `{src}` to fail compilation"),
    }
}

#[test]
fn typed_rel_grammar_lowers_to_named_typed_columns() {
    let prog = Program::compile(
        "rel located(thing: Ent, place: Ent)\nrel score(who: Ent, points: Int)",
        1,
    )
    .unwrap();

    assert_eq!(
        prog.schema("located"),
        Some(Schema::new(vec![
            Column::new("thing", Ty::Ent),
            Column::new("place", Ty::Ent),
        ]))
    );
    assert_eq!(
        prog.schema("score"),
        Some(Schema::new(vec![
            Column::new("who", Ty::Ent),
            Column::new("points", Ty::Int),
        ]))
    );
}

#[test]
fn untyped_columns_default_to_any() {
    // Backward-compatible: the pre-P1 untyped form still parses.
    let prog = Program::compile("rel located(thing, place)", 1).unwrap();
    assert_eq!(
        prog.schema("located"),
        Some(Schema::new(vec![
            Column::new("thing", Ty::Any),
            Column::new("place", Ty::Any),
        ]))
    );
}

#[test]
fn mixed_annotation_is_allowed() {
    let prog = Program::compile("rel edge(from: Ent, to: Ent, weight)", 1).unwrap();
    let cols = prog.rel_columns("edge").unwrap();
    assert_eq!(cols[0].ty, Ty::Ent);
    assert_eq!(cols[2].ty, Ty::Any); // unannotated → Any
}

#[test]
fn unknown_type_annotation_is_a_compile_error() {
    assert!(compile_err("rel r(a: Widget)").contains("unknown type"));
}

#[test]
fn duplicate_column_name_is_a_compile_error() {
    assert!(compile_err("rel r(a: Int, a: Int)").contains("duplicate column"));
}

#[test]
fn resolve_column_maps_name_to_index() {
    let prog = Program::compile("rel located(thing: Ent, place: Ent)", 1).unwrap();
    assert_eq!(prog.resolve_column("located", "thing"), Some(0));
    assert_eq!(prog.resolve_column("located", "place"), Some(1));
    assert_eq!(prog.resolve_column("located", "nope"), None);
    assert_eq!(prog.resolve_column("missing", "thing"), None);
}

#[test]
fn register_schemas_then_commit_boundary_enforces_types() {
    grmpl_conformance::for_each_store(|c| {
        let prog =
            Program::compile("rel located(thing: Ent, place: Ent)", 1).unwrap();
        let located = prog.rel_id("located").unwrap();

        let store = c.world();
        // Register name→id and the schema into the durable registry as-of edition 1.
        prog.register_schemas(store, store, grmpl_core::Edition(1)).unwrap();

        // The catalog and schema registry now agree with the program.
        assert_eq!(grmpl_core::Catalog::rel_id(store, "located").unwrap(), Some(located));
        assert_eq!(prog.schema("located"), grmpl_core::SchemaCatalog::schema(store, located).unwrap());

        let authority = Authority::new(grmpl_core::DomainId(1), vec![Scope::whole(located)]);

        // Well-typed write commits.
        let good = Patch::new().assert(Fact::new(
            located,
            Tuple::from([Value::Ent(Entity(100)), Value::Ent(Entity(7))]),
        ));
        assert!(matches!(
            commit_patch(store, store, &good, &authority).unwrap(),
            CommitOutcome::Committed(_)
        ));

        // Ill-typed write (place must be Ent) is rejected at the boundary.
        let bad = Patch::new().assert(Fact::new(
            located,
            Tuple::from([Value::Ent(Entity(100)), Value::text("hallway")]),
        ));
        let err = commit_patch(store, store, &bad, &authority).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "got {err:?}");
    });
}
