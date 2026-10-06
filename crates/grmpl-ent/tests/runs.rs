//! **Runs are opt-in per relation** (fidelity gap G9).
//!
//! A relation's rows fold into runs only if it asks (`set_runs`, or a
//! branch's `set_default_runs`). Runs change no answer, fold a regular
//! relation into a few nodes, and coarsen what sharing-based provenance can
//! find of it. So they are off by default, and these laws pin both sides of
//! that choice on one store, and the rules a relation's shape keeps: fixed
//! once written, durable, and carried by forks and merges.

use grmpl_core::{EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, Layout, MergeOutcome};

const PLAIN: RelId = RelId(1);
const FOLDED: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn room(e: u64) -> Tuple {
    Tuple::from([ent(e), Value::text("room")])
}

/// The same 20 000 regular rows in both relations, in 1 000-row commits.
fn load(s: &EntStore) {
    for chunk in (0..20_000u64).collect::<Vec<_>>().chunks(1_000) {
        let ups: Vec<_> = chunk.iter().flat_map(|&e| [(PLAIN, room(e), 1), (FOLDED, room(e), 1)]).collect();
        s.commit(&ups).unwrap();
    }
}

#[test]
fn runs_are_off_unless_a_relation_asks() {
    let dir = tempfile::tempdir().unwrap();
    let s = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
    assert!(!s.runs(PLAIN) && !s.runs(FOLDED), "runs are off by default");
    s.set_runs(FOLDED, true).unwrap();
    load(&s);
    let at = s.current();
    // The same answers.
    assert_eq!(s.read_at(PLAIN, at).unwrap(), s.read_at(FOLDED, at).unwrap());
    let (lo, hi) = (Tuple::from([ent(5_000)]), Tuple::from([ent(6_000)]));
    assert_eq!(s.range_at(PLAIN, at, &lo, &hi).unwrap(), s.range_at(FOLDED, at, &lo, &hi).unwrap());
    assert_eq!(s.count_at(PLAIN, at, &lo, &hi).unwrap(), 1_000);
    assert_eq!(s.count_at(FOLDED, at, &lo, &hi).unwrap(), 1_000);

    // What folding buys: a cold range read touches a few frames, not a
    // block's worth of leaves.
    drop(s);
    let s = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
    let cold = |rel: RelId| {
        let before = s.frames_paged();
        assert_eq!(s.range_at(rel, at, &lo, &hi).unwrap().len(), 1_000);
        s.frames_paged() - before
    };
    let (plain, folded) = (cold(PLAIN), cold(FOLDED));
    assert!(folded * 4 <= plain, "a folded range read paged {folded} frames against {plain}");

    // What it costs: after a graft, backfollow finds the copy of plain rows
    // by their shared leaves, and nothing of folded ones, whose leaf the
    // graft's cut rebuilt.
    let template = s.current();
    s.instance_template(&[PLAIN, FOLDED], 10_000, 11_000, 1_000_000).unwrap();
    let copied = |rel: RelId| {
        let (lo, hi) = (Tuple::from([ent(10_000)]), Tuple::from([ent(11_000)]));
        s.backfollow(rel, template, &lo, &hi)
            .unwrap()
            .iter()
            .filter(|h| h.shift == 1_000_000)
            .map(|h| h.rows)
            .max()
            .unwrap_or(0)
    };
    assert!(copied(PLAIN) > 800, "backfollow found {} rows of the plain copy", copied(PLAIN));
    assert!(copied(FOLDED) < copied(PLAIN), "folding coarsens what sharing finds");
}

#[test]
fn a_relations_shape_is_fixed_once_written_and_carried() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        s.set_runs(FOLDED, true).unwrap();
        s.set_layout(FOLDED, Layout::Kd).unwrap();
        // A second setting keeps the first: the shape is both.
        assert!(s.runs(FOLDED));
        assert_eq!(s.layout(FOLDED), Layout::Kd);
        s.commit(&[(FOLDED, room(1), 1), (PLAIN, room(1), 1)]).unwrap();
        // Written: the same choice is a no-op, any other is refused.
        s.set_runs(FOLDED, true).unwrap();
        assert!(s.set_runs(FOLDED, false).is_err());
        assert!(s.set_runs(PLAIN, true).is_err());
        assert!(s.set_default_runs(true).is_err(), "a written relation would change");
    }
    // Durable without any further commit.
    let s = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
    assert!(s.runs(FOLDED) && !s.runs(PLAIN));
    assert_eq!(s.layout(FOLDED), Layout::Kd);
    // A fork carries shapes, and a merge unites them.
    let fork = s.fork_at(s.current()).unwrap();
    assert!(fork.runs(FOLDED));
    fork.set_runs(RelId(3), true).unwrap();
    for e in 0..300 {
        fork.commit(&[(RelId(3), room(e), 1)]).unwrap();
    }
    let MergeOutcome::Merged(m) = s.merge(&fork).unwrap() else { panic!("merge conflict") };
    assert!(m.runs(RelId(3)), "a merge keeps the other side's shape");
    assert_eq!(m.read_at(RelId(3), m.current()).unwrap().len(), 300);
}

#[test]
fn a_branch_may_fold_by_default() {
    let s = EntStore::new();
    s.set_default_runs(true).unwrap();
    assert!(s.runs(PLAIN));
    s.set_runs(FOLDED, false).unwrap();
    assert!(!s.runs(FOLDED), "a relation shaped by name keeps its own choice");
}
