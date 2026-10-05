# What the Ent is good at, and what it is not

Measured on the ent-native substrate after the gap work of plan v5
([`archive/ENT-GAPS-PLAN.md`](archive/ENT-GAPS-PLAN.md)), with `grmpl-store` deleted — the Ent is
now the only substrate, so these numbers are the system's numbers, not one leg's.

> **Method.** `cargo run -p grmpl-bench --release --bin entbench` measures the
> *substrate's shape*; `--bin grmpl-bench` runs the P13 axes, which measure the
> engine's *semantic* costs (churn, watch fan-out, preconditions, contention,
> arrangement sharing). One warmed wall-clock run per size, no statistical
> machinery: the signals here are orders of magnitude, not percent.
>
> **Environment.** 4-core Intel Xeon @ 2.80 GHz, 15 GB RAM, `rustc 1.94.1`,
> release profile, `fjall` on a container filesystem. Every commit issues a real
> `SyncAll`.
>
> **Caveat worth stating up front.** These are single-run figures on a shared
> virtual machine. Ratios within an axis are trustworthy — they span 10²–10⁴.
> Absolute wall-clock numbers, especially the fsync-bound ones, are not portable
> to other hardware.

---

## 1. The short version

The Ent buys you **cheap history, cheap copies, and sublinear questions**. It
pays for that with **a fixed per-commit durability cost and a few directory
frames per commit**. A full scan costs about twice a flat array — a constant,
not an asymptotic loss.

> Rows marked † were re-measured after format v6 (one root record, every
> structure a tree beneath it, nodes paged on demand) on an Apple-silicon laptop,
> where an fsync costs ~4 ms rather than ~1 ms; frame and node counts are
> hardware-independent. §6 has the before-and-after on that machine.

| The Ent is good at | Measured |
|---|---|
| Forking a whole world † | **2 node frames**, one fsync, flat from 1k to 100k rows |
| Instancing a template (DSP virtual copy) † | **14–21 node frames**, flat from 1k to 100k facts — a row-by-row copy writes 73–6,463 |
| Opening a large world † | **2 frames read** at any size; a 10-row read then pages in 4–6 |
| Answering "how many" over a span | **1.3 µs** at 100k rows — **2,050×** cheaper than the scan |
| Reading a key span instead of a relation | **24.5 µs** for 1% of 100k — **109×** cheaper than the scan |
| Proving a watcher is unaffected | **162 ns**, vs a ≥2.7 ms re-evaluation |
| Reading the deep past | **89 ns** at edition 1 of 10,000 |
| Commit work independent of relation size † | 7.8 → 11.8 frames as rows go 1k → 100k |
| Searching an entity column that tracks the key (v7) | **0.9–2.1 µs** warm, 6–11 frames cold — a scan is 2.2 ms at 100k rows |

| The Ent is not good at | Measured |
|---|---|
| Single-row commit latency | **~1 ms**, fsync-bound — ~1,000 commits/s |
| Reading a whole relation | **1.9× a flat `Vec` clone** — 27 ns/row vs 14 ns/row |
| Opening fjall itself † | grows with the data — 5 ms at 1k rows, 13 ms at 100k; the Ent adds nothing to it |
| Unconsolidated history † | ~8 nodes per commit; 1,000 commits → 7,896 nodes |
| Consolidation itself | **89 ms** to fold and collect 5,000 editions |
| Searching a scattered entity column (v7) | every leaf: **3,228 frames** cold at 100k rows, 181 µs warm — 17× an Arrangement |

---

## 2. Where the shape pays off

### Fork is genuinely free

```
fork whole world      1,000 rows    4,836,500 ns    2 node frames written   †
fork whole world     10,000 rows    4,895,250 ns    2 node frames written   †
fork whole world    100,000 rows    4,337,875 ns    2 node frames written   †
```

Flat, and **two frames written** at every size: the new leaves of the branch DAG
and of the branch enfilade, which link to the parent's Rel enfilade unchanged.
Every relation's nodes are shared. The time is one `SyncAll` (~4 ms on this
machine); before v6 a fork wrote no nodes but issued three syncs (flush, roots,
branch graph) and took 12 ms here.

