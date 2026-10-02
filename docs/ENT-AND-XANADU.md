# grmpl and the Xanadu `Ent` — how close is our implementation?

grmpl's `ent` is **not** short for "entity." It is named for, and consciously
derived from, the **`Ent`** at the heart of Project Xanadu's "Gold" design —
K. Eric Drexler's versioning enfilade. grmpl's founding note
([`idea.md`](../idea.md)) opens the whole project as a thought experiment on *"how
to 'complete' the ent data structure plex,"* with the explicit design criterion
that the language *"should use the ent (or the set of data structures we settle
on as our version of the ent) as its backbone."*

This note answers: **how much of Xanadu's `Ent` does `grmpl-ent` actually
implement?** It was rewritten after `grmpl-store` (the fjall LSM stand-in) was
deleted and `grmpl-ent` became the only substrate; the previous version assessed
the LSM.

**The headline:** the *design* is faithful to the Ent, and `grmpl-ent` realizes
its most important ideas — never overwrite, path-copied structural sharing,
versions as roots, version compare that costs the size of the change, content-
addressed persistence, interest routing. But the tree itself is a
**content-addressed persistent B+tree keyed by absolute tuples**, with cached
monoid summaries. It has no displacements in its nodes, so it is not an
enfilade in the Udanax sense, and the Xanadu mechanisms that depend on relative
coordinates — `O(1)` virtual copy, DSP-inherited context — are not built.

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

## 3. What `grmpl-ent` implements

`grmpl-ent` builds every structure on one primitive, `tree::Tree<K, V, M>`, and
persists all of them through one node store, the `granfilade`.

**The primitive is a persistent B+tree, not an enfilade.**

* Nodes hold up to 64 entries or children (`tree::B`), are immutable and
  `Arc`-held, and an insert path-copies only the root-to-leaf spine
  (`Tree::insert`). A new version costs `O(log n)` new nodes and shares
  everything else. **This is genuine Ent-style structural sharing.**
* Keys are **absolute**: a node is located by its separator keys, and a range
  read prunes by passing absolute bounds down the descent (`Tree::fold_range`).
  No node stores a displacement and nothing composes on the way down. In an
  enfilade, a subtree's position is relative to its parent (the *dsp*), which is
  what lets a subtree be relocated or virtually copied by changing one number.
* Each node caches a monoid **measure** of its subtree (`measure::Measure`). This
  is a classic augmented tree. The only measure in use is `Count` (entry count),
  which answers "how many rows in this span" and "did anything change in this
  edition range" in `O(log n)`.
* Shape depends on the order of operations, so a content key identifies a
  *shape*, not a logical value. Sharing is within one version lineage.

**Structures built on it:**

| Structure | What it is | Persisted how |
|---|---|---|
| Fact trees | one `Tree<Tuple, Diff, Count>` per relation per live edition | nodes in the granfilade; the root pointer per edition in fjall's `meta` keyspace |
| Edition log | `(edition, index) → (tuple, diff)` | granfilade; root pointer in `meta` |
| Version / relation directories | `edition → Fact root`, the live-relation set | **in memory**, rebuilt from the `meta` root pointers on open |
| Context tree | the name→`RelId` catalog and edition-versioned schemas, at the root scope | granfilade; root pointer in `meta` |
| Canopy | interest intervals with a `max-hi` measure and an endorsement lattice, so a change routes only to watchers whose interval it stabs | **in memory**, rebuilt empty on open and fork; watchers re-register |
| Branch DAG | branches with at most one parent (a tree; no merges), and common-ancestor lookup | a hand-encoded blob in `meta` |
| Granfilade | `SHA-256(frame) → frame`, with mark-and-sweep GC | fjall `nodes` keyspace; loaded **eagerly** on open, so the world is RAM-resident |

Everything the language stores goes through these trees: world relations,
inboxes, cursors, timers, counters, outboxes and materialized views are all
ordinary relations in the Fact trees. The edition clock, watermark and root
pointers are raw values in fjall's `meta` keyspace.

**Operations:**

* **Version compare** (`Tree::diff`, used by `EntStore::compare` and the
  differential engine) skips any subtree the two versions share, by pointer or
  content key, so it costs the size of the difference. This is the Ent's
  version-comparison idea, realized.
* **Fork** (`EntStore::fork_at`) shares every fact node and writes none, but
  rebuilds each relation's version directory, so it costs
  `O(relations × versions)`, not `O(1)`.
