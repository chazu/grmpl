# The desirable properties

Why rebuild a 1980s hypertext structure to run a multiplayer world? Because the
`Ent` bundles together a set of properties that are individually hard and, in
most systems, mutually antagonistic — history *versus* space, addressing *versus*
sparsity, copying *versus* cost. The enfilade gets them at once. This chapter is
the payoff list; Part III shows each one paying off in grmpl.

## 1. Nothing is ever overwritten — and that is affordable

Every version of the world is retained. That is trivial to *promise* and
ruinous to *implement* on ordinary storage, where retaining history means copying
it. Structural sharing is what makes it real: a new edition shares every
unchanged subtree with its parent, so keeping all of history costs the **sum of
the edits**, not the sum of the **sizes**. Permanence stops being a luxury.

## 2. Permanent, opaque identity

Addresses in Xanadu are *handles*, not integers you do arithmetic on. A span has
an identity that survives every later edit; a link to it keeps resolving because
the thing it names is a shared subtree that new versions still point at. grmpl
inherits this discipline as an explicit law: **editions and snapshots are
opaque**. A single machine may implement an edition as a monotonic counter; a
distributed one may use a causal frontier. Programs must not assume editions are
consecutive integers — so identity stays stable across implementations that
address time very differently.

## 3. `O(depth)` search of a sparse, transfinite space

Wids let you search an address space that is effectively unbounded and almost
entirely empty in time proportional to tree *depth*, not occupancy. Every
positional and range question — "what is at position P," "how many things lie in
`[lo, hi)`," "does anything dirty live under this scope" — is answered by reading
node summaries and pruning whole subtrees, in `O(log n)`. The alternative, on a
flat representation, is a linear scan. In a live world this is the difference
between a precondition check that is `O(history)` and one that is `O(depth)`.

## 4. `O(edit)` editing and relocation

Because content is addressed relatively (dsps) and stored persistently (path
copy), inserting, deleting, or **moving** a subtree costs only the edited path.
Relocating a huge subtree is a *single displacement change* at its top, not a
rewrite of everything inside it. The cost of a change tracks the *size of the
change*, which is the only scaling a live, edited-constantly world can tolerate.

## 5. The virtual copy

Structural sharing plus a displacement gives you a copy that shares storage with
its original and is placed wherever you like — `O(edit)` in time, near-zero in
space, and not a stale snapshot. In a world engine this is the mechanism behind
*instancing*: spin up a private copy of a dungeon, a room, or an entire zone per
party, sharing the template's structure until each instance diverges. What would
be an `O(state)` deep copy on a flat store is `O(edit)` here.

## 6. Interest management is built in

The canopy is an enfilade of *standing interest* with the same measured-tree
machinery as content. Because interest is summarized upward as a wid, a change to
a fact can be routed only to the observers whose interest actually covers it —
`O(log n + k)` stabbing, not a broadcast to every watcher. Subscriptions,
proximity sensors, room notifications, trigger regions, reactive UI: all one
mechanism, and that mechanism is a measured tree.

## 7. Context inherited down scopes

Dsps generalize from "displacement" to *any* context a subtree should receive
from its position: namespace, schema, authority, permissions, placement,
replication policy. A scope carries inherited context down; a wid summarizes what
lies below it up. This pairing is exactly the bridge to **clustering** — a
cluster partitions the tree along scope covers, migrates slices while identities
stay stable, and uses the interest summaries to know where updates must be
routed.

## 8. Cheap history unlocks world features

Once editions are cheap and permanent, a pile of otherwise-exotic features become
ordinary reads of old roots:

- time travel and auditing,
- lag compensation and replayable bug reports,
- forked instances and speculative NPC planning,
- undoable construction,
- snapshots for clients,
- testing behaviors against historical worlds.

None of these is a feature you *build*; each is a *consequence* of a substrate
that never overwrites and can address any past root in `O(depth)`.

---

These eight are why the `Ent` is worth the trouble. The rest of the book is grmpl
turning each of them from a property of a document engine into a property of a
programmable, relational, distributed world.
