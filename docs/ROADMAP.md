# grmpl — Roadmap

This roadmap sequences the work from the current substrate (a versioned,
differential store with a small surface language) toward the full design in
[`DESIGN.md`](../DESIGN.md). Each phase is a shippable increment that keeps the
whole workspace green (`cargo build && cargo test && cargo clippy`). Invariants
that every phase must preserve are in [`CLAUDE.md`](../CLAUDE.md).

---

## P0 — Repo & format safety

Harden the substrate's on-disk and on-wire formats and its determinism *before*
building schema/query machinery on top of them, so later phases inherit a stable
base rather than migrating a moving one.

### Work

* **One versioned codec.** Collapse the two duplicated value/tuple encodings
  (`grmpl-core::wire`, `grmpl-store::codec`) into a single canonical codec in
  `grmpl-core::wire`, shared by the message wire and the store record. Prefix
  every serialized artifact with a `FORMAT_VERSION` byte; decoders reject other
  versions loudly.
* **Durable catalog.** Persist the name→`RelId` catalog in the store's `__meta`
  keyspace, exposed through a `Catalog` trait in `grmpl-core` (the store-API
  boundary decision: the *contract* is core, the *durable map* is a store
  concern). Append-only — a name's id never silently changes.
* **Determinism.** `read_at` returns tuple-sorted rows; `scan_updates` returns
  updates in commit order `(edition, counter)`; the language `find`/`resolve`
  binds the least matching tuple.

### Acceptance

* `cargo build`, `cargo test`, `cargo clippy` all green on the whole workspace.
* A record/message written under one `FORMAT_VERSION` fails to decode under
  another (tested).
* The catalog round-trips across a store reopen; a conflicting rebind errors
  (tested).
* `read_at` is tuple-sorted and `scan_updates` is `(edition, counter)`-ordered
  regardless of write order (tested).

### Start here

`crates/grmpl-core/src/wire.rs` and `crates/grmpl-store/src/codec.rs` (the two
codecs), `crates/grmpl-store/src/lib.rs` (`read_at`, `scan_updates`, `__meta`),
`crates/grmpl-lang/src/compile.rs` (`find`/`resolve` binding). Commit `docs/`
and `CLAUDE.md` as part of this phase.

### Not in this phase

Schema evolution and column typing (P1/P8), order-preserving keys or range
scans (the store still uses full-scan consolidation — `DESIGN.md` §9), and
wiring the language layer to *consume* the durable catalog for id allocation
(a follow-on once schemas land in P1).

---

## P1 — Relation schemas (+ evolution minimum)

Give every relation an **arity and column types**, enforced at the commit
boundary, so later phases (aggregates, as-of reads, typing) build on typed
relations rather than bare tuples.

### Work

* **Schema types (core).** `Ty` (`Ent`/`Int`/`Text`/`Bool`/`Tuple`/`Any`),
  `Column {name, ty}`, and `Schema {columns}` are pure core value types above
  the bright line, with the invariant logic beside them: `Schema::check`
  (arity + per-column type admission) and `Schema::is_additive_over` (the
  evolution predicate).
* **Durable schema registry.** A `SchemaCatalog` trait in `grmpl-core` (the
  store-API boundary decision again — contract in core, durable map in the
  store) mapping `RelId → Schema`, **versioned by the edition** at which each
  version took effect. `grmpl-store` persists each version in `__meta` under
  `sch:{rel}{edition}` keys; `schema_at` answers as-of queries (needed before
  P6 exposes as-of reads). Evolution is **additive only** — a new version may
  append columns but never remove, reorder, retype, or rename an existing one.
* **Commit-boundary enforcement.** Beside the Authority check in
  `commit_patch` and `Domain::commit`, every asserted/retracted world fact whose
  relation has a registered schema must conform (arity + types), else
  `Error::Schema`. Unregistered relations pass — schemas are opt-in.
