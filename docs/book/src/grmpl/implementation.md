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

At the bottom is `Tree<K, V, M>`: a **persistent, weight-balanced, measured
tree**. It is immutable in the functional sense — `insert`/`remove` return a new
tree and share the old one's untouched subtrees (path copy) — and every node
carries the monoidal measure `M` of its subtree. `measure.rs` defines the
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
on disk**: a node's identity is its `ContentKey`, computed as
`hash(kind, measure-relevant fields, child content-keys)` — closed over child
*content keys only*, with physical id and allocation order excluded by
construction. So two logically-identical subtrees, however they were built,
intern to **one** stored node. A new edition writes only the nodes along its
edited path; everything else is already present under its content key and is
shared.

This is the mechanism behind cheap history *on disk*, not just in memory: a
commit grows the store by only the edited path, and reachability GC from retained
roots reclaims what no live edition points at.

> Node identity carries three roles by three mechanisms, because a single naïve
> id fails: a **content key** interns nodes for dedup and sharing (order- and
> pointer-independent, so structural sharing is canonical); a monotonic
> **`phys_id`** places nodes for locality; and equality that the language relies
> on is at the **leaf/query level**, not the tree shape — because balance
> heuristics legitimately vary internal shape, exactly as Gold's do.

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
  absent.

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
`fork_edition(at)` returns an opaque `Edition` on a new branch that **shares
structure** with its ancestor (an `O(edit)` virtual copy of the *world*, not an
`O(state)` deep copy). The DAG answers cross-branch provenance — `descends_from`,
`common_ancestor_with` — and reachability GC uses it to know which roots are
still live. Branch membership is carried as a WID upward measure (Gold's
`HistoryCrum inTrace:`), so **backfollow / version-compare** — "this content is
edition E, relocated" — is `O(measure)`, subtree-pruned, not a scan.

## The canopy — `canopy.rs`

The canopy is the interest index. Standing interests are held as a **measured
interval tree** (a max-hi segment tree, so a change *stabs* the tree for
overlapping interests in `O(log n + k)` rather than testing every watcher),
gated by an **endorsement flag-lattice** (`Endorsement`, `InterestId`) that
routes conservatively — a superset of what the pump will actually deliver, never
a subset. This is `CanopyCrum` with interest as the upward measure: a fact change
is routed only to the observers whose interest covers it.

## The context enfilade — `context.rs`

`Context` carries inherited scope down: namespace, schema, placement. It is the
DSPative-context generalization of dsps from "displacement" to "everything a
subtree should receive from where it sits." Authority itself lives in the
canopy's endorsement lattice (Gold-faithful — authority was never a dsp), while
namespace/schema/placement inherit down the context enfilade.

## Where the LSM stands now

`grmpl-store` (the fjall LSM) has not vanished. It survives as the
**construction-time conformance oracle** — an independent third leg the ent is
checked against — plus the `grmpl-bench` baseline and the showcase's substrate
demos. The end-state plan deletes it after a soak window, leaving a single
ent-native on-disk format; today it is deliberately kept as the differential
truth against which the Ent is proven.

Every distinctive Xanadu-`Ent` structural component is present and correct:
measured enfilade, granfilade, WID pruning (load-bearing in the language), DSP
transforms, structural-sharing fork, Edition enfilade + branch DAG, interval-tree
canopy with flag-lattice, and backfollow/version-compare. What remains are
**performance optimizations of already-correct capabilities** — the subject of
Part IV.
