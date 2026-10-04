//! **Merges: one branch's patches replayed onto another (Gold's
//! `newSuccessorAfter:`, by replay).**
//!
//! `EntStore::merge` makes a branch with two parents. It starts from the
//! merging store's present and replays every patch of the other's history that
//! has not already reached it, re-checking each patch's preconditions as a
//! racing commit would be. Pinned here:
//!
//! 1. **The replay law**, against a model: the merge equals the merging state
//!    with the other side's patches applied in order, or, if a precondition
//!    fails on the way, is refused at that patch and creates nothing.
//! 2. **Only what is missing is replayed**: merging again replays nothing, a
//!    later merge replays only what came since, merging an ancestor replays
//!    nothing, and a grandchild's merge carries its parent's patches too.
//! 3. **Grafts replay**, and conflict when the target block is taken.
//! 4. **Catalogs unite**; a name bound two ways is a conflict.
//! 5. Consolidated history cannot be replayed; a merge survives a reopen.

use std::collections::BTreeMap;

use grmpl_core::{Catalog, Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, MergeOutcome};

const R: RelId = RelId(1);
const S: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn row(e: u64, tag: i64) -> Tuple {
    Tuple::from([ent(e), Value::Int(tag)])
}

/// Deterministic xorshift64*.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type State = BTreeMap<(RelId, Tuple), Diff>;

fn state(s: &EntStore) -> State {
    let mut out = State::new();
    for rel in [R, S] {
        for (t, d) in s.read_at(rel, s.current()).unwrap() {
            out.insert((rel, t), d);
        }
    }
    out
}

/// One patch as committed: its preconditions and updates.
#[derive(Clone, Debug)]
struct Patch {
    pre: Vec<(RelId, Tuple)>,
    updates: Vec<(RelId, Tuple, Diff)>,
}

/// Commit a random patch on `s`, as it sees its own state: a plain insert or
/// retract, or a guarded move (retract a row it holds, assert another, on the
/// condition the row still holds) — the shape that conflicts.
fn random_patch(s: &EntStore, rng: &mut Rng) -> Patch {
    let now = state(s);
    let held: Vec<&(RelId, Tuple)> = now.keys().collect();
    let patch = if !held.is_empty() && rng.below(2) == 0 {
        let (rel, t) = held[rng.below(held.len() as u64) as usize].clone();
        let moved = row(rng.below(12), rng.below(2) as i64);
        let mut updates = vec![(rel, t.clone(), -now[&(rel, t.clone())])];
        if !now.contains_key(&(rel, moved.clone())) && moved != t {
            updates.push((rel, moved, 1));
        }
        Patch { pre: vec![(rel, t)], updates }
    } else {
        let rel = if rng.below(3) == 0 { S } else { R };
        let t = row(rng.below(60), rng.below(3) as i64);
        let d = if now.contains_key(&(rel, t.clone())) { -now[&(rel, t.clone())] } else { 1 };
        Patch { pre: Vec::new(), updates: vec![(rel, t, d)] }
    };
    if patch.pre.is_empty() {
        s.commit(&patch.updates).unwrap();
    } else {
        assert!(s.commit_if(&patch.pre, &patch.updates).unwrap().is_some(), "a patch on its own branch");
    }
    patch
}

/// The model's merge: `base` with `patches` applied in order, or the index of
/// the first patch whose preconditions fail.
fn model_merge(mut base: State, patches: &[Patch]) -> Result<State, usize> {
    for (i, p) in patches.iter().enumerate() {
        if p.pre.iter().any(|k| base.get(k).is_none_or(|d| *d <= 0)) {
            return Err(i);
        }
        for (rel, t, d) in &p.updates {
            let w = base.entry((*rel, t.clone())).or_insert(0);
            *w += d;
            if *w == 0 {
                base.remove(&(*rel, t.clone()));
            }
        }
    }
    Ok(base)
}

fn seed(s: &EntStore) {
    let ups: Vec<_> = (0..40).map(|e| (R, row(e, 0), 1)).chain((0..10).map(|e| (S, row(e, 0), 1))).collect();
    s.commit(&ups).unwrap();
}

