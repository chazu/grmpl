//! **The semantic core runs over the ent-native store.**
//!
//! The bright line says `grmpl-diff` (queries) and `grmpl-proc` (the optimistic
//! commit / process layer) depend only on the store *traits* — so they must run
//! over [`EntStore`] exactly as over the LSM. These tests drive the real query
//! engine and the real patch-commit path against the ent store end to end.

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, NoSchemas, Patch, RelId, Scope, TraceStore,
    Tuple, Value,
};
use grmpl_diff::{Query, Snapshot};
use grmpl_ent::EntStore;
use grmpl_proc::{commit_patch, CommitOutcome};

const LOCATED: RelId = RelId(1);
const NAMED: RelId = RelId(2);
const HELD: RelId = RelId(5);
const LAMP: Entity = Entity(100);
const ROOM: Entity = Entity(7);
const PLAYER: Entity = Entity(1);

fn e(x: Entity) -> Value {
    Value::Ent(x)
}

fn seed() -> EntStore {
    let s = EntStore::new();
    s.commit(&[
        (LOCATED, Tuple::from([e(LAMP), e(ROOM)]), 1),
        (LOCATED, Tuple::from([e(PLAYER), e(ROOM)]), 1),
        (NAMED, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1),
    ])
    .unwrap();
    s
}

#[test]
fn differential_query_runs_over_the_ent_store() {
    let store = seed();
    // A join: the named things co-located with the player — located ⋈ located ⋈
    // named — evaluated by the real grmpl-diff engine against an ent Snapshot.
    let here = Query::rel(LOCATED)
        .filter(|t| t.as_slice()[1] == e(ROOM))
        .project([0]); // (thing)
    let named = here.join(Query::rel(NAMED), [0], [0]).project([0, 2]); // (thing, name)
    let rows = named.find(&Snapshot::at_current(&store)).unwrap();
    assert_eq!(rows, vec![(Tuple::from([e(LAMP), Value::text("brass lamp")]), 1)]);
}

#[test]
fn optimistic_patch_commit_runs_over_the_ent_store() {
    let store = seed();
    let authority = Authority::new(
        DomainId(1),
        vec![Scope::whole(LOCATED), Scope::whole(HELD)],
    );
    // take lamp: guard it's in the room, move it into `held` — the real
    // grmpl-proc optimistic commit against the ent store.
    let lamp_here = Fact::new(LOCATED, Tuple::from([e(LAMP), e(ROOM)]));
    let take = Patch::new()
        .expect(lamp_here.clone())
        .retract(lamp_here)
        .assert(Fact::new(HELD, Tuple::from([e(PLAYER), e(LAMP)])));
    let outcome = commit_patch(&store, &NoSchemas, &take, &authority).unwrap();
    assert!(matches!(outcome, CommitOutcome::Committed(_)));

    let cur = store.current();
    assert_eq!(
        store.read_at(HELD, cur).unwrap(),
        vec![(Tuple::from([e(PLAYER), e(LAMP)]), 1)]
    );
    assert!(!store
        .read_at(LOCATED, cur)
        .unwrap()
        .into_iter()
        .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)])));

    // The guarded take is genuinely optimistic: repeating it now fails its
    // precondition (the lamp already left the room) and has no effect.
    let lamp_here = Fact::new(LOCATED, Tuple::from([e(LAMP), e(ROOM)]));
    let again = Patch::new()
        .expect(lamp_here.clone())
        .retract(lamp_here)
        .assert(Fact::new(HELD, Tuple::from([e(PLAYER), e(LAMP)])));
    assert!(matches!(
        commit_patch(&store, &NoSchemas, &again, &authority).unwrap(),
        CommitOutcome::Rejected
    ));
}
