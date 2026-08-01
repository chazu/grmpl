//! Row-typing tests for [`check_query`] / [`check_comp`] over the P7 IR.

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{Column, Edition, Entity, RelId, Schema, SchemaCatalog, Ty, Value};
use grmpl_diff::Agg;
use grmpl_lang::{Comp, MapExpr, PredExpr, QueryIr, RowExpr};

use super::*;

/// A minimal in-memory [`SchemaCatalog`] for tests: a flat `RelId → Schema` map,
/// edition-agnostic (every version is in effect at every edition). Enough to
/// drive the type synthesizer without pulling in a store.
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

// ---- iterate fixpoint soundness --------------------------------------------

#[test]
fn iterate_widens_to_the_full_fixpoint_not_one_kleene_step() {
    // Regression for the P8a review's unsoundness: a column can only widen to
    // `Any` on the *second* round, after a widening it depends on has happened.
    //
    // `step(Recur) = swap(Recur) ∪ const-row[Text, Int]`, over an `init` of type
    // `[Int, Int]`. A single Kleene step synthesizes `[Any, Int]` — it never sees
    // that the `Text` injected into column 0 reaches column 1 one round later,
    // when the swap moves it across. The true fixpoint is `[Any, Any]`, and the
    // runtime does produce a `Text` in column 1. Iterating to the fixpoint is
    // what makes the synthesized type honest.
    let cat = MemSchemas::with(&[
        (1, &[("a", Ty::Int), ("b", Ty::Int)]), // init: [Int, Int]
        (2, &[("driver", Ty::Ent)]),            // drives the constant arm
    ]);
    let swap = QueryIr::Project {
        input: Box::new(QueryIr::Recur),
        cols: vec![1, 0],
    };
    let const_row = QueryIr::Map {
        input: Box::new(rel(2)),
        map: MapExpr {
            out: vec![RowExpr::Lit(Value::text("x")), RowExpr::Lit(Value::Int(0))],
        },
    };
    let q = QueryIr::Iterate {
        init: Box::new(rel(1)),
        step: Box::new(QueryIr::Union(Box::new(swap), Box::new(const_row))),
    };
    // Column 1 must be `Any`, not `Int` — the single-step bug reported `Int`.
    assert_eq!(check(&q, &cat).unwrap().cols(), &[Ty::Any, Ty::Any]);
}

// ---- randomized soundness oracle -------------------------------------------
//
// A law test, not an example: the invariant is *soundness* — the synthesized
// `RowTy` must admit every value the runtime actually produces, column by column
// (`Ty::Any` admits all). We churn random data into a real store, generate
// random well-typed plans, `lower` + run them, and assert every output cell is
// admitted by the type its column was synthesized to have. This links the
// synthesizer to the runtime (catching typer/runtime layout drift) and is what
// would have caught the single-Kleene-step `Iterate` unsoundness above.

use grmpl_core::{EditionStore, Tuple, TraceStore};
use grmpl_diff::eval_snapshot;
use grmpl_ent::EntStore;

/// A tiny deterministic xorshift64 PRNG — reproducible, no external `rand` dep.
struct Rng(u64);

impl Rng {
    fn u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// A value in `0..n` (requires `n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.u64() % n as u64) as usize
    }
    fn coin(&mut self) -> bool {
        self.u64() & 1 == 0
    }
    /// A random *concrete* column type (never `Any` — base schemas are exact).
    fn concrete_ty(&mut self) -> Ty {
        [Ty::Ent, Ty::Int, Ty::Text, Ty::Bool][self.below(4)]
    }
    /// A random literal of a random concrete type.
    fn lit(&mut self) -> Value {
        let t = self.concrete_ty();
        self.value(t)
    }
    /// A random value of the given type (small domains, so joins/filters hit).
    fn value(&mut self, ty: Ty) -> Value {
        match ty {
            Ty::Ent => Value::Ent(Entity(self.below(4) as u64)),
            Ty::Int => Value::Int(self.below(5) as i64),
            Ty::Text => Value::text(["a", "b", "c"][self.below(3)]),
            Ty::Bool => Value::Bool(self.coin()),
            // base schemas only use the four ground types above.
            _ => Value::Int(0),
        }
    }
    fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            xs.swap(i, self.below(i + 1));
        }
    }
}

/// The synthesized row type of a (closed) plan — used *during generation* to
/// pick valid operator parameters, so every plan we build type-checks. Not
/// circular with the soundness assertion, which compares this to the runtime.
fn ty_of(q: &QueryIr, cat: &MemSchemas) -> RowTy {
    check_query(q, cat, AT).expect("generator built an ill-typed plan")
}

