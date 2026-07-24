# grmpl — Design & Implementation Plan (First Cut)

> A persistent relational process language whose execution model is *querying,
> deriving, watching, and patching a versioned world*, built on a differential
> core we own.

This document is the concrete design for the first cut. It fixes the semantic
core, the seven core types as Rust signatures, the substrate boundary
(fjall / iroh behind traits), the execution model, an end-to-end trace of
`take lamp`, and a milestone plan whose acceptance tests *are* the design laws.

---

## 0. Scope of the first cut

Locked decisions (from design discussion):

1. **Own differential core.** We define the differential calculus ourselves as
   the language's semantics. No binding to `differential-dataflow`/`timely` or
   `dbsp`. The DBSP calculus (∂, ∫, z⁻¹, nested fixpoint) is the *reference
   mathematics*, not a dependency.
2. **fjall behind traits.** An LSM-tree is physically a differential trace; we
   use fjall as the `TraceStore`/`EditionStore` implementation, but the
   language observes only opaque editions, never a `SeqNo`.
3. **Single authority domain in v1.** `Transport`/`MessageLog` traits exist and
   are stubbed in-process. **iroh is the designed-for real impl, deferred.**
4. **Time-travel seamed but deferred.** The LSM retains history; as-of query,
   replay, and forks are post-v1.

Non-goals for v1: distribution, time-travel/replay UI, the effect/type system
beyond stack-effect + read/write/send effect rows, capability types, invertible
printing, a polished surface syntax.

---

## 1. The bright line

```
┌────────────────────────────────────────────────────────────────────┐
│ SURFACE            rel · view · form · on         (last to design)   │
├────────────────────────────────────────────────────────────────────┤
│ SEMANTIC CORE  (ours, pure — this is the language)                   │
│   Relation · Query · Pattern · Patch · Snapshot · Process · Authority│
│   the differential calculus + the §12 laws (the spec)                │
├───────────────────────────── trait boundary ────────────────────────┤
│ SUBSTRATE  (swappable, below the semantic line)                      │
│   TraceStore / EditionStore  →  fjall (LSM: batch, snapshot, SeqNo)  │
│   Transport / MessageLog     →  in-proc (v1) · iroh (later)          │
└────────────────────────────────────────────────────────────────────┘
```

**The invariant:** substrate semantics must never become observable above the
boundary. The language sees *opaque editions* (frontiers) and *durable
messages* (causally ordered per channel). It never sees a fjall `SeqNo` or a
QUIC stream id. Break this and we are bound to the substrate; keep it and the
substrate is swappable.

---

## 2. The semantic core

### 2.1 Values, tuples, entities

```rust
/// A stable identity in the modeled world. An entity is a legitimate domain
/// value (Object law). It is NOT hidden tuple identity — tuples are structural.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entity(u64);

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Value {
    Ent(Entity),
    Int(i64),
    Text(Arc<str>),
    Bool(bool),
    Tuple(Arc<[Value]>),
    Bytes(Arc<[u8]>),
    /// Serialized P7 IR — a stored, live-redefinable behavior (P12). Opaque to
    /// the core (it names no IR); only `grmpl-lang` (de)codes it. Storing a
    /// behavior is asserting one of these; committing it re-runs the P8b
    /// effect/authority check at the commit boundary (`BehaviorChecker`).
    Code(Arc<[u8]>),
}

/// A row in a relation. Structural: identity is by content, never hidden.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tuple(Arc<[Value]>);

/// Interned relation name; the schema maps it to an arity + column types.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct RelId(u32);
```

### 2.2 Time and diffs — the differential substrate

