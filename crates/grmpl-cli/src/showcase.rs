//! `grmpl showcase` — a narrated tour of the substrate's distinctive features.
//!
//! Each scene stands up a small world and demonstrates one capability, printing
//! what it is showing. The features that are reachable from the text surface use
//! the built-in MOO (`worlds/moo.grmpl`); the deeper substrate features (replay,
//! forks, live behaviors-as-relations, scheduling) are driven programmatically,
//! since they have no surface syntax yet.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, Edition, EditionStore, Entity, Fact, NoSchemas, Patch, RelId, Scheduled,
    Scope, TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_ent::EntStore;
use grmpl_lang::{dispatch, PredExpr, Program, StoredBehavior, Word};
use grmpl_proc::{
    commit_patch, decode_activation, enqueue, record_run, replay_from, CommitOutcome, Process,
    Scheduler, SeqAlloc,
};

const BUILTIN_MOO: &str = include_str!("../../../worlds/moo.grmpl");

// Showcase world entities.
const ALICE: Entity = Entity(1);
const BOB: Entity = Entity(2);
const FOYER: Entity = Entity(10);
const LIBRARY: Entity = Entity(11);
const LAMP: Entity = Entity(20);
const BOOK: Entity = Entity(21);

pub fn run() -> Result<(), String> {
    banner();
    scene_views()?;
    scene_aggregates()?;
    scene_handlers()?;
    scene_optimistic()?;
    scene_reactive()?;
    scene_history()?;
    scene_replay()?;
    scene_forks()?;
    scene_live_behaviors()?;
    scene_scheduling()?;
    println!("\n═══ End of tour. Try `grmpl run` to play the world yourself. ═══");
    Ok(())
}

fn banner() {
    println!("═══════════════════════════════════════════════════════════════");
    println!(" grmpl — a differential, relational substrate for a versioned world");
    println!(" A narrated tour. Every 'MOO' feature below is ordinary substrate.");
    println!("═══════════════════════════════════════════════════════════════");
}

fn scene(n: u32, title: &str) {
    println!("\n─── {n}. {title} ───");
}

// ===========================================================================
// 1. Views: reading the world is a relational query.
// ===========================================================================
fn scene_views() -> Result<(), String> {
    scene(1, "Views — the world facade is a join query");
    let w = MooWorld::seeded()?;
    println!("`view here(alice)` compiles to a three-way join (located ⋈ located ⋈ named).");
    let here = w.view("here", &[Value::Ent(ALICE)])?;
    print!("  alice sees: ");
    println!("{}", names(&here, 1).join(", "));
    let ways = w.view("ways", &[Value::Ent(ALICE)])?;
    println!("  exits: {}", names(&ways, 0).join(", "));
    println!("No table stores 'the room contents' — it is derived on read.");
    Ok(())
}

// ===========================================================================
// 2. Aggregates: `yield ..., count()` lowers to Query::Reduce.
// ===========================================================================
fn scene_aggregates() -> Result<(), String> {
    scene(2, "Aggregates — `yield room, sum(coins)` is a Reduce");
    let w = MooWorld::seeded()?;
    let treasure = w.view("treasure", &[])?;
    println!("`view treasure() {{ located(t,room) value(t,coins) named(room,rname) yield rname, sum(coins) }}`:");
    for (t, _) in &treasure {
        println!("  {}: {} coins", text_at(t, 0), int_at(t, 1));
    }
    println!("Folded over the room group, on read — no denormalized total is stored.");
    Ok(())
}

// ===========================================================================
// 3. Command handlers in both coexisting surfaces.
// ===========================================================================
fn scene_handlers() -> Result<(), String> {
    scene(3, "Verbs — one handler, two surfaces, identical effects");
    let w = MooWorld::seeded()?;
    let alice = w.player(ALICE)?;

    println!("`take` is written point-free (concatenative surface, P11):");
    let e0 = w.store.current();
    w.enqueue(ALICE, 0, "take lamp")?;
    alice.run_to_idle(&w.store, &NoSchemas).map_err(err)?;
    println!("  told: {}", w.tell_since(ALICE, e0)?);
    println!("  alice now holds the lamp: {}", w.holds(ALICE, LAMP)?);

    println!("`drop` is the statement surface — the mirror verb, same primitives:");
    let e1 = w.store.current();
    w.enqueue(ALICE, 1, "drop lamp")?;
    alice.run_to_idle(&w.store, &NoSchemas).map_err(err)?;
    println!("  told: {}", w.tell_since(ALICE, e1)?);
    println!("  lamp is back in the room: {}", !w.holds(ALICE, LAMP)?);
    Ok(())
}

