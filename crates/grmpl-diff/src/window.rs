//! The **windowing layer** (P9c design §3, §5, §8.3) — the reduction from a
//! signed, unbounded delta stream to the finite, ordered structures the v1
//! pattern engine already consumes.
//!
//! A pattern **never** runs over the raw live stream; it runs over a *window*, a
//! finite, edition-bounded slice of the trace (§3). Windowing is precisely that
//! reduction, and it is what recovers termination for free: inside a fixed
//! window the progress measure is the remaining delta count, finite by
//! construction. Liveness (the poll/advance loop) stays outside the algebra.
//!
//! This module owns both halves:
//!
//! * **The window grammar** ([`Window`], [`tumbling`], [`sliding`]) — pure
//!   edition arithmetic over the half-open interval `(from, to]`, and the two
//!   materializations a window admits: an **event slice**
//!   ([`Window::events`], raw [`Update`]s in commit order, event mode §4b) or a
//!   **consolidated tuple-set** ([`Window::consolidate`], state mode §4a).
//! * **State-mode consolidation** ([`consolidate_window`],
//!   [`ConsolidatedWindow`]) — the snapshot-anchored net contents, described
//!   below.
//!
//! State mode is the "match the window's net contents" reading of a window:
//! consolidate first — fold `Σ diff` per tuple, drop zeros, sort — and hand the
//! survivors to the existing slice engine. `+t` then `−t` **cancels before
//! matching**, so the pattern never sees `t`. Retraction is absorbed by
//! *arithmetic, upstream of the algebra*; nothing in `grmpl-pattern` learns
//! about signs, and state mode therefore needs no new `MatchInput` instance at
//! all. Event mode instead *reifies* the sign into the matched value
//! (`grmpl_pattern::{reify_delta, DeltaInput}`) — which is why this module hands
//! back a plain `Vec<Update>` and never names the algebra: the bright line runs
//! between them, and `grmpl-diff` depends only on the [`TraceStore`] **trait**.
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
//! truncated history, inherited for free from `eval_delta`/`eval_snapshot` and
//! (for event mode) from `scan_updates` itself.
//!
//! # What is *not* here
//!
//! Windows are **edition-bounded only** (§5). Session windows (gap-based) and
//! count windows (last N updates) are explicitly **deferred**: a count window
//! needs a counter cursor over commit order, which `Update` does not carry.
//! `iter`, the fixpoint sub-coordinate of `Time`, is not a window axis either —
//! it is internal to `Iterate`.

use grmpl_core::{Diff, Edition, Error, RelId, Result, TraceStore, Tuple, Update, Value};

use crate::multiset::{self, Multiset};
use crate::query::{eval_delta, eval_snapshot, Query};

/// An **edition-bounded window**: the half-open edition interval `(from, to]`,
/// aligned exactly with [`TraceStore::scan_updates`] and [`eval_delta`] — no new
/// primitive and no new ordering (design §5).
///
/// A window is a pure *value*: constructing one touches no store. It is the
/// bound that makes matching legal at all (§3), and it materializes two ways:
///
/// * [`events`](Self::events) — the **event slice**: raw [`Update`]s of one
///   relation in commit order `(edition, counter)`, ready for
///   `grmpl_pattern::DeltaInput` (event mode, §4b).
/// * [`consolidate`](Self::consolidate) — the **consolidated tuple-set**: the
///   snapshot-anchored net contents of a query, ready for the `&[Value]` slice
///   engine (state mode, §4a).
///
/// The two are *not* interchangeable readings of the same bytes: the event slice
/// is what the window *did*, in order, signs included; the consolidated set is
/// what the window *left*, anchored against the world at `from`. They agree
/// exactly on the degenerate snapshot window `(Edition::ZERO, E]`, where the
/// anchor is the empty world — see [`consolidate_events`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Window {
    from: Edition,
    to: Edition,
}

impl Window {
    /// The window `(from, to]`. `from` must not exceed `to` — an inverted
    /// interval is an ill-formed plan, not an empty one ([`Error::Query`]).
    pub fn new(from: Edition, to: Edition) -> Result<Window> {
        if from > to {
            return Err(Error::Query(format!(
                "window bounds inverted: from {} is after to {}",
                from.0, to.0
            )));
        }
        Ok(Window { from, to })
    }

    /// The exclusive lower bound (the anchor edition).
    pub fn from(&self) -> Edition {
        self.from
    }

    /// The inclusive upper bound.
    pub fn to(&self) -> Edition {
        self.to
    }

    /// How many editions the window spans (`to − from`); zero for an empty
    /// window. This is the count of *editions in scope*, not of updates — a
    /// single edition may carry any number of updates.
    pub fn span(&self) -> u64 {
        self.to.0 - self.from.0
    }

    /// The window covers no edition (`from == to`), so every materialization of
    /// it is empty.
    pub fn is_empty(&self) -> bool {
        self.from == self.to
    }

    /// Is edition `e` in scope? `from < e ≤ to` — exclusive at the anchor,
    /// inclusive at the far end, exactly as `scan_updates` reads it. This is the
    /// membership predicate the tumbling-partition and sliding-coverage laws are
    /// stated over.
    pub fn contains(&self, e: Edition) -> bool {
        self.from < e && e <= self.to
    }

