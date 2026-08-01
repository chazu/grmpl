# Concurrency under distribution — causal frontiers, per-domain clocks, and the bill that comes due

[`CONCURRENCY.md`](CONCURRENCY.md) describes the model as implemented and then
proposes six ways to cash in the parallelism it permits. Its item 6 —
**per-domain commit clocks** — is listed last, marked *large / high risk*, and
described in one paragraph as "the local form of P15 distribution."

This document is that paragraph expanded. It argues one claim:

> **Distribution and multi-core write parallelism are the same change at
> different radii.** Both require exactly one thing the substrate does not do
> today — stop representing "which version" as a single integer on a single
> clock. Everything else, including the part that is usually hardest
> (exactly-once cross-node delivery), is already built and tested.

**Status.** Part 1 is description: it states what is already true, with sources,
and separately what is not. Parts 2 and 3 are **proposal and analysis — not a
record of landed work.** No part of the staged plan below has been implemented.

> **Sources.** The frontier model is stated in
> [`docs/book/src/grmpl/editions.md`](book/src/grmpl/editions.md) and founded in
> `idea.md` §10. The distribution sketch is
> [`docs/book/src/future/index.md`](book/src/future/index.md) §"Distribution
> along scope covers (P15)" and `docs/ROADMAP.md` P15. Laws cited are
> `DESIGN.md` §2.3 #6 (actors), §4/§4.2 (cross-domain routing), §5.2 (patch–edition),
> §10 (opaque editions).

---

## Part 1 — What an edition would become, and what is already in place

### 1. The counter and the frontier are one object at two scales

On one machine there is exactly one commit clock. Commits serialize, so you
number them: 1, 2, 3. That is a **total order** — for any two editions, one is
unambiguously before the other. `grmpl_core::Edition` is a `u64` and this is
what it means today.

Spread the world across machines committing concurrently and there is no clock
handing out the next integer — and you do not want one, because forcing every
commit through a global counter costs a network round-trip per commit and
destroys exactly the throughput distribution was supposed to buy. Machine A and
machine B must be able to commit at the same instant without talking.

So "which version am I looking at?" stops being a number. The replacement is
built from the version DAG in four steps.

**The causal partial order.** Every edition was computed from earlier ones — the
snapshot its patch read, the branches a merge combines. Draw an edge from cause
to effect:

```text
e ≼ f   ≝   e is in f's causal history
            (f directly or transitively descends from e)
```

This is *partial*: two commits made concurrently on different branches are
**incomparable** — neither `e ≼ f` nor `f ≼ e`.

**A version is a downward-closed set.** "The world as far as I am concerned" is
the set `H` of commits I consider to have happened. For `H` to be coherent it
must be downward-closed under `≼`:

```text
if  f ∈ H  and  e ≼ f   then   e ∈ H
```

You cannot include a commit without all its causal ancestors, or you are reading
an effect whose cause you omitted — a torn snapshot. This is what distributed
systems call a **consistent cut**.

**The frontier is its leading edge** — the maximal elements of `H`:

```text
frontier(H)  =  { e ∈ H  :  there is no  f ∈ H  with  e ≺ f }
```

Two properties make this the right handle. It is an **antichain** (no two tips
are comparable — they are the concurrent latest-per-source), and it **fully
determines `H`**, since `H` is the downward closure of its frontier. So a
frontier is a compact name for an entire version.

**The order on frontiers is containment of histories:**

```text
frontier(H₁) ≤ frontier(H₂)   iff   H₁ ⊆ H₂
```

Concurrent frontiers — `{A:7, B:3}` versus `{A:5, B:9}` — are incomparable.
A comparison that returns a clean boolean on the counter returns *"these are
concurrent"* here. **That is the whole reason portable code may not write
`edition < other`.**

Two collapses are worth naming, because they are why this is not exotic:

- **Vector clocks are frontiers.** When the DAG comes from N sequential sources,
  each contributes exactly one tip and the frontier written per source is
  `{A:7, B:3, C:12}` — a vector clock, arrived at rather than adopted.
- **The counter is a frontier too.** A chain has antichains of exactly one
  element, and the downward closure of "commit `n`" is "commits `1..n`." The
  monotonic counter *is* the frontier of a world with one source.

