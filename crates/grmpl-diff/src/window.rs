//! State-mode window matching (P9c design §4a, §8.2) — the reduction from a
//! signed, unbounded delta stream to the finite, ordered `&[Value]` structure
//! the v1 pattern engine already consumes.
//!
//! A pattern **never** runs over the raw live stream; it runs over a *window*, a
//! finite, edition-bounded slice of the trace (§3). State mode is the "match the
//! window's net contents" reading of such a window: consolidate first — fold
//! `Σ diff` per tuple, drop zeros, sort — and hand the survivors to the existing
//! slice engine. `+t` then `−t` **cancels before matching**, so the pattern never
//! sees `t`. Retraction is absorbed by *arithmetic, upstream of the algebra*;
//! nothing in `grmpl-pattern` learns about signs, and state mode therefore needs
//! no new `MatchInput` instance at all.
//!
//! # Snapshot anchoring (the correctness trap)
//!
//! A delta inside the window may retract a tuple asserted *before* the window
//! opened. Consolidating the window's deltas **in isolation** then yields a
//! spurious negative multiplicity — a `(t, −1)` row for a tuple that is simply
//! *gone*, not "present −1 times". State-mode windows are therefore
//! **snapshot-anchored**: each touched tuple's window delta is resolved against
//! its weight in the base snapshot at `from`, so
//!
//! ```text
//!   net(t)  =  snapshot(q, from)(t)  +  delta(q, (from, to])(t)  =  snapshot(q, to)(t)
//! ```
//!
//! for every tuple the window touched. The result is exactly `find(q, to)`
//! restricted to those tuples — the two formulations the design calls
//! equivalent — so a pre-window assert retracted in-window resolves to weight
//! zero and is dropped by [`strip_zeros`](crate::multiset::strip_zeros) rather
//! than surfacing as a negative row.
//!
//! # Window bounds
//!
//! The window is the edition interval `(from, to]`, aligned exactly with
//! [`TraceStore::scan_updates`] and [`eval_delta`] — no new primitive and no new
//! ordering. (The design writes it `[from, to)`; it is the same half-open
//! interval the whole substrate uses, exclusive at the anchor and inclusive at
//! the far end.) The degenerate window `(Edition::ZERO, E]` is the **snapshot
//! window** of §5: its anchor is the empty world, so its contents are precisely
//! `find(q, E)`, which is how state-mode matching unifies with today's tuple
//! matching.
//!
//! Both edition doors of the P6 watermark apply unchanged: `from` below the
//! consolidation horizon is rejected by the store rather than answered from
//! truncated history, inherited for free from `eval_delta`/`eval_snapshot`.

use grmpl_core::{Diff, Edition, Result, TraceStore, Tuple, Value};

use crate::multiset::{self, Multiset};
use crate::query::{eval_delta, eval_snapshot, Query};

/// The snapshot-anchored net contents of one edition window: what a state-mode
/// pattern match sees, and nothing else.
///
/// Rows are **tuple-sorted** and **zero-free**, so the value is a deterministic
/// function of the trace regardless of the `HashMap` fold order used to build
/// it. Each row's weight is the tuple's true multiplicity at `to`
/// ([`consolidate_window`]), never an isolated window sum — see the module docs
/// on anchoring.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConsolidatedWindow {
    from: Edition,
    to: Edition,
    rows: Vec<(Tuple, Diff)>,
}

impl ConsolidatedWindow {
    /// The window's exclusive lower bound (the anchor edition).
    pub fn from(&self) -> Edition {
        self.from
    }

    /// The window's inclusive upper bound.
    pub fn to(&self) -> Edition {
        self.to
    }

    /// The consolidated rows, tuple-sorted and zero-free.
    pub fn rows(&self) -> &[(Tuple, Diff)] {
        &self.rows
    }

    /// The number of distinct tuples the window touched (not the summed
    /// multiplicity — see [`values`](Self::values)).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// No tuple survived consolidation: either the window is empty, or every
    /// change in it cancelled against the anchor.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The window's contents as the flat `&[Value]` sequence the v1 pattern
    /// engine matches (`Pattern::run_in`): one `Value::Tuple` per **present**
    /// row, repeated by its multiplicity, in the same tuple-sorted order as
    /// [`rows`](Self::rows).
    ///
    /// "Present" means positive weight, the same set boundary
    /// `distinct`/`reduce`/`Snapshot::holds` already take. A non-positive row
    /// can only arise where the query's own snapshot at `to` is negative there
    /// (a `Query::Negate`, or a trace that retracts what it never asserted) —
    /// anchoring guarantees it is never an artefact of the window bounds — and
    /// such a row contributes no values, since "present −1 times" is not a
    /// sequence the engine can match.
    pub fn values(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (t, d) in &self.rows {
            for _ in 0..(*d).max(0) {
                out.push(Value::Tuple(t.0.clone()));
            }
        }
        out
    }
}

/// Consolidate the window `(from, to]` of `q` into its snapshot-anchored net
/// contents (design §4a) — the state-mode reduction, ready for the existing
/// slice engine via [`ConsolidatedWindow::values`].
///
/// The anchor is evaluated only when the window actually changed something, so
/// an empty window costs one `eval_delta` and no snapshot read.
pub fn consolidate_window(
    q: &Query,
    store: &dyn TraceStore,
    from: Edition,
    to: Edition,
) -> Result<ConsolidatedWindow> {
    let delta = eval_delta(q, store, from, to)?;
    if delta.values().all(|d| *d == 0) {
        return Ok(ConsolidatedWindow { from, to, rows: Vec::new() });
    }
    // Anchor every *touched* tuple against its weight at `from`, so an
    // in-window retraction of a pre-window assert resolves to zero instead of
    // surfacing as a spurious negative row.
    let anchor = eval_snapshot(q, store, from)?;
    let mut net = Multiset::new();
    for (t, d) in &delta {
        // `eval_delta` already consolidates to a zero-free multiset, so this
        // guard is belt-and-braces: it states the *touched* set (nonzero window
        // delta) rather than relying on that. Without it, a tuple whose window
        // changes cancel would re-enter carrying its anchor weight — world
        // content, not window content.
        if *d == 0 {
            continue;
        }
        let base = anchor.get(t).copied().unwrap_or(0);
        multiset::add(&mut net, t.clone(), base + d);
    }
    // Likewise defensive: `to_sorted_vec` filters zeros too, but the design
    // names this pipeline as fold → strip_zeros → sort, and `multiset::add`
    // leaves a cancelled entry at weight zero rather than removing it.
    multiset::strip_zeros(&mut net);
    Ok(ConsolidatedWindow { from, to, rows: multiset::to_sorted_vec(&net) })
}

impl Query {
    /// The snapshot-anchored contents of this query over the edition window
    /// `(from, to]` — state-mode matching's input (design §4a). See
    /// [`consolidate_window`].
    pub fn consolidate_window(
        &self,
        store: &dyn TraceStore,
        from: Edition,
        to: Edition,
    ) -> Result<ConsolidatedWindow> {
        consolidate_window(self, store, from, to)
    }
}
