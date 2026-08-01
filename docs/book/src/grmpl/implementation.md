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
  measure.rs      the WID monoid (Count, and the Measure trait)
  granfilade.rs   content-addressed node store; structural sharing on disk
  store.rs        the Edition + Fact enfilades = an EntStore (the store traits)
  dsp.rs          DSP coordinate transforms (Dsp, DspEnf)
  dag.rs          the branch/edition DAG (DagWood: Branch, BranchId, Dag)
  canopy.rs       interest routing (interval tree + endorsement flag-lattice)
  context.rs      the context enfilade (inherited scopes)
```

## The enfilade primitive — `tree.rs`, `measure.rs`

At the bottom is `Tree<K, V, M>`: a **persistent, measured B+ tree**. It is
immutable in the functional sense — `insert`/`remove` return a new tree and share
the old one's untouched subtrees (path copy) — and every node carries the
monoidal measure `M` of its subtree. A node holds a *run* of up to 64 entries (or
children), not one: a node is one content-addressed granfilade record, so arity
is the difference between one record per **run** of tuples and one per tuple.
That constant factor, not any asymptotic gap, is what decides whether the Ent can
sit under the language at all. `measure.rs` defines the
`Measure` trait (an associative fold with identity) and the canonical `Count`
measure.

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

## The Edition and Fact enfilades — `store.rs`

`EntStore` is where the plex becomes *a store*. It implements the `grmpl-core`
store traits by running two enfilades side by side, committed atomically together:

- **The Edition enfilade** serves `scan_updates` — the raw, commit-ordered,
  per-multiplicity delta log. Its subtlety is that each update's `submit_index`
  is stored as an **immutable node payload**, not derived from tree rank, so the
  log order survives rebalance, consolidation, and GC. `assert; retract; assert`
  is three entries, not a net.
- **The Fact enfilade** serves `read_at`/`holds`/joins — the net-per-tuple
  state, tuple-keyed and measured, delivered tuple-sorted with zero-weight tuples
  absent. Every live as-of root is persisted with its commit, so recovery is a
  root lookup. (It used to be written only at the watermark, with `open`
  replaying the whole log tail back through the fold — checkpoint-and-replay
  recovery, which is the LSM shape the mandate rules out from underneath the
  Ent.)

The **context enfilade** is committed beside them, and the durable catalog and
the edition-versioned schema registry are bindings in it at the root scope —
so `schema_at` is a WID range walk over the relation's `(rel, edition)` span
rather than a scan, and both survive GC as live roots.

A single commit opens one transaction over the granfilade, writes the touched
nodes of both enfilades plus the edition bump, and issues one durable sync — the
Patch–edition law realized as one atomic batch. The result conforms to the same
`FjallStore` contract the LSM store does (verified across 16 seeds × 150 rounds),
survives reopen, and runs the real `grmpl-diff`/`grmpl-proc` and the full MOO
runtime.

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

`Dag` (a `DagWood`, after Gold) is the `fulltrace`: the version/branch graph.
`fork_at(at)` returns a store on a new branch that **shares structure** with its
ancestor — an `O(edit)` virtual copy of the *world*, not an `O(state)` deep copy.
Every branch lives in one granfilade with its roots namespaced by branch, so the
fork writes roots naming nodes already present and encodes no node frames at all;
the branch graph is serialized alongside, so forks survive a reopen. The DAG
answers cross-branch provenance — `descends_from`, `common_ancestor_with` — and
reachability GC roots from every branch.

**Backfollow / version-compare** is implemented and correct, but not yet
subtree-pruned: `compare` short-circuits when two editions share a Fact root and
otherwise walks both sides. Carrying trace membership as a WID upward measure
(Gold's `HistoryCrum inTrace:`) is future work — see Part IV.

## The canopy — `canopy.rs`

The canopy is the interest index. Standing interests are held in a **measured
interval tree** (a max-hi segment tree, so a change *stabs* the tree for
overlapping interests in `O(log n + k)` rather than testing every watcher), gated
by an **endorsement flag-lattice** (`Endorsement`, `InterestId`) that routes
conservatively — a superset of what the pump will actually deliver, never a
subset.

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

That routes per *relation*. Routing per *key range* — which is what the canopy
indexes, and what would let two watchers on disjoint parts of one relation miss
each other's commits — needs the pump to register an interest with a range,
which it can only do for a view whose key span is known (the E2b `RangeRel`
pushdown). That step is what puts the `Canopy` type itself on the path, and it
has not been taken.

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

## Where the LSM stands now

`grmpl-store` (the fjall LSM) has not vanished. It survives as the
**differential conformance oracle** — an independent second leg the ent is
checked against — and as the showcase's substrate demo. The end-state plan
deletes it after a soak window, leaving a single ent-native on-disk format.

The oracle is no longer a handful of store-contract tests. `grmpl-conformance`
states a law once, against the substrate *traits*, and runs it on every
implementation: the whole of `grmpl-proc`, `grmpl-lang` and `grmpl-session` —
the optimistic commit protocol, the reactive pump, the scheduler, replay, GC
policy, schema enforcement, behaviours-as-relations, the concatenative surface,
the session runtime, and every seeded oracle among them — runs twice, once on
each substrate. "The language runs on the Ent" is a property the suite checks
rather than a sentence in this chapter. `grmpl run`, `grmpld` and `grmpl-bench`
open an `EntStore`.

Every distinctive Xanadu-`Ent` structural component is present: measured
enfilade, granfilade, WID pruning (load-bearing in the language), DSP transforms,
durable structural-sharing forks, Edition + Fact + context enfilades, branch DAG,
interval-tree canopy, and backfollow/version-compare. What remains is a mix of
optimizations and of *wiring* — the canopy and the DSP overlay are correct but
not yet on the running system's path, and the Derived enfilade is unbuilt. Plan
v5 (`docs/ENT-GAPS-PLAN.md`) tracks them; Part IV describes where they lead.