    /// **Event-mode materialization** (§4b): `rel`'s raw updates whose edition
    /// lies in the window, in **commit order** `(edition, counter)` — the exact
    /// order they were written, guaranteed by the [`TraceStore::scan_updates`]
    /// contract, so no scan order leaks into a match. Feed it to
    /// `grmpl_pattern::DeltaInput`, which reifies each update's sign into the
    /// matched value.
    ///
    /// Event mode is **per base relation** by construction: only raw updates
    /// carry the per-edition commit order a log-structured match needs, and a
    /// derived `Query`'s delta is a consolidated multiset with no order at all.
    pub fn events(&self, store: &dyn TraceStore, rel: RelId) -> Result<Vec<Update>> {
        store.scan_updates(rel, self.from, self.to)
    }

    /// **State-mode materialization** (§4a): the snapshot-anchored net contents
    /// of `q` over this window. See [`consolidate_window`].
    pub fn consolidate(&self, q: &Query, store: &dyn TraceStore) -> Result<ConsolidatedWindow> {
        consolidate_window(q, store, self.from, self.to)
    }
}

/// The window `(from, to]` — the free-function spelling of [`Window::new`],
/// which is how the design writes it (`window(from, to)`, §8.3).
pub fn window(from: Edition, to: Edition) -> Result<Window> {
    Window::new(from, to)
}

/// **Tumbling** windows of `size` editions covering `(from, to]`: the disjoint
/// consecutive sequence `(from, from+size]`, `(from+size, from+2·size]`, … with
/// the last window clamped to `to` (design §5).
///
/// Every edition in `(from, to]` lies in **exactly one** window — no loss, no
/// duplication, no overlap — so the windows partition the range and the union of
/// their contents is the whole range's contents. An empty range yields no
/// windows; `size` must be positive.
pub fn tumbling(from: Edition, to: Edition, size: u64) -> Result<Vec<Window>> {
    sliding(from, to, size, size)
}

/// **Sliding** windows of `size` editions advancing by `step`, covering
/// `(from, to]`: window *k* is `(from + k·step, from + k·step + size]`, clamped
/// at `to`, for every *k* whose anchor is still below `to` (design §5).
///
/// `step` must be positive and **must not exceed `size`**. `step < size` is the
/// sliding case proper (consecutive windows overlap by `size − step` editions);
/// `step == size` degenerates to [`tumbling`]. A `step > size` would leave
/// editions in no window at all — a gap grammar, which is not one of the shapes
/// §5 admits — so it is rejected as an ill-formed plan rather than silently
/// dropping updates.
///
/// Because the tail windows are clamped, an update at edition `e` is in window
/// *k* exactly when `from + k·step < e ≤ from + k·step + size`: clamping never
/// changes membership, only where a window stops.
pub fn sliding(from: Edition, to: Edition, size: u64, step: u64) -> Result<Vec<Window>> {
    if size == 0 {
        return Err(Error::Query("window size must be positive".into()));
    }
    if step == 0 {
        return Err(Error::Query("window step must be positive".into()));
    }
    if step > size {
        return Err(Error::Query(format!(
            "sliding step {step} exceeds window size {size}: editions between windows would \
             fall in no window (gap windows are not an edition-bounded window shape)"
        )));
    }
    if from > to {
        return Err(Error::Query(format!(
            "window bounds inverted: from {} is after to {}",
            from.0, to.0
        )));
    }
    let mut out = Vec::new();
    let mut anchor = from.0;
    while anchor < to.0 {
        // Saturating so a size or step near `u64::MAX` clamps to `to` instead of
        // wrapping into an inverted window.
        let end = anchor.saturating_add(size).min(to.0);
        out.push(Window { from: Edition(anchor), to: Edition(end) });
        match anchor.checked_add(step) {
            Some(next) => anchor = next,
            None => break,
        }
    }
    Ok(out)
}

/// Fold an **event slice** into a consolidated, tuple-sorted, zero-free
/// `Vec<(Tuple, Diff)>`: the window's own deltas summed per tuple.
///
/// This is the *unanchored* fold — "what the window did", the arithmetic §4a
/// warns about taking for the window's contents. It equals the state-mode
/// [`consolidate_window`] only where the anchor contributes nothing: on the
/// snapshot window `(Edition::ZERO, E]`, or for tuples the world did not already
/// hold at `from`. In general
///
/// ```text
///   consolidate_window(rel, from, to)  =  strip_zeros( snapshot(rel, from) + consolidate_events(events) )
/// ```
///
/// restricted to the touched tuples — which is exactly why state-mode windows
/// are snapshot-anchored and this function is *not* the state-mode reading. It
/// exists so the relation between the two materializations is expressible (and
/// testable) rather than folklore.
pub fn consolidate_events(updates: &[Update]) -> Vec<(Tuple, Diff)> {
    let m = multiset::from_pairs(updates.iter().map(|u| (u.tuple.clone(), u.diff)));
    multiset::to_sorted_vec(&m)
}

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
