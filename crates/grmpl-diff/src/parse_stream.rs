//! The **parse stream** (P9c design §6, §8.4) — incremental match maintenance:
//! a [`DeltaStream`](crate::DeltaStream) *of parses* rather than of tuples.
//!
//! [`Window`] reduced an unbounded, signed delta stream to a finite ordered
//! structure the pattern engine can match (§3), and TKT-114/115 gave the two
//! materializations. This module answers the question those left open: when the
//! window advances and the trace **retracts** something a pattern previously
//! matched, what happens to the *match*?
//!
//! # The lifted Snapshot–stream law
//!
//! Let `M(W)` be the set of parses of a pattern over window `W` in a fixed mode.
//! For a monotonically advancing window sequence `W₀ ⊆ W₁ ⊆ …`, a [`ParseStream`]
//! emits `M(W₀)` and then signed parse deltas, so
//!
//! ```text
//!   M(W₀)  +  Σ deltas  =  M(W_current)
//! ```
//!
//! This is the Snapshot–stream law of `DeltaStream` (`watch.rs`) lifted from
//! tuples to parses, and it is stated over the *same* poll shape: the first
//! [`poll`](ParseStream::poll) reports the whole match-set at the registered
//! window as positive rows without moving the cursor, and each later poll
//! reports only what changed — so no interval is missed.
//!
//! The advancing sequence is built by moving the window's **far end**, never by
//! re-anchoring: `Wᵢ = (from, tᵢ]` with `tᵢ` increasing, which is what makes
//! `W₀ ⊆ W₁ ⊆ …` true of the *edition ranges*. (A sliding family re-anchors, so
//! its windows are not nested and the law above is not stated over it;
//! [`advance_to`](ParseStream::advance_to) therefore refuses to retreat.) Note
//! that nesting the ranges does **not** make the match-sets nested — that is the
//! whole point of what follows.
//!
//! # Why this needs DRed, and how it reuses it
//!
//! A wider window is not a bigger match-set. In state mode the window's contents
//! are its snapshot-anchored *net* rows, so an in-window retraction **removes** a
//! row, and every parse resting on that row loses a derivation. The naive
//! maintenance — "delete each parse that used the retracted row" — is exactly
//! the Delete–Rederive trap: it **over-deletes**, because a parse may have had
//! *other* derivations (another row supporting the same parse, or the same row
//! at multiplicity two) and must survive. Conversely a parse whose last support
//! is gone must genuinely disappear; keeping it would leave a stale parse.
//!
//! That is the same non-monotone problem `recursive.rs` solves for recursive
//! views with DRed (Gupta–Mumick–Subrahmanian): **overdeletion** removes
//! everything that had *a* derivation through a retracted fact, over-approximating,
//! and **regrowth** re-derives from the survivors, repairing the
//! over-approximation. `advance` here is that shape taken to its limit, which is
//! precisely the boundary-recompute strategy the design prescribes (§6):
//!
//! ```text
//!   parse-stream(P, mode) := for each advanced window Wᵢ:
//!                              Mᵢ = run P over Wᵢ            (finite, terminating)
//!                              emit set_difference(Mᵢ, Mᵢ₋₁) (signed parse delta)
//! ```
//!
//! * **Overdeletion** is total: the previous match-set is dropped wholesale
//!   rather than tracked per-derivation, so no parse can survive on stale
//!   support. Parses carry no provenance, so there is nothing finer to key on —
//!   the same reason `recursive.rs` diffs old-vs-new base instead of recording
//!   derivations.
//! * **Regrowth** is a full re-parse of the new window. Because the pattern is
//!   re-run over the window's *current* contents, a parse with surviving support
//!   is re-derived and never leaves the set — the over-deletion is repaired by
//!   construction, which is exactly DRed's second phase.
//! * The emitted delta is [`set_difference`] — literally the function
//!   `IncrementalFixpoint::advance` diffs its fixpoint with, generic in the key
//!   so there is one definition and no second implementation.
//!
//! Termination is inherited, not re-argued: each `Mᵢ` is a run over a *finite*
//! window, so the v1 progress-measure story applies verbatim (§3).
//!
//! # Parses are a set
//!
//! `M(W)` is a **set**: a parse derived two ways is one parse. That is what
//! [`set_difference`] computes (it is the same set reading `as_set` takes of the
//! fixpoint), and it is what makes re-derivation load-bearing — under multiset
//! semantics losing one of two derivations would merely decrement a weight and
//! the trap above would not exist. Emitted weights are therefore `+1` (this
//! parse appeared) and `-1` (this parse is gone), never a multiplicity.
//!
//! # Honest tradeoff
//!
//! Window-recompute is correct and is a composition of pieces the substrate
//! already ships, but it re-parses per advance instead of sharing arrangements
//! across windows. A truly differential incremental parser (DBSP-style
//! incremental matching over sequences) is explicitly out of scope here and is
//! filed as follow-on work (§6, TKT-117).
//!
//! # Bright line
//!
//! This module never names the pattern algebra. A [`WindowParser`] is a caller-
//! supplied function from a window to its parses, so `grmpl-diff` keeps its
//! production dependency on `grmpl-core`'s [`TraceStore`] **trait** alone, and
//! `grmpl-pattern` stays a dev-dependency — the same seam TKT-114/115 hold.
//! Which mode is in play (state or event) is the parser's business, not the
//! maintainer's: the law is stated "in a fixed mode" and holds for either.

