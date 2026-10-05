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

**The headline:** `grmpl-ent` has Gold's versioning core: never-overwrite,
path-copied structural sharing, versions as roots, a persisted version DAG,
`O(1)` relocation by displacement, virtual copy by sharing subtrees, and since
step 4 Gold's history layer: every node's containers indexed, so backfollow
finds every version holding some content. Around
that core it makes choices of its own: an ordered B+ tree instead of Gold's
binary splay tree (since step 6, binary splits on any column are a
per-relation alternative), immutable content-addressed nodes instead of
identity-addressed objects updated in place, facts identified by value, and
cached `Count`/`Extent` summaries that Gold's content trees do not have. Since
step 5 its version DAG has merges, built by replaying patches. It does not
have Gold's canopies or recorders, and will not: they were examined and
declined, because they pay under conditions grmpl deliberately lacks (see
[`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md#declined-canopies-and-recorders-g5-g6)).

This note was first written from a coarse reading of Gold. A later
line-by-line audit, [`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md), corrected it; where
the two disagree, the audit is the source-backed one. The open gaps are kept in
[`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md).

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

The `oroots` table is vestigial: it is built `smalltalkOnly` and its stores are
commented out. **The live Ent is `fulltrace`**, a `DagWood` that hands out
`TracePosition`s and orders them. Around it, the same file carries the rest of
the backend (details and line citations in
[`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md) §1):

* **The trace.** A partial order of versions. Branching is implicit, and
  `newSuccessorAfter:` makes a position with **two parents**, a merge. Every
  derived edition (copy, transform, combine) gets a new trace position.
* **The O-tree** (`Loaf` family) holds an orgl's content. It is a **binary
  splay tree**: a `SplitLoaf` splits on one dimension's distinction, a
  `DspLoaf` displaces its one child, and leaves cover regions, some of them
  lazy or placeholders. Its nodes cache **no** summary of what lies below.
* **`Dsp`** (displacement) family — a `DspLoaf`'s child is displaced by its
  dsp, so relocating or virtually copying a subtree is `O(1)` and shares it.
* **The H-tree** (`HistoryCrum`) inverts the O-tree: every node records the
  nodes that contain it. Walking it upward answers "which editions hold this
  content" (backfollow) and "what do these two editions share" (compare).
* **Canopies** (`CanopyCrum`) are shared trees of permission and endorsement
  flags OR-ed upward, hung over the history and content trees, which prune
  backfollow and standing queries.
* **Recorders and the Agenda**: standing backfollow queries, and persistent
  background steps for unbounded work.
* **Persistence**: identity-addressed objects (`Abraham`s) updated in place,
  written in flocks and snarfs under a fixed root (the `Turtle`). Gold never
  uses the word "granfilade"; `GrandHashTable` is a collection, not the store.

