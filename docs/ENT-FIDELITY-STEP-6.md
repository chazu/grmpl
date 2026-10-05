# Ent fidelity, step 6: splits on any dimension

**Status:** on the working tree, format v9 (a fresh-store cutover: node frames
gain a k-d split tag, each branch a layout default and layout directory, and
every extent a per-column count of entity cells).
**Question it set out to answer:** Gold's content tree is binary, and each
node divides its region on one dimension (`SplitLoaf`, `udanax-top.st`
8102–8107), so it partitions space as a k-d tree does. grmpl's Fact trees are
B+ trees ordered by the whole key, which is why an extent prunes on the lead
column and on nothing scattered (step 2). What does a k-d Fact tree cost and
buy, beside the B+ one, in an Ent whose nodes are immutable and
content-addressed?

This closes gap G8 (see [`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md)).

---

## 1. The choices

Three, made before any code:

* **A per-relation layout, beside the B+ tree.** A relation is laid out as B+
  trees (the default) or as k-d trees. Every law runs against both.
* **Binary splits balanced by scapegoat rebuild**, standing in for Gold's
  splay. Splay rewrites shared nodes in place, which content addressing rules
  out.
* **Split on the column of widest spread, at the median.**

## 2. What was built

* **A third node kind**, `Kind::Split { col, pivot, children: [below, above] }`,
  in the same `Tree` as the B+ nodes. A pivot is a one-column key, so it
  persists and displaces through the existing key codec. Paging, content keys,
  the granfilade, the history index and GC all carry over unchanged; the
  granfilade gains one frame tag.
* **Reads need no layout.** A split on column `0` divides keys exactly as a
  separator would, so every key-range walk treats it as one. A split on any
  other column interleaves its children's keys, so a walk in key order enters
  both and sorts what it gathers. Only writes dispatch on the layout, since a
  lone leaf is valid in both.
* **`tree::kd`**: insert, remove, build, cut, join and graft for the k-d
  layout, and a range on any column (`kd_range_on`), which narrows on the
  splits over that column as Gold's `limitRegion` does.
* **The layout directory**: each branch holds a default layout and a
  `relation → layout` tree beside its Rel enfilade, so a relation can be laid
  out before its first fact, and forks and merges carry the choice. A layout is
  fixed once a relation has a version.
* **Store reads use the splits**: a range or lookup on a non-lead column of a
  k-d relation answers from its own tree, at any edition, and builds no
  Arrangement.

## 3. Balance: scapegoats and rotations

The scapegoat was the chosen mechanism, and it works for inserts: a subtree
whose insert path grows too deep for its size is rebuilt by median splits.
20,000 inserts, sorted or random, end at the balanced height.

It is the wrong tool for grafts. A graft composes a copy beside the rest of
the tree, and a scapegoat rebuild of anything containing the copy turns its
shared nodes into fresh ones: the virtual copy stops being virtual. So the
k-d layout keeps a second mechanism. **Splits on the same column form a
binary search tree over that column, and rotations among them are valid**, so
a join or cut on column `0`, which is all a graft does, rebalances by
weight-balanced rotation, as a join-based search tree does. Two hundred
instances of one template, each into the next block up, add ten levels for
201 copies. The ideal is about eight. A rotation cannot pass a split on another
column, so a heavy side whose root splits elsewhere is hung unbalanced, and the
scapegoat catches it on a later insert.

## 4. Version compare: three tries

`Tree::diff` on B+ trees walks frontiers in key order, so it finds shared
subtrees however the spines above them were rebuilt. A k-d tree has no key
order to walk. Each try below was measured cold, across a 1,000-row graft
into 100,000 rows.

1. **Pair children of splits that match** (same column, same pivot), and
   compare anything else entry by entry. One rotation above a shared subtree
   breaks the pairing, so a graft's rebalancing join made the compare read the
   relation: 5,390 frames.
2. **Partition, do not pair.** Each side is a set of pieces. Pieces that are
   the same node at the same position cancel, and otherwise the largest split
   divides both sides at its pivot. Still about 5,000 frames. A piece that lies
   wholly on one side of a pivot, but whose own splits do not say so, was
   opened down its spine. Its fragments never cancelled against the other
   side's whole nodes, and the fragmentation cascaded through the tree.
3. **Carry bounds.** Each piece carries what its ancestors' splits say about
   it (Gold's `limitRegion`), and its extent: if either places it wholly on one
   side, it goes there unread. 101 frames on the test world. On the
   benchmark's scattered world, bounds alone left 2,059 frames: an instance
   into fresh space hangs beside the old tree under a column-`0` split, and the
   old tree's root splits on the destination, so no ancestor bounds its lead
   column. Its extent does, once the extent can prove that every cell of a
   column is an entity. With that, 52 frames, against the B+ tree's 42.

The extent now counts, per column, the rows holding an entity. Where the count
equals the rows, the bounds hold every cell, so `Measure::side_of` can place a
subtree on one side of a pivot. Where they differ, a number or a missing cell
could sort anywhere, and it claims nothing. The same test places subtrees for
the graft's cut, for ranges on any column and for backfollow's walk.

## 5. What it costs and buys

From [`PERFORMANCE-ENT.md`](PERFORMANCE-ENT.md) §12, at 100k scattered exits,
cold:

| | B+ | k-d |
|---|---|---|
| box on the scattered column | 3,221 frames | 65 |
| range on the scattered column, in the past | 3,192 frames | 77 |
| range on the scattered column, at the present | an Arrangement (252 ms to build, a second write on every commit after) | its own splits |
| one room's exits (lead column) | 7 frames | 450 |
| `read_at` the whole relation | 0.89 ms | 8.7 ms (a sort) |
| commit, graft, compare across a graft | about the same | about the same |

## 6. Findings

* **The k-d tree trades the lead column for every column, at about √n.**
  With two columns that vary, half the splits are on each. A box on either
  column prunes to a few paths, and a read on just one of them enters both
  sides of every split on the other: about the square root of the leaves. A
  B+ tree plus an Arrangement per column is the opposite trade, exact on each
  column it pays a whole copy for. Neither dominates. Which to choose depends
  on whether a relation is mostly read by its key or mostly searched by other
  columns.
* **Correlated columns make the k-d tree a B+ tree.** When the destination
  tracks the room, the widest spread is almost always the lead column, the
  tree splits on little else, and a lead read costs a binary tree's depth.
  The k-d tree pays for scattered columns only where they exist.
* **Binary nodes are deeper nodes.** A B+ tree of 80k rows is three levels; a
  binary tree over the same leaves is eleven or more. Gold's O-tree had the
  same depth, without content-addressed frames to page in.
* **Content addressing changes how a tree can stay balanced.** Splay is out;
  scapegoat rebuilds are safe but un-share what they rebuild; rotations on one
  column keep sharing but cannot cross columns. The k-d layout needs both
  mechanisms: rotations for joins, scapegoats for inserts.
* **A compare needs bounds, as Gold's searches did.** Without the region each
  piece inherits from its ancestors, a shape-independent compare reads the
  relation. Gold threads `limitRegion` down its searches for the same reason.
  grmpl also has a summary Gold lacks, the extent, and needs both: bounds for
  what the splits above say, extents for what a subtree itself holds.
* **Identity compare reports the highest shared node, in either layout.**
  `shared_region` reports each node of one version that the other holds, at
  every position the other holds it, and does not look inside it. A k-d graft
  into fresh space leaves the node holding the template intact at its own
  position, so the walk stops there, and the template's appearance at the
  copy's shift goes unreported. This is not specific to the k-d layout: a B+
  template in the middle of the key space is hidden the same way (checked:
  template rows 5,000–6,000 of 21,000 give `shared_region` nothing at the
  shift). Step 4's test only passed because its template sat at the end of the
  key space, where B+ joins rebuild the nodes above it. `backfollow`, which
  starts from the leaves as Gold does, finds the copy in every case. Fixed
  after this step: both methods now walk every leaf, as Gold's `compare:`
  does (see `ENT-FIDELITY-GAPS.md`).

## 7. Laws and mutants

* `tree/kd_laws.rs`: random histories of inserts, removes, grafts, cuts, joins
  and rebuilds against a `BTreeMap`, read through displaced handles and after
  a granfilade round trip, with the k-d invariants checked at every step;
  depth under sorted and random inserts; balance and sharing under 200 grafts;
  edits inside a displaced copy; and when an extent may place a subtree.
* `tests/kd_layout.rs`: one random history of commits, grafts and reopens
  into a store of each layout, compared on every read, compare, span compare,
  fork and merge; layout choice, durability, forks and merges; the costs in
  §5 on cold stores; and a compare placed by extents alone (graft into fresh
  space) and by bounds alone (a relation of numbers).
* The conformance suite runs every law of the language on both layouts
  (`ent` and `ent-kd`), and `cargo test --features grmpl-ent/kd-default`
  runs every ent test with k-d as the default. The ent tests that pin B+
  costs or shapes (in `extents.rs`, `graft_compare.rs`, `history.rs`,
  `scopes.rs` and `whole_ent.rs`) now pin the B+ layout explicitly.
* **20 of 23 mutants are caught.** The three survivors are equivalent: a
  column-`0` cut at `key < pivot` instead of `key <= pivot` gives the same
  halves when the key is the pivot, and the two column-range bounds only enter
  a side that can hold no match. Two mutants first survived and drove new
  laws. One was a remove inside a copy that ignored its displacement: no test
  removed from a copy large enough to have a split at its root. The other was
  each half of the diff's placement (bounds, extents) dropped alone, since the
  test worlds let either one cover for the other.

## 8. Not done

* **Splits choose by raw range.** An entity column and an integer column are
  compared in their own units, so one with large ids wins over one with small
  numbers. Text columns are split only when no numeric column varies.
* **Lead-column reads are not rescued.** A relation read mostly by its key
  should stay B+; nothing chooses the layout for it.
* **Per-dimension dsps (G10)** would let a graft move one column and not
  another; the k-d tree's pivots would then displace per column. Declined
  since (see `ENT-FIDELITY-GAPS.md`).
