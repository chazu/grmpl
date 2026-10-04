# The name-zoo: Green, Gold, and what grmpl kept

Xanadu ran for four decades and rewrote itself more than once, so its vocabulary
is not one vocabulary. Two designs matter for this book, and they name things
differently:

- **Xanadu Green** (the `xu88` lineage) — the classic three-enfilade design.
  Its names are the ones people usually quote: *textfilade*, *poomfilade*,
  *spanfilade*, and the *granfilade* beneath them.
- **Udanax Gold** — the later, more abstract design, and the one whose Smalltalk
  source was released. Its vocabulary is the `Ent`, the `Orgl`, the `DagWood`,
  the `Loaf` and `Crum` families, and `Dsp`s. (*Wid* and *granfilade* are
  Green's words.)

grmpl follows **Gold** — that is where the `Ent` lives, and `grmpl-ent` is named
for it. But Green's names are the ones that survive in general circulation, and
several of them describe jobs grmpl *does* do under other names. This chapter is
the map, in both directions, so that no term in this book (or in the source) is
left as an unexplained noise.

## Green: the invariant stream and its three indexes

Green's central move is a split that grmpl inherits in spirit:

- The **I-stream** (*invariant stream*) is content that is **written once and
  never changes**. Every atom in it has a permanent address. Nothing is ever
  edited in place, because nothing in the I-stream is ever edited at all.
- The **V-stream** (*variant stream*) is a **document**: an ordered sequence of
  *references* into the I-stream. Editing a document rearranges references. It
  does not touch content.

Once you have that split, "editing" and "quoting" become the same operation —
both are just arrangements of references — and three indexes are needed to make
it work:

| Green enfilade | What it holds | The question it answers |
|---|---|---|
| **textfilade** | the I-stream itself — the permanent content | "what are the bytes at this invariant address?" |
| **poomfilade** | a document's map from V-space to I-space | "what content is at position 400 of *this* document?" |
| **spanfilade** | the inverse index: I-stream spans → the documents containing them | "which documents quote this span?" |
| **granfilade** | the storage substrate all of them are built on | "give me the node under this key" |

**POOM** stands for *Permutation Of Ordered Material*, which is exactly what a
document is in this design: a permutation over shared, immutable content. The
**spanfilade** is the one that makes Xanadu's signature feature possible —
*backfollow*, the ability to ask of any span "show me everywhere this is quoted"
and get an answer without scanning every document in the pool.

### Tumblers: the transfinite addresses

Both earlier chapters lean on the phrase "a sparse, effectively transfinite
address space" without naming the thing that implements it. Green's answer is the
**tumbler**: an address that is not an integer but a *hierarchically structured*
number, written as a sequence of digit-groups. Tumblers can always be subdivided
— between any two of them there is room for infinitely many more — so you can
always insert new material "between" two existing positions without renumbering
anything after it. Tumbler arithmetic (comparison, subtraction, span containment)
is what a wid-pruned descent is actually computing over.

grmpl **does not implement tumblers.** Its addresses are tuple keys, and its
density comes from the tuple ordering rather than from digit-group arithmetic.
The property tumblers bought — insert anywhere without renumbering — grmpl gets
instead from the fact that facts are *keyed by content*, not by position, so
there is no ordinal to renumber. This is a genuine divergence and worth naming as
one.

## Gold: the vocabulary this book uses

Gold generalizes the above. Rather than three special-purpose enfilades it has
one content-tree *family* (the O-tree), history and canopy trees hung off its
nodes, and a versioning backbone over them.

- **`Ent`** — the versioned-content backbone. It declares `oroots` and
  `fulltrace`, but `oroots` is vestigial (`smalltalkOnly`, `NOCOPY`, its stores
  commented out); the live `Ent` is the `fulltrace` and its `newTrace`. The
  whole of the next chapter.
- **`Orgl` / `OrglRoot`** — one content structure rooted in a tree. Roughly
  "one document," or in grmpl's generalization, one *relation's* worth of facts.
  An `OrglRoot` is the handle you hold to a particular version of one orgl; it is
  stamped with a trace position and owned by the editions that use it.
- **`TracePosition`** — a point in history: `BoundedTrace(branch, position)`,
  ordered by `isLE:`. In grmpl a version point is the pair `(branch, edition)`,
  which is precisely a trace position.
- **`Loaf`** — a **node** of an orgl's content tree, the *O-tree*. The tree is
  a binary splay tree, not a B-tree: a `SplitLoaf` splits on a distinction, a
  `DspLoaf` displaces its one child, and the leaves are `OExpandingLoaf`s.
  Balance is not guaranteed. Loaves cache no summary of what lies below.
  grmpl's own node is not a loaf: it is a B+ tree node holding a run of up to
  64 entries in one granfilade record, one disk record per *batch* of items
  rather than per item. That is a constant factor, but the one that decides
  whether the structure is usable at all.
- **`Crum`** — a node of the trees hung *off* the O-tree. The specializations
  are where much of Gold's design lives:
  - **`HistoryCrum`** — every O-tree node has one, holding its **O-parents**,
    the nodes that contain it. The H-tree is the O-DAG inverted, and it is what
    *backfollow* ("which editions contain this content") and identity-based
    compare (`sharedRegion:`, `mapSharedTo:`) climb.
  - **`CanopyCrum`** — a node of a *canopy*: a shared binary tree of
    **permission and endorsement flags**, OR-ed upward. The **Bert canopy**
    hangs on the H-tree and prunes backfollow.
  - **`SensorCrum`** — a crum of the **Sensor canopy**, which hangs on the
    O-tree. Its flags are the filters of the standing queries (recorders)
    planted below, and it prunes the check for which of them an edit can
    affect. grmpl's `canopy.rs` borrows the name for an interval index of
    watchers, with OR-ed `Endorsement` flags on the watchers rather than on
    content.
- **`Dsp`** — a displacement, held in Gold by a `DspLoaf` node. Covered in
  [*Wids and Dsps*](./wids-dsps.md).
- **`DagWood`** — the branch structure of the `fulltrace`. Explained where it is
  declared, in [the next chapter](./the-ent.md#the-dagwood).
- **`GrandHashTable`** — an extensible on-disk hash **collection**, used inside
  the `BeGrandMap` (which registers range elements by id). It is not a node store,
  and Gold never uses the word *granfilade*. Gold's storage is **Abrahams**:
  persistent objects addressed by identity and updated in place, written in
  *flocks* packed into fixed-size *snarfs*. grmpl took the name *granfilade*
  from Green for its own content-addressed node store,
  `grmpl-ent/src/granfilade.rs`.

## The map to grmpl

Green's jobs do not disappear in grmpl; they are redistributed:

| Green's job | grmpl's answer |
|---|---|
| textfilade — permanent content | the **Fact enfilade**, versioned by edition; nothing is overwritten |
| I-stream / V-stream split | **editions**: facts are immutable, an edition is the arrangement current at a point in history |
| poomfilade — V-space → I-space | **graft**: a displaced pointer to shared content places ordered material under another name |
| spanfilade — who quotes this span | **backfollow / version-compare** (`EntStore::compare`), plus the canopy for the *standing* form of the same question |
| granfilade | the **granfilade**, kept under its own name |
| tumblers | *not implemented* — tuple keys instead |

And Gold's, more directly:

| Gold | grmpl |
|---|---|
| `Ent` | the whole `grmpl-ent` crate |
| `OrglRoot` stamped with a `TracePosition` (`oroots` is vestigial) | the **Version enfilade**, `edition → Fact root` |
| `fulltrace` | the Edition enfilade (linear, within a branch) **+** the `DagWood` (between branches; grmpl has no merges) |
| `Orgl` | one relation's facts |
| `Loaf` (binary splay-tree node) | a `Tree` node (B+ tree, 64-entry run, one granfilade record) |
| `HistoryCrum` (O-parents), backfollow | *not implemented* — version-compare prunes on shared content keys instead |
| `CanopyCrum` / `SensorCrum` (flag canopies) | `canopy.rs` borrows the name: an interval index of watchers (`InterestKey`, `Endorsement`) |
| `DspLoaf` | the dsp on every `Tree` handle; `dsp.rs` — `Displace` |
| `DagWood` | `dag.rs` — `Dag`, `Branch`, `BranchId` |
| Abrahams in flocks and snarfs | `granfilade.rs` — content-addressed nodes, grmpl's own design |

With the zoo named, the rest of the book can use these words without apology.
