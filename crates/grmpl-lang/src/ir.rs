//! # Core IR — the CBPV value/computation/thunk split, reified
//!
//! grmpl's semantics rest on a call-by-push-value distinction (DESIGN.md §2.3):
//! *assembling* a dataflow plan or a grammar is pure — it produces a **value** —
//! while *materializing / subscribing / committing* is a **computation**, and a
//! computation held as data is a **thunk**. Before this phase the value layer
//! leaked: a compiled `view`'s filters, a `form`'s constructor, and a row
//! transform all escaped into opaque `Arc<dyn Fn>` closures the moment they were
//! built, so the plan-as-value was only *partly* inspectable. Typing (P8a),
//! constructor inversion (P9b), and storing behaviors as relations (P12) all
//! need the whole plan to be data.
//!
//! This module reifies the split:
//!
//! * **Value** — inspectable data, pure to build. The three closure families
//!   become [`PredExpr`] (was `grmpl_diff::Pred`), [`MapExpr`] (was
//!   `grmpl_diff::MapFn`), and [`CtorSpec`] (was `grmpl_pattern::Ctor`), over
//!   the scalar leaf [`RowExpr`]. A whole plan is a [`QueryIr`] (the reified
//!   `grmpl_diff::Query`) and a whole grammar is a [`FormIr`].
//! * **Computation** — a value paired with the effectful verb that runs it:
//!   [`Comp`] names *find* / *watch* / *parse* (commit lives in `grmpl-proc`).
//!   `Comp::Watch` is exactly the plan a P5 `on watch` reactive handler installs.
//! * **Thunk** — a suspended computation held as a value: the on-handler
//!   `grmpl_proc::Behavior` (`snapshot × message ⇒ patch`), produced by
//!   [`crate::Program::behavior`].
//!
//! Closures are generated only at the very end, by the `lower` methods here —
//! nowhere else in `compile.rs`. Everything up to that point is data.

use std::sync::Arc;

use grmpl_core::{Entity, RelId, Tuple, Value};
use grmpl_diff::{Agg, MapFn, Pred, Query};
use grmpl_pattern::{Bindings, Ctor, Form, Pattern, Rule, VarId};

/// A scalar expression over the columns of the current row — the leaf of the
/// reified value layer. Either a positional column reference or a constant.
#[derive(Clone, PartialEq, Debug)]
pub enum RowExpr {
    /// The `i`-th column of the row.
    Col(usize),
    /// A constant value.
    Lit(Value),
}

impl RowExpr {
    fn eval(&self, row: &Tuple) -> Value {
        match self {
            RowExpr::Col(i) => row.as_slice()[*i].clone(),
            RowExpr::Lit(v) => v.clone(),
        }
    }
}

/// A reified predicate — the inspectable form of `grmpl_diff::Pred`
/// (`Arc<dyn Fn(&Tuple) -> bool>`). Closures are generated only by [`lower`].
///
/// [`lower`]: PredExpr::lower
#[derive(Clone, PartialEq, Debug)]
pub enum PredExpr {
    /// Two row expressions compare equal (column=literal and column=column
    /// filters both take this shape).
    Eq(RowExpr, RowExpr),
    /// Conjunction of sub-predicates. An empty `And` is always true.
    And(Vec<PredExpr>),
}

impl PredExpr {
    /// Evaluate against a row, without materializing a closure.
    pub fn test(&self, row: &Tuple) -> bool {
        match self {
            PredExpr::Eq(a, b) => a.eval(row) == b.eval(row),
            PredExpr::And(ps) => ps.iter().all(|p| p.test(row)),
        }
    }

    /// Final lowering: generate the `Pred` closure.
    pub fn lower(self) -> Pred {
        Arc::new(move |t: &Tuple| self.test(t))
    }
}

/// A reified row→row transform — the inspectable form of `grmpl_diff::MapFn`
/// (`Arc<dyn Fn(&Tuple) -> Tuple>`). The output row is built column-by-column
/// from expressions over the input row.
#[derive(Clone, PartialEq, Debug)]
pub struct MapExpr {
    /// The output row, one [`RowExpr`] per column.
    pub out: Vec<RowExpr>,
}

