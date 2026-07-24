//! The world model: relation ids, the command grammar, and the player behavior
//! that lowers verbs to ordinary patches (DESIGN.md §6, P3).
//!
//! Every verb — `dig`, `create`, `take`, `go`, `look` — is a plain guarded
//! `Patch`: it reads the world through relational queries and proposes asserts /
//! retracts. World construction is not a privileged engine operation; it is the
//! same commit machinery as `take lamp`. New entities come from the replay-safe
//! [`Alloc`](grmpl_proc::Alloc) counter.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, Entity, Fact, Message, Patch, RelId, Result, Scope, Tuple, Value,
};
use grmpl_diff::{Query, Snapshot};
use grmpl_pattern::{Bindings, Form, Pattern, Rule, VarId};
use grmpl_proc::{Alloc, Behavior};

// ---- relation layout -----------------------------------------------------
/// `(thing, place)` — where each entity is.
pub const LOCATED: RelId = RelId(1);
/// `(thing, name)` — an entity's display name.
pub const NAMED: RelId = RelId(2);
/// `(owner, thing)` — carried objects.
pub const HELD: RelId = RelId(5);
/// `(from_room, direction, to_room)` — one-way passages.
pub const EXITS: RelId = RelId(7);
/// `(player, name)` — the identity map (minimal auth: a name is an identity).
pub const PLAYER: RelId = RelId(8);
/// Player command inbox `(player, seq, body)`.
pub const INBOX: RelId = RelId(10);
/// Inbox cursor `(player, next_seq)`.
pub const CURSOR: RelId = RelId(11);
/// Outbound text `(player, text)` — what a client is told.
pub const TELL: RelId = RelId(20);
/// Single-row entity-id counter `(next: Int)`.
pub const ENTITY_SEQ: RelId = RelId(30);

/// The starting room every player is spawned into.
pub const ROOT_ROOM: Entity = Entity(1);
/// Allocation begins here, above the fixed well-known entities (the root room).
pub const ID_BASE: i64 = 1000;

/// The authority a player process (and the provisioner) commits under. Players
/// are builders in P3, so this owns every world relation a verb writes,
/// including the id counter it advances. (Scoping authority per player is a
/// later refinement; the optimistic commit still arbitrates concurrent writes.)
pub fn world_authority() -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(LOCATED),
            Scope::whole(NAMED),
            Scope::whole(HELD),
            Scope::whole(EXITS),
            Scope::whole(PLAYER),
            Scope::whole(CURSOR),
            Scope::whole(ENTITY_SEQ),
        ],
    )
}

fn ent(e: Entity) -> Value {
    Value::Ent(e)
}

// ---- the command grammar -------------------------------------------------
// Verbs take a single-token noun (e.g. `create lamp`, `dig hall`, `go hall`).
fn verb1(word: &'static str, tag: &'static str) -> Rule {
    Rule::new(
        Pattern::Seq(vec![Pattern::Lit(Value::text(word)), Pattern::Bind(VarId(0))]),
        move |b: &Bindings| {
            let noun = b.get(&VarId(0)).cloned().unwrap_or(Value::text(""));
            Value::Tuple(Arc::from(vec![Value::text(tag), noun]))
        },
    )
}

/// The MOO-style command grammar as a `Form` — the surface `form` of DESIGN §7,
/// here built programmatically.
pub fn command_form() -> Form {
    Form::new(vec![
        verb1("take", "Take"),
        verb1("create", "Create"),
        verb1("dig", "Dig"),
        verb1("go", "Go"),
        Rule::new(Pattern::Lit(Value::text("look")), |_b| {
            Value::Tuple(Arc::from(vec![Value::text("Look")]))
        }),
    ])
}

fn tell(player: Entity, text: impl AsRef<str>) -> Message {
    Message { inbox: TELL, body: Tuple::from([ent(player), Value::text(text)]) }
}

/// The room `player` is currently in, if any.
fn room_of(snap: &Snapshot, player: Entity) -> Result<Option<Entity>> {
    let rows = Query::rel(LOCATED)
        .filter(move |t| t.as_slice()[0] == ent(player))
        .project([1])
        .find(snap)?;
    Ok(rows.first().and_then(|(t, _)| match t.as_slice().first() {
        Some(Value::Ent(e)) => Some(*e),
        _ => None,
    }))
}

/// A thing named `noun` located in `room` (the least such entity — deterministic).
fn thing_in_room(snap: &Snapshot, room: Entity, noun: &Value) -> Result<Option<Entity>> {
    let here = Query::rel(LOCATED)
        .filter(move |t| t.as_slice()[1] == ent(room))
        .project([0]); // (thing)
    let named = here.join(Query::rel(NAMED), [0], [0]).project([0, 2]); // (thing, name)
    let rows = named.find(snap)?;
    Ok(rows
        .iter()
        .find(|(t, _)| &t.as_slice()[1] == noun)
        .and_then(|(t, _)| match t.as_slice().first() {
            Some(Value::Ent(e)) => Some(*e),
            _ => None,
        }))
}

