# Actor scheduling implementation guide

## Status

Implemented, 2026-08-04.

The accepted implementation lives in `grmpl-proc` scheduling/process
primitives, `grmpl-lang` package and behavior IR, `grmpl-type` effect checking,
and `grmpl::Runtime`. Focused law tests are
`grmpl-proc/tests/schedule.rs`, `grmpl-lang/tests/actor_package.rs`,
`grmpl-session/tests/actor_scheduling.rs`, and the schedule cases in
`grmpl-type/tests/{effect,behavior_commit}.rs`. Shotengai's package actors and
CLI regressions are the product tracer bullets.

This guide turns
[`world-package-phase-2-durable-actors.md`](world-package-phase-2-durable-actors.md)
into concrete delivery slices. It is intentionally source-backed: the timer,
sequence, clock, process, package, behavior-IR, and authority mechanisms named
here already exist unless a section is explicitly marked **to add** or
**to change**.

Phase 1 remains the foundation. This phase must not introduce another behavior
interpreter, another inbox layout, live wall-clock reads inside behavior, or a
world-specific scheduler in `grmpl-cli`.

## Outcome

After this work a package can declare a fixed set of actors, schedule future
messages through a host grant, and be driven deterministically from committed
clock samples:

```rust
runtime.record_sample(wall_ms, environmental_sample)?;
let report = runtime.drive_to_idle()?;
```

The first product proof is the Shotengai cat. Its package schedules its next
patrol message in the same patch as its movement. Recording time and driving
the runtime moves the cat without a player command. Restarting before or after
timer fire neither loses nor duplicates the move.

The second proof is multi-step combat. Player attacks schedule durable combat
messages; the combat actor owns counterattack calculation, the durable defeat
signal, victory claiming, and rewards. Closing and reopening at the
schedule/fire/actor-attention boundaries resumes from committed
inbox/cursor/timer state. The adapter still interprets the durable player-defeat
signal as a request to leave and clean up a DSP instance; store-topology effects
are phase 3.

## Decisions fixed for this tranche

The three phase-2 questions are closed for implementation:

1. **One logical driver per authority domain.** This is an operator invariant,
   not a durable lease protocol. The existing timer-row and sequence guards
   remain race-safe, but two independent actor drivers are unsupported because
   their successful interleaving is not canonical. Lease/epoch failover is
   deferred.
2. **Static package actors only.** Every driven actor is an `entity` constant in
   source. Transactional actor creation, dynamic actor registries, and
   multi-player actor discovery are deferred.
3. **A default drive has 1,024 units of fuel.** One successfully fired timer or
   one successfully processed actor message consumes one unit. A configurable
   entry point may use a different positive limit. Fuel exhaustion is a normal,
   visible incomplete result, never success and never deletion of pending work.

Additional fixed rules:

- The runtime has no background thread. Adapters explicitly record samples and
  call the driver.
- `wall_ms` from the latest committed clock sample is phase 2's simulation
  `now`. Samples must be nondecreasing. The third clock column remains an
  environmental sample for compatibility; scheduling does not secretly use it
  in place of phase 1's granted RNG streams.
- Due timers have priority over ready actors. One timer or one actor message is
  committed per loop iteration, then readiness is recomputed.
- A deterministic actor fault stops the whole canonical drain at that actor.
  Its cursor remains unchanged, so the same call will encounter it again until
  code or data is repaired.
- Actor-to-actor delivery uses the scheduling capability, including immediate
  work scheduled at `0`. Ordinary `emit` is not retrofitted into an ordered
  actor-send primitive in this phase.

## Current foundation and exact seams

### Already landed

- `grmpl_core::Scheduled` carries `timers`, absolute `due`, target `inbox`,
  target `Entity`, and message `body`.
- `Patch::schedule` keeps a scheduled entry in the causal behavior patch.
- `commit_patch` and `Domain::commit` turn scheduled entries into timer facts
  and apply normal schema and authority checks before the same atomic commit.
- `Scheduler` reads tuple-sorted due timers and fires each by expecting and
  retracting the timer while asserting the inbox row and advancing `SeqAlloc`.
- `ClockDriver` commits `(seq, wall_ms, rand)` samples as ordinary data.
- `Process` reads `(actor, seq, body)` inbox rows, evaluates one behavior against
  a pinned snapshot, and commits effects with `(actor, next_seq)` cursor
  advancement.
- Phase 1's `BehaviorIr::InvokeCapability` is the stable extension envelope for
  schedule; no new behavior-IR framing tag is required.

