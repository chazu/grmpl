//! **Differential match maintenance** (P9c design §6 "honest tradeoff", §8.5 #5;
//! TKT-117) — the deferred half of the parse stream: matching maintained by
//! *shared arrangements* instead of a full re-parse per advance.
//!
//! [`ParseStream`](crate::ParseStream) (TKT-116) established the lifted
//! Snapshot–stream law and satisfied it the cheap way — **window-recompute +
//! `set_difference`**: every advance re-materializes the window from its anchor
//! and re-runs the pattern over the whole thing. Correct, but the design flagged
//! the cost honestly: *"it recomputes per advance rather than sharing
//! arrangements. A truly differential incremental parser (DBSP-style incremental
//! matching over sequences) would be more efficient and is a real research
//! problem."* This module is that parser.
//!
//! [`DiffParseStream`] emits **exactly** the delta sequence `ParseStream` emits —
//! that equivalence is the correctness law (`tests/differential_parse_law.rs`),
//! and it is what makes this a drop-in acceleration rather than a second
//! semantics. What changes is only *how much work* an advance costs.
//!
//! # The two recomputes, and what replaces each
//!
//! An advance of the recompute maintainer does two expensive things.
//!
//! **1. It rebuilds the window's sequence from the anchor.** State mode's
//! [`consolidate_window`](crate::consolidate_window) costs
//! `eval_delta(q, from, to)` *plus* `eval_snapshot(q, from)` on **every** advance
//! — work linear in the whole window, re-done from scratch each time.
//! [`WindowSequence`] replaces it with genuine maintenance: the anchor snapshot
//! at `from` is evaluated **once** (through the existing
//! [`Arrangements`] memo, so shared sub-DAGs of the plan are shared with it), and
//! each advance folds in only the *new* segment `eval_delta(q, prev_to, to)`.
//! Per-advance cost drops from "the window" to "what changed".
//!
//! **2. It re-parses every anchor position.** That is the part the design calls
//! the real research problem, and [`MatchArrangement`] is the answer.
//!
//! # The match arrangement
//!
//! An *arrangement*, in the sense `grmpl-diff` already uses for queries
//! ([`Arrangements`], `DESIGN.md` §3.2), is a sub-computation **evaluated once
//! and referenced many times**. The query arrangement keys a sub-DAG's value by
//! `(node identity, edition)`. The match arrangement keys an **anchor's
//! parse-set** by the *content the parser actually read to produce it*:
//!
//! ```text
//!   (consumed span, ends-here?)  →  parses
//! ```
//!
//! [`SequenceParser::parses_at`] returns, beside the parses at an anchor, the
//! **reach**: one past the furthest item it inspected. Its contract (stated on
//! the trait, and *verified* by the law oracle rather than trusted) is that the
//! parse-set is a function of `seq[anchor..reach]` alone — plus, when
//! `reach == seq.len()`, of the fact that the sequence ends there. Two anchors
//! whose consumed spans agree therefore have the same parses, whether they are
//! two positions in one sequence or the same position across two advances. The
//! arrangement makes that identity payable: a hit costs a hash lookup, not a
//! parse.
//!
//! Two things fall out, and they are exactly the two window shapes:
//!
//! * **Appending** (event mode, and the tail of any window): an anchor whose
//!   parse stopped short of the end has an unchanged key, so it is **never
//!   re-parsed**. Only anchors that ran to the old end — a grammar-bounded
//!   handful — are recomputed. This is the incremental-lexing invariant, and it
//!   is why a growing event log costs `O(lookahead)` per advance instead of
//!   `O(window)`.
//! * **Local change** (state mode, whose sequence is tuple-sorted): an update
//!   perturbs the sorted order at one place; every anchor whose consumed span
//!   avoids it hits the arrangement.
//!
//! [`DiffParseStream::last_parse_calls`] against
//! [`last_anchors`](DiffParseStream::last_anchors) is the instrumentation that
//! makes the claim falsifiable — the oracle asserts the sharing is real, not just
//! architecturally available.
//!
//! # Why the reuse is sound
//!
//! A parser is a deterministic function of its cursor (the same purity
//! precondition [`WindowParser`](crate::WindowParser) already carries). If it
//! produced `P` after reading `seq[a..r]` and **never looked past `r`**, then at
//! any anchor `a'` in any sequence with `seq'[a'..a'+(r−a)] == seq[a..r]` it makes
//! the identical sequence of reads and produces the identical `P` — what follows
//! is unobserved, so it cannot matter. When it *did* read to the end (`r == len`)
//! the end itself was observed, so "ends here" joins the key and a reuse is only
//! offered at an anchor that also ends there.
//!
//! At most one arrangement entry can ever apply at a given anchor, so the probe
//! order is immaterial: two entries with different span lengths that both matched
//! would require the parser to read `r₁` items from one input and `r₂ ≠ r₁` items
//! from another whose content agrees over the overlap and whose end-status
//! matches — impossible for a deterministic reader.
//!
//! # Stable identity (the P13b ABA hazard, closed)
//!
//! The query arrangement is keyed by `(Arc::as_ptr(node), edition)`, which is why
//! it is only ever built **fresh per top-level evaluation**: a recycled node
//! address gives a stale same-edition hit (`grmpl-bench`'s smoke test reproduces
//! it). A match arrangement is meant to *outlive* an advance — that is its whole
//! point — so a transient pointer key is not available to it. It is keyed by
//! **content**: the consumed span itself, compared by value. Nothing is hashed
//! down to a digest, so there is no collision to reason about either; the key is
//! stable across advances, across streams, and across process lifetimes by
//! construction. A [`MatchArrangement`] is consequently safe to share between
//! several streams over the same grammar — the [`Arrangements`] idiom
//! (`eval_snapshot_with`) one level up the tower.
//!
//! # Bright line
//!
//! Unchanged from TKT-116. This module never names the pattern algebra: a
//! [`SequenceParser`] is caller-supplied, so `grmpl-diff` keeps its production
//! dependency on `grmpl-core`'s [`TraceStore`] **trait** alone and
//! `grmpl-pattern` stays a dev-dependency. Nothing here is serialized — the
//! arrangement is in-memory, exactly as the reification is — so there is **no
//! `FORMAT_VERSION` bump**.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use grmpl_core::{Diff, Edition, Error, RelId, Result, TraceStore, Update, Value};

