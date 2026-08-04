# World packages and typed behaviors

World packages own initial state, bounded scalar computation, and static actor
scheduling. Use `Runtime::load_package` for passive packages and
`Runtime::load_driven_package` when the package declares actors.

## Package shape

```grmpl
package example bootstrap 1

entity WORLD = 1

rel entity_seq(next: Int)
rel rng_state(owner: Ent, state: Int)
rel result(thing: Ent, roll: Int, score: Float)

requires allocate entities(counter: entity_seq, first: 1000, last: 1999)
requires random rolls(
    state: rng_state,
    owner: WORLD,
    algorithm: xorshift64star_v1
)

bootstrap {
    entity_seq(1000)
    rng_state(WORLD, 1)
}
```

Bootstrap facts and a package-private marker commit together as edition 1.
Opening the exact package is an edition-preserving no-op. A changed or corrupt
marker, or an unmarked nonzero store, fails closed. v4 is a fresh-store cutover;
there is no v3 decoder or migration command.

Compilation checks entity uniqueness, schemas, duplicate facts, allocator
ranges and exact seeds, nonzero RNG seeds, and a canonical SHA-256 digest that
is independent of physical relation IDs and declaration order.

## Expressions and control flow

```grmpl
let damage = max(1, power - guard / 2)
let remaining = max(0, hp - damage)
let ratio = float(remaining) / float(max_hp)

if remaining == 0 {
    assert defeated(target)
} else {
    assert health_ratio(target, ratio)
}
```

`Int` arithmetic is checked. Overflow, division by zero, and remainder by zero
are deterministic behavior faults. `Float` is finite IEEE-754 binary64 with
canonical positive zero; NaN and infinities are rejected at parse, decode, and
evaluation boundaries. Mixed `Int`/`Float` arithmetic is not implicit—use
`float(int_expression)`.

Named and concatenative surfaces type-check into `BehaviorIr`. Stored code
serializes that same IR, so execution, effect inference, and the code-value
codec do not maintain separate interpreters.

## Granted stateful primitives

```grmpl
fresh entities as thing
random rolls below 100 as roll
```

The host must provide an exact matching grant. Allocation reads a seeded
counter from the behavior snapshot, checks its inclusive range, and seals one
expect/retract/assert transition into the result patch. Scalar RNG uses the
versioned `xorshift64star_v1` transition and unbiased rejection sampling; all
rejected draws advance only in-frame and the final state is sealed once.

Replay from the same snapshot/message therefore produces the same patch. An
optimistic loser commits neither dependent facts nor state consumption and
rebuilds from the winner's successor state. Capability effects include the
symbolic grant and its state relation write for authority analysis.

## Static actors and scheduling

```grmpl
requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)

authority patrol_writes {
    write cursor
    write located
    write timers
}

actor CAT { inbox patrol_inbox cursor cursor authority patrol_writes }

schedule world_clock at next_due send Tick() to CAT
```

Actors are source-declared entity constants. Each actor has one typed inbox,
one cursor, one bootstrap sequence counter, and one named authority request.
The host supplies a `RuntimePolicy` with stable-name `NamedAuthority` scopes and
a schedule grant whose target allowlist caps the actors source may address.

Scheduled constructors are inverted through the target actor's `form`; the
durable timer body is the raw token tuple that handler parses. Phase 2 admits
only `Text` constructor arguments. Timer delivery and ordinary command enqueue
share one actor-keyed sequence counter.

The host records nondecreasing simulation samples with `record_sample`, then
calls `drive_to_idle` or `drive_with_fuel`. The driver is timer-first, orders
ready actors by `(inbox RelId, entity, sequence)`, and commits one unit per
iteration. The default budget is 1,024 units. `DriveStatus` distinguishes idle,
fuel exhaustion, and a sticky actor fault whose cursor did not advance.

## Current world boundary

`worlds/moo.grmpl` and `worlds/shotengai.grmpl` own their initial facts and
finite content tables. Manor `create`/`dig` use granted source allocation.
Shotengai player combat uses typed expressions, `omen` uses committed scalar
RNG, and its cat/combat continuations are package actors. Native presentation,
player-login lifecycle, DSP dungeon instancing, whole-world forks/routing, and
the cribbage shuffle remain phase-3 capabilities.
