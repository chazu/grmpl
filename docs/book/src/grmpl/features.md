# Features so far, and the Ent underneath them

grmpl's language and runtime were built as a sequence of shippable phases (P0
onward), each keeping the whole workspace green and each backed by a randomized
law-oracle test. This chapter tours what is implemented, and — the point of the
book — names the `Ent` property each feature rests on. The recurring pattern:
grmpl's semantics were designed around the enfilade even while the store was
still an LSM stand-in, so cutting over to `grmpl-ent` lit up the structure the
laws had assumed all along.

> The tour below is selective, not a changelog: it stops at the phases that
> illustrate an `Ent` property. The ones it skips are real but less
> illustrative — **P7** (the Core IR, a reified CBPV split), **P9** (the pattern
> algebra: inputs, printing, streams), **P10** (replay and forks), **P11** (the
> concatenative surface), **P13** (benchmarks and engine statefulness) and
> **P14** (diff generalization to abelian groups). `docs/ROADMAP.md` is the
> complete list. **P15** — distribution — is Part IV.

## The world model — relations, not objects

An object is an entity identifier participating in relations — `located`,
`held`, `named`, `prototype`, `exits` — never a mutable record behind an API
(the **Object law**). Inheritance and delegation are a recursive view:

```text
implements(entity, behavior) :=
    direct_behavior(entity, behavior)
  | prototype(entity, parent), implements(parent, behavior)
```

*Ent property:* facts are the leaves of the **Fact enfilade**; a view is a
standing query over it. Delegation is queryable, overridable, and *historically
versioned* because the facts it reads are.

## P0 — one versioned codec, durable catalog, determinism

A single value/tuple encoding (`grmpl-core::wire`) underlies both the message
wire and the on-disk record, every artifact prefixed with a `FORMAT_VERSION`
byte. The name→`RelId` catalog is append-only and durable. `read_at` is
tuple-sorted; `scan_updates` is commit-ordered.

*Ent property:* **one serialization** frames every enfilade node payload too, and
the determinism rules are exactly what let the Ent hash *logical content* (sort
before hash — no HashMap order, RNG, or pointer input) so structural sharing is
canonical and replay is exact.

## P1 — relation schemas, additive-only, as-of

Every relation may carry an ordered list of named, typed columns, enforced at the
commit boundary. Schemas evolve **additively only** (append columns, never
remove/reorder/retype) and are **versioned by the edition** they took effect, so
`schema_at` answers as-of queries.

*Ent property:* schemas live in the **Context enfilade** — inherited context,
versioned along history. An as-of read at edition E sees the typing in force *at
E*, because history is retained, not overwritten.

## P2 — reduce / aggregates

A `Reduce` operator groups by key and folds each group (`Count`/`Sum`/`Min`/
`Max`), so views yield derived measures. Its delta is a boundary recompute
(`reduce@to − reduce@from`), maintained differentially.

*Ent property:* the seed of the **Derived enfilade** — maintained query state
that is `find`'s derivative. Grouping is a fold, the same monoidal shape as a WID
measure. That enfilade is now persistent: a materialized view is an ordinary
relation in the Fact enfilade, so it survives a reopen and is carried by a fork.

## P3 — client sessions and world construction

The world becomes reachable from clients: connect, become a player, `dig` /
`create` / `go` / `take` / `look`. Every verb is an ordinary guarded `Patch`;
world-building shares the exact commit machinery as `take lamp`, with no
privileged path. Entity allocation is replay-safe — the id read and counter bump
ride in the *same* patch as the effects that use them.

*Ent property:* construction and play are the *same* operation — allocate a new
edition atomically or have no effect (**Patch–edition law**). `take`'s
precondition is the optimistic race point; two players racing the lamp resolve to
exactly one winner.

## P4 — scheduling and simulation time

A patch can schedule a future message; simulation time and randomness enter the
world **only as committed data**, via a single sanctioned clock/randomness driver
that commits `(seq, wall_ms, rand)` rows. Timers fire by a `commit_if`
preconditioned on the timer row — the exactly-once guard.

*Ent property:* the **Replay law** made structural. Because the three times stay
separate and nondeterminism is *data in the Edition enfilade*, replaying the same
samples reproduces the identical world — the precondition for time travel and
forking.

## P5 — reactive handlers (`on watch`)

`on watch <view>` binds a maintained view to a Process's inbox: signed deltas are
pumped in as messages, in one atomic commit that also advances a durable
watch-cursor. Cascades are async message chains, never reentrancy — the pump only
*appends*.

*Ent property:* the **Attention law** and the **Canopy**. `watch` is the
maintained derivative of `find` (Snapshot–stream law); the pump's `eval_delta`
over `[cursor, current)` is a differential read of retained history. The pump no
longer re-evaluates blindly: it asks the substrate whether the interval could
have touched it, and on the Ent that question is answered by a measure — relation
-wide from the Edition enfilade, or per key span from the canopy itself.

## P6 — history: as-of, retention, GC

As-of reads are exact by construction because every update is a durable
`(edition, counter)` row. Retention is an explicit **consolidation watermark**:
`consolidate(up_to)` folds history at or below the watermark into a checkpoint,
deletes the folded rows, and bumps the watermark — one atomic batch. Reads below
the watermark *error* loudly rather than answer wrongly; GC never consolidates
past the least live watch cursor.

*Ent property:* "nothing is overwritten" made operational. On the Ent this is
reachability GC over the granfilade from retained roots — the same structural
sharing that makes history cheap makes bounded retention a matter of dropping
unreachable nodes.

## P8 — typing and effects

Value/row typing (`check_query`) and effect rows: an `on`-handler's write set is
inferred and checked against a process `Authority` at relation granularity, with
key-ranges still checked at the commit boundary.

*Ent property:* the **Authority law** — one commit, one authority domain. Effects
tell the runtime exactly what a handler touches, which is also what a future
cluster needs for placement.

## P12 — behaviors as relations (live code)

The defining MOO capability: **code is ordinary data**. A behavior is a
`Value::Code` (opaque serialized IR) stored in a relation; dispatch is the
recursive `implements(entity, behavior)` view, and `select_behavior` picks the
least matching behavior. So *redefining a behavior is an ordinary `Patch`*, and
the next dispatch follows it — the live-code law. Committing a behavior re-runs
the P8 effect/authority check at the commit boundary.

*Ent property:* the deepest expression of the **Object law** and of cheap
history. Code lives in the Fact enfilade like any other fact, is versioned like
any other fact, and can be rolled back, forked, or audited like any other fact.
Live programming *is* patching the world.

## What it feels like

All of this is observable in a real session against the built-in world
(`cargo run -p grmpl-cli -- run`):

```text
> watch
Now watching the world — changes will stream as [world] deltas.

> greet cat
You introduce yourself. You are now acquainted with Whiskers.
Whiskers pads out of the room.

> take lamp
Taken.
  [world] - brass lamp
```

`watch` installed a maintained query (Attention law, Canopy). `greet` and `take`
were guarded patches that allocated new editions (Patch–edition law). The
`[world] -` line is a signed delta from the maintained view (Snapshot–stream
law). The cat wandering off was a scheduled/reactive consequence, delivered as a
message (Authority law). Every line is one of the seven laws, resting on one of
the five enfilades.
