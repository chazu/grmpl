# grmpl performance characteristics

How fast are operations on a grmpl world, and *why* — traced to the underlying
data structures. This is a code-grounded reference: every complexity claim cites
the loop or scan that produces it, and the numbers come from the benchmark
harness ([`crates/grmpl-bench`](../crates/grmpl-bench), run against a **real
`FjallStore`**, release build, macOS/arm64, 2026-07).

> Reproduce: `cargo run -p grmpl-bench --release`.

**TL;DR.** Commits are **fsync-bound** (~240/s), not CPU-bound. Reads and joins
are **linear scans of whole relations** (there is no secondary/point index), so
they cost O(relation state) — cheap while relations are small, and kept small by
*consolidation* (GC). The one super-linear cost is the optimistic precondition
check, which is **O(relation history)** until you consolidate. Values are `Arc`-
backed, so the engine clones tuples freely; the expense is the scans and the
`HashMap` rebuilds, never the copies.

---

## 1. The `Ent` and value representation

Everything the world stores is a `Tuple` of `Value`s, and the shape of those
types explains most of the cost model.

```rust
// crates/grmpl-core/src/value.rs
pub struct Entity(pub u64);     // Copy — a bare 8-byte handle
pub struct RelId(pub u32);      // Copy
pub enum Value {
    Ent(Entity), Int(i64), Bool(bool),   // inline, Copy-sized payloads
    Text(Arc<str>), Tuple(Arc<[Value]>), // reference-counted
    Bytes(Arc<[u8]>), Code(Arc<[u8]>),   // reference-counted
}
pub struct Tuple(pub Arc<[Value]>);      // Arc-backed
```

Consequences:

* **An entity is a plain `u64`.** Comparing, hashing, or passing an `Entity` is a
  register-width operation — no allocation, no refcount, no indirection. `RelId`
  is a `u32`. This is why store keys and join keys stay tiny.
* **Heap values are `Arc`, so `clone()` is O(1)** (a refcount bump), regardless of
  string length or tuple arity. The query engine leans on this hard: it clones
  key columns and matched tuples throughout joins and memo hits. The cost of a
  join is the `HashMap` it builds and the relations it re-reads — **not** the
  value copies.
* **Ordering is structural.** `Value` and `Tuple` derive `Ord` lexicographically
  (tag order `Ent < Int < Text < Bool < Tuple < Bytes < Code`, then into the
  contents). Identity is by content, never by pointer. This is the sort key used
  everywhere the store returns rows in a deterministic order.

### On the wire

Every committed row and every message is serialized once through
`grmpl_core::wire`. Encoding is a single linear pass with one `Vec<u8>`
allocation; per value it is `tag(1) || payload`:

| Value    | Encoded size                       |
|----------|------------------------------------|
| `Ent`    | 9 bytes (tag + fixed 8 BE)         |
| `Int`    | 9 bytes                            |
| `Bool`   | 2 bytes                            |
| `Text`   | 1 + 4 (len) + UTF-8 bytes          |
| `Tuple`  | 1 + 4 (len) + Σ elements           |
| `Bytes`/`Code` | 1 + 4 (len) + raw            |

A stored record is `version(1) || diff(8, LE) || encoded_tuple`. Encoding cost is
O(encoded size) and is paid **per committed row**; at single-commit rates it is
lost in the fsync, and it only becomes visible at large batch sizes. **Decoding
re-allocates** a fresh `Arc` per `Text`/`Bytes`/`Tuple`, so read paths rebuild
values from bytes rather than sharing the in-memory `Arc`s.

---

## 2. The physical storage model

`FjallStore` is a log-structured (LSM) store with **one fjall keyspace per
relation** (`rel_{id}`). A relation is an append-only log of signed updates:

```
key   = edition(8, BE) || counter(8, BE)      // 16 bytes, = commit order
value = version(1) || diff(8, LE) || encoded_tuple
```

* The `(edition, counter)` key **is** the commit order — deterministic, sorted,
  range-scannable.
* The **`edition = 0` prefix is reserved for checkpoints**: after consolidation,
  one net row per surviving tuple, disjoint from the live tail (editions ≥ 1).
* A relation's current state is therefore *checkpoint + the tail of updates since
  the last watermark* — folded (summed by tuple) on read.

There is **no secondary index**: no index on tuple content, no index on join
keys. Every "does this tuple exist" or "give me this relation" question is a
linear scan of the relation's log. This single fact drives sections 3–4.

Meta (`__meta` keyspace) holds the edition clock, watermark, the durable
name→`RelId` catalog, and versioned schemas.

---

## 3. Cost of each store operation

