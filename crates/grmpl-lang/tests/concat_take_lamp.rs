//! # P11 capstone: `take lamp`, written concatenatively
//!
//! The same handler as `grmpl-proc/tests/take_lamp.rs`, but its `on` arm is a
//! **point-free word body** (`match Take(noun) [ … ]`) instead of the v1
//! statement body (`match Take(noun) { … }`). The two surfaces coexist over one
//! CBPV core and lower to the same effect primitives, so — the acceptance
//! criterion — driving the identical trace produces **identical editions**:
//! same commit clock, same world, byte for byte.
//!
//! The relations, `view`, and `form` are untouched between the two programs:
//! **view bodies keep their named logic variables**; only the effect seam goes
//! point-free.

use std::sync::Arc;

use grmpl_core::NoSchemas;
use grmpl_core::{
    Authority, Diff, DomainId, EditionStore, Entity, RelId, Scope, TraceStore, Tuple, Value,
};
use grmpl_lang::Program;
use grmpl_proc::{enqueue, CommitOutcome, Process};
use grmpl_store::FjallStore;

/// Shared preamble: relations, the `visible` view, and the `command` form. Both
/// programs below embed this verbatim — the point-free change is confined to the
/// `on` handler.
const PREAMBLE: &str = r#"
    rel located(thing, place)
    rel named(thing, name)
    rel permits(viewer, verb, thing)
    rel held(owner, thing)
    rel tell(player, text)
    rel inbox(process, seq, body)
    rel cursor(process, pos)

    view visible(viewer) {
        located(viewer, room)
        located(thing, room)
        named(thing, name)
        permits(viewer, "see", thing)
        yield thing, name
    }

    form command {
        "take" noun -> Take(noun)
        "look"      -> Look()
    }
"#;

