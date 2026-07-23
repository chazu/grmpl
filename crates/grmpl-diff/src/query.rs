//! `Query` — a dataflow fragment *as a value* (DESIGN.md §2.3 #2).
//!
//! Building a `Query` is pure; evaluating it against an edition (`eval_snapshot`)
//! or maintaining it across editions (`eval_delta`) is a computation. The
//! operators are genuinely differential: linear operators push child deltas
//! straight through, `join` uses the bilinear rule
//! `Δ(A⋈B) = ΔA⋈B_from + A_to⋈ΔB` (DESIGN.md §3.1), and `distinct` recomputes
//! its non-linear boundary. `iterate`/`reduce` arrive in later milestones.

use std::collections::HashMap;
use std::sync::Arc;

use grmpl_core::{Diff, Edition, Result, RelId, TraceStore, Tuple, Value};

use crate::multiset::{self, Multiset};

/// A pure tuple→tuple transform.
pub type MapFn = Arc<dyn Fn(&Tuple) -> Tuple + Send + Sync>;
/// A pure tuple→bool predicate.
pub type Pred = Arc<dyn Fn(&Tuple) -> bool + Send + Sync>;

/// An immutable query plan.
#[derive(Clone)]
pub enum Query {
    Rel(RelId),
    Map { input: Box<Query>, f: MapFn },
    Filter { input: Box<Query>, pred: Pred },
    Project { input: Box<Query>, cols: Arc<[usize]> },
    Join {
        left: Box<Query>,
        right: Box<Query>,
        left_key: Arc<[usize]>,
        right_key: Arc<[usize]>,
    },
    Union(Box<Query>, Box<Query>),
    Negate(Box<Query>),
    Distinct(Box<Query>),
    /// Reference to the enclosing `Iterate`'s current value (the recursion
    /// variable). Only meaningful inside an `Iterate`'s `step`.
    Recur,
    /// Least fixpoint of `distinct(init ∪ step(Recur))` — recursive views such
    /// as `implements` (DESIGN.md §3.1). One recursion variable per `Iterate`
    /// (single-level recursion in v1).
    Iterate { init: Box<Query>, step: Box<Query> },
    /// A shared arrangement: a sub-DAG evaluated once and reused wherever the
    /// same `Arc` is referenced (DESIGN.md §3.2 — arrangement sharing is the
    /// compiler's main optimization). Build with [`Query::into_shared`].
    Shared(Arc<Query>),
}

impl Query {
    pub fn rel(r: RelId) -> Query {
        Query::Rel(r)
    }
    pub fn map(self, f: impl Fn(&Tuple) -> Tuple + Send + Sync + 'static) -> Query {
        Query::Map { input: Box::new(self), f: Arc::new(f) }
    }
    pub fn filter(self, p: impl Fn(&Tuple) -> bool + Send + Sync + 'static) -> Query {
        Query::Filter { input: Box::new(self), pred: Arc::new(p) }
    }
    pub fn project(self, cols: impl Into<Arc<[usize]>>) -> Query {
        Query::Project { input: Box::new(self), cols: cols.into() }
    }
    pub fn join(self, right: Query, left_key: impl Into<Arc<[usize]>>, right_key: impl Into<Arc<[usize]>>) -> Query {
        Query::Join {
            left: Box::new(self),
            right: Box::new(right),
            left_key: left_key.into(),
            right_key: right_key.into(),
        }
    }
    pub fn union(self, other: Query) -> Query {
        Query::Union(Box::new(self), Box::new(other))
    }
    pub fn negate(self) -> Query {
        Query::Negate(Box::new(self))
    }
    pub fn distinct(self) -> Query {
        Query::Distinct(Box::new(self))
    }
    /// The recursion variable, for use inside a `step`.
    pub fn recur() -> Query {
        Query::Recur
    }
    /// Least-fixpoint recursion: `init` seeds the relation; `step` (which refers
    /// to `Query::recur()`) contributes derived tuples until nothing new appears.
    pub fn iterate(init: Query, step: Query) -> Query {
        Query::Iterate { init: Box::new(init), step: Box::new(step) }
    }
    /// Mark this sub-DAG as a shared arrangement. Clone the returned value to
    /// reference the same arrangement from several places; it is then evaluated
    /// once per edition rather than once per reference.
    pub fn into_shared(self) -> Query {
        Query::Shared(Arc::new(self))
    }
}

// ---- helpers -------------------------------------------------------------

fn key(t: &Tuple, cols: &[usize]) -> Vec<Value> {
    cols.iter().map(|&i| t.as_slice()[i].clone()).collect()
}

fn project_tuple(t: &Tuple, cols: &[usize]) -> Tuple {
    Tuple::new(cols.iter().map(|&i| t.as_slice()[i].clone()).collect::<Vec<_>>())
}

fn concat(l: &Tuple, r: &Tuple) -> Tuple {
    let mut v = Vec::with_capacity(l.arity() + r.arity());
    v.extend_from_slice(l.as_slice());
    v.extend_from_slice(r.as_slice());
    Tuple::new(v)
}

