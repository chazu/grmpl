# grmpl and the Xanadu `Ent` — how close is our implementation?

grmpl's `ent` is **not** short for "entity." It is named for, and consciously
derived from, the **`Ent`** at the heart of Project Xanadu's "Gold" design —
K. Eric Drexler's versioning enfilade. grmpl's founding note
([`idea.md`](../idea.md)) opens the whole project as a thought experiment on *"how
to 'complete' the ent data structure plex,"* with the explicit design criterion
that the language *"should use the ent (or the set of data structures we settle
on as our version of the ent) as its backbone."*

This note answers: **how much of Xanadu's `Ent` does `grmpl-ent` actually
implement?** It was rewritten after `grmpl-store` (the fjall LSM stand-in) was
deleted and `grmpl-ent` became the only substrate; the previous version assessed
the LSM.

**The headline:** the *design* is faithful to the Ent, and so, now, is the
core data structure. `grmpl-ent` realizes never-overwrite, path-copied
structural sharing, versions as roots, version compare that costs the size of
the change, content-addressed persistence, interest routing — and, since the
tree gained displacements, Gold's **dsps on pointers**, with persistent
split/join and an `O(log n)` **virtual copy** that the store uses for template
instancing. And the whole world is now *in* the Ent: one root record links to
the branch DAG and to the per-branch state, every directory and the canopy are
trees beneath it, and nodes page in on demand, so opening a world reads two
frames whatever its size. What remains short of the Ent is listed in §5:
summaries richer than a count, DSP-inherited context, a reverse index over
grafts, merges in the branch DAG, and Green's 2D enfilades.

---

## 1. What Xanadu Gold's `Ent` is (from the source)

`gold/udanax-top.st:6092`:

```smalltalk
Abraham subclass: #Ent
    instanceVariableNames: '
        oroots {MuTable NOCOPY smalltalk of: TracePosition and: OrglRoot}
        fulltrace {DagWood}'
    category: 'Xanadu-Be-Ents'!
```

An `Ent` is the **versioned-content backbone**: a map from `TracePosition` →
`OrglRoot` (each *orgl* is a content structure rooted in an **enfilade**) plus a
`fulltrace` **DAG** of the whole version history. Around it, the same file carries
the classic enfilade machinery:

* **`Loaf`/`Crum`** families — the nodes of a *measured tree* (a B-tree-like
  structure). `CanopyCrum`, `HistoryCrum`, `SensorCrum` are specializations.
* **`Dsp`** (displacement) family — a subtree's key is its parent's key *plus a
  displacement*, so relocating or virtually-copying a subtree is a cheap key
  change, and context flows **down** the tree.
* **wids** (widths) — every node advertises *the range of addresses its subtree
  covers*, so a sparse, effectively transfinite space is searchable in
  `O(depth)`; summaries flow **up** the tree.
* **`GrandNode`/`GrandHashTable`** — the *granfilade*, the persistent storage.

Two moves make the `Ent` powerful: **structural sharing** (a new version shares
every unchanged subtree, allocating new crums only along the edited path → an edit
or virtual copy is `O(edit)`), and **dual measures** (dsps carry context down,
wids summarize content up).

---

## 2. What grmpl means by "the Ent" (from the design note)

grmpl does *not* try to make one universal `Ent`. `idea.md` §1 defines the backbone
as **a coordinated family of persistent enfilades**:

> 1. **Fact enfilades** contain stored relations and their indexes.
> 2. **Edition enfilades** preserve historical roots, patches, branches, and
>    causal ancestry.
> 3. **Context enfilades** carry **DSPative** information down scopes: authority,
>    namespace, schema, permissions, placement…
> 4. **Canopy enfilades** index standing queries, subscriptions, sensors…
> 5. **Derived enfilades** maintain materialized views and incremental query
>    state.

over a common physical abstraction that is transparently the enfilade:

> ```
> persistent measured action tree
>     + stable node identities
>     + cheap split/join
>     + WIDative subtree summaries
>     + DSPative inherited context
>     + historical editions
>     + canopy indexes
> ```

This is Xanadu's `Ent`, **generalized** from a hypertext-document engine to a
relational/differential world substrate. The mapping is remarkably direct:

| Xanadu Gold `Ent`/enfilade         | grmpl's envisioned enfilade (`idea.md`)                         |
|------------------------------------|----------------------------------------------------------------|
| orgl content trees (`OrglRoot`)    | **Fact enfilades** — relations + their indexes                 |
| `fulltrace` DAG + versioned roots  | **Edition enfilades** — historical roots, patches, branches, ancestry |
| `Dsp` inherited displacement       | **Context enfilades** — DSPative authority/namespace/schema down scopes |
| `CanopyCrum` / upward interest     | **Canopy enfilades** — standing queries, subscriptions, sensors |
| (no equivalent)                    | **Derived enfilades** — differential materialized views (grmpl's addition) |
| wids (width summaries up)          | **WIDative measurements** — fact kinds, key ranges, entity counts, spatial bounds, dirty regions, subscription interests (`idea.md` §10) |

grmpl even keeps Xanadu's key discipline as an explicit law: **editions/snapshots
are opaque** (`idea.md` §10 — "A single-node implementation may use a
monotonically increasing commit number… User programs should not assume editions
are globally consecutive integers"), exactly the Xanadu stance that addresses are
handles, not integers you do arithmetic on.

So at the level of **design**, grmpl is a faithful, ambitious reconstruction of
the `Ent` — with two deliberate extensions Xanadu never had: the **relational /
Datalog** data model (facts, joins, recursive views) and **differential
dataflow** (Derived enfilades, `watch` = the maintained derivative of `find`).

---

## 3. What `grmpl-ent` implements

`grmpl-ent` builds every structure on one primitive, `tree::Tree<K, V, M>`, and
persists all of them through one node store, the `granfilade`.

**The primitive is a displaced B+tree — an enfilade with tuple coordinates.**

* Nodes hold up to 64 entries or children (`tree::B`), are immutable and
  `Arc`-held, and an insert path-copies only the root-to-leaf spine. A new
  version costs `O(log n)` new nodes and shares everything else. **This is
  genuine Ent-style structural sharing.**
* **Every pointer carries a dsp**, as on Gold's `DspLoaf`: a `Tree` handle is a
  shared node plus its displacement relative to its parent, and a node stores its
  keys in its own local frame. A descent accumulates dsps; reads move stored keys
  *up* to the query (`Displace::cmp_displaced`) rather than the query down, and
  writes push a node's dsp one level down as they copy it. `relocate` is `O(1)`.
* What a dsp displaces is grmpl's own coordinate: a tuple key moves by shifting
  every entity cell together (`dsp::Displace`). Separators and range pruning are
  still by key order, so this is an enfilade over ordered tuple coordinates rather
  than Udanax's tumbler widths — the wid is a cached measure, not a width.
* Persistent **split** and **join** cost `O(log n)` new nodes, and **graft** — the
  virtual copy — splits a span out, relocates it, and joins it back in elsewhere,
  sharing every interior node with the original.
* Each node caches a monoid **measure** of its subtree (`measure::Measure`). The
  only measure in use is `Count`, which answers "how many rows in this span" and
  "did anything change in this edition range" in `O(log n)`.
* Shape depends on the order of operations, so a content key identifies a
  *shape*, not a logical value. Sharing is within one version lineage.

**Structures built on it:**

Every structure is a tree, and every tree hangs from one root record:

```
root record ──► branch DAG         Tree<BranchId, Branch>          (Gold's fulltrace DagWood)
            └─► branch enfilade    Tree<BranchId, BranchState>     (Gold's oroots)
                  BranchState = clock, watermark,
                    ├─► Rel enfilade       Tree<RelId, RelRoots>
                    │     RelRoots ─► Version enfilade  Tree<edition, Fact tree>
                    │              ─► Edition log       Tree<(edition, i), update | graft>
                    │              ─► Arrangements      Tree<column, Fact tree>
                    ├─► context enfilade   catalog + schemas
                    └─► canopy, fired-set, interest registry
```

| Structure | What it is |
|---|---|
| Fact trees | one `Tree<Tuple, Diff, Count>` per relation per live edition |
| Edition log | `(edition, index) → update`, or one `Graft` entry per relation for a virtual copy |
| Version / relation directories | `edition → Fact root`, `relation → its roots` |
| Context tree | the name→`RelId` catalog and edition-versioned schemas, at the root scope |
| Canopy | interest intervals with a `max-hi` measure and an endorsement lattice, so a change routes only to watchers whose interval it stabs; persisted with the commits routed to it |
| Branch DAG | branches with at most one parent (a tree; no merges), and common-ancestor lookup |
| Granfilade | `SHA-256(frame) → frame` in fjall's `nodes` keyspace, one root record in `meta`, mark-and-sweep GC |

A tree holds another tree as a **link**: the linked root's content key rides in
the frame's reference run beside an internal node's children, so GC follows a
link exactly as it follows a child, and the payload records the linked tree's
dsp, size and measure. Internal frames likewise record each child's size and
measure, so a child read back from disk is a **paged** node: counted, measured
and comparable by key without being read. Opening a world reads the root record
and two frames; a read pages in the paths it walks. GC treats the root record
and every paged node still unread in memory as roots, so a pinned reader keeps
its version after consolidation retires it.

Everything the language stores goes through these trees: world relations,
inboxes, cursors, timers, counters, outboxes and materialized views are all
ordinary relations in the Fact trees.

**Operations:**

* **Version compare** (`Tree::diff`, used by `EntStore::compare` and the
  differential engine) skips any subtree the two versions share, by pointer or
  content key, so it costs the size of the difference. This is the Ent's
  version-comparison idea, realized.
* **Fork** (`EntStore::fork_at`) shares every fact node and writes none. Forking
  at the present shares the whole Rel enfilade and writes two frames (the new DAG
  and branch-enfilade leaves); forking into the past cuts each relation's version
  directory and log with a persistent split, `O(relations × log n)`.
* **Template instancing** (`EntStore::instance_template`) is a **graft** per
  relation: `O(log n)` new nodes and one log entry however large the template is. The edition log records a `Graft` entry
  and `scan_updates` expands it from that edition's Fact root, so watchers and
  replay see ordinary updates; the canopy routes it by span.

**Outside the Ent entirely:** the differential engine's working state —
arrangements and multisets in `grmpl-diff` — is plain in-memory hash maps. The
one persistent derived structure, `grmpl_proc::Materialized`, writes a view's
output into an ordinary relation, so it does live in the Ent, but nothing in the
runtime uses it yet.

---

## 4. Scorecard

| Ent / enfilade property | In the design (`idea.md`) | In `grmpl-ent` |
|---|---|---|
| Never overwrite; historical editions retained | ✅ core law | ✅ versions are roots; as-of reads |
| Opaque edition identity | ✅ explicit law | ✅ `Edition` is opaque to the language |
| Patch = guarded, atomic next edition | ✅ semantic center | ✅ `commit_if`, group-committed |
| Structural sharing / path copy | ✅ | ✅ `O(log n)` new nodes per commit |
| Version compare costs the edit | ✅ | ✅ `Tree::diff` prunes shared subtrees |
| Content-addressed persistent node store | ✅ granfilade | ✅ SHA-256 keyed, GC'd, paged on demand |
| One root; every structure a tree beneath it | ✅ the `Ent` object | ✅ root record → DAG + branch enfilade → everything |
| Measured tree with upward summaries | ✅ "WIDative summaries" | ⚠️ cached measures over ordered keys; `Count` only |
| **DSP displacements composing down the tree** | ✅ | ✅ a dsp on every pointer, accumulated by descent |
| **Cheap split / join** | ✅ "cheap split/join" | ✅ persistent, `O(log n)` new nodes |
| **Virtual copy / relocation** | ✅ | ✅ relocate `O(1)`; graft `O(log n)`, used for instancing |
| DSP-inherited context down scopes | ✅ Context enfilades | ❌ catalog and schemas only, at the root scope |
| Edition ancestry DAG (`fulltrace`) | ✅ Edition enfilades | ⚠️ a persisted enfilade, but a tree of branches: no merges |
| Canopy indexing interest | ✅ Canopy enfilades | ✅ interval routing, persisted with the commits routed to it |
| Derived state in the Ent | ✅ Derived enfilades | ⚠️ `Materialized` exists, unwired; engine state in memory |
| Sequences as measured enfilades (§6 parsing) | ✅ | ❌ |
| Udanax Green 2D enfilades (poom/span) | — | ❌ |

---

## 5. Verdict

The design is a faithful, ambitious generalization of the Ent, and the
implementation now has the Ent's core: its **versioning** (immutable versions,
path copying, cheap history, comparison that costs the change,
content-addressed persistence), its **coordinate system** (dsps on pointers,
`O(1)` relocation, `O(log n)` virtual copy by graft), and its **shape on disk**
(one root, `oroots` and `fulltrace` beneath it, every structure a tree, nodes
paged in on demand). It differs from Udanax in what the coordinates are:
ordered tuples whose entity cells move, rather than tumbler widths.

What is still short of the Ent:

1. **Summaries are thin.** The only measure is `Count`. Gold's wids also carry a
   subtree's extent in its coordinate space, which is what prunes a search in
   more than one dimension; ordered keys get by on separators, two dimensions
   would not.
2. **No reverse index.** Nothing answers "which instances share this template's
   nodes" — Green's spanfilade question. A graft points one way only, and
   **version compare across a graft** falls back to an in-order merge of the
   subtrees whose separators differ, costing the instance's size rather than its
   node count.
3. **DSP-inherited context.** Context enfilades that carry namespace, authority
   or placement down a scope tree are not built, and nothing in the language
   declares scopes yet; building the mechanism first would only produce another
   unused placeholder.
4. **The branch DAG has no merges.** It is a tree of branches, each with one
   parent.
5. **Green's 2D enfilades** (poom/span) have no counterpart.

Three costs of the paged layout are worth naming. A page-in that fails panics:
a paged node's frame is referenced by a durable parent and protected from GC
while any handle can reach it, so a missing frame means a damaged store, not a
race. Every branch rewrites the one root, so staging a commit takes a world-wide
lock (the `fsync` is still shared by the group, as before). And opening the
store's fjall database grows with the data — 4 ms at 1,000 rows, 90 ms at
200,000 — which is fjall's own recovery; the Ent's part of an open is two
frames at every size.

`store.rs` and everything above it — the `TraceStore` contract, the language,
the laws — were untouched by the coordinate change, which is what the bright line
promised: the tree changed underneath, and the conformance and law suites passed
unchanged.

---

### Sources & method

* Xanadu Gold read directly: [`dotmpe/udanax-mpe`](https://github.com/dotmpe/udanax-mpe)
  `gold/udanax-top.st` — the `Ent` class (line 6092), and the `Loaf`/`Crum`/`Dsp`/
  `Orgl`/`CanopyCrum`/`GrandNode` families; `gold/udanax-spaces.st` — the
  `Arrangement`/`Dsp` coordinate spaces. Background:
  [Enfilade (Xanadu)](https://en.wikipedia.org/wiki/Enfilade_(Xanadu)),
  [xanadu.com/tech](https://xanadu.com/tech/).
* grmpl read directly: [`idea.md`](../idea.md) and `crates/grmpl-ent/src/`
  (`tree.rs`, `granfilade.rs`, `store.rs`, `dsp.rs`, `context.rs`, `canopy.rs`,
  `dag.rs`, `measure.rs`).
