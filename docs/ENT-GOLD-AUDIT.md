# The Ent against Gold's source: an audit

**What this is:** a component-by-component reading of Udanax Gold's Ent, with
line citations, and how `grmpl-ent` compares with each component. It replaces
guesses in [`ENT-AND-XANADU.md`](ENT-AND-XANADU.md) with what the source says.
**Source:** [`dotmpe/udanax-mpe`](https://github.com/dotmpe/udanax-mpe) at
`0791030`, `gold/udanax-top.st` (cited as plain line numbers) and
`gold/udanax-spaces.st` (cited as *spaces*). Gold never uses the word
"granfilade"; that term is Udanax Green's.
**Date:** 2026-10-03.

Classifications:
* **faithful:** grmpl does what Gold does;
* **extrapolated:** a reasonable generalization of Gold;
* **divergent:** grmpl does it differently;
* **missing:** Gold has it and grmpl does not;
* **grmpl only:** not in Gold at all.

---

## 1. What Gold's Ent is, from the source

Gold's backend is six interlocking structures. The Ent proper is only the
first; the rest hang off every node it manages.

### 1.1 The trace: a partial order of versions

* `Ent` has two fields, `oroots` and `fulltrace {DagWood}` (6092–6095).
  `oroots` is vestigial: built `smalltalkOnly`, its stores are commented out,
  and it is never transmitted (6112, 6141, 6225, 6232–6236). **The live Ent is
  the DagWood**, and its one live operation is `newTrace` (6105).
* A `TracePosition` is `BoundedTrace(branch, position)` (62924). `isLE:` asks
  whether one position is in another's past, by caching the maximum position
  reached on each ancestor branch (62967, 4264–4268).
* **Branching is implicit.** Asking for a successor of a branch's tip extends
  the branch; asking at any other position opens a new branch there
  (4297–4306, 4333–4336).
* **Merges exist.** `newSuccessorAfter:` returns "a new tracePosition that is
  after both" by making a `DagBranch` with two parents (62999–63009,
  4395–4411). The class comment names root, tree and dag branches
  (4245–4247).
* **Every derived edition gets a new trace.** `copy:` and `transformedBy:` take
  `newSuccessor`; `combine:` of two editions takes `newSuccessorAfter:`, a
  merge (2534, 2541, 2559–2563, 2039).

### 1.2 The O-tree: a binary splay tree over regions

* An orgl's content tree is binary (7834–7852):
  * a `SplitLoaf` splits on a **distinction**, everything in `myIn` inside a
    region and everything in `myOut` outside it (8102–8107). On a cross space a
    distinction cuts one dimension, so the tree partitions like a k-d tree
    (*spaces* 9805);
  * a `DspLoaf` is a unary node that displaces its child (7854–7858);
  * leaves are `OExpandingLoaf`s, which a splay may turn into a `SplitLoaf` in
    place (8556–8565).
* **Three leaf kinds.**
  * `RegionLoaf` maps a whole region to one shared range element: run-length
    content (9285–9307).
  * `OVirtualLoaf` holds a primitive array and fakes Be objects until asked
    (9072–9131): lazy content.
  * `OPartialLoaf` is a placeholder for content not yet supplied (8779–8871).
* **No summary is cached in O-tree nodes.** `count` and `domain` recurse
  (8121–8124); only leaves and the root store a region. Search prunes on the
  splits and on a `limitRegion` narrowed on the way down (8127–8133, 8264–8270).
  The upward summaries live in the canopy crums (§1.4).
* **Displacements.** `transformedBy:` wraps a node in a `DspLoaf` in `O(1)`;
  nested dsps compose rather than stack (7455–7460, 7970–7975). Point lookups
  move the query down through `inverseOf:`; regions come up through `ofAll:`
  (7875–7902). A `GenericCrossDsp` shifts each dimension independently
  (29266–29368).
* **Copy and combine.** `copy:` splays the region's distinctions into one
  subtree and shares it (10066–10098). The splay mutates shared nodes in place
  (8475–8515). `combine:` is a disjoint union that may interleave (8216–8234,
  10179–10193).
* **Balance is not guaranteed.** Only copying splays; notes say "should
  softSplay" and "This should be splaying!!" (7401, 8137), and
  `actualSoftSplay` has no caller.

### 1.3 The H-tree: history and backfollow

* Every O-tree node and every range element has a history crum (7377, 2242).
  An `HUpperCrum` holds its **O-parents**, the nodes that contain it, plus a
  trace cut and a bert crum (27493–27497, 27556). An `HBottomCrum` belongs to
  an orgl root and holds its trace and the **editions** using it (27314–27318).
