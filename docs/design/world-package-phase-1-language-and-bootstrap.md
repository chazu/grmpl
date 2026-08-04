# World packages, typed behavior, and transactional primitives

## Status

Implemented 2026-08-04. This document is the normative tranche-1 contract and
records the acceptance laws exercised by the implementation.

Parent roadmap:
[`WORLD-PACKAGE-REMAINING-WORK.md`](../WORLD-PACKAGE-REMAINING-WORK.md).

## Outcome

After this phase, a `.grmpl` package can install its own initial world, evaluate
bounded typed computations inside behaviors, allocate fresh entities, and make
scalar random choices without a world-specific Rust coordinator.

Concretely:

- MOO `create` and `dig` become language-defined behaviors over a granted
  allocator.
- Shotengai's native `seed_world` and `seed_rule_tables` are replaced by package
  bootstrap data.
- Combat damage, HP, reward, and progression choices that are presently finite
  arithmetic lookup tables or Rust coordination become ordinary checked
  expressions and guarded effects.
- A small Shotengai random-choice path proves committed RNG consumption.

This phase does **not** promise autonomous actor driving, basement instancing,
whole-world forks, structured presentation, or a whole-deck shuffle. Those
belong to phases 2 and 3.

## Baseline seams replaced by this phase

The implementation built on, rather than replaced, these pre-existing
mechanisms:

- `Program::compile_with_catalog` gives relation names stable durable IDs but is
  explicitly a single-writer provisioning operation.
- `Runtime::compile` compiles against the catalog and registers schemas
  effective at the next edition.
- `Patch`, `commit_patch`, and `Backoff` already provide atomic guarded effects
  and retry.
- `Alloc` already reads a unary `(next: Int)` relation from a pinned snapshot
  and seals its guarded counter advance into the consuming patch.
- Shotengai's RNG already expects, retracts, and asserts `(world, state)` in the
  same patch as the dealt cards.
- Named statement arms and concatenative/stored behaviors currently execute
  through related but distinct paths. `Word` is durable IR under the shared
  `wire::FORMAT_VERSION`.
- Catalog and schema metadata live in the Ent context enfilade and persist
  separately from ordinary trace commits.

The design must preserve those existing laws while closing the source-level
gaps.

## Scope decisions

### Included

- one package declaration per source unit;
- one versioned bootstrap block;
- explicit entity constants;
- typed fact literals over declared relations;
- immutable local bindings and statement-level conditional branches;
- checked `Int` and finite binary64 `Float` arithmetic and boolean predicates;
- unary entity allocators using the current `Alloc` row shape;
- keyed scalar RNG streams using `(owner: Ent, state: Int)`;
- host capability grants and capability-aware effect checking; and
- clean-store installation with a fresh-store v4 cutover for old serialized
  behavior/store data.

### Excluded

- loops, recursion, exceptions, mutable locals, implicit numeric coercions,
  non-finite floating-point values, and arbitrary user functions;
- keyed entity allocation or a merger of `Alloc` and `SeqAlloc`;
- collection construction, iteration, sampling without replacement, or deck
  shuffling;
- external entropy sampling from behavior code;
- automatic bootstrap reconciliation or general data migration;
- actor declarations, timers, instancing, forks, or structured responses; and
- general FFI/native-call words.

These exclusions are semantic boundaries, not merely missing parser syntax.

## 1. Package source model

### Concrete first surface

The first surface has this shape:

```grmpl
package kasumi_shotengai bootstrap 1

entity WORLD = 1
entity EAST_GATE = 100
entity TAMA = 130

rel entity_seq(next: Int)
rel rng_state(owner: Ent, state: Int)
rel named(thing: Ent, name: Text)
rel located(thing: Ent, place: Ent)
rel combat_tuning(damage_scale: Float)

requires allocate entities(
    counter: entity_seq,
    first: 1000000,
    last: 1999999
)

requires random cards(
    state: rng_state,
    owner: WORLD,
    algorithm: xorshift64star_v1
)

bootstrap {
    entity_seq(1000000)
    rng_state(WORLD, 4886718345)
    named(EAST_GATE, "East Gate")
    located(TAMA, EAST_GATE)
    combat_tuning(1.25)
}
```

