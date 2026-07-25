# The Ent: content plus history

With the measured tree, wids, and dsps in hand, we can read the `Ent` itself. In
the Udanax Gold source it is declared, minus the ceremony, as:

```smalltalk
Abraham subclass: #Ent
    instanceVariableNames: '
        oroots   "a map: TracePosition → OrglRoot"
        fulltrace "a DAG of the whole version history" '
```

An `Ent` is the **versioned-content backbone**. It has two parts:

- **`oroots`** — a map from a *trace position* (a point in history) to an
  **`OrglRoot`**, the root of a content enfilade (one *orgl* — roughly, one
  document or one world-state). This is the *content* side: the actual measured
  trees of stuff.
- **`fulltrace`** — a **DAG** recording the entire version history: which edition
  descends from which, where branches split, how versions relate. This is the
  *history* side.

So the `Ent` is precisely **content + history**: a family of content enfilades,
plus a version graph that ties their roots together over time. Everything from
the earlier chapters is in service of these two.

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
