//! M6 acceptance (DESIGN.md §6, §9): `take lamp` runs end to end, and a
//! `watch visible` observer sees the `- lamp` delta.
//!
//! This wires all four surface mechanisms to the core:
//!   * `form`  — a `Pattern`/`Form` parses the token line into a command value;
//!   * `view`  — `visible` is an ordinary relational `Query`;
//!   * noun resolution is a `find` over that view;
//!   * the take is a guarded `Patch`, committed optimistically;
//!   * `on`    — a `Process` drives parse → resolve → patch → commit;
//!   * reactivity — a `DeltaStream` on `visible` observes the retraction.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, Message, Patch, RelId, Result, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_core::NoSchemas;
use grmpl_diff::{Query, Snapshot};
use grmpl_pattern::{Bindings, Form, Pattern, Rule, VarId};
use grmpl_proc::{enqueue, Behavior, CommitOutcome, Process};
use grmpl_store::FjallStore;

// ---- schema --------------------------------------------------------------
const LOCATED: RelId = RelId(1); //  (thing, place)
const NAMED: RelId = RelId(2); //    (thing, name)
const HELD: RelId = RelId(5); //     (owner, thing)
const PERMITS: RelId = RelId(6); //  (viewer, verb, thing)
const INBOX: RelId = RelId(10); //   player command inbox
const CURSOR: RelId = RelId(11);
const TELL: RelId = RelId(20); //    outbound "tell" messages: (player, text)

const LAMP: Entity = Entity(100);
const ROOM: Entity = Entity(7);
const PLAYER: Entity = Entity(1); // the actor issuing "take lamp"
const BYSTANDER: Entity = Entity(2); // the observer watching `visible`

fn ent(e: Entity) -> Value {
    Value::Ent(e)
}

// ---- view visible(viewer) ------------------------------------------------
// visible(viewer) := located(viewer,@room), located(@thing,@room),
//                    named(@thing,@name), permits(viewer, see, @thing)
//                    yield { thing, name }
fn visible(viewer: Entity) -> Query {
    let viewer_room = Query::rel(LOCATED)
        .filter(move |t| t.as_slice()[0] == ent(viewer))
        .project([1]); // (room)
    let things_here = viewer_room
        .join(Query::rel(LOCATED), [0], [1])
        .project([1]); // (thing) — things sharing the viewer's room
    let named_things = things_here
        .join(Query::rel(NAMED), [0], [0])
        .project([0, 2]); // (thing, name)
    let permitted = Query::rel(PERMITS)
        .filter(move |t| t.as_slice()[0] == ent(viewer) && t.as_slice()[1] == Value::text("see"))
        .project([2]); // (thing)
    named_things
        .join(permitted, [0], [0])
        .project([0, 1])
        .distinct() // (thing, name)
}

// ---- form command --------------------------------------------------------
fn command_form() -> Form {
    let take = Rule::new(
        Pattern::Seq(vec![Pattern::Lit(Value::text("take")), Pattern::Bind(VarId(0))]),
        |b: &Bindings| {
            let name = b.get(&VarId(0)).cloned().unwrap_or(Value::text(""));
            Value::Tuple(Arc::from(vec![Value::text("Take"), name]))
        },
    );
    let look = Rule::new(Pattern::Lit(Value::text("look")), |_b| {
        Value::Tuple(Arc::from(vec![Value::text("Look")]))
    });
    Form::new(vec![take, look])
}

// A noun token matches a name if it equals the name or one of its words.
fn name_matches(full: &Value, token: &Value) -> bool {
    match (full, token) {
        (Value::Text(f), Value::Text(t)) => {
            f.as_ref() == t.as_ref() || f.split_whitespace().any(|w| w == t.as_ref())
        }
        _ => full == token,
    }
}

// ---- on player.inbox -----------------------------------------------------
fn player_behavior(viewer: Entity) -> Behavior {
    let form = command_form();
    Box::new(move |snap: &Snapshot, body: &Tuple| -> Result<Patch> {
        // Parse the command line (Pattern law).
        let cmds = form.parse_all(body.as_slice());
        let cmd = match cmds.first() {
            Some(c) => c.clone(),
            None => return Ok(Patch::new()), // unparseable: no effect
        };
        let parts = match &cmd {
            Value::Tuple(p) => p.clone(),
            _ => return Ok(Patch::new()),
        };
        if parts[0] != Value::text("Take") {
            return Ok(Patch::new()); // Look etc. — not exercised here
        }
        let target_name = parts[1].clone();

        // Resolve the noun as a query over `visible` (not heap navigation).
        // A noun matches if it is a word of the thing's name ("lamp" ~ "brass lamp").
        let rows = visible(viewer).find(snap)?;
        let thing = rows
            .iter()
            .find(|(t, _)| name_matches(&t.as_slice()[1], &target_name))
            .map(|(t, _)| t.as_slice()[0].clone());
        let thing = match thing {
            Some(Value::Ent(e)) => e,
            _ => {
                // Not visible: a reply, no world change.
                return Ok(Patch::new().emit(Message {
                    inbox: TELL,
                    body: Tuple::from([ent(viewer), Value::text("You don't see that here.")]),
                }));
            }
        };

        // Its place (the viewer's room) — needed for the precondition.
        let place = Query::rel(LOCATED)
            .filter(move |t| t.as_slice()[0] == ent(thing))
            .project([1])
            .find(snap)?;
        let room = match place.first() {
            Some((t, _)) => t.as_slice()[0].clone(),
            None => return Ok(Patch::new()),
        };

        // The guarded take: expect it here, move it to the player, tell them.
        let here = Fact::new(LOCATED, Tuple::from([ent(thing), room]));
        Ok(Patch::new()
            .expect(here.clone())
            .retract(here)
            .assert(Fact::new(HELD, Tuple::from([ent(viewer), ent(thing)])))
            .emit(Message {
                inbox: TELL,
                body: Tuple::from([ent(viewer), Value::text("Taken.")]),
            }))
    })
}