// ===========================================================================
// 4. Optimistic concurrency: two writers race one precondition.
// ===========================================================================
fn scene_optimistic() -> Result<(), String> {
    scene(
        4,
        "Optimistic concurrency — racing writers, exactly one wins",
    );
    let w = MooWorld::seeded()?;
    let auth = world_authority(&w.r);
    let lamp_here = Fact::new(
        w.r.located,
        Tuple::from([Value::Ent(LAMP), Value::Ent(FOYER)]),
    );

    // Both build the same guarded take against the current world.
    let take_for = |who: Entity| {
        Patch::new()
            .expect(lamp_here.clone())
            .retract(lamp_here.clone())
            .assert(Fact::new(
                w.r.held,
                Tuple::from([Value::Ent(who), Value::Ent(LAMP)]),
            ))
    };

    println!("alice and bob both `expect located(lamp, foyer)` then take it:");
    let a = commit_patch(&w.store, &NoSchemas, &take_for(ALICE), &auth).map_err(err)?;
    let b = commit_patch(&w.store, &NoSchemas, &take_for(BOB), &auth).map_err(err)?;
    println!("  alice: {}", outcome(&a));
    println!(
        "  bob:   {}   (its precondition vanished — commit_if rejected it)",
        outcome(&b)
    );
    println!(
        "  lamp held by alice: {}, by bob: {}",
        w.holds(ALICE, LAMP)?,
        w.holds(BOB, LAMP)?
    );
    Ok(())
}

// ===========================================================================
// 5. Reactive on-watch: a maintained view pumps deltas, exactly once.
// ===========================================================================
fn scene_reactive() -> Result<(), String> {
    scene(
        5,
        "Reactive on-watch — a view change streams as an activation",
    );
    let w = MooWorld::seeded()?;
    let auth = Authority::new(
        DomainId(1),
        vec![
            Scope::whole(w.r.wmail),
            Scope::whole(w.r.wcursor),
            Scope::whole(w.r.wseq),
        ],
    );
    let (ow, _) = w
        .prog
        .install_watch("world", &[], ALICE, ALICE, auth, &w.store, &NoSchemas)
        .map_err(err)?;
    // The MOO watch is `including current`: consume the initial snapshot.
    ow.pump(&w.store, &NoSchemas).map_err(err)?;
    let base = w.activation_count()?;
    println!("alice is watching the `world` view. Now bob takes the book...");

    let book_here = Fact::new(
        w.r.located,
        Tuple::from([Value::Ent(BOOK), Value::Ent(FOYER)]),
    );
    let bob_take = Patch::new()
        .expect(book_here.clone())
        .retract(book_here)
        .assert(Fact::new(
            w.r.held,
            Tuple::from([Value::Ent(BOB), Value::Ent(BOOK)]),
        ));
    commit_patch(&w.store, &NoSchemas, &bob_take, &world_authority(&w.r)).map_err(err)?;

    let delivered = ow.pump(&w.store, &NoSchemas).map_err(err)?;
    println!("  pump delivered {delivered} activation(s), each weight-1 (exactly once):");
    for (diff, name) in w.new_activations(base)? {
        println!("    {} {}", if diff > 0 { "+" } else { "-" }, name);
    }
    println!("  the durable watch-cursor guarantees no re-delivery across restarts.");
    Ok(())
}

// ===========================================================================
// 6. As-of history: read any past edition.
// ===========================================================================
fn scene_history() -> Result<(), String> {
    scene(6, "As-of history — the past is not overwritten");
    let w = MooWorld::seeded()?;
    let e0 = w.store.current();
    println!(
        "edition {} — alice's room holds: {}",
        e0.0,
        names(&w.view("here", &[Value::Ent(ALICE)])?, 1).join(", ")
    );

    // Mutate: alice takes the lamp.
    let lamp_here = Fact::new(
        w.r.located,
        Tuple::from([Value::Ent(LAMP), Value::Ent(FOYER)]),
    );
    let take = Patch::new()
        .expect(lamp_here.clone())
        .retract(lamp_here)
        .assert(Fact::new(
            w.r.held,
            Tuple::from([Value::Ent(ALICE), Value::Ent(LAMP)]),
        ));
    commit_patch(&w.store, &NoSchemas, &take, &world_authority(&w.r)).map_err(err)?;

    let now = w.store.current();
    let past = w
        .prog
        .view("here", &[Value::Ent(ALICE)])
        .map_err(err)?
        .find(&Snapshot::new(&w.store, e0))
        .map_err(err)?;
    let present = w
        .prog
        .view("here", &[Value::Ent(ALICE)])
        .map_err(err)?
        .find(&Snapshot::at_current(&w.store))
        .map_err(err)?;
    println!(
        "edition {} (as-of the past): {}",
        e0.0,
        names(&past, 1).join(", ")
    );
    println!(
        "edition {} (now):            {}",
        now.0,
        names(&present, 1).join(", ")
    );
    println!("Same query, two editions — history is queryable, never mutated in place.");
    Ok(())
}