```rust
/// Engine timestamp = (edition coordinate, iteration coordinate).
/// `edition` is the commit clock of an authority domain (total order in v1).
/// `iter` is the internal fixpoint coordinate for recursive views; it is
/// engine-private and never surfaces to the language.
///
/// NOTE: simulation time and wall time are NOT here. They ride as `Value`s in
/// tuples/messages, preserving the three-times separation (Replay law).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    pub edition: u64,
    pub iter: u32,
}

/// A Z-weight. Multiset by default; `+1` assert, `-1` retract, sums cancel.
/// Generalized later to any abelian group (WID measures ride here).
pub type Diff = i64;

/// One differential update: "at `time`, `tuple`'s multiplicity changed by `diff`".
pub struct Update {
    pub tuple: Tuple,
    pub time: Time,
    pub diff: Diff,
}

/// An opaque edition — a point in the (v1: totally ordered) commit clock.
/// In the distributed future this becomes a causal frontier (an antichain);
/// the language treats it as opaque either way (§10 future-proofing).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edition(u64);
```

The **fundamental law of the substrate** (definition of "value at an edition"):

```
value(rel, E) = Σ { u.diff : u ∈ trace(rel), u.time.edition ≤ E }   (consolidated)
```

`find` is reading this sum at one `E`; `watch` is streaming the `Update`s whose
`edition` crosses `E` upward. Every language operation is one of those two.

### 2.3 The seven core types

#### (1) `Relation` — a differential collection

```rust
/// A named, persistent collection of tuples, modeled as a differential trace.
/// Base relations (`rel`) are engine INPUTS; derived relations (`view`) are
/// dataflow OUTPUTS. Both present the same interface.
pub struct Relation {
    pub id: RelId,
    /// The arranged, indexed, persisted trace (backed by TraceStore).
    trace: TraceHandle,
}

impl Relation {
    /// Read the consolidated contents as-of an edition (the `find` primitive).
    pub fn read_at(&self, at: Edition) -> impl Iterator<Item = (Tuple, Diff)>;

    /// Read only the tuples matching a key prefix as-of an edition
    /// (index scan — the join/lookup primitive).
    pub fn scan_prefix(&self, key: &[Value], at: Edition)
        -> impl Iterator<Item = (Tuple, Diff)>;
}
```

#### (2) `Query` — a dataflow fragment *as a value*

```rust
/// A query plan is an immutable VALUE: a function `Collections → Collection`
/// built without running. Assembling it is pure; materializing/subscribing is a
/// Computation. This is the value/computation split that earns its keep.
#[derive(Clone)]
pub enum Query {
    Rel(RelId),
    Map    { input: Box<Query>, f: MapFn },
    Filter { input: Box<Query>, pred: Pred },
    Project{ input: Box<Query>, cols: Arc<[usize]> },
    Join   { left: Box<Query>, right: Box<Query>, on: JoinKey },
    Union  { left: Box<Query>, right: Box<Query> },
    Negate (Box<Query>),
    Distinct(Box<Query>),                       // set semantics
    Reduce { input: Box<Query>, key: Arc<[usize]>, agg: Agg },
    Iterate{ init: Box<Query>, step: Box<Query> }, // recursive fixpoint
}

impl Query {
    /// Evaluate against one edition — a Computation returning a snapshot relation.
    pub fn find(&self, snap: &Snapshot) -> Result<Vec<(Tuple, Diff)>>;

    /// Install as a maintained view — a Computation returning a delta stream.
    /// Snapshot–stream law: the first item is `find(current)`, subsequent items
    /// are signed deltas, with no missing interval.
    pub fn watch(&self, from: Edition, rt: &Runtime) -> Result<DeltaStream>;
}

pub struct DeltaStream { /* frontier-tracked subscription over arrangements */ }
```

#### (3) `Pattern` — matching over ordered data (parsing = matching)

```rust
/// A relation between an ordered input structure and a binding environment.
/// The SAME algebra runs over token seqs, tuples, records, ASTs, delta streams
/// (Pattern law). v1 implements it over token/value sequences only.
pub enum Pattern {
    Lit(Value),
    Bind(VarId),                       // capture: @name
    Seq(Vec<Pattern>),
    Choice(Vec<Pattern>),
    Repeat(Box<Pattern>),
    Guard(Box<Pattern>, Pred),
    Construct(Ctor),                   // -> Take { name: @name }
}

impl Pattern {
    /// Ambiguity returns a RELATION of parses, not a special error model.
    pub fn run(&self, input: &[Value]) -> Vec<(Bindings, /*rest*/ &[Value])>;
}
```