/// Multiset join on key columns, concatenating matched rows; weights multiply.
fn join_multisets(left: &Multiset, lk: &[usize], right: &Multiset, rk: &[usize]) -> Multiset {
    let mut idx: HashMap<Vec<Value>, Vec<(&Tuple, Diff)>> = HashMap::new();
    for (rt, rd) in right {
        idx.entry(key(rt, rk)).or_default().push((rt, *rd));
    }
    let mut out = Multiset::new();
    for (lt, ld) in left {
        if let Some(rs) = idx.get(&key(lt, lk)) {
            for (rt, rd) in rs {
                multiset::add(&mut out, concat(lt, rt), ld * rd);
            }
        }
    }
    multiset::strip_zeros(&mut out);
    out
}

fn negate(m: &Multiset) -> Multiset {
    m.iter().map(|(t, d)| (t.clone(), -d)).collect()
}

/// The non-linear `distinct` boundary at one edition: weight 1 for every tuple
/// whose accumulated input weight is positive.
fn distinct_snapshot(input: &Multiset) -> Multiset {
    input
        .iter()
        .filter(|(_, d)| **d > 0)
        .map(|(t, _)| (t.clone(), 1))
        .collect()
}

// ---- evaluation ----------------------------------------------------------

/// A per-edition arrangement cache: `(shared-node identity, edition) → value`.
/// Reusing one across several queries shares their common sub-DAGs.
pub type Arrangements = HashMap<(usize, u64), Multiset>;

/// Full evaluation at one edition (the `find` primitive).
pub fn eval_snapshot(q: &Query, store: &dyn TraceStore, at: Edition) -> Result<Multiset> {
    let mut arr = Arrangements::new();
    eval_inner(q, store, at, None, None, &mut arr)
}

/// Evaluate reusing a caller-supplied arrangement cache, so shared sub-DAGs
/// (marked with [`Query::into_shared`]) evaluate once across many queries at the
/// same edition.
pub fn eval_snapshot_with(
    q: &Query,
    store: &dyn TraceStore,
    at: Edition,
    arr: &mut Arrangements,
) -> Result<Multiset> {
    eval_inner(q, store, at, None, None, arr)
}

/// Evaluate `q` at `at` with the recursion variable ([`Query::recur`]) bound to
/// `recur`. Used by the incremental recursive maintainer to evaluate a `step`
/// against a chosen frontier.
pub fn eval_with_recur(
    q: &Query,
    store: &dyn TraceStore,
    at: Edition,
    recur: &Multiset,
) -> Result<Multiset> {
    let mut arr = Arrangements::new();
    eval_inner(q, store, at, Some(recur), None, &mut arr)
}

/// Evaluate `q` at `at` with the recursion variable optionally bound (`recur`)
/// and with some base relations replaced by in-memory contents (`overrides`):
/// wherever the plan reads `Rel(r)` and `r` is in `overrides`, the supplied
/// multiset is used instead of the store. Non-overridden relations still read
/// from the store at `at`. Used by DRed overdeletion to evaluate `step` against
/// *only the deleted tuples* of a base relation.
pub fn eval_with(
    q: &Query,
    store: &dyn TraceStore,
    at: Edition,
    recur: Option<&Multiset>,
    overrides: &HashMap<RelId, Multiset>,
) -> Result<Multiset> {
    let mut arr = Arrangements::new();
    eval_inner(q, store, at, recur, Some(overrides), &mut arr)
}

