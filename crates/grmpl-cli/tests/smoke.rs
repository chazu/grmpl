//! Smoke tests: the bundled world compiles and its surface pieces resolve, and
//! a `take` verb actually mutates the world through the compiled behavior. These
//! guard `worlds/moo.grmpl` against drifting out of sync with the language.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, NoSchemas, Scope, TraceStore, Tuple, Value,
};
use grmpl_lang::Program;
use grmpl_proc::{enqueue, CommitOutcome, Process};
use grmpl_store::FjallStore;

const MOO: &str = include_str!("../../../worlds/moo.grmpl");

#[test]
fn builtin_world_compiles_and_exposes_its_surface() {
    let prog = Program::compile(MOO, 1).expect("moo.grmpl must compile");

    // Relations the host binary depends on by name.
    for rel in [
        "located", "named", "held", "exits", "players", "value", "person", "label", "knows",
        "patrol", "card", "cardval", "cardrank", "slotpair", "slottrio", "sum15two", "sum15three",
        "tell", "inbox", "cursor", "wmail", "wcursor", "wseq",
    ] {
        assert!(prog.rel_id(rel).is_some(), "missing `rel {rel}`");
    }

    // Views resolve (parameterized and nullary, including aggregates + scoring).
    for (v, args) in [
        ("here", &[Value::Ent(Entity(1))][..]),
        ("ways", &[Value::Ent(Entity(1))]),
        ("folks", &[Value::Ent(Entity(1))]),
        ("acquainted", &[Value::Ent(Entity(1))]),
        ("contents", &[Value::Ent(Entity(1))]),
        ("crib_pairs", &[Value::Ent(Entity(1))]),
        ("crib_fif2", &[Value::Ent(Entity(1))]),
        ("crib_fif3", &[Value::Ent(Entity(1))]),
        ("treasure", &[]),
        ("world", &[]),
    ] {
        assert!(prog.view(v, args).is_ok(), "view {v} did not resolve");
    }

    // The command grammar and the behavior compile.
    assert!(prog.form("command").is_ok());
    assert!(Program::behavior(&Arc::new(prog), "inbox", Entity(1)).is_ok());
}

#[test]
fn npc_tick_walks_the_patrol() {
    // The cat's whole "AI" is the `Tick` behavior over the `patrol` relation.
    let prog = Arc::new(Program::compile(MOO, 1).unwrap());
    let rid = |n: &str| prog.rel_id(n).unwrap();
    let (located, patrol, inbox, cursor) =
        (rid("located"), rid("patrol"), rid("inbox"), rid("cursor"));
    let (cat, a, b) = (Entity(30), Entity(10), Entity(11));

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (located, Tuple::from([Value::Ent(cat), Value::Ent(a)]), 1),
            (patrol, Tuple::from([Value::Ent(a), Value::Ent(b)]), 1),
        ])
        .unwrap();
    enqueue(&store, inbox, cat, 0, tokens("tick")).unwrap();

    let npc = Process {
        entity: cat,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(located), Scope::whole(cursor)],
        ),
        inbox,
        cursor_rel: cursor,
        behavior: Program::behavior(&prog, "inbox", cat).unwrap(),
    };
    npc.step(&store, &NoSchemas).unwrap();

    // The cat walked one patrol edge, a -> b, on its own.
    let at = store.current();
    assert!(store
        .read_at(located, at)
        .unwrap()
        .contains(&(Tuple::from([Value::Ent(cat), Value::Ent(b)]), 1)));
    assert!(!store
        .read_at(located, at)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([Value::Ent(cat), Value::Ent(a)])));
}

