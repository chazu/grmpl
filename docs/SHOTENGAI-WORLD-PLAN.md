# Kasumi Shotengai world plan

## Goal

Build a second, durable MOO-like world on grmpl, set in a fictional old
Japanese shotengai. It must retain every demonstrated capability of the manor
MOO while adding a combat dungeon, monsters, a JRPG-like job system, and a
final artifact that forks the complete world and moves the player onto the new
branch.

The manor remains the small language showcase. Kasumi Shotengai is the larger
game and the acceptance case for durable world branching.

## Product contract

### Setting

Kasumi Shotengai is an aging covered shopping arcade at dusk. Its surface map
contains the east gate, central arcade, Tsukikage Kissaten, a repair shop, a
sento, the shopping-street cooperative office, a shrine alley, a shuttered
cinema, and a rooftop public-address room. A freight lift below the cinema
opens onto a private basement dungeon.

The dungeon is a relocatable template with service corridors, flooded storage,
a forgotten underground arcade, a ledger vault, monsters, loot, a boss, and the
Copying Mirror. The monsters are fictional remnants of the arcade: receipt
moths, shutter maws, mannequin shells, and the Last Customer.

### Manor feature parity

The shotengai must provide equivalents for all current playable MOO features:

| Manor capability | Shotengai capability |
| --- | --- |
| `look`, `go`, `take`, `drop`, `inventory`, `say` | The same base verbs |
| Fog of identity and `greet` | Residents reveal their names when greeted |
| Autonomous cat patrol | An autonomous arcade patrol |
| `who` | Current player roster |
| `treasure` aggregate | Merchandise value per room |
| Observatory live report | Rooftop PA room with live world queries |
| Cribbage rule-table scoring | A playable card table in the kissaten |
| Reactive `watch` stream | Exactly-once world-change stream |
| DSP private vault | A DSP-instanced basement dungeon |
| Durable history and replay | Unchanged substrate laws |

### Jobs and abilities

The initial job roster is Courier, Cook, Repairer, Shrine Attendant, and Night
Watch. A player has one active job, but every job retains its own rank and job
points when the player changes jobs. The cooperative office's time clock is the
in-world job-change point.

Only the active job receives job points. Each job has a signature ability whose
ability rank grows with that job's rank. Ability progress remains available
after switching jobs, providing the cross-job progression loop requested by the
game design. The first implementation has three ranks per job and data-driven
stats and ability power.

### Combat

Combat is deterministic and turn-based. `attack <monster>` and
`use <ability>` produce one guarded player attack followed, if the monster
survives, by one monster counterattack. A defeated monster awards job points
exactly once. Player defeat returns the player to the sento with job progress
intact.

The player patch schedules an immediate `CombatStep()` for the static combat
actor. Counterattack, the guarded one-shot reward claim, job/ability
advancement, boss passage unlock, and defeat signaling are durable actor work;
closing between schedule, timer fire, and actor attention loses nothing.

Tranche 1 added checked arithmetic and comparison expressions. Player damage
and HP outcomes are language-defined formulas; genuinely data-driven job and
ability progression remains relational data:

```text
job_progression(job, old_rank, old_points, reward,
                new_rank, new_points, ability,
                old_ability_rank, new_ability_rank)
```

Randomness, if introduced later, must enter as committed data through the
existing clock/randomness driver. The first combat slice is deterministic.

### Dungeon instancing

`enter basement` relocates the whole dungeon template into a fresh entity block
with `EntStore::instance_template`. Every entity-valued coordinate in the
template moves together. The instance and its return room are recorded as world
facts so leaving and reconnecting do not depend on process memory.

`leave` removes the instance facts and returns the player to the surface.
Instance loot cannot leak out unless explicitly made persistent by a later game
rule.

### Copying Mirror

The mirror can be activated only in the final chamber after the boss is
defeated. Activation:

1. forks the active `EntStore` at its current edition;
2. proves the child has the same logical world at the cut;
3. records a visible echo marker only on the child;
4. changes the durable player-to-branch route in the root control branch; and
5. continues the session against the child store.

The parent and child then evolve independently. Reopening the same store reads
the route from the root branch and resumes the child with `EntStore::branch`.
The copied mirror can be activated again, producing a real branch lineage.

Branches are currently permanent roots for reachability GC; grmpl has no branch
retirement API. The local game therefore permits recursive copies and reports
their depth, but a future untrusted public server must add quotas or branch
retirement before allowing unlimited activations.

## Architecture

All setting rules, relation declarations, views, parsers, patch-producing
behaviors, initial conditions, and finite rule tables live in
`worlds/shotengai.grmpl` and install atomically as a v4 package.

`grmpl-cli` provides a `shotengai [STORE_DIR]` command. Its remaining native
responsibilities are terminal rendering, committed-time sampling/actor driving,
card dealing, entity instancing, whole-store forking, and durable branch
routing. Ordinary movement, object handling, greeting, job changes, player
attacks, patrol, combat continuation, bootstrap, and scalar omen draws are
compiled grmpl behavior/data.

Relation IDs are resolved through the durable catalog, schemas are declared at
the commit boundary, and inbox sequence numbers are allocated from durable
sequence relations rather than process-local counters.

## Delivery slices

1. **Contract and command:** add this document, the CLI command, the world file,
   durable catalog compilation, schemas, and an idempotent package bootstrap.
2. **Surface parity:** implement the shotengai map, residents, identity fog,
   patrol, inventory, aggregate, rooftop report, card table, and reactive watch.
3. **RPG tracer bullet:** implement one complete job/ability/monster/victory
   path, including counterattack and advancement.
4. **Full job table:** declare all five jobs and three progression ranks in
   package bootstrap.
5. **Dungeon:** instance the complete basement template, cleanly leave it, and
   make monster/loot state instance-local.
6. **Artifact:** fork the complete active world, route into the child, persist
   that route across reopen, expose ancestry/depth, and prove divergence.
7. **Hardening:** add compile/surface tests, scripted play tests, combat race and
   exactly-once reward tests, instancing isolation, durable reopen, replay, and
   fork identity/divergence tests.

## Acceptance gates

- The existing manor smoke tests and session tests remain green.
- The shotengai world compiles and all required relations/views/forms resolve.
- A scripted session exercises every manor-equivalent feature.
- Changing jobs preserves each job's independent progress.
- Winning combat changes HP, grants exactly one reward, and upgrades an ability.
- Two dungeon instances are disjoint and leaving cleans the selected instance.
- At the mirror cut the fork has the same logical projection as its parent.
- A child-only write is absent from the parent.
- Closing and reopening the store resumes the routed branch.
- A second mirror activation produces a descendant branch.
- Time and random inputs never bypass committed data.
- `cargo build`, `cargo test`, and `cargo clippy --all-targets` all pass.

## Implementation status

Implemented initially on 2026-08-02; package tranche updated on 2026-08-04:

- `worlds/shotengai.grmpl` contains the relations, views, command grammar, and
  guarded behaviors.
- `grmpl shotengai [STORE_DIR]` runs the durable game and defaults to
  `.grmpl/shotengai`.
- Package bootstrap now owns the initial world and rule tables. Player combat
  arithmetic and a scalar `omen` RNG path execute through typed `BehaviorIr`;
  the edge runtime grants RNG, drives actors, instances and cleans the basement,
  consumes one-shot monster reward claims, and persists the active branch route.
- Nine game tests cover package reopen, language compilation, manor feature parity, job and
  ability progression, deterministic replay, stale combat races, player defeat,
  dungeon isolation, recursive mirror forks, parent/child divergence, and route
  recovery after reopen.
