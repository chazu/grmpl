# Ent fidelity, step 4: Gold's history layer

**Status:** landed on `main` (no format bump: the root record gains slots,
and a store without them rebuilds its index on first use).
**Question it set out to answer:** the Gold audit
([`ENT-GOLD-AUDIT.md`](ENT-GOLD-AUDIT.md)) found grmpl missing the mechanism
Gold's backend is built around: every content node knows the nodes that
contain it, so the system can ask which versions hold some content
(backfollow) and what two versions share wherever it sits (identity compare).
Can that be built over immutable, content-addressed nodes, and what does it
cost?

It closes gaps G1 (history), G2 (backfollow) and G3 (identity compare), and
builds the first job of G7 (the Agenda).

---

## 1. What was built

### The history index (`crates/grmpl-ent/src/history.rs`)

Gold keeps a history crum on every O-tree node, holding the node's O-parents,
and a bottom crum at each orgl root holding the editions that use it. grmpl's
nodes are immutable, so they cannot carry back-pointers that change as new
versions arrive. The H-tree is instead **an index beside the nodes**, keyed by
content key, in four trees hung from the root record:

| Tree | Key → value | Gold's counterpart |
|---|---|---|
| `parents` | `(child, parent, child's dsp)` | an `HUpperCrum`'s O-parents |
| `holders` | `(root, branch, (relation, edition))` | an `HBottomCrum`'s editions |
| `born` | `(node, branch) → first edition seen there` | the crum's history cut |
| `cursor` | `branch → edition indexed through` | — |

* **Keys are data, not links.** The index never keeps content alive. GC sweeps
  whatever it sweeps, and a query skips versions that consolidation retired.
* **Edges carry the displacement**, so a node shared at two offsets (a block
  grafted beside its template) has two edges, and an answer says where the
  content sits.
* **The history cut is per branch.** Content addressing means one node can be
  built independently on two branches, so a single cut would be unsound.
  `born` records the first edition per branch, and "can node *n* be in version
  *v*" is tested against *v*'s lineage: Gold's `isLE:` pruning.

### Indexing is deferred work

A commit never touches the index. `EntRoot::catch_up` indexes the versions
written since each branch's cursor:
* branches in id order, so a fork's ancestors are indexed through its fork
  point before it is;
* each version from its root, stopping at every node already born on the
  version's lineage, so it visits only the version's new nodes.

A fork starts indexing after its fork point: what it inherited is found
through the DAG at query time, so a fork costs the index nothing.

Queries catch the index up first, so answers are exact. `step_history(budget)`
does the work in bounded, durable steps instead: Gold's Agenda, for the one job
grmpl has. Progress persists in the root record with the next stage, and
indexing is idempotent, so a crash only repeats work.

### Backfollow: `EntStore::backfollow(rel, at, lo, hi)`

