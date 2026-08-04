//! Incremental maintenance of a recursive view (DESIGN.md §3.1, §10 risk 1).
//!
//! The stateless `eval_delta` path recomputes a recursive fixpoint from ∅ on
//! every change (the sanctioned fallback). This maintainer keeps the fixpoint
//! materialized and updates it in place, incrementally, for **both** directions
//! of change:
//!
//! * **Monotone** base changes (insertions only) grow the fixpoint by semi-naïve
//!   iteration warm-started from the previous fixpoint — only the growing
//!   frontier drives new derivations. This is the cheap, CALM-monotone case
//!   (DESIGN.md §10 #4).
//! * **Non-monotone** base changes (any retraction) are handled by **DRed**
//!   (delete-and-re-derive, Gupta–Mumick–Subrahmanian):
//!     1. **Overdeletion** removes every derived fact that had *a* derivation
//!        using a deleted base/`init` fact — even if it had other derivations.
//!        Seeded from the derivations broken by the deletion (found by diffing
//!        `step` over the old vs. new base) and propagated through the recursion
//!        variable, so a fact losing its last *grounded* support cascades. This
//!        over-approximates: it can also remove facts that survive.
//!     2. **Regrowth** (the same semi-naïve pass) re-derives, from the surviving
//!        set, everything still reachable under the new base — repairing the
//!        over-approximation and folding in any co-committed insertions.
//!        No recompute-from-∅.
//!
//! Diffing old-vs-new base is why DRed here needs no per-tuple derivation
//! provenance: a one-step re-evaluation of `step` at each edition tells us
//! exactly which derivations the change broke. Crucially, overdeletion keys on
//! *broken derivations*, not on loss-of-all-support, so it correctly collapses
//! **non-well-founded derivation cycles** — e.g. `impl(a)` and `impl(b)` that
//! mutually support each other through a prototype cycle after their `direct`
//! grounding is retracted (a plain "still one-step supported?" test would keep
//! them forever).
//!
//! Either way the result is identical to `find(iterate(init, step))`, so the
//! maintainer is a drop-in acceleration of the same law — validated against a
//! boundary recompute across randomized mixed churn (see the tests).
//!
//! Both fast paths assume the recursion is **linear** (`step` distributes over
//! unions of the recursion variable — e.g. `base ⋈ recur`), which covers
//! transitive-closure views such as `implements`.

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, RelId, Result, TraceStore, Tuple};

use crate::multiset::Multiset;
use crate::query::{eval_snapshot, eval_with, eval_with_recur, Query};

/// Which maintenance path `advance` took (instrumentation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Maintenance {
    /// Base only grew: semi-naïve regrowth warm-started from the fixpoint.
    Grow,
    /// Base shrank (possibly alongside growth): DRed overdeletion then regrowth.
    DeleteRederive,
}

/// Collect every base relation referenced by a query (for monotonicity checks,
/// and for the delta path's "was anything touched?" routing question).
pub(crate) fn collect_rels_of(q: &Query, out: &mut Vec<RelId>) {
    collect_rels(q, out)
}

/// Collect every base relation referenced by a query (for monotonicity checks).
fn collect_rels(q: &Query, out: &mut Vec<RelId>) {
    match q {
        Query::Rel(r) | Query::RangeRel { rel: r, .. } | Query::RangeRelOn { rel: r, .. } => {
            out.push(*r)
        }
        Query::Map { input, .. }
        | Query::Filter { input, .. }
        | Query::Project { input, .. } => collect_rels(input, out),
        Query::Negate(a) | Query::Distinct(a) => collect_rels(a, out),
        Query::Reduce { input, .. } => collect_rels(input, out),
        Query::Join { left, right, .. } | Query::Union(left, right) => {
            collect_rels(left, out);
            collect_rels(right, out);
        }
        Query::Iterate { init, step } => {
            collect_rels(init, out);
            collect_rels(step, out);
        }
        Query::Shared(inner) => collect_rels(inner, out),
        Query::Recur => {}
    }
}

/// The set (weight-1) view of a multiset: every tuple with positive weight.
fn as_set(m: &Multiset) -> Multiset {
    m.iter().filter(|(_, d)| **d > 0).map(|(t, _)| (t.clone(), 1)).collect()
}