/// v1 statement surface.
fn v1_src() -> String {
    format!(
        "{PREAMBLE}
        on inbox parse command {{
            match Take(noun) {{
                resolve visible(self) where name ~ noun
                find located(thing, room)
                expect located(thing, room)
                retract located(thing, room)
                assert held(self, thing)
                emit tell(self, \"Taken.\")
            }}
        }}
        "
    )
}

/// P11 concatenative surface. Reads bottom-to-top like a stack machine:
/// `self swap resolve …` pushes the viewer under the noun and resolves it to
/// the lamp; `dup find located 1` looks the lamp's room up; `dup2 expect …`
/// keeps a copy of `(thing room)` so the same fact can be both a precondition
/// and a retraction; the tail asserts `held` and emits the reply.
fn concat_src() -> String {
    format!(
        "{PREAMBLE}
        on inbox parse command {{
            match Take(noun) [
                self swap resolve visible name ~
                drop
                dup find located 1
                dup2 expect located
                retract located
                self swap assert held
                self \"Taken.\" emit tell
            ]
        }}
        "
    )
}

const LAMP: Entity = Entity(100);
const ROOM: Entity = Entity(7);
const PLAYER: Entity = Entity(1);

fn e(x: Entity) -> Value {
    Value::Ent(x)
}

struct World {
    prog: Arc<Program>,
    located: RelId,
    named: RelId,
    permits: RelId,
    held: RelId,
    tell: RelId,
    inbox: RelId,
    cursor: RelId,
}

impl World {
    fn rels(&self) -> [RelId; 7] {
        [
            self.located,
            self.named,
            self.permits,
            self.held,
            self.tell,
            self.inbox,
            self.cursor,
        ]
    }
}

/// Compile `src`, seed the identical starting world, enqueue `take lamp`, and
/// run one step of the player process. Returns the world and store so the
/// caller can read the resulting editions.
fn run(src: &str) -> (World, FjallStore, tempfile::TempDir) {
    let prog = Arc::new(Program::compile(src, 1).unwrap());
    let rid = |n: &str| prog.rel_id(n).unwrap();
    let w = World {
        located: rid("located"),
        named: rid("named"),
        permits: rid("permits"),
        held: rid("held"),
        tell: rid("tell"),
        inbox: rid("inbox"),
        cursor: rid("cursor"),
        prog,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (w.located, Tuple::from([e(LAMP), e(ROOM)]), 1),
            (w.located, Tuple::from([e(PLAYER), e(ROOM)]), 1),
            (w.named, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
            (w.permits, Tuple::from([e(PLAYER), Value::text("see"), e(LAMP)]), 1),
        ])
        .unwrap();

    enqueue(
        &store,
        w.inbox,
        PLAYER,
        0,
        Tuple::from([Value::text("take"), Value::text("lamp")]),
    )
    .unwrap();

    let behavior = Program::behavior(&w.prog, "inbox", PLAYER).unwrap();
    let player = Process {
        entity: PLAYER,
        authority: Authority::new(
            DomainId(1),
            vec![
                Scope::whole(w.located),
                Scope::whole(w.held),
                Scope::whole(w.cursor),
            ],
        ),
        inbox: w.inbox,
        cursor_rel: w.cursor,
        behavior,
    };
    let outcome = player.step(&store, &NoSchemas).unwrap();
    assert!(matches!(outcome, Some(CommitOutcome::Committed(_))));

    (w, store, dir)
}

fn snapshot(w: &World, store: &FjallStore) -> Vec<(RelId, Vec<(Tuple, Diff)>)> {
    let cur = store.current();
    w.rels()
        .into_iter()
        .map(|r| (r, store.read_at(r, cur).unwrap()))
        .collect()
}

#[test]
fn concatenative_take_lamp_matches_v1_editions() {
    let (wc, sc, _dc) = run(&concat_src());
    let (wv, sv, _dv) = run(&v1_src());

    // Same commit clock: the concatenative handler allocated exactly the
    // editions the statement handler did.
    assert_eq!(sc.current(), sv.current());

    // Same world, relation by relation: the lamp left the room, the player
    // holds it, and "Taken." was told — identical facts at identical editions.
    assert_eq!(snapshot(&wc, &sc), snapshot(&wv, &sv));

    // And spot-check the actual outcome, so a doubly-empty world can't pass.
    let cur = sc.current();
    assert_eq!(
        sc.read_at(wc.held, cur).unwrap(),
        vec![(Tuple::from([e(PLAYER), e(LAMP)]), 1)]
    );
    assert_eq!(
        sc.read_at(wc.tell, cur).unwrap(),
        vec![(Tuple::from([e(PLAYER), Value::text("Taken.")]), 1)]
    );
    assert!(!sc
        .read_at(wc.located, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)])));
}

#[test]
fn concatenative_absent_thing_makes_no_change() {
    // A `resolve` miss aborts the point-free arm with no change, exactly as the
    // statement surface does.
    let prog = Arc::new(Program::compile(&concat_src(), 1).unwrap());
    let rid = |n: &str| prog.rel_id(n).unwrap();
    let (located, held, tell, inbox, cursor) = (
        rid("located"),
        rid("held"),
        rid("tell"),
        rid("inbox"),
        rid("cursor"),
    );
    let named = rid("named");
    let permits = rid("permits");

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (located, Tuple::from([e(LAMP), e(ROOM)]), 1),
            (located, Tuple::from([e(PLAYER), e(ROOM)]), 1),
            (named, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
            (permits, Tuple::from([e(PLAYER), Value::text("see"), e(LAMP)]), 1),
        ])
        .unwrap();
    enqueue(
        &store,
        inbox,
        PLAYER,
        0,
        Tuple::from([Value::text("take"), Value::text("sword")]),
    )
    .unwrap();

    let behavior = Program::behavior(&prog, "inbox", PLAYER).unwrap();
    let player = Process {
        entity: PLAYER,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(located), Scope::whole(held), Scope::whole(cursor)],
        ),
        inbox,
        cursor_rel: cursor,
        behavior,
    };
    player.step(&store, &NoSchemas).unwrap();

    let cur = store.current();
    // Lamp untouched, nothing held or told.
    assert!(store
        .read_at(located, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)])));
    assert!(store.read_at(held, cur).unwrap().is_empty());
    assert!(store.read_at(tell, cur).unwrap().is_empty());
    // The message was still consumed — re-stepping is idle.
    assert!(player.step(&store, &NoSchemas).unwrap().is_none());
}