#### (4) `Patch` — a guarded proposal for the next edition

```rust
/// The semantic center. A Patch is a VALUE describing a candidate next edition;
/// committing it is a Computation. More than a bag of writes.
pub struct Patch {
    pub preconditions: Vec<Fact>,   // must hold in the snapshot it was built from
    pub asserts:       Vec<Fact>,   // + diffs
    pub retracts:      Vec<Fact>,   // - diffs
    pub emits:         Vec<Message>,// outbound (durable) messages
    pub cursor_advance: Option<InboxCursor>, // actor exactly-once bookkeeping
}

/// A ground tuple in a specific relation.
pub struct Fact { pub rel: RelId, pub tuple: Tuple }
```

#### (5) `Snapshot` — an immutable read of the world at an edition

```rust
/// An immutable view of every relation as-of one Edition. Backed by a fjall
/// repeatable-read snapshot below the line; opaque above it.
pub struct Snapshot {
    pub edition: Edition,
    reader: SnapshotReader, // wraps TraceStore::snapshot(edition)
}

impl Snapshot {
    pub fn read(&self, rel: RelId) -> impl Iterator<Item = (Tuple, Diff)>;
    pub fn holds(&self, fact: &Fact) -> bool; // precondition check
}
```

#### (6) `Process` — an actor = a concurrency domain, not an object

```rust
/// Stable identity + authority scope + ordered persistent mailbox + behavior.
/// Players/rooms/zones/services are Processes; chairs and lamps are not.
pub struct Process {
    pub id: ProcessId,
    pub authority: Authority,
    pub inbox: RelId,          // a persistent mailbox relation
    pub cursor: InboxCursor,   // next-unprocessed position
    pub behavior: Behavior,    // Snapshot × Message → Patch (pure)
}

/// The pure handler (Replay law): deterministic in (snapshot, message).
pub type Behavior = fn(&Snapshot, &Message) -> Result<Patch>;
```

#### (7) `Authority` — the unit of atomic commit

```rust
/// The slice of the world a Process may write. One atomic commit touches ONE
/// authority domain (Authority law); cross-domain consequences are messages.
pub struct Authority {
    pub domain: DomainId,
    /// Relations (optionally key-ranges) this authority may assert/retract into.
    pub owns: Vec<Scope>,
}

pub struct Scope { pub rel: RelId, pub range: Option<KeyRange> }
```

### 2.4 The laws, as the spec

| Law | Statement | Enforced by |
|-----|-----------|-------------|
| **Snapshot–stream** | `watch(q)` = the changing result of `find(q)`: `initial + Σdeltas = find(current)` | `DeltaStream` registers against a frontier; M2/M3 tests |
| **Patch–edition** | `commit(snap, patch)` creates exactly one new edition or has no effect | fjall atomic `batch()`; M1/M4 test |
| **Replay** | `behavior(snap, msg)` is deterministic | `Behavior` is pure; wall/random enter as data; M5 test |
| **Authority** | one atomic commit touches one authority domain | `Patch` validated against `Authority.owns` before commit |
| **Pattern** | parsing is matching over ordered data | `Pattern` is the only parse mechanism; M6 |
| **Object** | entities are values; properties/behaviors are relations | no property heap exists; enforced by absence |
| **Attention** | reactivity is a maintained query | `on` = `watch` + handler; no callback primitive exists |

---

## 3. The differential engine (ours)

### 3.1 Operators and how each handles diffs

| Operator | Class | Diff rule |
|----------|-------|-----------|
| map / filter / project | linear | diffs pass through unchanged |
| union / negate | linear | concat / flip sign |
| join | **bilinear** | `Δ(A⋈B) = ΔA⋈Bₙₒw + Aₙₒw⋈ΔB + ΔA⋈ΔB`, evaluated against arranged inputs |
| distinct / consolidate | non-linear | track multiplicity per key; emit ±1 as count crosses 0 |
| reduce / aggregate | non-linear | recompute only changed keys |
| iterate (fixpoint) | recursive | semi-naïve over the `iter` coordinate; retraction via signed diffs |

