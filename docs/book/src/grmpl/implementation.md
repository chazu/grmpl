# grmpl-ent: the implementation

The `grmpl-ent` crate is the `Ent` made real: a coordinated family of persistent,
measured, versioned enfilades over a shared granfilade, implementing the
`grmpl-core` store traits so the whole language runs on it. It was built
bottom-up, each step tested against the LSM store as a differential oracle and
against the full feature suite. This chapter walks its modules, each tied back to
the Part I mechanism it realizes.

```text
crates/grmpl-ent/src/
  tree.rs         the enfilade primitive — persistent measured tree
  measure.rs      the WID monoid family (Measure; Count, SumDiff, KeyBounds)
  hash.rs         SHA-256, vendored and pinned — the content-key hash
  granfilade.rs   content-addressed node store; structural sharing on disk
  store.rs        the enfilades that make an EntStore (the store traits)
  dsp.rs          DSP coordinate transforms (Dsp, DspEnf)
  dag.rs          the branch/edition DAG (DagWood: Branch, BranchId, Dag)
  canopy.rs       interest routing (interval enfilade + endorsement flag-lattice)
  context.rs      the context enfilade (inherited scopes)

crates/grmpl-proc/src/
  derived.rs      the Derived enfilade — materialized views that survive reopen
```

## The enfilade primitive — `tree.rs`, `measure.rs`

At the bottom is `Tree<K, V, M>`: a **persistent, measured B+ tree**. It is
immutable in the functional sense — `insert`/`remove` return a new tree and share
the old one's untouched subtrees (path copy) — and every node carries the
monoidal measure `M` of its subtree. A node holds a *run* of up to 64 entries (or
children), not one: a node is one content-addressed granfilade record, so arity
is the difference between one record per **run** of tuples and one per tuple.
That constant factor, not any asymptotic gap, is what decides whether the Ent can
sit under the language at all. A node holding a run of crums is, in Gold's terms,
a **loaf**.

`measure.rs` defines the `Measure` trait (an associative fold with identity) and
a small **family** of measures, because a product of monoids is a monoid — a tree
can carry several upward summaries at once with no new tree machinery:

- **`Count`** — the entry count. Every enfilade carries at least this; it is what
  makes "how many" a read of cached summaries.
- **`SumDiff`** — the Σ of entry *weights* beneath a node. The Fact enfilade's
  values *are* net weights, so this makes "what is the total weight over this key
  span" an `O(log n)` fold rather than a materialize-then-sum: the difference
  between an aggregate reading the tree's *shape* and reading its *rows*.
