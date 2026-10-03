# Ent fidelity: the score

**What this is:** the standing list of where `grmpl-ent` still falls short of
the Ent/enfilade design. The design is `idea.md` §1 (the enfilade plex) and §6
and §10, plus Gold's `Ent` as read in [`ENT-AND-XANADU.md`](ENT-AND-XANADU.md).
Update this file when a gap closes or a new one is found.
**Last updated:** 2026-10-03, after fidelity steps 1–3, gap 4, and gap 2's
reclassification.

The step reports say what each step built and what it cost:
[`ENT-FIDELITY-STEP-2.md`](ENT-FIDELITY-STEP-2.md) and
[`ENT-FIDELITY-STEP-3.md`](ENT-FIDELITY-STEP-3.md).

---

## The plex at a glance

| `idea.md` §1 member | Status |
|---|---|
| Fact enfilades: stored relations and their indexes | ✅ measured by `(Count, Extent)`; Arrangements as derived indexes |
| Edition enfilades: roots, patches, branches, ancestry | ⚠️ persisted and forkable, but branches form a tree: no merges (gap 3) |
| Context enfilades: scope-inherited context | ✅ for core: over nested spans of entity space, carried by grafts; ⏸ authority, namespace and placement wait on a nesting that core lacks (gap 2) |
| Canopy enfilades: standing interest | ✅ interval routing, persisted with the commits routed to it |
| Derived enfilades: materialized views, incremental state | ✅ for joins and `distinct`; ⚠️ aggregates and `inherit` recompute (gap 7) |
| Sequences as measured enfilades (§6) | ❌ not built (gap 1) |
| Green's spanfilade (reverse index over copies) | ✅ by source and by target; ⚠️ as two 1-D trees (gap 5) |

## Open gaps

