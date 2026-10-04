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

Distribution is deferred: `grmpl-transport` has only an in-process net, and no
networked transport (iroh is the designed-for one) is built.

## Invariants

### The bright line (`DESIGN.md` §1)

The semantic core (`grmpl-core`, `-diff`, `-proc`, `-lang`, `-pattern`) is
*above the line*: pure value types and the substrate **traits**
(`TraceStore`, `EditionStore`, `Catalog`, `Transport`). It names no storage or
network technology. Only `grmpl-ent` names `fjall` (as the granfilade's node
store). Substrate crates depend on the traits, never the reverse. The language observes **opaque `Edition`s**, never
physical sequence numbers.

### One serialization, versioned (`grmpl-core::wire`)

There is exactly **one** value/tuple encoding: `grmpl_core::wire`. Every framing
builds on its `encode_tuple`/`decode_tuple` — the message wire *and* the
granfilade's on-disk node frame. `grmpl-ent` does **not** keep a private copy.

Every serialized artifact begins with a single `wire::FORMAT_VERSION` byte:

* `message   = version(1) || inbox(u32, BE) || encoded_tuple`
* `node frame = version(1) || tag(1) || n_refs(u32, BE) || [content_key]*n
                || count(u32, BE) || payload` — an internal node's refs are its
  children and its payload is its separators, then each child's dsp (`i64`),
  size (`u64`) and measure; a leaf's payload is its entries and its refs are the
  **links** its values hold (a value may be a whole tree), in order
* `root record = version(1) || n(u8) || [present(1) || content_key?]*n`

Node content keys are **SHA-256** (`grmpl_core::hash`), vendored and pinned
against the FIPS vectors: the hash is part of the on-disk format, so it may not
drift with the toolchain, and world content is player-supplied, so it must be
collision-resistant against chosen input.

Decoders reject any other version loudly (`Error::Codec`) rather than misreading
an evolved layout. **Bump `FORMAT_VERSION` on any change to the tag set or
framing.**

### Displacement (`grmpl-ent::tree`, `dsp`)

Every `Tree` handle carries a **dsp** (its node's position relative to its
parent); nodes store keys in their local frame. Two rules keep it correct:

* **Reads move stored keys up to the query, never the query down.** A
  displacement is order-preserving only over the keys a subtree holds; a query
  key moved into a block's frame can wrap the entity id space. Compare with
  `Displace::cmp_displaced` at the accumulated offset.
* **Writes open a node into its parent's frame before copying it** (pushing its
  dsp down a level), so copied spines carry dsp `0` and untouched subtrees stay
  shared. Roots persist normalized, so a root pointer is a bare content key.

`instance_template` is a graft: an occupied target block is refused, never
merged into.

### One root, paged (`grmpl-ent::granfilade`, `store`)

The granfilade has **one** mutable slot, the root record. Everything durable is
a tree reachable from it: the branch DAG and the branch enfilade, whose values
are each branch's whole state (clock, Rel enfilade, context enfilade, canopy).
**Never add a second meta key**; link a new tree from an existing one instead.

* **Root records land in staging order.** Every branch rewrites the one root, so
  staging takes the family's root lock and the group-commit queue is shared by
  every branch of a world. Lock order is edition → root → durability.
* **A paged node's frame outlives every handle that can still page it in.** GC
  roots are the root record *and* every paged node still unread in memory; a
  sweep removes swept keys from the granfilade's `present` set, so a resident
  node that loses its frame is written again if a later root reaches it.

### Extents and the spanfilade (`grmpl-ent::measure`, `spanfilade`)

Fact trees are measured by `(Count, Extent)`: per column, the bounding box of
the subtree's entity cells (grmpl's own summary; Gold's content trees cache
none). Three rules keep the searches it drives
exact:

* **An extent displaces exactly as its keys do.** It is stored in the node's
  local frame; `Measure::displace` shifts every bound by the dsp. A measure that
  ignored the dsp would prune the matches out of every grafted block.
* **`Tree::search(admit, keep)` needs the two tests to agree**: whenever `keep`
  accepts an entry, `admit` accepts every subtree holding it. Otherwise a match
  hides under a pruned subtree.
* **`instance_template` refuses a template whose extent leaves its block**, since
  a graft moves every entity cell.

The spanfilade records every graft by source span and by target span, per
branch. It is **append-only** — retraction and consolidation leave it alone — and
a fork into the past keeps only the grafts made by the fork edition.

Version compare (`Tree::diff`) skips a subtree only when both sides hold **the
same node at the same absolute position** (parent offset plus dsp). The same
node relocated holds different entries. `compare_spans` splices each graft's
source, as of the edition before it, into the earlier version, so the copy
compares as unchanged and is reported by span.

### History (`grmpl-ent::history`; `backfollow`, `shared_region`)

Gold's H-tree, as an index beside the immutable nodes, hung from root-record
slots 2–5 (`parents`, `holders`, `born`, `cursor`).

* **Commits never touch it.** `EntRoot::catch_up` indexes deferred; queries
  catch it up first, so answers are exact; `step_history` does it in durable
  steps. Indexing is idempotent, so lost progress is only repeated work.
* **Ancestors first.** Branches are indexed in id order, each from its fork
  point, so a node inherited through a fork is already born on the lineage and
  never re-walked; inherited versions are found through the DAG at query time.
* **`born` is per branch**: content addressing lets one node be built
  independently on two branches. Pruning by it is sound only as "born on the
  lineage by then", never as one global cut.
* **It holds content keys as data, never links**, so GC ignores it; a query
  skips versions consolidation retired, including ones it folded into the
  watermark checkpoint (several holders can stand for that one version).
* **Sharing is identity**: backfollow and `shared_region` find shared nodes,
  not equal values. Backfollow starts from the leaves of the span, as Gold
  does.

### Context and derived enfilades (`context`/`inherit`, `materialized view`)

* **A materialized view never changes an answer.** `Query::Materialized` means
  its plan. A read uses the stored copy only when the reader proves
  (`EditionReader::touched_since`) that no input moved since the copy's cursor;
  a delta uses the copy only when it is current at both ends. Anything else
  evaluates the plan.
* **The copy is the view's open linear form**: parameters as leading columns,
  no final `distinct` or aggregate, each row weighted by its derivations. Store
  the distinct set instead and `distinct`'s delta can no longer be read off it.
* **A refresh advances its cursor even over an empty delta**, or an unchanged
  copy becomes unprovably current and every read evaluates.
* **Scope spans are inclusive** (`first..=last`), so a block's own scope lies
  inside the block and survives `instance_template`'s self-containment check.
  The most specific span wins: latest start, then earliest end, then least
  value.
* `TraceStore::lookup` and `read_containing` default to read-and-filter, so a
  store without indexes is never slower for being asked; the Ent probes.

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
     └── grmpl-transport (in-process net between domains)

grmpl (public runtime facade: compiled worlds, sessions, and TCP adapter;
       source remains under crates/grmpl-session during the migration)
grmpl-conformance (dev-only: one law suite, every substrate)
```

The **Ent is the only substrate**: `grmpl-store` (the fjall LSM) was the
construction-time differential oracle and is deleted. The store contract is now
stated absolutely in `grmpl-ent/tests/store_laws.rs` — determinism, the
patch–edition law, history/consolidation, and fork identity, each against an
independent model — and every law of the language runs through
`grmpl-conformance`, which is what made the cutover checkable.

`grmpl` is an **edge** crate, *not* part of the semantic core: it sits
above the bright line and wires the core to clients, so it may name a concrete
transport (std TCP) exactly as an application would. The bright line constrains
the core crates, not the app built on them.