- **`KeyBounds`** — the least and greatest key beneath a node. This is not what
  prunes an ordinary range read (a B+ node's separators already carry that); it
  earns its place in comparisons *between two versions*, where a subtree whose
  key span is disjoint from the other side's cannot contribute a difference and
  so can be dismissed without descent.

The Fact enfilade carries the pair `(Count, SumDiff)`; the canopy carries its own
`Reach`. Adding a measure is adding a monoid, not a tree.

The primitive already delivers the two Part I superpowers in miniature:

- `t.measure()` is the whole-tree fold; `t.measure_range(&lo, &hi)` is the
  **WID-pruned** range fold — it reads node summaries and skips whole subtrees to
  answer "how much lies in `[lo, hi)`" in `O(log n)`.
- Old versions are immutable and cheap: a retained snapshot is unaffected by
  later edits, and the edit that produced the new version allocated only
  `O(log n)` nodes.

A seeded law oracle churns random insert/remove operations and checks, every
round and across 24 seeds, that the tree agrees with a `BTreeMap` reference on
ordering, membership, size, total measure, and range measure — and that retained
snapshots never mutate.

## The granfilade — `granfilade.rs`

The granfilade is the persistent node substrate: a **content-addressed** node
table (fjall used purely as a node blob store). Its job is **structural sharing
on disk**: a node's identity is its `ContentKey` — the SHA-256 of its frame,
closed over its entries and its children's content keys, with physical id and
allocation order excluded by construction. A new edition writes only the nodes
along its edited path; everything else is already present under its key.

Sharing is **within a version lineage**. A content key identifies a *shape*, and
because the tree's balance depends on insertion order, two different histories
reaching the same logical map get different keys. That is deliberate: identity in
grmpl is logical, not structural — `DESIGN.md` and the store contract define it
by `iter`/`scan_updates`, and Gold likewise guarantees content/version identity
and never byte-identical node layout.

Commits are path-sized in **work** as well as in bytes: each node memoizes its
content key, and a node that is both memoized and known-durable here is returned
without being re-serialized — along with everything beneath it, since a node's
key closes over its children's. The memo alone would not be safe, because it says
"this is its key", not "it is on *this* disk": a fork clones nodes memoized
against its parent's granfilade, so each granfilade also tracks which keys it
knows are durable.

This is the mechanism behind cheap history *on disk*, not just in memory: a
commit grows the store by only the edited path, and reachability GC from retained
roots reclaims what no live edition points at.

> The hash is part of the on-disk format, so it is pinned: SHA-256, vendored and
> checked against the FIPS vectors. The previous `DefaultHasher` had neither
> property it needed — `std` does not specify its algorithm across releases (a
> granfilade could hash differently under a new toolchain), and its fixed known
> keys are not collision-resistant against player-supplied content, where a
> collision means one node silently aliasing another.

## The store's enfilades — `store.rs`

`EntStore` is where the plex becomes *a store*. Part II described the plex as a
family of **five** enfilades; that is the design's taxonomy, by *job*. Physically
the store's state is more finely divided than that — and, since G-2a, it is **one
enfilade root, ordered all the way down**. Where the state used to be a handful
of `HashMap`s holding trees, every directory is now itself a measured tree, so
"how many relations," "how many live editions," and "which orderings does this
relation carry" are all *measures* rather than iterations, and none of them
depends on hash order (which the Determinism invariant explicitly warns about).

The two that carry the semantics:

- **The Edition enfilade** (`(edition, submit_index) → (tuple, diff)`) serves
  `scan_updates` — the raw, commit-ordered, per-multiplicity delta log. Its
  subtlety is that each update's `submit_index` is stored as an **immutable node
  payload**, not derived from tree rank, so the log order survives rebalance,
  consolidation, and GC. `assert; retract; assert` is three entries, not a net.
- **The Fact enfilade** (`tuple → net Σdiff`) serves `read_at`/`holds`/joins —
  the net-per-tuple state, tuple-keyed and measured, delivered tuple-sorted with
  zero-weight tuples absent. It carries the `(Count, SumDiff)` measure pair.

And the four directories that hold them, each an enfilade in its own right:

- **The Version enfilade** (`edition → Fact root`) — one persistent root per live
  as-of edition. This is Gold's `oroots` exactly: a map from a trace position to
  the content root current at it. Because `last_le` on a measured tree is a
  descent, an as-of read is a lookup rather than a search — and because every
  live root is persisted with its commit, **recovery is a root lookup**. (It used
  to be written only at the watermark, with `open` replaying the whole log tail
  back through the fold — checkpoint-and-replay recovery, which is the LSM shape
  the mandate rules out from underneath the Ent.)
- **The Rel enfilade** (`rel → its roots`) — the directory of live relations,
  each holding its Version enfilade, its Edition log, and its Arrangements.
- **The Arrangement enfilade** (`lead column → the same facts, rotated`) —
  alternate orderings of one relation's facts, so a predicate on a *trailing*
  column can prune too. Covered under WID pruning below.
- **The fired-interest enfilade** (`(interest, edition) → ()`) — which interests
  each commit stabbed. Keyed interest-first, so "did interest *i* fire anywhere
  in `(from, to]`?" is a single WID range measure. Covered under the canopy.

Beside them sit the **context enfilade** — carrying the durable catalog and the
edition-versioned schema registry as bindings at the root scope, so `schema_at`
is a WID range walk over the relation's `(rel, edition)` span rather than a scan
— the **canopy**, and the **branch enfilade** of the `DagWood`. All of them are
live GC roots.

A single commit opens one transaction over the granfilade, writes the touched
nodes of every one of them plus the edition bump, and issues one durable sync —
the Patch–edition law realized as one atomic batch. A commit writes only the
version it created, not every live one. The store contract this satisfies —
determinism, the patch–edition law, history and consolidation, fork identity — is
stated **absolutely** in `grmpl-ent/tests/store_laws.rs`, each law against an
independent model, and the result survives reopen and runs the real
`grmpl-diff`/`grmpl-proc` and the full MOO runtime.

## WID pruning, load-bearing in the language — `store.rs` + the lowerer

The point of wids is to make queries sublinear, and in grmpl they *are* — end to
end, not as a private store optimization. A `TraceStore::read_range` substrate
primitive (default: filtered `read_at`; on the Ent: the `O(result + log n)`
enfilade range walk) feeds a `RangeRel` operator in `grmpl-diff`, and the
`grmpl-lang` lowerer **auto-emits it** from a lead-column equality. So an
entity-keyed view like `view v(x) { r(x, …) }` prunes to the matching key *at the
source* instead of scanning the relation. The pushdown is an evaluation-time
rewrite, so the typed, inspected IR stays the plain `Rel`+`Filter` — the wid walk
is invisible to the program and load-bearing underneath it.

**Trailing columns prune too, through Arrangements.** The primary order prunes on
the lead column only, so a predicate on any *other* column would have to scan. An
**Arrangement** is one more measured tree over the same facts, rotated so the
column of interest leads — after which pruning on it is the ordinary WID range
walk. The rotation is invertible and preserves distinctness, so an Arrangement
holds exactly the same facts as the primary order and is maintained with every
commit; it is built on first use, so the cost is paid only for columns some query
actually asks about. The substrate side is `read_range_on`, `grmpl-diff` has a
`RangeRelOn` operator, and the lowerer auto-emits it from a trailing-column
equality — preferring the lead column when a query offers both, since a lead
range prunes on *any* store while a trailing one prunes only where an Arrangement
is kept. On a store without one, `read_range_on` falls back to read-and-filter,
which is exactly the `Filter` it replaced.

This is the shape the design predicted: "multi-order arrangements" are not a new
mechanism but the same primitive replicated per order — more measured trees.

## DSP coordinate transforms — `dsp.rs`

`Dsp` and `DspEnf` implement the displacement algebra: `DspNode(child,
displacement)` shares the child subtree and composes the displacement on the way
down. This is `O(1)` relocation and the `O(edit)` virtual copy — the one thing
content-hashing *alone* cannot do, because it relocates shared content rather
than duplicating it.

The subtle part landed in `E6c`: the dsp is **threaded through the WID walk**.
`DspEnf` answers a displaced `range` / `measure_range` by transforming the
*query* into the shared tree's own coordinates and pruning there — so a virtual
copy is still searched in `O(result + log n)` with no materialization. Relocation
and fast search compose instead of fighting.

In the playable world this is *instancing*: the MOO's `enter vault` / `leave`
verbs spin up a private, disjoint sub-world as a DSP virtual copy of a template
and tear it down again — each party's dungeon sharing the template's structure
until it diverges.

## The branch/edition DAG — `dag.rs`

`Dag` is grmpl's `DagWood` — [the branch structure of the
`fulltrace`](../enfilade/the-ent.md#the-dagwood). The Edition enfilade records
history *within* a branch, linearly; the `DagWood` records it *between* branches,
each remembering its parent and the exact edition it forked from. A version point
is the pair `(branch, edition)` — Gold's `TracePosition`.

`fork_at(at)` returns a store on a new branch that **shares structure** with its
ancestor — an `O(edit)` virtual copy of the *world*, not an `O(state)` deep copy.
Every branch lives in one granfilade with its roots namespaced by branch, so the
fork writes roots naming nodes already present and encodes **zero** node frames;
a 5000-row world forks for the cost of the roots. The branch registry is itself
an enfilade like every other directory in the store's state — not a `BTreeMap`
beside them — so "how many branches" is a measure, iteration is ordered, and a
retained `Dag` is a persistent version of the graph that a later fork cannot
mutate underneath it. Forks survive a reopen.

The DAG answers the two cross-branch provenance questions — `descends_from`
(reachability, the happens-before relation) and `common_ancestor_with` (the merge
base) — and reachability GC roots from every branch.

**Backfollow / version-compare** is subtree-pruned at every level. `compare`
short-circuits when two editions share a Fact root — but it also does so at
*every node beneath* the root, because two versions that share a subtree share it
by content key, and a subtree whose `KeyBounds` are disjoint from the other
side's cannot contribute a difference. So comparing two editions costs the size
of the *difference*, not the size of the relation: structural sharing, which
makes history cheap to store, is the same thing that makes history cheap to
compare. This is Gold's `HistoryCrum inTrace:` in grmpl's terms — trace
membership answered as an upward measure rather than a walk.

## The canopy — `canopy.rs`

The canopy is the interest index — grmpl's `CanopyCrum`/`SensorCrum`. Standing
interests are held in a **measured interval tree**: each node's upward measure
(`Reach`) carries the maximum high-endpoint beneath it, which is what lets a
change *stab* the tree for overlapping interests in `O(log n + k)` rather than
testing every watcher — a subtree whose `max_hi` falls below the query point
cannot contain an overlap and is never descended into.

`Reach` carries a second component: an **endorsement flag-lattice**
(`Endorsement`) summarizing the endorsements of everything beneath. It is a
lattice ordered by inclusion — the join is union, the top (`ALL`) dominates
everything — so a subtree can be dismissed when its *union* of endorsements
already fails to dominate what the query requires. Because the summary is a
union, it is **conservative by construction**: it can only ever over-approximate
what lies below, so routing yields a superset of what the pump will actually
deliver, never a subset. A false positive costs a wasted evaluation; a false
negative would be a lost update, and the lattice makes that unrepresentable.

It is a real enfilade: interests live in the same persistent measured tree as
everything else, keyed `(rel, lo, id)` so one relation's interests are a
contiguous low-endpoint-ordered span, and it round-trips through the granfilade.
Registering is an `O(log n)` persistent insert.

**Routing is load-bearing, but by a coarser mechanism than the canopy.** The
reactive pump no longer re-evaluates its view on every pump: it asks the
substrate, through `TraceStore::touched_since`, whether an interval could have
touched the view's base relations, and the Ent answers from the *Edition*
enfilade's cached measures in `O(log n)`. The contract is conservative — `false`
is a proof of no change, `true` merely means "possibly" — so the default `true`
leaves every store correct and only a store that can prove a negative saves the
work. Measured: 50 installed watches, one commit to a relation none of them
reads, zero differential evaluations.

**And per key range, through the canopy itself.** When a view reads one relation
through one key span — the shape the E2b pushdown produces for an entity-keyed
view — the pump asks `touched_range_since` instead, and the Ent answers it from
the canopy: the interest is registered on first ask, every commit stabs the
canopy as it lands, and the answer is then a WID measure over a fired-interest
enfilade keyed interest-first. So two watchers over disjoint spans of one
relation do not wake each other, which is precisely what relation-wide routing
cannot express. An interval reaching back before an interest existed widens to
the relation-wide answer, since nothing was routed to it then — conservative,
never a false negative.

## The context enfilade — `context.rs`

The context enfilade carries inherited scope down: namespace, schema, placement.
It is the DSPative-context generalization of dsps from "displacement" to
"everything a subtree should receive from where it sits." Authority itself lives
in the canopy's endorsement lattice (Gold-faithful — authority was never a dsp).

It is a real enfilade — a persistent measured tree over the granfilade, versioned
and GC-rooted like the others — and it is load-bearing: the **durable catalog**
and the **edition-versioned schema registry** are bindings in it at the root
scope. Both are invariants `CLAUDE.md` names, and both used to exist only on the
LSM, so a world running on the Ent had no durable names and committed with
`NoSchemas`.

## The Derived enfilade — `grmpl-proc/src/derived.rs`

The fifth member of the plex, and the one with no Xanadu ancestor. What existed
before it was `grmpl-diff`'s arrangement memo, keyed by `(Arc::as_ptr(node),
edition)` — which by its own docstring could not outlive a single advance. A
view's incremental state died with the process.

The realization is deliberately **not** a new storage mechanism. A materialized
view *is an ordinary relation* — one the engine maintains rather than one a
handler writes — so it lands in the Fact enfilade and inherits everything that
comes with being there: versioned by edition, structurally shared across
editions, persisted with the commit that produced it, reachable from GC, carried
by a fork, readable as-of any live edition. The design note asked for "a
persistent, shared derived tree … like the Fact enfilade does"; the answer turned
out to be *the Fact enfilade*.

Maintenance mirrors `on watch`: a durable cursor marks the frontier already
folded in, `refresh` evaluates the view's delta over `(cursor, current]` and
commits it into the derived relation as one atomic step, and that commit is
preconditioned on the cursor — so two maintainers racing the same interval
resolve to exactly one winner, by the ordinary optimistic-commit rule rather than
by a lock. It is gated by the same `touched_since` routing as everything else, so
a refresh over an interval that could not have touched the view does no
differential work at all.

## Where the LSM stood, and why it is gone

`grmpl-store` — the fjall LSM that stood in for the Ent while the language was
built — **has been deleted.** The Ent is the only substrate. `fjall` survives in
exactly one role, as the granfilade's node blob store, which is the only place in
the workspace that names it.

Deleting it was done last, and only after its law suites were ported, because the
LSM was carrying something real: it had been the **differential oracle**, and a
law expressed as "the ent agrees with the other store" evaporates when the other
store does. So the store contract is now stated **absolutely** in
`grmpl-ent/tests/store_laws.rs` — determinism, the patch–edition law, history and
consolidation, fork identity — each against an *independent model* rather than
against a second implementation. Deleting the LSM without that would have deleted
the laws along with it.

`grmpl-conformance` remains, and is the reason the cutover was checkable at all.
It states a law once, against the substrate *traits*, and runs it against every
implementation: the whole of `grmpl-proc`, `grmpl-lang` and `grmpl-session` — the
optimistic commit protocol, the reactive pump, the scheduler, replay, GC policy,
schema enforcement, behaviours-as-relations, the concatenative surface, the
session runtime, and every seeded oracle among them, some ninety laws. "The
language runs on the Ent" is a property the suite checks rather than a sentence
in this chapter. Every binary — `grmpl run`, `grmpl showcase`, `grmpld`,
`grmpl-bench` — opens an `EntStore`.

Every distinctive Xanadu-`Ent` structural component is present and on the running
system's path: measured enfilade, granfilade, WID pruning on lead *and* trailing
columns, DSP transforms (on the instancing path), durable structural-sharing
forks, the Fact / Edition / Version / Rel / Arrangement / context / canopy /
branch enfilades, the `DagWood`, an interval-enfilade canopy the pump routes
through, subtree-pruned backfollow, and a persistent Derived enfilade. No module
in `grmpl-ent` has zero callers.

What remains is not wiring but *reach*: the enhancements the structure makes
newly possible. Part IV describes them.