### 2. Four things that are already true

These are the enabling facts, and they are load-bearing for the claim that P15
is additive rather than a rewrite.

| # | Enabling fact | Where | Status |
|---|---|---|---|
| 1 | The option was reserved in the type and in law | `grmpl-core/src/time.rs:25-30`; `DESIGN.md` §10; `CLAUDE.md` invariants | ✅ stated |
| 2 | Authority: one commit touches one domain; cross-domain effects are messages | `DESIGN.md` §2.3 #6, §5.2; checked in `grmpl-proc::commit` | ✅ enforced |
| 3 | Exactly-once cross-domain apply, no 2PC | `grmpl-proc/src/domain.rs` | ✅ implemented, single-node transport |
| 4 | A durable causal DAG with `≼` as a measure | `grmpl-ent/src/dag.rs`; `grmpl-ent/tests/durable_fork.rs` | ✅ implemented |
| 5 | CALM classification — which reads are coordination-free | `grmpl-type/src/calm/`; `DESIGN.md` §10 risk 4 | ✅ implemented (P8c) |

**(1) The option was reserved.** `Edition`'s doc comment says outright: *"In the
distributed future this becomes a causal frontier; the language treats it as
opaque either way."* `DESIGN.md` marks the total order **v1-only**, and
`CLAUDE.md` lists as an invariant that the language observes opaque `Edition`s,
*never physical sequence numbers*. A program that cannot compare editions cannot
break when comparison starts returning "concurrent." As
[`editions.md`](book/src/grmpl/editions.md) puts it: *we have deliberately not
told you what an edition is, so that we can change what it is later without
breaking your code.*

**(2) Authority removes the hard problem before it appears.** One commit touches
one domain; cross-domain effects are messages, never shared mutation. There is
therefore **no cross-machine transaction to coordinate** — no two-phase commit,
no distributed lock, no consensus on the write path. This is why
[`future/index.md`](book/src/future/index.md) can call the distributed story
*additive*. It is the single most important structural fact in this document.

**(3) The genuinely hard part is already built.** `grmpl-proc/src/domain.rs`
implements the durable-outbox pattern: a patch's `emit`s are partitioned, and
messages to a remote inbox are written into a durable outbox **in the same
atomic commit** as the local effects. `flush_outbox` ships them at-least-once
and retracts on success; `receive` applies each into the local inbox,
deduplicating by `(sender, seq)` via the `seen` relation. That is exactly-once
apply without a distributed transaction — working today over an in-process
transport. Swapping in `grmpl-transport`'s iroh is a transport change, not a
semantics change.

**(4) The DAG and the routing measure exist.** `grmpl-ent/src/dag.rs` is the
`DagWood` — the fulltrace's branch structure, durable, encoded/decoded with the
store. `is_ancestor` *is* `≼`, the question consistent cuts ask constantly;
`common_ancestor` gives the merge base. `durable_fork.rs` proves ancestry
survives a reopen and that forks share nodes structurally. Ancestry is answered
as a **WID upward measure** — subtree-pruned, `O(measure)` — not a DAG walk. And
the same measure family that answers `touched_since` for local `watch` routing
is what would tell a cluster where an update must be delivered:
[`future/index.md`](book/src/future/index.md) — *"canopy interest summaries tell
the cluster where updates must be routed."*

**(5) The cross-domain read rule is already typed.** `grmpl-type::calm`
classifies a `QueryIr` monotone iff it contains no `Negate` and no `Reduce`, and
CALM's theorem says a monotone query reads coordination-free across a domain
boundary. This is the answer `DESIGN.md` §10 risk 4 reserved — *"the principled
answer is CALM monotonicity typing; noted now so the effect rows are designed to
carry it, not retrofitted"* — and P8c delivered it. It substantially discounts
bill item 2 (Part 3), which is why that item has two treatments rather than one.

> **A note on one document.** `docs/ENT-AND-XANADU.md` says there is "no
> fulltrace-style causal DAG in the store yet" and that `fork` is an `O(state)`
> verbatim copy. That is a gap analysis of the **deleted** `grmpl-store` LSM,
> written before the Ent migration; both gaps are closed. Read it as history,
> not as current state.