#[test]
fn a_merge_replays_the_other_side_or_refuses_at_the_first_conflict() {
    let (mut merged, mut refused) = (0, 0);
    for seed_n in 1..=60u64 {
        let mut rng = Rng(seed_n);
        let t = EntStore::new();
        // A small world, so the two sides contend for the same rows.
        t.commit(&(0..8).map(|e| (R, row(e, 0), 1)).chain((0..3).map(|e| (S, row(e, 0), 1))).collect::<Vec<_>>())
            .unwrap();
        for _ in 0..rng.below(5) {
            random_patch(&t, &mut rng);
        }
        let s = t.fork_at(t.current()).unwrap();
        let mut theirs = Vec::new();
        for _ in 0..2 + rng.below(12) {
            if rng.below(2) == 0 {
                random_patch(&t, &mut rng);
            } else {
                theirs.push((s.current().0 + 1, random_patch(&s, &mut rng)));
            }
        }
        let branches_before = t.dag().branches().len();
        let patches: Vec<Patch> = theirs.iter().map(|(_, p)| p.clone()).collect();
        match (t.merge(&s).unwrap(), model_merge(state(&t), &patches)) {
            (MergeOutcome::Merged(m), Ok(want)) => {
                merged += 1;
                assert_eq!(state(&m), want, "seed {seed_n}");
                assert_eq!(m.current().0, t.current().0 + patches.len() as u64, "one edition per patch");
                let dag = m.dag();
                let rec = dag.get(m.branch_id()).unwrap();
                assert_eq!(rec.parent, Some((t.branch_id(), t.current().0)));
                assert_eq!(rec.merged, Some((s.branch_id(), s.current().0, m.current().0)));
                assert!(m.descends_from(&t, t.current()) && m.descends_from(&s, s.current()));
            }
            (MergeOutcome::Conflict(c), Err(i)) => {
                refused += 1;
                assert_eq!((c.branch, c.edition.0), (s.branch_id(), theirs[i].0), "seed {seed_n}: {c:?}");
                assert_eq!(t.dag().branches().len(), branches_before, "a refused merge made a branch");
            }
            (MergeOutcome::Merged(_), Err(i)) => panic!("seed {seed_n}: merged, but patch {i} should conflict"),
            (MergeOutcome::Conflict(c), Ok(_)) => panic!("seed {seed_n}: refused at {c:?}, but the model merges"),
        }
    }
    assert!(merged > 10 && refused > 10, "the law saw {merged} merges and {refused} conflicts");
}

/// Merge, insisting it succeeds.
fn merge(a: &EntStore, b: &EntStore) -> EntStore {
    match a.merge(b).unwrap() {
        MergeOutcome::Merged(m) => m,
        MergeOutcome::Conflict(c) => panic!("unexpected conflict {c:?}"),
    }
}

#[test]
fn only_what_is_missing_is_replayed() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    s.commit(&[(R, row(50, 1), 1)]).unwrap();
    t.commit(&[(R, row(51, 1), 1)]).unwrap();
    let m = merge(&t, &s);
    assert_eq!(m.current().0, t.current().0 + 1);

    // Again: everything of `s` already flowed into `m`.
    let again = merge(&m, &s);
    assert_eq!((again.current(), state(&again)), (m.current(), state(&m)));
    // Merging an ancestor: `t`'s history is all behind `m`.
    let anc = merge(&m, &t);
    assert_eq!(anc.current(), m.current());

    // Later patches of `s`: only they are replayed.
    s.commit(&[(R, row(52, 1), 1)]).unwrap();
    s.commit(&[(S, row(53, 1), 1)]).unwrap();
    let later = merge(&m, &s);
    assert_eq!(later.current().0, m.current().0 + 2);
    assert!(state(&later).contains_key(&(S, row(53, 1))));

    // The other direction: `s` takes `t`'s side, and sees the same world.
    let back = merge(&s, &t);
    let mut want = state(&s);
    want.insert((R, row(51, 1)), 1);
    assert_eq!(state(&back), want);
}

#[test]
fn a_grandchild_carries_its_parents_patches() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    s.commit(&[(R, row(50, 1), 1)]).unwrap();
    let x = s.fork_at(s.current()).unwrap();
    s.commit(&[(R, row(51, 1), 1)]).unwrap(); // after x forked: not x's
    x.commit(&[(R, row(52, 1), 1)]).unwrap();
    let m = merge(&t, &x);
    let got = state(&m);
    assert!(got.contains_key(&(R, row(50, 1))), "the parent's patch before the fork");
    assert!(!got.contains_key(&(R, row(51, 1))), "the parent's patch after the fork");
    assert!(got.contains_key(&(R, row(52, 1))), "the grandchild's own patch");
    assert_eq!(m.current().0, t.current().0 + 2);
}

