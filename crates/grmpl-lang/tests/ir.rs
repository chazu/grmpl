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

// ---- P9b: invertible printing over CtorSpec ----

#[test]
fn ctor_spec_print_inverts_build() {
    let spec = CtorSpec {
        tag: "Take".into(),
        args: vec![VarId(0)],
    };
    let mut b: Bindings = Bindings::new();
    b.insert(VarId(0), Value::text("lamp"));
    let built = spec.build(&b);
    // build -> print recovers the binding; print -> build reproduces the value.
    assert_eq!(spec.print(&built), Some(b.clone()));
    assert_eq!(spec.build(&spec.print(&built).unwrap()), built);

    // shape guards: wrong tag, wrong arity, and non-tuple all reject.
    let wrong_tag = Value::Tuple(Arc::from(vec![Value::text("Drop"), Value::text("lamp")]));
    assert_eq!(spec.print(&wrong_tag), None);
    let wrong_arity = Value::Tuple(Arc::from(vec![Value::text("Take")]));
    assert_eq!(spec.print(&wrong_arity), None);
    assert_eq!(spec.print(&Value::text("Take")), None);

    // nullary ctor: only the bare tag tuple prints, to empty bindings.
    let nil = CtorSpec {
        tag: "Look".into(),
        args: vec![],
    };
    assert_eq!(
        nil.print(&Value::Tuple(Arc::from(vec![Value::text("Look")]))),
        Some(Bindings::new())
    );
}

#[test]
fn ctor_spec_is_invertible_flags_repeated_vars() {
    let injective = CtorSpec {
        tag: "Pair".into(),
        args: vec![VarId(0), VarId(1)],
    };
    let repeated = CtorSpec {
        tag: "Pair".into(),
        args: vec![VarId(0), VarId(0)],
    };
    assert!(injective.is_invertible());
    assert!(!repeated.is_invertible());
    assert!(CtorSpec {
        tag: "Nil".into(),
        args: vec![],
    }
    .is_invertible());

    // A shape-valid value whose two slots disagree: the repeated-var spec cannot
    // rebuild it (that is exactly what is_invertible() warns about), while the
    // injective spec round-trips it cleanly.
    let v = Value::Tuple(Arc::from(vec![
        Value::text("Pair"),
        Value::Int(1),
        Value::Int(2),
    ]));
    assert_ne!(repeated.build(&repeated.print(&v).unwrap()), v);
    assert_eq!(injective.build(&injective.print(&v).unwrap()), v);
}

/// Randomized round-trip law oracle (seeded xorshift64*, no new dep; seed
/// printed on every failure for replay). Over random specs — with a small
/// variable pool so repeats, hence non-invertible specs, occur often — it
/// checks the two directions of the P9b law each round:
///   * build → print recovers every argument variable (holds for ANY spec);
///   * for invertible specs, print → build is an exact two-sided inverse on
///     every shape-valid value.
#[test]
fn ctor_spec_roundtrip_law_holds_under_random_churn() {
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
    fn rand_value(rng: &mut Rng) -> Value {
        match rng.below(3) {
            0 => Value::Int((rng.next() % 100) as i64),
            1 => Value::text(["a", "b", "c", "lamp", "take"][rng.below(5) as usize]),
            _ => Value::Bool(rng.below(2) == 0),
        }
    }
    fn rand_spec(rng: &mut Rng) -> CtorSpec {
        let tag = ["Take", "Look", "Go", "Use"][rng.below(4) as usize].to_string();
        let n = rng.below(5); // 0..=4 slots
        // pool of 3 variables => repeats (non-invertible specs) are common.
        let args = (0..n).map(|_| VarId(rng.below(3) as u32)).collect();
        CtorSpec { tag, args }
    }

    for seed in 1..=64u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for _ in 0..40 {
            let spec = rand_spec(&mut rng);

            // Direction 1 — build then print recovers every argument variable,
            // for ANY spec, given a binding that binds all of its args.
            let mut b: Bindings = Bindings::new();
            for &v in &spec.args {
                b.entry(v).or_insert_with(|| rand_value(&mut rng));
            }
            let built = spec.build(&b);
            let recovered = spec
                .print(&built)
                .unwrap_or_else(|| panic!("seed {seed}: print rejected build output {built:?}"));
            for &v in &spec.args {
                assert_eq!(
                    recovered.get(&v),
                    b.get(&v),
                    "seed {seed}: build->print lost var {v:?} for {spec:?}"
                );
            }

            // Direction 2 — for INVERTIBLE specs, print is an exact two-sided
            // inverse on every shape-valid value; for all specs, print;build is
            // idempotent (its image is a fixpoint).
            let mut slots = vec![Value::text(&spec.tag)];
            slots.extend((0..spec.args.len()).map(|_| rand_value(&mut rng)));
            let v = Value::Tuple(Arc::from(slots));
            let rebuilt = spec.build(&spec.print(&v).expect("shape-valid value must print"));
            if spec.is_invertible() {
                assert_eq!(
                    rebuilt, v,
                    "seed {seed}: invertible print->build is not identity for {spec:?}"
                );
            }
            assert_eq!(
                spec.build(&spec.print(&rebuilt).unwrap()),
                rebuilt,
                "seed {seed}: print;build not idempotent for {spec:?}"
            );
        }
    }
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