Two moves make the `Ent` powerful: **structural sharing** (a new version shares
every unchanged subtree, so an edit or a virtual copy is `O(edit)`), and
**history** (shared content knows everything that contains it).

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
| `Dsp` displacement                 | **Context enfilades** — DSPative authority/namespace/schema down scopes (an extrapolation) |
| sensor canopy + recorders          | **Canopy enfilades** — standing queries, subscriptions, sensors |
| (no equivalent)                    | **Derived enfilades** — differential materialized views (grmpl's addition) |
| canopy flags OR-ed up; regions at leaves and roots | **WIDative measurements** — fact kinds, key ranges, entity counts, spatial bounds, dirty regions, subscription interests (`idea.md` §10) |

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
* **Every pointer carries a dsp.** Gold puts a dsp in a separate `DspLoaf`
  node; grmpl folds it into the handle, which is equivalent: a `Tree` handle is a
  shared node plus its displacement relative to its parent, and a node stores its
  keys in its own local frame. A descent accumulates dsps; reads move stored keys
  *up* to the query (`Displace::cmp_displaced`) rather than the query down, and
  writes push a node's dsp one level down as they copy it. `relocate` is `O(1)`.
* What a dsp displaces is grmpl's own coordinate: a tuple key moves by shifting
  every entity cell together (`dsp::Displace`). Separators and range pruning are
  still by key order, so this is an enfilade over ordered tuple coordinates rather
  than Gold's coordinate-space regions; the summary is a cached measure.
* Persistent **split** and **join** cost `O(log n)` new nodes, and **graft** — the
  virtual copy — splits a span out, relocates it, and joins it back in elsewhere,
  sharing every interior node with the original.
* Each node caches a monoid **measure** of its subtree (`measure::Measure`).
  `Count` answers "how many rows in this span" and "did anything change in this
  edition range" in `O(log n)`. Fact trees also carry an **`Extent`**: per
  column, the least and greatest entity id under the subtree: the subtree's box
  in the space a dsp moves, stored in the node's local frame and displaced on the
  way down. Gold's content trees cache no such summary; the extent is grmpl's
  own, in the spirit of `idea.md`'s "WIDative summaries".
  `Tree::search` walks any measure this way, skipping (and never paging in) a
  subtree whose summary rules it out, so a query on a column the tree is not
  ordered by still prunes.
* Shape depends on the order of operations, so a content key identifies a
  *shape*, not a logical value. Sharing is within one version lineage.

**Structures built on it:**

Every structure is a tree, and every tree hangs from one root record:

```
root record ──► branch DAG         Tree<BranchId, Branch>          (Gold's fulltrace DagWood)
            └─► branch enfilade    Tree<BranchId, BranchState>
                  BranchState = clock, watermark,
                    ├─► Rel enfilade       Tree<RelId, RelRoots>
                    │     RelRoots ─► Version enfilade  Tree<edition, Fact tree>
                    │              ─► Edition log       Tree<(edition, i), update | graft>
                    │              ─► Arrangements      Tree<column, Fact tree>
                    ├─► context enfilade   catalog + schemas
                    ├─► canopy, fired-set, interest registry
                    └─► spanfilade         Tree<(source, target, edition), graft>
                                           and the same keyed (target, source, edition)
```

| Structure | What it is |
|---|---|
| Fact trees | one `Tree<Tuple, Diff, (Count, Extent)>` per relation per live edition |
| Edition log | `(edition, index) → update`, or one `Graft` entry per relation for a virtual copy |
| Version / relation directories | `edition → Fact root`, `relation → its roots` |
| Context tree | the name→`RelId` catalog and edition-versioned schemas, at the root scope |
| Canopy | interest intervals with a `max-hi` measure and an endorsement lattice, so a change routes only to watchers whose interval it stabs; persisted with the commits routed to it |
| Spanfilade | every graft, keyed by source span and again by target span, measured by the hull of its spans |
| Branch DAG | branches with at most one parent (a tree; no merges), and common-ancestor lookup |
| Granfilade | `SHA-256(frame) → frame` in fjall's `nodes` keyspace, one root record in `meta`, mark-and-sweep GC. The name is Udanax Green's; Gold stores identity-addressed objects updated in place |

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
  differential engine) skips any subtree the two versions share at the same
  position, by pointer or content key, so it costs the size of the difference.
  It walks each version as a frontier of whole subtrees rather than pairing
  nodes by their separators, so a shared subtree is found however the spines
  above it were rebuilt, by a split, a fuse, a join or a graft's seams.
  `EntStore::compare_spans` goes one step further, as Green's compare does: it
  reports each graft in the interval as the span it copied, read from the
  spanfilade, and lists only the rows that differ beyond the copies. This is
  the Ent's version-comparison idea, realized.
* **Fork** (`EntStore::fork_at`) shares every fact node and writes none. Forking
  at the present shares the whole Rel enfilade and writes two frames (the new DAG
  and branch-enfilade leaves); forking into the past cuts each relation's version
  directory and log with a persistent split, `O(relations × log n)`.