use crate::multiset::{self, Multiset};
use crate::query::{eval_delta, eval_snapshot_with, Arrangements, Query};
use crate::recursive::set_difference;
use crate::window::Window;

// ---------------------------------------------------------------------------
// The parser seam
// ---------------------------------------------------------------------------

/// What a [`SequenceParser`] reports at one anchor: the parses, and how far it
/// had to look to find them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AnchoredParses {
    /// Every parse anchored at this position, in any order. Duplicates collapse:
    /// a match-set is a set (the reading TKT-116 fixed).
    pub parses: Vec<Value>,
    /// One past the furthest item inspected — the **reach**. Must satisfy
    /// `anchor ≤ reach ≤ seq.len()`; see the contract on
    /// [`SequenceParser::parses_at`].
    pub reach: usize,
}

/// Runs a grammar at **one anchor** of a materialized sequence, reporting how
/// much of it was read.
///
/// This is the seam that keeps the algebra out of `grmpl-diff`'s production graph
/// (module docs), refined from [`WindowParser`](crate::WindowParser) so the
/// maintainer can share work: a `WindowParser` answers "all parses of this
/// window", which is atomic and therefore recomputable only in whole, while a
/// `SequenceParser` answers per anchor and says what it read.
///
/// # Contract
///
/// Two obligations, both load-bearing, both asserted by the law oracle rather
/// than taken on trust:
///
/// 1. **Purity.** `parses_at` is a deterministic function of `(seq, anchor)`.
///    (The same precondition [`WindowParser`](crate::WindowParser) carries: the
///    maintainer diffs consecutive states, so an impure parser breaks the law
///    through the parser.)
/// 2. **Reach soundness.** `anchor ≤ reach ≤ seq.len()`, and the returned
///    parse-set depends only on `seq[anchor..reach]` — together with the fact
///    that the sequence *ends* at `reach`, when `reach == seq.len()`. Concretely:
///    if `reach < seq.len()`, appending or altering anything at or beyond `reach`
///    must not change the result.
///
/// Reporting `reach == seq.len()` is **always sound** and is the correct fallback
/// for a grammar whose lookahead an implementation cannot bound (one that
/// consults [`MatchInput::measure`] over the whole tail, say). It simply forgoes
/// sharing at that anchor; it never makes the answer wrong.
///
/// [`MatchInput::measure`]: https://docs.rs/grmpl-pattern
pub trait SequenceParser<T> {
    /// Every parse anchored at `anchor`, and the reach.
    fn parses_at(&self, seq: &[T], anchor: usize) -> Result<AnchoredParses>;
}

