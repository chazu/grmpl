# grmpl — invariants & commands

`grmpl` is a differential, relational substrate for *deriving, watching, and
patching a versioned world*. The full design is in [`DESIGN.md`](DESIGN.md); the
phased plan is in [`docs/ROADMAP.md`](docs/ROADMAP.md). This file is the short
list of load-bearing invariants and the commands that verify them.

## Commands

Verify **all three** before considering any change done:

```sh
cargo build          # whole workspace compiles
cargo test           # suite is green
cargo clippy --all-targets
```

The `iroh` transport feature is off by default and must stay off for the core
build/test path (see `DESIGN.md` §13, the iroh version note).

## Invariants

### The bright line (`DESIGN.md` §1)

The semantic core (`grmpl-core`, `-diff`, `-proc`, `-lang`, `-pattern`) is
*above the line*: pure value types and the substrate **traits**
(`TraceStore`, `EditionStore`, `Catalog`, `Transport`). It names no storage or
network technology. Only `grmpl-ent` names `fjall` (as the granfilade's node
store); only `grmpl-transport` names `iroh`. Substrate crates depend on the
traits, never the reverse. The language observes **opaque `Edition`s**, never
physical sequence numbers.

### One serialization, versioned (`grmpl-core::wire`)

There is exactly **one** value/tuple encoding: `grmpl_core::wire`. Every framing
builds on its `encode_tuple`/`decode_tuple` — the message wire *and* the
granfilade's on-disk node frame. `grmpl-ent` does **not** keep a private copy.

Every serialized artifact begins with a single `wire::FORMAT_VERSION` byte:

* `message   = version(1) || inbox(u32, BE) || encoded_tuple`
* `node frame = version(1) || tag(1) || n_children(u32, BE) || [content_key]*n
                || count(u32, BE) || payload`

Node content keys are **SHA-256** (`grmpl_ent::hash`), vendored and pinned
against the FIPS vectors: the hash is part of the on-disk format, so it may not
drift with the toolchain, and world content is player-supplied, so it must be
collision-resistant against chosen input.

Decoders reject any other version loudly (`Error::Codec`) rather than misreading
an evolved layout. **Bump `FORMAT_VERSION` on any change to the tag set or
framing.**

### Determinism

Reads and deltas are deterministic regardless of the store's physical scan
order:

* `TraceStore::read_at` returns **tuple-sorted** rows (consolidation runs over a
  `HashMap`, whose order is not stable).
* `TraceStore::scan_updates` returns updates in **commit order**
  `(edition, counter)` — the exact order in which they were written, not scan
  order.
* `TraceStore::compare` returns **tuple-sorted** state differences.
* The language `find`/`resolve` binds to the **least** matching tuple, never
  whichever the scan surfaced first (`grmpl-lang::compile`).

### Concurrency

The write path is **group-committed** and the read path is **off the lock**;
neither weakens a law, and `docs/CONCURRENCY.md` is the full account.

* **Durability gates the commit call, not the clock.** `commit`/`commit_if`
  return only once the edition they return is durable. `EditionStore::current`
  is the **allocated** edition, because `commit_if` validates preconditions
  against the allocated state and reads must agree with the validator — a store
  whose clock lags what it validates against livelocks every guarded
  read-modify-write. `EntStore::durable_edition` is the on-disk frontier.
* **Every counter is guarded.** `Alloc::seal` and `SeqAlloc::seal` precondition
  the present counter row, so concurrent allocation resolves to one winner. The
  only unguarded write is the *first* seed of a counter, which must ride inside
  an already-guarded commit or an un-raced setup path.
* **A rejection is retried, not swallowed** (`grmpl-proc::Backoff`). The retry
  rebuilds the patch from current state, which is what makes a lost race
  re-decide rather than vanish. Backoff jitter draws no entropy from the
  environment, so the Replay law is untouched.
* **Reads go through a pinned `EditionReader`** (`Snapshot` holds one). Two
  reads that must be decided together must come from **one** snapshot — a check
  at one edition and a counter read at another is a race no precondition can
  close.

### Catalog (`grmpl-core::Catalog`)

The name→`RelId` catalog is **append-only** and **durable**: `grmpl-ent`
persists it as bindings in the **context enfilade** at the root scope. A name's id, once
bound, never silently changes (rebinding to a different id is an error). The
*contract* lives in `grmpl-core` (names and `RelId`s are core types); the
durable map is a store concern — the language resolves stable ids across reopens
through the trait without ever naming the storage engine.

### Relation schemas (`grmpl-core::schema`, `SchemaCatalog`)

Every relation may carry a **schema**: an ordered list of named, typed columns
(`Ty` = `Ent`/`Int`/`Text`/`Bool`/`Tuple`/`Bytes`/`Any`). Like the catalog, the schema
types and the invariant logic (`Schema::check`, `Schema::is_additive_over`) are
**core**; the durable registry is a **store** concern — `grmpl-ent` persists each
version in the context enfilade under a `(rel, edition)` key, **versioned by the
edition** it took effect, so `schema_at` is a WID range walk over the relation's
version span rather than a scan.

* **Additive-only evolution.** A relation's schema may only *grow*: a new
  version must be a prefix-superset of the current one (existing columns
  unchanged, only appended) at a strictly later edition. Any other change is
  `Error::Schema`. A re-put of the identical schema is idempotent.
* **Commit-boundary enforcement.** Beside the Authority check in `commit_patch`
  and `Domain::commit`, every asserted/retracted world fact whose relation has a
  registered schema must conform (arity + column types). Schemas are **opt-in**:
  an unregistered relation is unchecked. Enforcement takes a `&dyn
  SchemaCatalog` (`NoSchemas` opts out).
* **One serialization.** Schemas are framed by `grmpl_core::wire::encode_schema`
  under the shared `FORMAT_VERSION` byte (a separate `Ty` tag namespace); a
  change to either the value tags or the schema `Ty` tags bumps the version.

### Patch–edition law (`DESIGN.md` §4.1, §5.2)

A `commit` allocates the next edition **and** writes atomically, or has no
effect — there is no window in which an edition is allocated but not written.
One authority domain has one commit clock (edition allocation is serialized
within the domain). `commit_if` re-checks preconditions and writes as one atomic
step, so racing commits resolve to exactly one winner.

## Workspace layout

```
grmpl-core ── grmpl-diff ── grmpl-proc ── grmpl-lang
     │            │              │
     ├── grmpl-ent (the Ent; fjall as the granfilade node store)
     ├── grmpl-pattern ──────────┴── grmpl-lang
     └── grmpl-transport (iroh, feature-gated)

grmpl-session (P3 edge crate: TCP sessions, provisioning, world verbs)
grmpl-conformance (dev-only: one law suite, every substrate)
```

The **Ent is the only substrate**: `grmpl-store` (the fjall LSM) was the
construction-time differential oracle and is deleted. The store contract is now
stated absolutely in `grmpl-ent/tests/store_laws.rs` — determinism, the
patch–edition law, history/consolidation, and fork identity, each against an
independent model — and every law of the language runs through
`grmpl-conformance`, which is what made the cutover checkable.

`grmpl-session` is an **edge** crate, *not* part of the semantic core: it sits
above the bright line and wires the core to clients, so it may name a concrete
transport (std TCP) exactly as an application would. The bright line constrains
the core crates, not the app built on them.