// ===========================================================================
// 7. Deterministic replay: behavior is pure in (snapshot, message).
// ===========================================================================
fn scene_replay() -> Result<(), String> {
    scene(
        7,
        "Deterministic replay — re-derive every patch from history",
    );
    let w = MooWorld::seeded()?;
    let alice = w.player(ALICE)?;
    w.enqueue(ALICE, 0, "take lamp")?;
    w.enqueue(ALICE, 1, "go north")?;

    let live = record_run(&w.store, &alice, &NoSchemas).map_err(err)?;
    let replayed = replay_from(&w.store, &alice, Edition::ZERO).map_err(err)?;
    println!(
        "ran {} messages live, then replayed from edition 0.",
        live.len()
    );
    println!(
        "  live log == replayed log, step for step: {}",
        live == replayed
    );
    println!("  (each Step carries the exact patch and edition it committed.)");
    Ok(())
}

// ===========================================================================
// 8. Forks: an O(domain) bit-identical copy of the whole world.
// ===========================================================================
fn scene_forks() -> Result<(), String> {
    scene(8, "Forks — an O(edit) virtual copy you can diverge safely");
    let w = MooWorld::seeded()?;
    // Do a little history first.
    let lamp_here = Fact::new(
        w.r.located,
        Tuple::from([Value::Ent(LAMP), Value::Ent(FOYER)]),
    );
    let take = Patch::new()
        .expect(lamp_here.clone())
        .retract(lamp_here)
        .assert(Fact::new(
            w.r.held,
            Tuple::from([Value::Ent(ALICE), Value::Ent(LAMP)]),
        ));
    commit_patch(&w.store, &NoSchemas, &take, &world_authority(&w.r)).map_err(err)?;

    // The Ent's fork is a new branch in the *same* granfilade, sharing every
    // node with its ancestor: O(edit), not an O(state) byte copy.
    let encoded_before = w.store.frames_encoded();
    let fork = w.store.fork_at(w.store.current()).map_err(err)?;
    let node_frames = w.store.frames_encoded() - encoded_before;
    let same = projection(&w.store, &w.r)? == projection(&fork, &w.r)?;
    println!("forked the world at edition {}.", w.store.current().0);
    println!("  fork holds the same world as its source: {same}");
    println!("  node frames written to copy it: {node_frames} (it shares them all)");

    // Diverge the fork; the source is untouched.
    let bob_take = Patch::new().assert(Fact::new(
        w.r.held,
        Tuple::from([Value::Ent(BOB), Value::Ent(BOOK)]),
    ));
    commit_patch(&fork, &NoSchemas, &bob_take, &world_authority(&w.r)).map_err(err)?;
    let diverged = projection(&w.store, &w.r)? != projection(&fork, &w.r)?;
    println!("  wrote to the fork; source and fork now differ: {diverged}");
    println!("  the source world never saw the fork's commit.");
    Ok(())
}

