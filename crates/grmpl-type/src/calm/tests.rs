//! Tests for CALM classification ([`classify`]) and its **falsification
//! oracle**.
//!
//! The unit tests pin the per-operator verdict. The oracle is the load-bearing
//! part: a law test, not an example. The classification claims something about
//! *runtime* behavior — a `Monotone` plan's output set never shrinks as its
//! inputs grow — so we falsify it against the real engine. We churn a growing
//! sequence of insertions into a real store, snapshot each generated plan at
//! every growth step, and assert that every plan `classify` calls `Monotone` has
//! a non-decreasing positive support across the whole sequence. A single dropped
//! row under pure insertion would falsify the classifier; none may.
//!
//! The oracle also guards against being *vacuously* satisfied: it asserts it
//! actually exercised monotone plans over real supersets, and — crucially — that
//! the harness demonstrably *can* observe a retraction, by requiring at least one
//! `NonMonotone` plan to shed a row across the same churn. Monotone plans passing
//! is only meaningful because retraction is observable here and they avoid it.

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{
    Column, Edition, EditionStore, Entity, RelId, Schema, SchemaCatalog, TraceStore, Tuple, Ty,
    Value,
};
use grmpl_diff::{eval_snapshot, Agg};
use grmpl_ent::EntStore;
use grmpl_lang::{MapExpr, PredExpr, QueryIr, RowExpr};

use super::{classify, Blocker, Monotonicity};
use crate::{check_query, RowTy};

// ---- unit: the per-operator verdict ----------------------------------------

fn rel(id: u32) -> QueryIr {
    QueryIr::Rel(RelId(id))
}

#[test]
fn linear_and_set_operators_are_monotone() {
    // rel / map / filter / project / join / union / distinct / recur-in-iterate.
    let plan = QueryIr::Distinct(Box::new(QueryIr::Union(
        Box::new(QueryIr::Join {
            left: Box::new(QueryIr::Filter {
                input: Box::new(QueryIr::Map {
                    input: Box::new(rel(1)),
                    map: MapExpr {
                        out: vec![RowExpr::Col(0)],
                    },
                }),
                pred: PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::Int(1))),
            }),
            right: Box::new(QueryIr::Project {
                input: Box::new(rel(2)),
                cols: vec![0],
            }),
            left_key: vec![0],
            right_key: vec![0],
        }),
        Box::new(QueryIr::Iterate {
            init: Box::new(rel(3)),
            step: Box::new(QueryIr::Project {
                input: Box::new(QueryIr::Recur),
                cols: vec![0],
            }),
        }),
    )));
    assert_eq!(classify(&plan), Monotonicity::Monotone);
    assert!(classify(&plan).is_monotone());
    assert!(classify(&plan).blockers().is_empty());
}

#[test]
fn negate_blocks_monotonicity() {
    let plan = QueryIr::Negate(Box::new(rel(1)));
    assert_eq!(
        classify(&plan),
        Monotonicity::NonMonotone(vec![Blocker::Negation])
    );
    assert!(!classify(&plan).is_monotone());
}

#[test]
fn reduce_blocks_monotonicity_and_carries_its_aggregate() {
    for agg in [Agg::Count, Agg::Sum(1), Agg::Min(0), Agg::Max(0)] {
        let plan = QueryIr::Reduce {
            input: Box::new(rel(1)),
            key: vec![0],
            agg,
        };
        assert_eq!(
            classify(&plan),
            Monotonicity::NonMonotone(vec![Blocker::Aggregation(agg)])
        );
    }
}

#[test]
fn a_blocker_nested_under_monotone_operators_is_still_found() {
    // distinct(map(negate(rel))) — the negate is deep, but it still blocks.
    let plan = QueryIr::Distinct(Box::new(QueryIr::Map {
        input: Box::new(QueryIr::Negate(Box::new(rel(1)))),
        map: MapExpr {
            out: vec![RowExpr::Col(0)],
        },
    }));
    assert_eq!(classify(&plan).blockers(), &[Blocker::Negation]);
}

#[test]
fn a_blocker_inside_an_iterate_step_blocks_the_fixpoint() {
    // The fixpoint of a non-monotone step is not monotone: a set-difference
    // (`union(recur, negate(rel))`) in the step must surface as a blocker.
    let plan = QueryIr::Iterate {
        init: Box::new(rel(1)),
        step: Box::new(QueryIr::Union(
            Box::new(QueryIr::Recur),
            Box::new(QueryIr::Negate(Box::new(rel(1)))),
        )),
    };
    assert_eq!(classify(&plan).blockers(), &[Blocker::Negation]);
}

