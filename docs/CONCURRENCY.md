# The concurrency model, and what cashing it in would look like

`grmpl`'s concurrency model is **optimistic concurrency control against an
edition clock, with actors as the isolation unit**. It is closer to a
transactional database under snapshot isolation than to a threaded game server:
there is no executor in the core, no async runtime, and no lock held across user
code. Concurrency is a *semantic* property — actors, editions, optimistic commit
— not a runtime one.

This document states the model as implemented, then states plainly which of the
parallelism the model permits is currently left unclaimed, and what claiming it
would involve. Part 1 is description; part 2 is a proposal and is **not** a
record of landed work.

> **Sources.** The model is specified in [`DESIGN.md`](../DESIGN.md) §2.3 #6,
> §2.4, §5.2, §5.3 and implemented in `grmpl-proc`, `grmpl-ent`, and
> `grmpl-session`. Every measurement quoted below comes from
> [`PERFORMANCE.md`](PERFORMANCE.md) §5 and
> [`PERFORMANCE-ENT.md`](PERFORMANCE-ENT.md) §3–5 — single-run figures on a
> shared 4-core VM, so ratios within an axis are the signal, absolute
> wall-clock is not portable.

---

## Part 1 — The model as it stands

### 1. Actors are the concurrency domains

`Process` (`grmpl-proc::process`) is stable identity + `Authority` scope +
ordered persistent inbox + pure `Behavior`. `DESIGN.md` §2.3 #6 names it
directly: *an actor = a concurrency domain, not an object*. Players, rooms,
zones and services are Processes; lamps and chairs are not. Cross-actor effects
are **messages**, never shared mutation.

The `Behavior` is `Snapshot × message → Patch`, pure by type
(`Box<dyn Fn(..) + Send + Sync>`). Wall clock and randomness cannot enter it
except as committed data — that is the Replay law, and it is also what makes a
behavior safe to evaluate on any thread.

### 2. Optimistic commit, not locking

The protocol (`DESIGN.md` §5.2, implemented in `grmpl-proc::commit`):

1. build the patch purely from a pinned `Snapshot` at edition `Eₛ`;
2. validate every write against `Authority.owns` (Authority law), against the
   relation's registered schema (P1), and — if it installs `Value::Code` — against
   the effect checker (P12);
3. hand preconditions + effects to the store's `commit_if`, which re-checks and
   writes **as one atomic step**.

Two writers racing the same fact resolve to exactly one winner; the loser gets
`CommitOutcome::Rejected` with **zero effect** and retries against the new
edition. `DESIGN.md` §6 step 9 puts it flatly: *no lock, no distributed
transaction — optimistic commit against editions.*

That is the `take lamp` race, and it is tested against every substrate in
`grmpl-proc/tests/optimistic_commit.rs` with 8 threads racing one lamp, and
end-to-end across two loopback sockets in
`grmpl-session/tests/tcp_session.rs`.

### 3. Exactly-once processing via cursor-in-the-batch

`Process::patch_at` attaches the inbox-cursor advance to the patch the behavior
produced, so the cursor moves **in the same atomic commit** as the effects:

* crash *before* commit → nothing written, cursor unmoved, message redelivered,
  the deterministic behavior re-runs;
* crash *after* commit → cursor already advanced, message skipped.

Exactly-once processing without a distributed transaction, and without a dedup
table on the local path.

### 4. Race-safe allocation without a global lock

`SeqAlloc` (`grmpl-proc::schedule`) folds a counter bump into the committing
patch *preconditioned on the present counter row*, so concurrent allocators on
the same key resolve to one winner and the loser retries against the winner's
value. The single unguarded case — the very first allocation of a key, when
there is no row to precondition on — is seeded once on an un-raced path via
`SeqAlloc::seed`.

`Scheduler::fire_due` uses the same trick: each due timer is delivered by one
`commit_if` **preconditioned on the timer row**, so the first fire retracts the
row and a racing driver or post-crash retry is rejected, never duplicated.

A seeded randomized law oracle races many threads across OS-scheduled
interleavings and asserts monotonicity, no-loss/no-dup, and observational
equivalence per seed: `grmpl-proc/tests/seq_contention_law.rs`.

### 5. Reactivity is messages, never re-entrancy

`OnWatch::pump` (`grmpl-proc::watch`) reads the deltas a maintained view has
produced since a durable watch-cursor and, in one atomic commit, materializes
each delta as an inbox message **and** advances the cursor. A view firing never
invokes a handler re-entrantly; cascades are chains of separate commits. The
Attention law again: *reactivity is a maintained query; no callback primitive
exists.*

### 6. What actually runs concurrently today