* **The H-tree is the O-DAG inverted.** Building a container calls
  `child addOParent: self` (8054, 8288, 9447), and the invariant is "parent's
  trace >= child's trace" (27110). A shared subtree is one with several
  O-parents (27113–27117). The upward sets double as reference counts: a node
  with none destroys itself (7545–7553, 9670–9677).
* **Backfollow** answers "which editions contain this content":
  1. walk south on the O-tree to the leaves of the queried region (8013–8018,
     8380–8385);
  2. turn north at each leaf (8622–8631) and climb O-parent sets, skipping
     crums already seen (27190–27203);
  3. at each crum, narrow the query by the bert canopy and stop if nothing can
     pass (27577–27595);
  4. at a bottom crum, hand each using edition to the recorder (27372–27381);
  5. for indirect queries, continue north from each found edition, since an
     edition is itself content of other editions (44690–44698).
* **Version compare is identity, not position.** `sharedRegion:` and
  `mapSharedTo:` ask which keys of A hold content that is also in B, wherever it
  sits (3001–3021). The walk returns a whole subtree when its history crum is
  `inTrace:` B's trace, pruned by `isLE:` (7963–7968, 27507–27518), and
  `compare:` returns a mapping across dsps (27547–27554).
* **Content identity is range elements.** A `RegionLoaf` shares range-element
  identity with every other leaf over the same element (9490–9498). Two equal
  values written separately are different content.

### 1.4 Canopies: summaries shared across trees

* A `CanopyCrum` is a node of its own binary tree, with a 32-bit flag word
  OR-ed upward (4504–4513, 4530, 4853). It is shared by pointer: many O-tree and
  H-tree crums point at one canopy crum, and `computeJoin` reuses an existing
  ancestor (4549–4556).
* **Bert canopy:** hung on the H-tree. Its flags are the permissions and
  endorsements of the editions above. It prunes backfollow (5115, 5254–5278,
  27587–27603).
* **Sensor canopy:** hung on the O-tree. Its flags are the filters of the
  standing queries planted below. It prunes the check for which standing
  queries an edit can affect (5279, 5456–5476, 7508–7523).
* A prop change propagates rootward one crum per persistent step, stopping as
  soon as the flags stop changing (895–911, 4831–4857).

### 1.5 Recorders and the Agenda

* A `ResultRecorder` is "the persistent embodiment of a query" (44473). An
  `EditionRecorder` stands for "transcluders of this", a `WorkRecorder` for
  "works containing this", each direct or indirect (44589, 44703).
* Registering one runs **the future, then the past**, so nothing falls between:
  plant recorders on the sensor canopy over the content, then backfollow the
  present (3197–3229). The two "err by overlapping rather than gapping", and
  the trail deduplicates (11212).
* Results are written into a **trail edition** at fresh ids (11209–11226). A
  `RecorderFossil` keeps the query durable, with its login authority, and goes
  extinct when its trail is released (10546–10646).
* The **Agenda** is persistent, crash-resumable background work: an
  `AgendaItem` is stepped until done, even across a crash, outside any
  transaction (336–339). Canopy propagation, recorder triggers and hash-table
  doubling all run on it (505–585, 1090–1130). It exists because those jobs
  are unbounded (7513).

### 1.6 Persistence

* An `Abraham` is a persistent object whose identity is itself and whose hash
  is a sequence number (6–10, 120, 202–211). **Storage is identity-addressed
  and updated in place**: `diskUpdate` marks an object dirty and the packer
  rewrites it where it lives (76, 16911–16915). `contentsHash` exists but
  addressing never uses it (116).
* Objects are written in **flocks** (an Abraham plus what it owns) packed into
  fixed-size **snarfs** (51541–51551). A nested Abraham is written as a
  reference; reading one returns a stub that becomes real when touched, and a
  clean object can be turned back into a stub (17050–17128, 17297–17330).
* The **Turtle** is the fixed root flock; its boot object is the `BeGrandMap`,
  set once (16957–16987, 11476–11482). The grand map registers range elements
  by id in both directions and owns the Ent (1468–1493, 1681).
* `GrandHashTable` is an extensible on-disk hash collection, doubled in the
  background through the Agenda (48155–48369, 505–585). It is a container used
  inside the grand map, not the Ent's store.
