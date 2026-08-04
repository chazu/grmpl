//! **The session layer without a writer lock.**
//!
//! P3 shipped `Server` with `lock: Mutex<()>` around every commit, because the
//! entity counter's `seal` was an unguarded retract/assert: two concurrent
//! spawns would both allocate the same id. With [`Alloc`](grmpl_proc::Alloc)
//! guarded, that lock is gone and the session layer is serialized only by the
//! store.
//!
//! These are the laws that replace it. Each one is a *world* invariant checked
//! against the committed trace, not an assertion about what a thread believed —
//! and each is reachable only now that clients can commit concurrently:
//!
//! 1. **No id is handed out twice.** Concurrent `dig`/`create` from several
//!    clients produce distinct entities, and every command lands.
//! 2. **One name, one identity.** Concurrent logins of the *same* new name bind
//!    exactly one player entity — the loser of the counter race re-reads the
//!    world, finds the winner's `PLAYER` row, and adopts it rather than spawning
//!    a second player.
//! 3. **Exactly one taker.** The optimistic race that P3 could only test
//!    sequentially now runs for real: N clients race one lamp through their own
//!    sockets-equivalent command path, exactly one is told "Taken.", and every
//!    loser is *told something* — the retry re-decides against the winner's
//!    world instead of silently dropping the command.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use grmpl_core::{Entity, TraceStore, Value};
use grmpl_session::world::{HELD, LOCATED, NAMED, PLAYER};
use grmpl_session::Server;

fn server(case: &grmpl_conformance::Case) -> Arc<Server> {
    let store: Arc<dyn TraceStore> = case.trace();
    let server = Server::new(store);
    server.init().unwrap();
    server
}

/// Every entity that carries a `NAMED` row, with its name.
fn named(store: &dyn TraceStore) -> Vec<(Entity, String)> {
    let at = store.current();
    store
        .read_at(NAMED, at)
        .unwrap()
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .filter_map(|(t, _)| match (t.as_slice().first(), t.as_slice().get(1)) {
            (Some(Value::Ent(e)), Some(Value::Text(n))) => Some((*e, n.to_string())),
            _ => None,
        })
        .collect()
}

/// Law 1 — concurrent world construction hands out no id twice.
#[test]
fn concurrent_builders_never_collide_on_an_entity_id() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);
        const BUILDERS: usize = 4;
        const EACH: usize = 5;

        thread::scope(|scope| {
            for b in 0..BUILDERS {
                let server = Arc::clone(&server);
                scope.spawn(move || {
                    let mut s = server.login(&format!("builder{b}")).unwrap();
                    for k in 0..EACH {
                        let out = s.submit(&format!("create thing-{b}-{k}")).unwrap();
                        assert_eq!(
                            out,
                            vec![format!("Created thing-{b}-{k}.")],
                            "every command lands — a lost race retries, it does not vanish"
                        );
                    }
                });
            }
        });

        let store = Arc::clone(server.store());
        let rows = named(&*store);

        // Every created thing is present, exactly once.
        for b in 0..BUILDERS {
            for k in 0..EACH {
                let want = format!("thing-{b}-{k}");
                let hits = rows.iter().filter(|(_, n)| *n == want).count();
                assert_eq!(hits, 1, "`{want}` must appear exactly once, saw {hits}");
            }
        }

        // No entity id was handed to two different things. This is the law the
        // unguarded counter broke: it would have produced two `NAMED` rows
        // sharing one entity.
        let ids: Vec<Entity> = rows.iter().map(|(e, _)| *e).collect();
        let distinct: HashSet<Entity> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            distinct.len(),
            "no entity id is shared by two things: {rows:?}"
        );
    });
}

/// Law 2 — racing logins of one name bind one identity.
#[test]
fn concurrent_logins_of_one_name_bind_exactly_one_player() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);
        const CONNECTIONS: usize = 6;

        let players: Vec<Entity> = thread::scope(|scope| {
            let handles: Vec<_> = (0..CONNECTIONS)
                .map(|_| {
                    let server = Arc::clone(&server);
                    scope.spawn(move || server.login("mallory").unwrap().player())
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let distinct: HashSet<Entity> = players.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            1,
            "one name is one durable identity even under concurrent logins: {players:?}"
        );

        // And the world holds exactly one PLAYER row for that name — the losers
        // adopted the winner's identity, they did not each spawn one.
        let store = Arc::clone(server.store());
        let at = store.current();
        let bound: Vec<Entity> = store
            .read_at(PLAYER, at)
            .unwrap()
            .into_iter()
            .filter(|(t, d)| *d > 0 && t.as_slice().get(1) == Some(&Value::text("mallory")))
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect();
        assert_eq!(bound, vec![players[0]], "exactly one durable PLAYER binding");

        // Distinct names still get distinct players.
        let a = server.login("norah").unwrap().player();
        let b = server.login("oscar").unwrap().player();
        assert_ne!(a, b);
        assert!(!distinct.contains(&a) && !distinct.contains(&b));
    });
}

/// Law 3 — the take race, run concurrently for real.
#[test]
fn racing_clients_yield_exactly_one_taker_and_no_silent_losers() {
    grmpl_conformance::for_each_store(|c| {
        let server = server(c);

        // Build the room and the lamp through a client, as P3 requires.
        let mut builder = server.login("builder").unwrap();
        assert_eq!(builder.submit("dig hall").unwrap(), vec!["Dug hall."]);
        assert_eq!(builder.submit("go hall").unwrap(), vec!["You move."]);
        assert_eq!(builder.submit("create lamp").unwrap(), vec!["Created lamp."]);

        let store = Arc::clone(server.store());
        let lamp = named(&*store)
            .into_iter()
            .find(|(_, n)| n == "lamp")
            .map(|(e, _)| e)
            .expect("lamp was created");

        const RACERS: usize = 6;
        // Everyone walks into the hall first (sequentially — the race is the take).
        let mut sessions: Vec<_> = (0..RACERS)
            .map(|i| {
                let mut s = server.login(&format!("racer{i}")).unwrap();
                assert_eq!(s.submit("go hall").unwrap(), vec!["You move."]);
                s
            })
            .collect();

        let results: Vec<Vec<String>> = thread::scope(|scope| {
            let handles: Vec<_> = sessions
                .iter_mut()
                .map(|s| scope.spawn(move || s.submit("take lamp").unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let winners = results.iter().filter(|r| r.iter().any(|l| l == "Taken.")).count();
        assert_eq!(winners, 1, "exactly one taker: {results:?}");

        // No loser is silent. The retry re-runs the behavior against the winner's
        // world, so every loser gets a real answer — before the guard + retry
        // landed, a lost race left the command unconsumed and the client with
        // nothing at all.
        for r in &results {
            assert!(!r.is_empty(), "no client is left without a reply: {results:?}");
        }

        // World invariant: one holder, and the lamp left the floor.
        let at = store.current();
        let held: Vec<Entity> = store
            .read_at(HELD, at)
            .unwrap()
            .into_iter()
            .filter(|(t, d)| *d > 0 && t.as_slice().get(1) == Some(&Value::Ent(lamp)))
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect();
        assert_eq!(held.len(), 1, "exactly one owner holds the lamp");
        let on_floor = store
            .read_at(LOCATED, at)
            .unwrap()
            .into_iter()
            .any(|(t, d)| d > 0 && t.as_slice().first() == Some(&Value::Ent(lamp)));
        assert!(!on_floor, "the lamp left the floor");
    });
}
