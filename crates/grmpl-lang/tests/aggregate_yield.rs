//! TKT-106: the text-surface aggregate *yield* grammar.
//!
//! P2 (TKT-74) landed the engine `Query::Reduce` operator and a programmatic
//! named-column surface, [`Program::reduce_view`] + [`NamedAgg`]. This ticket
//! adds the text grammar — `view team_totals() { score(p,t,pts) yield t,
//! sum(pts) }` — that lowers a `yield` clause carrying an aggregate to the same
//! `Query::Reduce`.
//!
//! The acceptance test (below) is a **seeded randomized-churn law oracle**, not
//! example-based unit tests (a couple of witnesses are kept for readability but
//! are not the acceptance test). Each round generates a random aggregate view
//! and asserts two invariants against random source data:
//!
//!   (1) **Grammar ≡ programmatic.** The text-surface view lowers to a query
//!       that produces *exactly* the same result as the TKT-74
//!       `reduce_view`/`NamedAgg` surface over the equivalent plain view — the
//!       two paths are observationally equivalent for the same aggregation.
//!   (2) **Determinism.** The output is identical regardless of the order the
//!       source tuples were committed in — no HashMap-iteration order leaks
//!       through the reduce.
//!
//! A tiny xorshift64* PRNG keeps the churn reproducible with no external `rand`
//! dependency (mirrors grmpl-ent's store_laws.rs and grmpl-lang's
//! catalog.rs); every assertion prints its `seed` so a failure replays.

use grmpl_core::{Diff, Entity, RelId, Tuple, Value};
use grmpl_diff::{Query, Snapshot};
use grmpl_lang::{NamedAgg, Program};

fn ent(x: u64) -> Value {
    Value::Ent(Entity(x))
}

// --- Witnesses (readability, not the acceptance test) -----------------------

const TEAM_TOTALS: &str = r#"
    rel score(player: Ent, team: Ent, points: Int)
    view team_totals() {
        score(p, t, pts)
        yield t, sum(pts)
    }
"#;

#[test]
fn sum_yield_groups_and_folds() {
    grmpl_conformance::for_each_store(|c| {
        let prog = Program::compile(TEAM_TOTALS, 1).unwrap();
        let score = prog.rel_id("score").unwrap();

        let store = c.store();
        store
            .commit(&[
                (score, Tuple::from([ent(1), ent(10), Value::Int(3)]), 1),
                (score, Tuple::from([ent(2), ent(10), Value::Int(5)]), 1),
                (score, Tuple::from([ent(3), ent(20), Value::Int(7)]), 1),
            ])
            .unwrap();

        let q = prog.view("team_totals", &[]).unwrap();
        assert_eq!(
            q.find(&Snapshot::at_current(store)).unwrap(),
            vec![
                (Tuple::from([ent(10), Value::Int(8)]), 1),
                (Tuple::from([ent(20), Value::Int(7)]), 1),
            ]
        );
    });
}

#[test]
fn count_yield_counts_distinct_projected_rows() {
    grmpl_conformance::for_each_store(|c| {
        // `count()` folds the size of each group *after* the view's `Distinct`, so
        // it counts the distinct **projected** rows per group — exactly what the
        // TKT-74 `NamedAgg::Count` surface does (set-semantics, see `reduce_view`).
        //
        // A subtle consequence worth pinning: because a `count()` yield adds no
        // column of its own, the projection is just the grouping columns, so every
        // group collapses to a single distinct row and the count is `1`. Counting
        // the *rows* of a group would need a non-group column in the projection,
        // which this set-valued surface cannot express — a faithful inheritance of
        // the P2 reduce, not a defect of the grammar. (Recorded as a follow-on.)
        let src = r#"
            rel score(player: Ent, team: Ent, points: Int)
            view roster() { score(p, t, pts) yield t, count() }
        "#;
        let prog = Program::compile(src, 1).unwrap();
        let score = prog.rel_id("score").unwrap();

        let store = c.store();
        store
            .commit(&[
                (score, Tuple::from([ent(1), ent(10), Value::Int(3)]), 1),
                (score, Tuple::from([ent(2), ent(10), Value::Int(5)]), 1),
                (score, Tuple::from([ent(3), ent(20), Value::Int(7)]), 1),
            ])
            .unwrap();

        let q = prog.view("roster", &[]).unwrap();
        assert_eq!(
            q.find(&Snapshot::at_current(store)).unwrap(),
            vec![
                (Tuple::from([ent(10), Value::Int(1)]), 1),
                (Tuple::from([ent(20), Value::Int(1)]), 1),
            ]
        );
    });
}

#[test]
fn malformed_aggregate_yields_are_rejected() {
    let rel = "rel score(p: Ent, t: Ent, pts: Int)\n";
    // Unknown aggregate function.
    assert!(Program::compile(&format!("{rel}view v() {{ score(p,t,pts) yield t, avg(pts) }}"), 1).is_err());
    // `count` takes no column.
    assert!(Program::compile(&format!("{rel}view v() {{ score(p,t,pts) yield t, count(pts) }}"), 1).is_err());
    // `sum` needs a column.
    assert!(Program::compile(&format!("{rel}view v() {{ score(p,t,pts) yield t, sum() }}"), 1).is_err());
    // At most one aggregate per view.
    assert!(Program::compile(
        &format!("{rel}view v() {{ score(p,t,pts) yield t, sum(pts), max(pts) }}"),
        1
    )
    .is_err());
}

// --- Randomized-churn law oracle (the acceptance test) ----------------------