Gold's `rangeTranscluders`, over versions. As in Gold:
1. walk south to the leaves holding the span;
2. climb north from each leaf through the O-parent sets, memoized so leaves
   share the climb above their common ancestors (Gold's crum cache);
3. at each root, take the versions using it, and extend them through the DAG
   to every branch that forked after them.

Each answer is a version, the shift at which it holds the content, and how
many of the queried rows its shared leaves hold.

### Identity compare: `EntStore::shared_region(a, b)`

Gold's `sharedRegion` / `mapSharedTo`: walk version `a` from its root. For each
node, climb towards `b`'s root, admitting only nodes born on `b`'s lineage.
Report a node found whole; open one that is not.

`shared_region_by_descent(a, b)` gives the same answer without the index:
collect `b`'s node keys from its interior frames, then walk `a` against them.
It was added to measure Gold's method against, and it won (§3).

## 2. What sharing means here

Backfollow finds **shared nodes**, not equal values. That follows from the two
divergences the audit confirmed: grmpl identifies facts by value, but its
nodes are content-addressed and immutable.

* A version that kept a leaf untouched holds that leaf's rows by identity, and
  is found.
* A version that rebuilt the leaf around an edit holds the same rows again
  only by value, and is not found for them.
* A graft shares the template's interior leaves, so instances are found at
  their shift. The leaves at a graft's seams are rebuilt by the split, so they
  are not.

So "which versions hold this" in grmpl means "which versions still share these
leaves". That is close to Gold's identity semantics, and narrower than a value
search, which the relational engine already answers.

## 3. Results

Measured on a durable store: a 100k-row relation with a 1,000-row template, ten
instances, then 200 or 2,000 one-row commits. Every number is cold (a freshly
reopened store), release build.

| | 200 commits | 2,000 commits |
|---|---|---|
| indexing, per one-row commit | ~200 µs; 82 edges, 4 nodes | ~220 µs; 91 edges, 4 nodes |
| backfollow from the template block | 399 frames, 13 ms; 2,266 holdings | not measured (the answer grows with the versions) |
| backfollow from one row | 165 frames, 0.5 ms; 221 holdings | — |
| identity compare, Gold's upward method | 532–630 frames, ~2 ms | 3,195–4,373 frames, ~23 ms |
| identity compare, downward walk | 119–124 frames, ~0.9 ms | 120–167 frames, ~1.2 ms |

* **The H-tree's price is the edges.** Every child of every new node gains a
  parent, so a one-row commit adds about one edge per child along its new
  spine: 82–91 at this size. That is the cost Gold's comments flag ("could be
  drastically improved for orgl creation", `udanax-top.st` 27563).
* **Backfollow is answer-sized.** The template's content is in every version
  since each graft, on every branch, at eleven shifts, and each is one holding.
  The climb is shared through the memo, so the cost tracks the answer, not the
  queried rows.
* **For comparing two known versions, Gold's method loses here.** An old node
  gains a parent in every later version, so climbing from it walks the whole
  later history: the cost grows with history, and born-pruning trims it by up
  to a quarter in one direction while costing time in the other. A downward
  walk costs the two versions' interior nodes, because a B+ tree's interior
  frames already name their children's keys, so leaves are never read. Gold's
  binary splay tree with in-place nodes had no such frames, which may be why it
  climbed.

Mutation-checked: all 24 mutants tried on the index, the queries and the
catch-up are caught. The first run found that the laws could not see:
* how content was cut into pieces (the model shared that code), now checked
  against a range count;
* indexing order and fork starts, which change cost, not answers, now pinned
  by a cost test;
* whether indexing progress persists, now checked across a reopen.

It also exposed a fidelity error, fixed: backfollow first cut its query into
maximal shared subtrees. Gold starts from leaves, and so finds versions that
share some leaves of a subtree but not all of them.

The laws themselves (`src/history_laws.rs`) check backfollow and both compares
against brute force: every retained version on every branch walked whole. The
histories mix edits, grafts, forks at the present and into the past,
consolidation and partial indexing steps, in memory and across a reopen. They
caught one real bug while being written. Consolidation folds several versions
into one checkpoint, so two history holders can stand for one version, and
backfollow counted its rows twice.

## 4. What step 4 says about the Ent

**Strengths confirmed:**

* **Backfollow is the H-tree's reason to exist.** "Every version on every
  branch that still shares this content" has no cheap answer without upward
  links: the alternative is walking every version. With them it is a memoized
  climb that costs the answer.
* **The history fits beside immutable nodes.** Keying the index by content key
  keeps the nodes immutable and the GC story unchanged. Making it deferred
  keeps commits exactly as they were.

**Weaknesses exposed:**

* **The edges are expensive.** About one per child of every new node, in a tree
  of fan-out 64. Gold's binary tree pays two per new node. A wide tree makes
  path copies cheap and history indexing dear.
* **Upward compare grows with history.** Gold's way of comparing two versions
  is the wrong tool once interior frames name their children; the H-tree
  should answer only the questions that have no target.
* **The answer is per version.** A piece of content unchanged for 2,000
  versions is 2,000 holdings. Gold returns editions the same way, but a range
  form (from edition, to edition, shift) would be the natural compression.

**Still not faithful, after step 4:** trace merges (G4), the canopies that
would prune backfollow by permission (G5), standing backfollow queries (G6),
and a general Agenda (G7 has one job). See
[`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md).