The genuinely hard operator is **`iterate` with retraction** (recursive views like
`implements` losing a prototype edge). It is prototyped in isolation before
anything depends on it (M3). Everything else is mechanical from the table.

### 3.2 Arrangements

An *arrangement* is a trace indexed by a key, shared across the queries that
need that key. **Arrangement sharing is the compiler's main optimization** —
two views that both index `located` by `place` share one arrangement or pay
twice. Arrangements are the physical realization of the "derived enfilade";
their by-key summaries are the enfilade's WID measures.

Arrangements live behind `TraceStore`: hot data in the fjall memtable, cold in
SST levels — the LSM's own tiering *is* the hot/cold split.

---

## 4. The substrate traits

```rust
/// Persistence for base-relation inputs and arranged traces. fjall impl below.
pub trait TraceStore: Send + Sync {
    /// Append a batch of updates atomically at a new edition, or no effect.
    /// Maps to `db.batch(); batch.insert(..); batch.commit()`.
    fn commit_batch(&self, updates: &[(RelId, Update)], edition: Edition) -> Result<()>;

    /// A repeatable-read snapshot as-of an edition. Maps to `db.snapshot()`.
    fn snapshot(&self, at: Edition) -> Result<SnapshotReader>;

    /// Scan a relation (optionally by key prefix) within a snapshot.
    fn scan(&self, snap: &SnapshotReader, rel: RelId, prefix: Option<&[Value]>)
        -> Result<Box<dyn Iterator<Item = (Tuple, Diff)>>>;
}

/// The current edition clock for an authority domain.
pub trait EditionStore: Send + Sync {
    fn current(&self) -> Edition;
    fn advance(&self) -> Edition; // called inside a successful commit
}

/// Durable, causally-ordered-per-channel messaging. In-proc in v1; iroh later.
pub trait Transport: Send + Sync {
    fn send(&self, to: DomainId, msg: &Message) -> Result<()>;
    fn recv(&self) -> Result<Option<(DomainId, Message)>>;
}
```

### 4.1 fjall mapping (confirmed against current API)

- **`commit` → `db.batch()`**: one atomic write batch spans multiple keyspaces
  (relations). Asserts/retracts, the outbound message rows, and the inbox-cursor
  advance all land in **one** batch → the Patch–edition law and durable
  exactly-once actor processing from a single primitive.
- **`Snapshot` → `db.snapshot()`**: repeatable-read; `snapshot.get`/scan see a
  fixed edition even as writes continue. This is `find q at E`.
- **`Edition` ↔ `SeqNo`** *below the line only*: fjall's `SeqNo` is monotonic;
  higher shadows lower (MVCC). We map an `Edition` to a seqno watermark. Above
  the boundary, `Edition` is opaque.
- **Retention caveat (superseded by P6).** This paragraph imagined leaning on
  fjall's MVCC — lazy GC of stale versions during compaction, time-travel via
  pinned snapshots, "only the latest edition live." **P6 does not.** Because
  `grmpl-store` owns an append-only `(edition, counter)` row per update (not one
  shadowing KV version per key), as-of reads are exact *by construction* at every
  surviving edition, and retention is an **explicit consolidation watermark** the
  store manages — `consolidate` folds history ≤ the watermark into a per-relation
  checkpoint and discards the raw rows, turning the as-of read into
  `O(checkpoint + tail)`. Reads/scans below the watermark are a hard `Error`
  (the four *edition doors*), and the GC policy (`grmpl-proc::gc`) never advances
  the watermark past the minimum durable watch cursor. See ROADMAP "P6 —
  History".

### 4.2 iroh mapping (deferred; designed-for)

- `Authority.domain` (`DomainId`) ↔ iroh **`EndpointId`** (Ed25519 pubkey).
- Cross-domain `send` = `ep.connect(addr, alpn)` → `conn.open_bi()`; receive via
  `ep.accept()` → `conn.accept_bi()`.
