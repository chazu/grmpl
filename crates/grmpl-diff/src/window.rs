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
//! * **The window grammar** ([`Window`], [`tumbling`], [`sliding`], [`sessions`],
//!   [`CountWindow`]) — pure edition arithmetic over the half-open interval
//!   `(from, to]`, and the two materializations a window admits: an **event
//!   slice** ([`Window::events`], raw [`Update`]s in commit order, event mode
//!   §4b) or a **consolidated tuple-set** ([`Window::consolidate`], state mode
//!   §4a).
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
//! # The rest of the §5 grammar (TKT-117)
//!
//! §5 tabulates five window shapes and defers two of them. Both now exist, and
//! the reason they were deferred is exactly the difference between them:
//!
//! * [`sessions`] — **gap-based**. Still edition-bounded, so it is pure edition
//!   arithmetic like [`tumbling`]/[`sliding`]: partition the editions that
//!   actually carry updates wherever the inter-event gap exceeds a timeout, and
//!   hand back one tight `(first−1, last]` window per session. Because each
//!   session *is* an edition interval, both materializations work on it
//!   unchanged.
//! * [`CountWindow`] — **last N updates**. This one is not an edition shape at
//!   all: it needs "a counter cursor over commit order", and a boundary that
//!   falls *inside* an edition has no snapshot to anchor against. It is
//!   therefore an **event-mode** window with its own maintained type rather than
//!   a `Vec<Window>`; see its docs.
//!
//! `iter`, the fixpoint sub-coordinate of `Time`, is not a window axis — it is
//! internal to `Iterate`.

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

/// **Session** windows (design §5, deferred there; TKT-117): split a sequence of
/// **active editions** — the editions that actually carry updates — wherever the
/// gap between consecutive ones exceeds `timeout`, and return one tight window
/// per session.
///
/// Session *k* is the window `(first − 1, last]`, where `first`/`last` are its
/// own extreme active editions. That is the narrowest edition interval containing
/// exactly the session's activity: `from` sits one below `first` because a window
/// is exclusive at its anchor, so `first` is the earliest edition in scope.
///
/// The law this satisfies, in both directions — under-splitting and
/// over-splitting are both errors:
///
/// * **Within a session**, every consecutive gap is `≤ timeout`.
/// * **At every boundary** between consecutive sessions, the gap is `> timeout`.
///
/// Together those make the split unique: it is the coarsest partition with no
/// over-long internal gap. Sessions are consequently disjoint and separated by at
/// least one edition — a boundary gap exceeds `timeout ≥ 1`, so the next
/// session's anchor `first − 1` is strictly above the previous session's `last`.
///
/// `actives` must be **strictly ascending** (the order `scan_updates` delivers,
/// deduplicated) and must not contain [`Edition::ZERO`], which is the empty world
/// before any commit and therefore carries no update. `timeout` must be positive:
/// consecutive editions are at least one apart, so a zero timeout would make
/// every event its own session, which is a `tumbling` of size 1 wearing a
/// disguise rather than a gap grammar. Ill-formed input is [`Error::Query`], the
/// same reading [`sliding`] takes of a gap-producing step.
pub fn sessions(actives: &[Edition], timeout: u64) -> Result<Vec<Window>> {
    if timeout == 0 {
        return Err(Error::Query(
            "session timeout must be positive: consecutive editions are at least one apart, so a \
             zero timeout closes a session at every event"
                .into(),
        ));
    }
    let mut out = Vec::new();
    let mut run: Option<(Edition, Edition)> = None; // (first, last) of the open session
    for (i, e) in actives.iter().copied().enumerate() {
        if e == Edition::ZERO {
            return Err(Error::Query(
                "session windows: edition 0 is the empty world and carries no update".into(),
            ));
        }
        match run {
            None => run = Some((e, e)),
            Some((first, last)) => {
                if e <= last {
                    return Err(Error::Query(format!(
                        "session windows: active editions must be strictly ascending, but item \
                         {i} is {} after {}",
                        e.0, last.0
                    )));
                }
                if e.0 - last.0 <= timeout {
                    run = Some((first, e)); // same session: the gap is within timeout
                } else {
                    out.push(Window { from: Edition(first.0 - 1), to: last });
                    run = Some((e, e)); // the gap closed it
                }
            }
        }
    }
    if let Some((first, last)) = run {
        out.push(Window { from: Edition(first.0 - 1), to: last });
    }
    Ok(out)
}

