# The Ent: content plus history

With the measured tree, wids, and dsps in hand, we can read the `Ent` itself. In
the Udanax Gold source it is declared, minus the ceremony, as:

```smalltalk
Abraham subclass: #Ent
    instanceVariableNames: '
        oroots {MuTable NOCOPY smalltalk of: TracePosition and: OrglRoot}
        fulltrace {DagWood}'
    category: 'Xanadu-Be-Ents'!
```

An `Ent` is the **versioned-content backbone**. It has two parts:

- **`oroots`** — a map from a **`TracePosition`** (a point in history) to an
  **`OrglRoot`**, the root of a content enfilade (one *orgl* — roughly, one
  document or one world-state). This is the *content* side: the actual measured
  trees of stuff.
- **`fulltrace`** — a **`DagWood`** recording the entire version history: which
  edition descends from which, where branches split, how versions relate. This is
  the *history* side.

So the `Ent` is precisely **content + history**: a family of content enfilades,
plus a version graph that ties their roots together over time. Everything from
the earlier chapters is in service of these two.

## The DagWood

The word turns up in the declaration and then all through Part III, so it is
worth stopping on.

A `DagWood` is the **branch structure of the version history** — a directed
acyclic graph of *branches*. The name is the structure: a DAG whose nodes are
themselves the roots of trees is not a tree but a **wood** — many trees, sharing
ancestry.

The division of labour is the point. History has two axes, and the `Ent` keeps
them in two different structures:

- **Within** a branch, history is **linear**: edition 1, then 2, then 3, in
  submit order. That is an ordinary measured enfilade — grmpl's Edition enfilade.
- **Between** branches, history **forks**: each branch remembers its parent and
  the exact edition it split at. That is the `DagWood`.

Put them together and a version *point* is the pair `(branch, edition)` — which
is exactly what a `TracePosition` is. The linear enfilade per branch, plus the
branch graph tying them together, *is* the `fulltrace`.

Splitting it this way is what makes provenance cheap. The linear log alone cannot
answer cross-branch questions, and a single flat DAG over every commit would make
the common case — "what happened next on this branch?" — a graph walk. With the
split, the two questions the `DagWood` exists for are small graph queries over
*branches* (of which there are few) rather than over *commits* (of which there
are many):

- **is-ancestor** — does point *a* lie on the history flowing into point *b*?
  This is the *backfollow* reachability relation, and in
  [*Editions and causal frontiers*](../grmpl/editions.md) it is the
  happens-before order `≼` that makes a causal frontier well-defined.
- **common-ancestor** — the latest point two divergent branches share: the merge
  base, where two histories parted.

grmpl implements it as `Dag` in
[`grmpl-ent/src/dag.rs`](../grmpl/implementation.md#the-branchedition-dag--dagrs),
and — like everything else in the store's state — the branch registry is itself
an enfilade rather than a map beside one.

## What the two parts buy you

**On the content side (`oroots` / orgls):** each orgl is a measured, persistent
enfilade. So within any single version you get `O(depth)` addressing (wids),
`O(edit)` insertion and relocation (dsps + path copy), and virtual copies
(sharing + dsps). Content is never mutated in place; an edit produces a new
`OrglRoot` that shares all the unchanged subtrees with the old one.

**On the history side (`fulltrace`):** because a new version shares structure
with its parent, *retaining every version is affordable*. The `fulltrace` DAG can
point at the root of every edition that ever existed, and the total storage is
proportional to the *sum of the edits*, not the sum of the *sizes*. This is what
makes "nothing is ever overwritten" a practical policy rather than a fantasy. It
also makes branching natural: a branch is just another edge in the DAG, another
root sharing structure with its ancestor.

## Two moves, restated

The whole `Ent` rests on two moves you have now seen from several angles:

1. **Structural sharing** — a new version shares every unchanged subtree,
   allocating new nodes only along the edited path. An edit, a branch, or a
   virtual copy is `O(edit)`.

2. **Dual measures** — wids summarize content upward (so search and routing
   prune from the top in `O(depth)`), dsps carry context downward (so relocation
   and virtual copy are one key change).

Around these sit the specializations the Gold source names, and which Part III
rebuilds:

- **`CanopyCrum`** — nodes of the *canopy*, an enfilade of standing *interest*
  rather than content. Where a content enfilade indexes "what is here," the
  canopy indexes "who is watching here," so a change can be routed only to the
  observers whose interest covers it. This is Xanadu's answer to the
  notification/subscription problem, and it is the same measured-tree trick with
  interest as the upward measure.

- **`HistoryCrum`** — carries *trace membership* as an upward measure, so asking
  "is this content part of edition E?" is `O(measure)` (subtree-pruned), not a
  scan. This is the read side of provenance.

- **`SensorCrum`** — a node carrying an active *sensor*: a standing trigger sited
  in the tree, which fires when a change lands beneath it. Where a `CanopyCrum`
  is the index of who is watching, a `SensorCrum` is a watch itself, placed at
  the region it cares about.

- **`Loaf`** — a **block of crums**, and the unit the granfilade actually stores.
  A crum is a *logical* node; a loaf is the *physical* record holding a run of
  them. The distinction is a constant factor rather than an asymptotic one, and
  it is nonetheless the constant factor that decides whether the whole structure
  is affordable — see [the name-zoo](./names.md#gold-the-vocabulary-this-book-uses).

- **The granfilade (`GrandNode` / `GrandHashTable`)** — the persistent node
  store beneath everything. Nodes are placed and retrieved here; structural
  sharing is realized by *content-addressing* nodes so that equal subtrees are
  stored once.

## From a hypertext engine to a world

Xanadu built the `Ent` to hold *documents*. grmpl's wager is that the same
machine — measured trees, wids up, dsps down, structural sharing, a version DAG,
a canopy of interest — is the right substrate for a *relational, versioned
world*: a MOO. The mapping turns out to be remarkably direct, and stating it is
the job of the next part. First, though, it is worth pausing on *why* you would
want this machine at all — the properties that make it worth the trouble.