* `consistent:` blocks are pseudo-transactions with no rollback, written lazily
  and in batches (16487–16516, 17195–17290). There is no tracing GC: lifetime
  is explicit `destroy` plus reference flags (17151–17154, 17242–17251).

---

## 2. Component map

### Version history

| Gold mechanism | grmpl | Class. | Note |
|---|---|---|---|
| One Ent per grand map | one `Family`/`EntRoot` per world | faithful | |
| `fulltrace` DagWood | `Dag` of branches, persisted | faithful | |
| `BoundedTrace(branch, position)`, `isLE:` | `(BranchId, edition)`, `is_ancestor` | faithful (tree case) | no ancestry cache |
| Implicit branching at a non-tip successor | explicit `fork_at` | extrapolated | |
| **Merges** (`DagBranch`, `newSuccessorAfter:`) | none | **missing** | Gold uses them for every `combine:` |
| A new trace per derived edition (copy, transform, combine) | a new edition per commit; only grafts are recorded as derivations | divergent | |
| `oroots` (trace → orgl root) | branch enfilade (branch → state) | divergent | Gold's is vestigial |

### Content trees

| Gold mechanism | grmpl | Class. | Note |
|---|---|---|---|
| Binary `SplitLoaf` on a distinction (k-d-like) | B+ tree ordered by whole key, B = 64 | **divergent** | grmpl cannot split on an arbitrary dimension; this is why its extents prune only along the sort order (step 2) |
| Splay, mutating shared nodes in place | persistent, balanced, path-copied | divergent | grmpl's is better-defined |
| `DspLoaf` node | `dsp` on every handle | extrapolated | same power |
| `transformedBy:` `O(1)`, composing | `relocate` | faithful | |
| Dsp pushed down on restructure | `open` | faithful | |
| Query moved down by `inverseOf:` | stored keys moved up (`cmp_displaced`) | divergent | Gold's dsps are bijections; grmpl's ids can wrap |
| Per-dimension dsps (`GenericCrossDsp`) | one shift for every entity cell | extrapolated (narrower) | |
| No cached summaries; prune on splits and `limitRegion` | cached `Count` and `Extent` | **grmpl only** | grmpl's extents are its own, not Gold's wids |
| `copy:` by splay | `split` | extrapolated | |
| `combine:` (disjoint, may interleave) | `join` (no interleaving) | divergent (narrower) | |
| copy + transform + combine | `graft` | extrapolated | |
| `RegionLoaf` (one element over a region) | none | **missing** | run-length content |
| `OVirtualLoaf` (lazy array) | none | **missing** | |
| `OPartialLoaf` (placeholder) | none | **missing** | |

### History, compare, backfollow

| Gold mechanism | grmpl | Class. | Note |
|---|---|---|---|
| H-crum per node, O-parent sets | none: nodes have no upward links | **missing** | |
| Bottom crum: root → editions using it | forward map only (branch → roots) | divergent | |
| Upward sets as reference counts | mark-and-sweep from the root record | divergent | |
| Backfollow: editions containing content, transitive, across all versions | spanfilade: one hop, grafts only, per branch | **missing** (partial analog) | |
| `sharedRegion`/`mapSharedTo`: identity compare across positions and editions | `Tree::diff` (one relation, one branch, same position); `compare_spans` (grafts only) | divergent / partial | |
| Content identity = range elements | identity = tuple value | **divergent** | the relational model compares by value |
| Trace pruning (`inTrace:`, `isLE:`) | spanfilade edition bound, `as_of` | extrapolated | grmpl can restrict provenance "as of"; Gold's backfollow cannot |

### Canopies, recorders, Agenda

| Gold mechanism | grmpl | Class. | Note |
|---|---|---|---|
| Canopy as a shared summary tree hung over content and history | `Canopy`: an index of watcher intervals | **divergent** | same name, different structure |
| OR-ed flag word | `Reach.endorse`, OR-ed | faithful (algebra) | flags sit on watchers, not content; `route_endorsed` has no caller outside tests |
| **Bert canopy** (permissions, endorsements over history) | none | **missing** | |
| **Sensor canopy** + recorder hoisting | none | **missing** | |
| Incremental prop propagation, stopping early | measure recomputed on the path copy | extrapolated | synchronous |
| Standing transcluder/work queries (recorders) | watches are relational view deltas; `copies_of` on demand | divergent / missing | no standing "who copies this" |
| Durable query (`RecorderFossil`) | canopy and interests persisted per branch | extrapolated | no extinction lifecycle |
| Trail edition with dedup | inbox relation + delivery cursor | extrapolated | |
| "Overlap, never gap" between past and future | registration edition widening | extrapolated | |
| **Agenda** (persistent incremental background work) | none: all work is synchronous at commit | **missing** | |