### 3. Four things that are not true yet — the honest ledger

The opacity law is a **discipline over the language**, not a property the type
enforces on the substrate. `Edition` derives `PartialOrd, Ord`, so `<` compiles
everywhere inside the core crates, and the substrate uses it:

| Site | What it does | Why it breaks under a partial order |
|---|---|---|
| `grmpl-proc/src/watch.rs:194` | `if to <= from { return Ok(0) }` — "nothing new" | `to` and `from` may be incomparable; neither "nothing new" nor "something new" is the right answer |
| `grmpl-proc/src/gc.rs` (`min_watch_cursor`) | numeric `min` over cursor editions | a set of incomparable frontiers has no minimum, only a greatest lower bound |
| `grmpl-core/src/store.rs:20` (`current`) | returns one `Edition` | "latest" is not unique across concurrent sources |
| `grmpl-ent` (`read_at`, `scan_updates`) | fold history up to an integer | needs to fold a downward-closed set instead |

One more, softer: `grmpl_core::Time { edition: u64, iter: u32 }` derives
lexicographic `Ord`. The general formulation of differential dataflow *already*
uses **partially ordered** timestamps with a join-semilattice, so this is the
part of the system least disturbed by the change — but the derived `Ord` would
have to become an explicit lattice (`join`/`meet`) rather than a comparison.
Encouragingly, an audit shows `Time`'s ordering is barely load-bearing: call
sites overwhelmingly project `u.time.edition` as a `u64`
(`grmpl-diff/src/window.rs:334,475`, `grmpl-proc/src/replay.rs:116`), and the
`sort()` calls in `grmpl-diff` are over tuples and multisets, not times.

And the largest gap, which is not about ordering at all:

**There is no merge operation.** `dag.rs:184` can find the merge *base* of two
divergent branches, and `EntStore::common_ancestor_with` exposes it. Nothing
joins two divergent branches back into one. `DESIGN.md` does not mention merge.
This matters because a partial order without a merge is a system that can
diverge but never reconverge — see Part 3, item 5.

---

## Part 2 — Implementing item 6, staged

The staging principle: **each stage is independently valuable, independently
testable, and leaves the system shippable.** Nothing here requires the network
until stage E, and stages A–C are pure single-node work that also delivers
`CONCURRENCY.md`'s multi-core write parallelism.

| Stage | What | Effort | Independently useful? |
|---|---|---|---|
| A | Make the total order unobservable | days | ✅ removes latent breakage, zero behavior change |
| B | Shard the store by domain — N clocks, one process | week+ | ✅ **this is item 6's payoff**: parallel writers |
| C | `Edition` becomes a frontier | week+ | ✅ enables cross-domain consistent reads |
| D | Consistent cuts for cross-domain reads | week | ✅ correctness for D-spanning queries |
| E | Transport swap — the network | days | the actual distribution |

### Stage A — make the total order unobservable

**Goal:** every place the substrate depends on `Edition`'s totality is *named*,
so stage C is a change to a handful of implementations rather than an
open-ended audit.

1. **Remove `Ord`/`PartialOrd` from `Edition`.** Replace derived comparison with
   explicit, intention-revealing operations on the store trait:

   ```rust
   // sketch — grmpl-core::store
   /// Is `a` in `b`'s causal history? Total order today; DAG reachability later.
   fn precedes(&self, a: Edition, b: Edition) -> bool;
   /// Greatest lower bound — the latest version both arguments incorporate.
   fn meet(&self, a: Edition, b: Edition) -> Edition;
   /// Least upper bound, if the substrate can form one.
   fn join(&self, a: Edition, b: Edition) -> Option<Edition>;
   ```

   On today's counter these are `<=`, `min`, `max` — a one-line implementation
   and **zero behavior change**. That is the point: the stage is a refactor that
   passes the existing law suites unmodified, which is exactly how you know it
   did not change the model.

2. **Rewrite the four ledger sites** in terms of them. `watch.rs`'s
   `to <= from` becomes `store.precedes(to, from)`; `min_watch_cursor` folds
   with `meet` instead of `min`.