- Durability is ours, not iroh's: the outbound message is written inside the
  commit `batch()` (locally exactly-once), a delivery worker ships it over iroh
  (at-least-once), the receiver persists it into its inbox on *its* commit.
- **Do not use iroh-docs** (CRDT sync) — wrong consistency model. Connection
  layer only; optionally iroh-gossip (canopy routing) and BLAKE3 blobs
  (content-addressed immutable editions/snapshots) much later.

---

## 5. Execution model

### 5.1 `find` vs `watch`

- `find(q, snap)` — evaluate `q` against one `Snapshot`; returns consolidated
  tuples. No subscription, no engine state retained.
- `watch(q, from)` — register interest; returns a `DeltaStream` whose first item
  is `find(q, current)` and whose subsequent items are signed deltas. Race-free
  because registration pins the input frontier before the first read.

### 5.2 The commit protocol (optimistic)

```
1. handler builds Patch P from Snapshot S (edition Eₛ)         [pure]
2. validate: every Fact in P.asserts/retracts ⊆ Authority.owns [Authority law]
3. re-check: for each precond in P, EditionStore.current() view still holds
      - if any fails → RETRY: rebuild P from a fresh Snapshot, or surface failure
4. Eₙ = EditionStore.advance()
5. TraceStore.commit_batch(P.asserts(+) ∪ P.retracts(−)
                           ∪ P.emits(outbox rows)
                           ∪ P.cursor_advance, Eₙ)              [one fjall batch]
6. differential engine ingests the input diffs at Eₙ → arrangements update
   → every DeltaStream whose query is affected emits its delta
```

Step 5 is atomic (Patch–edition law). Step 3's precondition re-check on the live
edition is the optimistic-concurrency guard: two writers racing the same fact,
one commits, the other's precondition now fails and it retries or reports.

### 5.3 The process loop

```
watch(process.inbox, from=process.cursor)  // Attention law: no callbacks
  on each new inbox tuple m:
    P = process.behavior(snapshot_at(current), m)      // Replay law: pure
    P.cursor_advance = Some(next(process.cursor))
    commit(P)                                           // cursor moves IN the batch
```

Because the cursor advance is in the same atomic batch as the effects, a crash
before commit re-delivers `m` (behavior re-runs, deterministic); a crash after
commit sees the advanced cursor and skips `m`. That is exactly-once processing
without distributed transactions.

---

## 6. `take lamp`, end to end

World facts (base relations): `located(thing, place)`, `held(owner, thing)`,
`named(thing, text)`, `permits(viewer, verb, thing)`. Derived view:

```
view visible(viewer) :=
    located(viewer, @room), located(@thing, @room),
    named(@thing, @name), permits(viewer, see, @thing)
    yield { thing: @thing, name: @name }
```

Actors: `player-42` (a `Process`, authority over its session + the room slice it
occupies). Initial world: `located(lamp, room-7)`, `named(lamp,"brass lamp")`,
`located(player-42, room-7)`, `permits(player-42, see, lamp)`. Current edition `E₀`.

**1. Input arrives.** The client's line `"take lamp"` is committed as a message
into `player-42`'s inbox relation (its own tiny patch): `+ inbox(player-42, ["take","lamp"])`
at `E₁`.

**2. Attention fires.** `watch(inbox, from=cursor)` emits the new tuple. The
process loop wakes with message `m = ["take","lamp"]` and takes
`S = snapshot_at(E₁)`.

**3. Parse (Pattern law).** `form player-command` runs a `Pattern` over the token
sequence:
```
[ "take" @name ] -> Take { name: @name }
```
→ binding `@name = "lamp"`, value `Take { name: "lamp" }`. (Ambiguity would
return several parses; here one.)

**4. Resolve the noun (a Query, not navigation).**
`find visible(player-42, ?, ?)` against `S`, then restrict `name = "lamp"`:
- join `located(player-42,@room)` → `@room = room-7`
- join `located(@thing, room-7)` → `{lamp}` (and player itself, filtered)
- join `named`, `permits`, filter `name="lamp"` → single tuple `@thing = lamp`.