impl MapExpr {
    /// Evaluate against a row, without materializing a closure.
    pub fn apply(&self, row: &Tuple) -> Tuple {
        Tuple::new(self.out.iter().map(|e| e.eval(row)).collect::<Vec<_>>())
    }

    /// Final lowering: generate the `MapFn` closure.
    pub fn lower(self) -> MapFn {
        Arc::new(move |t: &Tuple| self.apply(t))
    }
}

/// A reified constructor — the inspectable form of `grmpl_pattern::Ctor`
/// (`Arc<dyn Fn(&Bindings) -> Value>`), i.e. a `form` rule's `-> Tag(args)`
/// arrow. It names a `tag` and, for each constructor slot, the pattern variable
/// that fills it.
///
/// Being plain data rather than a closure is the point: P9b can *invert* a
/// `CtorSpec` (recover bindings from a constructed value — the slots map back
/// one-to-one) and P12 can *store* it, neither of which is possible for an
/// `Arc<dyn Fn>`.
#[derive(Clone, PartialEq, Debug)]
pub struct CtorSpec {
    /// The tag placed in the constructed tuple's head position.
    pub tag: String,
    /// The bound variable filling each subsequent slot, in order.
    pub args: Vec<VarId>,
}

impl CtorSpec {
    /// Build the tagged tuple `[Text(tag), arg0, arg1, …]` from `bindings`. A
    /// slot whose variable is unbound becomes empty text, matching v1 `form`
    /// semantics.
    pub fn build(&self, bindings: &Bindings) -> Value {
        let mut vals = Vec::with_capacity(self.args.len() + 1);
        vals.push(Value::text(&self.tag));
        for id in &self.args {
            vals.push(bindings.get(id).cloned().unwrap_or_else(|| Value::text("")));
        }
        Value::Tuple(Arc::from(vals))
    }

    /// Whether [`build`] can be inverted by [`print`] as a genuine two-sided
    /// inverse — true iff the argument slots map back **one-to-one**, i.e. no
    /// pattern variable fills more than one slot.
    ///
    /// A repeated variable breaks invertibility: `build` then forces the two
    /// slots it fills to carry the same value, so a shape-valid constructed
    /// value whose repeated slots *disagree* has no binding that rebuilds it —
    /// `print` can still recover a binding, but `build(print(v))` need not equal
    /// `v`. When this returns `true`, the round-trip law holds in both
    /// directions (see [`print`]).
    ///
    /// [`build`]: CtorSpec::build
    /// [`print`]: CtorSpec::print
    pub fn is_invertible(&self) -> bool {
        let mut seen = std::collections::HashSet::with_capacity(self.args.len());
        self.args.iter().all(|v| seen.insert(*v))
    }

    /// Invert [`build`]: recover the bindings a constructed `value` came from.
    ///
    /// Succeeds iff `value` has this constructor's shape — a [`Value::Tuple`]
    /// whose head is `Text(tag)` equal to `self.tag` and whose arity is
    /// `self.args.len() + 1` (the tag plus one slot per argument). It returns
    /// the variable→slot map, reading each argument slot back into the variable
    /// that filled it; on any shape mismatch it returns `None`.
    ///
    /// **Round-trip law.** For every binding `b` that binds all of `self.args`,
    /// `self.print(&self.build(b))` agrees with `b` on those variables. When
    /// [`is_invertible`] holds, the other direction is exact too: for every
    /// value `v` this spec could construct, `self.build(&self.print(v)?) == v`.
    ///
    /// (A slot whose variable was *unbound* was built as empty text, so `print`
    /// recovers it as bound-to-empty — the law is stated over fully-bound
    /// bindings, matching v1 `form` semantics.)
    ///
    /// [`build`]: CtorSpec::build
    /// [`is_invertible`]: CtorSpec::is_invertible
    pub fn print(&self, value: &Value) -> Option<Bindings> {
        let vals = match value {
            Value::Tuple(vals) => vals,
            _ => return None,
        };
        if vals.len() != self.args.len() + 1 {
            return None;
        }
        match &vals[0] {
            Value::Text(t) if **t == *self.tag => {}
            _ => return None,
        }
        let mut bindings = Bindings::with_capacity(self.args.len());
        for (id, slot) in self.args.iter().zip(&vals[1..]) {
            bindings.insert(*id, slot.clone());
        }
        Some(bindings)
    }

