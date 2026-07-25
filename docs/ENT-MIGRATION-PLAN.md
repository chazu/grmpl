# Migrating grmpl to a true Ent — plan (v3 — STABLE after 3 adversarial rounds)

> **Review status:** hardened through three rounds of four-critic adversarial
> review (Xanadu fidelity · Rust systems realism · grmpl-invariant preservation ·
> migration risk). Round 3 verdict: **STABLE, 4/4** — every prior CRITICAL/MAJOR
> resolved with concrete, code-level specs; only B0-tuning knobs remain (§7).

**Goal (committed).** Reach **full parity with the properties of Xanadu Gold's
`Ent`** — measured trees, WID upward summaries, **DSP coordinate transforms**,
structural sharing, an edition/branch DAG, a canopy interest index, and
**backfollow/version-compare provenance** — **while preserving every grmpl system
feature** (relational/differential model, the seven laws, P0–P15, the bright line,
one-serialization).

**Full parity is scheduled, ambition-justified work, exempt from the
measured-need gate.** The project's *"measured need, not scheduled"* ethos governs
**order and validation** — *do the shippable, measured wins first; gate each phase
on beating a benchmark* — but it does **not** govern *whether* the full-Ent phases
(Dsp, backfollow) happen. They are the stated goal, so they are committed. This is
the explicit resolution of the round-1/2 "goal vs gating" contradiction.

**What changed in v3.** Two rounds of four-critic adversarial review. Round 2
verified the log-as-truth reframe against the Gold source and confirmed **Tier A
is sound and shippable**; the residual findings were concrete engineering specs.
v3 folds all of them in, tagged `⟢ r2:` at the fix.

---

## 0. Architecture invariant: the Ent is the log **plus** a coordinated family of derived enfilades

