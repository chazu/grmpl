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

## Later phases

Sequenced from the backlog; each builds on P0's stable formats. See the
corresponding tickets for detail.

* **P1 — Relation schemas** (+ evolution minimum): arity/column types per
  relation; the language allocates stable ids via the durable catalog.
* **P2 — Reduce / aggregates.**
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
