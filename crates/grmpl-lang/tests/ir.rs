//! The core IR (P7): compiling the surface now goes through inspectable data —
//! a [`QueryIr`] for a `view`, a [`FormIr`] for a `form` — and closures are
//! generated only by the final `lower`. These tests pin the *shape* of that
//! data (so it really is inspectable, as P8a/P9b/P12 require) and confirm that
//! lowering it reproduces the runnable `Query`/`Form` behavior.

use std::sync::Arc;

use grmpl_core::{Diff, RelId, TraceStore, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_lang::ir::{Comp, CtorSpec, MapExpr, PredExpr, QueryIr, RowExpr};
use grmpl_lang::Program;
use grmpl_pattern::{Bindings, VarId};
use grmpl_store::FjallStore;

fn t(vals: impl Into<Arc<[Value]>>) -> Tuple {
    Tuple::new(vals)
}

// ---- the reified value layer, in isolation ----

#[test]
fn row_and_pred_exprs_evaluate_and_lower() {
    let row = t([Value::Int(3), Value::text("x"), Value::Int(3)]);

    // column == literal, and column == column, are the two view filter shapes.
    let lit = PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(Value::text("x")));
    let cols = PredExpr::Eq(RowExpr::Col(0), RowExpr::Col(2));
    assert!(lit.test(&row));
    assert!(cols.test(&row));

    // an empty conjunction is vacuously true; a false conjunct fails the whole.
    assert!(PredExpr::And(vec![]).test(&row));
    let both = PredExpr::And(vec![
        lit.clone(),
        PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::Int(9))),
    ]);
    assert!(!both.test(&row));

    // lowering yields a closure with identical behavior.
    let closure = lit.clone().lower();
    assert_eq!(closure(&row), lit.test(&row));
}

#[test]
fn map_expr_applies_and_lowers() {
    let row = t([Value::Int(1), Value::text("a")]);
    // swap the columns and append a constant.
    let m = MapExpr {
        out: vec![
            RowExpr::Col(1),
            RowExpr::Col(0),
            RowExpr::Lit(Value::Int(9)),
        ],
    };
    let want = t([Value::text("a"), Value::Int(1), Value::Int(9)]);
    assert_eq!(m.apply(&row), want);
    assert_eq!(m.clone().lower()(&row), want);
}

#[test]
fn ctor_spec_builds_tagged_tuple_and_lowers() {
    let spec = CtorSpec {
        tag: "Take".into(),
        args: vec![VarId(0)],
    };
    let mut b: Bindings = Bindings::new();
    b.insert(VarId(0), Value::text("lamp"));
    let want = Value::Tuple(Arc::from(vec![Value::text("Take"), Value::text("lamp")]));
    assert_eq!(spec.build(&b), want);
    assert_eq!(spec.clone().lower()(&b), want);

    // an unbound arg becomes empty text (v1 form semantics).
    let unbound = CtorSpec {
        tag: "Take".into(),
        args: vec![VarId(0)],
    };
    assert_eq!(
        unbound.build(&Bindings::new()),
        Value::Tuple(Arc::from(vec![Value::text("Take"), Value::text("")]))
    );
}

// ---- the surface lowers through inspectable IR ----

#[test]
fn view_ir_is_inspectable_data() {
    // A single-atom view with a literal filter: the whole plan is data.
    let prog = Program::compile("rel r(a, b)\nview v() { r(a, \"x\") yield a }", 1).unwrap();
    let ir = prog.view_ir("v", &[]).unwrap();
    let expected = QueryIr::Distinct(Box::new(QueryIr::Project {
        input: Box::new(QueryIr::Filter {
            input: Box::new(QueryIr::Rel(RelId(1))),
            pred: PredExpr::And(vec![PredExpr::Eq(
                RowExpr::Col(1),
                RowExpr::Lit(Value::text("x")),
            )]),
        }),
        cols: vec![0],
    }));
    assert_eq!(ir, expected);
}

