# What the Ent is good at, and what it is not

Measured on the ent-native substrate after the v5 gap work
([`ENT-GAPS-PLAN.md`](ENT-GAPS-PLAN.md)), with `grmpl-store` deleted — the Ent is
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
pays for that with **a fixed per-commit durability cost and an eager reopen**. A
full scan costs about twice a flat array — a constant, not an asymptotic loss.

| The Ent is good at | Measured |
|---|---|
| Forking a whole world | **0 node frames**, ~2.4 ms, flat from 1k to 100k rows |
| Answering "how many / how much" over a span | **1.3 µs** at 100k rows — **2,050×** cheaper than the scan |
| Reading a key span instead of a relation | **24.5 µs** for 1% of 100k — **109×** cheaper than the scan |
| Proving a watcher is unaffected | **162 ns**, vs a ≥2.7 ms re-evaluation |
| Reading the deep past | **89 ns** at edition 1 of 10,000 |
| Commit work independent of relation size | 4.1 → 8.1 frames as rows go 1k → 100k |

| The Ent is not good at | Measured |
|---|---|
| Single-row commit latency | **~1 ms**, fsync-bound — ~1,000 commits/s |
| Reading a whole relation | **1.9× a flat `Vec` clone** — 27 ns/row vs 14 ns/row |
| Reopening a large world | **40 ms** at 100k rows — eager, `O(state)` |
| Unconsolidated history | ~4 nodes per commit; 1,000 commits → 3,930 nodes |
| Consolidation itself | **89 ms** to fold and collect 5,000 editions |

---

## 2. Where the shape pays off

### Fork is genuinely free

```
fork whole world      1,000 rows    2,195,857 ns    0 node frames written
fork whole world     10,000 rows    2,533,012 ns    0 node frames written
fork whole world    100,000 rows    2,375,068 ns    0 node frames written
```

Flat, and **zero nodes written** at every size. A fork is a new branch in the
same granfilade whose roots name nodes already stored, so the only work is
writing those roots. The ~2.4 ms is entirely the `SyncAll` — the same ~1 ms floor
every commit pays, twice (roots, then the branch graph).

This is the single clearest case for the whole design. A copying store makes
forking a 100k-row world proportional to 100k rows; here it is proportional to
nothing.

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
single-row commit   1,000 rows     906,323 ns   4.1 frames/commit
single-row commit  10,000 rows     993,975 ns   6.1 frames/commit
single-row commit 100,000 rows   1,146,128 ns   8.1 frames/commit
```

100× the rows costs 2 extra node frames — one per extra level of tree depth. The
wall-clock rise is the fsync moving more bytes, not more algorithmic work.

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

### Reopen is eager

```
open + rebuild     1,000 rows    2,207,770 ns
open + rebuild    10,000 rows    6,370,748 ns
open + rebuild   100,000 rows   40,279,846 ns
```

Recovery is a root lookup — no log replay — but `load` then reads the *entire*
tree back eagerly, one KV `get` per node. So open is `O(state)`: 40 ms for 100k
rows, and it would be 400 ms for a million. Lazy paging (fault nodes in on
demand, keep only the root resident) is the obvious fix and is not implemented.

### Unconsolidated history accumulates

```
1,000 single-row commits →  3,930 nodes   (~3.9 nodes/commit)
5,000 single-row commits → 26,026 nodes   (~5.2 nodes/commit)
```

Every commit path-copies, and every copied node is retained until GC. The cost is
bounded and predictable — a few nodes per commit — but it is not free, and it
grows with tree depth. Consolidation reclaims essentially all of it:

```
consolidate + gc   1,000 editions    11.5 ms    3,930 → 32 nodes   (99.2% collected)
consolidate + gc   5,000 editions    88.9 ms   26,026 → 161 nodes  (99.4% collected)
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

- worlds are forked, snapshotted, and rewound — those are free here and
  proportional elsewhere;
- reads are *questions about spans* ("how many things in this room", "what is in
  this key range") rather than full-relation sweeps;
- many observers watch a large world and most changes concern few of them;
- the past is read as often as the present.

It is the wrong shape when:

- the workload is row-at-a-time commits and cannot batch — you will spend your
  time in `fsync`, not in the tree;
- the dominant read is "give me every row" — a flat array wins, though only by
  ~1.9×, so this is a reason to prefer something else, not a reason to avoid this;
- worlds are enormous and restarts must be fast — reopen is eager and linear
  until lazy paging lands.

### Follow-ups, in the order the numbers justify them

1. **Group commit.** Every axis in this report bottoms out at the same ~1 ms
   `SyncAll`: churn, fork, contention, even the precondition axis. Amortising one
   fsync across concurrent committers is the largest single win available and does
   not weaken the Patch–edition law — the law demands one atomic durable write per
   edition, not one per committer.
2. **Lazy node paging.** Fixes the 40 ms reopen *and* the resident-memory floor,
   both of which are `O(state)` for the same reason: `load` is eager.
3. **Background consolidation.** Moves the 89 ms sweep off the commit path.
4. **Retry backoff**, if fairness under contention ever matters — the protocol is
   correct but currently starves losers.

Two things the report deliberately does not claim. The scan penalty is a measured
1.9×, not the order of magnitude an earlier draft of this document asserted before
it was measured. And the P13 precondition axis is still *titled* as though
`holds_at` scans the relation tail; it does not, and the title is stale — the
numbers there are flat in history and should be read as an fsync measurement.
