# grmpl-bench — the P13 benchmark harness

Measures five engine costs against a **real `FjallStore`**, so the P13
engine-statefulness work is gated on numbers rather than intuition. This is an
edge/tooling crate above the bright line (it names fjall via `grmpl-store`,
exactly as an application would); it measures the semantic core, it is not part
of it.

## Running

```sh
cargo run -p grmpl-bench --release            # the whole suite
cargo run -p grmpl-bench --release -- precond # one axis
```

Axes: `churn`, `watch`, `precond`, `contention`, `arrangement`, `all`. Always
build `--release` — a debug build's numbers reflect the compiler, not the
engine. `cargo test -p grmpl-bench` runs every scenario at a tiny size and
asserts its invariants (so the harness cannot bit-rot), plus a behavioural
characterization of the arrangement memo's ABA hazard.

## The five axes and what they surface

* **Churn throughput** (`churn`) — raw `commit` vs the `commit_patch` process
  path, at batch sizes 1/16/256 rows per patch. Isolates per-commit fixed cost
  (edition lock + fjall persist) from per-fact cost.
* **Watch fan-out** (`watch`) — `deltas` changes fanned out to 1..256
  independent v1 `DeltaStream`s (there is *no* shared arrangement between
  streams yet), plus one `OnWatch::pump` batch. The pessimistic baseline the
  push-driven-subscription work must beat.
* **Precondition cost** (`precond`) — optimistic `commit_if` vs a bare `commit`,
  as the precondition relation's history grows 100→10k. `holds_at` sums a
  tuple's weight over the relation's **whole checkpoint + tail** with no point
  index, so precondition cost is O(relation-history), not O(live-rows).
* **Contention & retry fairness** (`contention`) — 1..8 threads racing one
  counter's precondition through `commit_if`; reports throughput, retry rate,
  and `min_wins / max_wins` fairness.
* **Arrangement memo** (`arrangement`) — a `k`-way shared sub-DAG evaluated once
  vs `k` times (base-read count is the witness), and the memo's memory footprint
  (`entries` × cached tuples) when reused across editions.

## Baseline (2026-07, release, macOS/darwin — the P13 starting line)

Absolute numbers are machine- and fsync-specific; the **shapes** are the signal.

| axis | finding |
|------|---------|
| churn | ~240 commits/s on **both** paths — fjall's per-commit `SyncAll` persist (~4 ms) dominates, so `commit_patch`'s authority/schema overhead is in the noise. Batching amortizes it: **facts/s** ≈ 240 (1/patch) → 3.7k (16) → 52k (256). |
| watch | Total poll time is linear in watcher count (3.4 ms @1 → 174 ms @256) — streams share nothing, so fan-out is O(watchers × interval). |
| precond | `ns/precond-row` is ~constant per scanned row; total precondition cost scales with history. At hist=10k, k=4 the check adds ~4.2 ms over the ~4.4 ms bare-commit floor — i.e. `commit_if` ≈ 2× a plain commit, and growing. **The motivation for an indexed/arranged precondition check.** |
| contention | retry_rate climbs 0 → 0.47 → 0.72 → 0.81 for 1→2→4→8 threads; fairness falls 1.0 → 0.57; throughput does **not** improve with more racers (the single-writer persist serializes them). |
| arrangement | Sharing reads the base **once** vs `k` times (k=32: 0.75 ms vs 5.8 ms). Reusing one memo across 200 editions pins ~120k cached tuples — the compute/memory trade the statefulness work will make durable. |

## The arrangement-memo ABA hazard (why the memo is transient)

The shared-arrangement cache (`grmpl_diff::Arrangements`) keys entries by
`(Arc::as_ptr(node) as usize, edition)` — a **transient heap pointer**. A cache
that outlives the `Query` value owning its `Arc`s is unsound: a freed node's
address can be recycled by a *different* `Query` node, and at the same edition
the stale entry is returned for the wrong sub-DAG. `tests/smoke.rs`
(`aba_hazard_stale_hit_when_pointer_is_recycled`) reproduces exactly this — a
`none` query wrongly yielding an `all` query's rows once the pointer is recycled.
It is safe today *only* because `eval_snapshot` builds a fresh cache per
top-level call and never persists it. Any P13 durable/stateful arrangement must
therefore key on a **stable, non-pointer identity**, not `Arc::as_ptr`.
