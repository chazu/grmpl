# World-package roadmap

## Status

Three-phase plan, updated 2026-08-04. Phases 1 and 2 are implemented; phase 3
remains design-scoped work.

This roadmap expands the six remaining-work items on the final slide of
[`unified-runtime-deck.md`](unified-runtime-deck.md). The previous single design
mixed language work, autonomous runtime work, and hazardous store-topology work
into one apparent project. They are now three separately useful phases with
separate design and acceptance contracts.

## Goal

Make Kasumi Shotengai an ordinary package on `grmpl::Runtime`, without creating
another engine or pretending every native capability must move into the
language at once.

The boundary remains the one in [`CONTEXT.md`](../CONTEXT.md):

- portable deterministic rules and initial world data belong in `.grmpl`;
- native powers are explicit, narrow capabilities granted by the runtime; and
- store driving, transport I/O, and final rendering stay in Rust adapters.

## The three phases

| Phase | Status | Final-slide items | Useful result | Explicitly deferred |
| --- | --- | --- | --- | --- |
| [1. Language and package foundation](design/world-package-phase-1-language-and-bootstrap.md) | Implemented | Arithmetic/control flow; committed allocation/RNG; bootstrap facts | Worlds install their own data and express bounded transactional game rules, fresh entities, and scalar random choices | Autonomous clocks, instancing, forks, structured presentation, general collection/shuffle operations |
| [2. Durable actors](design/world-package-phase-2-durable-actors.md) | Implemented | Durable actor scheduling | NPCs and multi-step rules advance from committed time/events rather than adapter-injected ticks | Store-topology effects and rich presentation |
| [3. Full Shotengai cutover](design/world-package-phase-3-full-cutover.md) | Deferred | Authority-scoped DSP/forks; richer presentation | Basement instances, Copying Mirror, cards, and semantic responses leave the Shotengai CLI | General native FFI, unbounded public forks, a full UI framework |

The phases are ordered dependencies, not one release train:

```text
package + typed behavior + transactional primitives
                         |
                         v
              durable actor driving
                         |
                         v
       structural effects + responses + cutover
```

Each phase is independently valuable and leaves the later capabilities as
explicit native world capabilities rather than accidental adapter behavior.

## Phase 1: minimum useful language tranche

Phase 1 is implemented. Its full contract and acceptance laws are
[`world-package-phase-1-language-and-bootstrap.md`](design/world-package-phase-1-language-and-bootstrap.md).

It adds:

- one package declaration and versioned bootstrap block;
- stable source-level entity constants and typed fact literals;
- an atomic, idempotent installation marker;
- checked `Int` and finite binary64 `Float` expressions, comparisons, booleans,
  immutable bindings, and bounded `if` statements;
- named, host-granted entity allocation over the existing guarded `Alloc`;
- named, host-granted scalar RNG streams with specified persistent semantics;
- capability-aware static effects and commit-boundary stored-code checks; and
- a fresh-store v4 cutover, with no v3 migrator, for new float and serialized
  behavior-IR framing.

Its exit is intentionally narrower than “remove all Shotengai Rust.” It removed
native world seeding, arithmetic-table workarounds where formulas are the real
rule, and native MOO `create`/`dig`. It proves committed RNG with scalar draws.
The current whole-deck card shuffle remains native because scalar RNG plus
bounded conditionals is not a collection language.

## Phase 2: autonomous world runtime

Phase 2 is implemented. Its accepted contract is in
[`world-package-phase-2-durable-actors.md`](design/world-package-phase-2-durable-actors.md).
Its source-backed delivery guide is
[`world-package-phase-2-actor-scheduling-implementation-guide.md`](design/world-package-phase-2-actor-scheduling-implementation-guide.md).

It exposes the already-landed `Scheduled`, `Scheduler`, `ClockDriver`,
`SeqAlloc`, and `Process` mechanics through package actor/authority declarations
and one runtime drive contract. The first acceptance case is a patrol that moves
after committed simulation time advances without any player command.

This phase removes input-coupled `tick` injection and gives multi-step combat a
durable message-driven form. It does not grant access to Ent instancing or
branch creation.

## Phase 3: structural capabilities and final cutover

Phase 3 is scoped in
[`world-package-phase-3-full-cutover.md`](design/world-package-phase-3-full-cutover.md).

It contains the work with the most consequential storage and product policy:

- transactional DSP/template instancing inside one branch;
- recoverable, idempotent whole-world fork and route workflows;
- branch quotas and, before untrusted use, branch retirement;
- deterministic collection support sufficient for card dealing;
- structured semantic response values and common renderers; and
- deletion of the remaining world-specific Shotengai coordinator.

Instancing and forks remain separate designs inside the phase. Instancing can
plausibly become one guarded patch. Fork creation and root-route replacement
span branches and are explicitly a durable protocol, not a fictitious atomic
commit.

## Cross-phase invariants

Every phase preserves:

1. **Replay:** behavior depends only on its pinned snapshot and message;
   external time/entropy enters as committed data.
2. **Patch–edition:** ordinary world effects and their consumed counters/state
   commit as one edition or have no effect.
3. **Authority:** relation ownership and native capability grants are separate
   checks; package source cannot grant itself power.
4. **Determinism:** arithmetic, RNG, timer order, collection order, and response
   encoding have specified semantics and law tests.
5. **Bright line:** semantic interfaces do not name fjall, Ent internals,
   threads, TCP, or other implementation technology.
6. **Honest boundaries:** operations spanning branches expose their durable
   workflow rather than claiming single-edition atomicity.
7. **Bounded execution:** language control, collection materialization, and
   structural expansion are fuel/size bounded.

## Overall completion

The roadmap is complete when Shotengai runs through the same public runtime as
the manor and its CLI contains no world-specific fact seeding, gameplay
calculation, gameplay commits, RNG coordination, tick injection, Ent
instancing/fork calls, or semantic response formatting. Rust still opens and
drives the store, grants capabilities, transports messages, and renders the
world's structured responses.

Every phase uses the repository verification contract:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets
```
