# The concurrency model, and what cashing it in would look like

`grmpl`'s concurrency model is **optimistic concurrency control against an
edition clock, with actors as the isolation unit**. It is closer to a
transactional database under snapshot isolation than to a threaded game server:
there is no executor in the core, no async runtime, and no lock held across user
code. Concurrency is a *semantic* property — actors, editions, optimistic commit
— not a runtime one.

This document states the model as implemented, then states plainly which of the
parallelism the model permits is currently left unclaimed, and what claiming it
would involve. Part 1 is description; part 2 is a work list whose items are
individually marked **landed** or *proposal* — items 0, 1 and 4 have landed, and
their entries record what the doing changed about the plan.

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
| Store | `EntStore` is `Send + Sync` behind a single `Mutex<Inner>`. **Group commit (landed):** the lock covers allocation, apply and encoding; the batch + `SyncAll` happen outside it, shared across a group. |
| Actor loop | Pull-based and caller-driven: `Process::step` / `run_to_idle` / `run_to_idle_retrying`. Nothing spins it — a real scheduler is still item 3. |
| Session server | **No writer lock (landed).** `Server.lock` is deleted; the entity counter is guarded like the P4 inbox seqs, and a rejected command retries under a `Backoff`. |
| Cross-domain | Durable outbox written in-commit, at-least-once shipping, receiver dedups by `(sender, seq)` (`grmpl-proc::domain`) — exactly-once apply without 2PC. |
| Branches | `EntStore::fork_at` gives cheap branch-level isolation via structural sharing in a shared granfilade. Independent worlds are real parallelism; within a branch there is one clock and one writer. |

### 7. The limitation, and what removing it did

This section used to end the document's descriptive half with a flat statement:
**writes are single-writer.** Every commit serialized behind the edition lock
plus fsync, so racing writers added rejects rather than throughput.

```
race x1   221 commits/s   retry_rate 0.00        (PERFORMANCE.md §5)
race x8   170/s           retry_rate 0.85
```

`PERFORMANCE-ENT.md` §3 added that contention was also *unfair*:

```
race x1 threads   1,012 commits/s   0 rejects   fair(min/max)=1.000
race x8 threads     935 commits/s   8 rejects   fair(min/max)=0.000
```

`fair(min/max)=0.000` means at least one thread committed nothing. Correctness
was never at risk — exactly one winner per contested precondition, which is the
law — but a hot precondition was a serialization point, not a scaling one, and
the retry loop starved losers.

**Items 0, 1 and 4 below have since landed.** The same axis now reads:

```
race x1 threads     848 commits/s     0 rejects   fair(min/max)=1.000
race x2 threads     937 commits/s     0 rejects   fair(min/max)=0.914
race x4 threads   1,740 commits/s    88 rejects   fair(min/max)=0.994
race x8 threads   3,138 commits/s   138 rejects   fair(min/max)=0.992
```

Read the ratios within the run, not the absolute numbers (a shared VM, one run):
throughput now **rises 3.7× from 1 to 8 committers** where it used to be flat,
and `fair(min/max)` went from `0.000` to `0.992` — no thread starves. The
retry rate rose from 0.4% to 6.5%, which is the expected shape: more writers are
actually in flight to lose races.

**The design is genuinely concurrent; the write path is no longer the thing
stopping it.** Reads are — see item 2.

---

## Part 2 — Cashing it in

Three independent capabilities were on the table: **durability amortization**,
**reader/writer decoupling**, and **domain-parallel execution**. Ordered below by
the payoff the measurements justified.

| # | Item | Effort | Payoff | Risk | Status |
|---|---|---|---|---|---|
| 0 | Guard `Alloc`, drop `Server.lock` | hours | unblocks the rest | low | **landed** |
| 1 | Group commit | days | largest single win | medium (visibility gating) | **landed** |
| 2 | Snapshot handles | days | removes reader stalls | medium (trait surface) | proposal |
| 3 | Actor scheduler | week+ | scales with cores | medium | proposal |
| 4 | Retry backoff | hours | fairness | low | **landed** |
| 5 | Shared arrangements, background consolidation | week | read fan-out | low | proposal |
| 6 | Per-domain commit clocks | large | true write parallelism | high | proposal |

### 0. Delete the session writer lock — **landed**

`grmpl-session::session` held `lock: Mutex<()>` around every commit. P4 had
already freed inbox seqs by moving them onto the guarded `SeqAlloc`; the one
remaining reason for the lock was `Alloc` (`grmpl-proc::alloc`), the entity
counter, whose `seal` was an **unguarded** retract/assert.

The fix was the pattern `SeqAlloc::seal` already used: fold the counter bump in
*with a precondition on the present counter row*. Two racing spawns now resolve
to one winner and the loser retries; `Server.lock` is deleted.

Three things that only showed up in the doing:

* **The seed must ride inside the guard.** A first allocation has no row to
  precondition on, so it must be un-raced. Seeding a player's inbox-seq counter
  in a *second* commit after the spawn let two logins of one name seed it twice
  and hand out a duplicate seq. It is now asserted in the same atomic commit the
  entity-counter guard protects, which is what makes it un-raced. (`INBOX_SEQ`
  joined `world_authority` for this: a spawn creates a process, so it owns that
  process's inbox plumbing — the read cursor was already there.)
* **A rejection must be retried, not swallowed.** `Process::run_to_idle` stops at
  the first rejection, which was right when the lock made rejections impossible
  and wrong the moment it was gone: a lost race left the command unconsumed and
  the client with no reply at all. `run_to_idle_retrying` rebuilds the patch from
  the *current* world, so a lost `take lamp` re-decides against the winner's
  state and answers "you don't see that here" instead of nothing.
* **Same-name logins need no special case.** Two logins of one new name contend
  on the entity counter like anything else; the loser re-reads, finds the
  winner's `PLAYER` row, and adopts that identity. Durable identity is upheld by
  the counter guard, not by serialization.

Laws: `grmpl-proc/tests/alloc.rs` (racing allocations hand out no id twice — it
fails on the unguarded `seal`, with 48 allocations collapsing to 30 distinct ids;
and every contender commits its whole quota under backoff) and
`grmpl-session/tests/concurrent_world.rs` (concurrent builders never collide,
concurrent same-name logins bind one identity, racing clients yield exactly one
taker and no silent losers).

### 1. Group commit — the largest single win available — **landed**

Every axis in `PERFORMANCE-ENT.md` bottoms out on the same ~1 ms `SyncAll` in
`Granfilade::write_full`. `EntStore::commit_if` held `Mutex<Inner>` across
*both* the in-memory apply *and* that fsync, so N committers paid N fsyncs
strictly in series.

Shape, as built:

* **Short critical section.** Take the lock; check preconditions; allocate the
  edition; apply in memory; encode the nodes and meta into a `StagedWrite` and
  queue it; release. Encoding stays inside the lock because it reads the trees;
  it is CPU, not I/O.
* **One leader per group.** Whoever needs the write performs it — there is no
  dedicated writer thread, so an uncontended commit is its own group of one and
  pays no handoff. `Granfilade::write_group` applies the whole queue as a single
  `db.batch()` plus one `SyncAll`.
* **Followers wait** on a condvar for `durable >= mine`, then return.

**The hazard, and what the fix actually turned out to be.** This document
proposed gating visibility on durability — `EditionStore::current()` returning
the durable edition. *That does not work, and the reason is worth recording.*

`commit_if` must validate preconditions against the **allocated** state. Checking
them against a lagging watermark would let two committers in the same group both
win a contested precondition, breaking exactly-one-winner — the one thing that
must not break. And once the validator is at the allocated edition, reads must be
too: otherwise every optimistic read-modify-write builds its patch on a stale
world and is rejected. Not occasionally — *systematically*, because under load
the watermark lags for as long as any group is in flight. The first
implementation of this item livelocked a guarded allocator within a dozen
attempts, and `grmpl-ent/tests/group_commit.rs::a_guarded_allocator_does_not_livelock`
is the regression that pins it.

So the gate is on the **commit call**, not the clock:

* `commit`/`commit_if` return only after the edition they return is durable;
* `EntStore::durable_edition()` exposes the on-disk frontier for an observer that
  externalizes a read *without* committing.

That preserves what the law is protecting. The patch–edition law is about
*atomicity* — an edition is allocated and written as one step — and a batch is
still atomic, so no state ever carries part of an edition. No committer learns of
its own edition before disk does, so no player is told "Taken." for a take a
crash would erase. The residual window is precise: between a peer's stage and its
group's fsync, a third party can read an edition not yet on disk — but it cannot
externalize that read *through the store*, because the queue is FIFO in edition
order and a group's fsync covers every edition staged before it, so any commit
built on edition `E` is itself durable only once `E` is.

**Measured** (`grmpl-bench contention`, §7 above): 8 committers went from 935 to
3,138 commits/s. `Granfilade::syncs()` is the ops counter that keeps the claim
falsifiable, in the same spirit as `frames_encoded` — 192 concurrent commits cost
50 fsyncs, and a test fails if grouping ever silently stops.

Consolidation, `gc`, `fork_at` and the context enfilade drain the queue before
writing on their own: consolidation is the one occasion that *replaces* roots, so
a staged commit landing afterwards would re-insert one it had just retired.

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

### 4. Retry backoff — fix the fairness bug — **landed**

`fair(min/max)=0.000` at 8 threads: correct, but one thread starved.
`grmpl-proc::retry::Backoff` is full-jitter exponential backoff (delay drawn
uniformly from `[0, min(base·2ᵏ, cap))`, defaulting to 16 attempts from 50 µs to
a 5 ms ceiling) with a bounded attempt count, so a livelock surfaces as an
`Error::Store` rather than an infinite spin. `fair(min/max)` is now 0.992 at 8
threads.

**The jitter is not nondeterminism.** Backoff decides *when a thread tries
again*, never *what it writes*; the retried patch is still rebuilt from a fresh
snapshot and is still a pure function of committed data. It deliberately draws no
entropy from the environment either — jitter's job is to decorrelate contenders,
not to be unpredictable, so the sequence is a xorshift seeded off a process-wide
counter. No clock is read and no OS randomness is sampled, so the Replay law is
untouched and the whole thing stays reproducible under a debugger.

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

> Expanded in [`CONCURRENCY-DISTRIBUTED.md`](CONCURRENCY-DISTRIBUTED.md): causal
> frontiers, a staged implementation of this item, and the costs it incurs.


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

Order taken: **0 and 4 first** (both cheap, both immediately visible in the
fairness numbers), then **1**. All three have landed, and every law suite listed
above stayed green **unmodified** — which was the point of the check. Next is
**2**, now the binding constraint: with the write path amortized, a reader that
takes the global mutex three times for a three-way join is what a committer's
fsync blocks. Items 3, 5, 6 are each their own phase.
