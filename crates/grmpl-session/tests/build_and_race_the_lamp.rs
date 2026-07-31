//! P3 acceptance (roadmap): **build a room and race for a lamp, entirely from
//! clients.** Every world change below is issued as a command line through a
//! `Session` — there is no privileged setup path. It exercises:
//!
//!   * provisioning + spawn-as-a-commit (each `login` places a player);
//!   * replay-safe entity allocation (every `dig`/`create`/spawn gets a fresh
//!     id from the counter relation);
//!   * `dig`/`go`/`create` verbs as ordinary patches (world construction);
//!   * the optimistic take race resolved to exactly one winner.

use std::sync::Arc;

use grmpl_core::{Entity, TraceStore, Value};
use grmpl_session::world::{HELD, LOCATED, NAMED};
use grmpl_session::Server;

fn server(case: &grmpl_conformance::Case) -> Arc<Server> {
    let store: Arc<dyn TraceStore> = case.trace();
    let server = Server::new(store);
    server.init().unwrap();
    server
}

fn located(store: &dyn TraceStore, thing: Entity) -> Option<Entity> {
    let at = store.current();
    store.read_at(LOCATED, at).unwrap().into_iter().find_map(|(t, d)| {
        if d > 0 && t.as_slice().first() == Some(&Value::Ent(thing)) {
            match t.as_slice().get(1) {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            }
        } else {
            None
        }
    })
}

fn named_entity(store: &dyn TraceStore, name: &str) -> Option<Entity> {
    let at = store.current();
    store.read_at(NAMED, at).unwrap().into_iter().find_map(|(t, d)| {
        if d > 0 && t.as_slice().get(1) == Some(&Value::text(name)) {
            match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            }
        } else {
            None
        }
    })
}

#[test]
fn build_a_room_then_race_for_the_lamp() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);

        // --- build the world, entirely through the builder's client -----------
        let mut builder = server.login("builder").unwrap();
        assert_eq!(builder.submit("dig hall").unwrap(), vec!["Dug hall."]);
        assert_eq!(builder.submit("go hall").unwrap(), vec!["You move."]);
        assert_eq!(builder.submit("create lamp").unwrap(), vec!["Created lamp."]);

        let store = Arc::clone(server.store());
        let hall = named_entity(&*store, "hall").expect("hall was dug");
        let lamp = named_entity(&*store, "lamp").expect("lamp was created");

        // Allocation handed out distinct ids, all above the fixed base.
        assert_ne!(hall, lamp);
        assert!(hall.0 >= 1000 && lamp.0 >= 1000);
        // The builder walked into the hall and the lamp landed there.
        assert_eq!(located(&*store, builder.player()), Some(hall));
        assert_eq!(located(&*store, lamp), Some(hall));

        // --- two players join and walk to the hall ----------------------------
        let mut p1 = server.login("alice").unwrap();
        let mut p2 = server.login("bob").unwrap();
        assert_eq!(p1.submit("go hall").unwrap(), vec!["You move."]);
        assert_eq!(p2.submit("go hall").unwrap(), vec!["You move."]);

        // A look confirms both see the lamp before the race.
        let seen = p1.submit("look").unwrap();
        assert!(seen.iter().any(|l| l.contains("lamp")), "alice sees the lamp: {seen:?}");

        // --- the race: both grab the lamp; exactly one wins -------------------
        let r1 = p1.submit("take lamp").unwrap();
        let r2 = p2.submit("take lamp").unwrap();

        let winners: Vec<&str> = [(&p1, &r1), (&p2, &r2)]
            .iter()
            .filter(|(_, r)| r.iter().any(|l| l == "Taken."))
            .map(|(s, _)| if s.player() == p1.player() { "alice" } else { "bob" })
            .collect();
        assert_eq!(winners.len(), 1, "exactly one take succeeds: r1={r1:?} r2={r2:?}");
        // The loser is told they can't have it.
        let (_, loser) = [(&p1, &r1), (&p2, &r2)]
            .into_iter()
            .find(|(_, r)| !r.iter().any(|l| l == "Taken."))
            .unwrap();
        assert!(loser.iter().any(|l| l == "You don't see that here."), "loser declined: {loser:?}");

        // World invariant: exactly one HELD(_, lamp) row, and the lamp is no longer
        // on the floor.
        let held: Vec<Entity> = store
            .read_at(HELD, store.current())
            .unwrap()
            .into_iter()
            .filter(|(t, d)| *d > 0 && t.as_slice().get(1) == Some(&Value::Ent(lamp)))
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect();
        assert_eq!(held.len(), 1, "exactly one owner holds the lamp");
        assert!(held[0] == p1.player() || held[0] == p2.player());
        assert_eq!(located(&*store, lamp), None, "the lamp left the floor");
    });
}

#[test]
fn reconnecting_with_the_same_name_rebinds_the_same_player() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);
        let s1 = server.login("carol").unwrap();
        let first = s1.player();
        drop(s1);
        // Same name, same durable identity — no second entity allocated.
        let s2 = server.login("carol").unwrap();
        assert_eq!(s2.player(), first, "identity persists across reconnect");

        // A different name is a different player.
        let s3 = server.login("dave").unwrap();
        assert_ne!(s3.player(), first);
    });
}

#[test]
fn unknown_command_and_bad_noun_change_nothing() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);
        let mut p = server.login("eve").unwrap();
        assert_eq!(p.submit("frobnicate widget").unwrap(), vec!["Huh?"]);
        assert_eq!(p.submit("take moon").unwrap(), vec!["You don't see that here."]);
        assert_eq!(p.submit("go north").unwrap(), vec!["You can't go that way."]);
    });
}