| Operation                     | Complexity                                   | Real cost driver               |
|-------------------------------|----------------------------------------------|--------------------------------|
| `commit(updates)`             | O(#updates) + **1 fsync**                    | the fsync (~4 ms) dominates    |
| `commit_if` precondition      | **O(checkpoint + tail)** *per precondition*  | linear `holds_at` scan         |
| `read_at(rel, edition)`       | O(checkpoint + tail-since-watermark) + sort  | full consolidating read        |
| `scan_updates(rel, from, to)` | O(#updates in window) + sort                 | range scan (cheap for deltas)  |
| `watermark()`                 | O(1)                                         | cached                         |
| `consolidate(up_to)` (GC)     | O(Σ relations' checkpoint + tail)            | rewrites checkpoints, deletes history |
| `fork(path)`                  | **O(domain state)**                          | verbatim keyspace copy + fsync |
| `canonical_dump()`            | O(domain state) + sort                       | full dump (fork/replay verify) |

### Commits are fsync-bound

`commit` writes all updates in one atomic batch and `persist(SyncAll)`. The CPU
work (encode + insert) is trivial next to the durability fsync:

```
raw commit  (1/patch)     245 commits/s   (~4.1 ms/commit)
raw commit  (256/patch)   205 commits/s → 52,562 facts/s
commit_patch (1/patch)    247 commits/s
```

Throughput in *commits* is ~flat regardless of batch size (the fsync is fixed
cost); throughput in *facts* scales with batch — **batch your writes** to amortize
the fsync (1 → 16 → 256 facts/patch ≈ 245 → 3.9k → 52k facts/s).

### The precondition check is O(history) — the headline bottleneck

`commit_if` (used by every `expect` in a behavior) re-checks each precondition
with `holds_at`, which **sums a tuple's weight by scanning the entire checkpoint
and the entire tail**, decoding each record just to compare one tuple. There is
no point lookup. So an `expect` costs O(relation history) until the relation is
consolidated:

```
commit     hist=100    k=0    233/s   ns/probe 4.28M          (bare-commit floor)
commit_if  hist=100    k=4    240/s   ns/probe 4.16M
commit_if  hist=1000   k=4    198/s   ns/probe 5.06M
commit_if  hist=10000  k=1    169/s   ns/probe 5.93M
commit_if  hist=10000  k=4     95/s   ns/probe 10.5M    ← ~2.5× the bare commit
```

At 10k rows of history with 4 preconditions, the check *doubles-and-more* the
commit time and keeps growing. This is exactly what **consolidation** exists to
fix: after GC, `holds_at` scans `checkpoint + short tail` — O(state), not
O(history). Practically: `expect` against a small, consolidated relation is
negligible; `expect` against a hot, never-GC'd relation degrades linearly.

### Reads consolidate on the fly

`read_at` folds checkpoint + tail into a `HashMap`, drops zero-weight tuples,
and sorts. Cost is O(checkpoint + tail-since-watermark). `scan_updates` is a
cheap range over just the requested edition window (checkpoints, at edition 0,
are never scanned as deltas) — this is what makes reactive maintenance and
replay affordable.

### Consolidation turns O(history) into O(state)

`consolidate(up_to)` folds each relation's old checkpoint + tail into fresh
checkpoint rows, **physically deleting** the folded history, and advances the
watermark atomically. It is the lever that keeps `read_at`/`holds_at`/`fork`
proportional to *present state* rather than *total history*. A store you never
consolidate grows unbounded and its precondition checks slow down with it; a
consolidated store forks and reads in proportion to what's live.

---

## 4. Cost of reads, views, and joins

A `view` is a query plan evaluated against a snapshot. `Query::find` is a
recursive tree-walk with these leaf/node costs:

* **Base relation leaf** → a full `read_at` (§3). Every base relation a view
  touches is read *in full and consolidated* on every evaluation. No filter or
  key is pushed into the store; `filter`/`project` run in memory afterward.
* **Join** → an **in-memory hash join, rebuilt from scratch every evaluation**:
  it builds a `HashMap` over the right side, probes with the left, and multiplies
  weights. One join is O(|L| + |R| + |matches|) in memory — but L and R are each
  a fresh full `read_at`. There is **no persistent join index**.
* **`distinct` / `reduce` (aggregates)** → O(input): a full pass / group-and-fold
  over the whole input multiset.
* **Recursion (`Iterate`)** → least fixpoint: O(#iterations × full-eval-per-step);
  each round re-reads its base relations.

So a view's cost ≈ Σ(sizes of the base relations it joins), re-scanned each time
it runs. For a MOO whose relations hold tens of rows this is microseconds; it
scales linearly with world size, and there is currently no index to make a
selective query sublinear in the relation.

### Arrangements: share a subquery instead of recomputing it

When the *same* sub-DAG is referenced k times, wrapping it as a `Query::Shared`
memoizes its whole result (keyed by node identity + edition) so it is evaluated
**once**, not k times:

```
unshared k=32   9.93 ms   (32 base reads)
shared   k=32   1.36 ms   ( 1 base read)   ≈ 7–8× faster
```

This is the engine's main optimization — read the base once, reuse it across
every consumer. The trade is memory: a memo held across N editions pins ~all the
tuples it cached (the benchmark pins ~120k tuples across 200 editions).

> **Caveat (implementation note):** the snapshot arrangement memo keys on a
> transient heap pointer (`Arc::as_ptr`), which has a known ABA hazard if a memo
> outlives the query that owns it. It is safe today because `find` builds a fresh
> memo per top-level call and never persists it; any *durable* arrangement must
> re-key on content (the pattern engine already does this).

### Incremental maintenance (`eval_delta`)

Reactive `on watch` and differential views maintain results from deltas rather
than recomputing:

* **Base delta** = `scan_updates(from, to)` — O(window), cheap.
* **Linear ops** (map/filter/project/union/negate) pass deltas straight through —
  O(delta).
* **Join delta** uses the bilinear rule `Δ(A⋈B) = ΔA⋈B + A⋈ΔB` — but each term
  needs a *full snapshot* of the opposite side, so a maintained join is
  delta-on-one-side × full-scan-on-the-other.
* **`distinct`/`reduce`/`iterate` deltas** are a full recompute at both ends and a
  difference (the sanctioned recompute-on-change fallback for the non-linear
  boundaries).

---

## 5. Concurrency & fan-out

* **Writes are single-writer.** Every commit serializes behind the edition lock +
  fsync, so racing writers do **not** increase throughput — they increase
  rejects. `commit_if` losers retry:

  ```
  race x1   221 commits/s   retry_rate 0.00
  race x2   213/s           retry_rate 0.50
  race x4   198/s           retry_rate 0.72
  race x8   170/s           retry_rate 0.85
  ```

  Correctness is never at risk (exactly one winner per contended precondition),
  but a hot precondition is a serialization point, not a scaling one.

* **Watch fan-out is linear and unshared.** Each independent watcher polls its own
  stream; total work is O(watchers × deltas). 1 → 256 watchers over 200 deltas
  goes 2 ms → 154 ms. Watchers over the *same* view could share an arrangement;
  independent v1 streams do not.

---

## 6. What this means for the MOO

Mapping [`worlds/moo.grmpl`](../worlds/moo.grmpl) operations onto the above:

| Player action        | What runs                                             | Cost                                             |
|----------------------|-------------------------------------------------------|--------------------------------------------------|
| `look`               | `here`/`ways`/`contents` views — joins over `located`, `named`, `exits` | Σ(base relation sizes), re-scanned; µs at MOO scale |
| `take` / `drop` / `go` | behavior → patch with one `expect` + a `commit_if`  | ~4 ms (the fsync); `expect` = `holds_at` over `located` (small ⇒ negligible) |
| `greet`              | `resolve folks` (a join) + two `assert`s, one commit  | one view eval + one fsync                        |
| cat `tick`           | `find located` + `find patrol` + guarded move, one commit | one view eval + one fsync per turn           |
| `treasure` / Observatory | `sum` aggregate view over `located ⋈ value ⋈ named` | O(rows) group-and-fold, re-read each look     |
| cribbage `score`     | `crib_fif2`/`crib_fif3`/`crib_pairs` views join the 5-card hand against the rule tables (`cardval` 52 rows, `sum15three` a few hundred) | bounded by the rule-table sizes × the small hand joins — the `sum15three` join is the largest term |

The MOO never notices any of the bottlenecks because its relations are tiny and
its store is effectively fresh. The same world scaled to millions of rows would
feel: (a) each command still ~one fsync; (b) `look` growing linearly with how
much is in the world; (c) `expect`-heavy verbs slowing down between
consolidations; (d) cribbage `score` unchanged (its inputs are the fixed rule
tables + a 5-card hand). The fixes are the ones the substrate already names:
**batch commits, consolidate to bound history, and share arrangements** for
repeated sub-queries — with an indexed precondition/relation lookup being the
main open optimization (the P13 statefulness work).

---

## 7. Summary of the cost model

* **Entity = `u64`, values = `Arc`** → tiny keys, O(1) clones; copying is never the
  bottleneck.
* **Log-per-relation, no secondary index** → reads/joins/preconditions are linear
  scans of relation state.
* **Commits are durable (fsync)** → ~240/s; batch to amortize.
* **Preconditions are O(history)** → consolidate to make them O(state).
* **Joins re-read bases every eval** → arrangements memoize the repeated ones (~8×).
* **Deltas are cheap; non-linear boundaries recompute** → reactive views are affordable, aggregates/recursion recompute on change.
* **Single-writer, unshared fan-out** → contention serializes rather than scales; correctness holds regardless.