fn subtract_in_place(a: &mut Multiset, remove: &Multiset) {
    a.retain(|t, _| !remove.contains_key(t));
}

/// `new − old` as signed set deltas (`+1` added, `-1` removed).
///
/// Generic in the key so the *one* definition serves both maintainers: the
/// recursive view diffs its fixpoint (`K = Tuple`), and the parse stream diffs
/// its match-set (`K = Value`, see [`crate::parse_stream`]). Both are
/// boundary-recompute strategies whose emitted delta is exactly this set
/// difference — the composition P9c §6 calls for — so neither reimplements it.
pub(crate) fn set_difference<K: Clone + Eq + std::hash::Hash>(
    new: &HashMap<K, Diff>,
    old: &HashMap<K, Diff>,
) -> HashMap<K, Diff> {
    let mut d = HashMap::new();
    for t in new.keys() {
        if !old.contains_key(t) {
            d.insert(t.clone(), 1);
        }
    }
    for t in old.keys() {
        if !new.contains_key(t) {
            d.insert(t.clone(), -1);
        }
    }
    d
}

/// A materialized, incrementally-maintained recursive view.
pub struct IncrementalFixpoint {
    init: Query,
    step: Query,
    base_rels: Vec<RelId>,
    r: Multiset, // current fixpoint (a set)
    at: Edition,
    /// Instrumentation: regrowth (semi-naïve) iterations taken by the last `advance`.
    pub last_iterations: usize,
    /// Instrumentation: overdeletion rounds taken by the last `advance` (0 unless
    /// the change retracted a supporting fact).
    pub last_overdeletion_rounds: usize,
    /// Instrumentation: which path the last `advance` took.
    pub last_path: Maintenance,
}

impl IncrementalFixpoint {
    /// Materialize the fixpoint at `from`.
    pub fn new(init: Query, step: Query, store: &dyn TraceStore, from: Edition) -> Result<Self> {
        let seed = Query::iterate(init.clone(), step.clone());
        let r = eval_snapshot(&seed, store, from)?;
        let mut base_rels = Vec::new();
        collect_rels(&init, &mut base_rels);
        collect_rels(&step, &mut base_rels);
        Ok(IncrementalFixpoint {
            init,
            step,
            base_rels,
            r,
            at: from,
            last_iterations: 0,
            last_overdeletion_rounds: 0,
            last_path: Maintenance::Grow,
        })
    }

    /// The current fixpoint (a set).
    pub fn current(&self) -> &Multiset {
        &self.r
    }

    /// A base change is monotone iff no referenced base tuple was retracted.
    fn is_monotone(&self, store: &dyn TraceStore, from: Edition, to: Edition) -> Result<bool> {
        for rel in &self.base_rels {
            let before: HashMap<Tuple, i64> = store.read_at(*rel, from)?.into_iter().collect();
            let after: HashMap<Tuple, i64> = store.read_at(*rel, to)?.into_iter().collect();
            for (t, w) in &before {
                if *w > 0 && after.get(t).copied().unwrap_or(0) <= 0 {
                    return Ok(false); // a supporting tuple vanished
                }
            }
        }
        Ok(true)
    }

    /// Advance to edition `to`, returning the signed delta of the fixpoint.
    pub fn advance(&mut self, store: &dyn TraceStore, to: Edition) -> Result<Multiset> {
        let monotone = self.is_monotone(store, self.at, to)?;
        let (start, od_rounds, path) = if monotone {
            (self.r.clone(), 0, Maintenance::Grow)
        } else {
            let (survivors, rounds) = self.overdelete(store, to)?;
            (survivors, rounds, Maintenance::DeleteRederive)
        };
        let (r_new, iters) = self.grow(store, to, start)?;

        self.last_path = path;
        self.last_overdeletion_rounds = od_rounds;
        self.last_iterations = iters;

        let delta = set_difference(&r_new, &self.r);
        self.r = r_new;
        self.at = to;
        Ok(delta)
    }

