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
network technology. Only `grmpl-store` names `fjall`; only `grmpl-transport`
names `iroh`. Substrate crates depend on the traits, never the reverse. The
language observes **opaque `Edition`s**, never physical sequence numbers.

### One serialization, versioned (`grmpl-core::wire`)

There is exactly **one** value/tuple encoding: `grmpl_core::wire`. Every framing
builds on its `encode_tuple`/`decode_tuple` — the message wire *and* the
`grmpl-store` on-disk record. `grmpl-store` does **not** keep a private copy.

Every serialized artifact begins with a single `wire::FORMAT_VERSION` byte:

* `message = version(1) || inbox(u32, BE) || encoded_tuple`
* `record  = version(1) || diff(8, LE)   || encoded_tuple`

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
* The language `find`/`resolve` binds to the **least** matching tuple, never
  whichever the scan surfaced first (`grmpl-lang::compile`).

### Catalog (`grmpl-core::Catalog`)

The name→`RelId` catalog is **append-only** and **durable**: `grmpl-store`
persists it in the `__meta` keyspace under `cat:{name}` keys. A name's id, once
bound, never silently changes (rebinding to a different id is an error). The
*contract* lives in `grmpl-core` (names and `RelId`s are core types); the
durable map is a store concern — the language resolves stable ids across reopens
through the trait without ever naming the storage engine.

### Relation schemas (`grmpl-core::schema`, `SchemaCatalog`)

Every relation may carry a **schema**: an ordered list of named, typed columns
(`Ty` = `Ent`/`Int`/`Text`/`Bool`/`Tuple`/`Any`). Like the catalog, the schema
types and the invariant logic (`Schema::check`, `Schema::is_additive_over`) are
**core**; the durable registry is a **store** concern — `grmpl-store` persists
each version in `__meta` under `sch:{rel}{edition}` keys, **versioned by the
edition** it took effect (so `schema_at` answers as-of queries).

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
     ├── grmpl-store (fjall)     │
     ├── grmpl-pattern ──────────┴── grmpl-lang
     └── grmpl-transport (iroh, feature-gated)

grmpl-session (P3 edge crate: TCP sessions, provisioning, world verbs)
     └── depends on core + diff + proc + pattern + store
```

`grmpl-session` is an **edge** crate, *not* part of the semantic core: it sits
above the bright line and wires the core to clients, so it may name a concrete
transport (std TCP) exactly as an application would. The bright line constrains
the core crates, not the app built on them.