#[test]
fn every_blocker_is_reported_in_pre_order() {
    // union(reduce(rel), negate(rel)) — reduce visited before negate.
    let plan = QueryIr::Union(
        Box::new(QueryIr::Reduce {
            input: Box::new(rel(1)),
            key: vec![0],
            agg: Agg::Count,
        }),
        Box::new(QueryIr::Negate(Box::new(rel(2)))),
    );
    assert_eq!(
        classify(&plan).blockers(),
        &[Blocker::Aggregation(Agg::Count), Blocker::Negation]
    );
}

#[test]
fn blocker_display_is_informative() {
    assert!(format!("{}", Blocker::Negation).contains("negate"));
    assert!(format!("{}", Blocker::Aggregation(Agg::Count)).contains("reduce"));
}

// ---- the randomized falsification oracle -----------------------------------

/// A minimal edition-agnostic [`SchemaCatalog`] to type generated plans (so the
/// generator can pick valid column indices/arities). Not used at runtime — the
/// engine reads raw tuples.
#[derive(Default)]
struct MemSchemas(Mutex<HashMap<RelId, Schema>>);

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

/// A tiny deterministic xorshift64 PRNG — reproducible, no `rand` dep (mirrors
/// the P8a soundness-oracle generator).
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
    fn below(&mut self, n: usize) -> usize {
        (self.u64() % n as u64) as usize
    }
    fn coin(&mut self) -> bool {
        self.u64() & 1 == 0
    }
    fn concrete_ty(&mut self) -> Ty {
        [Ty::Ent, Ty::Int, Ty::Text, Ty::Bool][self.below(4)]
    }
    fn lit(&mut self) -> Value {
        let t = self.concrete_ty();
        self.value(t)
    }
    /// Small domains, so joins/filters actually hit and groups actually share
    /// keys (which is what makes aggregates churn — and retractions observable).
    fn value(&mut self, ty: Ty) -> Value {
        match ty {
            Ty::Ent => Value::Ent(Entity(self.below(3) as u64)),
            Ty::Int => Value::Int(self.below(4) as i64),
            Ty::Text => Value::text(["a", "b", "c"][self.below(3)]),
            Ty::Bool => Value::Bool(self.coin()),
            _ => Value::Int(0),
        }
    }
    fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            xs.swap(i, self.below(i + 1));
        }
    }
}

/// The synthesized arity of a (closed) generated plan — used only to pick valid
/// operator parameters during generation, so every plan type-checks and runs.
fn arity_of(q: &QueryIr, cat: &MemSchemas) -> usize {
    check_query(q, cat, AT)
        .expect("generator built an ill-typed plan")
        .arity()
}

fn ty_of(q: &QueryIr, cat: &MemSchemas) -> RowTy {
    check_query(q, cat, AT).expect("generator built an ill-typed plan")
}

fn compatible(a: Ty, b: Ty) -> bool {
    a == Ty::Any || b == Ty::Any || a == b
}

/// Build a random well-typed (hence runnable) plan over `rels`. Inside an
/// `Iterate` the subtree stays free of `Reduce`/`Iterate` (the runtime forbids
/// an aggregate or nested fixpoint there). Includes `Negate` and `Reduce`, so
/// both monotone and non-monotone plans are generated.
fn gen(rng: &mut Rng, cat: &MemSchemas, rels: &[RelId], depth: usize, in_iterate: bool) -> QueryIr {
    if depth == 0 || rng.below(3) == 0 {
        return QueryIr::Rel(rels[rng.below(rels.len())]);
    }
    let n_ops = if in_iterate { 7 } else { 9 };
    match rng.below(n_ops) {
        0 => {
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let a = arity_of(&input, cat);
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
            let input = gen(rng, cat, rels, depth - 1, in_iterate);
            let a = arity_of(&input, cat);
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
            let a = gen(rng, cat, rels, depth - 1, in_iterate);
            let b = gen(rng, cat, rels, depth - 1, in_iterate);
            let m = arity_of(&a, cat).min(arity_of(&b, cat)).max(1);
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
            // A recursive fixpoint over a monotone step (all monotone here).
            let a = 1 + rng.below(2);
            let driver = rels[rng.below(rels.len())];
            let init = QueryIr::Map {
                input: Box::new(QueryIr::Rel(driver)),
                map: MapExpr {
                    out: (0..a).map(|_| RowExpr::Lit(rng.lit())).collect(),
                },
            };
            let step = QueryIr::Project {
                input: Box::new(QueryIr::Recur),
                cols: (0..a).collect(),
            };
            QueryIr::Iterate {
                init: Box::new(init),
                step: Box::new(step),
            }
        }
    }
}

