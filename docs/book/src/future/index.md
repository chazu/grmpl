# Future enhancements

The point of paying the cost of a real `Ent` is not the features already
shipped — most of those *could* have been built, more slowly, on a flat store.
The point is what becomes *newly possible* once the substrate is a measured,
versioned, structurally-shared tree family. This chapter collects those: some are
concrete, deferred performance work with a named reason; others are capabilities
the Ent unlocks that grmpl has not yet reached for. All of them are *natural* on
the Ent and *awkward-to-impossible* on an append log.

## Near-term: optimizations of already-correct capabilities

These are deferred deliberately — the capability exists and is correct; only its
asymptotics are on the table.

### Multi-order arrangements

The lead-column WID pushdown is done: an entity-keyed view prunes to its key at
the source. **Multi-order arrangements** maintain several column orderings per
relation (several measured trees over the same facts), so `RangeRel` can prune on
*trailing* columns too — making secondary-key preconditions and joins sublinear
as well. Non-lead filters run correctly today; they just run unpruned. This is
purely "more measured trees," the same primitive replicated per order.

### Durable fork sharing one granfilade

In-memory structural-sharing forks and the DagWood ancestry are done. True
`O(edit)` **durable** node-sharing wants all branches living in *one* granfilade,
with roots namespaced by branch, so a fork on disk shares nodes with its ancestor
rather than copying them. This is a persistence-layer design — a durable *copy*
fork would be weaker than the faithful sharing form, so it is left for the real
design rather than hacked in.

### Path-only persistence

On-disk structural sharing already holds — a commit grows the store by only the
edited path. The remaining win is making the per-commit *traversal* `O(log n)`
instead of `O(n)` by memoizing each node's content key. Deferred because a safe
implementation must dodge pointer-ABA and a tree↔granfilade layering break: an
invasive core change, performance-only, not to be done hastily.

## Persistent Derived enfilades

Today the Derived enfilade lives as *behavior*: `grmpl-diff` maintains views
incrementally, but arrangements are an in-memory per-eval memo. Making them a
**persistent, shared derived tree** — materialized views that survive reopen and
share structure across editions like the Fact enfilade does — turns incremental
view maintenance into a first-class, durable member of the plex. This is the one
enfilade with no Xanadu ancestor, and its persistent form is where grmpl's
differential extension of the `Ent` fully lands.

## Full canopy interest compilation

The canopy is a real interval tree with an endorsement lattice, routing
conservatively. The frontier is compiling `watch` interest into **wid-summarized
scope covers** so that interest routing is not just per-watcher stabbing but a
measure the cluster can read off a subtree: "everything under this scope is
watched for these kinds of change." That is the bridge from local reactivity to
distributed interest management.

## Distribution along scope covers (P15)

This is the big one, and it is why dsps and wids were worth building. A cluster
**partitions the enfilade along canonical scope covers**. Because context is
inherited down (dsps) and content is summarized up (wids):

- relation slices and actors **migrate while entity identities stay stable** —
  identity is a handle, not a location;
- the language never exposes "node," "shard," or "network call" — only authority
  and asynchronous process boundaries, which the runtime maps to threads,
  processes, or machines;
- **canopy interest summaries tell the cluster where updates must be routed** —
  the same measure that powers local `watch` powers cross-machine delivery.

The Authority law already guarantees one commit touches one domain and
cross-domain effects are messages — so the distributed story is *additive*, not a
rewrite. The Ent is the thing that makes migration cheap (virtual copy / relocate
a subtree) and routing precise (wid-summarized interest).

## Cheap history unlocks world features

Every one of these is an ordinary read of an old root or a fork of the world —
consequences of a substrate that never overwrites and can address any past
edition in `O(depth)`:

- **Time travel and auditing** — `find q at E` for any past `E`.
- **Replayable bug reports** — a bug is a snapshot plus a message; replay is
  deterministic by law.
- **Lag compensation** — resolve an action against the edition the client saw.
- **Forked instances and instanced worlds** — `fork_edition` is an `O(edit)`
  virtual copy of the *whole world*; the MOO's `enter vault` already does this at
  room scale.
- **Speculative NPC planning** — fork, simulate ahead, discard or commit.
- **Undoable construction** — building is patching; undo is an older root.
- **Client snapshots** — hand a client an opaque edition; deltas bring it
  forward.
- **Testing behaviors against historical worlds** — run new code against the
  exact state a bug occurred in.

## Provenance and version-compare as a first-class surface

Backfollow / version-compare is implemented as an `O(measure)` operation
(trace membership is a WID upward measure). The enhancement is exposing it as a
*language and tooling* surface: "show me every edition in which this fact held,"
"diff the world between these two editions," "where did this content come from" —
Xanadu's original transclusion-and-provenance vision, now over a relational
world. Because provenance is a measure rather than a scan, these queries are
cheap enough to be interactive.

## Parsing and transformation over measured sequences

The **Pattern law** says parsing is matching over ordered data, and the pattern
engine already uses content-keyed match arrangements and windowed measured
matching over sequences. Pushing this further — representing token streams, ASTs,
and byte streams as **measured enfilades** — lets parsing reuse the very same
split/search/summary/incremental-update machinery as the rest of the system:
incremental reparse of a huge input touches only the edited span, ambiguous
parses come back as a *relation* of possibilities, and sufficiently invertible
forms can run backward to print. Parsing stops being a separate subsystem and
becomes one more enfilade.

---

The through-line: grmpl already earns the name `Ent` by its laws and by an
implementation with every distinctive structural component present. What is left
is to spend that structure — turning `O(depth)` search, `O(edit)` copy, upward
interest summaries, and downward inherited context into a persistent derived
layer, a provenance surface, and, ultimately, a distributed world that migrates
freely while its identities never move. Those are not features you bolt on. They
are what the enfilade was for.
