//! The P2 named-column aggregate surface: `Program::reduce_view` groups and
//! folds a view's *yielded columns by name*, lowering to a `Query::Reduce`. This
//! is the surface that "waits on P1" — column names resolve against the view's
//! `yield` list.

use grmpl_core::{Diff, Entity, TraceStore, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_lang::{NamedAgg, Program};
use grmpl_store::FjallStore;

const SRC: &str = r#"
    rel score(player: Ent, team: Ent, points: Int)
    view scored() {
        score(p, t, pts)
        yield t, pts
    }
"#;

fn ent(x: u64) -> Value {
    Value::Ent(Entity(x))
}

fn find_sorted(q: &grmpl_diff::Query, store: &FjallStore) -> Vec<(Tuple, Diff)> {
    q.find(&Snapshot::at_current(store)).unwrap()
}

#[test]
fn reduce_view_folds_named_columns() {
    let prog = Program::compile(SRC, 1).unwrap();
    let score = prog.rel_id("score").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    // team 10: players 1(3pts), 2(5pts); team 20: player 3(7pts).
    store
        .commit(&[
            (score, Tuple::from([ent(1), ent(10), Value::Int(3)]), 1),
            (score, Tuple::from([ent(2), ent(10), Value::Int(5)]), 1),
            (score, Tuple::from([ent(3), ent(20), Value::Int(7)]), 1),
        ])
        .unwrap();

    // scored yields (t, pts) = (team, points). Group by `t`, sum `pts`.
    let sum = prog.reduce_view("scored", &[], &["t"], NamedAgg::Sum("pts".into())).unwrap();
    assert_eq!(
        find_sorted(&sum, &store),
        vec![
            (Tuple::from([ent(10), Value::Int(8)]), 1),
            (Tuple::from([ent(20), Value::Int(7)]), 1),
        ]
    );

    // Count rows per team, Max points per team.
    let count = prog.reduce_view("scored", &[], &["t"], NamedAgg::Count).unwrap();
    assert_eq!(
        find_sorted(&count, &store),
        vec![
            (Tuple::from([ent(10), Value::Int(2)]), 1),
            (Tuple::from([ent(20), Value::Int(1)]), 1),
        ]
    );

    let max = prog.reduce_view("scored", &[], &["t"], NamedAgg::Max("pts".into())).unwrap();
    assert_eq!(
        find_sorted(&max, &store),
        vec![
            (Tuple::from([ent(10), Value::Int(5)]), 1),
            (Tuple::from([ent(20), Value::Int(7)]), 1),
        ]
    );
}

#[test]
fn reduce_view_rejects_unknown_columns() {
    let prog = Program::compile(SRC, 1).unwrap();
    assert!(prog.reduce_view("scored", &[], &["nope"], NamedAgg::Count).is_err());
    assert!(prog
        .reduce_view("scored", &[], &["t"], NamedAgg::Sum("nope".into()))
        .is_err());
    assert!(prog.reduce_view("nosuchview", &[], &["t"], NamedAgg::Count).is_err());
}