    /// Final lowering: generate the `Ctor` closure.
    pub fn lower(self) -> Ctor {
        Arc::new(move |b: &Bindings| self.build(b))
    }
}

/// A dataflow plan as inspectable data — the reified `grmpl_diff::Query`. It
/// mirrors `Query` one-for-one except that the two closure-carrying operators
/// (`Map`, `Filter`) hold a reified [`MapExpr`] / [`PredExpr`] instead of an
/// opaque `Fn`. [`lower`] turns it into a runnable `Query`, generating those
/// closures as the very last step.
///
/// `Query::Shared` (an arrangement-sharing hint, DESIGN.md §3.2) is deliberately
/// absent: it is an evaluation-time optimization, not a semantic node, and is
/// introduced during/after lowering rather than reified here.
///
/// [`lower`]: QueryIr::lower
#[derive(Clone, PartialEq, Debug)]
pub enum QueryIr {
    /// A base relation.
    Rel(RelId),
    /// Transform each row (reified `MapFn`).
    Map { input: Box<QueryIr>, map: MapExpr },
    /// Keep rows satisfying the predicate (reified `Pred`).
    Filter { input: Box<QueryIr>, pred: PredExpr },
    /// Reorder / select columns.
    Project {
        input: Box<QueryIr>,
        cols: Vec<usize>,
    },
    /// Equi-join on the paired key columns.
    Join {
        left: Box<QueryIr>,
        right: Box<QueryIr>,
        left_key: Vec<usize>,
        right_key: Vec<usize>,
    },
    /// Multiset union.
    Union(Box<QueryIr>, Box<QueryIr>),
    /// Sign negation.
    Negate(Box<QueryIr>),
    /// Collapse to set semantics.
    Distinct(Box<QueryIr>),
    /// Group by `key` and fold each group with `agg` (P2 aggregate yield).
    Reduce {
        input: Box<QueryIr>,
        key: Vec<usize>,
        agg: Agg,
    },
    /// The enclosing `Iterate`'s recursion variable.
    Recur,
    /// Least fixpoint of `distinct(init ∪ step(Recur))`.
    Iterate {
        init: Box<QueryIr>,
        step: Box<QueryIr>,
    },
}

impl QueryIr {
    /// Final lowering: build the runnable `Query`, generating `Map`/`Filter`
    /// closures. Every other node maps straight onto its `Query` counterpart.
    pub fn lower(self) -> Query {
        match self {
            QueryIr::Rel(r) => Query::rel(r),
            QueryIr::Map { input, map } => Query::Map {
                input: Box::new(input.lower()),
                f: map.lower(),
            },
            QueryIr::Filter { input, pred } => lower_filter(*input, pred),
            QueryIr::Project { input, cols } => input.lower().project(cols),
            QueryIr::Join {
                left,
                right,
                left_key,
                right_key,
            } => left.lower().join(right.lower(), left_key, right_key),
            QueryIr::Union(a, b) => a.lower().union(b.lower()),
            QueryIr::Negate(q) => q.lower().negate(),
            QueryIr::Distinct(q) => q.lower().distinct(),
            QueryIr::Reduce { input, key, agg } => input.lower().reduce(key, agg),
            QueryIr::Recur => Query::recur(),
            QueryIr::Iterate { init, step } => Query::iterate(init.lower(), step.lower()),
        }
    }
}