### Sequence-key seam to change first

Normal `enqueue_seq` and `seed_seq` use this counter layout:

```text
inbox_seq(actor: Ent, next: Int)
```

`Scheduler` currently asks the same `SeqAlloc` abstraction for a different key,
`(inbox: Int, actor: Ent)`. A package using one relation for commands and timers
would therefore maintain two counters and could assign the same inbox sequence
twice.

Phase 2 standardizes the actor inbox invariant:

```text
one actor -> one declared inbox -> one counter keyed only by actor
```

Change `Scheduler::seq_key` to `[Value::Ent(target)]`. Update the P4 scheduling
tests to use `(actor, next)` and add a regression that interleaves normal
`enqueue_seq` with timer fire and observes one contiguous sequence. The timer
row still stores the inbox relation because it is the delivery route; it is not
part of the counter key.

This change is valid only because phase 2 rejects two actor declarations for
the same entity and gives each actor exactly one inbox.

### Message-construction seam

`Process` passes an inbox body through the declared `form`; it does not dispatch
an already-constructed `Tick()` tuple. Therefore this source:

```grmpl
schedule world_clock at due send Tick() to TAMA
```

must compile `Tick()` back to the raw token tuple accepted by TAMA's form. It
must not write `["Tick"]` directly.

The compiler selects the target actor's inbox handler and its form, finds one
invertible rule whose constructor is `Tick`, and renders the rule's pattern:

```grmpl
form command {
    "tick" -> Tick()
}
```

becomes the durable body `("tick",)`. Constructor arguments in this tranche
must be `Text`, matching the current form-binding type. If a rule is ambiguous,
contains a bind not recoverable from its constructor, repeats a constructor
argument, or has the wrong arity, package compilation fails. General typed form
payloads are not part of actor scheduling; durable relations should carry
typed combat/timer state when necessary.

## Package surface

The accepted first surface is:

```grmpl
package kasumi_shotengai bootstrap 1

entity TAMA = 30

rel clock(seq: Int, wall_ms: Int, random: Int)
rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel inbox_seq(process: Ent, next: Int)
rel cursor(process: Ent, pos: Int)

requires schedule world_clock(
    clock: clock,
    timers: timers,
    sequences: inbox_seq
)

authority patrol_writes {
    write located
    write cursor
    write timers
}

actor TAMA {
    inbox inbox
    cursor cursor
    authority patrol_writes
}
```

Inside a statement arm:

```grmpl
schedule world_clock at due send Tick() to TAMA
```

There is no concatenative schedule word in the first slice. Both surfaces still
share `BehaviorIr`; concatenative syntax can be added later without changing
runtime semantics.

### AST and compiled model to add

Add these source-level forms in `grmpl-lang/src/ast.rs` and
`grmpl-lang/src/parser.rs`:

```text
Decl::RequireSchedule { name, clock, timers, sequences }
Decl::Authority { name, writes: Vec<relation_name> }
Decl::Actor { entity_constant, inbox, cursor, authority }
Stmt::Schedule { capability, due, tag, arguments, target_actor }
```

Compilation produces deterministic, name-sorted values:

```text
CapabilityRequirement::Schedule
  name
  clock relation name
  timer relation name
  sequence relation name

AuthorityRequest
  name
  requested write relation names

CompiledActor
  source name
  entity
  inbox relation name + RelId
  cursor relation name + RelId
  authority request name
```

`Program` owns the actor registry required by behavior lowering and runtime
construction. `CompiledPackage` exposes actors and authority requests for host
policy resolution.

Schedule requirements join allocation and random requirements in the canonical
bootstrap digest because their durable clock/timer/sequence bindings are part
of the installed resource contract. Actor declarations and authority requests
are executable policy, like behavior bodies, and are excluded from the
bootstrap digest; changing them does not pretend to migrate initial data.

### Compile-time validation

Reject the package unless all of the following hold:

- actor names resolve to distinct entity constants;
- no entity appears in two actor declarations;
- every actor inbox has exact schema `(Ent, Int, Tuple)` and has an `on` handler;
- every actor cursor has exact schema `(Ent, Int)`;
- the schedule clock is `(Int, Int, Int)`;
- the timer relation is `(Int, Int, Ent, Tuple)`;
- the sequence relation is `(Ent, Int)`;
- the named authority exists and requests the actor cursor relation;
- every relation in an authority request is declared;
- each schedule statement names a schedule capability and a declared actor;
- `due` is `Int`;
- the target form constructor is uniquely renderable and every supplied
  argument is `Text`; and