/// Build a random well-typed plan over `rels`. When `in_iterate` is set the
/// subtree stays free of `Reduce`/`Iterate` (the runtime forbids an aggregate or
/// a nested fixpoint inside an `Iterate`), keeping every lowered plan runnable.
fn gen(rng: &mut Rng, cat: &MemSchemas, rels: &[RelId], depth: usize, in_iterate: bool) -> QueryIr {
    // Leaf: a base relation.
    if depth == 0 || rng.below(3) == 0 {
        return QueryIr::Rel(rels[rng.below(rels.len())]);
    }
    // Restrict the operator menu inside an Iterate to the reduce/iterate-free set.
    let n_ops = if in_iterate { 7 } else { 9 };
    match rng.below(n_ops) {
        0 => {
            // Map: each output is a valid column reference or a literal.
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let a = ty_of(&input, cat).arity();
            let k = 1 + rng.below(3);
            let out = (0..k)
                .map(|_| {
                    if a > 0 && rng.coin() {
                        RowExpr::Col(rng.below(a))
                    } else {
                        RowExpr::Lit(rng.lit())
                    }
                })
                .collect();
            QueryIr::Map {
                input: Box::new(input),
                map: MapExpr { out },
            }
        }
        1 => {
            // Filter: `col == literal` with the literal typed to match the column.
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let t = ty_of(&input, cat);
            if t.arity() == 0 {
                return input;
            }
            let i = rng.below(t.arity());
            let ti = t.at(i).unwrap();
            let lit_ty = if ti == Ty::Any { rng.concrete_ty() } else { ti };
            QueryIr::Filter {
                input: Box::new(input),
                pred: PredExpr::Eq(RowExpr::Col(i), RowExpr::Lit(rng.value(lit_ty))),
            }
        }
        2 => {
            // Project: a random (possibly repeating/reordering) column selection.
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let a = ty_of(&input, cat).arity();
            if a == 0 {
                return input;
            }
            let k = 1 + rng.below(a);
            let cols = (0..k).map(|_| rng.below(a)).collect();
            QueryIr::Project {
                input: Box::new(input),
                cols,
            }
        }
        3 => {
            // Join: pick a compatible key pair, else a keyless cross join.
            let left = gen(rng, cat, rels, depth - 1, in_iterate);
            let right = gen(rng, cat, rels, depth - 1, in_iterate);
            let tl = ty_of(&left, cat);
            let tr = ty_of(&right, cat);
            let mut pairs = Vec::new();
            for i in 0..tl.arity() {
                for j in 0..tr.arity() {
                    if compatible(tl.at(i).unwrap(), tr.at(j).unwrap()) {
                        pairs.push((i, j));
                    }
                }
            }
            let (left_key, right_key) = if pairs.is_empty() {
                (vec![], vec![])
            } else {
                let (i, j) = pairs[rng.below(pairs.len())];
                (vec![i], vec![j])
            };
            QueryIr::Join {
                left: Box::new(left),
                right: Box::new(right),
                left_key,
                right_key,
            }
        }
        4 => {
            // Union: project both sides to a common arity, then combine.
            let a = gen(rng, cat, rels, depth - 1, in_iterate);
            let b = gen(rng, cat, rels, depth - 1, in_iterate);
            let m = ty_of(&a, cat).arity().min(ty_of(&b, cat).arity()).max(1);
            let pa = QueryIr::Project {
                input: Box::new(a),
                cols: (0..m).collect(),
            };
            let pb = QueryIr::Project {
                input: Box::new(b),
                cols: (0..m).collect(),
            };
            QueryIr::Union(Box::new(pa), Box::new(pb))
        }
        5 => QueryIr::Negate(Box::new(gen(rng, cat, rels, depth - 1, in_iterate))),
        6 => QueryIr::Distinct(Box::new(gen(rng, cat, rels, depth - 1, in_iterate))),
        7 => {
            // Reduce: aggregate over a valid column (`Sum` only over `Int`/`Any`).
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let t = ty_of(&input, cat);
            if t.arity() == 0 {
                return input;
            }
            let key = (0..rng.below(t.arity()))
                .map(|_| rng.below(t.arity()))
                .collect();
            let agg = match rng.below(4) {
                0 => Agg::Count,
                1 => {
                    let ints: Vec<usize> = (0..t.arity())
                        .filter(|&i| matches!(t.at(i), Some(Ty::Int) | Some(Ty::Any)))
                        .collect();
                    if ints.is_empty() {
                        Agg::Count
                    } else {
                        Agg::Sum(ints[rng.below(ints.len())])
                    }
                }
                2 => Agg::Min(rng.below(t.arity())),
                _ => Agg::Max(rng.below(t.arity())),
            };
            QueryIr::Reduce {
                input: Box::new(input),
                key,
                agg,
            }
        }
        _ => {
            // Iterate: the fixpoint-widening bug class. `init` is a *uniform*
            // row of one concrete type `u` and arity `a >= 2`; the step rotates
            // the recursion variable one position and unions in a constant row
            // that injects a *foreign* type `f` at a single column `p`. The
            // rotation carries that foreign value out of column `p` into another
            // column one round later — so a column that a single Kleene step
            // still calls a concrete type actually holds a foreign value at
            // runtime. Only iterating to the fixpoint synthesizes a type that
            // admits it; a single step is unsound and this plan reveals it.
            let a = 2 + rng.below(2); // 2..=3
            let driver = rels[rng.below(rels.len())];
            let u = rng.concrete_ty();
            // A foreign type distinct from `u` (so widening is observable).
            let mut f = rng.concrete_ty();
            while f == u {
                f = rng.concrete_ty();
            }
            let p = rng.below(a);

            let init = QueryIr::Map {
                input: Box::new(QueryIr::Rel(driver)),
                map: MapExpr {
                    out: (0..a).map(|_| RowExpr::Lit(rng.value(u))).collect(),
                },
            };
            // Rotate: output column i draws recursion column (i + 1) % a.
            let rot: Vec<usize> = (0..a).map(|i| (i + 1) % a).collect();
            let arm1 = QueryIr::Project {
                input: Box::new(QueryIr::Recur),
                cols: rot,
            };
            let const_out = (0..a)
                .map(|i| RowExpr::Lit(rng.value(if i == p { f } else { u })))
                .collect();
            let arm2 = QueryIr::Map {
                input: Box::new(QueryIr::Rel(driver)),
                map: MapExpr { out: const_out },
            };
            QueryIr::Iterate {
                init: Box::new(init),
                step: Box::new(QueryIr::Union(Box::new(arm1), Box::new(arm2))),
            }
        }
    }
}

