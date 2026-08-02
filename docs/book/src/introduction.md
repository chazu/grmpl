# Introduction

> A program does not mutate objects. It queries an immutable edition of the
> world and produces a guarded patch that creates the next edition.

**grmpl** is a differential, relational substrate for *deriving, watching, and
patching a versioned world*. Its intended use case is high-performance,
distributed multiplayer worlds in the vein of [MOO](https://en.wikipedia.org/wiki/MOO):
persistent shared spaces where many people (and programs) build, script, and
inhabit a world that is never turned off and never overwritten.

That is an ambitious pile of requirements — persistence, history, concurrency,
streaming subscriptions, live code, distribution — and the temptation is to
solve each with a separate subsystem bolted to the side. grmpl takes the
opposite bet. It starts from a single data structure and asks how much of the
system can *fall out* of it:

- Relations and joins,
- Streams as the derivative of queries,
- Persistence as an immutable log of editions,
- Concurrency as guarded, optimistic patches,
- Subscriptions as a maintained interest index,
- Time travel, forking, and replay as cheap consequences of never overwriting.

The data structure at the bottom is not new. It is the **enfilade**, and the
particular arrangement of enfilades grmpl reaches for is the **`Ent`** — the
versioning backbone of Ted Nelson and K. Eric Drexler's Project Xanadu, the
original hypertext system. grmpl's `ent` is *not* short for "entity." It is
named for, and consciously derived from, that `Ent`.

This book tells the whole arc:

1. **[Part I](./enfilade/story.md)** — the story and the mechanics of the
   enfilade and the Ent: what problem it was invented to solve, how a *measured
   tree* with two kinds of measure (wids and dsps) works, and what the `Ent`
   assembles out of that machinery. It ends with a
   [name-zoo](./enfilade/names.md) mapping Xanadu's two vocabularies — Green's
   *poomfilade* and *spanfilade*, Gold's `Ent` and `DagWood` — onto each other
   and onto grmpl.

2. **[Part II](./properties/index.md)** — the desirable properties that make the
   Ent worth rebuilding: permanent identity, `O(depth)` search of a
   transfinite space, structural sharing, cheap virtual copies, and built-in
   interest management.

3. **[Part III](./grmpl/plex.md)** — grmpl's implementation: the "completed Ent
   plex" as a coordinated *family* of enfilades, the seven design laws, the
   `grmpl-ent` crate that realizes the Ent on disk, and a tour of the features
   shipped so far — each one shown resting on a specific feature of the Ent.

4. **[Part IV](./future/index.md)** — the enhancements the Ent makes newly
   *possible*: things that are hard or impossible on a flat store but become
   natural once the substrate is a measured, versioned, structurally-shared
   tree.

A note on honesty of framing. grmpl earns the name `Ent` by its **contract and
its laws**, and — as of the work described here — by an actual `grmpl-ent`
implementation with the distinctive Xanadu structural components present:
measured enfilade, granfilade, WID pruning, DSP coordinate transforms,
structural-sharing fork, an edition/branch DAG, and an interval-tree canopy.
Where something is a semantics-complete stand-in, or a deliberately deferred
optimization, this book says so plainly.
