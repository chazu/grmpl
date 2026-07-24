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
  `Query::Reduce`. This waits on P1's named columns; a full `view … yield
  count(…)` grammar is a follow-on (TKT filed).

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

### Start here

`crates/grmpl-diff/src/query.rs` (`Agg`, `Reduce`, `reduce_snapshot`,
`eval_delta`), `crates/grmpl-diff/src/recursive.rs` (`collect_rels`),
`crates/grmpl-core/src/error.rs` (`Error::Query`),
`crates/grmpl-lang/src/compile.rs` (`NamedAgg`, `reduce_view`).
Oracle template: `crates/grmpl-diff/tests/reduce_stream.rs`.

### Not in this phase

Per-key incremental aggregate state (P13), aggregates inside recursion, richer
aggregates (average/distinct-count/user-defined), and a parser grammar for
aggregate yields in `view` (follow-on ticket).

---

## Later phases

Sequenced from the backlog; each builds on P0/P1's stable formats. See the
corresponding tickets for detail.

* **P3 — Client sessions & world construction.**
* **P4 — Scheduling & simulation time.**
* **P5 — Reactive handlers (`on watch`).**
* **P6 — History:** as-of, retention, GC.
* **P7 — Core IR** (CBPV split reified).
* **P8 — Typing:** value/row types, effect rows, CALM.
* **P9 — Pattern algebra:** inputs, printing, streams.
* **P10 — Replay & forks.**
* **P11 — Concatenative surface.**
* **P12 — Behaviors as relations** (live code).
* **P13 — Benchmarks,** then engine statefulness.
* **P14 — Diff generalization** (abelian groups).
* **P15 — Distribution.**
