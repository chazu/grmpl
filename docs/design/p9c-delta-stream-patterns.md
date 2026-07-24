# P9c — Delta-stream patterns (design)

Status: **design / research** (TKT-97, sub of P9 / TKT-81). This document is the
gate: **no implementation lands until it is reviewed.** It fixes the semantics of
running the `grmpl-pattern` algebra over a *delta stream* — the three open
questions the ticket names: **retraction**, **termination**, and **window
semantics** — and ends with a decomposed, gated implementation plan to be filed
as sub-tickets.

## 1. Where this sits

P9 ("Pattern algebra: inputs, printing, streams") has three parts:

* **P9a** (TKT-95/112) — *inputs*: the `MatchInput` seam. One engine
  (`Pattern::run_in`) reads any ordered structure through a cursor with a
  well-founded progress measure. Three instances landed: `&[Value]`
  (tuple/record), `AstInput` (AST-descend), `BytesInput` (bytes).
* **P9b** (TKT-96) — *printing*: invertible construction (`CtorSpec::print`),
  the `-> construction` arrow run backwards.
* **P9c** (this doc) — *streams*: the fourth `MatchInput` shape promised by
  `DESIGN.md` §2.3 #3 — *"the SAME algebra runs over token seqs, tuples,
  records, ASTs, **delta streams**"*.

The substrate already has a delta stream: `grmpl_diff::DeltaStream`
(`crates/grmpl-diff/src/watch.rs`). Its Snapshot–stream law is *the first `poll`
is `find(query)` at the registration edition; each later `poll` is the
consolidated signed change up to `current()`, so `initial + Σ deltas =
find(current)`*. Each item is a signed update `(Tuple, Diff)`; the underlying
records are `grmpl_core::Update { tuple, time: Time { edition, iter }, diff }`
(`Diff = i64`, `+1` assert / `−1` retract, sums cancel). `DeltaStream` is
poll-driven and totally ordered in commit order `(edition, counter)`.

## 2. The core tension

The v1 pattern algebra assumes its input is a **finite, monotone, append-only**
ordered structure: a slice you can walk left-to-right to a definite end, with a
`measure()` that strictly decreases per `next()` and hits `0` exactly at the end.
`Repeat` terminates *only* because that measure is well-founded.

A delta stream violates every one of those assumptions:

1. **Unbounded** — a live stream has no end; `at_end()` never becomes true, so
   `measure()` has no `0`, `parse_all` (consume-to-end) is meaningless, and
   `Repeat` cannot terminate.
2. **Signed** — items carry a `Diff`; a `−1` *retracts* a tuple asserted
   earlier. A monotone matcher has no notion of "un-see an item."
3. **Temporal** — order is commit order (edition), and *which* editions are in
   scope is itself a question (the whole point of a window).

The design must reconcile these without breaking the "one algebra, many
cursors" invariant and without dragging storage across the bright line.

## 3. The key move: a window is the reduction to `MatchInput`

**A pattern never runs over the raw live stream. It runs over a *window* — a
finite, edition-bounded slice of deltas.** Windowing is precisely the reduction
from "unbounded temporal stream" to "finite ordered structure," which is exactly
the shape `MatchInput` already requires.

```
  unbounded live stream  ──window [from,to)──▶  finite ordered delta slice
                                                        │
                                            existing Pattern::run_in engine
```

This cleanly splits two concerns that v1 never had to separate:

* **Liveness / unboundedness** is a *runtime* concern — the poll/advance loop
  that keeps re-windowing (`DeltaStream::poll`, the watch pump). It stays outside
  the pattern algebra entirely.
* **Structural matching** is a *pattern* concern — it only ever sees one
  finite window at a time, over which the v1 termination story holds verbatim.

Consequence (a hard invariant this design introduces):

> **Matching requires a bound. The bound is the window.** There is no
> `run_in` over a live, open-ended stream; the API only accepts a materialized,
> finite window. The temporal "keep watching" is the outer reactive loop, not a
> `Pattern::Repeat`.

This immediately answers **termination**: inside a fixed window the progress
measure is the remaining delta count, finite by construction, so `Repeat`
terminates identically to the slice case. `at_end()` means *window* exhausted,
not *stream* exhausted.

## 4. Retraction: two modes, both keep the engine monotone

The stream is signed, but the pattern engine must stay a clean relation
`input → Vec<parses>` with no "retract a past match" operation (that would wreck
the value/computation split). We reconcile this with **two matching modes**, and
crucially *retraction is absorbed before the engine runs in both*:

### 4a. State mode (consolidated) — "match the window's net contents"

Consolidate the window to its net multiset first: fold `Σ diff` per tuple
(`grmpl_diff::multiset::{add, strip_zeros, to_sorted_vec}`), drop zeros, sort.
`+t` then `−t` **cancels before matching** — the pattern never sees `t`. What
survives is a sorted `Vec<(Tuple, Diff)>` with positive weights: *exactly the
`&[Value]` shape the v1 engine already matches.*