| Layer | Reality |
|---|---|
| Threads | Plain `std::thread`. `grmpl-session::net` spawns one thread per TCP connection. |
| Async | **None in the core.** `tokio` appears only under the off-by-default `iroh` feature of `grmpl-transport`. |
| Store | `EntStore` is `Send + Sync` behind a single `Mutex<Inner>`; `commit_if` takes it, checks preconditions, applies, and fsyncs. |
| Actor loop | Pull-based and caller-driven: `Process::step` / `run_to_idle`. Nothing spins it — `run_to_idle` notes "a real scheduler would retry after new input." |
| Session server | Commits still serialize behind one writer `Mutex<()>` (`Server.lock`, the interim P3 scheme). Inbox seqs escaped it in P4 via `SeqAlloc`; the entity counter did not. |
| Cross-domain | Durable outbox written in-commit, at-least-once shipping, receiver dedups by `(sender, seq)` (`grmpl-proc::domain`) — exactly-once apply without 2PC. |
| Branches | `EntStore::fork_at` gives cheap branch-level isolation via structural sharing in a shared granfilade. Independent worlds are real parallelism; within a branch there is one clock and one writer. |

### 7. The honest limitation

`PERFORMANCE.md` §5 states it without spin: **writes are single-writer.** Every
commit serializes behind the edition lock plus fsync, so racing writers do not
add throughput — they add rejects.

```
race x1   221 commits/s   retry_rate 0.00
race x2   213/s           retry_rate 0.50
race x4   198/s           retry_rate 0.72
race x8   170/s           retry_rate 0.85
```

`PERFORMANCE-ENT.md` §3 adds that contention is also *unfair*:

```
race x1 threads   1,012 commits/s   0 rejects   fair(min/max)=1.000
race x8 threads     935 commits/s   8 rejects   fair(min/max)=0.000
```

`fair(min/max)=0.000` means at least one thread committed nothing. Correctness
is never at risk — exactly one winner per contested precondition, which is the
law — but a hot precondition is a serialization point, not a scaling one, and
the retry loop currently starves losers.

**The design is genuinely concurrent; the implementation deliberately serializes
the write path and has not yet cashed in the parallelism the model permits.**

---

## Part 2 — Cashing it in (proposal, not landed work)

Three independent capabilities are on the table: **durability amortization**,
**reader/writer decoupling**, and **domain-parallel execution**. Ordered below by
the payoff the measurements justify.

| # | Item | Effort | Payoff | Risk |
|---|---|---|---|---|
| 0 | Guard `Alloc`, drop `Server.lock` | hours | unblocks the rest | low |
| 1 | Group commit | days | largest single win | medium (visibility gating) |
| 2 | Snapshot handles | days | removes reader stalls | medium (trait surface) |
| 3 | Actor scheduler | week+ | scales with cores | medium |
| 4 | Retry backoff | hours | fairness | low |
| 5 | Shared arrangements, background consolidation | week | read fan-out | low |
| 6 | Per-domain commit clocks | large | true write parallelism | high |

### 0. Delete the session writer lock

`grmpl-session::session` holds `lock: Mutex<()>` around every commit. P4 already
freed inbox seqs by moving them onto the guarded `SeqAlloc`; the one remaining
reason for the lock is `Alloc` (`grmpl-proc::alloc`), the entity counter, whose
`seal` is an **unguarded** retract/assert — its own doc comment says so and names
the single-writer session layer as the interim compensation.

The fix is a straight port of the pattern `SeqAlloc::seal` already uses: fold the
counter bump in *with a precondition on the present counter row*, seed the row
once on the un-raced init path (`Server::init`). Two racing spawns then resolve
to one winner and the loser retries; `Server.lock` deletes.

Small in isolation, but it converts the session layer from "serialized by
construction" to "serialized only by the store", which is what makes items 1–3
observable at all.

### 1. Group commit — the largest single win available

Every axis in `PERFORMANCE-ENT.md` bottoms out on the same ~1 ms `SyncAll` in
`Granfilade::write_full`. `EntStore::commit_if` holds `Mutex<Inner>` across
*both* the in-memory apply *and* that fsync, so N committers pay N fsyncs
strictly in series.

Shape:

* **Short critical section.** Take the lock; check preconditions; allocate the
  edition; apply in memory; stage the encoded nodes and meta into a pending
  batch; release.
* **One leader per group.** A single `db.batch()` plus one `SyncAll` covering
  every staged edition in the group.
* **Followers wait** on a condvar for `durable_edition >= mine`, then return.

**The one real hazard, and its fix.** The patch–edition law forbids a window in
which an edition is allocated but not written. Group commit deliberately creates
that window *in memory*, so visibility must be gated on durability rather than on
allocation:

* `EditionStore::current()` returns the **durable** edition;
* `commit_if` returns only after its group's fsync completes.

No observer can then act on an edition a crash would erase — no player is told
"Taken." for a take that did not happen. The law demands one atomic durable write
*per edition*, not one *per committer*, and that is the loophole worth taking.
Anything that weakens the gate instead of the batching is a violation, not an
optimization.

