# grmpl runtime capabilities for MOO worlds

## Resulting surface

The supported product surface is one Rust library crate, `grmpl`, and one
product binary, `grmpl`:

```text
grmpl run [WORLD.grmpl] [STORE_DIR]
grmpl serve [WORLD.grmpl] [STORE_DIR] [ADDR]
grmpl shotengai [STORE_DIR]
grmpl showcase
```

`run` and `serve` are adapters over the same `grmpl::Runtime` and built-in
`grmpl::MooRuntime`. They load through the durable catalog, register the
program's schemas, atomically install source-owned bootstrap facts, allocate durable inbox
sequences, instantiate the same behaviors, and use the same retry policy.
`grmpld` has been removed. The semantic crates (`grmpl-core`, `grmpl-diff`,
`grmpl-proc`, `grmpl-lang`, `grmpl-pattern`, and `grmpl-ent`) remain internal
modules with intentionally narrow interfaces; merging them would reduce
locality without reducing the public surface.

Kasumi Shotengai uses the same v4 package compiler, atomic installer, and actor
driver. Its remaining native DSP, fork, shuffle, and presentation seams are the
explicit phase-3 boundary, not a second bootstrap/runtime path.

## Runtime architecture

`grmpl::Runtime` is the public deep module. It owns the store/program binding
and exposes relation resolution, current views, process construction,
race-safe command enqueueing, source-declared watch installation, committed
clock sampling, and bounded canonical actor draining.
`grmpl::MooRuntime` is a world package over that general runtime: it resolves
the MOO relation set and supplies presentation and player-provisioning seams.
`grmpl::Server` adds
durable player identity and sessions without choosing a transport.

```text
terminal (`grmpl run`) ─┐
TCP (`grmpl serve`) ────┼─> Server / MooRuntime ─> Runtime ─> semantic modules
embedded Rust ──────────┘
```

The key boundary is world lifecycle versus presentation. An adapter may read
views, render rows, and drive actors, but it does not independently compile a
program, invent relation IDs, declare schemas, seed sequence counters, or
construct a parallel set of world verbs.

## MOO capability inventory

### Implemented in `.grmpl`

The language-defined portion of `worlds/moo.grmpl` includes:

- Typed relation declarations for location, containment, exits, identity,
  NPCs, cards, process inboxes/cursors, allocation counters, and watches.
- Relational views using joins, projection, distinctness, parameter binding,
  and `count`, `sum`, `min`, and `max` reduction.
- Structural command forms that turn token streams into tagged messages.
- Statement and concatenative behavior bodies with deterministic `find` and
  `resolve`, plus guarded `expect`, `assert`, `retract`, and `emit` effects.
- MOO verbs `take`, `drop`, `go`, `say`, and `greet`.
- An autonomous `tick` behavior whose patrol policy is relation data.
- Inventory, location, exits, roster, treasure, identity-fog, cribbage, and
  whole-world views.
- A source-declared reactive world watch with durable pump cursor and sequence
  relations.
- Versioned package/entity/bootstrap declarations and explicit allocation/RNG
  requirements.
- Immutable `let`, checked `Int` and finite binary64 `Float` expressions,
  comparisons, short-circuit booleans, and `if`/`else`.
- `fresh` and scalar `random ... below ...`, with their durable state changes
  sealed into the same patch as dependent effects.
- Manor `create`/`dig`, Shotengai combat damage, and Shotengai's `omen` draw.
  Named, concatenative, and stored bodies execute one typed `BehaviorIr`.
- Static `authority`/`actor` declarations and granted `schedule` statements.
  Shotengai's cat patrol, counterattack, victory claim, and job advancement are
  durable actor messages rather than adapter-injected continuations.

These operations lower to ordinary patches and queries. There is no separate
TCP implementation of them.

### Implemented as native Rust capabilities

Some current MOO behavior is native because the language has no expression for
the operation, not because the transport owns a second world:

- Opening the Ent store, resolving host grants, and registering versioned schemas.
- Atomic player provisioning. Login remains a host lifecycle operation even
  though its allocator uses the durable counter contract.
- Formatted `look` output and adapter-specific presentation of query rows.
- Recording external time and calling the package's bounded actor driver.
- TCP accept/read/write and terminal input/output.
- Card shuffling/dealing. Sampling without replacement remains a phase-3
  collection/shuffle capability, distinct from scalar random draws.
- Private vault creation through Ent DSP relocation, instance selection, and
  teardown in the terminal adapter.

The first four items are world-runtime capabilities. The rest are presentation
or host capabilities. This distinction matters: moving a capability into
`MooRuntime` removes divergence between adapters; moving it into `.grmpl` makes
the world portable to any conforming runtime.

### Implemented below the language in reusable Rust modules

The semantic modules provide durable editions and history, atomic guarded
commits, authority checks, deterministic differential queries, incremental
watches, replay-safe counters, scheduling primitives, content-addressed Ent
storage, forks/branches, and DSP transforms. These are not MOO-specific, and a
world reaches the subset exposed by `Runtime` or by a deliberately granted
native capability.

## Verification of the unified path

Acceptance tests exercise the public runtime against the Ent substrate and
through real loopback TCP. They cover durable reconnect identity, concurrent
entity allocation, contested takes with one winner and no silent loser,
language-defined movement, dynamic world construction, deterministic replay,
fork checkpoints, schema-aware commits, and exactly-once/in-order reactive
materialization with durable delivery resume.

## What remains after phase 2

Shotengai now uses package bootstrap, typed combat expressions, committed
scalar RNG, and durable scheduled actors. The remaining adapter work is
intentionally bounded. The roadmap and transaction laws are in
[WORLD-PACKAGE-REMAINING-WORK.md](WORLD-PACKAGE-REMAINING-WORK.md).

- Phase 3: DSP template instancing/retirement, whole-world fork and branch
  routing, deterministic collection/shuffle semantics, and richer presentation
  values.

The v4 tranche deliberately has no v3 migrator. Existing v3 stores remain
untouched and require a matching v3 build; v4 packages install only into fresh
stores.