The authoritative, byte-canonical source of truth is grmpl's existing raw
`(edition, counter)` append log (the Edition-enfilade's job, already done well). It
serves `scan_updates`, replay, and `canonical_dump` **byte-identically and
unchanged**. The Ent's measured trees are **derived, rebuildable indexes over the
log** — never the system of record. `⟢ r2:` dissolves the read_at-vs-scan_updates
contradiction (raw log keeps the raw sequence; net-state tree is a fast path;
root-diff is an *internal* `eval_delta` accelerator, never the `scan_updates`
implementation).

**We never delete the log, and we never delete `grmpl-store`.** `grmpl-store`
(the LSM log) and the ent-vs-LSM differential oracle are retained **permanently as
a CI-only reference** — a genuinely independent third leg (LSM, ent, model) so no
correlated model bug can pass unseen. "Cutover" means *default the runtime to
`grmpl-ent`*, not *delete the reference*. `⟢ r2:` resolves both the
reframe-vs-decommission contradiction and the correlated-failure-oracle risk.

**Authoritative vs derived keyspaces (load-bearing).** Physical state is
partitioned by keyspace name: **authoritative** = the log `rel_*` records +
`__meta` (edition clock, watermark, catalog, schemas — **and nothing else**);
everything else (`idx_*`, `node_*`, `arr_*`, `canopy_*`, and any future derived
prefix) is **derived**. `canonical_dump`, `fork(path)`, and the P10 byte-identity
predicate range over the authoritative set as an **allowlist** (`rel_* ∪ __meta`),
*not* a denylist of known derived prefixes — so a later derived keyspace (e.g.
`dsp_*`) cannot silently leak into the dump. Critically, the monotonic `phys_id`
allocation cursor lives in a **derived** keyspace, **never in `__meta`** — else two
different commit-batchings of the same log would allocate different phys_ids and
`__meta` would diverge byte-wise, breaking the very dump-identity test. Derived
state is separately checked **query-equivalent** (identical `read_at`/`holds_now`/
`range_at` answers over the key domain) — *not* structurally equal, since tree
shape legitimately varies (see §1). `⟢ r2/r3:` fixes "canonical_dump dumps every
keyspace" (allowlist), pins the `phys_id`-cursor placement, and defines "derived
logically-equal" as query-equivalence rather than an ill-posed structural check.

---

## 1. Node identity: split identity from placement (the trilemma, resolved)

Round 2 (systems + fidelity, both CRITICAL): one node id cannot be
locality-preserving **and** canonical **and** version-sharing at once. v3 splits
the three roles onto three mechanisms:

* **Identity / dedup / canonicity** — a **content key** (structural hash of the
  node's canonical bytes) interned in a `node_intern` dictionary
  (`content_key → phys_id`). Equal subtrees intern to the same entry →
  cross-version structural sharing and **leaf-level canonicity** (the same *leaf
  set* dedups identically regardless of insertion order).
* **Physical placement / locality** — a **monotonic `phys_id`** allocated on first
  intern, used as the fjall node-table key so a node's neighbors stay clustered
  (locality preserved; `phys_id` is *not* content-derived and may differ across
  equal trees).
* **The equality axis is query-level, not tree-shape.** Because §2 adopts Gold's
  heuristic *balance-on-insertion*, two batchings of the same log share the same
  leaves but may build different **internal** structure. So canonicity is claimed
  only at the **leaf/query level**: `canonical_dump`/replay/fork identity are
  defined over the authoritative log (§0), and derived trees are certified
  **query-equivalent** (equal `read_at`/`holds_now`/`range_at` over the key
  domain), never by internal node-set equality. This is safe for every grmpl law:
  `read_at` sorts its output, `scan_updates` sorts by `(edition,counter)`, and
  `find` binds the least tuple — all leaf/query-level, independent of internal
  balance. `⟢ r2/r3:` resolves the trilemma *and* the §1-vs-§2 "equal tree"
  over-claim against heuristic balancing.

This is a **B0 freeze deliverable**: the intern scheme, the content-key hash
(sort-before-hash; no HashMap-iteration, RNG, or pointer input), and the node
frame are chosen and frozen — *pending B1 validation*, with the node-frame version
byte as the stated escape hatch. B0 **must also (a) measure the `node_intern`
write-path cost** (it is a second `content_key → phys_id` LSM index consulted every
commit) so the B1 budget is set from data, not guessed, and **(b) reserve
dsp-threading hooks** in the node/traversal frame so B4's coordinate transforms are
not a from-scratch rework of B1–B3's core. `⟢ r3:` measure-intern-cost-at-B0;
pre-reserve-dsp-hooks; freeze revocable at a stated re-open cost.

---

## 2. Full-Ent parity, faithfully (fidelity, corrected & committed)

Round 2 verified every Gold citation against `udanax-top.st`. v3 commits the
mechanisms, not just the names:

* **DSP = coordinate transform** (`DspLoaf {myDsp, myO}`: shares `myO`, stores one
  displacement, composes dsps down the descent). B4 implements a
  `DspNode(child, displacement)` over the key coordinate space → **O(edit)
  *relocating* copy** (content-hash alone shares nothing under relocation — every
  key changes). Retrofitting dsp-threading into the traversal/measure core is a
  **rework of the loaf algebra, not a bolt-on**; v3 states this discontinuity
  honestly and, crucially, notes it needs **no migrator** — a dsp-aware node frame
  is a new node-frame version, and derived trees are **dropped and rebuilt from
  the log**. `⟢ r2:` parity-ladder discontinuity + "node-frame evolution needs no
  migrator."
* **Backfollow / version-compare is committed** (the read side of the trace):
  B4 implements `Loaf»compare:with:`-style provenance — "this content is the same
  as edition E, relocated" — not merely a set-delta. `⟢ r2:` backfollow restored
  from non-goal to committed goal.
* **WID pruning needs multiple orders.** A single tuple-`Ord` prunes only
  lead-prefix queries; real joins bind trailing columns, so **Arrangements (one
  measured tree per relevant column order)** are first-class from Tier A, with
  box-region (`CrossSpace`/`BoxRegion`) measures as the general form. Every
  range-pruning claim is benchmarked on a **trailing-column** query.
* **Authority lives in the canopy flag-lattice, not a separate "scope" tree.**
  Gold carries permissions in `CanopyCrum` endorsement flags + a catch-all
  (`OtherEndorsements`), never in dsps and never in a parallel scope-cover tree.
  v3 folds authority into the canopy endorsement lattice; `Catalog`/`SchemaCatalog`
  remain the durable, GC-exempt façade for **names/schemas only**. `⟢ r2:` "scope
  context" was a non-Gold structure — removed.
* **Canopy discipline = pointer-stable rebalancing, refcounted** (Gold's
  `BertCrum`: "heuristically balanced upon insertion … in such a way that the
  ocrums … need not be updated → no backpointers"), interest as a **flag OR-lattice
  + explicit catch-all bucket**. Routing is **conservative** (a superset of the
  naive pump's deltas, never a subset). `Canopy` is typed in **core vocabulary
  only** (new `grmpl_core` types `EditionDelta` and a tuple `KeyRange`; a
  compile-test asserts the trait signature names only `grmpl_core` types). `⟢ r2:`
  "never rebalance" mis-stated Gold; canopy vocab must be core.

*Footnote:* the fjall node table is a modern substitute for Gold's
`GrandNode`/`GrandHashTable` *granfilade* (hash-partitioned pages + online
doubling); we do not call it the granfilade.

---

## 3. Preservation guards (the acceptance spine, made concrete)

Every row is a conformance test run against **all three legs** (LSM, ent, model)
*and* the **full P0–P15 suite** (`fork.rs`, `replay.rs`, `count_window_law.rs`,
`windowing_law.rs`, `law_oracle.rs`, `showcase`) on `grmpl-ent` every phase.

| Invariant | Concrete guard | `⟢ r2` |
|-----------|----------------|--------|
| `scan_updates` raw/ordered/multiplicity | served by the untouched log; byte-equal `Vec<Update>` incl. same-edition order and `+1/-1/+1` | resolved |
| `read_at` net, sorted | equal to log; zero-crossing tuples absent from the index | resolved |
| **Edition doors** | watermark check runs **first, before any index consult**, on `read_at`/`holds_now`/`range_at`; **`fork_edition(at)` itself errors when `at < watermark`** (no coherent branch base survives below W); matrix incl. "index still retains a below-W node ⇒ read below W **errors**" | MAJOR-2 / r3 |
| `canonical_dump` / fork identity | defined over the **authoritative** keyspace set; derived keyspaces excluded by name-prefix; test: same log via two batchings ⇒ dump byte-equal **and** derived trees logically-equal **and** derived keyspaces non-empty | CRITICAL-1 |
| Determinism / replay | equality over the **content-key** view; sort-before-hash; no RNG/pointer/HashMap-order input to the content key | resolved |
| **Atomic crash-consistency** | **exactly one `db.batch()` per commit** (log rows + all index/node mutations + edition bump), one `persist(SyncAll)`; guard-test fails on >1 `batch.commit()`/commit; reopen-after-simulated-crash test asserts index == log | MINOR-3 |
| **Runtime index==log detector** | Fact-tree root measure carries `(Σdiff, count)` cross-checked O(1) against a rolling per-relation log checksum every commit; scheduled background rebuild-and-diff | systems M3 |
| GC | reachability from retained roots ≥ watermark; **current-edition index root pinned (never collectible)**; only historical nodes below W collectible; serialized under the edition lock; catalog/schema GC-exempt | resolved + fidelity MINOR |
| Canopy | routed deltas a **superset**, never subset, of the pump; no double-delivery; proven pump-equivalent on P5/P12 | resolved |
| **Format** | value `FORMAT_VERSION` **strict everywhere, no multi-version reader** (the "migrator reads old value-version" clause is deleted — the migrator bridges *structure only*: current-version log → new node frames); node frame carries/inherits the value-version of its embedded `wire::encode_tuple` payloads and **errors `Codec`** on mismatch; node-frame version added to CLAUDE.md's bump rule | MAJOR-3 |
| Bright line | no enfilade/WID vocab above the store trait; `fork_edition → Edition`; `Canopy`/`DagStore` in core types | resolved |
| **Rebuild** | derived indexes rebuildable **from the authoritative store down to the watermark** (below it the doors govern); rebuild is a **background/online** op (store serves the log meanwhile); reindex throughput benchmarked | MINOR-2 + systems M5 |

---

## 4. Trait evolution (additive + a capability sub-trait)

```
EditionStore                                   (unchanged)
TraceStore  + holds_now(rel, tuple) -> Diff                    // point index
            + range_at(rel, at, KeyRange, order) -> Vec<(Tuple,Diff)>  // Arrangement scan
                                                               // fully implementable by grmpl-store
DagStore : TraceStore                                          // NEW capability sub-trait
            + fork_edition(at) -> Edition                      // opaque; ent only
            + ancestry(a, b) -> AncestryInfo                   // needs the DAG; ent only
Canopy  (core-typed)  register(RelId, KeyRange) / route(EditionDelta)
```
`fork_edition`/`ancestry` sit on **`DagStore: TraceStore`**, which only
`grmpl-ent` implements — so `grmpl-store` remains a *fully valid* `TraceStore`
through Tier A, and "this backend has no DAG" is a **compile-time** fact, not a
runtime error. The conformance oracle asserts the ent takes the **overridden fast
path** (not a default) with per-op **cost** assertions, so a green suite can't hide
a regression. `⟢ r2:` MAJOR-7 + default-impls-hide-regressions.

---

## 5. Sequencing (measured wins first; full parity committed; every gate has teeth)

Effort labels are honest: **⧗⧗⧗ = a research-grade subsystem.** Every phase is
gated against a **consolidated-LSM baseline at n ≤ 10k** with **thresholds and
workloads fixed in this plan (steward-owned), before the phase starts** — never
self-authored mid-phase. `⟢ r2:` self-authored-benchmark conflict-of-interest.

**Tier A — Indexed store (= roadmap P13 statefulness). Ships value to the live
system first.**
* **A1 — net-weight point index** ⧗ · `enc(tuple)→Σdiff` maintained in the commit
  batch; `holds_now` = one point-get. **Gates:** precondition curve flat in
  history; **and** precondition-*free* commit throughput at batch-256 regresses
  ≤ **10%** vs the 245 c/s · 52k facts/s floor; report the write-amplification
  factor and RMW cost on an *un-consolidated* tail. `⟢ r2:` M1 (measure the tax,
  not just the win).
* **A2 — Arrangement indexes** ⧗⧗ · per-column-order trees; selective
  trailing-column reads/joins. **Gate:** beat the linear scan on a *trailing-column*
  query; **cap #Arrangements** by the write-amp ceiling from A1.
* **A must survive `consolidate`** (P6 rewrites checkpoints/deletes history): the
  index is scoped to **current** state and is maintained across consolidation;
  as-of historical `range_at` is a Tier-B concern. `⟢ r2:` Tier-A/consolidate
  interaction + current-only scope.

**G-A (technical-readiness checkpoint, not a "should we?" gate):** Tier A wins
confirmed on real workloads **and** B0 spike done **and** node format frozen **and**
the model oracle validated (below) → proceed to B1. Full parity is committed, so
G-A gates *readiness*, not *scope*.

**B0 — spike, FREEZE, and stand up the independent oracle** ⧗⧗ · decide the
node-identity intern scheme (§1) and node/measure frame against grmpl's `Tuple`-Ord
and measured-write cost; **freeze the format** (revocable via the version byte).
Build the **spec-derived model oracle** (written from DESIGN/`idea.md`, *not* by
reading `grmpl-ent` — a model cribbed from the impl is circular), **validate
model ≡ today's `grmpl-store`** on the full P0–P15 suite, and **mutation-test** it
(it must catch injected ent bugs) before B1 trusts it. `⟢ r2:` B0 front-loads the
real risk; model must be independent + validated + mutation-tested.

**Tier B — the persistent Ent (extends the roadmap at P14+).** Committed. Each
phase's benchmark **workload + threshold is fixed here**:
* **B1 — persistent measured Fact tree + Arrangements + intern node table** ⧗⧗⧗ ·
  *ships:* MVCC reads, version-shared storage. **Gate (bounded, not
  no-regression — a COW tree cannot match a raw append on bytes):** commit-latency
  ≤ **+25%**, bytes-fsync'd ≤ **3×**, peak-RSS ≤ **2×** vs the LSM at n ≤ 10k,
  *justified by* the branch/fork win B2 delivers. If even the bounded budget is
  breached at scale, that is **documented and escalated**, not silently absorbed —
  parity remains committed, but the cost is surfaced for a design decision.
  `⟢ r2:` C2 (unmeetable "no-regression" gate → bounded budget + explicit escalation).
* **B2 — edition/branch DAG (mirrors `DagWood`) + `fork_edition`/GC** ⧗⧗⧗ ·
  *ships:* O(edit) *localized* branch. **Workload:** fork + **localized** edit
  (target O(edit)) **and** fork + **broadcast** edit (retract-all-of-R → honestly
  O(state); the claim is restricted to localized edits). **GC gate:** p99/max
  commit latency *observed by a concurrent writer during GC* ≤ the range-delete
  `consolidate` baseline, at n ≤ 10k with ≥ 2 retained branches; breach ⇒ redesign
  to incremental/epoch GC before B2 ships. `⟢ r2:` B2 benchmark must test
  fork-then-edit both ways; GC is a write-stall, bound the pause.
* **B3 — canopy** ⧗⧗⧗ · *ships:* wid-routed interest. **Workload:** many watchers,
  **mostly uninterested** in each commit; **gate:** watch cost sublinear in
  uninterested watchers **and** superset-equivalent to the pump on P5/P12.
* **B4 — full-Ent: Dsp coordinate transforms + backfollow/version-compare**
  ⧗⧗⧗ · **committed** (goal-justified, not workload-gated). Reworks the loaf
  traversal/measure core for dsp-threading; adds provenance queries. **Workload
  (for validation, not go/no-go):** relocation-heavy namespacing (O(edit)
  relocating copy) + a version-compare provenance query. New node frame → rebuild
  indexes from the log, **no migrator**.

**Cutover:** default `grmpl-ent`; **keep `grmpl-store` + the differential oracle
permanently as a CI reference** (three legs forever). Decommission of the *default*
LSM path is gated: **3 consecutive minor releases** with ent default and **zero
divergence across LSM, ent, and model** on the full P0–P15 suite each release, a
**named owner sign-off**, and one **exercised backup/restore** of a migrated store.
`⟢ r2:` decommission N/divergence/owner pinned; migrator one-way ⇒ backup gate;
three legs kept.

---

## 6. Success metrics
* **Full parity, tested per property:** measured trees, WID summaries (multi-order),
  DSP relocation (O(edit) localized copy), structural sharing, edition/branch DAG,
  canopy interest, backfollow/version-compare — each with a test proving the
  property it claims.
* **Features preserved:** full P0–P15 green on `grmpl-ent`; `grmpl run`/`showcase`
  unchanged; the seven laws proven; three-leg oracle green.
* **Measured, on the right baseline** (consolidated LSM, n ≤ 10k): flat
  preconditions (A1) within a ≤10% commit tax; selective trailing-column joins
  (A2); bounded commit/bytes/RSS (B1); O(edit) localized branch + bounded GC pause
  (B2); sublinear watch (B3); O(edit) relocating copy + provenance (B4).
* **Roadmap-aligned:** Tier A = P13; Tier B = P14+ (extends, doesn't fork).

---

## 7. Status & the few remaining tuning knobs (round 3: STABLE, 4/4)

Three review rounds (four independent critics: Xanadu fidelity, systems realism,
invariant preservation, migration risk) have converged: round 3 returned
**STABLE from all four**, with every round-1/2 CRITICAL/MAJOR verified resolved by
concrete, code-level specs (not re-assertion) and no regression introduced. What
remains are **tuning knobs to settle *during* B0**, not open design holes:

* **B1 budget from data, not guess.** The +25% / 3× / 2× envelope is a placeholder;
  reset it from B0's measured intern-write + node-write costs before B1 starts.
* **Canopy interest granularity.** Reuse core's single-column-int `KeyRange` (limits
  interest to one column) or introduce a richer core tuple-range type — decide at
  B0/B3; conservative-superset routing is correctness-safe either way.
* **`node_intern` as a second write-path index.** Bounded by the B1 budget and
  measured at B0 (§1); the one item to *quantify* early, not defer.

**Committed, not open:** the **three legs stay forever** (LSM + ent + model as a
permanent CI reference). Sunsetting the differential leg would re-create the
correlated-failure exposure it closes, so §0/§5's "three legs forever" is binding —
this is not a question to reopen. Likewise **B4 (Dsp + backfollow) is committed**
(goal-justified, not workload-gated); it is an open-ended ⧗⧗⧗ core-traversal rework
with no time-box, mitigated by the B0 dsp-hook reservation, the version-byte escape
hatch, rebuild-from-log (no migrator), and the fact that **Tier A + B1–B3 each ship
standalone value** even if B4 runs long.