3. **Give `Time` an explicit lattice** — `join`/`meet` over `(edition, iter)` —
   rather than derived lexicographic `Ord`, per the audit above. **This is not
   the same change as item 1**, and the difference constrains stage C: see
   "The context-free constraint" below.

4. **Add a lint-level guard.** The bright line is already checked by
   convention; add a test asserting `Edition` exposes no ordering, so the
   discipline stops depending on reviewer memory.

**Exit criterion:** `cargo test` and `grmpl-conformance` green, **unmodified**,
with `Edition: !Ord`.

### Stage B — shard the store by domain

This is where item 6's payoff actually lands, and it needs no network.

`DESIGN.md` §5.2 already says **one authority domain has one commit clock**. The
implementation collapses all domains onto one `EntStore` with one clock, so
disjoint writers serialize behind a mutex they have no semantic reason to share.
Stage B takes the design at its word.

- **One `EntStore` per domain**, each with its own edition clock, its own
  `Mutex<Inner>`, and its own fsync — all inside one process, sharing one
  granfilade so structural sharing and the `DagWood` still span them.
- **Routing already exists.** `Domain.routes: HashMap<RelId, DomainId>` maps
  inbox relations to owning domains; `Domain::commit` already partitions a
  patch's emits into local writes and outbox rows. A cross-shard message becomes
  an outbox row instead of a local write — *the same code path already taken for
  a remote domain*, just with a different transport underneath.
- **The Authority check becomes the shard check.** A commit that tries to write
  outside its domain is already an Authority violation; now it is also
  structurally impossible, since the writer holds one shard's handle.

**Why this is the real win:** the contention numbers in `CONCURRENCY.md` §7 —
221→170 commits/s from 1 to 8 threads, `fair(min/max)=0.000` — measure threads
racing *one* clock. Eight threads on eight domains share nothing: no lock, no
fsync, no rejects. This is the "commits/s across N disjoint authority domains"
axis that document's verification section asks for.

**Sequencing note:** stage B composes with, and does not replace, `CONCURRENCY.md`
items 0–2. Group commit (item 1) still amortizes fsync *within* a shard; snapshot
handles (item 2) still remove reader stalls. Do those first — they are cheaper and
their benefit multiplies across shards.

### Stage C — `Edition` becomes a frontier

Only now does the representation change.

```rust
// sketch — grmpl-core
/// An opaque version handle. A frontier over the version DAG: an antichain
/// whose downward closure is the set of commits this version incorporates.
/// One source → one tip → structurally the old counter.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Edition(Frontier);   // private; no Ord, no pub u64
```

Four constraints keep this honest:

- **Single-source stays cheap.** A one-tip frontier must not allocate or
  cost more than the `u64` it replaces on the common path. This is the
  representation's acceptance test, not a nice-to-have — a world with one domain
  must not pay for a generality it does not use.
- **`precedes` becomes DAG reachability**, answered by the `DagWood`'s
  `is_ancestor` as a WID measure — `O(measure)`, subtree-pruned. The data
  structure that makes this affordable already exists and is already durable.
- **`meet` becomes greatest-lower-bound over tips**, which is what `common_ancestor`
  computes. Again: already implemented, currently used for one purpose.
- **`join`/`meet` must be context-free** — the constraint below, and the one
  most likely to be discovered too late.

#### The context-free constraint

Stage A splits editions and timestamps deliberately, and the split is not
bookkeeping:

| | `Edition` | `Time` |
|---|---|---|
| What its order means | causal reachability in the version DAG | the differential dataflow coordinate `(edition, iter)` |
| Where the ops live | the **store trait** — `fn precedes(&self, a, b)` | a **type-level** `impl Lattice for Time` |
| Why | only the substrate knows the DAG | dataflow operators need it *inside* the dataflow, where there is no store handle |

`Time` is a **product** order, so its lattice is derived from `Edition`'s. That
is where the two meet, and where a naive stage C breaks: differential's operator
math needs

```rust
fn join(&self, other: &Self) -> Self   // no &self store — nowhere to put the DAG
```

If `Edition::join` requires DAG access, `Time` cannot implement that signature,
and the frontier math the engine runs on every recursive view suddenly needs a
store handle threaded through the dataflow. That would be a large, ugly, and
entirely avoidable refactor.