/// Lower a `Filter`, pushing a **lead-column equality** into the store as a
/// [`Query::range`] read when possible — the compiler half of E2b.
///
/// `Filter(Rel(r), … col0 == k …)` over a key `k` with a well-defined successor
/// (an `Ent` or `Int`) lowers to `RangeRel(r, [k], [k⁺])` — which, since tuple
/// order is lexicographic and [`Value`] order is type-major, contains **exactly**
/// the rows whose first column is `k` — with any remaining conjuncts left as a
/// residual `Filter`. So the substrate's WID enfilade prunes to the matching key
/// instead of scanning the whole relation, and the query is unchanged in meaning:
/// this is an evaluation-time optimization (like `Query::Shared`), which is why it
/// lives in lowering and leaves [`QueryIr`] — the inspected/typed form — as the
/// plain `Rel`+`Filter`. Any other shape lowers unchanged.
fn lower_filter(input: QueryIr, pred: PredExpr) -> Query {
    if let QueryIr::Rel(r) = input {
        // Prefer the lead column: it is the trace's primary order, so the range
        // is always prunable, on any store.
        if let Some((key, residual)) = pushdown_col_eq(&pred, 0) {
            let hi = lead_successor(&key).expect("candidate key has a successor");
            let base = Query::range(r, Tuple::from([key]), Tuple::from([hi]));
            return residual_filter(base, residual);
        }
        // Otherwise a *trailing*-column equality (G-9). This prunes only on a
        // store that keeps an Arrangement for that column; everywhere else
        // `read_range_on` falls back to read-and-filter, which is exactly the
        // `Filter` this replaces. So it is never worse and sometimes sublinear.
        if let Some((col, key, residual)) = pushdown_any_eq(&pred) {
            let hi = lead_successor(&key).expect("candidate key has a successor");
            let base = Query::range_on(r, col, key, hi);
            return residual_filter(base, residual);
        }
        return Query::Filter { input: Box::new(Query::rel(r)), pred: pred.lower() };
    }
    Query::Filter { input: Box::new(input.lower()), pred: pred.lower() }
}

/// Re-attach whatever conjuncts the pushdown did not consume.
fn residual_filter(base: Query, residual: Option<PredExpr>) -> Query {
    match residual {
        None => base,
        Some(p) => Query::Filter { input: Box::new(base), pred: p.lower() },
    }
}

/// The first *non-lead* column constrained to a literal with a successor, with
/// the remaining conjuncts. Column 0 is excluded — the caller tried it first and
/// it has the better range.
fn pushdown_any_eq(pred: &PredExpr) -> Option<(usize, Value, Option<PredExpr>)> {
    let conjuncts = conjuncts_of(pred);
    let (k, col) = conjuncts
        .iter()
        .enumerate()
        .find_map(|(i, p)| col_lit(p).filter(|(c, _)| *c != 0).map(|(c, _)| (i, c)))?;
    let (_, key) = col_lit(conjuncts[k]).expect("find_map matched a column literal");
    Some((col, key, residual_of(&conjuncts, k)))
}

/// The least value strictly greater than every tuple whose first column is `v` —
/// `v`'s successor in the total [`Value`] order — for the types that have one
/// (`Ent`/`Int`, guarding overflow). `None` for `Text`/`Bool` and at the numeric
/// ceiling, where no exact half-open key range exists and the caller keeps the
/// plain filter.
fn lead_successor(v: &Value) -> Option<Value> {
    match v {
        Value::Ent(e) => e.0.checked_add(1).map(|n| Value::Ent(Entity(n))),
        Value::Int(n) => n.checked_add(1).map(Value::Int),
        _ => None,
    }
}

/// If `pred` constrains column 0 to a literal with a [`lead_successor`], return
/// that key and the residual predicate (the other conjuncts, or `None` if the
/// equality was the whole predicate). Only a *pushable* key (successor exists) is
/// selected, so the caller can always build the range.
fn pushdown_col_eq(pred: &PredExpr, want: usize) -> Option<(Value, Option<PredExpr>)> {
    let conjuncts = conjuncts_of(pred);
    let k = conjuncts.iter().position(|p| col_lit(p).is_some_and(|(c, _)| c == want))?;
    let (_, key) = col_lit(conjuncts[k]).expect("position found a column literal");
    Some((key, residual_of(&conjuncts, k)))
}

fn conjuncts_of(pred: &PredExpr) -> Vec<&PredExpr> {
    match pred {
        PredExpr::And(ps) => ps.iter().collect(),
        other => vec![other],
    }
}

