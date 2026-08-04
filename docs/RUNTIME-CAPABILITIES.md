# grmpl runtime capabilities for MOO worlds

## Resulting surface

The supported product surface is one Rust library crate, `grmpl`, and one
product binary, `grmpl`:

```text
grmpl run [WORLD.grmpl] [STORE_DIR]
grmpl serve [WORLD.grmpl] [STORE_DIR] [ADDR]
grmpl showcase
```

`run` and `serve` are adapters over the same `grmpl::Runtime` and built-in
`grmpl::MooRuntime`. They compile through the durable catalog, register the
program's schemas, seed the same world package, allocate durable inbox
sequences, instantiate the same behaviors, and use the same retry policy.
`grmpld` has been removed. The semantic crates (`grmpl-core`, `grmpl-diff`,
`grmpl-proc`, `grmpl-lang`, `grmpl-pattern`, and `grmpl-ent`) remain internal
modules with intentionally narrow interfaces; merging them would reduce
locality without reducing the public surface.

The Kasumi Shotengai prototype is deliberately outside this migration. Its
working files are not part of this change.

## Runtime architecture

`grmpl::Runtime` is the public deep module. It owns the store/program binding
and exposes relation resolution, current views, process construction,
race-safe command enqueueing, and source-declared watch installation.
`grmpl::MooRuntime` is a world package over that general runtime: it resolves
the MOO relation set, bootstraps the built-in manor, and supplies the small set
of native capabilities the language cannot yet express. `grmpl::Server` adds
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

These operations lower to ordinary patches and queries. There is no separate
TCP implementation of them.

### Implemented as native Rust capabilities

Some current MOO behavior is native because the language has no expression for
the operation, not because the transport owns a second world:

- Opening the Ent store, compiling a program through its durable name catalog,
  and registering versioned schemas.
- Initial manor/template facts and finite cribbage rule tables.
- Durable entity allocation and atomic player provisioning.
- `dig` and `create`, which require fresh entity IDs.
- Formatted `look` output and adapter-specific presentation of query rows.
- Driving player/NPC processes and retrying rejected optimistic commits.
- TCP accept/read/write and terminal input/output.
- Card shuffling/dealing. The terminal adapter currently owns its deterministic
  process-local PRNG state.
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

## What Shotengai still needs from grmpl

The excluded Shotengai prototype demonstrates that the substrate is already
capable of the world, but too much orchestration must currently be written in a
world-specific Rust adapter. A clean port should add the following capabilities
to the public world model, in leverage order.

1. **Transactional expressions and control flow.** Add integer arithmetic,
   comparisons, boolean conditions, and conditional branches to behavior
   bodies. Shotengai currently works around their absence with finite relations
   for damage, progression, card totals, and other calculations. Tables are
   useful world data, but they should not be the only way to express bounded
   arithmetic or choose an effect.

2. **Granted deterministic primitives.** Add capability-checked behavior
   primitives for fresh entity allocation and committed RNG consumption. Both
   must participate in the behavior's guarded patch: allocation must seal the
   entity counter, and RNG must expect/retract/assert its durable state in the
   same commit. This would move player-created objects, dungeon IDs, combat
   rolls, and card deals out of bespoke Rust coordinators without introducing
   ambient nondeterminism.

3. **World bootstrap in the package.** Add source-level fact literals or a
   declarative bootstrap section with an idempotent installation contract.
   Initial places, people, jobs, abilities, monsters, dungeon templates, and
   finite rule tables currently require a large Rust seeder even though they are
   ordinary relation data.

4. **Transactional substrate effects.** Expose DSP template instancing and
   whole-world fork/routing as explicit, authority-scoped effects with defined
   commit boundaries. Shotengai's basement and Copying Mirror currently call
   `instance_template`, `fork_at`, branch routing, cleanup, and lineage APIs
   directly from Rust. A port needs durable instance allocation/retirement and
   branch selection to compose with the command that requested them. Branch
   quotas or retirement policy are also needed before world-copying is public.

5. **Durable actor scheduling.** Surface the existing scheduling machinery in
   `.grmpl` so NPC patrols, job clocks, and multi-step combat can advance from
   committed time/events rather than an adapter calling `tick` after terminal
   input. Ordering and retry behavior must remain replay-deterministic.

6. **Presentation values.** Add basic text interpolation/list rendering or a
   structured response value. This is lower leverage than transactional
   capabilities—terminal and TCP can legitimately render differently—but it is
   needed before rich room, combat, job, lineage, and card output can leave the
   Shotengai Rust adapter completely.

Arithmetic/control flow, allocation/RNG, and bootstrap are the minimum useful
language tranche. DSP/fork effects should follow as carefully scoped runtime
capabilities rather than general host escape hatches. With those pieces,
Shotengai can become another world package over `grmpl::Runtime` instead of a
new runtime or binary.