// ===========================================================================
// 9. Behaviors as relations (P12): live code you retract and re-assert.
// ===========================================================================
fn scene_live_behaviors() -> Result<(), String> {
    scene(9, "Live behaviors — code is data, dispatch is a view");
    let (store, _dir) = fresh_store()?;
    let prog = Program::compile(
        "rel direct_behavior(entity, code)\n\
         rel prototype(entity, parent)\n\
         rel greeted(who)\n\
         rel waved(who)",
        1,
    )?;
    let direct = prog.rel_id("direct_behavior").unwrap();
    let proto = prog.rel_id("prototype").unwrap();
    let greeted = prog.rel_id("greeted").unwrap();
    let waved = prog.rel_id("waved").unwrap();
    let (player, avatar) = (Entity(1), Entity(2));
    let auth = Authority::new(DomainId(1), vec![Scope::whole(direct), Scope::whole(proto)]);

    // A behavior is a value: guard = match-all, body = point-free words.
    let greet = StoredBehavior::from_words(
        &prog,
        PredExpr::And(vec![]),
        vec![grmpl_core::Ty::Text],
        vec![Word::Drop, Word::SelfEntity, Word::Assert("greeted".into())],
    )?;
    let wave = StoredBehavior::from_words(
        &prog,
        PredExpr::And(vec![]),
        vec![grmpl_core::Ty::Text],
        vec![Word::Drop, Word::SelfEntity, Word::Assert("waved".into())],
    )?;

    // player inherits from avatar; avatar's behavior is `greet`.
    let install = Patch::new()
        .assert(Fact::new(
            proto,
            Tuple::from([Value::Ent(player), Value::Ent(avatar)]),
        ))
        .assert(Fact::new(
            direct,
            Tuple::from([Value::Ent(avatar), greet.to_value()]),
        ));
    commit_patch(&store, &NoSchemas, &install, &auth).map_err(err)?;

    let msg = Tuple::from([Value::text("ping")]);
    let snap = Snapshot::at_current(&store);
    let patch = dispatch(&prog, direct, proto, player, &snap, &msg)
        .map_err(err)?
        .ok_or("no behavior matched")?;
    println!("dispatch resolves through the prototype chain (a recursive view):");
    println!(
        "  player's inherited behavior asserts: {}",
        rel_name(&patch, greeted, waved)
    );

    // Live redefinition: an ordinary retract + assert. No new mechanism.
    let redefine = Patch::new()
        .retract(Fact::new(
            direct,
            Tuple::from([Value::Ent(avatar), greet.to_value()]),
        ))
        .assert(Fact::new(
            direct,
            Tuple::from([Value::Ent(avatar), wave.to_value()]),
        ));
    commit_patch(&store, &NoSchemas, &redefine, &auth).map_err(err)?;

    let snap = Snapshot::at_current(&store);
    let patch = dispatch(&prog, direct, proto, player, &snap, &msg)
        .map_err(err)?
        .ok_or("no behavior matched")?;
    println!(
        "after retract+assert of the avatar's code cell, same message now asserts: {}",
        rel_name(&patch, greeted, waved)
    );
    println!("  the world's behavior changed at runtime — no redeploy, just a commit.");
    Ok(())
}

// ===========================================================================
// 10. Scheduling: durable timers, nondeterminism recorded as data.
// ===========================================================================
fn scene_scheduling() -> Result<(), String> {
    scene(10, "Scheduling — durable timers fire exactly once");
    let (store, _dir) = fresh_store()?;
    const TIMERS: RelId = RelId(40);
    const SEQS: RelId = RelId(41);
    const INBOX: RelId = RelId(42);
    const PROC: Entity = Entity(1);
    let auth = Authority::new(
        DomainId(1),
        vec![
            Scope::whole(TIMERS),
            Scope::whole(SEQS),
            Scope::whole(INBOX),
        ],
    );

    // Seed the seq counter once on the un-raced path.
    let key = vec![Value::Int(INBOX.0 as i64), Value::Ent(PROC)];
    let alloc = SeqAlloc::read(&store, SEQS, key).map_err(err)?;
    commit_patch(&store, &NoSchemas, &alloc.seed(Patch::new()), &auth).map_err(err)?;

    // Schedule a message due at sim-time 5, as one atomic commit.
    let sched = Patch::new().schedule(Scheduled {
        timers: TIMERS,
        due: 5,
        inbox: INBOX,
        target: PROC,
        body: Tuple::from([Value::text("the sun rises")]),
    });
    commit_patch(&store, &NoSchemas, &sched, &auth).map_err(err)?;
    println!("scheduled a message due at sim-time 5 (a durable timer row).");

    let sch = Scheduler::new(TIMERS, SEQS, auth);
    let early = sch.fire_due(&store, &NoSchemas, 4).map_err(err)?;
    println!("  fire_due(now=4): {early} fired (not due yet)");
    let on_time = sch.fire_due(&store, &NoSchemas, 5).map_err(err)?;
    println!("  fire_due(now=5): {on_time} fired");
    let again = sch.fire_due(&store, &NoSchemas, 5).map_err(err)?;
    println!("  fire_due(now=5) again: {again} fired (idempotent — exactly once)");

    // Show the message landed in the inbox.
    let at = store.current();
    for (t, d) in store.read_at(INBOX, at).map_err(err)? {
        if d > 0 {
            if let Some(Value::Tuple(body)) = t.as_slice().get(2) {
                if let Some(Value::Text(s)) = body.first() {
                    println!("  the inbox now holds: \"{s}\"");
                }
            }
        }
    }
    Ok(())
}