- every static actor has exactly one non-negative bootstrap sequence row. Existing
  bootstrap inbox messages, if any, occupy the contiguous range
  `0..next_sequence`; duplicates, holes, negative sequences, or a cursor beyond
  `next_sequence` are package errors.

The clock and timer relations may start empty. A package can start an actor by
bootstrapping an inbox message and setting its sequence counter to the following
value, for example:

```grmpl
bootstrap {
    inbox(TAMA, 0, ("start",))
    inbox_seq(TAMA, 1)
}
```

## Grants and authority

Requirements still do not grant power. Add a schedule grant with exact stable
relation names and an allowed target-actor set:

```rust
GrantSet::new()
    .grant_schedule(
        "world_clock",
        "clock",
        "timers",
        "inbox_seq",
        ["TAMA", "COMBAT"],
    )?
```

Grant resolution produces:

```text
ResolvedCapabilityGrant::Schedule
  clock: RelId
  timers: RelId
  sequences: RelId
  targets: actor source name -> (entity, inbox RelId)
```

Extra host targets are not visible to the package. Every statically used target
must be in the host grant. Stored code is rechecked: a schedule invocation must
have a literal actor symbol in its stable capability envelope, and
`EffectChecker::with_grants` rejects malformed or ungranted targets before the
code value commits.

Schedule execution adds:

- the symbolic schedule capability to `EffectRow.capabilities`; and
- the resolved timer relation to `EffectRow.writes`.

It does not add the clock, sequence, or target inbox to the actor's write set.
The actor writes a timer. The driver later reads the clock and atomically writes
the timer retraction, sequence advance, and inbox delivery under separate host
driver authority.

### Actor and driver policy to add

Introduce a nonbreaking driven-package entry point:

```rust
Runtime::load_driven_package(store, source, rel_base, &RuntimePolicy)
```

`RuntimePolicy` contains:

```text
domain: DomainId
capabilities: GrantSet
actor grants: actor name -> named relation scopes
driver grant: named relation scopes
default fuel: 1_024
```

A named scope refers to a stable relation and is resolved only after package
catalog compilation. It may grant the whole relation or an `Int`/`Ent` column
range, preserving the existing `Authority` model. The host grant may be
narrower than the source's relation-level request. For TAMA a useful policy is:

```text
located where thing == TAMA
cursor  where process == TAMA
timers  where target == TAMA
```

The runtime verifies that every requested relation has at least one host scope,
then runs `check_handler_authority` using the resolved authority. Concrete range
violations remain guarded at commit.

The driver grant must cover:

- the schedule clock relation for sample appends;
- timer rows for fire-time retraction;
- the sequence relation for every static actor key; and
- each target inbox slice written by timer fire.

Missing actor or driver authority fails package loading, before any actor is
driven. Package source cannot widen either authority.

## Behavior-IR execution

Do not add a `BehaviorOp::Schedule` framing tag. Lower the source statement to
the existing stable capability envelope:

```text
InvokeCapability
  capability = "world_clock"
  arguments = [literal actor symbol, due, rendered token 0, ...]
  destinations = []
```

Extend the per-invocation capability frame with a list of `Scheduled` values.
Invoking a resolved schedule grant:

1. validates the literal actor symbol against the resolved target map;
2. validates `due` as `Int` and the rendered body as the target form's token
   shape;
3. constructs `Scheduled { timers, due, inbox, target, body }`; and
4. appends it to the frame.

Frame sealing calls `patch.schedule` for each value in source order. A behavior
fault before sealing returns no patch. Commit-boundary authority and schema
checks cover the resulting timer facts exactly as they do for programmatic
`Patch::schedule` today.

## Driver API and algorithm

### Public result

Add:

```rust
pub struct DriveReport {
    pub status: DriveStatus,
    pub committed: usize,
    pub timers_fired: usize,
    pub actor_steps: usize,
}

pub enum DriveStatus {
    Idle,
    FuelExhausted,
    ActorFault {
        actor: Entity,
        sequence: i64,
        message: String,
    },
}
```

Store, schema, codec, and authority failures remain `Err`. Deterministic
behavior faults are data in `DriveStatus` because they are an expected stopped
state with durable pending work.

Expose:

```rust
Runtime::record_sample(wall_ms, environmental_sample)
Runtime::drive_to_idle()                    // fuel = 1_024
Runtime::drive_with_fuel(NonZeroUsize)
```

`record_sample` rejects a wall time lower than the latest committed sample.
It only commits data; it does not implicitly drive.