* **DSP instancing** (`EntStore::instance_template`) reads a template block
  through a shifted view (`dsp::DspEnf`) and then **commits copied facts**, so an
  instance costs `O(template)`. A `Dsp` here is one shift of a whole query's
  entity coordinates, not a per-node displacement.

**Outside the Ent entirely:** the differential engine's working state —
arrangements and multisets in `grmpl-diff` — is plain in-memory hash maps. The
one persistent derived structure, `grmpl_proc::Materialized`, writes a view's
output into an ordinary relation, so it does live in the Ent, but nothing in the
runtime uses it yet.

---

## 4. Scorecard

| Ent / enfilade property | In the design (`idea.md`) | In `grmpl-ent` |
|---|---|---|
| Never overwrite; historical editions retained | ✅ core law | ✅ versions are roots; as-of reads |
| Opaque edition identity | ✅ explicit law | ✅ `Edition` is opaque to the language |
| Patch = guarded, atomic next edition | ✅ semantic center | ✅ `commit_if`, group-committed |
| Structural sharing / path copy | ✅ | ✅ `O(log n)` new nodes per commit |
| Version compare costs the edit | ✅ | ✅ `Tree::diff` prunes shared subtrees |
| Content-addressed persistent node store | ✅ granfilade | ✅ SHA-256 keyed, GC'd; eager load |
| Measured tree with upward summaries | ✅ "WIDative summaries" | ⚠️ augmented B+tree; `Count` only |
| **DSP displacements composing down the tree** | ✅ "DSPative inherited context" | ❌ absolute keys; no dsp in nodes |
| **`O(1)` virtual copy / relocation** | ✅ "cheap split/join" | ❌ instancing is `O(template)` |
| DSP-inherited context down scopes | ✅ Context enfilades | ❌ catalog and schemas only, at the root scope |
| Edition ancestry DAG (`fulltrace`) | ✅ Edition enfilades | ⚠️ branch tree, no merges; stored as a blob |
| Canopy indexing interest | ✅ Canopy enfilades | ⚠️ real interval routing, but in memory |
| Derived state in the Ent | ✅ Derived enfilades | ⚠️ `Materialized` exists, unwired; engine state in memory |
| Sequences as measured enfilades (§6 parsing) | ✅ | ❌ |
| Udanax Green 2D enfilades (poom/span) | — | ❌ |

---

## 5. Verdict

The design is a faithful, ambitious generalization of the Ent, and the
implementation has the Ent's **versioning** right: immutable versions, path
copying, cheap history, comparison that costs the change, content-addressed
persistence. What it does not have is the Ent's **coordinate system**. Its tree
is a Merkle B+tree over absolute tuple keys — closer to Datomic or Dolt than to
Udanax — with Xanadu vocabulary on some of its parts.

Making it a true enfilade means changing the primitive, not adding modules:

1. **Relative positions in nodes.** Each child carries a displacement relative
   to its parent, and a descent accumulates them. Widths become the extent of a
   subtree in that coordinate space, not just a cached count.
2. **`O(1)` relocation and virtual copy** fall out of (1): a template instance
   becomes a new parent node that points at the template's subtree with a
   different displacement, and diverges copy-on-write.
3. **Context inheritance** becomes a DSP down a scope tree rather than repeated
   point lookups.
4. **Persist what is now rebuilt:** the version and relation directories, the
   canopy, and the branch DAG as trees in the granfilade, and load lazily
   rather than eagerly.

`store.rs` and everything above it — the `TraceStore` contract, the language,
the laws — can stay as they are while the primitive changes underneath, and the
conformance suite (`grmpl-conformance`) is the place to run the current B+tree
as an oracle against the new one.

---

### Sources & method

* Xanadu Gold read directly: [`dotmpe/udanax-mpe`](https://github.com/dotmpe/udanax-mpe)
  `gold/udanax-top.st` — the `Ent` class (line 6092), and the `Loaf`/`Crum`/`Dsp`/
  `Orgl`/`CanopyCrum`/`GrandNode` families; `gold/udanax-spaces.st` — the
  `Arrangement`/`Dsp` coordinate spaces. Background:
  [Enfilade (Xanadu)](https://en.wikipedia.org/wiki/Enfilade_(Xanadu)),
  [xanadu.com/tech](https://xanadu.com/tech/).
* grmpl read directly: [`idea.md`](../idea.md) and `crates/grmpl-ent/src/`
  (`tree.rs`, `granfilade.rs`, `store.rs`, `dsp.rs`, `context.rs`, `canopy.rs`,
  `dag.rs`, `measure.rs`).
