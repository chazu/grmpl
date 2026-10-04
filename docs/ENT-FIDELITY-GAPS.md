# Ent fidelity: the score

**What this is:** the standing list of where `grmpl-ent` falls short of Udanax
Gold's Ent, as the source shows it. Each Gold gap is backed by the line-by-line
audit [`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md). Gaps against `idea.md`'s
extrapolations of the Ent are kept separately below, since they are not Gold.
Update this file when a gap closes or a new one is found.
**Last updated:** 2026-10-03, after step 4 (the history layer).

The step reports say what each step built and what it cost:
[`ENT-FIDELITY-STEP-2.md`](ENT-FIDELITY-STEP-2.md),
[`ENT-FIDELITY-STEP-3.md`](ENT-FIDELITY-STEP-3.md) and
[`ENT-FIDELITY-STEP-4.md`](ENT-FIDELITY-STEP-4.md).

---

## Deliberate divergences (confirmed 2026-10-03)

* **Content-addressed, immutable nodes.** Gold stores identity-addressed
  objects and updates them in place; grmpl's nodes are SHA-256-keyed and never
  overwritten. This gives atomic editions, structural sharing for free and a
  well-defined crash story. It also rules out Gold's splay, which rewrites
  shared nodes in place, so grmpl's tree is a balanced, path-copied B+ tree.
* **Facts are identified by value.** Gold's content identity is range
  elements: two separately written copies of equal content are different
  content, and backfollow follows identity. In grmpl two equal tuples are the
  same fact. So grmpl's provenance can only come from recorded copy
  operations and from sharing nodes, never from comparing values.

Other representation choices, not gaps: tuples whose entity cells move under a
dsp, rather than regions of coordinate spaces; a dsp on every handle rather
than a separate `DspLoaf` node.

## Gold gaps

From the most central (audit §4). Each needs its laws and cold-store
measurements before it counts as closed, as in steps 2–3.

| # | Gold mechanism | Status |
|---|---|---|
| G1 | **History (the H-tree):** every node knows the nodes that contain it, and the versions at the top | ✅ step 4: an index beside the immutable nodes |
| G2 | **Backfollow:** which versions and editions hold this content, transitively, across the whole Ent | ✅ step 4: `backfollow`, from leaves, across branches |
| G3 | **Identity-based compare** (`sharedRegion`, `mapSharedTo`): what two versions share, wherever it sits | ✅ step 4: `shared_region` (Gold's upward method) and `shared_region_by_descent`, which measures faster |
| G4 | **Merges in the trace**, and a trace per derived operation (copy, transform, combine) | ❌ |
| G5 | **Canopies:** the bert canopy pruning backfollow, the sensor canopy pruning standing-query checks | ❌ (grmpl's "canopy" is an interval index of watchers) |
| G6 | **Recorders:** standing backfollow queries, past then future, into a trail | ❌ (watches are relational, a different thing) |
| G7 | **The Agenda:** persistent, crash-resumable background work | ⚠️ one job: history indexing runs deferred, in bounded durable steps |
| G8 | **Splits on any dimension** (k-d-like `SplitLoaf`s) | ❌ (why extents prune only along the sort order) |
| G9 | **Lazy and run-length leaves:** region, virtual and partial loaves | ❌ |
| G10 | **Per-dimension dsps** (`GenericCrossDsp`) | ❌ (one shift for every entity cell) |
| G11 | **Unloading clean nodes** back to stubs | ❌ (a paged node never unloads) |

Already faithful: versions as roots, the persisted version DAG (tree case),
`isLE`-style ancestry, `O(1)` relocation, copy by sharing subtrees, paged stubs,
one root with everything beneath it, and since step 4 the history layer.

## Gaps against `idea.md`'s extrapolations

These are the design note's generalizations of the Ent, not Gold. They are
uses of an Ent as much as parts of one.

### Sequences as measured enfilades ❌

* **Design:** `idea.md` §6 holds token sequences in measured enfilades, so
  parsing shares the tree's split, search, summaries and incremental update.
* **Today:** `grmpl-pattern` parses whatever input it is handed. No sequence
  lives in the Ent. A candidate experiment once the Gold gaps are closed.

### Context beyond entity space ⏸

* **Design:** context inherited down scopes carries authority, namespace,
  schema, permissions, placement and simulation parameters (`idea.md` §1, §10).
  Gold has no counterpart: its dsps displace, they do not carry context.
* **Done for core:** `context`/`inherit` bind values over nested inclusive
  spans of entity space, and a graft carries a block's bindings with it.
* **Why the rest waits:** it needs a scope tree in core, and core has none.
  Authority domains and packages are flat, and a fork copies its parent whole.
  The nesting `idea.md` §10 has in mind comes with clustering, which is
  deferred. Blocks owned by packages or instances are world policy, not core.

### Derived state is incremental only for linear views and `distinct` ⚠️

* **Aggregates:** a materialized view with an aggregate takes deltas by
  recompute. Fix: store each group's partial fold.
* **`inherit`:** a scope change recomputes every view that inherits through
  it. Fix: store each entity's winning scope.
* **Choosing what to materialize** is manual.

### Extents cover entity cells only (grmpl's own summary)

* Text and number columns carry no bounding box. Deliberate: it keeps every
  frame's measure fixed-size. The extent itself is grmpl's, not Gold's.

### Green's 2-D enfilade

* The spanfilade (Udanax Green's, not Gold's) answers both directions as two
  1-D interval trees rather than one 2-D enfilade. G8 is the Gold-side form of
  the same weakness.

## Closed

### Version compare across a graft ✅ (closed 2026-10-03)

* **Was:** a compare across a graft read the whole relation, and listed the
  copy row by row.
* **Now:** it reads the copy and its seams, and `EntStore::compare_spans`
  names the copy by span at a cost independent of its size. This is a
  positional compare; Gold's identity-based compare is G3. See
  [the closing note below](#how-the-graft-compare-gap-closed).

---

## How the graft-compare gap closed

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
