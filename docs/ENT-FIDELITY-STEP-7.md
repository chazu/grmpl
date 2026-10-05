# Ent fidelity, step 7: run-length, lazy and partial leaves

**Status:** landed on `main`, format v10 (a fresh-store cutover: a leaf is
a run of tagged items, and every child and link records its reserved keys);
runs made opt-in per relation in format v11 (§6).
**Question it set out to answer:** Gold's content tree ends in loaves of
several kinds. A `RegionLoaf` maps a whole region to one shared range
element, an `OVirtualLoaf` holds a primitive array and fakes an object per key
only when asked, and an `OPartialLoaf` is a region of placeholders whose
content arrives later (`udanax-top.st` 9285–9307, 9072–9131, 8779–8871).
grmpl's leaves were plain runs of entries. What do Gold's leaf kinds become
in a store whose facts are identified by value, and what do they cost?

This closes gap G9 (see [`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md)).

---

## 1. The choices

* **A run steps every numeric column by its own stride.** Row `i` is the
  first row with each entity or integer column stepped `i` times (a stride of
  `0` holds a column fixed). That covers `kind(e, "room")` over a block, and
  exits whose entity cells all move together, as a displacement moves them.
* **Runs form by themselves.** No API changes.
* **Then, after measuring: runs are opt-in per relation, off by default**
  (§6).
* **A minimal partial leaf**: keys reserved but unfilled.

## 2. What was built

* **Leaves of items** (`tree::leaf`). A leaf holds up to `B` items, each a
  row, a **run** (`n` rows: a first key, a per-column stride and one value),
  or a **hole** (a run of keys holding no rows). A run is Gold's region and
  virtual loaves at once. It is run-length, since one item stands for any
  number of rows. It is lazy, since a row is computed only when a read reaches
  it. Facts are values in grmpl, so a run is purely a representation: it holds
  exactly the rows it computes.
* **Formation.** A row that continues the run before it, or starts the run
  after it, joins it; three rows in a step become a run; a leaf folds its runs
  before it splits; a write inside a run splits it around the row. Keys step
  through `Displace::stride_to` and `step`, so only tuple keys form runs. A
  value type says whether two values may share a run (`RunValue`).
* **Measures of runs** (`Measure::run`). A run's count is its length and its
  extent is the bounds of its first and last rows, both in `O(1)`. So a range
  measure over a million folded rows costs what it costs over one.
* **Reads compute.** Point, range, count, as-of, search and iteration all
  answer from items. A search tests a run's own extent before its rows. The
  B+ diff compares two runs on the same step as one stretch, so an edit inside
  a long run costs the edit.
* **Holes** (`reserve`, `kd_reserve`). A hole is a span of keys, refused if
  any row or hole lies between its first key and its last. It goes in by a
  cut and a join in the B+ layout, and is routed down the splits in the k-d
  layout. `any_in`, and so a graft's target check, sees holes; `get`,
  iteration and counts do not. Writing a row at a hole's key fills it. A node
  caches its reserved keys beside its rows. The store has no writer for holes
  yet: they are a tree-level mechanism with laws, waiting for a consumer.

## 3. What broke, and what it taught

* **Arity counts items now, and both write paths change it.** A remove inside
  a run turns one item into two, so a full leaf can overflow on a *remove*.
  Joining runs turns three items into one, so a leaf can fall below the floor
  on an *insert*. Each B+ write path now repairs both, which the B+ tree never
  needed before.
* **In the k-d layout, a run's key range is not its own.** Leaves are divided
  by column, so another leaf can hold a key between two rows of a run. Two
  merges hit this: a hole placed inside a run's range, and a rebuild that
  brought leaves together. Merges now cut a run around what falls inside it,
  and a rebuilt leaf is made disjoint first (`leaf::disjoint`). Two runs whose
  rows alternate come apart into rows.
* **Runs coarsen identity.** This is the main finding. grmpl's identity is
  node sharing: the history index, `backfollow`, `shared_region` and the diff's
  skipping all find content by the nodes two versions hold in common. A run
  makes one leaf hold many rows. So any edit near it, and any cut a graft makes
  through it, rewrites the node that held them all. A 20,000-row relation of
  regular rows is two runs in one leaf, and after a graft its copy shares no
  node with the template. Gold did not meet this: its identity lives in range
  elements, which a `RegionLoaf` shares however its loaves are rebuilt.
  Compression and provenance-by-sharing pull against each other here.

  Five ent tests depended on sharing at the old grain (history, graft compare,
  join maintenance, the k-d graft-sharing law and a GC test). They now use rows
  no step repeats, since they are about sharing, not runs. One was coarsened
  more subtly. Each room's three exits stepped together, so every room became
  a 3-row run, and a leaf of 64 items then held three blocks instead of one.

## 4. What it costs and buys

From [`PERFORMANCE-ENT.md`](PERFORMANCE-ENT.md) §13, 100k rows, B+ layout:

| | regular (runs) | irregular (rows) |
|---|---|---|
| nodes stored | 5 | 3,229 |
| 1,000-row range, cold | 3 frames | 39 |
| commit inside the block | 6 frames written | 11 |
| compare across it, cold | 4 frames | 23 |
| `read_at` the whole relation | 5.4 ms | 2.4 ms |
| `backfollow` of a grafted template | 0 of 1,000 rows | 896 of 1,000 |

## 5. Laws and mutants

* `tree/leaf_laws.rs`: random histories in both layouts, biased towards
  blocks that fold, mixed with scattered rows, edits inside runs, value
  changes, holes reserved and filled, grafts (runs and holes travel with
  them), cuts and joins. Every read is checked against a model of rows and
  reserved keys through displaced handles, with each layout's invariants after
  every step. Also: a 100k-row block loaded row by row is one item; an edit
  splits it and the row put back rejoins it; runs and holes round-trip
  through the granfilade; a hole refuses what it overlaps and fills key by
  key; interleaving runs come apart in one k-d leaf.
* The conformance suite runs every law of the language on both layouts, now
  with runs forming under them, and `--features grmpl-ent/kd-default` runs the
  ent suite on k-d.
* **18 of 20 mutants are caught.** The two survivors are equivalent. One
  drops the shortcut for writing a run's own value back: the run is cut, then
  rejoined. The other drops the run cutting in `merge`, which `disjoint`
  repeats in the only build that calls it. Five mutants first survived and
  drove new laws: a cut at a run's own last row; a rebuild gathering
  interleaving runs; rows whose changed text cell a careless stride would
  step; two runs from one key on different steps, in a diff; and a remove
  that splits a leaf under a displaced node, which only the first remove on a
  path can meet, since a path copy pushes displacements down.

## 6. Runs became opt-in

The measurements in §4 settled what runs are for. They pay only where rows
form arithmetic patterns (blocks of ids sharing a value, exits whose entity
cells step together), and there they cost provenance, which is the Ent's
point. So a relation's rows fold only if it asks: `set_runs(rel, true)`, or a
branch's `set_default_runs`. The choice is held with the layout as the
relation's **shape** (format v11): fixed once the relation is written,
durable, carried by forks and united by merges. With runs off, a relation is
exactly as it was before this step, provenance included; with runs on over
rows that cannot fold, nothing measurable changes. The conformance suite runs
every law of the language on a third substrate, `ent-runs`, so the mechanism
stays exercised whoever opts in.

## 7. Not done

* **Long runs as their own nodes.** Gold's region loaf is a node, not an item.
  Giving a long run a leaf of its own would recover identity at run grain. It
  needs B+ occupancy rules for a leaf that holds one item and many rows.
  Declined for now: with runs opt-in, its payoff is narrow.
* **Iteration allocates a tuple per computed row.** Stepping from the
  previous row in place would close most of the 2× on full scans.
* **Holes have no store writer.** Reserving keys at the store needs an
  edition, a patch record and a merge rule for reservations.
* **Runs step entity and integer columns only.** Text, floats and nested
  tuples hold fixed.
