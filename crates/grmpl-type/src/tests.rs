//! Row-typing tests for [`check_query`] / [`check_comp`] over the P7 IR.

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{Column, Edition, Entity, RelId, Schema, SchemaCatalog, Ty, Value};
use grmpl_diff::Agg;
use grmpl_lang::{Comp, MapExpr, PredExpr, QueryIr, RowExpr};

use super::*;

/// A minimal in-memory [`SchemaCatalog`] for tests: a flat `RelId → Schema` map,
/// edition-agnostic (every version is in effect at every edition). Enough to
/// drive the type synthesizer without pulling in `grmpl-store`.
#[derive(Default)]
struct MemSchemas(Mutex<HashMap<RelId, Schema>>);

impl MemSchemas {
    fn with(defs: &[(u32, &[(&str, Ty)])]) -> MemSchemas {
        let cat = MemSchemas::default();
        for (rel, cols) in defs {
            let schema = Schema::new(cols.iter().map(|(n, t)| Column::new(*n, *t)).collect());
            cat.0.lock().unwrap().insert(RelId(*rel), schema);
        }
        cat
    }
}

impl SchemaCatalog for MemSchemas {
    fn put_schema(&self, rel: RelId, schema: &Schema, _at: Edition) -> grmpl_core::Result<()> {
        self.0.lock().unwrap().insert(rel, schema.clone());
        Ok(())
    }
    fn schema(&self, rel: RelId) -> grmpl_core::Result<Option<Schema>> {
        Ok(self.0.lock().unwrap().get(&rel).cloned())
    }
    fn schema_at(&self, rel: RelId, _at: Edition) -> grmpl_core::Result<Option<Schema>> {
        Ok(self.0.lock().unwrap().get(&rel).cloned())
    }
}

const AT: Edition = Edition::ZERO;

fn rel(id: u32) -> QueryIr {
    QueryIr::Rel(RelId(id))
}

fn check(q: &QueryIr, cat: &MemSchemas) -> std::result::Result<RowTy, TypeError> {
    check_query(q, cat, AT)
}

// ---- leaves ----------------------------------------------------------------

#[test]
fn value_ty_maps_each_ground_case() {
    assert_eq!(value_ty(&Value::Ent(Entity(1))), Ty::Ent);
    assert_eq!(value_ty(&Value::Int(3)), Ty::Int);
    assert_eq!(value_ty(&Value::text("x")), Ty::Text);
    assert_eq!(value_ty(&Value::Bool(true)), Ty::Bool);
    assert_eq!(
        value_ty(&Value::Tuple(std::sync::Arc::from([Value::Int(1)]))),
        Ty::Tuple
    );
}

#[test]
fn rel_takes_its_row_type_from_the_schema() {
    let cat = MemSchemas::with(&[(1, &[("thing", Ty::Ent), ("since", Ty::Int)])]);
    let t = check(&rel(1), &cat).unwrap();
    assert_eq!(t.cols(), &[Ty::Ent, Ty::Int]);
    assert_eq!(t.arity(), 2);
}

#[test]
fn unschemaed_relation_is_an_error() {
    let cat = MemSchemas::default();
    assert_eq!(
        check(&rel(7), &cat),
        Err(TypeError::UnschemaedRelation(RelId(7)))
    );
}

// ---- map -------------------------------------------------------------------

#[test]
fn map_types_each_output_expr() {
    let cat = MemSchemas::with(&[(1, &[("thing", Ty::Ent), ("n", Ty::Int)])]);
    // out = [ col1 (Int), literal Text ]
    let q = QueryIr::Map {
        input: Box::new(rel(1)),
        map: MapExpr {
            out: vec![RowExpr::Col(1), RowExpr::Lit(Value::text("k"))],
        },
    };
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Int, Ty::Text]);
}

#[test]
fn map_column_out_of_range() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent)])]);
    let q = QueryIr::Map {
        input: Box::new(rel(1)),
        map: MapExpr {
            out: vec![RowExpr::Col(3)],
        },
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::ColumnOutOfRange { index: 3, arity: 1 })
    );
}

// ---- filter ----------------------------------------------------------------

#[test]
fn filter_is_type_neutral_when_predicate_checks() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Int), ("b", Ty::Int)])]);
    let q = QueryIr::Filter {
        input: Box::new(rel(1)),
        pred: PredExpr::Eq(RowExpr::Col(0), RowExpr::Col(1)),
    };
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Int, Ty::Int]);
}