#[test]
fn randomized_synthesized_type_admits_every_runtime_row() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut plans_run = 0usize;
    let mut rows_checked = 0usize;

    for _ in 0..64 {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let cat = MemSchemas::default();

        // Random base relations with concrete schemas, registered for typing.
        let nrels = 2 + rng.below(2); // 2..=3
        let mut rels = Vec::new();
        for r in 0..nrels {
            let id = RelId((r + 1) as u32);
            let arity = 1 + rng.below(3); // 1..=3 columns
            let cols: Vec<Column> = (0..arity)
                .map(|c| Column::new(format!("c{c}"), rng.concrete_ty()))
                .collect();
            let schema = Schema::new(cols);
            cat.put_schema(id, &schema, AT).unwrap();
            rels.push((id, RowTy::from_schema(&schema)));
        }

        // Generate schema-conforming rows and commit them in *shuffled* order —
        // each its own edition — so the data is churned across the commit clock.
        let mut pending: Vec<(RelId, Tuple)> = Vec::new();
        for (id, row_ty) in &rels {
            let m = 2 + rng.below(3); // 2..=4 rows
            for _ in 0..m {
                let vals: Vec<Value> = row_ty.cols().iter().map(|&ty| rng.value(ty)).collect();
                pending.push((*id, Tuple::new(vals)));
            }
        }
        rng.shuffle(&mut pending);
        for (id, tup) in &pending {
            store.commit(&[(*id, tup.clone(), 1)]).unwrap();
        }
        let at = store.current();

        let rel_ids: Vec<RelId> = rels.iter().map(|(id, _)| *id).collect();
        for _ in 0..4 {
            let q = gen(&mut rng, &cat, &rel_ids, 4, false);
            // Every generated plan is well-typed by construction.
            let syn = check_query(&q, &cat, at).expect("generated plan must type-check");
            // A reduce/iterate combination the runtime forbids can only arise if
            // generation is buggy; otherwise every plan runs.
            let out = match eval_snapshot(&q.clone().lower(), &store, at) {
                Ok(m) => m,
                Err(_) => continue,
            };
            plans_run += 1;
            for (row, diff) in &out {
                if *diff == 0 {
                    continue;
                }
                // No layout drift: the runtime row is as wide as the type says.
                assert_eq!(
                    row.arity(),
                    syn.arity(),
                    "row/type arity drift in plan {q:?}"
                );
                for (i, v) in row.as_slice().iter().enumerate() {
                    let col_ty = syn.at(i).unwrap();
                    assert!(
                        col_ty.admits(v),
                        "unsound: column {i} synthesized {col_ty:?} rejects runtime value {v:?}\nplan = {q:?}",
                    );
                    rows_checked += 1;
                }
            }
        }
    }

    // Guard against a vacuous oracle: it must have actually exercised plans/rows.
    assert!(plans_run >= 100, "too few plans exercised: {plans_run}");
    assert!(rows_checked > 0, "oracle validated no rows");
}