#[test]
fn view_ir_reifies_join_keys() {
    // A two-atom view shares variable `b`: the join keys are inspectable.
    let prog = Program::compile(
        "rel r(a, b)\nrel s(c, d)\nview j() { r(a, b) s(b, e) yield a, e }",
        1,
    )
    .unwrap();
    let ir = prog.view_ir("j", &[]).unwrap();
    let expected = QueryIr::Distinct(Box::new(QueryIr::Project {
        input: Box::new(QueryIr::Join {
            left: Box::new(QueryIr::Rel(RelId(1))),
            right: Box::new(QueryIr::Rel(RelId(2))),
            left_key: vec![1],
            right_key: vec![0],
        }),
        cols: vec![0, 3],
    }));
    assert_eq!(ir, expected);
}

#[test]
fn lowered_view_ir_evaluates() {
    let prog = Program::compile("rel r(a, b)\nview v() { r(a, \"x\") yield a }", 1).unwrap();
    let r = prog.rel_id("r").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store
        .commit(&[
            (r, t([Value::Int(1), Value::text("x")]), 1),
            (r, t([Value::Int(2), Value::text("y")]), 1),
        ])
        .unwrap();

    // Lower the IR by hand and evaluate: only the row whose second column is
    // "x" survives, projected to its first column.
    let q = prog.view_ir("v", &[]).unwrap().lower();
    let got: Vec<(Tuple, Diff)> = q.find(&Snapshot::at_current(&store)).unwrap();
    assert_eq!(got, vec![(t([Value::Int(1)]), 1)]);
}

#[test]
fn form_ir_exposes_ctor_specs() {
    let prog = Program::compile(
        "form command {\n \"take\" name -> Take(name)\n \"look\" -> Look()\n}",
        1,
    )
    .unwrap();
    let ir = prog.form_ir("command").unwrap();
    assert_eq!(ir.rules.len(), 2);
    // The `-> Tag(args)` arrows are inspectable data, not closures.
    assert_eq!(
        ir.rules[0].ctor,
        CtorSpec {
            tag: "Take".into(),
            args: vec![VarId(0)]
        }
    );
    assert_eq!(
        ir.rules[1].ctor,
        CtorSpec {
            tag: "Look".into(),
            args: vec![]
        }
    );

    // Lowering reproduces the parser behavior.
    let form = ir.lower();
    assert_eq!(
        form.parse_all(&[Value::text("take"), Value::text("lamp")]),
        vec![Value::Tuple(Arc::from(vec![
            Value::text("Take"),
            Value::text("lamp")
        ]))]
    );
}

#[test]
fn reduce_view_ir_wraps_the_view_plan() {
    let prog = Program::compile(
        "rel score(team, pts)\nview scored() { score(t, p) yield t, p }",
        1,
    )
    .unwrap();
    let ir = prog
        .reduce_view_ir("scored", &[], &["t"], grmpl_lang::NamedAgg::Count)
        .unwrap();
    // The reduce sits atop the view's own plan.
    match ir {
        QueryIr::Reduce { input, key, agg } => {
            assert_eq!(key, vec![0]);
            assert_eq!(agg, grmpl_diff::Agg::Count);
            assert!(matches!(*input, QueryIr::Distinct(_)));
        }
        other => panic!("expected Reduce, got {other:?}"),
    }
}

// ---- the computation layer ----

#[test]
fn comp_constructors_name_the_computation() {
    let prog = Program::compile(
        "rel r(a, b)\nview v() { r(a, b) yield a }\nform f { \"x\" -> X() }",
        1,
    )
    .unwrap();

    let find = prog.find_view("v", &[]).unwrap();
    assert!(matches!(find, Comp::Find(_)));
    assert!(find.plan().is_some());

    // `on watch` installs the view's plan as a maintained delta stream.
    let watch = prog.watch_view("v", &[]).unwrap();
    assert!(matches!(watch, Comp::Watch(_)));
    // The plan under a Watch is the same data as `view_ir`, and still lowers.
    assert_eq!(watch.plan(), Some(&prog.view_ir("v", &[]).unwrap()));
    let _q = watch.plan().unwrap().clone().lower();

    let parse = prog.parse_form("f").unwrap();
    assert!(matches!(parse, Comp::Parse(_)));
    assert!(parse.plan().is_none());
}