**So the frontier representation must make `join`/`meet` computable without
consulting the DAG.** A per-source (vector-clock-shaped) frontier does exactly
that: `join` is pointwise max per source, `meet` pointwise min — pure structure,
no reachability query. The DagWood is then needed only for `precedes` on
arbitrary editions and for merge-base, *not* for the operator-level frontier
math that runs constantly.

This is a real constraint on the representation, and it happens to argue for the
shape §1 arrived at independently: the frontier written per source. Treat it as
a stage C acceptance criterion, not a discovery to make during implementation.

The wire format changes: `wire::FORMAT_VERSION` **must** bump, per the
`CLAUDE.md` invariant, since editions appear in cursor rows and outbox
envelopes.

### Stage D — consistent cuts

Covered as bill item 2 in Part 3; it is a stage because it must land with C.

### Stage E — the network

At this point a "remote domain" differs from a local shard only in which
`Transport` its `Domain` holds. `grmpl-transport` (iroh, feature-gated) already
implements the trait; `domain.rs` already tolerates at-least-once delivery,
reordering, and redelivery by construction. Migration of a domain between nodes
is `fork_edition` plus relocate — `O(edit)`, per
[`future/index.md`](book/src/future/index.md): *"relation slices and actors
migrate while entity identities stay stable — identity is a handle, not a
location."*

---

## Part 3 — The bill that comes due

Five things get harder. Each is stated as the problem, then a proposed
treatment.

### 1. `current()` has no unique answer

**Problem.** `EditionStore::current()` returns one `Edition`. Across concurrent
sources there is no unique "latest" — and a caller that treats a locally-observed
frontier as global will silently read a stale or torn world.

**Treatment.** Split the notion in two, and make the ambiguous one impossible to
call by accident:

- `current_in(domain) -> Edition` — the local tip of one domain's clock. Total
  within a domain, always well-defined, and what almost every internal caller
  actually wants (commit, cursor advance, `read_at` on a local relation).
- `observed() -> Edition` — the frontier this node has actually incorporated: a
  join of its own tip with the highest tip received from each peer. This is a
  *lower bound* on global progress, never a claim about it.

There is deliberately no `global_current()`. The cost is that some code must
name which domain it means; the benefit is that the impossible question cannot
be asked. Note this is already gated correctly for durability by
`CONCURRENCY.md` item 1: `current()` returns the **durable** edition, and the
same gate applies per-shard.

### 2. Cross-domain reads can tear

**Problem.** A read spanning domains can observe an effect whose cause it
omitted — precisely the downward-closure violation §1 defines. Single-node
snapshot isolation gives coherence for free because one clock orders everything;
sharded, it must be constructed.

**Treatment, first pass: pin the cut.** Construct the snapshot's frontier once,
and make the tearing window a *type* error rather than a runtime hazard.

- A `Snapshot` carries a frontier, not an edition, and is built by joining the
  per-domain tips **once**. It then reads each domain as-of its own tip in that
  frontier. Because a frontier is by construction downward-closed, every read
  through it is a consistent cut.
- This composes with `CONCURRENCY.md` item 2 (snapshot handles): that item
  already proposes cloning an immutable root handle per snapshot under one brief
  lock. Under sharding it clones *one root per domain*. The two changes want the
  same refactor, and doing item 2 first makes this nearly free.
- **The staleness that remains is honest.** A cut may not include a peer commit
  that already happened elsewhere. That is not a bug; it is the CAP-bounded
  truth about a partitioned world, and the actor model already requires callers
  to treat cross-domain state as message-delivered rather than instantly
  visible.

**Treatment, second pass: most reads do not need a cut at all.** Pinning is the
conservative universal answer, and this document reached for it before checking
what the project already owns. `DESIGN.md` §10 risk 4 designated the answer to
this exact question years ago — *"a query may read across authority domains even
though writes are single-domain; the principled answer is CALM monotonicity
typing"* — and **P8c implemented it**: `grmpl-type::calm::classify` reads
monotonicity straight off the `QueryIr`, with a plan monotone **iff it contains
no `Negate` and no `Reduce`**.