    /// DRed overdeletion: remove every fact in the current fixpoint that had *a*
    /// derivation using a retracted `init`/base fact, propagated through the
    /// recursion variable. Returns the surviving set (an under-approximation of
    /// the new fixpoint — regrowth repairs it) and the number of propagation
    /// rounds. Keying on broken derivations (not loss-of-all-support) is what
    /// makes this collapse non-well-founded derivation cycles correctly.
    fn overdelete(&self, store: &dyn TraceStore, to: Edition) -> Result<(Multiset, usize)> {
        let old = self.at;
        let r_old = &self.r;

        // Seed the overdeletion with every derivation that used a *retracted*
        // base tuple. For each base relation with deletions, evaluate `init` and
        // `step` (over the old fixpoint) with that relation overridden to hold
        // ONLY its deleted tuples — so the result is exactly the facts that had a
        // derivation resting on something now gone. Unioning over relations
        // covers "used any deleted tuple". This must key on the deleted tuples
        // themselves, not on "lost all one-step support": under a derivation
        // cycle the stale facts keep re-deriving each other, hiding the breakage.
        let mut od = Multiset::new();
        let mut frontier = Multiset::new();
        let seed = |t: &Tuple, od: &mut Multiset, frontier: &mut Multiset| {
            if r_old.contains_key(t) && od.insert(t.clone(), 1).is_none() {
                frontier.insert(t.clone(), 1);
            }
        };
        for rel in &self.base_rels {
            let before: HashMap<Tuple, i64> = store.read_at(*rel, old)?.into_iter().collect();
            let after: HashMap<Tuple, i64> = store.read_at(*rel, to)?.into_iter().collect();
            let deleted: Multiset = before
                .iter()
                .filter(|(t, w)| **w > 0 && after.get(*t).copied().unwrap_or(0) <= 0)
                .map(|(t, _)| (t.clone(), 1))
                .collect();
            if deleted.is_empty() {
                continue;
            }
            let ov = HashMap::from([(*rel, deleted)]);
            let via_init = as_set(&eval_with(&self.init, store, old, None, &ov)?);
            let via_step = as_set(&eval_with(&self.step, store, old, Some(r_old), &ov)?);
            for t in via_init.keys().chain(via_step.keys()) {
                seed(t, &mut od, &mut frontier);
            }
        }

        let mut rounds = 0;
        while !frontier.is_empty() {
            rounds += 1;
            let derived = as_set(&eval_with_recur(&self.step, store, old, &frontier)?);
            let mut next = Multiset::new();
            for t in derived.keys() {
                if r_old.contains_key(t) && !od.contains_key(t) {
                    next.insert(t.clone(), 1);
                }
            }
            for t in next.keys() {
                od.insert(t.clone(), 1);
            }
            frontier = next;
        }

        let mut survivors = r_old.clone();
        subtract_in_place(&mut survivors, &od);
        Ok((survivors, rounds))
    }

    /// Semi-naïve regrowth from a starting set (a subset of the target fixpoint):
    /// derive everything still reachable under the new base and add it. Used as
    /// the monotone fast path (start = previous fixpoint) and as DRed's re-derive
    /// phase (start = overdeletion survivors).
    fn grow(&self, store: &dyn TraceStore, to: Edition, mut r: Multiset) -> Result<(Multiset, usize)> {
        // Seed frontier: newly-derivable tuples from `init` and from `step` over
        // the whole starting set, minus what we already have.
        let init_set = as_set(&eval_snapshot(&self.init, store, to)?);
        let step_set = as_set(&eval_with_recur(&self.step, store, to, &r)?);
        let mut frontier = init_set;
        for (t, _) in step_set {
            frontier.entry(t).or_insert(1);
        }
        subtract_in_place(&mut frontier, &r);

        let mut iters = 0;
        while !frontier.is_empty() {
            iters += 1;
            for t in frontier.keys() {
                r.insert(t.clone(), 1);
            }
            // Linearity: new derivations come only from the new recur tuples.
            let derived = as_set(&eval_with_recur(&self.step, store, to, &frontier)?);
            let mut next = derived;
            subtract_in_place(&mut next, &r);
            frontier = next;
        }
        Ok((r, iters))
    }
}
