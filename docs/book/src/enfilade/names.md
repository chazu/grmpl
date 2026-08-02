# The name-zoo: Green, Gold, and what grmpl kept

Xanadu ran for four decades and rewrote itself more than once, so its vocabulary
is not one vocabulary. Two designs matter for this book, and they name things
differently:

- **Xanadu Green** (the `xu88` lineage) — the classic three-enfilade design.
  Its names are the ones people usually quote: *textfilade*, *poomfilade*,
  *spanfilade*, and the *granfilade* beneath them.
- **Udanax Gold** — the later, more abstract design, and the one whose Smalltalk
  source was released. Its vocabulary is the `Ent`, the `Orgl`, the `DagWood`,
  the `Loaf`/`Crum` families, `Dsp`s and wids.

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
one enfilade *family* with specialized node types, and a versioning backbone over
them.

- **`Ent`** — the versioned-content backbone: `oroots` (content) plus
  `fulltrace` (history). The whole of the next chapter.
- **`Orgl` / `OrglRoot`** — one content structure rooted in an enfilade. Roughly
  "one document," or in grmpl's generalization, one *relation's* worth of facts.
  An `OrglRoot` is the handle you hold to a particular version of one orgl.
- **`TracePosition`** — a point in history. `oroots` maps a `TracePosition` to
  the `OrglRoot` current *at* that point. In grmpl a version point is the pair
  `(branch, edition)`, which is precisely a trace position.
- **`Crum`** — a **node** of a measured tree. The specializations are where
  Gold's design lives:
  - **`CanopyCrum`** — a node of the *canopy*, the tree of standing interest.
  - **`HistoryCrum`** — carries **trace membership** as an upward measure, so
    "is this content in edition E?" prunes instead of scanning.
  - **`SensorCrum`** — a node carrying an active *sensor*: a standing trigger
    that fires when a change lands under it. Where a `CanopyCrum` indexes who is
    watching, a `SensorCrum` is the watch itself, sited in the tree. grmpl's
    `Endorsement`-gated interests in `canopy.rs` occupy this role.
- **`Loaf`** — a **block of crums**, and the unit the granfilade actually stores.
  This distinction is easy to skip and worth keeping: a *crum* is a logical node;
  a *loaf* is the physical record holding a run of them. It is the difference
  between one disk record per item and one per *batch* of items — a constant
  factor, but the constant factor that decides whether the structure is usable at
  all. grmpl's tree holds a run of up to 64 entries per granfilade record for
  exactly this reason; a grmpl node is a loaf.
- **`Dsp`** — a displacement. Covered in
  [*Wids and Dsps*](./wids-dsps.md).
- **`DagWood`** — the branch structure of the `fulltrace`. Explained where it is
  declared, in [the next chapter](./the-ent.md#the-dagwood).
- **`GrandNode` / `GrandHashTable`** — the **granfilade**, the persistent node
  store. This is the one name Green and Gold share, and the one grmpl kept
  verbatim: `grmpl-ent/src/granfilade.rs`.

## The map to grmpl

Green's jobs do not disappear in grmpl; they are redistributed:

| Green's job | grmpl's answer |
|---|---|
| textfilade — permanent content | the **Fact enfilade**, versioned by edition; nothing is overwritten |
| I-stream / V-stream split | **editions**: facts are immutable, an edition is the arrangement current at a point in history |
| poomfilade — V-space → I-space | **`DspEnf`**: a displacement over shared content is a permutation of ordered material by another name |
| spanfilade — who quotes this span | **backfollow / version-compare** (`EntStore::compare`), plus the canopy for the *standing* form of the same question |
| granfilade | the **granfilade**, kept under its own name |
| tumblers | *not implemented* — tuple keys instead |

And Gold's, more directly:

| Gold | grmpl |
|---|---|
| `Ent` | the whole `grmpl-ent` crate |
| `oroots` : `TracePosition → OrglRoot` | the **Version enfilade**, `edition → Fact root` |
| `fulltrace` | the Edition enfilade (linear, within a branch) **+** the `DagWood` (between branches) |
| `Orgl` | one relation's facts |
| `Crum` | a `Tree` node |
| `Loaf` | a node's 64-entry run — one granfilade record |
| `CanopyCrum` / `SensorCrum` | `canopy.rs` — `InterestKey`, `Endorsement` |
| `HistoryCrum inTrace:` | trace membership as a WID measure — `KeyBounds`, used by version-compare |
| `Dsp` | `dsp.rs` — `Dsp`, `DspEnf` |
| `DagWood` | `dag.rs` — `Dag`, `Branch`, `BranchId` |
| `GrandNode` | `granfilade.rs` — content-addressed nodes |

With the zoo named, the rest of the book can use these words without apology.