CALM's result is that a monotone query has a coordination-free evaluation: no
later input can retract a row the reader already saw, so **a monotone
cross-domain read needs no pinned cut and no barrier** — it can read a peer
domain's growing state directly and still be consistent. Only non-monotone reads
(negation, aggregation) need the pinned frontier above.

So the routing rule is a classification, not a policy:

```text
classify(plan) == Monotone      → read cross-domain uncoordinated
classify(plan) == NonMonotone   → pin the cut (first pass)
```

Three properties make this safe to lean on. The classification is **structural**
— plan shape only, no schemas, no data, no run, so it costs nothing at
evaluation time. It is **sound by over-approximation** — a `Monotone` verdict is
a guarantee proven by that module's falsification oracle, while `NonMonotone` is
a conservative *may*, so the asymmetry always errs toward asking for
coordination we could have skipped, never toward skipping coordination we
needed. And it is **already tested**, which means this bill item is closer to
paid than the rest of them.

The practical consequence for staging: the monotone path is available as soon as
domains are sharded (stage B), *before* frontiers exist (stage C). Reads that
classify `Monotone` need neither. That makes stage B more useful on its own than
this document first claimed.

### 3. Watch cursors become frontiers

**Problem.** `OnWatch::pump` computes an interval `(from, to]` and short-circuits
on `to <= from` (`watch.rs:194`). Over a partial order there is no interval, and
"caught up" is not a comparison.

**Treatment.** The pump's structure survives; three operations change.

- **"Caught up"** becomes `store.precedes(to, from)` — containment of histories,
  not `<=`.
- **The interval** becomes a set difference over the DAG: the commits in `to`'s
  downward closure absent from `from`'s. The `DagWood`'s WID measure prunes this
  by subtree, so it is `O(measure)` rather than a walk — this is exactly the
  question the enfilade is shaped to answer.
- **The cursor row** stores a frontier, not an `Int`. The `(watch: Ent, edition: Int)`
  layout in `watch.rs:54` becomes `(watch: Ent, frontier: Bytes)` under the
  bumped `FORMAT_VERSION`.

**What does not change, and this is the important part:** the exactly-once
guarantee. The cursor advance is a `commit_if` preconditioned on the present
cursor row within *one* domain, and that domain still has one clock. Racing
pumps still resolve to exactly one winner by the same mechanism. Distribution
does not weaken the Attention law — it only changes what a cursor *names*.

Corollary, from `CONCURRENCY.md` item 5: independent watchers can already pump in
parallel, since each pump preconditions on its own cursor row. Sharding makes
that parallelism physical rather than theoretical.

### 4. The GC watermark becomes a meet

**Problem.** `gc.rs::min_watch_cursor` takes a numeric `min` over cursor
editions to find the floor GC must not cross. A set of incomparable frontiers
has no minimum.

**Treatment.** Fold with `meet` (greatest lower bound) instead of `min` — the
operation introduced in stage A, computed by `common_ancestor`. The invariant is
unchanged in meaning: *the watermark never passes any live watch frontier*, so
every installed watch stays pumpable. Only the fold operator changes.

Two consequences worth stating plainly:

- **GC gets more conservative.** The meet of divergent frontiers can sit well
  below any individual cursor, so a world with long-diverged branches retains
  more history. That is correct — the history is genuinely still reachable — but
  it means retention becomes a function of branch divergence, which operators
  will need visibility into.
- **GC is per-domain.** Each shard consolidates against the meet of the cursors
  that read it. A quiet domain is not held back by a busy one, which is an
  improvement on today.

### 5. Merge is unspecified — the real gap

**Problem.** The other four items are mechanical: a known operation replaced by
its partial-order generalization. This one is a design question with no answer in
the repo. Divergent branches can be *detected* (`common_ancestor`) but not
*reconciled*. A partition that heals leaves two frontiers and no defined way to
produce one.

And the easy answer is explicitly closed off: `DESIGN.md:387` — **do not use
iroh-docs (CRDT sync) — wrong consistency model.** grmpl is not last-write-wins
and not automatically convergent; it is a world with authority and invariants,
where silently merging two divergent histories can produce a state neither
branch would have committed.

**Proposed framing, in preference order.** The honest recommendation is to make
merge *rare* rather than *automatic*:

1. **Prevent divergence for authored state.** Under the Authority law a domain
   has exactly one writer, so two domains cannot diverge on the same facts *by
   construction*. Divergence only arises from deliberate `fork_edition` or from
   a partition where both sides accepted writes to the same domain. Refusing the
   latter — a domain is writable on exactly one node at a time, with handoff
   rather than concurrent acceptance — makes merge an explicit feature of
   branching, not a recovery path. **This is the recommended default**: it keeps
   the strong-consistency story grmpl already has and gives up availability under
   partition, which is the right trade for a world with invariants.
2. **Merge as a patch, checked at the boundary.** When merge *is* wanted (world
   branching, instanced worlds, editorial forks), express it as an ordinary
   `Patch` computed from the two branches and their merge base, and put it
   through the normal commit boundary — Authority, schema, effect checks. Then a
   merge cannot install a state a commit could not have installed, which is the
   only invariant that really matters. The diff of each branch against the base
   is exactly what `grmpl-diff` computes.
3. **Conflict as data, not exception.** Where both branches changed the same
   fact, the merge patch records the conflict as a relation rather than picking a
   winner, and a Process resolves it — reactive, replayable, auditable, and
   subject to the same laws as everything else. This keeps policy out of the
   substrate, where it does not belong.

**This should be designed before stage C, not after.** Stage C makes divergence
representable; shipping that without a reconvergence story is how a system
acquires permanent forks.

---

## What would prove any of it

Same standard as [`CONCURRENCY.md`](CONCURRENCY.md): **none of this changes a
law.**

- `grmpl-conformance` and `grmpl-ent/tests/store_laws.rs` stay green
  **unmodified** — determinism, patch–edition, history/consolidation, fork
  identity. Stage A in particular is a pure refactor; if it needs a law suite
  edited, it changed the model.
- `grmpl-proc/tests/optimistic_commit.rs` and `seq_contention_law.rs` keep
  asserting exactly-one-winner under real threads, per domain, at whatever
  parallelism sharding produces.
- `grmpl-proc`'s cross-domain tests keep asserting exactly-once apply under
  redelivery and reordering — the property stage E leans on entirely.

New evidence the stages should produce:

| Stage | Measurement |
|---|---|
| A | zero delta on every existing benchmark (it is a refactor) |
| B | **commits/s across N disjoint authority domains** — the axis `CONCURRENCY.md` asks for; should scale with cores where today it flatlines |
| B | `fair(min/max)` → 1.0 for disjoint domains (no shared precondition to starve on) |
| C | single-source frontier ops within noise of the `u64` they replace |
| C/D | a randomized law oracle: concurrent commits across domains, asserting every `Snapshot` read is a consistent cut (no effect without its cause) |
| E | exactly-once apply under an adversarial transport (drop, dup, reorder) |

## Where this could be the wrong call

Stated plainly, because the rest of this document argues one side.

- **Sharding is irreversible in practice.** Once relations are partitioned by
  domain, a query spanning domains is permanently more expensive than one that
  does not, and world-modelling decisions become performance decisions. Today
  any relation can join any other at uniform cost. That is a real property being
  spent.
- **The payoff is unmeasured.** No benchmark yet shows that a realistic world
  *has* mostly-disjoint domains. If a MOO's hot path is a few shared rooms, the
  contention just relocates and the complexity buys nothing. **The N-disjoint-domain
  benchmark axis should be built and run against a realistic world before stage B
  is committed to** — it is cheap, and it is the honest gate on the entire plan.
- **`CONCURRENCY.md` items 0–5 are not exhausted.** Group commit and snapshot
  handles are days of work against a measured ~1 ms fsync and a measured reader
  stall. Item 6 is weeks against an unmeasured ceiling. The ordering in that
  document — 0 and 4, then 1, then 2 — remains correct, and this document does
  not argue for jumping the queue.

The conclusion is narrower than "build this": **the model is unusually ready for
distribution, the readiness is worth keeping, and stage A is worth doing now**
regardless of whether B–E ever happen — it costs days, changes no behavior, and
converts an unenforced discipline into a compiler-checked one. The rest should
wait on measurement.
