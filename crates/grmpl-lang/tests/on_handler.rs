//! The capstone: `take lamp` written entirely in the text surface — `rel`,
//! `view`, `form`, and now `on` — compiled to a `Behavior` and driven through a
//! real `Process`. This closes the loop from DESIGN.md §6/§9: every surface
//! mechanism lowers to the same core, no hand-written Rust behavior required.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, RelId, Scope, TraceStore, Tuple, Value,
};
use grmpl_lang::Program;
use grmpl_proc::{enqueue, CommitOutcome, Process};
use grmpl_store::FjallStore;

const SRC: &str = r#"
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

    on inbox parse command {
        match Take(noun) {
            resolve visible(self) where name ~ noun
            find located(thing, room)
            expect located(thing, room)
            retract located(thing, room)
            assert held(self, thing)
            emit tell(self, "Taken.")
        }
    }
"#;

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

fn setup() -> (World, FjallStore, tempfile::TempDir) {
    let prog = Arc::new(Program::compile(SRC, 1).unwrap());
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
    (w, store, dir)
}

fn player_process(w: &World) -> Process {
    let behavior = Program::behavior(&w.prog, "inbox", PLAYER).unwrap();
    Process {
        entity: PLAYER,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(w.located), Scope::whole(w.held), Scope::whole(w.cursor)],
        ),
        inbox: w.inbox,
        cursor_rel: w.cursor,
        behavior,
    }
}

#[test]
fn take_lamp_defined_entirely_in_text() {
    let (w, store, _dir) = setup();
    enqueue(&store, w.inbox, PLAYER, 0, Tuple::from([Value::text("take"), Value::text("lamp")])).unwrap();

    let player = player_process(&w);
    let outcome = player.step(&store).unwrap();
    assert!(matches!(outcome, Some(CommitOutcome::Committed(_))));

    let cur = store.current();
    // Lamp left the room, player holds it, "Taken." was told — all from text.
    assert!(!store
        .read_at(w.located, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)])));
    assert_eq!(store.read_at(w.held, cur).unwrap(), vec![(Tuple::from([e(PLAYER), e(LAMP)]), 1)]);
    assert_eq!(
        store.read_at(w.tell, cur).unwrap(),
        vec![(Tuple::from([e(PLAYER), Value::text("Taken.")]), 1)]
    );
}

#[test]
fn taking_absent_thing_makes_no_change() {
    let (w, store, _dir) = setup();
    enqueue(&store, w.inbox, PLAYER, 0, Tuple::from([Value::text("take"), Value::text("sword")])).unwrap();

    let player = player_process(&w);
    player.step(&store).unwrap();

    let cur = store.current();
    // The lamp is untouched; nothing was held or told (resolve found nothing).
    assert!(store
        .read_at(w.located, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)])));
    assert!(store.read_at(w.held, cur).unwrap().is_empty());
    assert!(store.read_at(w.tell, cur).unwrap().is_empty());
    // But the message was consumed (cursor advanced), so re-stepping is idle.
    assert!(player.step(&store).unwrap().is_none());
}
