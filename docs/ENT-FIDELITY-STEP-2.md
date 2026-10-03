# Ent fidelity, step 2: wids and the spanfilade

**Status:** landed on `prune-to-ent-design` as `7c9e05f` (format v7).
**Question it set out to answer:** what does the Ent gain from Gold's upward
summaries and Green's reverse index, and what does each cost?

grmpl's aim is a faithful implementation of the Xanadu Ent/enfilade family, so
that its strengths and weaknesses can be seen rather than assumed. Step 1 put
the whole world in the Ent under one root record. Step 2 added the two pieces
`ENT-AND-XANADU.md` listed as most missing:

1. **Summaries richer than a count.** Gold's wids record where a subtree lies in
   its coordinate space. grmpl's Fact trees only counted.
2. **A reverse index over virtual copies.** A graft pointed one way. Nothing
   said where an instance came from, or where a template had been copied.

The D4M associative-array model (a sparse array stored beside its transpose so
that either dimension is a range lookup) was the reference for the second piece.
It also turned out to be the right lens for the first.

---

## 1. What was built

### The wid: `Extent`

Every Fact tree node now carries `(Count, Extent)`. An `Extent` is, per column,
the least and greatest entity id among the subtree's entity cells: the
subtree's bounding box in entity space.

It is faithful to Gold in the ways that matter:

* **It is the coordinate space the dsps move.** grmpl's displacement shifts
  entity cells, so the wid bounds entity cells. Text and number columns are not
  summarized (see §4).
* **It lives in the node's local frame and is displaced on the way down**, as a
  key is. A grafted subtree's box moves with it at no cost. The displaced-tree
  law oracle (40 seeds × 300 rounds of insert, remove, graft, split and join)
  now checks the extent at every step against one computed from scratch.
* **A paged child's box is in its parent's frame**, so a search rules a subtree
  out without reading it from disk.

`Tree::search(admit, keep)` is the search a wid exists for: descend only into
subtrees whose summary `admit` accepts, and keep the entries `keep` accepts.
`EntStore::search_at` runs it over a box spanning several entity columns, at
any live edition.

The extent also turned a documented precondition into a checked one. A
template must be self-contained, because a graft moves every entity cell, and
`instance_template` now proves it from the extent of the block's span. That
reads two spines and refuses a template that names an outside entity. Both
shipped worlds' templates passed.

### The spanfilade

Each branch's state now links a `Spanfilade`: every graft, keyed
`(source, target, edition)` and again `(target, source, edition)`, each tree
measured by the hull of its spans.

| Green | grmpl | Question |
|---|---|---|
| spanfilade (I → V) | keyed by source | where was this block copied to? (`copies_of`) |
| POOM (V → I) | keyed by target | where did this block come from? (`sources_of`) |
| following POOMs back to I-space | `origin_of` | what entity did this one start as? |

It is append-only, as Green's is. Retracting an instance's facts does not
erase that the copy was made, and consolidation leaves it alone. It is per
branch and persisted with the branch state, and a fork into the past keeps only
the grafts made by the fork edition.

`origin_of` follows a chain of copies (an instance of an instance) back to its
start. Each step must be strictly older than the last, because a source exists
before it is copied, so the walk ends even when blocks are emptied and reused.

---

## 2. Results

Measured with `entbench` (release) on an Apple-silicon laptop, v6 and v7 run
alternately, twice each. Full tables are in `PERFORMANCE-ENT.md` §7.

### The central finding: the wid and the transpose are two different bets

grmpl now has two ways to answer "which facts have an entity in this span in a
column the tree is not ordered by":

* an **Arrangement**, the D4M answer: a second copy of the facts, rotated so
  the column leads;
* the **extent**, Gold's answer: a summary on the nodes already there.

Exits into a 10-room span, four exits per room, 100k rows:

| | destination near the source room | destination anywhere |
|---|---|---|
| wid search, warm | **2.1 µs** | 181 µs |
| wid search, cold | 139 µs, **11 frames** | 8.9 ms, **3,228 frames** |
| Arrangement, warm | 3.3 µs | 10.8 µs |
| Arrangement, first use | 181 ms to build | 251 ms to build |
| scan + filter | 2.2 ms | 1.5 ms |

**The wid is exactly as good as the locality of what it bounds.** When a column
tracks the key order, it beats the transpose with no second copy, no build, and
no second write per commit. An instanced template's rooms sit in one block, so
its exits are the good case. When a column is scattered, every leaf's box spans
the world: the search touches every leaf, and on a cold store reads every leaf
from disk, while the transpose stays exact in `O(log n + k)`.

That is not an implementation accident. A wid summarizes a subtree, and a
subtree is defined by the tree's order, so a wid can only prune along
dimensions that correlate with that order. Gold's own 2-D enfilades work
because the dimensions they bound (virtual and invariant position) are
correlated by construction. Content is transcluded in runs. A relational world
gives no such guarantee per column, and the benchmark shows what happens when it
fails.

D4M avoids the question by paying for it up front. It keeps the transpose,
which is exact for any data and costs storage and write amplification. The
spanfilade makes the same choice deliberately: two copies of a small index,
exact in both directions.