use std::collections::HashMap;

use grmpl_core::{Diff, Edition, Error, Result, TraceStore, Value};

use crate::recursive::set_difference;
use crate::window::Window;

/// A parse set: the distinct parses of one window, each at weight `1`.
type ParseSet = HashMap<Value, Diff>;

/// Runs a pattern over one materialized window and returns its parses.
///
/// This is the seam that keeps the algebra out of `grmpl-diff`'s production
/// graph (see the module docs): the maintainer knows only that a window has
/// parses, never how they are matched. An implementation materializes the
/// window — [`Window::consolidate`] for state mode, [`Window::events`] for event
/// mode — and runs a `grmpl_pattern::Form` over the result.
///
/// **Purity is a precondition of the law.** `parses` must be a deterministic
/// function of the window and the trace: same window, same trace, same parses
/// (as a set). The maintainer diffs consecutive runs, so a parser that consulted
/// a clock, a random source, or its own call history would emit deltas that no
/// from-scratch recompute reproduces — the law would fail through the parser,
/// not through the maintenance. Hence `&self`, not `&mut self`.
pub trait WindowParser {
    /// Every parse of this window, in any order. Duplicates are permitted and
    /// collapse: `M(W)` is a set.
    fn parses(&self, window: Window, store: &dyn TraceStore) -> Result<Vec<Value>>;
}

/// Any pure function of a window is a parser — the common case, so a closure
/// needs no wrapper type.
impl<F> WindowParser for F
where
    F: Fn(Window, &dyn TraceStore) -> Result<Vec<Value>>,
{
    fn parses(&self, window: Window, store: &dyn TraceStore) -> Result<Vec<Value>> {
        self(window, store)
    }
}

/// A maintained *stream of parses*: `M(W₀)` followed by signed parse deltas as
/// the window advances (design §6). See the module docs for the lifted
/// Snapshot–stream law and the DRed reading of [`advance_to`].
pub struct ParseStream<'a, P: WindowParser> {
    parser: P,
    store: &'a dyn TraceStore,
    /// The window as of the last delivery: `(from, cursor]`. `from` is fixed;
    /// only the far end advances.
    window: Window,
    /// `M(window)` — the maintained match-set, a set (weights are all `1`).
    matches: ParseSet,
    started: bool,
}