/// The positive support of a plan at `at`: the set of tuples whose consolidated
/// weight is strictly positive — the rows an observer sees as "present".
fn present(q: &QueryIr, store: &EntStore, at: Edition) -> Option<std::collections::HashSet<Tuple>> {
    let out = eval_snapshot(&q.clone().lower(), store, at).ok()?;
    Some(
        out.into_iter()
            .filter(|(_, d)| *d > 0)
            .map(|(t, _)| t)
            .collect(),
    )
}

#[test]
fn monotone_plans_never_retract_under_insertion() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);

    let mut monotone_supersets_checked = 0usize;
    let mut monotone_plans_run = 0usize;
    let mut nonmonotone_retractions_seen = 0usize;

    for _ in 0..48 {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let cat = MemSchemas::default();

        // Random base relations with concrete schemas.
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
        let rel_ids: Vec<RelId> = rels.iter().map(|(id, _)| *id).collect();

        // Generate a batch of plans up front; snapshot them at each growth step.
        let plans: Vec<QueryIr> = (0..6)
            .map(|_| gen(&mut rng, &cat, &rel_ids, 4, false))
            .collect();

        // Grow the inputs monotonically: several rounds, each committing a fresh
        // batch of positive insertions and pinning an edition. Inputs only ever
        // gain tuples, so this is the CALM premise (insert-only).
        let mut editions = Vec::new();
        for _ in 0..4 {
            let mut pending: Vec<(RelId, Tuple)> = Vec::new();
            for (id, row_ty) in &rels {
                let m = 1 + rng.below(3); // 1..=3 new rows per relation per round
                for _ in 0..m {
                    let vals: Vec<Value> = row_ty.cols().iter().map(|&ty| rng.value(ty)).collect();
                    pending.push((*id, Tuple::new(vals)));
                }
            }
            rng.shuffle(&mut pending);
            for (id, tup) in &pending {
                store.commit(&[(*id, tup.clone(), 1)]).unwrap();
            }
            editions.push(store.current());
        }

        for q in &plans {
            let monotone = classify(q).is_monotone();
            // Snapshot the plan's present-set at each pinned edition.
            let snaps: Vec<std::collections::HashSet<Tuple>> = editions
                .iter()
                .filter_map(|&e| present(q, &store, e))
                .collect();
            if snaps.len() < 2 {
                continue; // a plan the runtime refused (or too few points)
            }
            if monotone {
                monotone_plans_run += 1;
            }
            for pair in snaps.windows(2) {
                let (before, after) = (&pair[0], &pair[1]);
                if monotone {
                    // Falsification: a monotone plan must never drop a present row
                    // when the inputs only grew.
                    for row in before {
                        assert!(
                            after.contains(row),
                            "unsound: `classify` called this plan Monotone, but row \
                             {row:?} present at one edition vanished at the next under \
                             pure insertion\nplan = {q:?}",
                        );
                    }
                    if !before.is_empty() {
                        monotone_supersets_checked += 1;
                    }
                } else if before.iter().any(|row| !after.contains(row)) {
                    // The harness demonstrably observes a retraction here — this
                    // is what makes the monotone plans' silence meaningful.
                    nonmonotone_retractions_seen += 1;
                }
            }
        }
    }

    // Non-vacuity: we must have actually exercised monotone plans over real
    // supersets, and the harness must have proven it *can* see a retraction.
    assert!(
        monotone_plans_run >= 40,
        "too few monotone plans exercised: {monotone_plans_run}"
    );
    assert!(
        monotone_supersets_checked > 0,
        "oracle checked no non-empty monotone support"
    );
    assert!(
        nonmonotone_retractions_seen > 0,
        "oracle never observed a retraction — it cannot distinguish monotone from not"
    );
}