#[test]
fn filter_eq_of_incompatible_concrete_types_is_rejected() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Int), ("b", Ty::Text)])]);
    let q = QueryIr::Filter {
        input: Box::new(rel(1)),
        pred: PredExpr::Eq(RowExpr::Col(0), RowExpr::Col(1)),
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::Incomparable {
            left: Ty::Int,
            right: Ty::Text
        })
    );
}

#[test]
fn filter_eq_against_any_column_is_allowed() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Any)])]);
    let q = QueryIr::Filter {
        input: Box::new(rel(1)),
        pred: PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::Int(9))),
    };
    assert!(check(&q, &cat).is_ok());
}

#[test]
fn filter_and_checks_every_conjunct() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Int), ("b", Ty::Bool)])]);
    let q = QueryIr::Filter {
        input: Box::new(rel(1)),
        pred: PredExpr::And(vec![
            PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::Int(1))),
            // second conjunct is ill-typed: Bool vs Int
            PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(Value::Int(1))),
        ]),
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::Incomparable {
            left: Ty::Bool,
            right: Ty::Int
        })
    );
}

// ---- project ---------------------------------------------------------------

#[test]
fn project_selects_column_types_in_order() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent), ("b", Ty::Int), ("c", Ty::Text)])]);
    let q = QueryIr::Project {
        input: Box::new(rel(1)),
        cols: vec![2, 0],
    };
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Text, Ty::Ent]);
}

#[test]
fn project_out_of_range() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent)])]);
    let q = QueryIr::Project {
        input: Box::new(rel(1)),
        cols: vec![0, 5],
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::ColumnOutOfRange { index: 5, arity: 1 })
    );
}

// ---- join ------------------------------------------------------------------

#[test]
fn join_concatenates_left_and_right_columns() {
    let cat = MemSchemas::with(&[
        (1, &[("t", Ty::Ent), ("room", Ty::Ent)]),
        (2, &[("room", Ty::Ent), ("name", Ty::Text)]),
    ]);
    let q = QueryIr::Join {
        left: Box::new(rel(1)),
        right: Box::new(rel(2)),
        left_key: vec![1],
        right_key: vec![0],
    };
    assert_eq!(
        check(&q, &cat).unwrap().cols(),
        &[Ty::Ent, Ty::Ent, Ty::Ent, Ty::Text]
    );
}

#[test]
fn join_key_arity_mismatch() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent)]), (2, &[("b", Ty::Ent)])]);
    let q = QueryIr::Join {
        left: Box::new(rel(1)),
        right: Box::new(rel(2)),
        left_key: vec![0, 0],
        right_key: vec![0],
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::KeyArityMismatch { left: 2, right: 1 })
    );
}

#[test]
fn join_key_type_mismatch_never_matches() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent)]), (2, &[("b", Ty::Text)])]);
    let q = QueryIr::Join {
        left: Box::new(rel(1)),
        right: Box::new(rel(2)),
        left_key: vec![0],
        right_key: vec![0],
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::JoinKeyMismatch {
            pos: 0,
            left: Ty::Ent,
            right: Ty::Text
        })
    );
}

// ---- union / negate / distinct --------------------------------------------

#[test]
fn union_takes_column_wise_lub() {
    let cat = MemSchemas::with(&[
        (1, &[("a", Ty::Int), ("b", Ty::Text)]),
        (2, &[("a", Ty::Int), ("b", Ty::Ent)]),
    ]);
    let q = QueryIr::Union(Box::new(rel(1)), Box::new(rel(2)));
    // col 0 agrees (Int); col 1 differs (Text vs Ent) → widens to Any.
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Int, Ty::Any]);
}

#[test]
fn union_arity_mismatch() {
    let cat = MemSchemas::with(&[
        (1, &[("a", Ty::Int)]),
        (2, &[("a", Ty::Int), ("b", Ty::Int)]),
    ]);
    let q = QueryIr::Union(Box::new(rel(1)), Box::new(rel(2)));
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::UnionArityMismatch { left: 1, right: 2 })
    );
}

#[test]
fn negate_and_distinct_preserve_the_row_type() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent), ("b", Ty::Int)])]);
    let neg = QueryIr::Negate(Box::new(rel(1)));
    let dist = QueryIr::Distinct(Box::new(rel(1)));
    assert_eq!(check(&neg, &cat).unwrap().cols(), &[Ty::Ent, Ty::Int]);
    assert_eq!(check(&dist, &cat).unwrap().cols(), &[Ty::Ent, Ty::Int]);
}

