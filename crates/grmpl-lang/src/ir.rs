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

use grmpl_core::{RelId, Tuple, Value};
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
            QueryIr::Filter { input, pred } => Query::Filter {
                input: Box::new(input.lower()),
                pred: pred.lower(),
            },
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