**5. Build the Patch (a value).**
```
Patch {
  preconditions: [ located(lamp, room-7) ],
  retracts:      [ located(lamp, room-7) ],
  asserts:       [ held(player-42, lamp) ],
  emits:         [ tell(player-42, "Taken.") ],
  cursor_advance: Some(next),
}
```

**6. Validate + commit.**
- Authority law: `located[room-7]` and `held[player-42]` ∈ `player-42.owns` ✓.
- Precondition re-check on `EditionStore.current()`: `located(lamp, room-7)`
  still holds ✓.
- `Eₙ = advance()` → `E₂`. One fjall `batch()`:
  `− located(lamp,room-7)`, `+ held(player-42,lamp)`, `+ outbox(tell …)`,
  cursor→next. Atomic (Patch–edition law).

**7. Differential propagation (Attention law).** The two input diffs at `E₂`
flow through arrangements:
- `visible(observer-in-room-7)` loses `lamp`: any `DeltaStream` on it emits
  `- { thing: lamp, name: "brass lamp" }`.
- a `watch held(player-42, ?)` emits `+ { thing: lamp }`.
- `initial + Σdeltas` for each watcher now equals `find` at `E₂`
  (Snapshot–stream law).

**8. Client output.** The `tell(player-42, "Taken.")` outbox row is delivered
(in-proc v1) and the player sees `Taken.`

**9. Concurrency (the whole point).** Suppose `player-99` in the same room also
commits `take lamp`, racing from the same `E₁`. One commit reaches step 6 first
and advances to `E₂`. The second's precondition `located(lamp, room-7)` is now
false at `current()`; its commit has **no effect** (Patch–edition law), and it
retries against `E₂`: the lamp is no longer visible → `"You don't see that here."`
No lock, no distributed transaction — optimistic commit against editions.

---

## 7. Surface sketch (built last)

The surface is deliberately the final thing. v1 exposes four declaration forms
lowering to the core; exact concrete syntax is out of scope here.

```
rel located { thing: Entity, place: Entity }          -- base relation (input)
view visible(viewer) { … yield { thing, name } }       -- Query value
form player-command { [ "take" @name ] -> Take{name} } -- Pattern
on player.inbox { command parse; dispatch }            -- watch + handler
```

Semantics: `view` builds a `Query`; `watch view` installs a `DeltaStream`; `on`
is `watch` + a transactional `Behavior`; `actor` is sugar for an `Authority`
scope + `on inbox`. Deepest core is `relation · derive · watch · commit`.

Open question deferred to the surface phase: the concatenative-vs-relational
tension. The named logic variables (`@room`, `@thing`) that carry the relational
power are *not* concatenative; the point-free feel likely belongs only to a thin
patch/effect-plumbing seam, not the whole surface. Decided when we design syntax.

---

## 8. Crate layout (Rust workspace)

```
grmpl/
├── crates/
│   ├── grmpl-core/       # Value, Tuple, Entity, Time, Edition, the 7 types, laws
│   ├── grmpl-diff/       # the differential engine: operators, arrangements, iterate
│   ├── grmpl-store/      # TraceStore/EditionStore traits + fjall impl
│   ├── grmpl-proc/       # Process, Authority, commit protocol, process loop
│   ├── grmpl-transport/  # Transport/MessageLog trait + in-proc impl (iroh later)
│   ├── grmpl-pattern/    # Pattern algebra (parse = match)
│   └── grmpl-lang/       # surface: rel/view/form/on → core lowering
└── DESIGN.md
```

`grmpl-core` depends on nothing below the line. `grmpl-diff` depends on
`grmpl-core` and the `TraceStore` *trait* (not fjall). Only `grmpl-store`
names fjall; only `grmpl-transport` will name iroh. This is the bright line as
a dependency graph.

---

## 9. Milestones — acceptance tests are the laws