// ---------------------------------------------------------------------------
// The match arrangement
// ---------------------------------------------------------------------------

/// A **shared match arrangement**: `(consumed span, ends-here?) → parses`.
///
/// The matching analogue of [`Arrangements`] — a sub-computation evaluated once
/// and referenced many times — keyed by **content** rather than by a transient
/// pointer, so it is safe to outlive an advance and to be shared between streams
/// (see the module docs on the P13b ABA hazard).
///
/// Entries are pure facts about the grammar, not about any one trace, so an
/// arrangement never needs invalidating: a span that parsed to `P` yesterday
/// parses to `P` today. It only ever grows, which is the same memory-for-compute
/// trade `grmpl-bench`'s arrangement axis measures; clear it, or scope one per
/// stream, if the bound matters.
///
/// # Cost, honestly
///
/// A lookup must find the span *before* it knows how long the span is, so it
/// probes the span lengths the arrangement has already recorded. That is cheap
/// for a grammar with bounded lookahead — a handful of lengths, each a short
/// slice hash — and it degrades for one whose consumed span varies widely
/// (a `Repeat` that eats an arbitrary prefix records a new length each time it
/// eats a different amount). In the limit the probe costs one hash per distinct
/// length; it never costs a parse, and it never affects the answer.
pub type MatchArrangement<T> = HashMap<(Vec<T>, bool), Arc<[Value]>>;

/// Look for an entry that applies at `anchor`. At most one can (module docs), so
/// the probe order is immaterial; ascending span length is simply the cheapest.
fn probe<T: Clone + Eq + std::hash::Hash>(
    seq: &[T],
    anchor: usize,
    arr: &MatchArrangement<T>,
    spans: &BTreeSet<usize>,
) -> Option<Arc<[Value]>> {
    let len = seq.len();
    for &l in spans {
        if anchor + l > len {
            break; // ascending: no longer span can fit either
        }
        if let Some(hit) = arr.get(&(seq[anchor..anchor + l].to_vec(), anchor + l == len)) {
            return Some(hit.clone());
        }
    }
    None
}

/// A parse-set as the arrangement stores it: ascending and deduplicated, so the
/// stored value is a deterministic function of the span and no `HashMap` order
/// can reach a caller through it.
fn as_set(mut parses: Vec<Value>) -> Arc<[Value]> {
    parses.sort();
    parses.dedup();
    Arc::from(parses)
}

// ---------------------------------------------------------------------------
// The maintained sequence
// ---------------------------------------------------------------------------

/// The window's materialized sequence, **maintained** across advances rather
/// than rebuilt from the anchor.
///
/// This is the first of the two recomputes the module replaces. An implementation
/// keeps whatever state lets it answer `(from, to]` given that it already
/// answers `(from, prev_to]` — for [`EventSequence`] that is nothing at all (the
/// log only grows); for [`StateSequence`] it is a cached anchor snapshot and the
/// running window delta.
pub trait WindowSequence {
    /// The sequence's item type. It keys the [`MatchArrangement`], so it is
    /// compared by value — the stable identity the module docs require.
    type Item: Clone + Eq + std::hash::Hash;

    /// Bring the sequence up to `(from, to]`, given that it currently covers
    /// `(from, prev_to]`. Called with `prev_to == from` for the opening window,
    /// and thereafter with strictly advancing `to`. `from` never moves.
    fn advance(
        &mut self,
        store: &dyn TraceStore,
        from: Edition,
        prev_to: Edition,
        to: Edition,
    ) -> Result<()>;