* **Typed `rel` surface grammar + named columns.** `rel r(a: Ent, b: Int)`
  parses named, typed columns (an unannotated column defaults to `Ty::Any`);
  `Program::{schema, rel_columns, resolve_column, register_schemas}` expose the
  schema, resolve a column *name* to an index for `view`/`on`, and record each
  relation's schema into the durable registry.

### Acceptance

* `cargo build`, `cargo test`, `cargo clippy` all green on the whole workspace.
* A schema round-trips across a store reopen; a non-additive change errors and
  an additive one is versioned by edition, with `schema_at` returning the
  version in effect as-of any edition (tested, incl. a randomized-churn law
  oracle over the additive-evolution and as-of invariants).
* A write violating its relation's registered arity/type is rejected at the
  commit boundary; an unregistered relation is unchecked (tested).
* The typed `rel` grammar lowers to named, typed columns; an unknown type
  annotation is a compile error; the untyped form still parses (tested).

### Start here

`crates/grmpl-core/src/schema.rs` (the schema types + invariants),
`crates/grmpl-core/src/store.rs` (`SchemaCatalog`, `NoSchemas`),
`crates/grmpl-core/src/wire.rs` (`encode_schema`/`decode_schema`),
`crates/grmpl-store/src/lib.rs` (`__meta` `sch:` keyspace),
`crates/grmpl-proc/src/commit.rs` + `domain.rs` (enforcement beside authority),
`crates/grmpl-lang/{lexer,parser,ast,compile}.rs` (typed grammar + named cols).

### Not in this phase

Non-additive migration/backfill, column defaults, and richer types (row/effect
types — P8). *Consuming* the durable catalog to recover stable `RelId`s across
reopens (the language still assigns ids from `rel_base`) is a follow-on
(TKT-90).

---

## P2 — Reduce / aggregates

Add grouped aggregation to the differential engine: a `Reduce` operator that
groups its input by key columns and folds each group with an aggregate
(`Count`/`Sum`/`Min`/`Max`), so views can yield derived measures over typed
columns (P1).

### Work

* **`Reduce` operator (core engine).** `Query::Reduce { input, key, agg }` with
  `Agg` = `Count | Sum(col) | Min(col) | Max(col)` (grmpl-diff). Snapshot
  semantics group the *set boundary* of the input (present, positive-weight
  tuples — the only sensible reading for `Min`/`Max`, consistent with
  `distinct`) by `key` and emit one weight-1 tuple per non-empty group as
  `key-columns ++ [aggregate]`. Deterministic regardless of input scan order:
  distinct keys yield distinct outputs and every fold is order-invariant.
* **Stateless boundary-recompute delta.** `Reduce` is non-linear, so its delta
  over `(from, to]` is `reduce(input@to) − reduce(input@from)` — the same
  recompute-on-change rule as `distinct`. Per-key incremental state (recompute
  only the keys whose groups changed, `DESIGN.md` §3) is deferred to P13.
* **Threaded through the engine.** `collect_rels`, `eval_with`/`eval_inner`
  (Shared + Recur contexts), and `eval_delta` all handle `Reduce`. Aggregates
  are **rejected inside `Iterate`** (`Error::Query`): a recursive fixpoint over a
  non-monotone operator has no monotone semi-naïve maintenance.
* **Named-column yield surface (language).** `Program::reduce_view` groups and
  folds a view's *yielded columns by name* (`NamedAgg`), lowering to
  `Query::Reduce`. This waits on P1's named columns.
