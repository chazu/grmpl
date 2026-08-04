# Structural capabilities and full Shotengai cutover

## Status

Phase 3 scope document, 2026-08-04. This phase intentionally retains more open
design work than phases 1 and 2 because it changes store topology and the public
presentation boundary.

Parent roadmap:
[`WORLD-PACKAGE-REMAINING-WORK.md`](../WORLD-PACKAGE-REMAINING-WORK.md).

## Outcome

Remove the remaining world-specific Shotengai coordinator. The package requests
bounded basement instances and Copying Mirror forks, owns card-game semantics,
and emits structured semantic responses. The CLI is reduced to package
selection, store/driver operation, input, and rendering.

## 3A. Transactional template instancing

Instancing stays within one active branch and should satisfy the normal
patch–edition law.

Refactor `EntStore::instance_template` into a deterministic planning interface
that, from a pinned snapshot and a granted immutable template, produces the
relocated updates. Fold those updates into the requesting behavior's patch with:

- a guarded instance-range allocation;
- player placement and durable return route;
- instance ownership/lifecycle facts; and
- bounded retirement updates.

Package syntax lowers to phase 1's stable granted-capability instruction; it
does not add an Ent-specific behavior-IR tag or expose an arbitrary native call.

An instance grant fixes the source range, relation set, target ranges,
relocation policy, and maximum expanded rows. Source templates are immutable
after bootstrap. Retirement accepts a recorded instance identity, not an
arbitrary entity interval from package code, and preconditions every row it
retracts.

The feasibility spike must answer whether a materialized patch preserves the
Ent's structural-sharing advantage. If not, the store needs a transactional
structural-effect API rather than a host sequence of commits.

Acceptance: counter allocation, relocation, ownership, and player movement all
commit or none do; concurrent entries retry without leaked template rows; two
instances are disjoint; retirement cannot touch another owner's range.

## 3B. Recoverable whole-world forks

A fork and root-route change span branches and are not one ordinary `Patch`.
Expose a durable protocol:

1. Active-world behavior commits a one-shot fork request containing a
   deterministic request ID, player, and granted fork capability.
2. The coordinator derives source branch and request edition from the committed
   update; physical coordinates are not language values.
3. An idempotent substrate operation `fork_for(request_id, source, edition)`
   creates or returns exactly one child.
4. The coordinator initializes child-only state and records the ready child in
   root control data.
5. It conditionally changes the player's route from the expected source branch
   to that child.
6. Adapters reconnect only after the route is durable. Every step resumes after
   restart or records a durable terminal failure.

An adapter convention cannot provide exactly-once child creation: crashing
after `fork_at` but before recording its result can orphan a permanent branch.
The idempotent request mapping must be substrate-owned.

Fork grants require a hard live-branch quota. General untrusted use remains
disabled until branch retirement defines descendants, active routes, pinned
snapshots, and reachability-GC behavior. Recursive local Copying Mirror tests
use a bounded grant.

Acceptance injects a crash after every protocol step and proves convergence on
one child per request, durable route recovery, fork identity at the cut,
parent/child divergence, recursive lineage, and no branch creation on quota
failure.

## 3C. Collections and card dealing

Phase 1 deliberately provides scalar RNG, not a hidden collection language.
Moving Shotengai's whole-deck shuffle requires one of two designs:

1. bounded collection values and deterministic fold/shuffle operations in
   behavior IR; or
2. a narrow, versioned `shuffle` capability whose input deck and RNG state are
   committed data and whose result/state seal into one patch.

The choice must account for maximum collection size, type checking, unbiased
sampling, persistent algorithm versioning, and response materialization. A
general collection design has broader leverage; a shuffle capability has much
smaller semantic surface. Phase 3 must choose explicitly rather than smuggling a
host deck algorithm behind scalar `random`.

Acceptance proves unique cards, deterministic replay, rejected-patch RNG
atomicity, and equivalence between terminal/TCP clients using the same package
behavior.

## 3D. Structured semantic responses

World rules should produce presentation data without owning ANSI, TCP lines,
HTML, or localization policy.

The first response protocol can use existing nested `Value::Tuple` values:

```text
("grmpl.response", 1, ("text", "The Last Customer collapses."))
("grmpl.response", 1, ("fields", (("HP", 12), ("Job", "Courier"))))
("grmpl.response", 1, ("table", ("Name", "Rank"), rows))
```

Language support must include deterministic scalar-to-text conversion,
interpolation, standard response constructors, and bounded materialization of a
named view at the behavior's pinned snapshot. Passing a view name for the
adapter to query later is incorrect because it could observe another edition.

Responses carry an independent protocol version, validate before commit, and
obey row/cell/byte limits. Terminal and TCP renderers may differ visually but
must preserve the same semantic fields and ordering.

Acceptance separates golden response-value tests from renderer tests and ports
`look`, combat/status, card hands, and lineage output out of `shotengai.rs`.

## Cutover exit

Phase 3 is complete when a repository search finds no Shotengai-specific Rust
that:

- seeds world facts;
- computes or commits gameplay rules;
- owns RNG/deck state;
- injects actor ticks;
- calls `instance_template`, `fork_at`, or branch-routing mutation directly; or
- formats semantic room, combat, job, card, or lineage results.

Remaining Shotengai-specific Rust may select bundled package source/assets and
choose adapter rendering policy through general interfaces.

## Open questions reserved for phase 3

1. Can instancing be planned into a patch without losing the Ent's structural
   sharing or exceeding practical commit size?
2. What durable DAG metadata and API make `fork_for(request_id, …)` idempotent?
3. What branch-retirement rules are safe for routed sessions and descendants?
4. Do cards justify general bounded collections or a narrow shuffle capability?
5. What is the minimum response algebra that removes native formatting without
   becoming a UI framework?

These are intentionally not phase-1 blockers.

## Verification

In addition to focused laws above, the complete existing Shotengai suite must
pass through `grmpl::Runtime`: scripted manor parity, job retention, one-shot
combat reward, dungeon isolation, recursive mirror forks, route recovery,
parent/child divergence, watch resume, and deterministic replay.

Repository gates remain `cargo build --workspace`, `cargo test --workspace`, and
`cargo clippy --all-targets`.
