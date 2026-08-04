# Durable actors and runtime driving

## Status

Implemented, 2026-08-04. The concrete
delivery order, APIs, validation rules, and law tests are in
[`world-package-phase-2-actor-scheduling-implementation-guide.md`](world-package-phase-2-actor-scheduling-implementation-guide.md).

Parent roadmap:
[`WORLD-PACKAGE-REMAINING-WORK.md`](../WORLD-PACKAGE-REMAINING-WORK.md).

## Outcome

World actors advance from committed time and messages without an adapter
inventing domain events. A patrol moves when simulation time advances even if
no player enters a command. Multi-step combat and job clocks survive restart as
durable inbox/timer work.

## Existing foundation

The difficult persistence mechanics already exist:

- `Patch::schedule` records a `Scheduled` timer in the same commit as its cause.
- `Scheduler::fire_due` retracts a timer and appends its message atomically,
  guarded for exactly-once delivery.
- `SeqAlloc` assigns durable race-safe per-inbox sequence numbers.
- `ClockDriver` records external wall/random samples as replayable data.
- `Process` advances its inbox cursor in the same patch as behavior effects.

The package declaration and public runtime drive contract are now implemented.

## Package surface

Phase 1's host-capped requirement/grant model grows one capability:

```grmpl
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

Inside behavior, `schedule` lowers through phase 1's stable granted-capability
instruction and creates the existing semantic `Scheduled` value. It does not
read an OS clock:

```grmpl
schedule world_clock at due send Tick() to TAMA
```

Static actors use phase 1 entity constants. Dynamic actor creation is deferred
beyond phase 2.

Authority declarations are package requests, not self-grants. Runtime policy
caps their relation scopes and scheduling capabilities.

## Runtime drive contract

The public runtime separates input sampling from deterministic draining:

```text
record_sample(wall_ms, random_sample) -> committed sample
drive_to_idle() -> timers fired and actors processed at committed time
```

`drive_to_idle` repeatedly:

1. reads the latest committed simulation time;
2. fires due timers in canonical tuple order;
3. selects ready actors by `(inbox RelId, entity, next sequence)`;
4. runs one actor message through normal guarded commit/retry; and
5. repeats until quiescent or a configured fuel limit.

There is one logical sampler/actor driver per authority domain. The underlying
timer-fire primitive remains race-safe, but running redundant full domain
drivers is unsupported because phase 2 does not define their actor
interleaving. A deployment that needs failover must define driver ownership or
a durable lease before both run.

A stopped runtime advances nothing. On reopen, the next `drive_to_idle` sees
durable overdue timers and actor cursors and resumes them.

## Fault and fuel policy

- Optimistic rejection retries through existing `Backoff`.
- A deterministic behavior fault stops the actor with its cursor unchanged,
  using phase 1's fault contract.
- Fuel exhaustion returns a visible incomplete-drive result; pending durable
  work is not dropped or marked complete.
- A later call may continue the same drain.

## Shotengai tracer bullets

1. The cat handles `Tick`, moves, and schedules its next `Tick` in the same
   patch. Advancing committed time moves it without a player command.
2. Player attack emits/schedules the next combat step. Victory reward remains a
   guarded one-shot row and cannot be granted twice.
3. Close and reopen between combat steps; draining resumes exactly once from
   durable state.

The Shotengai adapter tick and combat coordinator paths have been removed; the
package cat and combat actors now own those continuations.

## Verification

- **Timer causality:** schedule and causal world effects share one edition.
- **Exactly-once fire:** racing timer workers deliver one inbox message.
- **Actor attention:** cursor and effects commit together across crash/restart.
- **Canonical replay:** one committed sample stream and canonical domain driver
  produce the same actor order and final world.
- **Input independence:** patrol movement requires no player command.
- **Fuel safety:** bounded drains leave all remaining work durable.

Repository gates remain `cargo build --workspace`, `cargo test --workspace`, and
`cargo clippy --all-targets`.

## Decisions for implementation

1. One logical driver is an operator invariant. Durable lease/epoch failover is
   deferred.
2. Static source-declared actors are sufficient for the Shotengai tracer
   bullets. Dynamic actor creation is deferred.
3. The default drive budget is 1,024 committed work units. One timer fire or one
   actor-message commit consumes one unit; exhaustion is a visible incomplete
   result.

The implementation guide also fixes timer-first canonical ordering, actor-keyed
inbox sequences shared with normal enqueue, monotonic committed clock samples,
and form-aware lowering of scheduled constructors.