* **Aggregate yield grammar (language, TKT-106).** The text surface now spells
  the same reduce directly in a view's `yield` clause: `view team_totals() {
  score(p,t,pts) yield t, sum(pts) }`. The plain `yield` identifiers become the
  grouping key and the single `count()`/`sum(c)`/`min(c)`/`max(c)` aggregate
  folds its column, so an aggregate-carrying `view` lowers to a `Query::Reduce`
  through the ordinary `Program::view`/`view_ir` path — observationally
  identical to the programmatic `reduce_view`/`NamedAgg` surface. (Lexer needed
  no change; parser `yield_clause`, `ast::{AggFunc,AggYield}`, and `view_ir`
  carry it.) At most one aggregate per view; malformed aggregates are compile
  errors. Known limitation inherited from the set-valued reduce: a bare
  `count()` adds no projected column, so it counts distinct group tuples (`1`
  per group) — a multiset/row `count` is left to a follow-on.

### Acceptance

* `cargo build`, `cargo test`, `cargo clippy` all green on the whole workspace.
* The snapshot–stream law (`initial + Σ deltas = find(current)`) holds for every
  aggregate under randomized assert/retract churn, including group creation,
  update, and emptying (last member retracted) — checked against an independent
  model that recomputes each aggregate from the present base directly (a genuine
  law oracle, not self-consistency).
* A `Reduce` placed inside an `Iterate` is rejected at evaluation (tested).
* `reduce_view` folds named columns and errors on unknown column/view names
  (tested).
* The `yield` aggregate grammar (TKT-106) lowers to *exactly* the same result
  as the programmatic `reduce_view`/`NamedAgg` surface, and its output is
  independent of source-commit order — both checked each round by a seeded
  randomized-churn law oracle (`crates/grmpl-lang/tests/aggregate_yield.rs`).

### Start here

`crates/grmpl-diff/src/query.rs` (`Agg`, `Reduce`, `reduce_snapshot`,
`eval_delta`), `crates/grmpl-diff/src/recursive.rs` (`collect_rels`),
`crates/grmpl-core/src/error.rs` (`Error::Query`),
`crates/grmpl-lang/src/compile.rs` (`NamedAgg`, `reduce_view`).
Oracle template: `crates/grmpl-diff/tests/reduce_stream.rs`.

### Not in this phase

Per-key incremental aggregate state (P13), aggregates inside recursion, and
richer aggregates (average/distinct-count/multiset-row-count/user-defined). The
parser grammar for aggregate yields in `view` landed in TKT-106.

---

## P3 — Client sessions & world construction (landed)

The world becomes reachable from clients: connect, become a player, build and
play — all as ordinary commits.

### Work

* **Replay-safe entity allocation** (`grmpl-proc::Alloc`). A single-row counter
  relation `(next: Int)` hands out fresh entity ids; the read and the counter
  bump ride in the *same* patch as the effects that use the id, so a replay from
  the same edition reproduces identical ids (Replay law). Interim single-writer;
  the durable, concurrency-safe allocator is P4.
* **Provisioning** (`grmpl-session::Server::login`). A connection becomes a
  player: a name is the minimal credential and a durable identity (the `PLAYER`
  relation rebinds the same entity across reconnects); a new player is *spawned
  as a commit* — allocated, named, and placed in the root room atomically.
* **Verbs as ordinary patches** (`grmpl-session::world`). `dig` / `create` /
  `go` / `take` / `look` are plain guarded `Patch`es over `LOCATED` / `NAMED` /
  `EXITS` / `HELD`; world construction shares the exact commit machinery as
  `take lamp`, no privileged path. `take`'s precondition is the optimistic race
  point.
* **Line-based TCP session layer** (`grmpl-session::net`, bin `grmpld`). One
  connection per player on its own thread; the first line logs in, each
  subsequent line is a command whose `TELL` text is written straight back. The
  interim single-writer inbox-seq scheme serializes commits behind one writer.

`grmpl-session` is an **edge** crate above the semantic core; it may name std
TCP precisely because it is not one of the core crates — the bright line
constrains the core, not the application on it.

### Acceptance

`crates/grmpl-session/tests/build_and_race_the_lamp.rs`: a builder digs a room,
walks in, and creates a lamp; two players walk in and race to take it — exactly
one wins, the loser is declined, and the store shows one `HELD` row — **entirely
through client command lines**. `tests/tcp_session.rs` re-runs the race across
two real loopback sockets on separate threads. `crates/grmpl-proc/tests/alloc.rs`
pins the allocator's replay-safety and counter invariant.

### Not in this phase

Durable / concurrent id + inbox-seq allocator (P4), per-player authority scoping,
websocket transport, reactive push of `watch` output to clients (P5).

---

## P4 — Scheduling & simulation time (landed)

A patch can schedule a future message, and simulation time and randomness enter
the world only as committed data — so replay is exact (DESIGN.md §2.2: the three
times stay separate; wall/simulation time ride as `Value`s, never as engine
coordinates). Prerequisite for P10 replay.

### Work

* **`Patch.scheduled` → durable timer rows** (`grmpl-core::Scheduled`, folded in
  `grmpl-proc::commit_patch` and `Domain::commit`). A scheduled entry lands as a
  durable timer row in the *same atomic commit* as the patch's effects, subject
  to the same Authority and Schema laws as any world write.
* **Durable, race-safe per-key seq allocator** (`grmpl-proc::SeqAlloc`),
  generalizing the P3 interim single-writer `Domain.outseq`. The counter advance
  is folded into the committing patch *with a precondition on the present counter
  row*, so two commits racing the same key resolve to exactly one winner and the
  loser retries against the winner's value. (The one unguarded case — the very
  first allocation of a key, when there is no row to precondition on — is seeded
  once on an un-raced path with `SeqAlloc::seed`.)
* **Atomic fire commit** (`grmpl-proc::Scheduler::fire_due`). Each due timer is
  delivered by one `commit_if` **preconditioned on the timer row**: retract the
  timer and append the inbox message (at an allocated seq) in one batch. The
  timer-row precondition is the exactly-once guard — the first fire retracts the
  row, so a racing driver or a post-crash retry is rejected, never duplicated.
* **Wall-clock / randomness driver** (`grmpl-proc::ClockDriver`). The single
  sanctioned home for nondeterminism: it samples the wall clock and randomness
  and commits them as ordinary *data* rows `(seq, wall_ms, rand)`. Firing reads
  `now` from the committed sample, and behaviors read time / random rolls from
  the trace — so replaying the same samples reproduces the identical world.

### Acceptance

`crates/grmpl-proc/tests/schedule.rs`: schedule → fire delivers exactly once at
the seeded seq and retracts the timer (M5 weight-1 witness); two patches racing
the same timer row yield exactly one winner; the guarded `SeqAlloc` hands out
contiguous unique seqs under a racing allocation; the clock driver records
samples as data. A randomized-churn law oracle (24 seeds) schedules timers at
random due times, advances a committed clock, fires after every sample, and
checks the delivered inbox against an independent model *and* against a second
replay run each round (exactly-once, due-ordered, contiguous seqs, deterministic
replay).

### Not in this phase

Reactive push of fired messages to clients (P5); wiring the session layer's
inbox-seq onto `SeqAlloc` (the generalization exists; the session swap is a
follow-on); replay & forks over the recorded samples (P10).

---

## P5 — Reactive handlers (`on watch`) (landed)

An `on watch <view>` belongs to a Process: a maintained view whose signed deltas
are *pumped* into that Process's inbox as messages. This is the Attention law
made concrete (DESIGN.md §2.4: reactivity is a maintained query; `on` = `watch` +
handler; no callback primitive exists). Activations are durable rows, not
callbacks — which is what makes the P10 activity log complete.

### Work

* **`grmpl-proc::OnWatch`** — binds a view (`grmpl-diff::Query`) to a Process's
  inbox via a **durable watch-cursor** relation `(watch: Ent, edition: Int)` and
  the shared P4 seq counter. `install` seeds the cursor at the current edition
  (**skip-initial** default); `install_including_current` seeds it at
  `Edition::ZERO`, so the first pump delivers the whole current view as `+` rows.
* **`OnWatch::pump`** — reads the batch of signed deltas since the cursor
  (`eval_delta` over `[cursor, current)`) and, in **one atomic commit**,
  materializes each delta as an inbox message (`(diff: Int, row: Tuple)` at a
  seq from the shared `SeqAlloc`) **and** advances the cursor. The commit is
  `commit_if`-preconditioned on the present cursor row, so racing pumps resolve
  to exactly one winner (each activation appears once, at a unique seq); the
  delivered stream is a pure function of committed data, so replay is exact.
  When the view is unchanged over the interval the pump commits nothing (so the
  cursor never chases the pump's own non-view commits); it legitimately lags
  `current` by those editions, and `eval_delta` from the lagging cursor stays
  empty until the next real view change, at which point that whole interval is
  delivered at once.
* **Cascades are async message chains, never reentrancy.** The pump only
  *appends*; it never runs a behavior. A change caused by handling an activation
  is delivered by a *later* pump as new messages — a chain of separate commits.

### Acceptance

`crates/grmpl-proc/tests/on_watch.rs`: skip-initial omits the pre-existing
snapshot then streams post-install deltas; `including current` delivers the
snapshot first; activations are ordinary inbox rows and a handler's change only
surfaces on the next pump (async cascade). A randomized-churn law oracle (32
seeds) interleaves random `BASE` churn with pumps and checks, each round, the
snapshot–stream law (`Σ delivered deltas = find(view, current)`) against an
independent model, exactly-once contiguous unique seqs (every inbox row weight
1), and a second replay run (deterministic delivery). A 4-thread race on a shared
store confirms concurrent pumps deliver each activation exactly once.

### Not in this phase

An `on watch` grammar surface in `grmpl-lang` and reactive push of activations to
connected clients in `grmpl-session` (the pump mechanism is delivered here; the
language sugar and session wiring are follow-ons); per-key incremental reduce
state (P13).

---

## P6 — History: as-of, retention, GC (landed)

The store is an append-only edition log, so as-of reads already work; the
premise here **differs from DESIGN.md §4.1**, which imagined leaning on fjall's
MVCC snapshots + lazy compaction (and so "kept only the latest edition live,
no as-of below the compaction horizon"). Instead every update is a durable
`(edition, counter)` row we own, so as-of reads are exact *by construction* —
and retention becomes an **explicit consolidation watermark** we manage, not a
side effect of the KV engine's GC.

### Work

* **Consolidation watermark** (`grmpl-store`, persisted in `__meta` under
  `watermark`). `TraceStore::consolidate(up_to)` folds each relation's history
  at editions ≤ the new watermark (clamped to `current`) into a **checkpoint**
  — the consolidated `(tuple, diff)` state stored in the reserved `edition = 0`
  key range of the relation's own keyspace (disjoint from every real update,
  which starts at edition 1) — **deletes** the folded raw rows, and bumps the
  watermark, all in **one atomic `batch()`** (crash-safe: old horizon or new,
  never half-cut). Monotonic: a horizon at or below the current watermark is a
  no-op.
* **O(history) → O(checkpoint + tail).** An as-of `read_at(at)` is now
  `checkpoint + tail(watermark, at]`, and `scan_updates(from, ..)` is just the
  tail — the collapsed history is physically gone, not scanned. (Proven
  white-box by counting keyspace rows after a consolidate.)
* **The watermark as an ERROR at all four edition doors.** `read_at` *at* an
  edition below the watermark, and `scan_updates` *from* below it, return
  `Error::Store` — the intermediate state has been discarded, so they answer
  loudly rather than wrongly. The two computed doors, `grmpl-diff::eval_delta`
  and the reactive watch-cursor pump, inherit the guard for free (they bottom
  out at `read_at`/`scan_updates`). A store retaining full history reports
  `Edition::ZERO`, so no door ever closes.
* **Never past the minimum durable watch cursor** (`grmpl-proc::gc`). The store
  cannot see watch cursors (ordinary trace rows, above the bright line), so the
  GC *policy* lives above it: `min_watch_cursor` reads the least live cursor,
  and `consolidate_to` clamps the requested horizon to it before consolidating.
  This keeps every installed `on watch` pumpable; consolidating *past* a cursor
  (bypassing the policy) is exactly what trips its pump at the door.
* **`find q at E` with schema-at-edition.** A `Snapshot` pinned at `E` already
  evaluates `find q at E`; it now also reports the column schema *in force at
  E* via `Snapshot::schema` (P1 `schema_at`), under the same watermark floor —
  an as-of read sees the typing of its own era.

### Acceptance

`crates/grmpl-store/tests/history.rs`: a randomized-churn law oracle (32 seeds)
interleaves random commits with random consolidations and, after every step,
checks against an independent full-history model that every `read_at`/
`scan_updates` at or above the watermark is byte-identical to un-GC'd history,
that both doors **error** below the watermark, and that the watermark is
monotonic and ≤ `current`; plus corner tests for reopen durability (watermark +
checkpoint persist) and the clamp/no-op cases. A white-box unit test counts
keyspace rows to prove `consolidate` truncates to checkpoint + tail.
`crates/grmpl-proc/tests/gc.rs`: `consolidate_to` never passes the minimum
durable watch cursor and keeps watches pumpable; consolidating past a cursor
trips the edition door; a randomized oracle (24 seeds) confirms the clamp over
arbitrary cursor configurations. `crates/grmpl-diff/tests/find_at.rs`: `find q
at E` and `Snapshot::schema` are both as-of the pinned edition and both survive
consolidation / error below the watermark.

### Not in this phase

Replay & forks over the checkpoints (P10 — a checkpoint is a fork point, but the
fork/replay machinery is deferred); a background GC scheduler / retention policy
driver (the mechanism and the safe `consolidate_to` entry point are here; *when*
to call them is an operational concern); per-relation retention windows.

---

## Later phases

Sequenced from the backlog; each builds on P0/P1's stable formats. See the
corresponding tickets for detail.

* **P7 — Core IR** (CBPV split reified).
* **P8 — Typing:** value/row types (P8a, landed — `grmpl-type::check_query`),
  effect rows + relation-level Authority check (P8b, landed —
  `grmpl-type::effect`: infer an `on`-handler's write set and check it against a
  process `Authority` at relation granularity; key-ranges stay checked at
  commit), CALM (P8c).
* **P9 — Pattern algebra:** inputs, printing, streams.
* **P10 — Replay & forks.**
* **P11 — Concatenative surface.**
* **P12 — Behaviors as relations** (live code, landed — the defining MOO
  capability). A behavior is ordinary data: `grmpl_core::Value::Code` (opaque
  serialized P7 IR; `Ty::Code`; `wire::FORMAT_VERSION` bumped 2→3, store record
  reuses `wire` so it persists unchanged). `grmpl_lang::behavior::StoredBehavior`
  is a message-pattern guard (the reified `PredExpr` — the ticket's "guards
  restricted to the P7 reified Pred language") plus a point-free `Word` body run
  by the *same* interpreter as an `on`-handler arm; its codec rides the shared
  version byte in a third IR tag namespace. Dispatch is a query: `implements_ir`
  is the recursive `implements(entity, behavior)` view (`idea.md` §3), and
  `select_behavior` picks the least matching behavior — so redefinition is an
  ordinary `Patch` and the next dispatch follows (the live-code law). Committing
  a behavior re-runs the P8b effect/authority check at the commit boundary via
  the core `BehaviorChecker` hook (`grmpl_type::EffectChecker`, wired through
  `grmpl_proc::commit_patch_checked`; `commit_patch` = the `NoBehaviorCheck`
  variant). Law oracles: behavior-codec round-trip, dispatch-equals-model under
  churn, and commit-boundary-recheck ⇔ static verdict + runtime soundness.
* **P13 — Benchmarks,** then engine statefulness.
* **P14 — Diff generalization** (abelian groups).
* **P15 — Distribution.**