#[test]
fn grafts_replay_and_conflict_on_a_taken_block() {
    let t = EntStore::new();
    t.commit(&(0..20).map(|e| (R, row(e, 0), 1)).collect::<Vec<_>>()).unwrap();
    let s = t.fork_at(t.current()).unwrap();
    s.instance_template(&[R], 0, 20, 1_000).unwrap();
    let m = merge(&t, &s);
    assert_eq!(m.read_at(R, m.current()).unwrap().len(), 40, "the copy came across");
    assert_eq!(m.copies_of(0, 20, m.current()).len(), 1, "and its record in the spanfilade");

    // `t` fills the target block first: the replayed graft is refused.
    let u = EntStore::new();
    u.commit(&(0..20).map(|e| (R, row(e, 0), 1)).collect::<Vec<_>>()).unwrap();
    let v = u.fork_at(u.current()).unwrap();
    v.instance_template(&[R], 0, 20, 1_000).unwrap();
    u.commit(&[(R, row(1_005, 0), 1)]).unwrap();
    match u.merge(&v).unwrap() {
        MergeOutcome::Conflict(c) => assert!(c.reason.contains("target block"), "{c:?}"),
        MergeOutcome::Merged(_) => panic!("a graft over a taken block merged"),
    }
}

#[test]
fn catalogs_unite_and_a_name_bound_twice_conflicts() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    s.register("only_on_s", RelId(9)).unwrap();
    let m = merge(&t, &s);
    assert_eq!(m.rel_id("only_on_s").unwrap(), Some(RelId(9)));

    let u = t.fork_at(t.current()).unwrap();
    u.register("contested", RelId(10)).unwrap();
    let w = t.fork_at(t.current()).unwrap();
    w.register("contested", RelId(11)).unwrap();
    match u.merge(&w).unwrap() {
        MergeOutcome::Conflict(c) => assert!(c.reason.contains("bound differently"), "{c:?}"),
        MergeOutcome::Merged(_) => panic!("a name bound two ways merged"),
    }
}

#[test]
fn consolidated_history_cannot_be_replayed() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    for e in 0..5 {
        s.commit(&[(R, row(100 + e, 0), 1)]).unwrap();
    }
    s.consolidate(Edition(s.current().0 - 1)).unwrap();
    assert!(t.merge(&s).is_err(), "patches below the watermark were replayed");
}

#[test]
fn a_merge_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (m_id, want) = {
        let t = EntStore::open(dir.path()).unwrap();
        seed(&t);
        let s = t.fork_at(t.current()).unwrap();
        s.commit(&[(R, row(50, 1), 1)]).unwrap();
        let m = merge(&t, &s);
        (m.branch_id(), state(&m))
    };
    let root = EntStore::open(dir.path()).unwrap();
    let m = root.branch(m_id).unwrap();
    assert_eq!(state(&m), want);
    let rec = m.dag().get(m_id).unwrap();
    assert!(rec.merged.is_some(), "the second parent was not persisted");
    // Merging the same side again replays nothing, so the patch logs persisted.
    let s = root.branch(rec.merged.unwrap().0).unwrap();
    assert_eq!(merge(&m, &s).current(), m.current());
}

#[test]
fn merging_a_merge_replays_nothing_twice() {
    let t = EntStore::new();
    seed(&t);
    let u = t.fork_at(t.current()).unwrap();
    let s = t.fork_at(t.current()).unwrap();
    s.commit(&[(R, row(50, 1), 1)]).unwrap();
    t.commit(&[(R, row(51, 1), 1)]).unwrap();
    let m = merge(&t, &s);
    // `u` lacks `t`'s patch, `s`'s patch, and `m`'s copy of `s`'s patch. The
    // copy names its original, which is replayed from `s`: once.
    let w = merge(&u, &m);
    let got = state(&w);
    assert_eq!(got.get(&(R, row(50, 1))), Some(&1), "replayed twice");
    assert_eq!(got.get(&(R, row(51, 1))), Some(&1));
    assert_eq!(w.current().0, u.current().0 + 2);
}

