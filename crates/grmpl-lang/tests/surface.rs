//! The text surface compiles to the same core constructors used by hand: a
//! `view` becomes a relational `Query` (joins + project + distinct), and a
//! `form` becomes a `Pattern`-based parser.

use std::sync::Arc;

use grmpl_core::{Diff, Entity, TraceStore, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_lang::Program;
use grmpl_store::FjallStore;

const SRC: &str = r#"
    // relations
    rel located(thing, place)
    rel named(thing, name)
    rel permits(viewer, verb, thing)

    // the MOO "visible objects" facade, as a relational view
    view visible(viewer) {
        located(viewer, room)
        located(thing, room)
        named(thing, name)
        permits(viewer, "see", thing)
        yield thing, name
    }

    // a command grammar
    form command {
        "take" name -> Take(name)
        "look"      -> Look()
    }
"#;

const LAMP: Entity = Entity(100);
const ROOM: Entity = Entity(7);
const P: Entity = Entity(1);
const Q: Entity = Entity(2);

fn e(x: Entity) -> Value {
    Value::Ent(x)
}

fn find_sorted(q: &grmpl_diff::Query, store: &FjallStore) -> Vec<(Tuple, Diff)> {
    q.find(&Snapshot::at_current(store)).unwrap()
}

#[test]
fn view_compiles_to_a_relational_query() {
    let prog = Program::compile(SRC, 1).unwrap();
    let located = prog.rel_id("located").unwrap();
    let named = prog.rel_id("named").unwrap();
    let permits = prog.rel_id("permits").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (located, Tuple::from([e(LAMP), e(ROOM)]), 1),
            (located, Tuple::from([e(P), e(ROOM)]), 1),
            (located, Tuple::from([e(Q), e(ROOM)]), 1),
            (named, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
            // Only Q is permitted to see the lamp.
            (permits, Tuple::from([e(Q), Value::text("see"), e(LAMP)]), 1),
        ])
        .unwrap();

    // visible(Q) yields the lamp with its name.
    let vis_q = prog.view("visible", &[e(Q)]).unwrap();
    assert_eq!(
        find_sorted(&vis_q, &store),
        vec![(Tuple::from([e(LAMP), Value::text("brass lamp")]), 1)]
    );

    // visible(P) is empty — P has no `see` permission (parameter + literal filter).
    let vis_p = prog.view("visible", &[e(P)]).unwrap();
    assert!(find_sorted(&vis_p, &store).is_empty());

    // Remove the lamp from the room: the recursive-free join now yields nothing.
    store.commit(&[(located, Tuple::from([e(LAMP), e(ROOM)]), -1)]).unwrap();
    assert!(find_sorted(&vis_q, &store).is_empty());
}

#[test]
fn form_compiles_to_a_parser() {
    let prog = Program::compile(SRC, 1).unwrap();
    let command = prog.form("command").unwrap();

    // "take lamp" → Take(lamp)
    let taken = command.parse_all(&[Value::text("take"), Value::text("lamp")]);
    assert_eq!(
        taken,
        vec![Value::Tuple(Arc::from(vec![Value::text("Take"), Value::text("lamp")]))]
    );

    // "look" → Look()
    let looked = command.parse_all(&[Value::text("look")]);
    assert_eq!(looked, vec![Value::Tuple(Arc::from(vec![Value::text("Look")]))]);

    // Garbage does not parse.
    assert!(command.parse_all(&[Value::text("frobnicate")]).is_empty());
}

#[test]
fn parse_errors_are_reported() {
    // Lexical/syntactic errors surface at compile time.
    assert!(Program::compile("rel located(", 1).is_err()); // unterminated
    assert!(Program::compile("view v() { located(a, b) }", 1).is_err()); // no yield

    // Semantic errors surface at instantiation (compile stores the AST).
    let prog = Program::compile("rel r(a, b)\nview v(x) { r(a, b) yield c }", 1).unwrap();
    assert!(prog.view("v", &[Value::Int(0)]).is_err()); // yields unbound `c`
    assert!(prog.view("v", &[]).is_err()); // wrong arity
}