    /// The sequence as it stands.
    fn items(&self) -> &[Self::Item];
}

/// **State mode** (§4a), maintained: the window's snapshot-anchored net contents
/// as the flat `&[Value]` sequence the v1 slice engine matches — the same value
/// [`ConsolidatedWindow::values`](crate::ConsolidatedWindow::values) computes,
/// reached without re-reading the window each advance.
///
/// The anchoring invariant is unchanged from TKT-114, and is what this has to
/// reproduce exactly: the contents are the tuples with a **nonzero net delta over
/// the whole window**, each at its true weight at `to`. Maintaining it needs two
/// pieces of state and no re-read:
///
/// * `anchor` — `snapshot(q, from)`, evaluated **once** (an advance never touches
///   it again), through a persistent [`Arrangements`] memo so a plan's shared
///   sub-DAGs are shared into it.
/// * `cum` — the running `Σ delta` per tuple over `(from, to]`, folded forward one
///   segment at a time.
///
/// Then `contents = { t : cum[t] ≠ 0, anchor[t] + cum[t] > 0 }`. Note that the
/// `cum[t] ≠ 0` test is not cosmetic: a tuple whose window changes **cancel** is
/// not touched by the window at all and must be absent even though its anchor
/// weight is positive — dropping that test would leak world content into window
/// content and diverge from the recompute path.
pub struct StateSequence {
    q: Query,
    anchor: Option<Multiset>,
    cum: Multiset,
    items: Vec<Value>,
    arr: Arrangements,
}

impl StateSequence {
    /// Maintain `q`'s state-mode contents.
    pub fn new(q: Query) -> StateSequence {
        StateSequence {
            q,
            anchor: None,
            cum: Multiset::new(),
            items: Vec::new(),
            arr: Arrangements::new(),
        }
    }

    /// The consolidated rows behind [`items`](WindowSequence::items):
    /// tuple-sorted, zero-free, each at its weight at the window's far end.
    pub fn rows(&self) -> Vec<(grmpl_core::Tuple, Diff)> {
        let anchor = match &self.anchor {
            Some(a) => a,
            None => return Vec::new(),
        };
        let mut net = Multiset::new();
        for (t, c) in &self.cum {
            if *c == 0 {
                continue;
            }
            multiset::add(&mut net, t.clone(), anchor.get(t).copied().unwrap_or(0) + c);
        }
        multiset::strip_zeros(&mut net);
        multiset::to_sorted_vec(&net)
    }
}

impl WindowSequence for StateSequence {
    type Item = Value;

    fn advance(
        &mut self,
        store: &dyn TraceStore,
        from: Edition,
        prev_to: Edition,
        to: Edition,
    ) -> Result<()> {
        if self.anchor.is_none() {
            // The one snapshot read of the stream's life. Everything after this
            // is segment deltas.
            self.anchor = Some(eval_snapshot_with(&self.q, store, from, &mut self.arr)?);
        }
        // Only the new segment — never `(from, to]` again.
        for (t, w) in &eval_delta(&self.q, store, prev_to, to)? {
            multiset::add(&mut self.cum, t.clone(), *w);
        }
        // A tuple whose cumulative window delta has cancelled back to zero is
        // *untouched* by the window, which is indistinguishable from never having
        // appeared — both are excluded, and a later segment's `d` rebuilds
        // `0 + d` either way. Dropping it is therefore semantics-preserving, and
        // it is what keeps `cum` bounded by the *live* touched set rather than by
        // every tuple the window has ever seen.
        multiset::strip_zeros(&mut self.cum);
        self.items = self
            .rows()
            .into_iter()
            .flat_map(|(t, d)| {
                // A row's multiplicity is how many times it appears in the
                // sequence; a non-positive weight (a `Negate`, or a trace that
                // retracts what it never asserted) contributes nothing, since
                // "present −1 times" is not a sequence the engine can match.
                std::iter::repeat_n(Value::Tuple(t.0.clone()), d.max(0) as usize)
            })
            .collect();
        Ok(())
    }

    fn items(&self) -> &[Value] {
        &self.items
    }
}