#[test]
fn cribbage_scoring_is_a_view() {
    // A hand of 7,8 (+ filler) has exactly one fifteen (7+8) and no pairs — and
    // that judgement is made by the `crib_fif2` / `crib_pairs` VIEWS, not code.
    let prog = Program::compile(MOO, 1).unwrap();
    let rid = |n: &str| prog.rel_id(n).unwrap();
    let (card, cardval, cardrank, slotpair, sum15two) = (
        rid("card"),
        rid("cardval"),
        rid("cardrank"),
        rid("slotpair"),
        rid("sum15two"),
    );
    let p = Entity(1);
    let i = Value::Int;

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let mut b = vec![
        // codes: 6 -> rank 6 (a "7"), 7 -> rank 7 (an "8"); values 7 and 8.
        (card, Tuple::from([Value::Ent(p), i(0), i(6)]), 1),
        (card, Tuple::from([Value::Ent(p), i(1), i(7)]), 1),
        (card, Tuple::from([Value::Ent(p), i(2), i(0)]), 1), // an ace (rank 0, val 1)
        (card, Tuple::from([Value::Ent(p), i(3), i(1)]), 1), // a two  (rank 1, val 2)
        (card, Tuple::from([Value::Ent(p), i(4), i(11)]), 1), // a queen (rank 11, val 10)
    ];
    // Minimal rule tables the two views need.
    for code in [6i64, 7, 0, 1, 11] {
        b.push((cardval, Tuple::from([i(code), i((code % 13 + 1).min(10))]), 1));
        b.push((cardrank, Tuple::from([i(code), i(code % 13)]), 1));
    }
    for a in 0..5i64 {
        for c in (a + 1)..5 {
            b.push((slotpair, Tuple::from([i(a), i(c)]), 1));
        }
    }
    b.push((sum15two, Tuple::from([i(7), i(8)]), 1));
    b.push((sum15two, Tuple::from([i(8), i(7)]), 1));
    store.commit(&b).unwrap();

    let snap = grmpl_diff::Snapshot::at_current(&store);
    let fif2 = prog.view("crib_fif2", &[Value::Ent(p)]).unwrap().find(&snap).unwrap();
    let pairs = prog.view("crib_pairs", &[Value::Ent(p)]).unwrap().find(&snap).unwrap();
    assert_eq!(fif2.len(), 1, "exactly one two-card fifteen (7+8)");
    assert_eq!(pairs.len(), 0, "no pairs in this hand");
}

#[test]
fn take_verb_moves_a_thing_through_the_compiled_behavior() {
    let prog = Arc::new(Program::compile(MOO, 1).unwrap());
    let rid = |n: &str| prog.rel_id(n).unwrap();
    let (located, named, held, inbox, cursor, tell) = (
        rid("located"),
        rid("named"),
        rid("held"),
        rid("inbox"),
        rid("cursor"),
        rid("tell"),
    );
    let (player, room, lamp) = (Entity(1), Entity(10), Entity(20));

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (located, Tuple::from([Value::Ent(player), Value::Ent(room)]), 1),
            (located, Tuple::from([Value::Ent(lamp), Value::Ent(room)]), 1),
            (named, Tuple::from([Value::Ent(lamp), Value::text("brass lamp")]), 1),
        ])
        .unwrap();

    enqueue(&store, inbox, player, 0, tokens("take lamp")).unwrap();
    let process = Process {
        entity: player,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(located), Scope::whole(held), Scope::whole(cursor), Scope::whole(tell)],
        ),
        inbox,
        cursor_rel: cursor,
        behavior: Program::behavior(&prog, "inbox", player).unwrap(),
    };
    let outcome = process.step(&store, &NoSchemas).unwrap();
    assert!(matches!(outcome, Some(CommitOutcome::Committed(_))));

    let at = store.current();
    // The lamp is now held and no longer on the floor.
    assert!(store
        .read_at(held, at)
        .unwrap()
        .contains(&(Tuple::from([Value::Ent(player), Value::Ent(lamp)]), 1)));
    assert!(!store
        .read_at(located, at)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([Value::Ent(lamp), Value::Ent(room)])));
}

fn tokens(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}