> **State mode needs no new `MatchInput` instance.** It is *consolidation
> (existing multiset code) + the v1 slice engine.* Retraction is handled by
> arithmetic, upstream of the algebra. This is "parse the current contents of
> the window" and is the natural mode for surface `form … over window`.

**Anchoring caveat (correctness).** A delta inside `[from, to)` may retract a
tuple asserted *before* `from`. Consolidating the window's deltas *in isolation*
then yields a spurious negative multiplicity. State-mode windows must therefore
be **snapshot-anchored**: the matched contents are `find(q, to)` restricted to
the tuples the window touched — equivalently, base snapshot at `from` plus the
window's deltas. The oracle (§7) must pin this against `eval_snapshot`/
`eval_delta`.

### 4b. Event mode (log-structured) — "match the sequence of changes"

Here retraction is *first-class data the pattern matches on*. Each update is
reified as a `Value` and surfaced in commit order, sign included — the same
trick `BytesInput` uses to surface a byte as `Value::Int`:

```
delta (tuple, diff, edition)  ⇒  Value::Tuple([ sign, tuple_as_value, edition ])
```

(`sign = Value::Int(diff.signum())`, `edition = Value::Int`.) Now a `Guard` can
filter on the sign and a `Bind` captures the tuple, so temporal/CEP patterns
become expressible: *"an assert of X later followed by a retract of X"* detects a
delete-after-insert; *"three retracts in a row"* detects churn. The engine stays
monotone-over-a-finite-sequence — it matches a literal event log; the sign is
just a field.

> **Event mode needs one thin new instance:** `DeltaInput`, a cursor over a
> finite `&[Update]` (or a pre-reified `&[Value]`) whose `next()` yields the
> reified delta value and whose `measure()` is the remaining update count.

### The finding

Neither mode requires the engine to learn retraction. State mode absorbs signs
by **consolidation before matching**; event mode absorbs them by **reifying the
sign into the matched value**. The pattern algebra remains the pure, monotone,
finite-input relation it is today. Retraction is real, but it never reaches the
combinators.

## 5. Window semantics

A window is an **edition range** `[from, to)`, aligned exactly with
`TraceStore::scan_updates(rel, from, to)` and `eval_delta(q, store, from, to)` —
no new primitive, no new ordering. `to` is `store.current()` at poll time. The
grammar of windows the runtime layer may build:

| Window | Definition | v1? |
|---|---|---|
| **Snapshot** | degenerate `[⊥, E]`; whole consolidated state at `E` | yes — *is* state-mode matching over `find(q, E)`; unifies with today's tuple matching |
| **Tumbling** | disjoint consecutive `[t₀,t₁), [t₁,t₂), …` | yes |
| **Sliding** | overlapping, advance by `step < size` | yes |
| **Session** | gap-based (close after N idle editions) | **deferred** |
| **Count** | last N updates / last N tuples | **deferred** (needs a counter cursor over commit order; edition ranges cover the common case) |

`iter` (the fixpoint sub-coordinate of `Time`) is **not** a window axis — windows
range over commit editions only; `iter` is internal to `Iterate`.

Windows compose with both modes: a window materializes to *either* a consolidated
tuple-set (state mode) *or* a reified event slice (event mode), then the existing
engine runs. Nothing in the pattern crate learns about `scan_updates`.

## 6. Incremental match maintenance (the parse stream)

The hardest question: when the window advances and the stream retracts an event a
pattern previously matched, should the *match* be retracted — i.e. can we emit a
`DeltaStream` **of parses** obeying the Snapshot–stream law?

**Extended law.** Let `M(W)` be the set of parses of pattern `P` over window `W`
(in a fixed mode). For a monotonically advancing window sequence `W₀ ⊆ W₁ ⊆ …`
(edition ranges `[from, tᵢ)` with `tᵢ` increasing), the *parse stream* emits
`M(W₀)`, then signed deltas `M(Wᵢ) − M(Wᵢ₋₁)`, so `M(W₀) + Σ deltas =
M(W_current)`. This is Snapshot–stream lifted from tuples to parses.

**Implementation: window-recompute + set-difference — reuse, don't invent.**
Each advance re-runs the finite (terminating) pattern over the new window and
diffs the match-set against the previous one with `set_difference` — *precisely*
the DRed / boundary-recompute strategy `grmpl-diff` already uses for `Iterate`
and recursive views (`recursive.rs::{advance, set_difference}`, `DESIGN.md`
§5.2). Delta-stream pattern maintenance is thus a *composition* of existing
substrate pieces:

```
parse-stream(P, mode) :=  for each advanced window Wᵢ:
                            Mᵢ   = run P over Wᵢ         (finite, terminating)
                            emit set_difference(Mᵢ, Mᵢ₋₁)   (signed parse delta)
```

**Honest tradeoff.** Window-recompute is correct and cheap to build on what
exists, but it recomputes per advance rather than sharing arrangements. A *truly
differential* incremental parser (DBSP-style incremental matching over
sequences) would be more efficient and is a real research problem. It is
**explicitly out of scope** here and should be filed as a distinct follow-on
ticket, not attempted under this phase.

## 7. What this costs the substrate (bright line, wire, tests)

* **Bright line — intact.** `grmpl-pattern` stays pure. The `DeltaInput`
  instance is a cursor over an *in-memory finite slice* — it takes no
  `TraceStore`, names no fjall. The windowing that calls `scan_updates` lives
  *above* the pattern crate — in `grmpl-diff` (which already owns
  `DeltaStream`/`eval_delta`/`multiset`) — and hands the pattern crate a
  finished `Vec<Update>` / `Vec<Value>`. Storage never crosses into the algebra.
* **Wire / FORMAT_VERSION — no bump.** Reifying a delta uses only existing
  `Value` constructors (`Value::Tuple`, `Value::Int`); the reification is
  in-memory, exactly like `BytesInput` surfacing a byte. Nothing new is
  serialized. (Contrast P9a/TKT-95, which *did* bump 1→2 for the new `Value::Bytes`
  tag — P9c introduces no new tag.) The doc states this so review can confirm it.
* **Determinism — preserved.** Event mode surfaces updates in commit order
  `(edition, counter)`; state mode consolidates and emits `to_sorted_vec`
  (tuple-sorted). No `HashMap` iteration order leaks into results.
* **Tests — law oracles, per repo convention.** TKT-112 established that
  `grmpl-pattern/tests/` must be *seeded randomized law oracles*, not
  single-example unit tests. The implementation must ship:
  1. **Progress-measure law** for `DeltaInput` (mirrors the P9a oracle):
     `measure()` strictly decreases by the consumed amount per `next()`, `== 0`
     iff at window end; `Repeat` over random windows terminates.
  2. **Consolidation law** (state mode): match over a random window == match over
     the snapshot-anchored consolidation, cross-checked against
     `eval_snapshot`/`eval_delta`.
  3. **Order law** (event mode): reified events appear in commit order.
  4. **Parse-stream law** (if §6 is in scope): `M(W₀) + Σ deltas = M(W_current)`
     under random insert/**retract** churn — the delta-stream analogue of the M2
     Snapshot–stream oracle, retraction included.
  Seed printed on every assertion for replay (established convention).

## 8. Decomposition — implementation sub-tickets (do NOT start here)

This is a research/design task; the gate forbids implementation. The following
should be filed as sub-tickets of TKT-81 for the orchestrator to route:

1. **`DeltaInput` + reification (event mode).** New `MatchInput` instance over a
   finite update slice in `grmpl-pattern`; reification helper; progress-measure
   law oracle. Pure, no store dep, no FORMAT_VERSION bump. *Size S.*
2. **State-mode matching.** `consolidate_window` (snapshot-anchored) in
   `grmpl-diff` feeding the existing slice engine; consolidation law oracle vs
   `eval_snapshot`/`eval_delta`. *Size S–M.*
3. **Windowing layer.** `window(from, to)` over `scan_updates` in `grmpl-diff`;
   tumbling + sliding; edition-bounded only. *Size M.*
4. **Parse stream (incremental match maintenance).** Window-recompute +
   `set_difference`; extended Snapshot–stream law oracle for parses under
   insert/retract churn. The genuinely hard piece — likely its own ticket.
   *Size M–L.*
5. **Deferred / follow-on (separate tickets, not this phase):** session &
   count windows; a truly differential incremental parser (shared arrangements).

## 9. Summary of findings

* **Windowing is the whole trick.** It reduces an unbounded temporal stream to
  the finite ordered structure `MatchInput` already consumes, and thereby
  *recovers termination for free* inside each window. Matching a live stream is
  forbidden; matching a window is the v1 story unchanged.
* **The pattern engine never learns retraction.** State mode absorbs signs by
  consolidation *before* matching (and needs no new instance — it is
  consolidation + the v1 slice engine). Event mode absorbs signs by *reifying*
  them into the matched value (one thin `DeltaInput` cursor). Both keep the
  algebra a pure, monotone, finite relation.
* **Incremental parse maintenance reuses DRed.** A `DeltaStream` of parses is
  window-recompute + `set_difference` — the exact recursive-view strategy the
  substrate already ships — obeying a lifted Snapshot–stream law. True
  differential parsing is deferred research.
* **The bright line and wire format are untouched.** No store dependency in the
  algebra, no new serialized tag, no FORMAT_VERSION bump.