// ===========================================================================
// Shared helpers
// ===========================================================================

/// A compiled MOO world over a fresh throwaway store, seeded with two players.
struct MooWorld {
    prog: Arc<Program>,
    store: EntStore,
    r: Rels,
    _dir: tempfile::TempDir,
}

struct Rels {
    located: RelId,
    named: RelId,
    held: RelId,
    exits: RelId,
    players: RelId,
    value: RelId,
    tell: RelId,
    inbox: RelId,
    cursor: RelId,
    wmail: RelId,
    wcursor: RelId,
    wseq: RelId,
}

impl Rels {
    /// Every relation this world writes — the domain of its logical projection.
    fn all(&self) -> [RelId; 12] {
        [
            self.located,
            self.named,
            self.held,
            self.exits,
            self.players,
            self.value,
            self.tell,
            self.inbox,
            self.cursor,
            self.wmail,
            self.wcursor,
            self.wseq,
        ]
    }
}

impl MooWorld {
    fn seeded() -> Result<MooWorld, String> {
        let prog = Arc::new(Program::compile(BUILTIN_MOO, 1)?);
        let rid = |n: &str| prog.rel_id(n).ok_or_else(|| format!("no rel {n}"));
        let r = Rels {
            located: rid("located")?,
            named: rid("named")?,
            held: rid("held")?,
            exits: rid("exits")?,
            players: rid("players")?,
            value: rid("value")?,
            tell: rid("tell")?,
            inbox: rid("inbox")?,
            cursor: rid("cursor")?,
            wmail: rid("wmail")?,
            wcursor: rid("wcursor")?,
            wseq: rid("wseq")?,
        };
        let (store, _dir) = fresh_store()?;
        let e = Value::Ent;
        let t = Value::text;
        store
            .commit(&[
                (r.located, Tuple::from([e(ALICE), e(FOYER)]), 1),
                (r.located, Tuple::from([e(BOB), e(FOYER)]), 1),
                (r.located, Tuple::from([e(LAMP), e(FOYER)]), 1),
                (r.located, Tuple::from([e(BOOK), e(FOYER)]), 1),
                (r.named, Tuple::from([e(ALICE), t("alice")]), 1),
                (r.named, Tuple::from([e(BOB), t("bob")]), 1),
                (r.named, Tuple::from([e(FOYER), t("Foyer")]), 1),
                (r.named, Tuple::from([e(LIBRARY), t("Library")]), 1),
                (r.named, Tuple::from([e(LAMP), t("brass lamp")]), 1),
                (r.named, Tuple::from([e(BOOK), t("old book")]), 1),
                (r.exits, Tuple::from([e(FOYER), t("north"), e(LIBRARY)]), 1),
                (r.exits, Tuple::from([e(LIBRARY), t("south"), e(FOYER)]), 1),
                (r.players, Tuple::from([e(ALICE), t("alice")]), 1),
                (r.players, Tuple::from([e(BOB), t("bob")]), 1),
                (r.value, Tuple::from([e(LAMP), Value::Int(10)]), 1),
                (r.value, Tuple::from([e(BOOK), Value::Int(3)]), 1),
            ])
            .map_err(err)?;
        Ok(MooWorld {
            prog,
            store,
            r,
            _dir,
        })
    }

    fn view(&self, name: &str, args: &[Value]) -> Result<Vec<(Tuple, grmpl_core::Diff)>, String> {
        let q = self.prog.view(name, args)?;
        q.find(&Snapshot::at_current(&self.store)).map_err(err)
    }

    fn player(&self, who: Entity) -> Result<Process, String> {
        let behavior = Program::behavior(&self.prog, "inbox", who)?;
        Ok(Process {
            entity: who,
            authority: world_authority(&self.r),
            inbox: self.r.inbox,
            cursor_rel: self.r.cursor,
            behavior,
        })
    }