* **Template instancing** (`EntStore::instance_template`) is a **graft** per
  relation: `O(log n)` new nodes and one log entry however large the template is. The edition log records a `Graft` entry
  and `scan_updates` expands it from that edition's Fact root, so watchers and
  replay see ordinary updates; the canopy routes it by span. The template's
  precondition — every entity it names lies in its block — is checked from the
  block's extent, reading two spines, and a template that names an outside
  entity is refused.
* **Column search** (`EntStore::search_at`) finds the facts whose entity
  columns fall in a box, at any live edition, by walking the extents. The as-of
  `read_range_on` uses it; at the current edition, `read_range_on` still uses
  an Arrangement (below).
* **Provenance** (`copies_of`, `sources_of`, `origin_of`) answers Green's two
  questions about a virtual copy from the spanfilade: where a block was copied
  to, and where a block came from, following a chain of copies back to the
  entity it started as. The index is the D4M layout — one sparse array,
  *source × target*, stored beside its transpose — and is append-only, as
  Green's spanfilade is: retracting an instance does not erase that it was
  made.

Two ways to answer a question about a column the tree is not ordered by now sit
side by side, which makes their trade measurable (`docs/PERFORMANCE-ENT.md`
§7). An **Arrangement** is the D4M answer: a second copy of the facts, rotated
so the column leads — exact in `O(log n + k)`, paid for with a build on first
use and a second write on every commit. The **extent** is Gold's answer: no
second copy, and pruning exactly as good as the column's locality — tight when
the column tracks the key (a room's exits lead to nearby rooms), useless when it
is scattered.

**Context enfilades: scopes over entity blocks.** A world declares a scope
relation (`context scopes`) whose rows bind a key to a value across an
inclusive span of entity ids, and a view atom `inherit scopes(e, "key", v)`
gives an entity the binding of the most specific span containing it — for
nested spans, the innermost. It is DSP-inherited context in the plain sense:
the spans are coordinates the dsps move, so grafting a template block carries
its scopes to the instance, displaced with its rooms, and an inner scope can
be retracted to let the outer one show through. The scope relation is an
ordinary Fact tree keyed by span start, and finding the spans that contain an
entity (`read_containing`) is an extent search: the bounds of the `first` and
`last` columns are an interval tree's min-low and max-high, so a stab reads a
few frames however many scopes there are.

**Derived enfilades: materialized views.** A `materialized view` keeps its
open form — parameters as leading columns, before its final `distinct`, each
row weighted by its number of derivations — in a relation in the Ent,
maintained per commit by a durable cursor. Those weights are the state
`distinct` needs to be maintained by its changes alone. So a read of a current
copy is a primary-order range read, a watch's delta is `compare` on the copy
between two editions (the size of the edit), and a refresh folds a linear
delta in. A read checks whether the view's inputs moved since the copy's cursor
and evaluates the view if they did, so materializing never changes an answer.
Under it, join maintenance reads only the rows of the unchanged side that match
the change's keys (`TraceStore::lookup`): index probes on the primary order,
or on an Arrangement, which is how the Arrangements became the persisted
derived state `DESIGN.md` calls them.

**Outside the Ent:** the differential engine's per-evaluation working state —
multisets in `grmpl-diff` — is still plain in-memory hash maps, rebuilt per
call. A view that is not materialized pays for that: `distinct` over a join
recomputes both ends of every interval.

---

## 4. Scorecard

The Gold column is from the source ([`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md));
the design column is `idea.md`.

| Property | In Gold | In the design (`idea.md`) | In `grmpl-ent` |
|---|---|---|---|
| Never overwrite; historical editions retained | ✅ | ✅ core law | ✅ versions are roots; as-of reads |
| Opaque edition identity | ✅ trace positions | ✅ explicit law | ✅ `Edition` is opaque to the language |
| Patch = guarded, atomic next edition | ⚠️ pseudo-transactions, no rollback | ✅ semantic center | ✅ `commit_if`, group-committed |
| Structural sharing / path copy | ✅ | ✅ | ✅ `O(log n)` new nodes per commit |
| Content tree shape | binary splay, split per dimension | — | B+ tree ordered by whole key (divergent), or per relation binary splits on any column, balanced by scapegoat rebuild and same-column rotation (step 6) |
| Leaf kinds | region (one element over a region), virtual (a primitive array), partial (placeholders) | — | items: rows, runs (run-length and lazy at once), holes (step 7); runs coarsen identity, which in grmpl is node sharing |
| Cached upward summaries in content nodes | ❌ (canopy crums instead) | ✅ "WIDative summaries" | ✅ `Count` and per-column `Extent` (grmpl's own) |
| Displacements composing down the tree | ✅ `DspLoaf` nodes | ✅ | ✅ a dsp on every handle |
| Relocation / virtual copy | ✅ `O(1)` / splay and share | ✅ | ✅ relocate `O(1)`; graft `O(log n)` |
| Version compare | by shared content identity (`sharedRegion`) | ✅ | by position and value (`Tree::diff`); by identity (`shared_region`) |
| History: content knows its containers (H-tree) | ✅ | — | ✅ an index beside the nodes, built deferred |
| Backfollow: which editions hold this | ✅ transitive, filtered | — | ✅ `backfollow`, across branches; ❌ not filtered by permission |
| Version DAG with merges | ✅ `DagBranch` | ✅ Edition enfilades | ✅ two-parent branches, merged by replaying patches (step 5) |
| Canopies (permission/endorsement flags) | ✅ bert + sensor | — | ⛔ declined (grmpl's canopy is an interval index of watchers) |
| Standing queries | ✅ recorders, into a trail | ✅ Canopy enfilades | ✅ watches over relational views (divergent); recorders declined |
| Persistent background work | ✅ the Agenda | — | ⏸ history indexing only; more as needed |
| Storage | identity-addressed, in place | ✅ granfilade | content-addressed SHA-256, immutable (deliberate) |
| One root; every structure beneath it | ✅ the Turtle | ✅ the `Ent` object | ✅ root record → DAG + branch enfilade → everything |
| Content identity | range elements | — | tuple values (deliberate) |
| DSP-inherited context down scopes | — | ✅ Context enfilades | ✅ over nested entity blocks; ⏸ other scopes wait on clustering |
| Derived state in the Ent | — | ✅ Derived enfilades | ✅ `materialized view` |
| Reverse index over copies (Green's spanfilade) | — | — | ✅ by source and by target |
| Sequences as measured enfilades (§6 parsing) | — | ✅ | ❌ |

---

## 5. Verdict

The design is a faithful, ambitious generalization of the Ent, and the
implementation now has the Ent's core: its **versioning** (immutable versions,
path copying, cheap history, comparison that costs the change,
content-addressed persistence), its **coordinate system** (dsps on pointers,
`O(1)` relocation, `O(log n)` virtual copy by graft), and its **shape on disk**
(one root, the version DAG beneath it, every structure a tree, nodes paged in
on demand). It differs from Gold in what the coordinates are (ordered tuples
whose entity cells move, rather than regions of coordinate spaces), in how
nodes are stored (content-addressed and immutable), and in what identifies
content (values, not range elements).

Since v7 it also has **summaries** Gold's content trees lack: Fact trees carry
each subtree's box in entity space, and a search prunes on any entity column.
And it has Green's **reverse index**: the spanfilade knows,
for every virtual copy, where it came from and where it went. Since step 3 it
has the last two members of `idea.md`'s family in working form: **context
enfilades**, scopes inherited down nested entity blocks and carried by grafts,
and **derived enfilades**, materialized views whose maintenance state lives in
the Ent.

What is still short of Gold, from the source (full list in
[`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md) §4, status in
[`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md)): per-dimension dsps and
unloading clean nodes. The
history layer landed in step 4
([`ENT-FIDELITY-STEP-4.md`](ENT-FIDELITY-STEP-4.md)) and merges in step 5
([`ENT-FIDELITY-STEP-5.md`](ENT-FIDELITY-STEP-5.md)), and splits on any
column in step 6 ([`ENT-FIDELITY-STEP-6.md`](ENT-FIDELITY-STEP-6.md)), and
run, lazy and partial leaves in step 7
([`ENT-FIDELITY-STEP-7.md`](ENT-FIDELITY-STEP-7.md)).
Canopies, recorders and a general Agenda were declined.

What is short of `idea.md`'s extrapolations:

1. **Scopes are entity blocks only.** Context is inherited down nested spans
   of entity ids, which is where the dsps act. Namespace, authority or schema
   inherited down other scopes are not built, because nothing else in core
   nests: authority domains and packages are flat, and a fork copies its parent
   whole. That reopens with clustering. Blocks owned by packages or instances
   are world policy a world can build today, not a core gap.
2. **Sequences are not enfilades.** `idea.md` §6's sequences in measured
   trees, with parsing that shares their split and summaries, are not built.
3. **Extents cover entity cells only.** Text and number columns are not
   summarized, so a search on them reads and filters, or uses an Arrangement.
   That keeps a frame's measures fixed-size; summarizing text would put
   arbitrary strings in every internal frame.
4. **Green's 2-D enfilades.** The spanfilade answers both of Green's directions,
   but as two 1-D interval trees each measured by a hull, not as one enfilade
   with 2-D wids. The Fact trees' extents are n-dimensional boxes, but they ride
   a tree ordered by its whole key, so they prune only as well as each column
   tracks that order.

Step 3's structures have costs too (`docs/PERFORMANCE-ENT.md` §8). A
materialized view is paid for in storage — its open form, which for a view
whose parameter joins widely (`here` pairs every two things in a room) is that
whole join — and in a commit per refresh that changes it; the first refresh
materializes it whole. A refresh probes the Arrangements it needs, which are
built on first use. A change to a scope makes every view that inherits through
it recompute both ends of the interval, because one binding can change any
entity's answer.

The extents and the spanfilade have costs of their own (measured in
`docs/PERFORMANCE-ENT.md` §7). A search on a scattered column prunes nothing: it
visits every leaf, and on a cold store pages every leaf in, where an
Arrangement would read `O(log n)` frames — the extent is only as good as the
locality of what it bounds. Every Fact frame carries the boxes, so a commit
writes about a quarter more bytes. And the spanfilade writes every graft twice,
and a fork into the past rebuilds it in `O(grafts)`, because it is keyed by
span, not edition.

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

The step-by-step results of this work, with what each step taught about the
structure, are in [`ENT-FIDELITY-STEP-2.md`](ENT-FIDELITY-STEP-2.md) and
[`ENT-FIDELITY-STEP-3.md`](ENT-FIDELITY-STEP-3.md).

### Sources & method

* Xanadu Gold read directly: [`dotmpe/udanax-mpe`](https://github.com/dotmpe/udanax-mpe)
  `gold/udanax-top.st` and `gold/udanax-spaces.st`, read line by line for
  [`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md). Background:
  [Enfilade (Xanadu)](https://en.wikipedia.org/wiki/Enfilade_(Xanadu)),
  [xanadu.com/tech](https://xanadu.com/tech/).
* grmpl read directly: [`idea.md`](../idea.md) and `crates/grmpl-ent/src/`
  (`tree.rs`, `granfilade.rs`, `store.rs`, `dsp.rs`, `context.rs`, `canopy.rs`,
  `dag.rs`, `measure.rs`, `spanfilade.rs`).
* D4M: Kepner et al., *Dynamic Distributed Dimensional Data Model* — associative
  arrays stored with their transpose so either dimension is a range lookup.
