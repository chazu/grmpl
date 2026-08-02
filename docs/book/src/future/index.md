# Future enhancements

The point of paying the cost of a real `Ent` is not the features already
shipped — most of those *could* have been built, more slowly, on a flat store.
The point is what becomes *newly possible* once the substrate is a measured,
versioned, structurally-shared tree family. This chapter collects those: some are
concrete, deferred performance work with a named reason; others are capabilities
the Ent unlocks that grmpl has not yet reached for. All of them are *natural* on
the Ent and *awkward-to-impossible* on an append log.

## What has already come off this list

Everything Plan v5 tracked as a gap has landed, and the descriptions moved to
[the implementation chapter](../grmpl/implementation.md). They are worth naming
here, because each one was on this page as future work and is now load-bearing:

- **Path-only persistence** — a commit's *work*, not just its on-disk growth, is
  flat in relation size.
- **Durable forks sharing one granfilade** — a 5000-row fork encodes zero node
  frames.
- **Multi-order arrangements** — alternate column orderings per relation, so a
  *trailing*-column equality prunes at the source too, not just a lead-column
  one. It was, as predicted, purely "more measured trees."
- **Subtree-pruned version-compare** — backfollow now prunes at every node, on
  shared content keys and on disjoint `KeyBounds`, so comparing two editions
  costs the size of the difference.
- **Persistent Derived enfilades** — a materialized view is an ordinary relation
  in the Fact enfilade, so it survives a reopen and is carried by a fork.
- **The canopy on the reactive path** — the canopy is an enfilade, and the pump
  routes through the substrate instead of re-evaluating on every pump.

What follows is what is genuinely still ahead.

## Interest compilation into scope covers

The canopy routes per *watcher*: a commit stabs the interval enfilade and the
answer is a set of interests that could have been touched. That is the right
mechanism for local reactivity and the wrong granularity for a cluster, which
needs to know not "which watchers" but "which *machines*."

The step is compiling `watch` interest into **wid-summarized scope covers**, so
interest routing becomes a measure a cluster can read off a subtree rather than a
per-watcher stabbing: *"everything under this scope is watched for these kinds of
change."* The endorsement lattice is already the right shape for this — it is a
conservative upward summary — so the work is in the covers, not the algebra. This
is the bridge from local reactivity to distributed interest management, and it is
the last piece needed before the item below.

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

Backfollow / version-compare is implemented, and subtree-pruned: comparing two
editions costs the size of the difference, not the size of the relation. What is
missing is not the mechanism but the *surface*. Exposing it to the language and
to tooling — "show me every edition in which this fact held," "diff the world
between these two editions," "where did this content come from" — is Xanadu's
original transclusion-and-provenance vision, now over a relational world. This is
the job Green gave the **spanfilade**: given a span, find everywhere it is
quoted. Because provenance is a measure rather than a scan, these queries are
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
implementation with every distinctive structural component present *and on the
running system's path*. The building phase is over; the persistent derived layer
that used to head this chapter is now in the Fact enfilade with everything else.
What is left is to **spend** that structure — turning `O(depth)` search,
`O(edit)` copy, upward interest summaries, and downward inherited context into a
provenance surface, a parser, and, ultimately, a distributed world that migrates
freely while its identities never move. Those are not features you bolt on. They
are what the enfilade was for.