### Persistence

| Gold mechanism | grmpl | Class. | Note |
|---|---|---|---|
| Identity-addressed objects updated in place | content-addressed immutable nodes (SHA-256) | **divergent** | deliberate, and better-defined |
| Flock as the unit of storage | node frame | extrapolated | |
| Stub / become real | paged handles | faithful (shape) | grmpl stubs also answer counts and measures |
| Stub again when clean (`purgeClean`) | none: a paged node never unloads | **missing** | |
| Snarfs, forwarding | fjall keyspace | divergent (delegated) | |
| Turtle (fixed root) | root record | extrapolated | rewritten every commit |
| `BeGrandMap` (id ↔ range element) | catalog + branch enfilade | missing (analogous) | no range-element ids |
| `GrandHashTable` | none | missing | a collection, not the store |
| `consistent:` pseudo-transactions, lazy durability | durable atomic edition per commit, group commit | divergent | grmpl's is stronger |
| Explicit destroy + reference flags | tracing GC | divergent | |

### Not in Gold at all (grmpl's extrapolations)

* Cached `Count` and `Extent` measures, and searches driven by them.
* Version compare that costs the edit (`Tree::diff` by position and value).
* Context enfilades (`context`/`inherit`) and derived enfilades
  (`materialized view`). These come from `idea.md`, not Gold.
* The spanfilade, from Udanax Green rather than Gold.
* Arrangements, group commit, as-of reads of provenance.

---

## 3. Corrections to `ENT-AND-XANADU.md`

* "Loaf/Crum families — the nodes of a measured tree (a B-tree-like
  structure)": the O-tree is a **binary splay tree**, split on distinctions.
* "every node advertises the range of addresses its subtree covers": O-tree
  nodes cache **no** summary; the upward summaries are canopy crums.
* "a dsp on every pointer, as on `DspLoaf`": Gold puts the dsp in **a separate
  node**. grmpl's per-handle dsp is an equivalent extrapolation.
* Calling the branch enfilade Gold's `oroots`: `oroots` is **vestigial** in
  Gold.
* "`GrandNode`/`GrandHashTable` — the granfilade": Gold has no granfilade; the
  grand hash table is a **collection** used by the grand map. A
  content-addressed store is grmpl's, not Gold's.
* Mapping `CanopyCrum` to "upward interest": Gold's canopy is a shared
  **summary tree of permission and endorsement flags** with two halves, bert
  and sensor; grmpl's canopy is an interval index of watchers.
* Calling `Extent` "Gold's wid": the extent is grmpl's own.

---

## 4. The fidelity gaps the source supports

Gold mechanisms grmpl lacks, from the most central:

1. **Backfollow through history** (the H-tree): upward links from content to
   everything that contains it, so "which versions and editions hold this" is
   answered transitively across the whole Ent. This is the mechanism Gold's
   compare, recorders and canopies are built around.
2. **Identity-based version compare** (`sharedRegion`, `mapSharedTo`): what
   content two editions share, wherever it sits.
3. **Trace merges, and a trace per derived edition**: the version order as a
   DAG in which copying, transforming and combining are all versions.
4. **The canopies**: the bert canopy pruning backfollow by permission and
   endorsement, and the sensor canopy pruning standing-query checks.
5. **Recorders**: standing backfollow queries, past then future, delivered
   into a trail.
6. **The Agenda**: persistent, crash-resumable background work for everything
   unbounded.
7. **Multi-dimensional splits**: a content tree that partitions on any
   dimension, k-d style.
8. **Lazy and run-length leaves**: region, virtual and partial loaves.
9. **Per-dimension dsps.**
10. **Unloading clean nodes** back to stubs.

## 5. Deliberate divergences to confirm

Two of grmpl's divergences are design choices that shape how the gaps above
could be closed:

* **Content-addressed immutable nodes** instead of identity-addressed objects
  mutated in place. They give atomic editions, structural sharing for free and
  a well-defined crash story. Gold's splay mutates shared nodes, which only
  works with identity addressing.
* **Value identity** instead of range-element identity. Relations compare
  tuples by value, so two equal facts are the same fact. Gold's backfollow and
  compare follow identity: the same content, not equal content. In grmpl,
  provenance can only come from recorded copy operations, as the spanfilade
  records grafts.