/// **Event mode** (§4b), maintained: the window's raw signed updates in commit
/// order — the sequence [`Window::events`] materializes, grown by appending each
/// advance's segment instead of re-scanning the window.
///
/// This is the append-only case the match arrangement handles best: history is
/// immutable, so advancing the far end only ever adds to the right, and every
/// anchor whose parse stopped short of the old end keeps its key. Event mode is
/// **per base relation** by construction (TKT-140): only raw updates carry the
/// per-edition commit order a log-structured match needs.
pub struct EventSequence {
    rel: RelId,
    items: Vec<Update>,
}

impl EventSequence {
    /// Maintain `rel`'s event slice.
    pub fn new(rel: RelId) -> EventSequence {
        EventSequence { rel, items: Vec::new() }
    }
}

impl WindowSequence for EventSequence {
    type Item = Update;

    fn advance(
        &mut self,
        store: &dyn TraceStore,
        _from: Edition,
        prev_to: Edition,
        to: Edition,
    ) -> Result<()> {
        self.items.extend(store.scan_updates(self.rel, prev_to, to)?);
        Ok(())
    }

    fn items(&self) -> &[Update] {
        &self.items
    }
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

/// A parse set: the distinct parses of one window, each at weight `1`.
type ParseSet = HashMap<Value, Diff>;

/// A **differentially maintained** stream of parses: the same lifted
/// Snapshot–stream law as [`ParseStream`](crate::ParseStream), reached by shared
/// arrangements rather than by re-parsing the window.
///
/// ```text
///   M(W₀)  +  Σ deltas  =  M(W_current)
/// ```
///
/// Emitted deltas are ascending by [`Value`], zero-free, and weighted `±1`;
/// [`advance_to`](Self::advance_to) refuses to retreat, because the law is stated
/// over a monotonically advancing window sequence built by moving the far end
/// only. All of that is deliberately identical to the recompute maintainer — the
/// acceptance law is that the two agree step for step.
pub struct DiffParseStream<'a, S: WindowSequence, P: SequenceParser<S::Item>> {
    seq: S,
    parser: P,
    store: &'a dyn TraceStore,
    window: Window,
    arrangement: MatchArrangement<S::Item>,
    /// The span lengths present in `arrangement`, ascending — the probe's index.
    spans: BTreeSet<usize>,
    matches: ParseSet,
    started: bool,
    /// Instrumentation: anchors scanned by the last advance.
    pub last_anchors: usize,
    /// Instrumentation: anchors the last advance actually had to **parse** — the
    /// arrangement misses. `last_parse_calls ≪ last_anchors` is the whole claim
    /// of this module, and the law oracle asserts it rather than assuming it.
    pub last_parse_calls: usize,
}