| M | Deliverable | Acceptance test (a law) |
|---|-------------|-------------------------|
| **M0** | Workspace; `Value/Tuple/Entity/Time/Edition`; fjall `TraceStore` skeleton | round-trips a relation through fjall snapshot |
| **M1** | Base relations as inputs; `find` (full scan at edition); commit via fjall batch | **Patch–edition**: commit creates exactly one edition or no effect (incl. crash-mid-batch = no effect) |
| **M2** | Differential operators (linear + join + distinct); `watch` on non-recursive views | **Snapshot–stream**: `initial + Σdeltas = find(current)` under random insert/retract |
| **M3** | `iterate`/fixpoint with retraction (`implements`) | recursive Snapshot–stream **including deletion** of a prototype edge |
| **M4** | Optimistic `commit` with precondition re-check + retry | two concurrent `take lamp` from same edition → exactly one wins, other has no effect then retries |
| **M5** | `Process` loop; inbox cursor advanced inside commit batch | **Replay + Authority**: exactly-once under simulated crash before/after commit; cross-authority write rejected |
| **M6** | Minimal surface (`rel/view/form/on`); `take lamp` runs end to end | the §6 trace executes; a `watch visible` observer sees the `- lamp` delta |

Post-v1 (explicitly deferred): time-travel/as-of via snapshot pinning; iroh
`Transport`; effect rows for placement; CALM-monotonicity typing for
coordination-free cross-domain reads; `form` over delta streams and byte
sequences; invertible printing.

---

## 10. Risks & open questions

1. **Recursive retraction (M3)** is the one algorithmically hard core piece.
   Prototype in isolation; if incremental deletion is too costly, fall back to
   recompute-on-change for recursive views only, behind the same interface.
2. **Arrangement memory.** Sharing is the mitigation; without it, view fan-out
   multiplies trace storage. Measure arrangement reuse early (M2).
3. **fjall diff semantics.** LSM values are last-writer-wins, not additive; the
   diff-accumulation layer ("sum diffs at edition ≤ E") is ours, above fjall.
   Confirm compaction does not silently coalesce our per-edition versions before
   we consolidate them (retention policy).
4. **Cross-domain read consistency** (post-v1). A query may *read* across
   authority domains even though writes are single-domain. The principled answer
   is CALM monotonicity typing; noted now so the effect rows are designed to
   carry it, not retrofitted.
5. **Surface tension** (concatenative vs relational) is unresolved by design —
   deferred to the surface phase, does not block the core.

---

*Next concrete step after approval: scaffold the workspace (M0) and stand up the
fjall `TraceStore` with the round-trip test, then M1's Patch–edition test.*

---

## 13. Post-v1 additions (implemented)

M0–M6 landed as specified. Three post-v1 items followed, each behind the
seams the core already exposed:

**Arrangement sharing** (`grmpl-diff`). A `Query::Shared(Arc<Query>)` node marks
a sub-DAG as a shared arrangement; evaluation memoizes it by node identity ×
edition, so a sub-DAG referenced N times is read once (proven with a
read-counting store: 1 read vs N). This is §3.2's "compiler's main optimization"
as an opt-in on the operator DAG. Memoization is bypassed inside `Iterate`
(different `recur` bindings per fixpoint step), preserving recursive correctness.

**Text surface** (`grmpl-lang`). A lexer + recursive-descent parser + compiler
for `rel`/`view`/`form`. A `view`'s conjunctive body is planned left-to-right,
joining each atom on shared variables, then projecting `yield` and de-duplicating
— so `visible(viewer)` compiles to exactly the hand-built join query, confirming
"the MOO facade compiles to relational queries" (§3). `form` lowers to a
`Pattern`/`Form` parser. `on` remains programmatic (it needs an action
sublanguage). Syntax errors surface at parse; unbound-yield/arity at instantiate.

**iroh `Transport`** (`grmpl-transport`, feature `iroh`). `grmpl_core::Transport`
is a bytes-level cross-domain boundary (opaque above the line). Two impls: an
in-process net (the v1 default and reference, with the durable cross-domain
message loop proven — emit → serialize → deliver → persist into the receiver's
inbox), and an iroh-backed QUIC transport where a `DomainId` maps to an endpoint;
`send` opens a bi-stream and waits for an ack. A real two-endpoint QUIC loopback
test passes.