This document proposes that spelling as the phase-1 grammar. The final syntax
review is deliberately narrow: it may change the punctuation or keywords listed
under [Remaining surface review](#remaining-surface-review), but not the
compiled model or constraints below.

### Compiled package

Compilation produces a `CompiledPackage`, not only a `Program`:

```text
CompiledPackage
  package_id: Text
  bootstrap_version: u32
  program: Program
  entities: name -> Entity
  bootstrap_facts: sorted unique (relation_name, Fact)[]
  requirements: sorted CapabilityRequirement[]
  bootstrap_digest: [u8; 32]
```

`Program::compile` remains available for catalog-free language tests.
`Runtime::load_package` becomes the supported product path. The existing
`Runtime::compile` may delegate to a compatibility package with no bootstrap or
capabilities while callers migrate.

Exactly one `package` declaration is required for `load_package`; package ID is
a stable identifier, not a display name. Phase 1 has no imports, package
composition, or namespace aliases.

### Entity constants

An entity declaration creates a compile-time constant of type `Ent`. It may be
used in bootstrap facts, view arguments, behavior expressions, and capability
requirements.

In a bootstrap block, a bare identifier must resolve to an entity constant. In
views and behaviors, entity constants occupy the package namespace and may not
be shadowed by a parameter or local binding. Phase 1 bootstrap literals are
entity constants, `Int`, finite `Float`, `Text`, `Bool`, and recursively nested
tuples. `Bytes` is reserved for runtime metadata in this tranche and `Code`
cannot be installed through a bootstrap literal.

The compiler rejects:

- duplicate entity names or numeric IDs;
- negative IDs or IDs above `i64::MAX` in this first syntax;
- a static ID inside any requested allocation range; and
- overlapping allocation ranges.

Entities remain explicit domain values. Constants do not add hidden tuple
identity or a second entity catalog.

## 2. Host grants and load contract

### Requirements are not grants

Package source describes the minimum capability it needs. The host passes a
`GrantSet` to `Runtime::load_package`. Loading resolves requirement relation
names through the compiled program and checks each request against its grant.

Phase 1 has two grant kinds:

```text
AllocateGrant
  name
  counter relation
  inclusive entity range

RandomGrant
  name
  state relation
  owner entity
  algorithm version
```

A grant must match the requirement's kind, relation, owner/key, and algorithm,
and must contain the requested allocation range. Extra host grants are not
visible to the package. Requirement and grant collections are deterministically
ordered by `(kind, name)`.

This is intentionally redundant with relation authority:

- authority answers whether an actor may write the concrete counter/state row;
- the grant answers whether the actor may invoke allocation/random semantics at
  all.

Both checks must pass.

### Execution without changing `Behavior`

The `grmpl_proc::Behavior` call shape remains
`snapshot × message -> Result<Patch>`. A behavior constructed by
`Runtime::load_package` closes over an immutable `ResolvedGrantSet`. The
interpreter creates a per-invocation execution frame holding allocator and RNG
state and seals those resources into the returned patch.

`Program::behavior` without a grant set continues to run capability-free code
and returns a deterministic configuration error if capability instructions are
present. This keeps semantic process APIs free of a concrete runtime type.

## 3. Bootstrap installation

### Reserved marker relation

The package compiler synthesizes one package-private relation under a reserved
name that source cannot declare or reference:

```text
grmpl:package/install(
  package: Text,
  bootstrap_version: Int,
  bootstrap_digest: Bytes
)
```

It is assigned through the same durable `Catalog` allocator and receives a
normal registered schema. Keeping the marker in the ordinary trace—not the
context enfilade—allows it to commit atomically with the bootstrap facts.

Catalog bindings and schemas still persist separately before that commit. This
does not weaken bootstrap atomicity: after a crash, harmless provisioning
metadata may exist, but either both world facts and marker exist at edition 1 or
neither does.

### Canonical bootstrap and digest

The compiler resolves constants and relation names, checks every fact against
its schema, rejects duplicate facts, and retains both the stable relation name
and resolved `RelId`. Commit order is `(RelId, Tuple)`, but digest order is
`(relation_name, encoded Tuple)`. The digest therefore identifies the same
package data across stores even when their unrelated catalog contents assign
different physical relation IDs.

`bootstrap_digest` is SHA-256 over a canonical, length-prefixed encoding of:

- package ID and bootstrap version;
- sorted entity declarations;
- sorted bootstrap relation names and tuples encoded with
  `grmpl_core::wire`; and
- persistent capability requirements: allocation ranges, RNG state binding,
  and RNG algorithm version.

Unrelated views, forms, and behavior bodies are excluded. World rules may
evolve without pretending to be a data migration. Changing installed entities,
facts, allocator ranges, RNG binding, or RNG algorithm requires a bootstrap
version change. Phase 1 installs that changed bootstrap in a fresh store; any
future in-place evolution needs a separate migration design.

Do not use Rust's `DefaultHasher`; the digest is a durable format.

### Load algorithm

`Runtime::load_package` is a provisioning-time single-writer operation, matching
the existing `compile_with_catalog` contract.

1. Parse the package declaration, relations, constants, requirements, and
   bootstrap block.
2. Compile through the durable catalog, including the synthetic marker
   relation.
3. Register all schemas effective at `current + 1`.
4. Resolve and validate host grants.
5. Read the marker relation at the current edition.
6. If exactly one matching marker exists, do not inspect or reconcile gameplay
   facts; installation is complete.
7. If a marker exists but package ID, version, or digest differs, fail closed.
8. If no marker exists, require `store.current() == Edition::ZERO`. An unmarked
   store with world history is not adopted automatically.
9. Commit all sorted bootstrap facts plus the marker in one ordinary trace
   commit. It must become edition 1.

Because fresh catalog ID allocation and first installation are already
single-writer provisioning operations, phase 1 does not invent an absence
precondition or distributed installer lock. Concurrent `load_package` calls are
unsupported and return a documented provisioning error where they can be
detected.

### Reopen and compatibility behavior

- Exact marker: reopen creates no world edition.
- Catalog/schema metadata but edition zero: resume installation safely.
- Unmarked nonzero store: refuse; use a fresh store.
- Older/newer bootstrap marker: refuse; phase 1 has no migration DSL.
- Multiple live marker rows: report store corruption; never choose one.

The existing native Shotengai store therefore requires a fresh package store.
There is no v3-to-v4 migrator in this phase. Merely adding a marker to populated
data would not prove that its weights match the canonical bootstrap.

## 4. Typed behavior computation

### One executable IR

Phase 1 introduces a typed `BehaviorIr` as the sole executable behavior plan.
Named statement arms and concatenative arms remain authoring surfaces, but both
must lower to `BehaviorIr` before execution. Stored behavior values serialize
that same IR.

This removes the current semantic risk in which `exec_stmts` and `exec_words`
would each need independent implementations of arithmetic, branches, resource
consumption, faults, and effect inference.

The IR contains:

```text
ValueExpr
  local | literal
  intrinsic(name, arguments)

BoolExpr
  eq | ne | lt | le | gt | ge
  not | and | or

BehaviorOp
  resolve | find
  let
  if { BehaviorOp* } else { BehaviorOp* }
  expect | assert | retract | emit
  invoke_capability(capability, arguments, destinations)
```

Locals are immutable typed slots. `self`, constructor arguments, `find` and
`resolve` results, `let` bindings, fresh entities, and random results all occupy
slots. The compiler rejects an unbound local, duplicate binding in one lexical
scope, or use outside its branch scope.

The concatenative compiler symbolically executes stack operations over typed
IR temporaries and emits the same local/value operations. It is not a second
runtime interpreter. Existing statement-versus-word edition-equivalence tests
remain the compatibility oracle.

Pure intrinsic names and capability names are compile-time symbols, not values
that world code can construct dynamically. Phase 1's closed intrinsic registry
contains the arithmetic, comparison, and boolean operations below. Its closed
capability-kind registry contains allocation and scalar RNG; later phases may
add scheduling and structural kinds without adding another behavior-IR framing
tag. Unknown names fail compilation/execution. This extensibility is not native
FFI: a source declaration, a known runtime kind, static effect inference, and a
matching host grant are all still required.

### Surface expressions

Named bodies gain immutable bindings and conditional statements:

```grmpl
let damage = max(1, power - guard)
let remaining = hp - damage

if remaining <= 0 {
    expect hp(monster, hp)
    retract hp(monster, hp)
    assert hp(monster, 0)
    assert defeated(monster)
} else {
    expect hp(monster, hp)
    retract hp(monster, hp)
    assert hp(monster, remaining)
}
```

Precedence is conventional: unary, multiplicative, additive, comparison,
`&&`, then `||`. Parentheses override precedence. `if` is a statement and does
not yield a value. `else` is optional and means an empty branch.

### Type rules

- `+`, `-`, `*`, `/`, `%`, unary negation, `min`, and `max` accept either two
  `Int` operands or two `Float` operands and return that same type.
- Mixed `Int`/`Float` arithmetic is rejected. `float(value: Int) -> Float` is
  the only phase-1 numeric conversion; there is no implicit widening and no
  `Float`-to-`Int` conversion.
- `<`, `<=`, `>`, and `>=` accept two `Int` values or two `Float` values.
- `==` and `!=` accept two values of the same concrete type.
- `&&`, `||`, and `!` require `Bool`; `&&` and `||` short-circuit.
- No numeric/text/bool coercions exist.
- A schema column typed `Any` cannot participate in a typed operation without a
  future explicit refinement operation. Shotengai relations used here must be
  concretely typed.
- Both branches are type-checked. Effects are the union of both branches.

All integer operations use checked `i64` semantics. Overflow, division by zero,
and remainder by zero are faults. Division truncates toward zero and remainder
matches Rust `i64` semantics for valid inputs.

### Float value and literal contract

`Float` is a first-class scalar type backed by finite IEEE-754 binary64. It is
not stored as an unconstrained Rust `f64`: the core owns a small canonical
wrapper so `Value` can continue to implement structural `Eq`, `Hash`, and
`Ord`.

- NaN and positive or negative infinity are not values and are rejected by the
  parser, decoder, conversion, and arithmetic evaluator.
- Negative zero is canonicalized to positive zero at every construction and
  decode boundary. Equality, hashing, ordering, and encoding therefore agree.
- Remaining finite values compare numerically. Their stable ordering is the
  finite subset of IEEE total order after zero canonicalization.
- The value wire form is a dedicated tag followed by the canonical 64-bit IEEE
  payload in big-endian order. `Ty::Float` receives its own schema tag.
- A decimal literal containing a decimal point or exponent is `Float`:
  `1.0`, `0.25`, `1e3`, and `2.5e-2`. An integer-looking literal such as `1`
  remains `Int`; a leading `-` is unary negation.

Basic operations use binary64 round-to-nearest, ties-to-even behavior. An
operation that would produce NaN or infinity is a deterministic behavior fault,
including division or remainder by zero and finite overflow. Phase 1 does not
include fused multiply-add, transcendental functions, configurable rounding,
or a host-dependent fast-math mode.

Float `%` uses a truncated quotient, matching Rust's finite `f64` remainder.
Converting an `Int` with `float(...)` is exact where representable and otherwise
rounds to the nearest binary64 value, ties to even. Decimal parsing must be
correctly rounded. Fixed codec/evaluator vectors include:

| Expression | Canonical result bits |
| --- | --- |
| `0.1 + 0.2` | `0x3fd3333333333334` |
| `5.5 % 2.0` | `0x3ff8000000000000` (`1.5`) |
| `float(9007199254740993)` | `0x4340000000000000` (`9007199254740992.0`) |

### Behavior faults

A runtime fault is deterministic and occurs before commit. The process cursor
does not advance, no partial patch commits, and `run_to_idle` stops and returns a
typed error identifying the actor, inbox sequence, and operation.

Backoff is only for optimistic `CommitOutcome::Rejected`; it must not retry a
deterministically faulty message forever. Phase 1 does not add dead-letter or
skip-message policy. Operators may fix live behavior/data and retry the still
unconsumed message.

## 5. Entity allocation capability

Phase 1 deliberately uses the existing `Alloc` contract rather than
generalizing it.

An allocation requirement names:

- one relation with exact schema `(next: Int)`;
- an inclusive first and last entity ID; and
- a capability name used by behavior code.

The bootstrap must contain exactly one weight-1 counter fact equal to `first`.
Missing, duplicate, non-unit, malformed, or out-of-range counter state is a
deterministic configuration fault. Capability execution never uses `Alloc`'s
unseeded fallback.

Within one behavior invocation, the execution frame constructs at most one
`Alloc` per capability from the pinned snapshot. Every `fresh` consumes the next
ID. At return, the frame seals each used allocator into the patch exactly once.
If the next ID would exceed `last`, behavior faults before commit.

```grmpl
fresh entities as room

assert named(room, name)
assert exits(here, name, room)
```

Static effects include both the `allocate:entities` capability and a write to
its counter relation. The actor must have a matching host grant and authority
over the counter row. A lost counter race rejects the entire patch; normal
process retry rebuilds against the winner and allocates a new ID.

Acceptance preserves the existing `Alloc` laws and adds a language-level race:
two builders creating different things never receive the same entity, and a
rejected creation leaves neither object facts nor a counter advance.

## 6. Scalar RNG capability

### Durable shape

Phase 1 standardizes one RNG-state shape:

```grmpl
rel rng_state(owner: Ent, state: Int)
```

A requirement fixes the relation, owner constant, and algorithm version. The
bootstrap must seed exactly one weight-1, nonzero row for that key. State is an
opaque 64-bit bit pattern stored through the existing `Int` cell; conversion
between `i64` and `u64` preserves bits.

### `xorshift64star_v1`

The first algorithm is Shotengai's current xorshift64* transition:

```text
x = state
x ^= x >> 12
x ^= x << 25
x ^= x >> 27
state' = x
output = x * 0x2545_F491_4F6C_DD1D mod 2^64
```

Zero state is invalid because it is absorbing. These fixed transition/output
vectors are part of the algorithm contract:

| Initial state | Successor state | Output |
| --- | --- | --- |
| `0x0000000000000001` | `0x0000000002000001` | `0x47e4ce4b896cdd1d` |
| `0x0000000123456789` | `0x0246aea6d582870c` | `0xeaef17c18b6ea85c` |
| `0x8000000000000001` | `0x8008001003000001` | `0x38ea0cf8a66cdd1d` |
| `0xffffffffffffffff` | `0xfff0001ffe000000` | `0xf92cc9e5c6000000` |

The first bounded-result vectors are `seed=1, bound=100 -> 65` and
`seed=0x0000000123456789, bound=52 -> 36`, each consuming one transition.

`random <capability> below <bound> as <local>` produces an unbiased integer in
`[0, bound)`. `bound` must be `1..=i64::MAX`. It uses rejection sampling:

```text
threshold = (-bound mod 2^64) mod bound
draw outputs until output >= threshold
result = output mod bound
```

Every rejected output still advances the in-frame state. The final state—not
each intermediate draw—is sealed into the behavior patch by expecting,
retracting, and asserting the state row once.

```grmpl
random encounters below 100 as roll
if roll < 15 {
    assert encounter_pending(self)
}
```

As with allocation, one execution frame owns one stream state per capability.
Two contenders drawing from one stream have one winner; the loser rebuilds from
the committed successor state. A rejected patch consumes no draw.

This scalar primitive does not implement a 52-card shuffle. Sampling without
replacement needs collection state/iteration or a separately specified shuffle
capability and is deferred to phase 3.

## 7. Effects, authority, and stored code

Extend `EffectRow` with a deterministically ordered set of capability uses in
addition to relation writes and sends:

```text
EffectRow
  writes: RelId set
  sends: RelId set
  capabilities: CapabilityId set
```

Inference rules are:

- arithmetic, predicates, bindings, and condition selection add no effect;
- branch effects are unioned;
- `fresh` adds its allocation capability and counter relation write;
- `random_below` adds its RNG capability and state relation write; and
- ordinary relation operations retain their current rules.

Compile/load checking requires every capability in the row to resolve to a host
grant and every implied state write to be relation-authorized. Concrete key
ranges remain checked at commit as today.

`EffectChecker` receives the resolved grant set alongside `Program`. When a
patch installs `Value::Code`, it decodes the new `BehaviorIr`, infers relation
and capability effects again, and rejects ungranted code before any edition is
allocated. Grant names are symbolic durable IR references; concrete host handles
are never serialized into code.

## 8. Storage-format decision

`StoredBehavior` currently serializes `Word` tags beneath the shared
`grmpl_core::wire::FORMAT_VERSION` byte. The repository explicitly requires a
format bump when that IR tag set changes. Replacing serialized word lists with
`BehaviorIr` therefore affects not only code values but every version-prefixed
artifact, including Ent node frames.

The v4 behavior framing must include the stable `intrinsic(name, arguments)` and
`invoke_capability(name, arguments, destinations)` envelopes defined above.
Later closed intrinsic/capability registries can then grow without changing the
IR framing tag set for every phase; unsupported symbolic names still fail
loudly at compile/load/execution.

The compatibility policy is decided: phase 1 is a **fresh-store v4 cutover**.
Adding `Float` value/schema tags and replacing serialized word lists with
`BehaviorIr` bumps the shared format from v3 to v4. Normal v4 open rejects v3
bytes with an actionable version diagnostic. The project will not implement a
v3 reader, offline migrator, or dual-version ordinary decoder in this phase.

Existing v3 stores remain untouched and may still be opened with a matching v3
build, but phase-1 packages must be installed into new stores. No command may
delete, overwrite, or repurpose an existing store automatically.

## 9. Delivery slices

Each slice is vertical and keeps the workspace green.

1. **Package AST and compiler:** package/entity/require/bootstrap declarations,
   canonical compiled facts, validation, digest vectors, and source tests.
2. **Atomic installer:** synthetic marker relation, `Runtime::load_package`,
   clean-store/reopen/crash tests, and mismatch diagnostics.
3. **Typed behavior IR and numeric values:** canonical finite `Float` in core
   value/schema/wire layers; named and concatenative lowering; `Int`/`Float`
   scalar operations, branches, faults, effect union, codec laws, and
   surface-equivalence tests.
4. **Allocation grant:** resolved grants, execution-frame sealing, MOO
   `create`/`dig` port, replay and contention laws.
5. **RNG grant:** fixed vectors, unbiased bounded draws, atomic state sealing,
   replay/contention laws, and one Shotengai scalar-random behavior.
6. **Shotengai bootstrap and rule port:** move initial facts and finite content
   tables into source; replace arithmetic-table/host formula paths with typed
   behavior while retaining genuinely data-driven tables.
7. **Cutover cleanup:** remove superseded MOO/Shotengai seeding and native
   arithmetic/allocation paths; document the still-native card, actor,
   instance, fork, and presentation capabilities for phases 2/3.

## 10. Verification and exit criteria

Repository gates:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets
```

Named phase laws:

- **Package determinism:** recompiling against the same durable catalog after
  relation declarations are reordered preserves resolved IDs, canonical facts,
  and digest.
- **Bootstrap atomicity:** a crash yields facts+marker or neither; exact reopen
  creates no edition; unmarked nonzero stores and mismatches fail closed.
- **Surface equivalence:** named and concatenative source lower to behavior IR
  with identical effects and committed editions.
- **Expression determinism:** checked evaluation and faults reproduce from the
  same snapshot/message.
- **Float canonicality:** finite values round-trip through value, tuple, schema,
  behavior, and store framing; NaN/infinity are rejected; negative zero encodes
  identically to positive zero; and fixed arithmetic vectors have fixed bits.
- **Allocation atomicity:** allocated IDs, counter advance, and consuming facts
  commit together; contention never duplicates an ID.
- **RNG atomicity:** result-dependent effects and final stream state commit
  together; replay repeats draws; contention never consumes one state twice.
- **Stored-code authority:** commit-boundary checking rejects code with an
  ungranted capability or unauthorized state relation.
- **World regression:** existing manor and Shotengai feature tests remain green,
  with phase-1 paths going through `Runtime::load_package`.

Phase 1 is complete when:

- package bootstrap owns initial manor and Shotengai facts;
- reopening either package is an edition-preserving installation no-op;
- MOO `create` and `dig` no longer use native Rust allocation;
- the selected Shotengai combat/progression formula paths no longer require
  seeded exhaustive arithmetic tables or native calculation;
- `Float` facts and behavior results survive close/reopen with canonical bytes,
  and non-finite results fault without advancing the process cursor;
- a Shotengai scalar random behavior uses committed RNG state through a grant;
- no package behavior can access allocation/RNG without both grant and
  relation authority; and
- every capability intentionally left native is listed as phase 2 or phase 3
  work rather than hidden in an adapter.

### Implementation evidence

The contract is pinned by repository-owned tests rather than only by the world
demos:

- core float, schema, wire, SHA-256, and v3 rejection vectors live beside the
  relevant `grmpl-core` and `grmpl-ent` modules;
- `crates/grmpl-session/tests/package.rs` covers canonical installation,
  grants, replay, exhaustion, and allocation/RNG contention;
- `crates/grmpl-session/tests/behavior_expressions.rs` and
  `crates/grmpl-lang/tests/behavior_ir_equivalence.rs` cover typed evaluation,
  exact float vectors, surface equivalence, deterministic faults, and reopen;
- `crates/grmpl-type/tests/effect.rs` and `behavior_commit.rs` cover capability
  effect union and stored-code grant/authority enforcement; and
- the manor construction/race tests and nine Shotengai tests exercise the
  package path through the product adapters.

## Resolved source surface

The fresh-store v4 cutover and the proposed source spelling were accepted and
implemented as follows:

- top-level header: `package <id> bootstrap <version>`;
- declarations: `entity NAME = 1`, `rel name(column: Type)`, separated by
  whitespace rather than semicolons;
- grants: `requires allocate name(...)` and `requires random name(...)`;
- installation data: `bootstrap { relation(arguments) ... }`;
- behavior flow: `let name = expression` and `if condition { ... } else { ... }`;
- capability calls: `fresh name as local` and
  `random name below bound as local`; and
- float literals: a decimal point or exponent distinguishes `Float` from
  `Int`, with explicit `float(int_expression)` for conversion.

These forms are now compatibility surface. Changing them, the package/install
model, numeric model, allocator shape, RNG algorithm contract, behavior fault
policy, authority model, or v4 framing requires a new design decision rather
than an implementation-time interpretation.