### What the summaries cost

| | v6 | v7 |
|---|---|---|
| commit frame bytes (1k / 10k / 100k rows) | 13.3 / 18.9 / 23.0 KB | 16.4 / 23.3 / 28.5 KB (+23–24%) |
| load frame bytes per row | 101–122 B | 102–128 B (+1–5%) |
| commit, fsync'd | 4.3–5.0 ms | 4.7–5.0 ms |
| commit, in memory | 3.0–4.1 µs | 3.5–5.0 µs |
| frames per commit | 7.8 / 9.8 / 12.1 | unchanged |
| `count_at`, 100k rows | 635 ns | 674 ns |

The bytes are the structural cost. Every internal frame records each child's
measure, and the extent adds about 17 bytes per column per child. Leaves carry
no measures, so bulk loads barely grow. Commit latency is still the fsync.
Reads, fork, instancing, consolidation and open were unchanged within noise.

The spanfilade costs a couple of frames per instancing (21 → 23 frames for a
100k-fact template) and nothing otherwise.

---

## 3. What went wrong on the way

Three regressions showed up in measurement and were fixed before landing. Each
is a lesson about rich measures in general.

1. **A search was slower than a scan.** The first `search` took one predicate on
   measures and tested each entry by building that entry's measure. For an
   extent, that is one heap allocation per row visited. On the scattered column,
   the search ran 1.57 ms against a 1.06 ms scan. Splitting the per-entry test
   (`keep`) from the subtree test (`admit`) made it 153 µs. *A measure that
   owns heap state should never be built per entry on a hot path.*
2. **Commit CPU rose 60% in memory.** Node construction folded with `combine`,
   which allocates a fresh extent per step, and cloned each child's measure
   through `displace` even when the dsp was zero. Folding in place
   (`absorb`, `absorb_entry`) brought it back to within a third of v6.
3. **`count_at` regressed 5×.** It folded the full `(Count, Extent)` measure
   along the boundary spines just to read the count. It now reads the cached
   subtree sizes (`Tree::count_range`), which every node already has, and is
   back to v6 speed. *Adding a measure taxes every range fold over that tree
   unless queries can project to what they need.*

None of the three changed a result; each changed only what it cost to get one.
Each was found by benchmarking against the previous format, not by tests.

### Test strength

Each new mechanism was mutation-checked: break it, and confirm a test fails.
Five mutants were tried. Three were caught at once. Two survived and exposed weak
tests:

* A fork that kept the parent's later grafts went unnoticed, because the test's
  fork reused the parent's exact target block and so produced an identical
  record. The test now forks into a different block.
* A relaxed edition bound in `origin` went unnoticed, because it mutated a
  redundant filter; the real bound was enforced one call earlier. The duplicate
  was removed, and a unit test now builds two same-edition grafts that swap
  blocks. With the real guard broken, that test loops forever, so it is caught
  by timeout rather than by failure.

---

## 4. What step 2 says about the Ent

**Strengths confirmed:**

* **Summaries compose with displacement for free.** Because the extent lives in
  the local frame, a virtual copy carries a correct box at no cost. This is the
  dsp/wid duality working as Gold intended.
* **Summaries compose with paging.** A pruned subtree is never read from disk,
  so a well-correlated query on a cold 100k-row world reads 11 frames.
* **Preconditions become proofs.** A summary over a span is a certificate about
  the span. The self-containment check costs two spine reads, where
  establishing the same fact otherwise means reading the block.

**Weaknesses exposed:**

* **A wid prunes only along the tree's order.** On uncorrelated data the wid
  search is worse than a scan cold and 17× worse than a transpose warm. Gold
  can rely on correlation; a general relational store cannot. Choosing between
  the wid and a transpose is a per-column question about the data, not a
  property of the structure.
* **Every summary is paid on every internal frame and every range fold.** The
  cost is bytes (+24% per commit) and CPU in whatever folds the measure. Richer
  measures need projection (`count_range`) or they slow down every query that
  only wanted part of them.
* **Summaries of variable-size values do not fit.** The extent covers entity
  cells because they are fixed-size. Bounding text would put arbitrary strings
  in every internal frame.

**Still not faithful, after step 2:**

* Version compare across a graft still merges row by row; the spanfilade knows
  the source, but `diff` does not consult it.
* Green's 2-D enfilade is answered by two 1-D interval trees, not one enfilade
  with 2-D wids.
* The branch DAG has no merges.
* Scope-inherited context (step 3) is not built.

---

## 5. Open questions for later steps

* **Choosing per column.** `read_range_on` uses the Arrangement at the current
  edition and the extent below it. A better rule would ask the extent itself:
  if the root's box for a column is not much narrower than the query's universe,
  the wid will not prune. That check costs nothing.
* **Version compare through the spanfilade.** `diff` between a template and its
  instance could pair subtrees by their recorded shift instead of merging.
* **Locality as a design input.** Entity allocation is where locality comes
  from. A world that allocates related entities in blocks (as instancing
  already does) gets the good column of the table above for free.