This is the single clearest case for the whole design. A copying store makes
forking a 100k-row world proportional to 100k rows; here it is proportional to
nothing. (The fork cuts each relation's version directory and log at the fork
edition with a persistent split, which shares the trees outright when the fork is
at the tip.)

### Instancing is a virtual copy

```
instance_template (graft)      1,000 facts     4,789,958 ns    14 node frames written
copy by commit (row by row)    1,000 facts     8,202,125 ns    73 node frames written
instance_template (graft)     10,000 facts     4,928,750 ns    15 node frames written
copy by commit (row by row)   10,000 facts    31,833,042 ns   653 node frames written
instance_template (graft)    100,000 facts     6,145,583 ns    21 node frames written
copy by commit (row by row)  100,000 facts   550,257,042 ns  6,463 node frames written
```

A template block in the middle of a relation is copied to a fresh block. The
graft splits the block out, relocates it by one dsp, and joins it back in, so
the instance shares every interior node with the template: the frames written
grow with the tree's depth, and the time is the commit's `SyncAll`. Committing
the same rows as copies grows linearly — about one frame per 15 facts and
~5 µs per fact at 100k. Both sides now also write the three directory frames
every v6 commit writes. (Measured later than the rest of this report, on an
Apple-silicon laptop; the frame counts are hardware-independent.)

### Measures answer without materializing

```
                        1k rows      10k rows     100k rows
count_at (measure)         313 ns        1,353 ns      1,305 ns
read_range (1% span)       855 ns        2,861 ns     24,533 ns
read_at (whole relation) 22,021 ns     241,443 ns  2,678,357 ns
```

`count_at` is flat — 313 ns to 1.3 µs while the data grows 100× — because it
folds cached subtree summaries and never builds a row. At 100k rows it is
**2,052× cheaper** than reading the relation to count it.

`read_range` costs the *result*: 1% of the relation costs about 1% of the scan
(24.5 µs vs 2.68 ms, **109×**). This is what makes an entity-keyed view in the
MOO cheap — the E2b pushdown turns it into exactly this call.

### History is not a tax on the present

```
read_at, newest edition   10,000 editions   551,429 ns   10,000 rows
read_at, oldest edition   10,000 editions        89 ns        1 row
```

Reading edition 1 out of 10,000 costs 89 ns. The cost is the *state at that
edition*, not the distance back to it: as-of is a descent of the Version enfilade
for the root in force, then a walk of that root. Nothing is replayed and nothing
is undone. On an append-log store, the second line is where you would pay for the
first 9,999 editions.

### Routing beats evaluating

```
touched_since (proves quiet)   100,000 rows   162 ns
```

162 ns to prove a watcher cannot have been affected, against a re-evaluation that
would have cost at least the 2.68 ms scan. The pump asks this before doing any
differential work, so an idle watcher on a busy world is ~16,000× cheaper than it
was. Two watchers on *disjoint key ranges of one relation* are separated too, via
the canopy.

### Commit work is flat in relation size

```
single-row commit   1,000 rows     906,323 ns   7.8 frames/commit   (frames †)
single-row commit  10,000 rows     993,975 ns   9.8 frames/commit
single-row commit 100,000 rows   1,146,128 ns  11.8 frames/commit
```

100× the rows costs 4 extra node frames — one per extra level of the Fact tree
and the edition log. The wall-clock rise is the fsync moving more bytes, not
more algorithmic work. The frames are v6 counts, about 3.7 more than v5 at
every size: the directories above the edited trees (version enfilade, Rel
enfilade, branch enfilade) are now copied on the path and written too.

---

## 3. Where it costs

### Every commit fsyncs — that is the throughput ceiling

~1 ms per single-row commit is **not** tree work; it is `SyncAll`. The P13 churn
axis shows what batching recovers:

```
raw commit     1 fact/patch       922 facts/s
raw commit    16 facts/patch   12,774 facts/s
raw commit   256 facts/patch   62,071 facts/s
```