/// Evaluation with an optional recursion variable in scope (`recur`), optional
/// base-relation overrides (`overrides`), and a shared-arrangement cache (`arr`).
fn eval_inner(
    q: &Query,
    store: &dyn TraceStore,
    at: Edition,
    recur: Option<&Multiset>,
    overrides: Option<&HashMap<RelId, Multiset>>,
    arr: &mut Arrangements,
) -> Result<Multiset> {
    Ok(match q {
        Query::Rel(r) => match overrides.and_then(|o| o.get(r)) {
            Some(m) => m.clone(),
            None => multiset::from_pairs(store.read_at(*r, at)?),
        },
        Query::Map { input, f } => {
            let child = eval_inner(input, store, at, recur, overrides, arr)?;
            let mut out = Multiset::new();
            for (t, d) in &child {
                multiset::add(&mut out, f(t), *d);
            }
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Filter { input, pred } => {
            let child = eval_inner(input, store, at, recur, overrides, arr)?;
            child.into_iter().filter(|(t, _)| pred(t)).collect()
        }
        Query::Project { input, cols } => {
            let child = eval_inner(input, store, at, recur, overrides, arr)?;
            let mut out = Multiset::new();
            for (t, d) in &child {
                multiset::add(&mut out, project_tuple(t, cols), *d);
            }
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Join { left, right, left_key, right_key } => {
            let l = eval_inner(left, store, at, recur, overrides, arr)?;
            let r = eval_inner(right, store, at, recur, overrides, arr)?;
            join_multisets(&l, left_key, &r, right_key)
        }
        Query::Union(a, b) => {
            let mut out = eval_inner(a, store, at, recur, overrides, arr)?;
            let rhs = eval_inner(b, store, at, recur, overrides, arr)?;
            multiset::merge(&mut out, &rhs);
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Negate(a) => negate(&eval_inner(a, store, at, recur, overrides, arr)?),
        Query::Distinct(a) => distinct_snapshot(&eval_inner(a, store, at, recur, overrides, arr)?),
        Query::Recur => recur.cloned().unwrap_or_default(),
        Query::Iterate { init, step } => {
            // Least fixpoint of R = distinct(init ∪ step(R)) over a finite
            // domain, so iteration terminates (R grows monotonically as a set).
            let init_val = eval_inner(init, store, at, None, overrides, arr)?;
            let mut r = distinct_snapshot(&init_val);
            loop {
                let stepped = eval_inner(step, store, at, Some(&r), overrides, arr)?;
                let mut union = init_val.clone();
                multiset::merge(&mut union, &stepped);
                let next = distinct_snapshot(&union);
                if next == r {
                    break;
                }
                r = next;
            }
            r
        }
        Query::Shared(inner) => {
            // Memoize only in a plain context: inside an `Iterate` the same node
            // evaluates against different `recur` values, and with `overrides`
            // the store contents are shadowed — caching by (identity, edition)
            // alone would be unsound in either case.
            if recur.is_none() && overrides.is_none() {
                let k = (Arc::as_ptr(inner) as usize, at.0);
                if let Some(hit) = arr.get(&k) {
                    return Ok(hit.clone());
                }
                let v = eval_inner(inner, store, at, None, None, arr)?;
                arr.insert(k, v.clone());
                v
            } else {
                eval_inner(inner, store, at, recur, overrides, arr)?
            }
        }
    })
}

/// The change in a query's result over `(from, to]` — computed incrementally
/// from children's deltas (linear ops), the bilinear rule (join), or a
/// boundary recompute (distinct). Equal to `snapshot(to) − snapshot(from)`.
pub fn eval_delta(q: &Query, store: &dyn TraceStore, from: Edition, to: Edition) -> Result<Multiset> {
    Ok(match q {
        Query::Rel(r) => {
            let ups = store.scan_updates(*r, from, to)?;
            multiset::from_pairs(ups.into_iter().map(|u| (u.tuple, u.diff)))
        }
        Query::Map { input, f } => {
            let d = eval_delta(input, store, from, to)?;
            let mut out = Multiset::new();
            for (t, w) in &d {
                multiset::add(&mut out, f(t), *w);
            }
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Filter { input, pred } => {
            let d = eval_delta(input, store, from, to)?;
            d.into_iter().filter(|(t, _)| pred(t)).collect()
        }
        Query::Project { input, cols } => {
            let d = eval_delta(input, store, from, to)?;
            let mut out = Multiset::new();
            for (t, w) in &d {
                multiset::add(&mut out, project_tuple(t, cols), *w);
            }
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Join { left, right, left_key, right_key } => {
            // Δ(A⋈B) = ΔA⋈B_from + A_to⋈ΔB
            let da = eval_delta(left, store, from, to)?;
            let db = eval_delta(right, store, from, to)?;
            let b_from = eval_snapshot(right, store, from)?;
            let a_to = eval_snapshot(left, store, to)?;
            let mut out = join_multisets(&da, left_key, &b_from, right_key);
            let rhs = join_multisets(&a_to, left_key, &db, right_key);
            multiset::merge(&mut out, &rhs);
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Union(a, b) => {
            let mut out = eval_delta(a, store, from, to)?;
            let rhs = eval_delta(b, store, from, to)?;
            multiset::merge(&mut out, &rhs);
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Negate(a) => negate(&eval_delta(a, store, from, to)?),
        Query::Distinct(a) => {
            // Non-linear: recompute the boundary at each end and difference.
            let to_set = distinct_snapshot(&eval_snapshot(a, store, to)?);
            let from_set = distinct_snapshot(&eval_snapshot(a, store, from)?);
            let mut out = to_set;
            multiset::merge(&mut out, &negate(&from_set));
            multiset::strip_zeros(&mut out);
            out
        }
        Query::Iterate { .. } => {
            // Recursive delta by boundary recompute — the design-sanctioned
            // recompute-on-change fallback for recursive views (DESIGN.md §10,
            // risk 1). Correct including retraction; incremental recursion via
            // the iteration coordinate is a later optimization behind this same
            // interface. `Δ = fixpoint(to) − fixpoint(from)`.
            let to_val = eval_snapshot(q, store, to)?;
            let from_val = eval_snapshot(q, store, from)?;
            let mut out = to_val;
            multiset::merge(&mut out, &negate(&from_val));
            multiset::strip_zeros(&mut out);
            out
        }
        // `Recur` is only meaningful inside an `Iterate`, whose delta is handled
        // above by boundary recompute; at top level it contributes nothing.
        Query::Recur => Multiset::new(),
        // A shared arrangement is transparent to delta computation.
        Query::Shared(inner) => eval_delta(inner, store, from, to)?,
    })
}