// ---- reduce ----------------------------------------------------------------

#[test]
fn reduce_appends_the_aggregate_column() {
    let cat = MemSchemas::with(&[(1, &[("g", Ty::Ent), ("n", Ty::Int)])]);
    let count = QueryIr::Reduce {
        input: Box::new(rel(1)),
        key: vec![0],
        agg: Agg::Count,
    };
    assert_eq!(check(&count, &cat).unwrap().cols(), &[Ty::Ent, Ty::Int]);

    let sum = QueryIr::Reduce {
        input: Box::new(rel(1)),
        key: vec![0],
        agg: Agg::Sum(1),
    };
    assert_eq!(check(&sum, &cat).unwrap().cols(), &[Ty::Ent, Ty::Int]);

    // Min/Max carry the folded column's own type.
    let max = QueryIr::Reduce {
        input: Box::new(rel(1)),
        key: vec![0],
        agg: Agg::Max(0),
    };
    assert_eq!(check(&max, &cat).unwrap().cols(), &[Ty::Ent, Ty::Ent]);
}

#[test]
fn reduce_sum_over_non_numeric_is_rejected() {
    let cat = MemSchemas::with(&[(1, &[("g", Ty::Ent), ("label", Ty::Text)])]);
    let q = QueryIr::Reduce {
        input: Box::new(rel(1)),
        key: vec![0],
        agg: Agg::Sum(1),
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::SumNonNumeric {
            col: 1,
            ty: Ty::Text
        })
    );
}

#[test]
fn reduce_key_out_of_range() {
    let cat = MemSchemas::with(&[(1, &[("g", Ty::Ent)])]);
    let q = QueryIr::Reduce {
        input: Box::new(rel(1)),
        key: vec![4],
        agg: Agg::Count,
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::ColumnOutOfRange { index: 4, arity: 1 })
    );
}

// ---- iterate / recur -------------------------------------------------------

#[test]
fn iterate_types_the_fixpoint_with_recur_bound() {
    let cat = MemSchemas::with(&[(1, &[("x", Ty::Ent), ("y", Ty::Ent)])]);
    // init = base; step = project(recur, [0,1]) — same arity, references Recur.
    let q = QueryIr::Iterate {
        init: Box::new(rel(1)),
        step: Box::new(QueryIr::Project {
            input: Box::new(QueryIr::Recur),
            cols: vec![0, 1],
        }),
    };
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Ent, Ty::Ent]);
}

#[test]
fn recur_outside_iterate_has_no_type() {
    let cat = MemSchemas::default();
    assert_eq!(
        check(&QueryIr::Recur, &cat),
        Err(TypeError::RecurOutsideIterate)
    );
}

#[test]
fn iterate_step_arity_must_match_init() {
    let cat = MemSchemas::with(&[(1, &[("x", Ty::Ent), ("y", Ty::Ent)])]);
    let q = QueryIr::Iterate {
        init: Box::new(rel(1)),
        // step drops a column → arity 1 vs init arity 2.
        step: Box::new(QueryIr::Project {
            input: Box::new(QueryIr::Recur),
            cols: vec![0],
        }),
    };
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::IterateArityMismatch { init: 2, step: 1 })
    );
}

// ---- comp ------------------------------------------------------------------

#[test]
fn check_comp_types_find_and_watch_and_skips_parse() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Ent)])]);
    let find = Comp::Find(rel(1));
    let watch = Comp::Watch(rel(1));
    assert_eq!(
        check_comp(&find, &cat, AT).unwrap().unwrap().cols(),
        &[Ty::Ent]
    );
    assert_eq!(
        check_comp(&watch, &cat, AT).unwrap().unwrap().cols(),
        &[Ty::Ent]
    );

    let parse = Comp::Parse(grmpl_lang::FormIr { rules: vec![] });
    assert_eq!(check_comp(&parse, &cat, AT).unwrap(), None);
}

#[test]
fn errors_propagate_through_nested_operators() {
    let cat = MemSchemas::with(&[(1, &[("a", Ty::Int)])]);
    // distinct(map(rel, [col 9])) — inner column error surfaces at the top.
    let q = QueryIr::Distinct(Box::new(QueryIr::Map {
        input: Box::new(rel(1)),
        map: MapExpr {
            out: vec![RowExpr::Col(9)],
        },
    }));
    assert_eq!(
        check(&q, &cat),
        Err(TypeError::ColumnOutOfRange { index: 9, arity: 1 })
    );
}