**67× the throughput from batching 256 facts per commit.** The Patch–edition law
requires one atomic durable write per edition; it says nothing about how many
facts an edition carries. A workload that commits row-at-a-time is paying for
durability, not for the Ent. This is the single most important tuning knob in the
system.

### A full scan costs about twice a flat array — but only about twice

```
                      100,000 rows
read_at (enfilade)     2,678,357 ns    27 ns/row
clone a flat Vec       1,407,148 ns    14 ns/row   — tree is 1.9x
```

The penalty is a stable **1.8–1.9×** at every size, not an order of magnitude:
the walk is in-order over wide leaves, so it is mostly the same memcpy the `Vec`
does plus node-chasing between leaves. Worth knowing in both directions — the
Ent's read path is built to *avoid* full scans (range, measure, routing), but
when you do want every row it is not a disaster, just a constant.

### Opening a store is fjall's cost now, not the Ent's

```
open fjall alone      100,000 rows   13,244,333 ns                     †
open EntStore         100,000 rows   11,282,375 ns   2 frames paged
first 10-row read     100,000 rows       20,875 ns   6 frames paged
```

Before v6, open read the *entire* tree back eagerly, one KV `get` per node: 40 ms
for 100k rows on the original machine, 22 ms on this one. Now it reads the root
record and two frames, and everything else pages in as reads reach it. What
remains is fjall opening its own database, which grows with the data (5 ms at
1k rows, 13 ms at 100k) and is the same with or without the Ent on top.

### Unconsolidated history accumulates

```
1,000 single-row commits →  7,896 nodes   (~7.9 nodes/commit)   †
5,000 single-row commits → 49,040 nodes   (~9.8 nodes/commit)
```

Every commit path-copies, and every copied node is retained until GC. The cost is
bounded and predictable — a few nodes per commit — but it is not free, and it
grows with tree depth. Since v6 it is about twice what it was (3,930 and 26,026
nodes), because the directories are trees on disk and each edition copies their
spines too. Consolidation reclaims essentially all of it:

```
consolidate + gc   1,000 editions    17.0 ms    7,896 → 36 nodes   (99.5% collected)   †
consolidate + gc   5,000 editions    54.4 ms   49,040 → 165 nodes  (99.7% collected)
```

but it is a stop-the-world sweep that holds the commit lock, and it is `O(stored
nodes)`. On a busy world it wants to be scheduled, not called inline.

### (Not a cost any more: preconditions)

The P13 axis is still titled *"holds_at scans the whole relation tail"*, which was
true of the LSM. It is not true now:

```
commit     hist=100     1,016,951 ns/probe
commit_if  hist=1,000   1,037,423 ns/probe
commit_if  hist=10,000  1,101,530 ns/probe
```

Flat across 100× the history — an optimistic precondition is an `O(log n)` point
`get` on the Fact enfilade, and what is left is the same ~1 ms fsync every commit
pays. The scenario's title is stale and should be corrected; the number is not.

### Watch fan-out is linear, and shared arrangements are the fix

```
deltastream   1 watcher      676 µs/watcher
deltastream 256 watchers     548 µs/watcher      (140 ms total)
```

Independent streams cost per watcher — 256 watchers is 256 evaluations. Per-watcher
cost is *flat*, so nothing degrades, but nothing is shared either. The arrangement
memo is what collapses it when the watchers read the same sub-query:

```
unshared  k=32    3,295 µs    32 base reads
shared    k=32    1,376 µs     1 base read     — 2.4x, 32x fewer reads
```

One base read instead of 32, for a 2.4× wall-clock win — the gap between those two
numbers is how much of the work is *not* the base read. Routing (above) is the
cheaper lever when watchers are idle; sharing is the lever when they are not.

### Contention resolves correctly but unfairly

```
race x1 threads   1,012 commits/s   0 rejects   fair(min/max)=1.000
race x8 threads     935 commits/s   8 rejects   fair(min/max)=0.000
```

