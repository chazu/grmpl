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
fn wid_range_read_and_count_match_a_full_scan() {
    let store = EntStore::new();
    // Seed rel 1 with single-column tuples Int(0..40).
    let updates: Vec<(RelId, Tuple, i64)> =
        (0..40).map(|k| (RelId(1), Tuple::from([Value::Int(k)]), 1)).collect();
    store.commit(&updates).unwrap();
    let at = store.current();

    for (a, b) in [(-5i64, 5), (10, 10), (7, 33), (0, 40), (30, 100), (50, 60)] {
        let lo = Tuple::from([Value::Int(a)]);
        let hi = Tuple::from([Value::Int(b)]);
        // Ground truth: the full sorted state filtered to the half-open span.
        let want: Vec<(Tuple, i64)> = store
            .read_at(RelId(1), at)
            .unwrap()
            .into_iter()
            .filter(|(t, _)| lo <= *t && *t < hi)
            .collect();
        assert_eq!(store.range_at(RelId(1), at, &lo, &hi).unwrap(), want, "range [{a},{b})");
        assert_eq!(store.count_at(RelId(1), at, &lo, &hi).unwrap(), want.len() as u64, "count [{a},{b})");
    }
}

#[test]
fn structural_sharing_fork_diverges_independently() {
    let parent = EntStore::new();
    parent
        .commit(&[
            (LOCATED, Tuple::from([e(LAMP), e(ROOM)]), 1),
            (LOCATED, Tuple::from([e(PLAYER), e(ROOM)]), 1),
        ])
        .unwrap();
    let mid = parent.current();
    parent
        .commit(&[(NAMED, Tuple::from([e(LAMP), Value::text("brass lamp")]), 1)])
        .unwrap();

    // Fork at current: byte-identical observable state (shared enfilade nodes).
    let fork = parent.fork_at(parent.current()).unwrap();
    for r in [LOCATED, NAMED] {
        assert_eq!(
            fork.read_at(r, fork.current()).unwrap(),
            parent.read_at(r, parent.current()).unwrap(),
            "fork state differs from parent at fork point"
        );
    }

    // Diverge: remove the lamp in the fork only.
    fork.commit(&[(LOCATED, Tuple::from([e(LAMP), e(ROOM)]), -1)]).unwrap();
    let lamp_here = |s: &EntStore| {
        s.read_at(LOCATED, s.current())
            .unwrap()
            .into_iter()
            .any(|(t, d)| d > 0 && t == Tuple::from([e(LAMP), e(ROOM)]))
    };
    assert!(lamp_here(&parent), "parent was mutated by a commit to the fork");
    assert!(!lamp_here(&fork), "fork did not diverge");

    // Fork at a past edition reproduces the parent's state then.
    let fork_mid = parent.fork_at(mid).unwrap();
    assert_eq!(fork_mid.current(), mid);
    assert!(
        fork_mid.read_at(NAMED, fork_mid.current()).unwrap().is_empty(),
        "a past fork saw a fact committed after the fork point"
    );
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