impl<'a, S: WindowSequence, P: SequenceParser<S::Item>> DiffParseStream<'a, S, P> {
    /// Register a differential parse stream over the initial window `W₀` with a
    /// private arrangement. Nothing is parsed yet: the first
    /// [`poll`](Self::poll) reports `M(W₀)`.
    pub fn open(
        seq: S,
        parser: P,
        store: &'a dyn TraceStore,
        window: Window,
    ) -> DiffParseStream<'a, S, P> {
        Self::open_with(seq, parser, store, window, MatchArrangement::new())
    }

    /// Register a stream over a **caller-supplied** arrangement, so several
    /// streams over the same grammar share one — the
    /// [`eval_snapshot_with`](crate::eval_snapshot_with) idiom for matching. The
    /// arrangement is taken by value and handed back by
    /// [`into_arrangement`](Self::into_arrangement).
    pub fn open_with(
        seq: S,
        parser: P,
        store: &'a dyn TraceStore,
        window: Window,
        arrangement: MatchArrangement<S::Item>,
    ) -> DiffParseStream<'a, S, P> {
        let spans = arrangement.keys().map(|(span, _)| span.len()).collect();
        DiffParseStream {
            seq,
            parser,
            store,
            window,
            arrangement,
            spans,
            matches: ParseSet::new(),
            started: false,
            last_anchors: 0,
            last_parse_calls: 0,
        }
    }

    /// The window through which this stream has delivered.
    pub fn window(&self) -> Window {
        self.window
    }

    /// The edition through which this stream has delivered.
    pub fn edition(&self) -> Edition {
        self.window.to()
    }

    /// The maintained sequence — the window's materialization.
    pub fn sequence(&self) -> &S {
        &self.seq
    }

    /// The parser being maintained. Pure, so this is how a caller (or an oracle)
    /// recomputes with the same grammar.
    pub fn parser(&self) -> &P {
        &self.parser
    }

    /// The shared arrangement, for inspection or reuse.
    pub fn arrangement(&self) -> &MatchArrangement<S::Item> {
        &self.arrangement
    }

    /// Take the arrangement back, to seed another stream over the same grammar.
    pub fn into_arrangement(self) -> MatchArrangement<S::Item> {
        self.arrangement
    }

    /// The maintained match-set `M(W_current)`, ascending by [`Value`].
    pub fn matches(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.matches.keys().cloned().collect();
        out.sort();
        out
    }

    /// Deliver the next batch, advancing to the store's current edition.
    pub fn poll(&mut self) -> Result<Vec<(Value, Diff)>> {
        self.advance_to(self.store.current())
    }

    /// Deliver the next batch, advancing the window's far end to `to`.
    ///
    /// The first call ignores `to` and reports `M(W₀)` at the registered window
    /// without moving the cursor; thereafter the window becomes `(from, to]` and
    /// the signed parse delta is returned. `to` may not precede the current far
    /// end ([`Error::Query`]).
    pub fn advance_to(&mut self, to: Edition) -> Result<Vec<(Value, Diff)>> {
        if !self.started {
            self.started = true;
            let w = self.window;
            self.seq.advance(self.store, w.from(), w.from(), w.to())?;
            self.matches = self.rebuild()?;
            return Ok(sorted(&self.matches));
        }
        if to < self.window.to() {
            return Err(Error::Query(format!(
                "differential parse stream window cannot retreat: already delivered through \
                 edition {}, asked to advance to {}",
                self.window.to().0,
                to.0
            )));
        }
        if to == self.window.to() {
            return Ok(Vec::new());
        }

        let window = Window::new(self.window.from(), to)?;
        self.seq.advance(self.store, window.from(), self.window.to(), to)?;
        let next = self.rebuild()?;
        let delta = set_difference(&next, &self.matches);
        self.matches = next;
        self.window = window;
        Ok(sorted(&delta))
    }

    /// `M(W)` for the current sequence, drawn from the arrangement wherever the
    /// consumed span is one already seen and parsed only where it is not.
    fn rebuild(&mut self) -> Result<ParseSet> {
        // Disjoint field borrows: the sequence is read while the arrangement is
        // written, which is exactly why they are separate fields.
        let seq = self.seq.items();
        let len = seq.len();
        let mut out = ParseSet::new();
        let mut calls = 0usize;

        for anchor in 0..len {
            let parses = match probe(seq, anchor, &self.arrangement, &self.spans) {
                Some(hit) => hit,
                None => {
                    calls += 1;
                    let AnchoredParses { parses, reach } = self.parser.parses_at(seq, anchor)?;
                    if reach < anchor || reach > len {
                        return Err(Error::Query(format!(
                            "SequenceParser reported reach {reach} at anchor {anchor} of a \
                             sequence of {len} items: the contract is anchor <= reach <= len"
                        )));
                    }
                    let span = seq[anchor..reach].to_vec();
                    let value = as_set(parses);
                    self.spans.insert(span.len());
                    self.arrangement.insert((span, reach == len), value.clone());
                    value
                }
            };
            for p in parses.iter() {
                out.insert(p.clone(), 1);
            }
        }

        self.last_anchors = len;
        self.last_parse_calls = calls;
        Ok(out)
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
    /// Open a [`DiffParseStream`] over this window as `W₀` — the free-function
    /// spelling read the other way round, mirroring
    /// [`parse_stream`](Window::parse_stream).
    pub fn diff_parse_stream<'a, S: WindowSequence, P: SequenceParser<S::Item>>(
        &self,
        seq: S,
        parser: P,
        store: &'a dyn TraceStore,
    ) -> DiffParseStream<'a, S, P> {
        DiffParseStream::open(seq, parser, store, *self)
    }
}
