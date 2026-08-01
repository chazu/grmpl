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
    Authority, Diff, DomainId, Entity, RelId, Scope, TraceStore, Tuple, Value,
};
use grmpl_lang::Program;
use grmpl_proc::{enqueue, CommitOutcome, Process};

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
fn run(store: &dyn TraceStore, src: &str) -> World {
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
    store
        .commit(&[
            (w.located, Tuple::from([e(LAMP), e(ROOM)]), 1),
            (w.located, Tuple::from([e(PLAYER), e(ROOM)]), 1),
            (w.named, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
            (w.permits, Tuple::from([e(PLAYER), Value::text("see"), e(LAMP)]), 1),
        ])
        .unwrap();

    enqueue(
        store,
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
    let outcome = player.step(store, &NoSchemas).unwrap();
    assert!(matches!(outcome, Some(CommitOutcome::Committed(_))));

    w
}

fn snapshot(w: &World, store: &dyn TraceStore) -> Vec<(RelId, Vec<(Tuple, Diff)>)> {
    let cur = store.current();
    w.rels()
        .into_iter()
        .map(|r| (r, store.read_at(r, cur).unwrap()))
        .collect()
}

#[test]
fn concatenative_take_lamp_matches_v1_editions() {
    grmpl_conformance::for_each_store(|c| {
        let sc_case = c.sibling();
        let sc = sc_case.store();
        let wc = run(sc, &concat_src());
        let sv_case = c.sibling();
        let sv = sv_case.store();
        let wv = run(sv, &v1_src());

        // Same commit clock: the concatenative handler allocated exactly the
        // editions the statement handler did.
        assert_eq!(sc.current(), sv.current());

        // Same world, relation by relation: the lamp left the room, the player
        // holds it, and "Taken." was told — identical facts at identical editions.
        assert_eq!(snapshot(&wc, sc), snapshot(&wv, sv));

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
    });
}

#[test]
fn concatenative_absent_thing_makes_no_change() {
    grmpl_conformance::for_each_store(|c| {
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

        let store = c.store();
        store
            .commit(&[
                (located, Tuple::from([e(LAMP), e(ROOM)]), 1),
                (located, Tuple::from([e(PLAYER), e(ROOM)]), 1),
                (named, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
                (permits, Tuple::from([e(PLAYER), Value::text("see"), e(LAMP)]), 1),
            ])
            .unwrap();
        enqueue(
            store,
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
        player.step(store, &NoSchemas).unwrap();

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
        assert!(player.step(store, &NoSchemas).unwrap().is_none());
    });
}

// --- Randomized-churn law oracle ---------------------------------------------
//
// The two witnesses above pin *one* trace each. The central P11 law is stronger:
// a handler written in the statement surface and the same handler written in the
// concatenative surface produce **byte-identical editions** — same commit clock,
// same world relation-by-relation — for *any* committed history, because both
// lower to the identical effect primitives. Every APPROVEd sibling in this fleet
// (determinism, replay, fork, MatchInput) shipped a seeded randomized-churn
// oracle for its law rather than example tests; this is P11's.
//
// Each round, both stores are seeded with the *same* random starting world
// (several things across two rooms, some unnamed, some un-permitted — so
// `resolve` sees present / absent / ambiguous nouns) and driven by the *same*
// random message stream (`take <noun>` and `look`), stepping the v1 and
// concatenative programs in lockstep and asserting bit-identical editions after
// every commit. A tiny xorshift64* PRNG keeps the churn reproducible with no
// external dependency; every assertion prints its `seed` so a failure replays.

/// Deterministic, seedable PRNG (xorshift64*), mirroring
/// `grmpl-ent/tests/store_laws.rs` — reproducible churn without a `rand` dep.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point of xorshift.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// The base relations a random starting world seeds. Selecting by name keeps the
/// generated world spec program-agnostic, so the *same* facts land in both the
/// v1 and concatenative stores.
enum R {
    Located,
    Named,
    Permits,
}

impl R {
    fn rel(&self, w: &World) -> RelId {
        match self {
            R::Located => w.located,
            R::Named => w.named,
            R::Permits => w.permits,
        }
    }
}

const ROOMS: [Entity; 2] = [Entity(10), Entity(11)];
/// Multi-word names so `name ~ noun` (word match) can hit on a bare noun.
const NAMES: [&str; 4] = ["brass lamp", "iron sword", "gold coin", "silver key"];
/// Nouns to `take`; `"gem"` names nothing, forcing resolve misses.
const NOUNS: [&str; 5] = ["lamp", "sword", "coin", "key", "gem"];

/// Compile `src` and open a fresh empty store, without seeding or enqueuing.
fn compile_world(_store: &dyn TraceStore, src: &str) -> World {
    let prog = Arc::new(Program::compile(src, 1).unwrap());
    let rid = |n: &str| prog.rel_id(n).unwrap();
    
    World {
        located: rid("located"),
        named: rid("named"),
        permits: rid("permits"),
        held: rid("held"),
        tell: rid("tell"),
        inbox: rid("inbox"),
        cursor: rid("cursor"),
        prog,
    }
}

/// The `PLAYER` process for `w`, owning exactly the relations the take-lamp
/// handler writes (identical to the witness tests).
fn make_process(w: &World) -> Process {
    let behavior = Program::behavior(&w.prog, "inbox", PLAYER).unwrap();
    Process {
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
    }
}

/// A random starting world as name-keyed facts, so the identical spec can be
/// committed to both stores. Several things across two rooms; each is randomly
/// named (or not) and randomly permitted to `PLAYER` (or not) — yielding
/// present, absent, and ambiguous nouns for `resolve`.
fn gen_world(rng: &mut Rng) -> Vec<(R, Tuple)> {
    let mut spec = Vec::new();
    let player_room = ROOMS[rng.below(2) as usize];
    spec.push((R::Located, Tuple::from([e(PLAYER), e(player_room)])));

    let n_things = 1 + rng.below(6); // 1..=6 things
    for i in 0..n_things {
        let thing = Entity(100 + i);
        let room = ROOMS[rng.below(2) as usize];
        spec.push((R::Located, Tuple::from([e(thing), e(room)])));
        // 3/4 named, so some things stay unnamed (invisible to the view).
        if rng.below(4) != 0 {
            let name = NAMES[rng.below(4) as usize];
            spec.push((R::Named, Tuple::from([e(thing), Value::text(name)])));
        }
        // 3/4 permitted, so some things stay un-permitted (invisible).
        if rng.below(4) != 0 {
            spec.push((
                R::Permits,
                Tuple::from([e(PLAYER), Value::text("see"), e(thing)]),
            ));
        }
    }
    spec
}

/// A random message stream: mostly `take <noun>`, some `look` (which no arm
/// matches — an identical no-op on both surfaces).
fn gen_messages(rng: &mut Rng) -> Vec<Tuple> {
    let n = 3 + rng.below(6); // 3..=8 messages
    (0..n)
        .map(|_| {
            let pick = rng.below(6);
            if pick < NOUNS.len() as u64 {
                Tuple::from([Value::text("take"), Value::text(NOUNS[pick as usize])])
            } else {
                Tuple::from([Value::text("look")])
            }
        })
        .collect()
}

/// Commit `spec` to `store`, resolving each relation name against `w`.
fn commit_world(w: &World, store: &dyn TraceStore, spec: &[(R, Tuple)]) {
    let batch: Vec<(RelId, Tuple, Diff)> =
        spec.iter().map(|(r, t)| (r.rel(w), t.clone(), 1)).collect();
    store.commit(&batch).unwrap();
}

/// The law: same commit clock, same world relation-by-relation.
fn assert_lockstep(
    seed: u64,
    wc: &World,
    sc: &dyn TraceStore,
    wv: &World,
    sv: &dyn TraceStore,
    at: &str,
) {
    assert_eq!(
        sc.current(),
        sv.current(),
        "seed {seed}: commit clock diverged {at}"
    );
    assert_eq!(
        snapshot(wc, sc),
        snapshot(wv, sv),
        "seed {seed}: world diverged {at}"
    );
}

/// One churn round: returns how many `take`s actually changed `held` (an
/// anti-vacuity signal — the world/messages must exercise the effect seam).
fn surface_churn_round(c: &grmpl_conformance::Case, seed: u64) -> u64 {
    let mut rng = Rng::new(seed);
    let sc_case = c.sibling();
    let sc = sc_case.store();
    let wc = compile_world(sc, &concat_src());
    let sv_case = c.sibling();
    let sv = sv_case.store();
    let wv = compile_world(sv, &v1_src());
    let pc = make_process(&wc);
    let pv = make_process(&wv);

    // Identical random starting world, committed to both stores.
    let world = gen_world(&mut rng);
    commit_world(&wc, sc, &world);
    commit_world(&wv, sv, &world);
    assert_lockstep(seed, &wc, sc, &wv, sv, "after world seed");

    // Identical random message stream, stepped in lockstep.
    let msgs = gen_messages(&mut rng);
    let mut changed = 0u64;
    for (i, body) in msgs.into_iter().enumerate() {
        let seq = i as i64;
        enqueue(sc, wc.inbox, PLAYER, seq, body.clone()).unwrap();
        enqueue(sv, wv.inbox, PLAYER, seq, body.clone()).unwrap();
        assert_lockstep(seed, &wc, sc, &wv, sv, "after enqueue");

        let held_before = sc.read_at(wc.held, sc.current()).unwrap();
        let oc = pc.step(sc, &NoSchemas).unwrap();
        let ov = pv.step(sv, &NoSchemas).unwrap();
        // Both surfaces make the identical scheduling decision.
        assert!(
            matches!(oc, Some(CommitOutcome::Committed(_)))
                && matches!(ov, Some(CommitOutcome::Committed(_))),
            "seed {seed}: step {i} did not commit on both surfaces (concat={oc:?}, v1={ov:?})"
        );
        assert_lockstep(seed, &wc, sc, &wv, sv, "after step");

        let held_after = sc.read_at(wc.held, sc.current()).unwrap();
        if held_after != held_before {
            changed += 1;
        }
    }
    changed
}

#[test]
fn concatenative_matches_v1_editions_under_random_churn() {
    grmpl_conformance::for_each_store(|c| {
    let mut takes_changed_held = 0u64;
    for seed in 1..=24u64 {
        takes_changed_held += surface_churn_round(c, seed);
    }
    // Guard against a vacuous pass: across 24 seeds, some `take` must have
    // actually retracted a thing and asserted `held`, so the effect seam — the
    // only place the two surfaces differ — was genuinely exercised.
    assert!(
        takes_changed_held > 0,
        "vacuous oracle: no take ever changed `held` across all seeds"
    );
    });
}