Throughput barely moves under 8-way contention and the retry rate stays at 0.4% —
the optimistic protocol is doing its job. But `fair(min/max)=0.000` means at least
one thread committed nothing: the winner-takes-most pattern of an unfair retry
loop. Correct (exactly one winner per contested edition, which is the law) but not
starvation-free. If fairness matters, that needs backoff the protocol does not
currently have.

---

## 4. One bug this found

Benchmarking is the reason this section exists. Commit cost was measured against
accumulated history and came back linear:

| history depth | before | after |
|---|---|---|
| 0 | 774 µs | 759 µs |
| 500 | 2,237 µs | 953 µs |
| 1,000 | 3,767 µs | 992 µs |
| 2,000 | 7,713 µs | 1,002 µs |
| 4,000 | **20,340 µs** | **1,120 µs** |

`persist` was rewriting a `fact:` meta key for *every live version* of every
touched relation on every commit — `O(live editions)` writes per commit, so N
commits into an unconsolidated world was `O(N²)`. A world left unconsolidated for
4,000 editions had commits 26× slower than a fresh one, degrading without bound.

The fix follows from a property the tree already had: **an older version's root is
immutable**. A commit inserts a new root beside it and never edits it, so only
the new root needs writing. Consolidation is the sole occasion that replaces
existing roots, so it keeps the full sweep.

26× degradation across the range became 1.5×, and the residual is page growth
rather than algorithmic. Commit into a 100k-row relation went 3.68 ms → 1.15 ms;
consolidate+gc over 5,000 editions went 359 ms → 89 ms.

No test caught this, because every law in the suite is a *correctness* law and
this was never incorrect. It is a good argument for keeping the benchmark axes
close to the substrate's claims.

---

## 5. What the numbers say about the design

The Ent is a **read-and-branch-optimised** substrate with a **fixed durability
floor**. It is the right shape when:

- worlds are forked, snapshotted, and rewound, or sub-worlds are instanced from
  templates — those are free or `O(log n)` here and proportional elsewhere;