/// `(column, literal)` when `p` pins a column to a literal that has a
/// [`lead_successor`] — only a pushable key is selected, so the caller can always
/// build the half-open range.
fn col_lit(p: &PredExpr) -> Option<(usize, Value)> {
    let (a, b) = match p {
        PredExpr::Eq(a, b) => (a, b),
        _ => return None,
    };
    let (col, v) = match (a, b) {
        (RowExpr::Col(c), RowExpr::Lit(v)) | (RowExpr::Lit(v), RowExpr::Col(c)) => (*c, v),
        _ => return None,
    };
    lead_successor(v).map(|_| (col, v.clone()))
}

/// Every conjunct but the `k`th, recombined.
fn residual_of(conjuncts: &[&PredExpr], k: usize) -> Option<PredExpr> {
    let rest: Vec<PredExpr> =
        conjuncts.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, p)| (*p).clone()).collect();
    match rest.len() {
        0 => None,
        1 => Some(rest.into_iter().next().unwrap()),
        _ => Some(PredExpr::And(rest)),
    }
}

/// One grammar rule as inspectable data — the reified `grmpl_pattern::Rule`: a
/// pattern and the [`CtorSpec`] it constructs.
///
/// The `Pattern` itself is already structural data in `grmpl-pattern` (its only
/// closure, `Pattern::Guard`, is not producible from the `form` surface), so it
/// is carried through unchanged; only the constructor needed reifying.
#[derive(Clone)]
pub struct RuleIr {
    /// The structural pattern to match.
    pub pattern: Pattern,
    /// The constructor applied to a successful match.
    pub ctor: CtorSpec,
}

/// A grammar as inspectable data — the reified `grmpl_pattern::Form`.
#[derive(Clone)]
pub struct FormIr {
    /// The alternative rules, tried in order.
    pub rules: Vec<RuleIr>,
}

impl FormIr {
    /// Final lowering: build the runnable `Form`, generating each rule's `Ctor`.
    pub fn lower(self) -> Form {
        Form::new(
            self.rules
                .into_iter()
                .map(|r| Rule {
                    pattern: r.pattern,
                    ctor: r.ctor.lower(),
                })
                .collect(),
        )
    }
}

/// The reified **computation** layer of the CBPV split: a value ([`QueryIr`] /
/// [`FormIr`]) paired with the effectful verb that runs it. A `Comp` is itself
/// data — pure to construct and inspect — until it is lowered and run, which is
/// what makes an unrun computation a thunk. (The remaining surface computation,
/// *commit*, lives in `grmpl-proc`; the on-handler thunk is a
/// `grmpl_proc::Behavior`.)
#[derive(Clone)]
pub enum Comp {
    /// Materialize a plan at an edition — lowers to a `Query` for `Query::find`.
    Find(QueryIr),
    /// Maintain a plan as a delta stream — lowers to a `Query` for `Query::watch`.
    /// This is the plan a P5 `on watch` reactive handler installs.
    Watch(QueryIr),
    /// Parse an input sequence — lowers to a `Form` for `Form::parse`.
    Parse(FormIr),
}

impl Comp {
    /// The reified plan under a [`Comp::Find`] / [`Comp::Watch`], for inspection.
    pub fn plan(&self) -> Option<&QueryIr> {
        match self {
            Comp::Find(q) | Comp::Watch(q) => Some(q),
            Comp::Parse(_) => None,
        }
    }
}

#[cfg(test)]
mod pushdown_tests {
    use super::*;

    fn ent(n: u64) -> Value {
        Value::Ent(Entity(n))
    }

    #[test]
    fn lead_successor_only_for_ordinal_types() {
        assert_eq!(lead_successor(&ent(5)), Some(ent(6)));
        assert_eq!(lead_successor(&Value::Int(5)), Some(Value::Int(6)));
        // No exact half-open key range for text/bool, or at the numeric ceiling.
        assert_eq!(lead_successor(&Value::text("x")), None);
        assert_eq!(lead_successor(&Value::Bool(true)), None);
        assert_eq!(lead_successor(&Value::Int(i64::MAX)), None);
        assert_eq!(lead_successor(&Value::Ent(Entity(u64::MAX))), None);
    }

