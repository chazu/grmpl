//! # CALM classification (P8c)
//!
//! P8a ([`crate::check_query`]) typed a plan's rows; P8b ([`crate::effect`])
//! typed a handler's writes. P8c classifies a [`QueryIr`] plan along the axis the
//! **CALM** theorem (*Consistency As Logical Monotonicity*, Hellerstein 2010)
//! makes load-bearing: is the plan **monotone**?
//!
//! A query is monotone when *adding* tuples to its inputs can only *add* tuples
//! to its output — its result set never shrinks as the inputs grow. CALM's
//! result is that a monotone query has a consistent, **coordination-free**
//! evaluation: a domain can read it against another domain's growing state
//! without a barrier, because no later input can retract a row it already saw
//! (`DESIGN.md` §10.4 — the principled answer to cross-domain read consistency).
//! A non-monotone query does not have that guarantee: a future insertion can
//! retract a row, so a coordination-free reader could observe a value that a
//! later fact takes back.
//!
//! ## The classification is structural and sound-by-over-approximation
//!
//! Monotonicity here is a property of the **plan shape**, read straight off the
//! P7 [`QueryIr`] — no schemas, no data, no run. Every operator is either
//! monotone-preserving or not:
//!
//! | operator                                            | monotone? | why                                                    |
//! |-----------------------------------------------------|-----------|--------------------------------------------------------|
//! | `Rel`                                               | ✓         | a base input only grows under insertion                |
//! | `Map` / `Filter` / `Project`                        | ✓         | row-wise: each input row maps to fixed output rows     |
//! | `Join`                                              | ✓         | positive × positive — new rows only add matches        |
//! | `Union`                                             | ✓         | more on either side is more out                        |
//! | `Distinct`                                          | ✓         | duplicate elimination never drops a present tuple       |
//! | `Recur`                                             | ✓         | the fixpoint accumulator, monotone within a monotone body |
//! | `Iterate`                                            | ✓ if body is | a least fixpoint of a monotone step is monotone     |
//! | `Negate`                                            | ✗         | a sign flip: an added input row *retracts* an output row |
//! | `Reduce`                                            | ✗         | updating a group retracts its prior aggregate tuple     |
//!
//! So a plan is monotone **iff it contains no [`Negate`] and no [`Reduce`] node**
//! anywhere in its tree ([`Iterate`] inherits its body's classification —
//! a `Negate` inside a `step` blocks the whole fixpoint). [`classify`] returns
//! [`Monotone`](Monotonicity::Monotone) for such plans, and otherwise the list of
//! [`Blocker`]s — the non-monotone operators it found, in pre-order.
//!
//! The direction of the analysis is **sound, not complete**: a `Monotone`
//! verdict is a guarantee (the plan genuinely never retracts under insertion,
//! proven by the falsification oracle in this module's tests), while a
//! `NonMonotone` verdict is a conservative *may*. A plan like
//! `distinct(union(a, negate(a)))` is always empty and so technically monotone,
//! but it carries a `Negate` and we flag it. That asymmetry is the safe one for a
//! coordination decision: we never call a retracting plan coordination-free; at
//! worst we ask for coordination a cleverer analysis could have spared.
//!
//! ## Why `Reduce` is non-monotone even for `Count`/`Sum`
//!
//! An aggregate emits one tuple per group, `key ++ [aggregate]`
//! (`grmpl-diff::Query::Reduce`). When a group gains a member the aggregate
//! *value* changes, so the output tuple `(key, old)` is retracted and
//! `(key, new)` asserted — the output set is not growing, it is *churning*. That
//! retraction is exactly what makes aggregation coordination-sensitive, and it is
//! consistent with the engine treating `Reduce` as a non-linear boundary
//! recompute on the DRed path (`DESIGN.md` §3) and forbidding it inside an
//! `Iterate` (a non-monotone step has no monotone semi-naïve maintenance —
//! `docs/ROADMAP.md`, P2).
//!
//! ## Bright line
//!
//! Like the rest of `grmpl-type`, this module is **above the line**: it reads
//! only the reified IR and the aggregate tag, names no storage or network, and
//! runs nothing. The falsification oracle that *validates* the classification
//! against real runtime behavior lives in the crate's tests, where the concrete
//! store is a dev-dependency (mirroring the P8a soundness oracle).
//!
//! [`QueryIr`]: grmpl_lang::QueryIr
//! [`Negate`]: grmpl_lang::QueryIr::Negate
//! [`Reduce`]: grmpl_lang::QueryIr::Reduce
//! [`Iterate`]: grmpl_lang::QueryIr::Iterate

