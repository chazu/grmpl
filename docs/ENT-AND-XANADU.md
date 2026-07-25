# grmpl and the Xanadu `Ent` — how close is our implementation?

grmpl's `ent` is **not** short for "entity." It is named for, and consciously
derived from, the **`Ent`** at the heart of Project Xanadu's "Gold" design —
K. Eric Drexler's versioning enfilade. grmpl's own founding note
([`idea.md`](../idea.md)) opens the whole project as a thought experiment on *"how
to 'complete' the ent data structure plex,"* with the explicit design criterion
that the language *"should use the ent (or the set of data structures we settle
on as our version of the ent) as its backbone."*

So the right question is not "is this a coincidence?" (it isn't) but: **how much
of Xanadu's `Ent` does grmpl actually implement today?** This note answers that by
reading both sides directly — the Udanax Gold Smalltalk source (`udanax-top.st`)
and grmpl's design note and store.

**The headline:** grmpl faithfully implements the `Ent`'s **semantic contract and
design laws**, but runs them today on an **LSM log that is a stand-in for the
`Ent`'s data structure**, not the measured, WID/DSP-summarized persistent tree
the design names. The vision is deeply Xanadu; the current storage layer is not
yet an enfilade.

---

## 1. What Xanadu Gold's `Ent` is (from the source)

`gold/udanax-top.st:6092`:

```smalltalk
Abraham subclass: #Ent
    instanceVariableNames: '
        oroots {MuTable NOCOPY smalltalk of: TracePosition and: OrglRoot}
        fulltrace {DagWood}'
    category: 'Xanadu-Be-Ents'!
```

An `Ent` is the **versioned-content backbone**: a map from `TracePosition` →
`OrglRoot` (each *orgl* is a content structure rooted in an **enfilade**) plus a
`fulltrace` **DAG** of the whole version history. Around it, the same file carries
the classic enfilade machinery:

* **`Loaf`/`Crum`** families — the nodes of a *measured tree* (a B-tree-like
  structure). `CanopyCrum`, `HistoryCrum`, `SensorCrum` are specializations.
* **`Dsp`** (displacement) family — a subtree's key is its parent's key *plus a
  displacement*, so relocating or virtually-copying a subtree is a cheap key
  change, and context flows **down** the tree.
* **wids** (widths) — every node advertises *the range of addresses its subtree
  covers*, so a sparse, effectively transfinite space is searchable in
  `O(depth)`; summaries flow **up** the tree.
* **`GrandNode`/`GrandHashTable`** — the *granfilade*, the persistent storage.

Two moves make the `Ent` powerful: **structural sharing** (a new version shares
every unchanged subtree, allocating new crums only along the edited path → an edit
or virtual copy is `O(edit)`), and **dual measures** (dsps carry context down,
wids summarize content up).

---

## 2. What grmpl means by "the Ent" (from the design note)

grmpl does *not* try to make one universal `Ent`. `idea.md` §1 defines the backbone
as **a coordinated family of persistent enfilades**:

> 1. **Fact enfilades** contain stored relations and their indexes.
> 2. **Edition enfilades** preserve historical roots, patches, branches, and
>    causal ancestry.
> 3. **Context enfilades** carry **DSPative** information down scopes: authority,
>    namespace, schema, permissions, placement…
> 4. **Canopy enfilades** index standing queries, subscriptions, sensors…
> 5. **Derived enfilades** maintain materialized views and incremental query
>    state.

over a common physical abstraction that is transparently the enfilade:

> ```
> persistent measured action tree
>     + stable node identities
>     + cheap split/join
>     + WIDative subtree summaries
>     + DSPative inherited context
>     + historical editions
>     + canopy indexes
> ```

This is Xanadu's `Ent`, **generalized** from a hypertext-document engine to a
relational/differential world substrate. The mapping is remarkably direct:

| Xanadu Gold `Ent`/enfilade         | grmpl's envisioned enfilade (`idea.md`)                         |
|------------------------------------|----------------------------------------------------------------|
| orgl content trees (`OrglRoot`)    | **Fact enfilades** — relations + their indexes                 |
| `fulltrace` DAG + versioned roots  | **Edition enfilades** — historical roots, patches, branches, ancestry |
| `Dsp` inherited displacement       | **Context enfilades** — DSPative authority/namespace/schema down scopes |
| `CanopyCrum` / upward interest     | **Canopy enfilades** — standing queries, subscriptions, sensors |
| (no equivalent)                    | **Derived enfilades** — differential materialized views (grmpl's addition) |
| wids (width summaries up)          | **WIDative measurements** — fact kinds, key ranges, entity counts, spatial bounds, dirty regions, subscription interests (`idea.md` §10) |

grmpl even keeps Xanadu's key discipline as an explicit law: **editions/snapshots
are opaque** (`idea.md` §10 — "A single-node implementation may use a
monotonically increasing commit number… User programs should not assume editions
are globally consecutive integers"), exactly the Xanadu stance that addresses are
handles, not integers you do arithmetic on.

So at the level of **design**, grmpl is a faithful, ambitious reconstruction of
the `Ent` — with two deliberate extensions Xanadu never had: the **relational /
Datalog** data model (facts, joins, recursive views) and **differential
dataflow** (Derived enfilades, `watch` = the maintained derivative of `find`).

---

## 3. What grmpl actually *implements* today

Here is the honest part. The shipping backbone is
[`grmpl-store`](../crates/grmpl-store) — a **fjall LSM** (log-structured merge
tree), described in [`docs/PERFORMANCE.md`](PERFORMANCE.md). It delivers the
`Ent`'s **semantics** but not its **data structure**:

* **Facts** are an **append-only log per relation** (`key = edition‖counter`),
  consolidated on read — **not** a measured tree. There are **no WIDative subtree
  summaries and no secondary index**. Consequences, straight from the perf notes:
  a precondition check (`holds_at`) is **O(relation history)** linear scan, and
  every join **re-reads whole relations**. This is *precisely* the cost a wid/dsp
  measured tree exists to avoid (`O(depth)` range/containment).
* **Editions** are a global monotonic clock over the log, with **checkpoints**
  (consolidation) to bound history — a real *edition* model, but a *flat log*, not
  an Edition enfilade with a branch/ancestry DAG. There is no `fulltrace`-style
  causal DAG in the store yet (branches exist only as separately-forked stores).
* **Copies** (`fork`) are a **verbatim `O(state)` copy** of the keyspaces — the
  opposite of structural sharing. A Xanadu virtual copy is `O(edit)`; grmpl's is
  `O(everything live)`.
* **DSPative context** is **not propagated through a tree**. Authority/schema are
  real (checked at the commit boundary via `Authority` scopes and the schema
  catalog), but they are looked up, not inherited down an enfilade's dsps. There
  is no Context enfilade.
* **The canopy is real but not enfilade-indexed.** `on watch` is a genuine
  maintained-query pump with exactly-once durable delivery (the Attention law
  holds), but interest routing is per-watcher, not compiled into wid-summarized
  scope covers — the `idea.md` "sensor canopy" is semantics-complete and
  structure-incomplete.
* **Derived enfilades** exist as behavior, not storage: `grmpl-diff` maintains
  views incrementally (`eval_delta`, arrangements), but arrangements are an
  in-memory per-eval memo, not a persistent derived tree.

The one place grmpl already reaches the `Ent`'s *data-structure* idea is the
**pattern engine**: `grmpl-pattern`/`grmpl-diff` P9c uses a content-keyed
`MatchArrangement` and windowed measured matching over sequences — "sequence data
… represented by measured enfilades" (`idea.md` §6) is partially real there,
including a **stable, content-addressed** arrangement identity (the fix for the
`Arc::as_ptr` memo hazard). That is the closest the codebase comes to an actual
measured/summarized tree.

---

## 4. Scorecard: `Ent` property → in grmpl's vision? in its implementation?

| `Ent` / enfilade property                         | grmpl design (`idea.md`) | grmpl implementation (today)                        |
|---------------------------------------------------|--------------------------|-----------------------------------------------------|
| Never overwrite; historical editions retained     | ✅ core law              | ✅ append log + editions + as-of reads              |
| Opaque edition/snapshot identity                   | ✅ explicit law          | ✅ `Edition` opaque; replay/forks proven identical  |
| Patch = guarded, atomic next-edition               | ✅ semantic center       | ✅ `commit_if`, one authority domain per commit     |
| Watch = maintained derivative of find              | ✅ Attention law         | ✅ `on watch`, exactly-once durable canopy pump     |
| Persistent **measured tree** (Loaf/Crum)           | ✅ "measured action tree" | ❌ LSM append-log per relation                      |
| **WIDative** upward summaries (range/measure index) | ✅ "WIDative summaries"  | ❌ none → O(history) preconds, O(relation) joins    |
| **DSPative** inherited context down scopes         | ✅ "Context enfilades"   | ⚠️ authority/schema checked, not tree-propagated    |
| **Structural sharing** / cheap virtual copy        | ✅ "cheap split/join"    | ❌ `fork` is O(state) verbatim copy                 |
| Branch/ancestry DAG (`fulltrace`)                  | ✅ "Edition enfilades"   | ⚠️ editions are linear; branches = separate forks   |
| Canopy interest compiled to scope covers           | ✅ "Canopy enfilades"    | ⚠️ per-watcher pump, not wid-indexed routing        |
| Differential derived state                         | ✅ "Derived enfilades" (extends Xanadu) | ⚠️ in-memory arrangements, not persistent |
| Measured trees for **sequences/parsing**           | ✅ §6                    | ⚠️ partial: P9c content-keyed `MatchArrangement`    |

---

## 5. Verdict

grmpl is, by intent and by its laws, **a reconstruction of Xanadu's `Ent`** — and
a genuine generalization of it, from a hypertext-document engine to a relational,
differential, versioned world substrate. Everything the `Ent` is *for* — a world
that is never overwritten, permanent opaque identity, cheap history, maintained
interest — is present and load-bearing in grmpl, encoded as explicit design laws
(`idea.md` §12) rather than lore.

What grmpl has **not yet built** is the `Ent`'s actual *data structure*. The
current store is a pragmatic LSM that provides the enfilade's **semantics** over a
flat append log — no measured tree, no WID summaries, no DSP-inherited context, no
structural sharing. That gap is exactly why the perf notes flag O(history)
preconditions and O(relation) joins as the open problem: those are the costs a
wid/dsp measured tree makes `O(depth)`. In other words, **the distance between
grmpl-today and Xanadu's `Ent` is precisely the distance `idea.md` already
names** — replace "append log + linear scan + verbatim fork" with a "persistent
measured action tree + WIDative summaries + DSPative context + structural
sharing." The roadmap's P13 statefulness / indexed-lookup work is the first step
of that migration; the P9c content-addressed match arrangements are a first
foothold on the other side.

So: **semantically, a close and faithful implementation of the Ent; structurally,
an LSM stand-in that has not yet become an enfilade.** The name is earned by the
contract, not (yet) by the tree.

---

### Sources & method

* Xanadu Gold read directly: [`dotmpe/udanax-mpe`](https://github.com/dotmpe/udanax-mpe)
  `gold/udanax-top.st` — the `Ent` class (line 6092), and the `Loaf`/`Crum`/`Dsp`/
  `Orgl`/`CanopyCrum`/`GrandNode` families; `gold/udanax-spaces.st` — the
  `Arrangement`/`Dsp` coordinate spaces. Background:
  [Enfilade (Xanadu)](https://en.wikipedia.org/wiki/Enfilade_(Xanadu)),
  [xanadu.com/tech](https://xanadu.com/tech/).
* grmpl read directly: [`idea.md`](../idea.md) (the founding design note — the
  "completed Ent plex," §1, §5, §6, §10, §12), `crates/grmpl-core/src/value.rs`,
  `crates/grmpl-store/src/lib.rs`, and [`docs/PERFORMANCE.md`](PERFORMANCE.md).
