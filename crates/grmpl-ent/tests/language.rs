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
fn version_compare_reports_what_changed_between_editions() {
    let store = seed();
    let before = store.current();
    // take the lamp: it leaves LOCATED, appears in HELD.
    store
        .commit(&[
            (LOCATED, Tuple::from([e(LAMP), e(ROOM)]), -1),
            (HELD, Tuple::from([e(PLAYER), e(LAMP)]), 1),
        ])
        .unwrap();
    let after = store.current();

    // LOCATED between the two editions: the lamp's row went from weight 1 to gone.
    let d = store.version_compare(LOCATED, before, after).unwrap();
    assert_eq!(d, vec![(Tuple::from([e(LAMP), e(ROOM)]), Some(1), None)]);

    // HELD: the player-lamp row appeared.
    let d = store.version_compare(HELD, before, after).unwrap();
    assert_eq!(d, vec![(Tuple::from([e(PLAYER), e(LAMP)]), None, Some(1))]);

    // NAMED was untouched — the versions are shared, so O(1) empty diff.
    assert!(store.version_compare(NAMED, before, after).unwrap().is_empty());
    // A relation compared to itself is empty.
    assert!(store.version_compare(LOCATED, after, after).unwrap().is_empty());
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
fn range_operator_prunes_at_the_source_and_matches_the_filter() {
    use grmpl_diff::{eval_delta, eval_snapshot};

    let store = EntStore::new();
    let updates: Vec<(RelId, Tuple, i64)> =
        (0..40).map(|k| (RelId(1), Tuple::from([Value::Int(k)]), 1)).collect();
    store.commit(&updates).unwrap();
    let at = store.current();

    let sorted = |m: grmpl_diff::Multiset| {
        let mut v: Vec<(Tuple, i64)> = m.into_iter().collect();
        v.sort();
        v
    };

    for (a, b) in [(-5i64, 5), (10, 10), (7, 33), (0, 40), (30, 100)] {
        let lo = Tuple::from([Value::Int(a)]);
        let hi = Tuple::from([Value::Int(b)]);
        // The pushed-down range operator equals the equivalent Rel-then-filter,
        // run by the real engine — but the Ent's `read_range` prunes at the source.
        let ranged = Query::range(RelId(1), lo.clone(), hi.clone());
        let filtered = Query::rel(RelId(1)).filter({
            let (lo, hi) = (lo.clone(), hi.clone());
            move |t| lo <= *t && *t < hi
        });
        assert_eq!(
            sorted(eval_snapshot(&ranged, &store, at).unwrap()),
            sorted(eval_snapshot(&filtered, &store, at).unwrap()),
            "range [{a},{b}) snapshot"
        );
    }

    // The operator composes in a join: pair each in-range key with itself.
    let lo = Tuple::from([Value::Int(10)]);
    let hi = Tuple::from([Value::Int(20)]);
    let joined = Query::range(RelId(1), lo, hi).join(Query::rel(RelId(1)), [0], [0]);
    let rows = sorted(eval_snapshot(&joined, &store, at).unwrap());
    assert_eq!(rows.len(), 10, "join over the range should yield 10 self-pairs");
    assert!(rows.iter().all(|(t, d)| *d == 1 && t.as_slice()[0] == t.as_slice()[1]));

    // The delta of the range operator over an edition step is the in-range change.
    let before = store.current();
    store.commit(&[(RelId(1), Tuple::from([Value::Int(15)]), -1), (RelId(1), Tuple::from([Value::Int(99)]), 1)]).unwrap();
    let after = store.current();
    let d = sorted(
        eval_delta(&Query::range(RelId(1), Tuple::from([Value::Int(10)]), Tuple::from([Value::Int(20)])), &store, before, after).unwrap(),
    );
    // 15 left the range; 99 is out of range, so it does not appear.
    assert_eq!(d, vec![(Tuple::from([Value::Int(15)]), -1)]);
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
fn dsp_instancing_makes_disjoint_private_sub_worlds() {
    const EXITS: RelId = RelId(3);
    const VALUE: RelId = RelId(4);
    let store = EntStore::new();

    // A self-contained vault template in the id block [1000, 1010): an antechamber
    // (1000) with a torch (1002) in it, a north exit to the vault proper (1001).
    store
        .commit(&[
            (LOCATED, Tuple::from([e(Entity(1002)), e(Entity(1000))]), 1),
            (NAMED, Tuple::from([e(Entity(1000)), Value::text("Antechamber")]), 1),
            (NAMED, Tuple::from([e(Entity(1001)), Value::text("Vault")]), 1),
            (NAMED, Tuple::from([e(Entity(1002)), Value::text("torch")]), 1),
            (EXITS, Tuple::from([e(Entity(1000)), Value::text("north"), e(Entity(1001))]), 1),
            (VALUE, Tuple::from([e(Entity(1002)), Value::Int(5)]), 1),
        ])
        .unwrap();

    let rels = [LOCATED, NAMED, EXITS, VALUE];
    // Two players enter: instance the template into disjoint blocks.
    store.instance_template(&rels, 1000, 1010, 1000).unwrap(); // block 2000s
    store.instance_template(&rels, 1000, 1010, 2000).unwrap(); // block 3000s

    let at = store.current();
    let located: Vec<(Tuple, i64)> = store.read_at(LOCATED, at).unwrap();
    let has = |t: Tuple| located.iter().any(|(x, d)| *d > 0 && *x == t);
    // The template survives, and each instance has its own relocated torch.
    assert!(has(Tuple::from([e(Entity(1002)), e(Entity(1000))])), "template torch gone");
    assert!(has(Tuple::from([e(Entity(2002)), e(Entity(2000))])), "instance A torch missing");
    assert!(has(Tuple::from([e(Entity(3002)), e(Entity(3000))])), "instance B torch missing");

    // The exit relocated with both endpoints together; the direction text is kept.
    let exits: Vec<(Tuple, i64)> = store.read_at(EXITS, at).unwrap();
    assert!(exits.iter().any(|(t, d)| *d > 0
        && *t == Tuple::from([e(Entity(2000)), Value::text("north"), e(Entity(2001))])));

    // Mutating instance A (take the torch: it leaves LOCATED) touches neither
    // instance B nor the template — the sub-worlds are genuinely disjoint.
    store.commit(&[(LOCATED, Tuple::from([e(Entity(2002)), e(Entity(2000))]), -1)]).unwrap();
    let at2 = store.current();
    let located2: Vec<(Tuple, i64)> = store.read_at(LOCATED, at2).unwrap();
    let has2 = |t: Tuple| located2.iter().any(|(x, d)| *d > 0 && *x == t);
    assert!(!has2(Tuple::from([e(Entity(2002)), e(Entity(2000))])), "A's torch should be gone");
    assert!(has2(Tuple::from([e(Entity(3002)), e(Entity(3000))])), "B's torch wrongly affected");
    assert!(has2(Tuple::from([e(Entity(1002)), e(Entity(1000))])), "template wrongly affected");
}

#[test]
fn fork_family_records_ancestry_in_the_dagwood() {
    // The DagWood (fulltrace branch structure): forks share one branch DAG, so
    // ancestry is queryable across the whole family even as each branch diverges.
    let root = EntStore::new();
    root.commit(&[(LOCATED, Tuple::from([e(LAMP), e(ROOM)]), 1)]).unwrap();
    let fork_point = root.current();

    // Fork the root, then fork the child again — a three-branch family.
    let child = root.fork_at(fork_point).unwrap();
    let grandchild = child.fork_at(child.current()).unwrap();

    // Distinct branch ids; root is branch 0.
    assert_eq!(root.branch_id(), 0);
    assert_ne!(child.branch_id(), root.branch_id());
    assert_ne!(grandchild.branch_id(), child.branch_id());

    // Each branch evolves independently after the fork.
    root.commit(&[(NAMED, Tuple::from([e(ROOM), Value::text("root world")]), 1)]).unwrap();
    child.commit(&[(NAMED, Tuple::from([e(ROOM), Value::text("child world")]), 1)]).unwrap();
    grandchild.commit(&[(NAMED, Tuple::from([e(ROOM), Value::text("grandchild")]), 1)]).unwrap();

    // Descent flows down the fork chain, not up or sideways.
    assert!(child.descends_from(&root, fork_point), "child must descend from root");
    assert!(grandchild.descends_from(&root, fork_point), "grandchild descends from root");
    assert!(!root.descends_from(&child, child.current()), "root must not descend from its fork");
    assert!(
        !grandchild.descends_from(&root, root.current()),
        "grandchild never saw root's post-fork commit"
    );

    // A sibling fork of the root is a cousin of the child — neither descends from
    // the other, and their merge base is the shared root at the fork point.
    let sibling = root.fork_at(fork_point).unwrap();
    assert!(!sibling.descends_from(&child, child.current()));
    assert!(!child.descends_from(&sibling, sibling.current()));
    assert_eq!(
        child.common_ancestor_with(&sibling),
        Some((root.branch_id(), fork_point.0)),
        "cousins should meet at the root fork point"
    );

    // A store from a disjoint DagWood shares no ancestry.
    let stranger = EntStore::new();
    assert!(!child.descends_from(&stranger, stranger.current()));
    assert_eq!(child.common_ancestor_with(&stranger), None);
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
