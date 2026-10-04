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

An `Ent` is the **versioned-content backbone**. It declares two fields, but only
one is live:

- **`oroots`** — declared as a map from a **`TracePosition`** (a point in
  history) to an **`OrglRoot`**. It is **vestigial**: built `smalltalkOnly`,
  marked `NOCOPY`, its stores commented out, and never transmitted. It is not
  where Gold keeps content.
- **`fulltrace`** — a **`DagWood`** recording the entire version history: which
  trace position descends from which, where branches split and where they merge.
  This is the live `Ent`, and its one live operation is `newTrace`.

Content lives beside the `Ent`, not in it. Each version of an orgl (one content
tree — roughly, one document or one world-state) has an **`OrglRoot`** stamped
with a trace position from the `fulltrace`, and that root is owned by the
**editions** that use it. So the machine is still **content + history**: a
family of content trees, each stamped with a point in one version graph.
Everything from the earlier chapters is in service of these two.

## The DagWood

The word turns up in the declaration and then all through Part III, so it is
worth stopping on.

A `DagWood` is the **branch structure of the version history** — a directed
acyclic graph of *branches*. The name is the structure: a DAG whose nodes are
themselves the roots of trees is not a tree but a **wood** — many trees, sharing
ancestry.

The division of labour is the point. History has two axes, and they are kept
differently:

- **Within** a branch, history is **linear**: edition 1, then 2, then 3, in
  submit order. In Gold that is a position number on the branch; in grmpl it is
  an ordinary measured enfilade, the Edition enfilade.
- **Between** branches, history **forks and merges**: each branch remembers its
  parent and the exact position it split at, and a branch can have two parents.
  `newSuccessorAfter:` returns a position after two others by making a
  `DagBranch` with two parents, and `combine:` of two editions uses it. That is
  the `DagWood`.

Put them together and a version *point* is the pair `(branch, position)` — which
is exactly what a `TracePosition` (`BoundedTrace`) is. In grmpl the linear run
per branch is an enfilade and the branch graph ties them together; grmpl's
branches form a tree, since it does not yet implement Gold's merges.

Splitting it this way is what makes provenance cheap. The linear log alone cannot
answer cross-branch questions, and a single flat DAG over every commit would make
the common case — "what happened next on this branch?" — a graph walk. With the
split, the two questions the `DagWood` exists for are small graph queries over
*branches* (of which there are few) rather than over *commits* (of which there
are many):

- **is-ancestor** — does point *a* lie on the history flowing into point *b*?
  This is Gold's `isLE:`, which prunes backfollow by trace, and in
  [*Editions and causal frontiers*](../grmpl/editions.md) it is the
  happens-before order `≼` that makes a causal frontier well-defined.
- **common-ancestor** — the latest point two divergent branches share: the merge
  base, where two histories parted.

grmpl implements it as `Dag` in
[`grmpl-ent/src/dag.rs`](../grmpl/implementation.md#the-branchedition-dag--dagrs),
and — like everything else in the store's state — the branch registry is itself
an enfilade rather than a map beside one.

## What the two parts buy you

**On the content side (orgls):** in Gold each orgl is a binary splay tree over
regions. Within any single version you get `O(depth)` addressing (descending
the splits), `O(1)` relocation (wrap a subtree in a `DspLoaf`), and virtual
copies (a copy splays a region into one subtree and shares it). A derived
edition gets a new `OrglRoot` that shares the unchanged subtrees with the old
one, though Gold's splay rearranges shared nodes in place. grmpl's tree is
balanced and path-copied instead, and never mutates a node.

**On the history side (`fulltrace`):** because a new version shares structure
with its parent, *retaining every version is affordable*. Every edition's root
is stamped with a position in the `fulltrace`, and the total storage is
proportional to the *sum of the edits*, not the sum of the *sizes*. This is what
makes "nothing is ever overwritten" a practical policy rather than a fantasy. It
also makes branching natural: a branch is just another edge in the DAG, another
root sharing structure with its ancestor.

## Two moves, restated

The whole `Ent` rests on two moves you have now seen from several angles:

1. **Structural sharing** — a new version shares every unchanged subtree,
   allocating new nodes only along the edited path. An edit, a branch, or a
   virtual copy is `O(edit)`.

2. **Dual measures** — summaries flow upward (so search and routing prune from
   the top in `O(depth)`), dsps carry context downward (so relocation and
   virtual copy are one key change). In Gold the upward summaries are canopy
   crums, not caches in the content tree, and each dsp is a `DspLoaf` node.

Around these sit the specializations the Gold source names. Part III borrows
some of the names; it does not rebuild the history crums or Gold's canopies:

- **`Loaf`** — a node of an orgl's content tree (the *O-tree*): a `SplitLoaf`
  splitting on a distinction, a `DspLoaf` displacing its child, or an
  `OExpandingLoaf` leaf. The tree is binary, and balance is not guaranteed.
  grmpl's node is different: a B+ tree node holding a run of up to 64 entries,
  stored as one record — see
  [the name-zoo](./names.md#gold-the-vocabulary-this-book-uses).

- **`HistoryCrum`** — every O-tree node has one, holding its **O-parents**: the
  nodes that contain it. The resulting H-tree is the O-DAG inverted. It drives
  *backfollow* ("which editions contain this content"): walk down to the
  leaves, then climb O-parent sets to the editions at the top. It also drives
  identity-based compare (`sharedRegion:`, `mapSharedTo:`), which asks what
  content two editions share wherever it sits. This is the read side of
  provenance.

- **`CanopyCrum`** — a node of a *canopy*, a shared binary tree of
  **permission and endorsement flags** OR-ed upward, so a walk can stop as soon
  as nothing below can pass. It has two halves. The **Bert canopy** hangs on
  the H-tree; its flags are the permissions and endorsements of the editions
  above, and it prunes backfollow.

- **`SensorCrum`** — a crum of the other half, the **Sensor canopy**, which
  hangs on the O-tree. Its flags are the filters of the standing queries
  (recorders) planted below, and it prunes the check for which of them an edit
  can affect. This is Gold's answer to the notification problem. grmpl's
  `canopy.rs` borrows the name for something else: an interval index of
  watchers.

- **Storage** — Gold never uses the word *granfilade*; that is Udanax Green's
  term. Gold stores **Abrahams**, persistent objects addressed by identity and
  updated in place, written in *flocks* packed into fixed-size *snarfs*.
  `GrandHashTable` is an extensible on-disk hash collection used inside the
  `BeGrandMap`, not the node store. grmpl's granfilade, which content-addresses
  nodes so equal subtrees are stored once, is grmpl's own design under Green's
  name.

## From a hypertext engine to a world

Xanadu built the `Ent` to hold *documents*. grmpl's wager is that the same
machine — measured trees, wids up, dsps down, structural sharing, a version DAG,
a canopy of interest — is the right substrate for a *relational, versioned
world*: a MOO. The mapping turns out to be remarkably direct, and stating it is
the job of the next part. First, though, it is worth pausing on *why* you would
want this machine at all — the properties that make it worth the trouble.