    #[test]
    fn pushdown_picks_the_lead_equality_and_keeps_the_rest() {
        // Bare lead equality → pushed, no residual.
        let p = PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(ent(5)));
        assert_eq!(pushdown_col_eq(&p, 0), Some((ent(5), None)));
        // Operand order does not matter.
        let p = PredExpr::Eq(RowExpr::Lit(ent(5)), RowExpr::Col(0));
        assert_eq!(pushdown_col_eq(&p, 0), Some((ent(5), None)));

        // Lead equality alongside a column-column join key → the join key stays.
        let join = PredExpr::Eq(RowExpr::Col(1), RowExpr::Col(0));
        let p = PredExpr::And(vec![
            PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(ent(5))),
            join.clone(),
        ]);
        assert_eq!(pushdown_col_eq(&p, 0), Some((ent(5), Some(join))));
    }

    #[test]
    fn pushdown_declines_unpushable_keys() {
        // Equality on a non-lead column → no *lead* pushdown…
        let p = PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(ent(5)));
        assert_eq!(pushdown_col_eq(&p, 0), None);
        // Lead column bound to a text literal (no successor) → no pushdown.
        let p = PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::text("x")));
        assert_eq!(pushdown_col_eq(&p, 0), None);
        assert_eq!(pushdown_any_eq(&p), None);
        // A column-column lead constraint is not a literal key.
        let p = PredExpr::Eq(RowExpr::Col(0), RowExpr::Col(2));
        assert_eq!(pushdown_col_eq(&p, 0), None);
        assert_eq!(pushdown_any_eq(&p), None);
    }

    /// **G-9's lowerer half.** A *trailing*-column equality is auto-emitted as a
    /// `RangeRelOn`, so a secondary-key view prunes at the source on a store
    /// carrying an Arrangement for that column — and reads exactly as the
    /// `Filter` it replaces on one that does not.
    #[test]
    fn pushdown_picks_a_trailing_equality_when_the_lead_has_none() {
        // …but it *is* a trailing pushdown, on column 1.
        let p = PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(ent(5)));
        assert_eq!(pushdown_any_eq(&p), Some((1, ent(5), None)));
        // Operand order does not matter.
        let p = PredExpr::Eq(RowExpr::Lit(Value::Int(7)), RowExpr::Col(2));
        assert_eq!(pushdown_any_eq(&p), Some((2, Value::Int(7), None)));
        // Other conjuncts survive as the residual.
        let join = PredExpr::Eq(RowExpr::Col(3), RowExpr::Col(0));
        let p = PredExpr::And(vec![
            PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(ent(5))),
            join.clone(),
        ]);
        assert_eq!(pushdown_any_eq(&p), Some((1, ent(5), Some(join))));
        // Column 0 is excluded — the caller already tried it and its range is
        // better, so a lead equality must not be claimed here.
        let p = PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(ent(5)));
        assert_eq!(pushdown_any_eq(&p), None);
    }

    /// The lead column is preferred when both are available: its range prunes on
    /// every store, the trailing one only where an Arrangement exists.
    #[test]
    fn lowering_prefers_the_lead_column() {
        let p = PredExpr::And(vec![
            PredExpr::Eq(RowExpr::Col(1), RowExpr::Lit(ent(9))),
            PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(ent(5))),
        ]);
        match lower_filter(QueryIr::Rel(RelId(1)), p) {
            Query::Filter { input, .. } => assert!(
                matches!(*input, Query::RangeRel { .. }),
                "a lead equality must lower to the lead range, not the trailing one"
            ),
            _ => panic!("expected a residual filter over a lead range"),
        }
    }

    /// With no lead equality, the trailing one is emitted.
    #[test]
    fn lowering_emits_a_trailing_range_when_that_is_all_there_is() {
        let p = PredExpr::Eq(RowExpr::Col(2), RowExpr::Lit(Value::Int(3)));
        match lower_filter(QueryIr::Rel(RelId(1)), p) {
            Query::RangeRelOn { rel, col, lo, hi } => {
                assert_eq!((rel, col), (RelId(1), 2));
                assert_eq!((lo, hi), (Value::Int(3), Value::Int(4)));
            }
            _ => panic!("expected a trailing-column range"),
        }
    }
}