- reads are *questions about spans* ("how many things in this room", "what is in
  this key range") rather than full-relation sweeps;
- many observers watch a large world and most changes concern few of them;
- the past is read as often as the present.

It is the wrong shape when:

- the workload is row-at-a-time commits and cannot batch — you will spend your
  time in `fsync`, not in the tree;
- the dominant read is "give me every row" — a flat array wins, though only by
  ~1.9×, so this is a reason to prefer something else, not a reason to avoid this;
- history is kept unconsolidated for a long time — every edition retains a
  few directory nodes as well as its data path, ~8 nodes per single-row commit.

### Follow-ups, in the order the numbers justify them

1. **Group commit.** Every axis in this report bottoms out at the same ~1 ms
   `SyncAll`: churn, fork, contention, even the precondition axis. Amortising one
   fsync across concurrent committers is the largest single win available and does
   not weaken the Patch–edition law — the law demands one atomic durable write per
   edition, not one per committer.
2. ~~**Lazy node paging.**~~ Done in v6: open reads two frames, and only the
   nodes a read reaches are resident.
3. **Background consolidation.** Moves the 89 ms sweep off the commit path.
4. **Retry backoff**, if fairness under contention ever matters — the protocol is
   correct but currently starves losers.

Two things the report deliberately does not claim. The scan penalty is a measured
1.9×, not the order of magnitude an earlier draft of this document asserted before
it was measured. And the P13 precondition axis is still *titled* as though
`holds_at` scans the relation tail; it does not, and the title is stale — the
numbers there are flat in history and should be read as an fsync measurement.

---

## 6. Format v6: the whole world in the Ent, before and after

v6 moved every directory into the granfilade under one root record and made
nodes page in on demand (`docs/ENT-AND-XANADU.md` §3). Both builds were run on
the same Apple-silicon laptop, one after the other (`entbench`, release; an
fsync costs ~4 ms here):

| Axis | v5 | v6 |
|---|---|---|
| Fork, 100k rows | 0 frames, 3 syncs, 12.4 ms | **2 frames, 1 sync, 4.3 ms** |
| Open, 100k rows | 21.9 ms, every node read | **11.3 ms, 2 frames read** (fjall alone: 13.2 ms) |
| First 10-row read after open, 100k rows | — (already resident) | 21 µs, 6 frames paged |
| Single-row commit, frames | 4.1 / 6.1 / 8.1 | 7.8 / 9.8 / 11.8 (1k / 10k / 100k rows) |
| Single-row commit, time, 100k rows | 4.8 ms | 4.9 ms |
| Instance a 100k-fact template | 18 frames, 5.0 ms | 21 frames, 6.1 ms |
| 1,000 unconsolidated commits | 3,930 nodes | 7,896 nodes |
| Consolidate + GC, 5,000 editions | 38 ms | 54 ms |
| `read_at` / `read_range` / `count_at`, 100k rows | 760 µs / 5.6 µs / 648 ns | 734 µs / 5.7 µs / 626 ns |

What it bought: open no longer reads the world, and the canopy, the branch DAG
and every directory survive a reopen as the same structures they were. What it
cost: each commit copies and writes the directory spines above the edited trees
— about 3.7 more frames — so unconsolidated history takes twice the nodes, and
consolidation has twice as many to sweep. Commit latency, which is the fsync, did
not move. Reads of resident data are unchanged within this run's noise.

---

## 7. Format v7: extents and the spanfilade

v7 gave every Fact tree node an `Extent` — per column, the bounding box of its
entity cells, Gold's wid — and gave each branch a spanfilade recording every
graft from both ends (`docs/ENT-AND-XANADU.md` §3). Same laptop as §6, release
builds of v6 and v7 run alternately, twice each.

### What the wid buys, and where it buys nothing

The extent and an Arrangement are two answers to one question: which facts have
an entity in a given span in a column the tree is not ordered by. The
Arrangement is the D4M answer, a second copy of the facts rotated so the column
leads. The extent is Gold's, a summary on the nodes already there. The axis
searches `exits(from, way, to)` for exits into a 10-room span, four exits per
room. In `near`, an exit leads to an adjacent room, so the destination column
tracks the key. In `far`, it leads anywhere.

| 100k rows | `near` | `far` |
|---|---|---|
| `search_at`, cold (frames paged) | 139 µs (**11 frames**) | 8.9 ms (**3,228 frames**, every leaf) |
| `search_at`, warm | **2.1 µs** | 181 µs |
| Arrangement, warm | 3.3 µs | 10.8 µs |
| Arrangement, first call (builds it) | 181 ms | 251 ms |
| `read_at` + filter | 2.2 ms | 1.5 ms |

At 1k and 10k rows the pattern is the same: `near` searches page in 6 and 9
frames and take about 1 µs warm, while `far` pages every leaf.

The wid is only as good as the locality of what it bounds. When a column tracks
the key — as an instanced template's exits do, since its rooms sit in one
block — the search beats the Arrangement and needs no second copy, no build and
no second write per commit. When a column is scattered, every leaf's box spans
the world: the search reads the whole relation, cold from disk if it must, and
the Arrangement is 17× faster warm. Neither is free: the Arrangement's build
cost 181–251 ms here, and it doubles every later commit's work on that relation.
`read_range_on` keeps using the Arrangement at the current edition and uses the
extent below it, where there is no Arrangement; `search_at` is the extent alone.

### What the extent costs

| | v6 | v7 |
|---|---|---|
| Exits commit, frame bytes (1k / 10k / 100k rows) | 13.3 / 18.9 / 23.0 KB | 16.4 / 23.3 / 28.5 KB (**+23–24%**) |
| Exits load, frame bytes per row | 101 / 114 / 122 B | 102 / 118 / 128 B |
| Exits commit, frames | 7.8 / 9.8 / 12.1 | unchanged |
| Exits commit, fsync'd | 4.3–5.0 ms | 4.7–5.0 ms (the fsync) |
| Exits commit, in memory | 3.0–4.1 µs | 3.5–5.0 µs |
| `count_at`, 100k rows | 635 ns | 674 ns |

Every internal frame records each child's measure, and the extent is 17 bytes a
column per child, so commits write about a quarter more bytes. Loading writes
mostly leaves, which carry no measures, so it grows by 1–5%. Commit latency is
still the fsync. In memory, the tree work rises by up to a third at 100k rows;
an early version cost 60% more, until the folds stopped allocating an extent per
entry and cloning measures that no dsp moved. `count_at` first regressed 5× by
folding the extents along the boundary spines just to read the count; it now
reads the cached sizes and is back where it was. Reads, fork, instancing,
consolidation and open did not move beyond run-to-run noise.

### The spanfilade

The spanfilade holds one entry per graft, written twice, so its cost is a few
frames per instancing (21 → 23 frames for a 100k-fact template) and nothing
otherwise. A fork into the past rebuilds it in `O(grafts)`, because it is keyed
by span rather than edition.


---

## 8. Step 3: derived state and scopes in the Ent

`viewbench` (release, same laptop) builds a moo-shaped world of N things, four
to a room, each named, on a durable store. "`world`" is `located ⋈ named` on the
thing; "`here(viewer)`" pairs the viewer with everything in its room.

### Maintenance: what a view's delta costs after a one-row move

| N things | bare join, keyed lookup | bare join, snapshot difference | view (`distinct` over the join) | materialized view |
|---|---|---|---|---|
| 1k | 4.7 µs | 1.1 ms | 1.4 ms | **3.5 µs** |
| 10k | 8.3 µs | 10.6 ms | 14.4 ms | **7.2 µs** |
| 100k | 12.6 µs | 208 ms | 272 ms | **12.0 µs** |

Keyed lookup makes a bare join's delta cost the change: four orders of magnitude
at 100k. But a compiled view ends in `distinct`, and `distinct` over a join has
no delta rule that reads only the change. It recomputes both ends, so the plain
view gains nothing from the cheaper join beneath it. That is the case for
derived state. The materialized view stores the join's rows with their
derivation counts, so its delta is a `compare` of the stored copy between two
editions, which costs the edit.

### Reading `here(viewer)`, and keeping the copy current

| N things | evaluated | materialized, current | materialized, stale | refresh after a move | first incremental refresh | first refresh (whole) |
|---|---|---|---|---|---|---|
| 1k | 354 µs | **3.0 µs** | 332 µs | 9.2 ms | 9.5 ms | 50 ms (5k rows) |
| 10k | 3.0 ms | **3.8 µs** | 3.0 ms | 9.3 ms | 23 ms | 341 ms (50k rows) |
| 100k | 47 ms | **4.7 µs** | 47 ms | 9.7 ms | 361 ms | 5.3 s (500k rows) |

* A current copy is a range read: flat in the world's size, 10,000× cheaper
  than evaluating at 100k things.
* A stale copy costs exactly an evaluation. The read proves the copy current
  before using it, and falls back when it cannot.
* A steady refresh is two fsync'd commits (one per view that changed), flat in
  the world's size. The first incremental refresh builds the Arrangement that
  `here`'s self-join probes (`located` by room), once.
* Materializing whole is expensive and grows with the open form: `here` pairs
  every two things in a room, so 100k things store 400k rows for it. The cost
  of a materialized view is its open form's size, not the view's.

### Scopes: which spans contain an entity

A scope relation of N disjoint spans, on a reopened store:

| N scopes | cold | warm |
|---|---|---|
| 1k | 4 frames paged, 206 µs | 0.66 µs |
| 10k | 5 frames paged, 32 µs | 0.62 µs |
| 100k | 7 frames paged, 97 µs | 0.63 µs |

The extents' bounds on the `first` and `last` columns prune like an interval
tree's, so a stab reads one path. An `inherit` over a view's rows is linear in
those rows while the scopes are unchanged. A change to a scope recomputes both
ends, because one binding can change any entity's answer.

## 9. Version compare across a graft

`Tree::diff` used to pair two nodes' children only when their separators were
identical, and to merge the whole subtree pair entry by entry otherwise. It now
walks both versions as frontiers of whole subtrees and skips any node both hold
at the same position, however the spines above it were rebuilt.
`EntStore::compare_spans` also names each graft by its span, from the
spanfilade (`ENT-FIDELITY-GAPS.md`, closed gaps).

### Frames paged, cold

A relation of 100k rows thinned by a seventh (so its separators are stale), on
a reopened store, before and after each edit:

| edit compared | old `diff` | new `diff` | `compare_spans` |
|---|---|---|---|
| graft of a 5,000-row block | 4,688 | **164** | **20** |
| one row inserted | 8 | 8 | — |
| forty scattered rows | 162 | 162 | — |
| 3,000 rows inserted densely (splits nodes) | 4,942 | **228** | — |
| ~14k rows refilled (output-bound) | 4,513 | 4,491 | — |

* Across a graft, the old descent merged from the root, because the graft's
  join rebuilds the spine there, and so read the whole relation. The new one
  reads the copy's leaves and the seams; `compare_spans` reads only the seams,
  however large the copy.
* A dense insert splits nodes, which changes separators high in the tree; the
  old descent merged everything below the first changed separator.
* Small edits cost what they did: one path per edited row in each version.

### In memory, one row changed

| rows | insert: old | insert: new | removal: old | removal: new |
|---|---|---|---|---|
| 1k | 0.77 µs | 0.78 µs | 15 µs | **1.0 µs** |
| 10k | 0.77 µs | 0.81 µs | 145 µs | **1.5 µs** |
| 100k | 0.86 µs | 0.96 µs | 1.68 ms | **2.2 µs** |

A removal that underflows a leaf fuses it with a sibling, and in a tree built
by sequential inserts every parent is at its floor, so the fuse cascades to the
root. Every separator on the way changed, so the old descent merged the whole
relation. Inserts are within 10%: the frontier pushes each opened node's
children where the old descent zipped them in place.

The warm compare of a materialized view's copy (§8), over repeated
`viewbench` runs, was 3.1–3.2 / 4.8–8.6 / 8.7–12.2 µs before and 3.5–4.3 /
6.5–8.0 / 11.3–13.4 µs after, at 1k / 10k / 100k things. The ranges overlap at
10k and 100k; at 1k the new compare is about half a microsecond slower.

## 10. The history index (Ent-fidelity step 4)

A durable 100k-row relation with a 1,000-row template block, ten instances
grafted from it, then one-row commits. Cold numbers are on a freshly reopened
store; release build.

### Indexing

| one-row commits | time per commit | edges per commit | nodes per commit |
|---|---|---|---|
| 200 | ~200 µs | 82 | 4 |
| 2,000 | ~220 µs | 91 | 4 |

Commits themselves are unchanged: indexing is deferred and runs on a query or
in `step_history`. Each new interior node adds an edge per child, so a commit's
edges are about its new spine times the fan-out.

### Queries

| query | cold cost | answer |
|---|---|---|
| backfollow, the 1,000-row template block (200 commits) | 399 frames, 13 ms | 2,266 holdings: every version since each graft, 11 shifts |
| backfollow, one row (200 commits) | 165 frames, 0.5 ms | 221 holdings |
| `copies_of` on the spanfilade, for comparison | 1 frame, 4 µs | 10 grafts (one hop, grafts only) |

### Identity compare: Gold's upward method against a downward walk

`shared_region` between the template's edition and the latest, both ways:

| | 200 commits | 2,000 commits |
|---|---|---|
| upward through the history index (`shared_region`) | 532–630 frames, ~2 ms | 3,195–4,373 frames, ~23 ms |
| downward walk (`shared_region_by_descent`) | 119–124 frames, ~0.9 ms | 120–167 frames, ~1.2 ms |

*Revised in step 6:* both methods now answer completely, as Gold's
`mapSharedTo` does, reporting content at every shift the other version holds
it rather than stopping at the first shared node (`entbench identity`, which
reproduces this world):

| complete (step 6) | 200 commits | 2,000 commits |
|---|---|---|
| upward | 733–750 frames, ~4.5 ms | 3,684–4,086 frames, ~17–19 ms |
| downward | 219 frames, ~1.5 ms | 221 frames, ~1.5 ms |

Completeness costs a visit to every leaf of the first version instead of
stopping high: about 100 more frames either way here. The upward climb from an
old node passes through every later version that
shares it, so its cost grows with history. Born-pruning (Gold's `isLE:`)
trims about a quarter in the late-against-early direction and costs a little
in the other. The downward walk reads only interior nodes, because interior
frames name their children's keys, so its cost is the two versions' interior
nodes at any history length.

## 11. Merges (Ent-fidelity step 5)

A durable 100k-row relation forked; the fork commits guarded moves, the parent
commits ten unrelated rows, then the parent merges the fork. Release build.

| merge | time | frames written |
|---|---|---|
| replaying 100 guarded patches | 21 ms | 726 |
| replaying 1,000 guarded patches | 143 ms | 7,169 |
| refused at the last of 100 patches | 1.8 ms | 0 |
| refused at the last of 1,000 patches | 15 ms | 0 |

Each replayed patch becomes its own edition of the merged branch, with its own
version roots, so a merge writes about seven frames per patch, all in one
batch with one `fsync`. A refused merge stops at the conflict and writes
nothing.

## 12. Layouts: B+ against k-d (Ent-fidelity step 6)

`entbench`'s last section: exits `(from, way, to)` with scattered
destinations, four per room, loaded in 1,000-row commits, then reopened so
every read starts cold. Release build, one run each. Frames are node frames
paged in (cold reads) or written (writes).

| at 100k rows | B+ | k-d |
|---|---|---|
| box on the scattered column, cold | 6.4 ms, 3,221 frames | 0.14 ms, 65 frames |
| box on the scattered column, warm | 141 µs | 2.4 µs |
| range on `to` below the present, cold | 6.1 ms, 3,192 frames | 0.17 ms, 77 frames |
| range on `to` at the present, warm | 3.9 µs (first call builds an Arrangement: 252 ms) | 35 µs, nothing built |
| one room's exits (lead column), cold | 31 µs, 7 frames | 0.59 ms, 450 frames |
| one room's exits, warm | 253 ns | 12.8 µs |
| `read_at` the whole relation | 0.89 ms | 8.7 ms |
| single-row commit | 4.8 ms, 28 frames | 4.4 ms, 22 frames |
| graft 1,000 rows | 10.9 ms, 18 frames | 5.0 ms, 24 frames |
| compare across that graft, cold | 42 frames | 52 frames |
| load, per row | 11 µs | 14 µs |

The k-d tree wins by fifty-fold wherever the question is about the scattered
column, and loses by the same order wherever it is about the lead column: a
read on `from` enters both sides of every split on `to`, about the square root
of the leaves. Sorted output costs a sort (ten-fold on `read_at`). Commits and
grafts cost the same, since a commit is bound by its `fsync`. At 10k rows the
shape is the same at a smaller scale (box: 319 frames against 23; lead read: 5
against 94).

## 13. Run leaves (Ent-fidelity step 7)

`entbench runs`: one relation of 100k `(entity, tag)` rows in 1,000-row
commits, B+ layout. *Regular* rows share a tag, so they fold into runs;
*irregular* rows carry a tag no step repeats, so they cannot. Cold reads on a
reopened store; release build, one run each.

| | regular (runs) | irregular (rows) |
|---|---|---|
| nodes stored after consolidation | 5 | 3,229 |
| 1,000-row range, cold | 3 frames, 60 µs | 39 frames, 132 µs |
| `read_at` the whole relation | 5.4 ms | 2.4 ms |
| commit of two rows inside the block | 6 frames written | 11 frames written |
| compare across that commit, cold | 4 frames | 23 frames |
| `backfollow` of a 1,000-row template after a graft | finds 0 rows of the copy | finds 896 rows |

Runs fold the relation into a handful of nodes, so ranges, edits and compares
read almost nothing. Two costs come with them. A full scan computes every row
(each a fresh tuple), about twice the time of copying stored ones. And
identity, which is node sharing, coarsens to the run's leaf: the template and
its copy no longer share a node, so sharing-based provenance finds nothing of
the copy.