/// The behavior for a player process: parse the command line and lower the verb
/// to a guarded patch (Replay law — pure in `snap` and `body`).
pub fn player_behavior(player: Entity) -> Behavior {
    let form = command_form();
    Box::new(move |snap: &Snapshot, body: &Tuple| -> Result<Patch> {
        let cmd = match form.parse_all(body.as_slice()).into_iter().next() {
            Some(Value::Tuple(p)) => p,
            _ => return Ok(Patch::new().emit(tell(player, "Huh?"))),
        };
        let verb = cmd.first().cloned().unwrap_or(Value::text(""));
        let noun = cmd.get(1).cloned();

        let room = match room_of(snap, player)? {
            Some(r) => r,
            None => return Ok(Patch::new().emit(tell(player, "You are nowhere."))),
        };

        match verb {
            v if v == Value::text("Look") => look(snap, player, room),

            v if v == Value::text("Create") => {
                let name = noun.unwrap_or(Value::text("thing"));
                let mut alloc = Alloc::from_snapshot(snap, ENTITY_SEQ, ID_BASE)?;
                let obj = alloc.fresh();
                let patch = Patch::new()
                    .assert(Fact::new(NAMED, Tuple::from([ent(obj), name.clone()])))
                    .assert(Fact::new(LOCATED, Tuple::from([ent(obj), ent(room)])))
                    .emit(tell(player, format!("Created {}.", text_of(&name))));
                Ok(alloc.seal(patch))
            }

            v if v == Value::text("Dig") => {
                let name = noun.unwrap_or(Value::text("room"));
                let mut alloc = Alloc::from_snapshot(snap, ENTITY_SEQ, ID_BASE)?;
                let dest = alloc.fresh();
                // A new room named `name`, a passage to it named `name`, and a
                // way back. Rooms and exits are ordinary tuples.
                let patch = Patch::new()
                    .assert(Fact::new(NAMED, Tuple::from([ent(dest), name.clone()])))
                    .assert(Fact::new(EXITS, Tuple::from([ent(room), name.clone(), ent(dest)])))
                    .assert(Fact::new(EXITS, Tuple::from([ent(dest), Value::text("back"), ent(room)])))
                    .emit(tell(player, format!("Dug {}.", text_of(&name))));
                Ok(alloc.seal(patch))
            }

            v if v == Value::text("Go") => go(snap, player, room, noun),

            v if v == Value::text("Take") => take(snap, player, room, noun),

            _ => Ok(Patch::new().emit(tell(player, "Huh?"))),
        }
    })
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

fn look(snap: &Snapshot, player: Entity, room: Entity) -> Result<Patch> {
    // Things here (excluding the looker), and the exits.
    let here = Query::rel(LOCATED)
        .filter(move |t| t.as_slice()[1] == ent(room))
        .project([0]);
    let named = here.join(Query::rel(NAMED), [0], [0]).project([0, 2]);
    let mut things: Vec<String> = named
        .find(snap)?
        .into_iter()
        .filter(|(t, _)| t.as_slice()[0] != ent(player))
        .map(|(t, _)| text_of(&t.as_slice()[1]))
        .collect();
    things.sort();
    let exits = Query::rel(EXITS)
        .filter(move |t| t.as_slice()[0] == ent(room))
        .project([1]);
    let mut ways: Vec<String> =
        exits.find(snap)?.into_iter().map(|(t, _)| text_of(&t.as_slice()[0])).collect();
    ways.sort();

    let here_line = if things.is_empty() {
        "You see nothing here.".to_string()
    } else {
        format!("You see: {}.", things.join(", "))
    };
    let exit_line = if ways.is_empty() {
        "There are no exits.".to_string()
    } else {
        format!("Exits: {}.", ways.join(", "))
    };
    Ok(Patch::new().emit(tell(player, here_line)).emit(tell(player, exit_line)))
}

fn go(snap: &Snapshot, player: Entity, room: Entity, noun: Option<Value>) -> Result<Patch> {
    let dir = match noun {
        Some(n) => n,
        None => return Ok(Patch::new().emit(tell(player, "Go where?"))),
    };
    let dest = Query::rel(EXITS)
        .filter(move |t| t.as_slice()[0] == ent(room) && t.as_slice()[1] == dir)
        .project([2])
        .find(snap)?;
    let dest = match dest.first().and_then(|(t, _)| match t.as_slice().first() {
        Some(Value::Ent(e)) => Some(*e),
        _ => None,
    }) {
        Some(e) => e,
        None => return Ok(Patch::new().emit(tell(player, "You can't go that way."))),
    };
    // Guard on the player still being in `room` so a stale move can't teleport.
    let from = Fact::new(LOCATED, Tuple::from([ent(player), ent(room)]));
    Ok(Patch::new()
        .expect(from.clone())
        .retract(from)
        .assert(Fact::new(LOCATED, Tuple::from([ent(player), ent(dest)])))
        .emit(tell(player, "You move.")))
}

fn take(snap: &Snapshot, player: Entity, room: Entity, noun: Option<Value>) -> Result<Patch> {
    let noun = match noun {
        Some(n) => n,
        None => return Ok(Patch::new().emit(tell(player, "Take what?"))),
    };
    let thing = match thing_in_room(snap, room, &noun)? {
        Some(e) => e,
        None => return Ok(Patch::new().emit(tell(player, "You don't see that here."))),
    };
    // The guarded take: expect it in the room, move it to the player. The
    // precondition is the optimistic race point — two players racing the same
    // lamp both build this, exactly one commit wins (M4).
    let here = Fact::new(LOCATED, Tuple::from([ent(thing), ent(room)]));
    Ok(Patch::new()
        .expect(here.clone())
        .retract(here)
        .assert(Fact::new(HELD, Tuple::from([ent(player), ent(thing)])))
        .emit(tell(player, "Taken.")))
}