#[test]
fn a_conflict_through_a_merge_names_the_original_patch() {
    let t = EntStore::new();
    seed(&t);
    let u = t.fork_at(t.current()).unwrap();
    let s = t.fork_at(t.current()).unwrap();
    // `s` moves row 3, guarded on it; `u` deletes it.
    let moved = s.commit_if(&[(R, row(3, 0))], &[(R, row(3, 0), -1), (R, row(45, 0), 1)]).unwrap().unwrap();
    u.commit(&[(R, row(3, 0), -1)]).unwrap();
    let m = merge(&t, &s);
    match u.merge(&m).unwrap() {
        MergeOutcome::Conflict(c) => assert_eq!((c.branch, c.edition), (s.branch_id(), moved), "{c:?}"),
        MergeOutcome::Merged(_) => panic!("a move of a deleted row merged"),
    }
}

#[test]
fn an_ancestors_patch_replays_before_a_descendant_that_needs_it() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    s.commit(&[(R, row(50, 1), 1)]).unwrap();
    let x = s.fork_at(s.current()).unwrap();
    x.commit_if(&[(R, row(50, 1))], &[(R, row(50, 1), -1), (R, row(55, 1), 1)]).unwrap().unwrap();
    let m = merge(&t, &x);
    let got = state(&m);
    assert!(!got.contains_key(&(R, row(50, 1))) && got.contains_key(&(R, row(55, 1))));
}

#[test]
fn a_replayed_patch_keeps_its_updates_in_order() {
    let t = EntStore::new();
    seed(&t);
    let s = t.fork_at(t.current()).unwrap();
    let ups = vec![(S, row(70, 0), 1), (R, row(71, 0), 1), (S, row(72, 0), 1), (R, row(73, 0), 1)];
    let e = s.commit(&ups).unwrap();
    let m = merge(&t, &s);
    for rel in [R, S] {
        let logged = |st: &EntStore, at: Edition| -> Vec<Tuple> {
            st.scan_updates(rel, Edition(at.0 - 1), at).unwrap().into_iter().map(|u| u.tuple).collect()
        };
        assert_eq!(logged(&m, m.current()), logged(&s, e), "relation {rel:?}");
    }
}

#[test]
fn a_fork_taken_mid_merge_replays_the_copies_it_holds() {
    let t = EntStore::new();
    seed(&t);
    let v = t.fork_at(t.current()).unwrap();
    let s = t.fork_at(t.current()).unwrap();
    s.commit_if(&[(R, row(3, 0))], &[(R, row(3, 0), -1), (R, row(46, 0), 1)]).unwrap().unwrap();
    s.commit(&[(R, row(47, 0), 1)]).unwrap();
    let m = merge(&t, &s);
    // Forked after the first replayed patch only: `s` is not yet in its
    // history, so its copy of that patch is the only way to it.
    let f = m.fork_at(Edition(t.current().0 + 1)).unwrap();
    let w = merge(&v, &f);
    let got = state(&w);
    assert!(got.contains_key(&(R, row(46, 0))) && !got.contains_key(&(R, row(47, 0))));
    // The copy keeps its precondition: with row 3 gone, it conflicts.
    v.commit(&[(R, row(3, 0), -1)]).unwrap();
    match v.merge(&f).unwrap() {
        MergeOutcome::Conflict(c) => assert_eq!(c.branch, m.branch_id(), "{c:?}"),
        MergeOutcome::Merged(_) => panic!("the copy lost its precondition"),
    }
}

#[test]
fn a_patch_held_as_a_copy_is_not_replayed_from_its_original() {
    let t = EntStore::new();
    seed(&t);
    let v = t.fork_at(t.current()).unwrap();
    let s = t.fork_at(t.current()).unwrap();
    s.commit(&[(R, row(46, 0), 1)]).unwrap();
    s.commit(&[(R, row(47, 0), 1)]).unwrap();
    let m = merge(&t, &s);
    let f = m.fork_at(Edition(t.current().0 + 1)).unwrap();
    // `w` holds a copy of a copy of `s`'s first patch; `s` is in neither
    // `w`'s lineage nor `f`'s.
    let w = merge(&v, &f);
    let x = merge(&w, &s);
    let got = state(&x);
    assert_eq!(got.get(&(R, row(46, 0))), Some(&1), "the first patch was applied twice");
    assert_eq!(got.get(&(R, row(47, 0))), Some(&1), "the second patch was not applied");
}