/// Deterministic, seedable PRNG (xorshift64*). Reproducible churn without a
/// `rand`/`proptest` dependency in the language's test harness.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point of xorshift.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// One choice of aggregate: its text spelling, and the equivalent `NamedAgg`
/// for the programmatic surface. The `col` (if any) names a view variable
/// carrying an `Int` — distinct from the grouping variables so the two paths'
/// projection layouts never accidentally coincide by aliasing.
fn choose_agg(rng: &mut Rng) -> (String, NamedAgg, Option<&'static str>) {
    let col = if rng.below(2) == 0 { "vc" } else { "vd" };
    match rng.below(4) {
        0 => ("count()".to_string(), NamedAgg::Count, None),
        1 => (format!("sum({col})"), NamedAgg::Sum(col.to_string()), Some(col)),
        2 => (format!("min({col})"), NamedAgg::Min(col.to_string()), Some(col)),
        _ => (format!("max({col})"), NamedAgg::Max(col.to_string()), Some(col)),
    }
}

/// Commit `tuples` in the given `order`, each as its own commit (its own
/// edition), then materialize `q`. Committing in different orders is how the
/// determinism sub-law is exercised: the result must not depend on it.
fn find_committed(
    case: &grmpl_conformance::Case,
    score: RelId,
    tuples: &[[Value; 4]],
    order: &[usize],
    q: &Query,
) -> Vec<(Tuple, Diff)> {
    // A fresh store of the same substrate: the whole point is that the result
    // depends on the committed set, not on commit order or on leftovers.
    let sib = case.sibling();
    let store = sib.store();
    for &i in order {
        store
            .commit(&[(score, Tuple::from(tuples[i].clone()), 1)])
            .unwrap();
    }
    // Bound rather than returned directly: a `Snapshot` holds a pinned reader
    // borrowed from the store, so the temporary must drop before `sib` does.
    let rows = q.find(&Snapshot::at_current(store)).unwrap();
    rows
}

#[test]
fn grammar_yield_equals_programmatic_reduce_under_random_churn() {
    grmpl_conformance::for_each_store(|c| {
        for seed in 1..=32u64 {
            law_round(c, seed);
        }
    });
}

fn law_round(case: &grmpl_conformance::Case, seed: u64) {
    let mut rng = Rng::new(seed);

    // Grouping columns: a non-empty subset of the two `Ent` variables, in a
    // random order (order must not matter to the equivalence). Kept disjoint
    // from the aggregate's `Int` column.
    let mut group: Vec<&str> = ["va", "vb"].into_iter().filter(|_| rng.below(2) == 0).collect();
    if group.is_empty() {
        group.push(if rng.below(2) == 0 { "va" } else { "vb" });
    }
    let (agg_text, named_agg, agg_col) = choose_agg(&mut rng);

    // The `yield` clause: the grouping columns, then the aggregate.
    let group_join = group.join(", ");
    let grammar_yield = format!("{group_join}, {agg_text}");
    // The equivalent PLAIN view for the programmatic path yields the same
    // grouping columns followed by the aggregate's (raw) column, if any — the
    // layout `reduce_view` reduces over.
    let mut plain_yields = group.clone();
    if let Some(c) = agg_col {
        plain_yields.push(c);
    }
    let plain_yield = plain_yields.join(", ");

    // One program, one `score` relation, two views over the same body: the
    // grammar view `g` (aggregate yield) and the plain view `p` (reduced
    // programmatically). Sharing the compile pins one `RelId` for `score`.
    let src = format!(
        "rel score(a: Ent, b: Ent, c: Int, d: Int)\n\
         view g() {{ score(va, vb, vc, vd) yield {grammar_yield} }}\n\
         view p() {{ score(va, vb, vc, vd) yield {plain_yield} }}\n"
    );
    let prog = Program::compile(&src, 1).unwrap();
    let score = prog.rel_id("score").unwrap();

    let grammar_q = prog
        .view("g", &[])
        .unwrap_or_else(|e| panic!("seed {seed}: grammar view failed to compile: {e}\n{src}"));
    let prog_q = prog
        .reduce_view("p", &[], &group, named_agg)
        .unwrap_or_else(|e| panic!("seed {seed}: reduce_view failed: {e}\n{src}"));

    // Random source rows over small domains so groups collide and Min/Max/Sum
    // see repeats and negatives.
    let n = 3 + rng.below(13) as usize; // 3..=15 rows
    let mut tuples: Vec<[Value; 4]> = Vec::with_capacity(n);
    for _ in 0..n {
        let a = 1 + rng.below(2);
        let b = 1 + rng.below(2);
        let c = rng.below(7) as i64 - 3; // -3..=3
        let d = rng.below(7) as i64 - 3;
        tuples.push([ent(a), ent(b), Value::Int(c), Value::Int(d)]);
    }

    let fwd: Vec<usize> = (0..n).collect();
    let rev: Vec<usize> = (0..n).rev().collect();

    let grammar_fwd = find_committed(case, score, &tuples, &fwd, &grammar_q);
    let prog_fwd = find_committed(case, score, &tuples, &fwd, &prog_q);
    let grammar_rev = find_committed(case, score, &tuples, &rev, &grammar_q);

    // (1) Grammar ≡ programmatic: the text surface lowers to the same reduce.
    assert_eq!(
        grammar_fwd, prog_fwd,
        "seed {seed}: grammar yield != programmatic reduce_view\n{src}"
    );
    // (2) Determinism: output independent of commit order.
    assert_eq!(
        grammar_fwd, grammar_rev,
        "seed {seed}: grammar yield output depends on commit order\n{src}"
    );
}