### Lower-level changes

Refine, rather than replace, existing primitives:

- `Scheduler::fire_next_due(now)` processes at most the least live due timer
  and reports `Fired`, `NoneDue`, or `Contended`.
- Existing `fire_due` can loop over `fire_next_due` for compatibility.
- A live malformed timer row is a codec error, not silently ignored.
- `Process::next_pending_sequence` performs only readiness inspection; it must
  not execute behavior while the driver is choosing an actor.
- `Process::step_retrying` processes at most one message, rebuilding after each
  optimistic rejection under the existing `Backoff` policy.

Do not use `Process::run_to_idle_retrying` inside the domain driver: draining
one actor completely would make actor order depend on which actor was chosen
first and could monopolize the runtime.

### Canonical loop

At each iteration:

1. If fuel is zero, return `FuelExhausted`.
2. Read the latest committed clock sample. If one exists, ask the scheduler to
   fire the least timer with `due <= wall_ms`.
3. If a timer commits, consume one fuel, increment `timers_fired`, and restart
   from step 1.
4. If timer contention did not settle, return a visible store/contention error;
   do not report idle.
5. Inspect all static actors without evaluating them. Ready actors are ordered
   by `(inbox RelId, actor Entity, next sequence)`.
6. Run exactly one message for the least ready actor, with normal retry.
7. On commit, consume one fuel, increment `actor_steps`, and restart.
8. On deterministic fault, return `ActorFault` with the cursor unchanged.
9. If there is no due timer and no ready actor, return `Idle`.

Future timers do not prevent `Idle`; they become runnable only after a later
committed clock sample. Messages for undeclared/dynamic actors are durable but
outside this driver's phase-2 registry.

This loop is deterministic because timer reads are tuple-sorted, actor order is
explicit, each behavior reads a pinned edition, and only committed samples
provide time. Recomputing after every commit is intentional: that commit may
have scheduled an immediate timer or made an earlier actor ready.

## Restart and failure behavior

No separate scheduler checkpoint is needed:

- Before schedule commit: neither causal effects nor timer exist.
- After schedule commit: both causal effects and timer exist.
- Before timer-fire commit: timer still exists and inbox does not.
- After timer-fire commit: timer is gone, inbox row and sequence advance exist.
- Before actor commit: inbox row remains at the cursor.
- After actor commit: effects and cursor advance exist together.

Consequently `Runtime::load_driven_package` reconstructs static `Process`
values from package declarations and host policy; it does not recover an
in-memory run queue. `drive_to_idle` derives all readiness from timer, inbox,
cursor, sequence, and clock relations.

Do not persist `DriveReport`, fuel, backoff state, or a list of ready actors.
They are observations/control bounds, not world state.

## Delivery slices

Each slice is independently testable and keeps existing APIs working.

### 1. Converge the scheduling substrate

Files:

- `crates/grmpl-proc/src/schedule.rs`
- `crates/grmpl-proc/src/process.rs`
- `crates/grmpl-proc/tests/schedule.rs`

Work:

- change timer delivery to actor-keyed sequence allocation;
- add command/timer shared-sequence regression coverage;
- expose one-timer fire with truthful contention/malformed outcomes;
- add one-message readiness/step helpers; and
- enforce nondecreasing clock samples.

This slice changes no `.grmpl` syntax.

### 2. Add package declarations and validation

Files:

- `crates/grmpl-lang/src/ast.rs`
- `crates/grmpl-lang/src/lexer.rs`
- `crates/grmpl-lang/src/parser.rs`
- `crates/grmpl-lang/src/compile.rs`
- `crates/grmpl-lang/src/package.rs`

Work:

- parse schedule requirement, authority, actor, and schedule statement;
- compile static actor and authority registries;
- validate canonical relation layouts and bootstrap actor sequence state;
- render scheduled constructors through target forms; and
- extend canonical requirement digest vectors.

### 3. Add schedule grants, effects, and execution

Files:

- `crates/grmpl-lang/src/package.rs`
- `crates/grmpl-lang/src/behavior_ir.rs`
- `crates/grmpl-type/src/effect.rs`
- stored-behavior codec/effect tests

Work:

- resolve schedule grants and allowed targets;
- collect scheduled values in the capability frame;
- infer timer writes and capability use through branches; and
- reject unknown/ungranted schedule symbols and targets at compile, stored-code
  commit, and execution boundaries.

### 4. Build the public canonical driver

Files:

- `crates/grmpl-session/src/runtime.rs`
- new `crates/grmpl-session/tests/actor_scheduling.rs`