fn player_process(viewer: Entity) -> Process {
    Process {
        entity: viewer,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(LOCATED), Scope::whole(HELD), Scope::whole(CURSOR)],
        ),
        inbox: INBOX,
        cursor_rel: CURSOR,
        behavior: player_behavior(viewer),
    }
}

fn seed_world(store: &FjallStore) {
    store
        .commit(&[
            (LOCATED, Tuple::from([ent(LAMP), ent(ROOM)]), 1),
            (LOCATED, Tuple::from([ent(PLAYER), ent(ROOM)]), 1),
            (LOCATED, Tuple::from([ent(BYSTANDER), ent(ROOM)]), 1),
            (NAMED, Tuple::from([ent(LAMP), Value::text("brass lamp")]), 1),
            (PERMITS, Tuple::from([ent(PLAYER), Value::text("see"), ent(LAMP)]), 1),
            (PERMITS, Tuple::from([ent(BYSTANDER), Value::text("see"), ent(LAMP)]), 1),
        ])
        .unwrap();
}

#[test]
fn take_lamp_end_to_end_with_observer() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_world(&store);

    let lamp_named = (Tuple::from([ent(LAMP), Value::text("brass lamp")]), 1i64);

    // The bystander watches `visible` and sees the lamp initially.
    let observer_q = visible(BYSTANDER);
    let mut observer = observer_q.watch(&store, store.current());
    assert_eq!(observer.poll().unwrap(), vec![lamp_named.clone()], "observer sees the lamp");

    // "take lamp" arrives in the player's inbox.
    enqueue(
        &store,
        INBOX,
        PLAYER,
        0,
        Tuple::from([Value::text("take"), Value::text("lamp")]),
    )
    .unwrap();

    // The player process runs it: parse → resolve → commit.
    let player = player_process(PLAYER);
    let outcome = player.step(&store, &NoSchemas).unwrap();
    assert!(matches!(outcome, Some(CommitOutcome::Committed(_))), "the take committed");

    let cur = store.current();

    // World effects: lamp left the room, the player holds it, "Taken." was told.
    let in_room = store
        .read_at(LOCATED, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([ent(LAMP), ent(ROOM)]));
    assert!(!in_room, "lamp is no longer located in the room");

    let held = store.read_at(HELD, cur).unwrap();
    assert_eq!(held, vec![(Tuple::from([ent(PLAYER), ent(LAMP)]), 1)]);

    let told = store.read_at(TELL, cur).unwrap();
    assert_eq!(told, vec![(Tuple::from([ent(PLAYER), Value::text("Taken.")]), 1)]);

    // Reactivity: the observer's maintained `visible` emits the retraction.
    let delta = observer.poll().unwrap();
    assert_eq!(delta, vec![(lamp_named.0.clone(), -1)], "observer sees `- lamp`");

    // And a fresh evaluation confirms the observer now sees nothing.
    assert!(observer_q.find(&Snapshot::at_current(&store)).unwrap().is_empty());
}

#[test]
fn taking_something_not_present_replies_without_changing_the_world() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_world(&store);

    // Ask for a "sword" that isn't here.
    enqueue(
        &store,
        INBOX,
        PLAYER,
        0,
        Tuple::from([Value::text("take"), Value::text("sword")]),
    )
    .unwrap();

    let player = player_process(PLAYER);
    player.step(&store, &NoSchemas).unwrap();

    let cur = store.current();
    // The lamp is untouched; a "don't see that" reply was told.
    assert!(store
        .read_at(LOCATED, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([ent(LAMP), ent(ROOM)])));
    assert!(store.read_at(HELD, cur).unwrap().is_empty());
    let told = store.read_at(TELL, cur).unwrap();
    assert_eq!(told, vec![(Tuple::from([ent(PLAYER), Value::text("You don't see that here.")]), 1)]);
}