use std::fmt;

use grmpl_diff::Agg;
use grmpl_lang::QueryIr;

/// A single non-monotone operator occurrence — the reason a plan is not
/// CALM-monotone. [`classify`] reports one per non-monotone node, in the order a
/// pre-order walk meets them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocker {
    /// A [`QueryIr::Negate`](grmpl_lang::QueryIr::Negate): the sign flip means an
    /// added input row retracts an output row.
    Negation,
    /// A [`QueryIr::Reduce`](grmpl_lang::QueryIr::Reduce) with this aggregate:
    /// updating a group retracts its prior aggregate tuple.
    Aggregation(Agg),
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::Negation => write!(f, "negate (a sign flip retracts under insertion)"),
            Blocker::Aggregation(agg) => {
                write!(f, "reduce/{agg:?} (an aggregate retracts its prior value)")
            }
        }
    }
}

/// The CALM classification of a plan: either it is monotone (and therefore
/// coordination-free to read across a domain boundary), or it is not, together
/// with the non-monotone operators that block it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Monotonicity {
    /// No non-monotone operator appears: the plan's output set only grows as its
    /// inputs grow, so it is safe to read coordination-free.
    Monotone,
    /// The plan contains the listed non-monotone operators (in pre-order). A
    /// future insertion may retract an already-observed row, so a
    /// coordination-free reader is not guaranteed a consistent view.
    NonMonotone(Vec<Blocker>),
}

impl Monotonicity {
    /// Is the plan CALM-monotone (coordination-free)?
    pub fn is_monotone(&self) -> bool {
        matches!(self, Monotonicity::Monotone)
    }

    /// The non-monotone operators found, in pre-order — empty iff monotone.
    pub fn blockers(&self) -> &[Blocker] {
        match self {
            Monotonicity::Monotone => &[],
            Monotonicity::NonMonotone(bs) => bs,
        }
    }
}

/// Classify a [`QueryIr`] plan by CALM monotonicity, inference-only.
///
/// Walks the plan in pre-order collecting every non-monotone operator
/// ([`Negate`](grmpl_lang::QueryIr::Negate) →
/// [`Blocker::Negation`], [`Reduce`](grmpl_lang::QueryIr::Reduce) →
/// [`Blocker::Aggregation`]). Returns [`Monotonicity::Monotone`] when none are
/// found, else [`Monotonicity::NonMonotone`] with the blockers. Every other
/// operator preserves monotonicity, so it contributes nothing and the walk
/// simply recurses through it (an [`Iterate`](grmpl_lang::QueryIr::Iterate)
/// inherits its `init` and `step`'s classification).
pub fn classify(q: &QueryIr) -> Monotonicity {
    let mut blockers = Vec::new();
    collect_blockers(q, &mut blockers);
    if blockers.is_empty() {
        Monotonicity::Monotone
    } else {
        Monotonicity::NonMonotone(blockers)
    }
}

/// Pre-order accumulate the non-monotone operators of `q` into `out`.
fn collect_blockers(q: &QueryIr, out: &mut Vec<Blocker>) {
    match q {
        // Leaves: monotone, nothing below.
        QueryIr::Rel(_) | QueryIr::Recur => {}
        // Monotone, single monotone-preserving child.
        QueryIr::Map { input, .. }
        | QueryIr::Filter { input, .. }
        | QueryIr::Project { input, .. }
        | QueryIr::Distinct(input) => collect_blockers(input, out),
        // Monotone, two monotone-preserving children.
        QueryIr::Join { left, right, .. } | QueryIr::Union(left, right) => {
            collect_blockers(left, out);
            collect_blockers(right, out);
        }
        // Least fixpoint: monotone iff its body is, so just recurse into both.
        QueryIr::Iterate { init, step } => {
            collect_blockers(init, out);
            collect_blockers(step, out);
        }
        // The two blockers. Record, then keep walking — a subtree can hold more.
        QueryIr::Negate(input) => {
            out.push(Blocker::Negation);
            collect_blockers(input, out);
        }
        QueryIr::Reduce { input, agg, .. } => {
            out.push(Blocker::Aggregation(*agg));
            collect_blockers(input, out);
        }
    }
}

#[cfg(test)]
mod tests;