impl Window {
    /// The **session windows** of `rel`'s activity inside this window (§5,
    /// TKT-117): scan the event slice, take the distinct editions it touches, and
    /// split them on gaps exceeding `timeout`. See [`sessions`].
    ///
    /// Sessions are cut from *observed activity*, so this is per base relation
    /// for the same reason event mode is (TKT-140): idleness is a property of a
    /// relation's own commit history, and only raw updates carry it.
    pub fn sessions(
        &self,
        store: &dyn TraceStore,
        rel: RelId,
        timeout: u64,
    ) -> Result<Vec<Window>> {
        let mut actives: Vec<Edition> =
            self.events(store, rel)?.iter().map(|u| Edition(u.time.edition)).collect();
        // `scan_updates` delivers commit order, so equal editions are adjacent
        // and `dedup` yields the strictly ascending sequence `sessions` wants.
        actives.dedup();
        sessions(&actives, timeout)
    }
}

/// What one [`CountWindow::advance_to`] did to the window: the updates that
/// arrived, and the ones displaced off the oldest end, both in arrival order.
///
/// This is the count window's delta, and it is complete — but the accounting is
/// over the *concatenation*, not over the previous contents alone:
///
/// ```text
///   contents_after  =  (contents_before ++ admitted)  with the first |evicted| dropped
///   |evicted|       =  max(0, |contents_before| + |admitted| − n)
/// ```
///
/// The distinction is real rather than pedantic. When a single advance brings
/// more than `n` updates, the surplus taken off the oldest end reaches **into the
/// admissions**: an update that arrived and was immediately displaced by a later
/// one in the same advance appears in *both* lists. That is the honest report —
/// it did pass through the window's commit order — and a consumer that treats
/// `admitted` as "now present" must apply `evicted` to it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CountDelta {
    /// Updates that arrived, in commit order.
    pub admitted: Vec<Update>,
    /// Updates displaced off the oldest end, oldest first — possibly including
    /// some of `admitted`, when the advance brought more than `n`.
    pub evicted: Vec<Update>,
}

/// A **count window** (design §5, deferred there; TKT-117): the last `n` updates
/// of one relation in arrival order, maintained as the trace advances.
///
/// # Why this is not a `Window`
///
/// Every other shape in §5 is an edition interval, and the design says why this
/// one cannot be: it "needs a counter cursor over commit order". An edition may
/// carry any number of updates, so the boundary of "the last `n`" generally falls
/// *inside* an edition — there is no `from` such that `(from, to]` holds exactly
/// `n` updates. That is not a missing convenience; it is the reason a count
/// window is **event mode only**:
///
/// * [`events`](Self::events) hands back the maintained `&[Update]` slice in
///   commit order, ready for `grmpl_pattern::DeltaInput` exactly as
///   [`Window::events`] is.
/// * There is deliberately **no** state-mode `consolidate`. State mode is
///   snapshot-*anchored* (see the module docs), and a boundary inside an edition
///   has no edition to anchor at, so the anchoring invariant TKT-114 pinned
///   cannot be stated for it. [`consolidate_events`] over
///   [`events`](Self::events) is available for the unanchored fold, with the
///   usual caveat that it is "what the window did", not "what it left".
///
/// # Maintenance
///
/// [`open`](Self::open) seeds the window from history — one scan up to the
/// opening edition, the analogue of a parse stream's anchor snapshot — and each
/// [`advance_to`](Self::advance_to) appends the new segment and evicts from the
/// front. The invariant, re-established after every advance, is simply:
///
/// ```text
///   contents  ==  the last min(n, updates seen) updates, in arrival order
/// ```
///
/// Eviction is therefore exactly one-for-one once the window is full: `k`
/// arrivals evict `k` of the oldest, so nothing stale is retained and nothing
/// live is dropped.
pub struct CountWindow {
    rel: RelId,
    n: usize,
    buf: Vec<Update>,
    at: Edition,
    seen: u64,
}