Ordered by how much of the design they leave out. Gaps 1 and 3 need a design
decision before code. Gaps 5–7 are refinements of things that already work.
Gap 2 is complete for core and waits on clustering (see
[Deferred](#deferred-until-core-has-what-they-need)).

### Gap 1 — Sequences as measured enfilades ❌

* **Design:** `idea.md` §6 holds token sequences in measured enfilades, so
  parsing shares the tree's split, search, summaries and incremental update.
* **Today:** `grmpl-pattern` parses whatever input it is handed and returns
  every parse. No sequence lives in the Ent, so editing input means parsing
  all of it again. The pruning of `prune-to-ent-design` deleted the old
  differential `parse_stream` prototype, which was the nearest thing.
* **Needs design:** what a sequence key is (a position that can be displaced,
  next to entity-keyed facts), and which consumer edits sequences
  incrementally. A command line is too short to measure anything on.

### Gap 3 — No merges in the edition DAG ⚠️

* **Design:** Gold's `fulltrace` is a DAG of version history.
* **Today:** branches form a tree; each has exactly one parent.
* **Needs design:** a merge edition with two parents, and what as-of reads,
  `compare`, the spanfilade's `as_of` and Replay mean across one.

### Gap 5 — Green's 2-D enfilade is approximated ⚠️

* **Design:** Green answers "where did this go" and "where did this come from"
  with one enfilade carrying 2-D wids.
* **Today:** the spanfilade answers both, as two 1-D interval trees each
  measured by a hull. Fact-tree extents are n-dimensional boxes, but they ride
  a tree ordered by its whole key. They prune only on columns that follow that
  order: a scattered column pages every leaf (`PERFORMANCE-ENT.md` §7).

### Gap 6 — Extents cover entity cells only ⚠️

* Text and number columns carry no bounding box, so a search on them reads and
  filters, or uses an Arrangement.
* This is deliberate: it keeps every frame's measure fixed-size. Gold's widths
  have no counterpart for arbitrary strings.

### Gap 7 — Derived state is incremental only for linear views and `distinct` ⚠️

* **Aggregates:** a materialized view with an aggregate reads from its copy
  but takes deltas by recompute. Fix: store each group's partial fold, the
  Reduce analogue of storing derivation counts.
* **`inherit`:** a scope change recomputes every view that inherits through
  it. Fix: store each entity's winning scope.
* **Choosing what to materialize** is manual. The extents and counts already
  in the Ent could drive it.

## Deferred until core has what they need

### Gap 2 — Context beyond entity space ⏸

* **Design:** context inherited down scopes carries authority, namespace,
  schema, permissions, placement and simulation parameters (`idea.md` §1, §10).
* **Done for core:** `context`/`inherit` bind values over nested inclusive
  spans of entity space, the coordinate the dsps move, and a graft carries a
  block's bindings with it. That is the Ent's mechanism: context flowing down
  the tree, displaced with what it describes.
* **Why the rest waits:** inheriting authority, namespace or placement needs a
  scope tree in core to inherit down, and core has none:
  * authority domains are flat (one in v1);
  * packages are flat and cannot import each other;
  * branches nest, but a fork copies its parent whole, so inheriting down them
    adds nothing.

  The nesting `idea.md` §10 has in mind comes with clustering ("scopes are the
  bridge to clustering"): placement and replication policy inherited down scope
  covers. Clustering is deferred, so gap 2 reopens with it.
* **Not a core gap:** giving packages or instances owned blocks of entity ids,
  with authority and allocation ranges bound over them, is world-building
  policy. A world can build it today from `context`, `instance_template` and
  key-range authority scopes. Shotengai's hard-coded instance blocks are an
  example.

## Closed gaps

### Gap 4 — Version compare across a graft ✅ (closed 2026-10-03)

* **Design:** a version compare costs the edit, and recognizes a virtual copy
  as shared content (Green's compare), so instancing a template is not a
  thousand new rows.
* **Was:** a compare across a graft read the whole relation, and listed the
  copy row by row.
* **Now:** it reads the copy and its seams, and `EntStore::compare_spans`
  names the copy by span at a cost independent of its size. See
  [the closing note below](#how-gap-4-closed).

## Deliberate departures (not gaps)

* **Tuples, not tumblers.** Coordinates are ordered tuples whose entity cells
  move under a dsp; Gold's are tumbler widths. A representation choice.
* **Clustering is deferred.** `idea.md` §10's partitioning along scope covers
  is not built, and only an in-process transport exists. Gap 2 reopens with it.

---

## How gap 4 closed

**The gap as written:** a compare across a graft "falls back to an in-order
merge of the subtrees whose separators differ, costing the instance's size
rather than its node count; the spanfilade knows where a copy came from, but
`diff` does not consult it."

**What was actually wrong was larger.** `Tree::diff` paired two nodes'
children only when their separators were identical, and otherwise merged the
whole subtree pair entry by entry. A graft's join rebuilds the spine from the
root, so the merge ran from the root and read the *whole relation*, not just
the instance. Any other edit that changes separators high in the tree did the
same:
* an insert dense enough to split nodes;
* a removal that underflows a leaf and fuses it, cascading up through
  half-full parents.

**The fix, in two parts.**

1. **`Tree::diff` walks frontiers, not node pairs.** Each version is a
   key-ordered run of whole subtrees. Two heads that are the same node at the
   same absolute position are dropped together, two differing nodes are both
   opened, and entries merge. A shared subtree is found however the spines
   above it were rebuilt. When one side holds it a level deeper, the walk
   realigns at the first shared leaf, so a misstep costs one path, never a
   subtree. One heuristic earned its place by measurement: an entry facing a
   node whose separator bound lies above it is one-sided without opening the
   node. Without it, a one-row compare reads 10 frames instead of 8. The other
   heuristics tried changed no measured cost and were cut: ordering by least
   key, ties by height, and span-below shortcuts.
2. **`EntStore::compare_spans` consults the spanfilade.** The relation's log
   finds the grafts in `(a, b]`, and the spanfilade gives each one's source.
   Each copy is spliced into `a`'s version from the source block as of the
   edition before the graft: the same nodes the graft shared, at the same
   displacement. `diff` then sees the copy as unchanged. The result names each
   copy by span and lists only the rows that differ beyond the copies, at a
   cost that does not grow with the copy: Green's compare, in node count.

**Measured** (`PERFORMANCE-ENT.md` §9): across a 5,000-row graft into a
100k-row relation on a reopened store, a row compare fell from 4,688 frames to
164, and `compare_spans` reads 20. An in-memory compare after one removal fell
from 1.7 ms to 2.2 µs at 100k rows. One-row inserts cost what they did.

**Pinned by** `grmpl-ent/tests/graft_compare.rs`:
* `diff` is exact between trees of any two shapes: different histories, stale
  separators, relocations and grafts, up and down.
* Replaying `compare_spans` rebuilds the later edition, over random histories
  with chained copies and refilled blocks.
* Cold-store frame bounds for grafts, small edits and node-splitting edits.

20 of 21 mutants are caught. The survivor drops the separator bound a first
child inherits from its parent, an optimization with no observable effect.

**What is still not built:** no language consumer reads `compare_spans` yet.
Watches and view deltas take rows, and rows cost the copy's size by
definition. A consumer that could take a span would be a watch that reports
"this block was instanced" rather than every row in it.