> **iroh version note.** iroh **0.95** currently hard-pins the pre-release
> `ed25519-dalek 3.0.0-pre.1`, which fails to compile against `pkcs8 0.11` on the
> Rust 1.95 toolchain (an upstream `KeyMalformed` variant change). The transport
> is therefore built against iroh **0.92** (stable `ed25519-dalek 2.2.0`), whose
> API is `NodeAddr`/`node_addr()`/`Watcher` rather than 0.95's
> `EndpointAddr`/`EndpointId`. Revisit the pin when iroh unpins the broken
> pre-release. The `iroh` feature is off by default, so this never affects the
> core build or test suite.

Crate graph now (bright line intact — only `grmpl-store` names fjall, only
`grmpl-transport` names iroh):

```
grmpl-core ── grmpl-diff ── grmpl-proc ── grmpl-lang
     │            │              │
     ├── grmpl-store (fjall)     │
     ├── grmpl-pattern ──────────┴── grmpl-lang
     └── grmpl-transport (iroh, feature-gated)
```

## 14. Follow-ons (implemented)

Three deeper items completed the picture.

**Cross-domain routing** (`grmpl-proc::Domain`). A patch's `emit`s are
partitioned at commit: local-inbox messages write straight in; remote ones land
in a durable **outbox** in the *same atomic commit*. A delivery pass ships
outbox rows over the `Transport` (at-least-once) and retracts them on success;
the receiver drains its transport and applies each message into its inbox,
deduplicating by `(sender, seq)` — so redelivery is idempotent and the end-to-end
guarantee is exactly-once *apply* without a distributed transaction. The
`Message` wire codec was hoisted into `grmpl-core::wire` (pure value
serialization) so the transport and the router share one encoding.

**Incremental recursion** (`grmpl-diff::IncrementalFixpoint`). Replaces
recompute-from-∅ for recursive views with a maintained materialized fixpoint:
**monotone** (insertion) changes are applied incrementally by semi-naïve
iteration warm-started from the previous fixpoint (cheap — only the growing
frontier drives derivations; the linear-recursion case such as `implements`),
while **non-monotone** (retraction) changes take the **DRed** (delete-and-
re-derive) path — no recompute. This is the CALM split in miniature (monotone =
cheap, non-monotone = a bounded overdeletion + regrowth), validated to agree with
boundary-recompute across 200 rounds of mixed churn.

*Incremental deletion (DRed) — implemented.* On a retraction the maintainer runs
two passes: **overdeletion** removes every derived fact that had *a* derivation
resting on a retracted base/`init` tuple — seeded by evaluating `init`/`step`
against *only the deleted tuples* of each base relation (via a new
`eval_with` relation-override primitive on the differential engine) and
propagated through the recursion variable — then **regrowth** (the same
semi-naïve pass) re-derives, from the survivors, everything still reachable under
the new base and folds in any co-committed insertions. Keying overdeletion on the
*broken derivation* rather than "lost all one-step support" is what makes it
collapse **non-well-founded derivation cycles** (mutually-supporting facts whose
only grounding was retracted); the random-churn oracle exercises exactly this,
and a named regression test (`deleting_a_cycles_grounding_collapses_it`) pins it.
The instrumentation `last_path` (`Grow` | `DeleteRederive`) and
`last_overdeletion_rounds` report which path ran. Both fast paths still assume
**linear** recursion (`step` distributes over unions of the recursion variable).

**`on` / action sublanguage** (`grmpl-lang`). The text surface now covers
behavior, not just views and grammars. An `on <inbox> parse <form> { match … }`
handler compiles to a `Behavior`: it parses the message, dispatches on the
command, and runs statements — `resolve <view> where <col> ~ <var>` (noun
resolution over a relational view), `find <rel>(…)` (bind columns from a base
fact), `expect`/`assert`/`retract`/`emit` (build the guarded patch). The full
`take lamp` scenario now runs **defined entirely in text**, driven through a real
`Process`, with no hand-written Rust behavior — every surface form (`rel`,
`view`, `form`, `on`) lowering to the same core.