impl<'a, P: WindowParser> ParseStream<'a, P> {
    /// Register a parse stream over the initial window `W₀`. Nothing is parsed
    /// yet: the first [`poll`](Self::poll) reports `M(W₀)`, exactly as
    /// `DeltaStream`'s first poll reports the snapshot at its registration
    /// edition.
    pub fn open(parser: P, store: &'a dyn TraceStore, window: Window) -> ParseStream<'a, P> {
        ParseStream { parser, store, window, matches: ParseSet::new(), started: false }
    }

    /// The window through which this stream has delivered — `W₀` before the
    /// first poll, then `(from, cursor]`.
    pub fn window(&self) -> Window {
        self.window
    }

    /// The parser being maintained. Since it is pure (see [`WindowParser`]),
    /// this is how a caller — or a law oracle — recomputes `M(W)` from scratch
    /// with the *same* grammar the stream is maintaining.
    pub fn parser(&self) -> &P {
        &self.parser
    }

    /// The edition through which this stream has delivered (the window's far
    /// end).
    pub fn edition(&self) -> Edition {
        self.window.to()
    }

    /// The maintained match-set `M(W_current)`, in ascending [`Value`] order.
    ///
    /// Deterministic: the set is sorted on the way out, so no `HashMap`
    /// iteration order reaches a caller. Empty before the first poll.
    pub fn matches(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.matches.keys().cloned().collect();
        out.sort();
        out
    }

    /// Deliver the next batch, advancing the window to the store's current
    /// edition — the [`DeltaStream::poll`](crate::DeltaStream::poll) idiom.
    ///
    /// The first call returns `M(W₀)` as positive rows **without** moving the
    /// cursor (so the registered window really is `W₀` and no interval is
    /// skipped); every later call returns the signed parse delta.
    pub fn poll(&mut self) -> Result<Vec<(Value, Diff)>> {
        self.advance_to(self.store.current())
    }

    /// Deliver the next batch, advancing the window's far end to `to`.
    ///
    /// The first call ignores `to` and reports `M(W₀)` at the registered window;
    /// thereafter the window becomes `(from, to]` and the signed parse delta is
    /// returned. Rows are ascending by [`Value`] and zero-free: `+1` is a parse
    /// that appeared, `-1` one that is gone.
    ///
    /// `to` may not precede the current far end. The lifted law is stated over a
    /// *monotonically advancing* window sequence, and a retreating one is an
    /// ill-formed plan rather than an empty delta ([`Error::Query`]) — the same
    /// reading [`Window::new`] takes of inverted bounds.
    pub fn advance_to(&mut self, to: Edition) -> Result<Vec<(Value, Diff)>> {
        if !self.started {
            self.started = true;
            self.matches = self.run(self.window)?;
            return Ok(sorted(&self.matches));
        }
        if to < self.window.to() {
            return Err(Error::Query(format!(
                "parse stream window cannot retreat: already delivered through edition {}, \
                 asked to advance to {}",
                self.window.to().0,
                to.0
            )));
        }
        if to == self.window.to() {
            return Ok(Vec::new());
        }

        // DRed, boundary-recompute form (§6): drop the previous match-set
        // wholesale (total overdeletion — parses carry no provenance to key a
        // finer one on) and re-derive by re-running the pattern over the new
        // window (regrowth). Re-deriving is what repairs the over-deletion: a
        // parse whose support survives in the window is matched again and never
        // leaves the set, while one whose last support is gone is simply not
        // re-derived. See the module docs.
        let window = Window::new(self.window.from(), to)?;
        let next = self.run(window)?;
        let delta = set_difference(&next, &self.matches);
        self.matches = next;
        self.window = window;
        Ok(sorted(&delta))
    }

    /// `M(w)` — the parser's parses of one window, collapsed to a set.
    fn run(&self, w: Window) -> Result<ParseSet> {
        let mut set = ParseSet::new();
        for parse in self.parser.parses(w, self.store)? {
            set.insert(parse, 1);
        }
        Ok(set)
    }
}

/// A `HashMap`-order-free rendering: ascending by [`Value`], zeros dropped.
fn sorted(m: &ParseSet) -> Vec<(Value, Diff)> {
    let mut v: Vec<(Value, Diff)> =
        m.iter().filter(|(_, d)| **d != 0).map(|(p, d)| (p.clone(), *d)).collect();
    v.sort();
    v
}

impl Window {
    /// Open a [`ParseStream`] over this window as `W₀` — the free-function
    /// spelling read the other way round, mirroring [`Query::watch`].
    ///
    /// [`Query::watch`]: crate::Query::watch
    pub fn parse_stream<'a, P: WindowParser>(
        &self,
        parser: P,
        store: &'a dyn TraceStore,
    ) -> ParseStream<'a, P> {
        ParseStream::open(parser, store, *self)
    }
}
