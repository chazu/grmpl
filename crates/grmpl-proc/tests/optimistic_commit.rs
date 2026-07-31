//! M4 acceptance (DESIGN.md §9): two concurrent `take lamp` from the same
//! edition → exactly one wins, the other has no effect and retries.

use std::sync::atomic::{AtomicUsize, Ordering};

use grmpl_core::{
    Authority, DomainId, Edition, Entity, Fact, Patch, Scope, Tuple, Value,
};
use grmpl_core::NoSchemas;
use grmpl_proc::{commit_patch, CommitOutcome};

const LOCATED: RelId = grmpl_core::RelId(1); // (thing, place)
const HELD: RelId = grmpl_core::RelId(5); //    (owner, thing)
use grmpl_core::RelId;

fn located(thing: u64, place: u64) -> Fact {
    Fact::new(LOCATED, Tuple::from([Value::Ent(Entity(thing)), Value::Ent(Entity(place))]))
}
fn held(owner: u64, thing: u64) -> Fact {
    Fact::new(HELD, Tuple::from([Value::Ent(Entity(owner)), Value::Ent(Entity(thing))]))
}

const LAMP: u64 = 100;
const ROOM: u64 = 7;

/// `take lamp` for player `p`: expect the lamp is here, move it to the player.
fn take_lamp(p: u64) -> Patch {
    Patch::new()
        .expect(located(LAMP, ROOM))
        .retract(located(LAMP, ROOM))
        .assert(held(p, LAMP))
}

fn world_authority() -> Authority {
    Authority::new(DomainId(1), vec![Scope::whole(LOCATED), Scope::whole(HELD)])
}

#[test]
fn concurrent_takers_exactly_one_wins() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        store.commit(&[(LOCATED, located(LAMP, ROOM).tuple, 1)]).unwrap();

        let auth = world_authority();
        let winners = AtomicUsize::new(0);
        const N: u64 = 8;

        std::thread::scope(|s| {
            for p in 0..N {
                let auth = &auth;
                let winners = &winners;
                s.spawn(move || {
                    if let CommitOutcome::Committed(_) = commit_patch(store, &NoSchemas, &take_lamp(p), auth).unwrap() {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        // Exactly one taker committed.
        assert_eq!(winners.load(Ordering::SeqCst), 1);

        // The lamp left the room, and exactly one player holds it.
        let cur = store.current();
        let in_room = store
            .read_at(LOCATED, cur)
            .unwrap()
            .into_iter()
            .filter(|(t, _)| *t == located(LAMP, ROOM).tuple)
            .count();
        assert_eq!(in_room, 0, "lamp should have left the room");

        let holders: Vec<_> = store
            .read_at(HELD, cur)
            .unwrap()
            .into_iter()
            .filter(|(t, d)| *d > 0 && t.as_slice()[1] == Value::Ent(Entity(LAMP)))
            .collect();
        assert_eq!(holders.len(), 1, "exactly one holder of the lamp");
    });
}

#[test]
fn loser_is_rejected_and_retry_has_no_effect() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        store.commit(&[(LOCATED, located(LAMP, ROOM).tuple, 1)]).unwrap();
        let auth = world_authority();

        // Both built from edition 1. First wins.
        let out1 = commit_patch(store, &NoSchemas, &take_lamp(1), &auth).unwrap();
        assert_eq!(out1, CommitOutcome::Committed(Edition(2)));

        // Second's precondition (lamp in room) no longer holds → no effect.
        let out2 = commit_patch(store, &NoSchemas, &take_lamp(2), &auth).unwrap();
        assert_eq!(out2, CommitOutcome::Rejected);

        // A naive retry of the same patch still fails — the world moved on.
        let retry = commit_patch(store, &NoSchemas, &take_lamp(2), &auth).unwrap();
        assert_eq!(retry, CommitOutcome::Rejected);

        // Player 1 holds the lamp; player 2 never did.
        let cur = store.current();
        assert_eq!(cur, Edition(2), "only one successful commit advanced the clock");
        let held_by_1 = store.read_at(HELD, cur).unwrap();
        assert_eq!(held_by_1, vec![(held(1, LAMP).tuple, 1)]);
    });
}

#[test]
fn write_outside_authority_is_rejected() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        // Authority owns only HELD, not LOCATED.
        let auth = Authority::new(DomainId(1), vec![Scope::whole(HELD)]);
        let patch = Patch::new().retract(located(LAMP, ROOM)).assert(held(1, LAMP));
        let err = commit_patch(store, &NoSchemas, &patch, &auth).unwrap_err();
        assert!(matches!(err, grmpl_core::Error::Authority(_)));
    });
}