    fn enqueue(&self, who: Entity, seq: i64, line: &str) -> Result<(), String> {
        let body = Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>());
        enqueue(&self.store, self.r.inbox, who, seq, body).map_err(err)?;
        Ok(())
    }

    /// The newest `tell` addressed to `who` since edition `since` — read in
    /// commit order (`scan_updates`), so "newest" is the last one written, not
    /// the tuple-sorted last.
    fn tell_since(&self, who: Entity, since: Edition) -> Result<String, String> {
        let to = self.store.current();
        let mut last = String::from("(nothing said)");
        for u in self
            .store
            .scan_updates(self.r.tell, since, to)
            .map_err(err)?
        {
            let s = u.tuple.as_slice();
            if u.diff > 0 && s.first() == Some(&Value::Ent(who)) {
                if let Some(Value::Text(text)) = s.get(1) {
                    last = text.to_string();
                }
            }
        }
        Ok(last)
    }

    fn holds(&self, who: Entity, thing: Entity) -> Result<bool, String> {
        let at = self.store.current();
        Ok(self
            .store
            .read_at(self.r.held, at)
            .map_err(err)?
            .into_iter()
            .any(|(t, d)| d > 0 && t == Tuple::from([Value::Ent(who), Value::Ent(thing)])))
    }

    fn activation_count(&self) -> Result<usize, String> {
        let at = self.store.current();
        Ok(self
            .store
            .read_at(self.r.wmail, at)
            .map_err(err)?
            .into_iter()
            .filter(|(_, d)| *d > 0)
            .count())
    }

    /// Activations with seq >= `base`, decoded to (diff, name).
    fn new_activations(&self, base: usize) -> Result<Vec<(i64, String)>, String> {
        let at = self.store.current();
        let mut out = Vec::new();
        for (t, d) in self.store.read_at(self.r.wmail, at).map_err(err)? {
            if d <= 0 {
                continue;
            }
            let seq = int_at(&t, 1);
            if (seq as usize) < base {
                continue;
            }
            if let Some(Value::Tuple(body)) = t.as_slice().get(2) {
                if let Some((diff, row)) = decode_activation(&Tuple(body.clone())) {
                    out.push((diff, text_at(&row, 1)));
                }
            }
        }
        Ok(out)
    }
}

fn fresh_store() -> Result<(EntStore, tempfile::TempDir), String> {
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let store = EntStore::open(dir.path()).map_err(err)?;
    Ok((store, dir))
}

/// The **logical projection** of a world: every relation's raw updates in
/// commit order. This is how `DESIGN.md` and the store contract define identity
/// — by `scan_updates`, not by node bytes — so it is what a fork or a replay has
/// to reproduce. (Comparing physical bytes would be both weaker, since it drops
/// same-edition submit order, and impossible to state for two substrates.)
fn projection(store: &EntStore, r: &Rels) -> Result<Vec<Vec<grmpl_core::Update>>, String> {
    let (from, to) = (store.watermark(), store.current());
    r.all()
        .iter()
        .map(|rel| store.scan_updates(*rel, from, to).map_err(err))
        .collect()
}

/// The authority a MOO verb commits under — owns every world relation it writes.
fn world_authority(r: &Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(r.located),
            Scope::whole(r.named),
            Scope::whole(r.held),
            Scope::whole(r.exits),
            Scope::whole(r.players),
            Scope::whole(r.tell),
            Scope::whole(r.cursor),
        ],
    )
}

fn outcome(o: &CommitOutcome) -> &'static str {
    match o {
        CommitOutcome::Committed(_) => "COMMITTED",
        CommitOutcome::Rejected => "rejected",
    }
}

/// Which of two candidate relations a dispatched patch's first assert targets.
fn rel_name(patch: &Patch, greeted: RelId, waved: RelId) -> &'static str {
    match patch.asserts.first().map(|f| f.rel) {
        Some(r) if r == greeted => "greeted(self)",
        Some(r) if r == waved => "waved(self)",
        _ => "(nothing)",
    }
}

/// The `Text` at column `i` of a tuple.
fn text_at(t: &Tuple, i: usize) -> String {
    match t.as_slice().get(i) {
        Some(Value::Text(s)) => s.to_string(),
        Some(other) => format!("{other:?}"),
        None => "?".to_string(),
    }
}

fn int_at(t: &Tuple, i: usize) -> i64 {
    match t.as_slice().get(i) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

/// The sorted display names at column `i` across rows.
fn names(rows: &[(Tuple, grmpl_core::Diff)], i: usize) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|(t, _)| text_at(t, i)).collect();
    v.sort();
    v
}

/// Uniform error stringifier for `grmpl_core::Error`.
fn err<E: std::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}