Expected: write throughput scales with committer count until it is bounded by
tree work rather than fsync. `grmpl-bench`'s contention axis already measures
exactly this.

### 2. Snapshot handles — lock-free reads

`Snapshot` is currently `(edition, &dyn TraceStore)` (`grmpl-diff::snapshot`), so
every `read` re-enters the store and takes the *global* mutex. A behavior
evaluating a three-way join takes that lock three times, and each acquisition can
block behind a committer sitting inside its ~1 ms fsync. **Readers are stalled by
durability they do not care about.**

The Ent's Fact enfilade is already immutable and versioned by edition — the G-2
persist fix turns on precisely that property ("an older version's root is
immutable"), and cheap forks fall out of the same fact. So a `Snapshot` should
clone the root handle for its edition under one brief lock at construction and
read lock-free thereafter.

That yields real snapshot isolation with **zero reader/writer contention** — the
read-and-branch-optimised shape `PERFORMANCE-ENT.md` §5 claims, actually cashed —
and it makes pure behaviors trivially parallel, since they would touch no shared
state until commit.

Cost: a `TraceStore` addition for an opaque snapshot handle. It must stay
technology-free above the bright line — the trait returns an opaque reader; only
`grmpl-ent` knows it is an `Arc` over a Fact root.

### 3. A real actor scheduler — parallelism across authority domains

This is the semantic payoff, and it is already *legal* rather than needing new
laws: the Authority law says one commit touches one domain, so **disjoint domains
cannot conflict by construction**. Nothing exploits that today — `Process::step`
and `run_to_idle` are caller-driven, and `run_to_idle` says outright that a real
scheduler would retry after new input.

What to build: a work-stealing pool over runnable processes (inbox non-empty),
with **one in-flight step per process** so per-actor ordering is preserved;
behaviors evaluated entirely off-lock against the snapshot handles from item 2;
only the commit synchronizing, amortized by item 1. The same executor drives
`OnWatch::pump` and `Scheduler::fire_due`, both of which are already
precondition-guarded for exactly-once under racing drivers.

With items 1–2 in place, a world of mostly-disjoint actors should scale with
cores rather than flatlining.

### 4. Retry backoff — fix the fairness bug

`fair(min/max)=0.000` at 8 threads: correct, but one thread starves. Randomized
exponential backoff in the `Rejected` path, plus a bounded retry count that
surfaces a real error rather than spinning forever. Cheap, and the fairness
metric already exists to prove it.

### 5. Read side: shared arrangements, measured routing, background work

Watch fan-out is linear and unshared — 256 watchers is 256 evaluations — while
sharing the arrangement for a common sub-query measures **2.4× wall-clock and 32×
fewer base reads**. Pair the arrangement memo across watchers over the same view
with the `touched_since` WID measure the Ent already implements (G-4), so idle
views are skipped in `O(log n)` (162 ns, versus a ≥2.7 ms re-evaluation).

Independent watchers can then pump in parallel: each pump's commit is
preconditioned on its own cursor row, so they are already race-safe against each
other.

Supporting work, same section: **background consolidation** (moves the 89 ms
sweep off the commit path; the watermark door already exists) and **lazy node
paging** (fixes the 40 ms eager reopen and the resident-memory floor).

### 6. Per-domain commit clocks

Today one store = one `EditionStore` = one total order, so every writer
serializes on one clock no matter how disjoint their authority. Sharding the
store *by domain* — `DESIGN.md` §5.2 already says one authority domain has one
commit clock — would give genuinely parallel writers, with cross-domain ordering
supplied by the message layer that already provides exactly-once apply via
durable outbox plus `(sender, seq)` dedup.

This is possible without touching the language because two decisions reserved it:
`DESIGN.md` marks the total order as **v1-only**, and the invariants require the
language to observe **opaque `Edition`s, never physical sequence numbers**. That
bright-line rule is exactly the option to make editions partially ordered later.
It is the local form of P15 distribution, and it is a large project — last, if at
all.

---

## What would prove any of it

The verification story is the strong part: **none of this changes a law.**

* `grmpl-conformance` and `grmpl-ent/tests/store_laws.rs` must stay green
  **unmodified** — determinism, patch–edition, history/consolidation, fork
  identity. If a law suite needs editing to accommodate a change here, that is
  the signal the change broke the model, not the test.
* `grmpl-proc/tests/seq_contention_law.rs` and
  `grmpl-proc/tests/optimistic_commit.rs` keep asserting exactly-one-winner under
  real threads, at whatever new level of parallelism the scheduler produces.
* The numbers that *should* move are `grmpl-bench`'s contention axis (throughput,
  retry rate, `fair(min/max)`) and the watch fan-out axis. A new axis — commits/s
  across N disjoint authority domains — is what item 3 and item 6 are for.

Suggested order: **0 and 4 first** (both cheap, both immediately visible in the
fairness numbers), then **1**, then **2**. Items 3, 5, 6 are each their own
phase.