impl CountWindow {
    /// Open a window of the last `n` updates of `rel` as of edition `at`, seeded
    /// from history.
    ///
    /// The seed is a single scan of `(⊥, at]` — the one-time cost of knowing what
    /// "the last `n`" means at all, paid once rather than per advance. Open at
    /// [`Edition::ZERO`] for an empty start that only ever sees what arrives
    /// after. `n` must be positive.
    pub fn open(store: &dyn TraceStore, rel: RelId, n: usize, at: Edition) -> Result<CountWindow> {
        if n == 0 {
            return Err(Error::Query("count window size must be positive".into()));
        }
        let all = store.scan_updates(rel, Edition::ZERO, at)?;
        let seen = all.len() as u64;
        let buf = last_n(&all, n).to_vec();
        Ok(CountWindow { rel, n, buf, at, seen })
    }

    /// The relation whose commit order this window counts.
    pub fn rel(&self) -> RelId {
        self.rel
    }

    /// The window's capacity `n` — the most updates it will ever hold.
    pub fn capacity(&self) -> usize {
        self.n
    }

    /// The edition through which this window has consumed.
    pub fn at(&self) -> Edition {
        self.at
    }

    /// How many updates have passed through since [`open`](Self::open) seeded it
    /// — including the seed. `len() == min(capacity(), seen())` always.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// The window's contents: the last `min(n, seen)` updates in arrival order,
    /// ready for `grmpl_pattern::DeltaInput`.
    pub fn events(&self) -> &[Update] {
        &self.buf
    }

    /// How many updates the window currently holds.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// The window holds nothing — `n` updates have not yet been seen at all.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The edition interval the window's contents currently span, as the tight
    /// `(first − 1, last]` — for reporting only.
    ///
    /// It is **not** a window the contents can be recovered from: the first
    /// edition in scope may be only *partly* in the count window (its earlier
    /// updates already evicted), which is precisely why a count window is not a
    /// [`Window`]. `None` when empty.
    pub fn span(&self) -> Option<Window> {
        let first = self.buf.first()?.time.edition;
        let last = self.buf.last()?.time.edition;
        Some(Window { from: Edition(first.saturating_sub(1)), to: Edition(last) })
    }

    /// Consume `rel`'s updates over `(at, to]`, admitting each in commit order
    /// and evicting from the front to keep the window at `n`.
    ///
    /// `to` may not precede the current position — a count window advances with
    /// the trace, and a retreat is an ill-formed plan rather than an empty delta
    /// ([`Error::Query`]), the same reading every other advancing cursor in the
    /// layer takes.
    pub fn advance_to(&mut self, store: &dyn TraceStore, to: Edition) -> Result<CountDelta> {
        if to < self.at {
            return Err(Error::Query(format!(
                "count window cannot retreat: already consumed through edition {}, asked to \
                 advance to {}",
                self.at.0, to.0
            )));
        }
        let admitted = store.scan_updates(self.rel, self.at, to)?;
        self.at = to;
        if admitted.is_empty() {
            return Ok(CountDelta::default());
        }
        self.seen += admitted.len() as u64;
        self.buf.extend(admitted.iter().cloned());
        // One eviction per arrival once full: drop the oldest surplus, oldest
        // first, so what remains is the last `n` in arrival order.
        let surplus = self.buf.len().saturating_sub(self.n);
        let evicted = self.buf.drain(..surplus).collect();
        Ok(CountDelta { admitted, evicted })
    }
}

/// The last `n` items of a slice — `min(n, len)` of them, in the slice's own
/// order. The pure kernel of a [`CountWindow`], separated so the eviction law is
/// stated over arithmetic rather than over a maintained buffer.
pub fn last_n<T>(items: &[T], n: usize) -> &[T] {
    &items[items.len().saturating_sub(n)..]
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
