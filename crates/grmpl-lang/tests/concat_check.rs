//! The "declared stack effects first" gate: a concatenative arm's cell
//! arithmetic is checked at compile time (`Program::compile`), before any
//! handler runs. Underflow, a non-empty leftover stack, and words naming
//! undeclared relations/views are all rejected up front; a balanced body
//! compiles.

use grmpl_lang::Program;

const PREAMBLE: &str = r#"
    rel located(thing, place)
    rel held(owner, thing)
    rel named(thing, name)
    rel permits(viewer, verb, thing)
    view visible(viewer) {
        located(viewer, room)
        located(thing, room)
        named(thing, name)
        permits(viewer, "see", thing)
        yield thing, name
    }
    form command { "take" noun -> Take(noun) }
"#;

fn compile_on(body: &str) -> Result<Program, String> {
    Program::compile(&format!("{PREAMBLE}\non inbox parse command {{ {body} }}\n"), 1)
}

#[test]
fn a_balanced_body_compiles() {
    // ( noun -- ): resolve consumes self+noun and yields (thing name); drop both,
    // then self+self into a held assertion. Net effect ( noun -- ).
    let prog = compile_on(
        "match Take(noun) [ self swap resolve visible name ~ drop self swap assert held ]",
    );
    assert!(prog.is_ok(), "expected a balanced arm to compile: {:?}", prog.err());
}

#[test]
fn underflow_is_rejected() {
    // `assert held` needs two cells; the arm supplies only `noun`.
    let err = compile_on("match Take(noun) [ assert held ]").err().expect("expected a compile error");
    assert!(err.contains("underflow"), "got: {err}");
}

#[test]
fn leftover_stack_is_rejected() {
    // Pushes `self` and never consumes it — the stack is not empty at the end.
    let err = compile_on("match Take(noun) [ self ]").err().expect("expected a compile error");
    assert!(
        err.contains("leaves") && err.contains("stack"),
        "got: {err}"
    );
}

#[test]
fn an_undeclared_relation_word_is_rejected() {
    let err = compile_on("match Take(noun) [ drop self self assert ghost ]").err().expect("expected a compile error");
    assert!(err.contains("ghost"), "got: {err}");
}

#[test]
fn an_undeclared_view_word_is_rejected() {
    let err =
        compile_on("match Take(noun) [ self swap resolve nowhere name ~ drop2 ]").err().expect("expected a compile error");
    assert!(err.contains("nowhere"), "got: {err}");
}

#[test]
fn statement_and_word_arms_coexist_in_one_handler() {
    // A handler with both an idle statement arm and a point-free word arm.
    let prog = compile_on(
        "match Look() { }
         match Take(noun) [ self swap resolve visible name ~ drop2 ]",
    );
    // Take arm: resolve yields (thing name); drop2 clears them. Net ( noun -- ).
    assert!(prog.is_ok(), "mixed-surface handler should compile: {:?}", prog.err());
}