Work:

- add named host policy resolution;
- construct static processes at package load;
- expose sample and bounded-drive APIs;
- implement canonical timer-first ordering and reports; and
- reconstruct entirely from durable data after reopen.

### 5. Port the Shotengai patrol

Files:

- `worlds/shotengai.grmpl`
- `crates/grmpl-cli/src/shotengai.rs`

Use a source relation for the next absolute patrol due point. A bootstrap
`StartPatrol()` message arms the first timer; `Tick()` moves TAMA, advances the
next-due fact, and schedules the next `Tick()` in the same patch. The adapter
records time and calls the runtime driver. Delete `Game::tick_patrol` and its
per-command invocation only after input-independent and reopen tests pass.

### 6. Port multi-step combat

Represent combat continuation as durable relation state plus immediate
scheduled messages to a static combat actor. Move, in order:

1. surviving-monster counterattack;
2. player defeat calculation and durable signalling;
3. victory/reward claim; and
4. job/ability advancement.

Consuming the player-defeat signal to perform DSP cleanup remains in the
adapter until phase 3 gives packages authority over store topology.

Keep the existing guarded one-shot monster reward row. Each transition consumes
the previous combat-state row and produces either the next scheduled step or a
terminal state in one patch. Close/reopen tests must pass between every pair of
steps before deleting `resolve_combat`, `award_victory`, and `counterattack`
from the adapter.

### 7. Cleanup and documentation

- remove adapter-injected patrol ticks and combat continuation;
- update `LANGUAGE.md`, `WORLD-PACKAGES.md`, and runtime capability inventory;
- mark the phase-2 design implemented only after all exit criteria pass; and
- leave dynamic actors, driver leases, DSP/forks, shuffling, and presentation
  in phase 3 or later.

## Verification matrix

### Substrate laws

- **Shared sequence:** command enqueue and timer fire allocate one contiguous
  actor sequence with no duplicates under contention.
- **Timer causality:** causal effects and scheduled timer share one edition.
- **Exactly-once fire:** two fire attempts produce one inbox row and one timer
  retraction.
- **Monotonic time:** a regressing clock sample is rejected without an edition.
- **Malformed timer:** bad live timer data fails closed rather than becoming
  invisible pending work.

### Language and policy laws

- parser and compiled-model round trips for every new declaration;
- declaration-order-independent actor/authority/requirement ordering;
- invalid actor schemas, duplicate entities, missing handlers, bad constructors,
  and bad bootstrap sequence state fail package compilation;
- missing schedule grants, disallowed targets, missing actor scopes, and missing
  driver scopes fail load;
- static effects include schedule plus timer write through both branches; and
- stored schedule code is rejected without both its grant/target and timer
  authority.

### Driver laws

- **Input independence:** committed time moves the patrol without player input.
- **Canonical order:** identical package/bootstrap/sample streams yield the
  same timer/actor edition sequence and final world.
- **Attention:** actor effects and cursor advance commit together.
- **Restart:** reopen before/after schedule, fire, and actor commit resumes once.
- **Fault:** deterministic behavior error identifies actor/sequence and leaves
  the cursor unchanged.
- **Fuel:** a self-scheduling immediate cycle returns `FuelExhausted` at exactly
  the requested unit count; the next call continues it.
- **Future idle:** a future timer with no ready actors returns `Idle` until time
  reaches its due value.

### Product regressions

- all existing manor and Shotengai tests remain green;
- patrol no longer depends on `Game::submit`;
- combat reward remains exactly once under stale/racing attempts;
- closing and reopening between combat messages changes no outcome; and
- repository search finds no `tick_patrol`, `resolve_combat`, `award_victory`,
  or `counterattack` adapter path after its corresponding port is accepted.

Repository gates:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets
git diff --check
```

## Explicit non-goals

- durable driver leases, epochs, or active/standby failover;
- dynamic actor spawn/retirement or discovery from arbitrary relation rows;
- multiple inboxes for one actor entity;
- parallel actor execution or a promise that multiple drivers have canonical
  interleaving;
- cron expressions, cancellation handles, mutable timers, relative-time syntax,
  or direct OS-clock access from behavior;
- typed form payload redesign or a general tuple-construction language;
- automatic watch pumping as part of the actor driver;
- DSP instancing, whole-world fork workflows, branch retirement, card shuffle,
  or structured presentation.

Those exclusions preserve a narrow phase: package-declared actors plus durable
time/message driving over mechanisms that already satisfy the core atomicity
and replay laws.
