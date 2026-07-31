# Closing the remaining gaps — *use* the Ent, don't simulate it (v5)

**Mandate (unchanged from [v4](ENT-MIGRATION-PLAN.md)).** The Ent is the
authoritative substrate of the language: a coordinated family of persistent,
measured, versioned **enfilades** over a shared **granfilade**, with no LSM log
underneath it in the end state.

**What this document adds.** v4 declared every distinctive Xanadu-`Ent`
structural component "present and correct", with three deferred *performance*
items. An audit of `crates/grmpl-ent` against that claim finds the components are
present as **modules**, but several of them are not the substrate the world runs
on — they are std-collection models of the idea, sitting beside the store rather
than under it. Three of the eight modules (`canopy`, `context`, `dsp::DspEnf`)
are referenced by **nothing outside their own unit tests**; the Fact enfilade is
not persisted at all (it is *replayed from the log* on open); the branch DAG does
not survive a reopen; the catalog and schema registry — two of the four
invariants `CLAUDE.md` names load-bearing — exist only on the LSM.

So the remaining work is not "three optimizations." It is: **make the ent
load-bearing everywhere it is currently simulated**, then finish the cutover.

---

## 0. The test: what counts as *using* the Ent

Every item below is graded against three questions. A component passes only when
all three answer yes.

1. **On the substrate.** Is its state a `Tree` over the granfilade — or a
   `Vec`/`HashMap`/`BTreeMap` *beside* one? A model of an enfilade is not an
   enfilade: it does not version, does not share structure, does not persist, and
   does not GC.
2. **Load-bearing.** Does something in the *running* system go through it — query
   evaluation, the commit path, the watch pump, `grmpl run`, `grmpld`? Concretely:
   if the module were deleted, would a test outside that module fail?
3. **Measured.** Is the `O(log n)` / `O(measure)` claim *asserted* — by counting
   nodes visited, rows materialized, or views evaluated — or only asserted in a
   docstring? Prose is not a measurement.

Question 3 needs infrastructure that does not exist yet, so it comes first (G-0a).

---

## 1. Audit: claimed vs. actual

| Component | v4 / book claims | Actual code | Verdict |
|---|---|---|---|
| Enfilade primitive (`tree.rs`) | persistent, measured, weight-balanced | exactly that, oracle-tested | ✅ used |
| Granfilade (`granfilade.rs`) | content-addressed, structural sharing on disk | that, but keys come from `DefaultHasher` and every commit re-walks the whole tree | ⚠️ see G-0b, G-1 |
| Edition enfilade (`store.rs`) | raw commit-order log with immutable `submit_index` | that, persisted per commit | ✅ used |
| Fact enfilade (`store.rs`) | "one persistent root per edition, structurally shared", persisted with the roots in one batch | **not persisted per commit** — only a checkpoint at the watermark; `open` rebuilds every above-watermark root by **replaying the Edition log** (`store.rs:327‑350`) | ❌ the log *is* the source of truth |
| Rel / version index | "the family of enfilades" | `HashMap<RelId, BTreeMap<u64, FactTree>>` (`store.rs:55‑57`) | ❌ simulated |
| WID measures (`measure.rs`) | "count, key-bounds, Σdiff, dirty, interest"; trace membership as an upward measure | only `Count` exists | ❌ mostly absent |
| Backfollow / version-compare | "`O(measure)`, subtree-pruned, not a scan" | `Tree::diff` materializes **both** trees in full (`tree.rs:179‑180`); the only pruning is one root-level `Arc::ptr_eq` | ❌ it is a scan |
| Branch DAG (`dag.rs`) | the durable `fulltrace` | in-memory `BTreeMap`; `EntStore::open` always constructs `Dag::new()` (`store.rs:73,86`) — ancestry does not survive a reopen | ❌ not durable |
| Structural-sharing fork | `O(edit)` virtual copy | in-memory only: `fork_at` returns a store with `gran: None` (`store.rs:202`), so forking a *durable* store silently yields a non-durable one | ⚠️ half |
| Canopy (`canopy.rs`) | "`CanopyCrum` with interest as the upward measure", "`on watch` over the canopy" | `Vec` + segment tree **rebuilt whole on every register/unregister** (`canopy.rs:187‑195`); **zero callers** — `grmpl_proc::watch::pump` re-evaluates the view every pump and never consults it | ❌ simulated *and* unused |
| Context (`context.rs`) | the context enfilade; namespace/schema/placement inherited down scopes | `BTreeMap<Scope, HashMap<String, Value>>`; **zero callers** | ❌ simulated *and* unused |
| DSP (`dsp.rs`) | `O(1)` relocation; instancing as virtual copy | `Dsp` (the value algebra) is used; `DspEnf` — the actual O(1) shared overlay — has **zero callers**. `instance_template` materializes every template fact (`store.rs:271‑283`) | ⚠️ algebra used, overlay unused |
| Catalog / SchemaCatalog | durable, append-only, ent-native | **`EntStore` implements neither**; only `FjallStore` does (`grmpl-store/src/lib.rs:478,518`). `grmpl run` therefore passes `NoSchemas` everywhere | ❌ absent |
| Derived enfilades | first-class member of the family (v4 §1.5) | not built, and not listed as deferred; `grmpl-diff` arrangements are an in-memory memo keyed by `(Arc::as_ptr, edition)` that by its own docstring cannot outlive an advance | ❌ absent |
| Cutover (E7) | "default the runtime to `grmpl-ent`" | only `grmpl run`. `grmpld`, `grmpl showcase`, `grmpl-bench`, and essentially every `grmpl-proc` / `grmpl-session` test still open a `FjallStore` | ⚠️ partial |

Two claims are not merely unimplemented but *unachievable as stated*, and need a
decision rather than a task:

* **"Content keys are order-independent"** (v4 §1, node identity). The tree is
  weight-balanced with a shape that depends on insertion order, and the content
  key is `hash(lck, rck, key, val)`. Two histories reaching the same logical map
  therefore get **different** root keys. Order-independence requires a
  canonically-shaped tree. See **G-2b** — this is a real fork in the design, and
  it gates cross-branch dedup and any structural `canonical_dump`.
* **"`DefaultHasher` has fixed keys, so the key is deterministic across
  processes"** (`granfilade.rs:30`). True within one build; `std` explicitly does
  not specify the algorithm or guarantee it across Rust releases. See **G-0b**.

---

## 2. The work

Each item states the **gap**, **why it is simulation**, **the change**, and the
**acceptance** that turns the claim into a test. Dependencies are noted; the
waves in §3 sequence them.

### Wave 0 — make claims measurable, and fix two correctness items

#### G-0a. An operations-counter harness

*Gap.* Every sublinear claim in the ent is prose. There is no way to fail a test
when a "`O(log n)` walk" quietly becomes a scan.

*Change.* A `#[cfg(feature = "count-ops")]` (or test-only) counter in
`grmpl-ent`: node frames **encoded**, node frames **loaded**, tree nodes
**visited**, and — above the line, in `grmpl-diff`/`grmpl-proc` — rows
**materialized** and views **evaluated**. Zero cost when off.

*Acceptance.* Every later item's acceptance criterion is expressed against these
counters. This item's own acceptance is that the existing E2b range-read test
asserts `nodes_visited = O(result + log n)` rather than merely returning the
right rows.

#### G-0b. Content keys that are stable and collision-resistant

*Gap.* `hash128` is two salted passes of `std::collections::hash_map::DefaultHasher`
(`granfilade.rs:314‑326`). Two problems: (a) `std` does not specify the algorithm
across releases, so a granfilade written under one toolchain may hash differently
under another — every node re-stored, structural sharing silently lost, and a
`load` of a key written by the old build failing outright; (b) `DefaultHasher::new()`
uses *known, fixed* keys, so it is not collision-resistant against anyone who can
choose content — and in a MOO, content is player-supplied. A content-key collision
in a content-addressed store is silent aliasing of one fact onto another.

*Change.* Vendor a specified hash in-crate under the `wire::FORMAT_VERSION`
umbrella — BLAKE3 (or an explicitly-implemented SipHash-1-3 with documented keys
if a dependency is unwanted) — truncated to a stated width, with the width and
construction named in the format doc. Bump the node-frame version.

*Acceptance.* A fixed vector test: a known tree hashes to a hard-coded key,
checked in — so any future change to the hash breaks loudly. A property test that
a store written and reopened round-trips every node.

#### G-0c. Doc-truth pass

*Gap.* `ENT-MIGRATION-PLAN.md` §Implementation-status, `docs/book/src/grmpl/implementation.md`,
and `docs/book/src/future/index.md` state as landed several things the audit above
shows are not: backfollow as `O(measure)`, trace membership as a WID measure, the
canopy as an enfilade wired to `on watch`, the context enfilade as carrying
namespace/schema, and order-independent content keys.

*Change.* Correct those passages to describe what the code does, and point at
this plan for what it will do. (Doing this *first* keeps the docs honest while
the waves land, rather than retroactively.)

*Acceptance.* Every "landed" claim in the book has a named test behind it.

#### G-0d. Measured version-compare

*Gap.* `EntStore::compare` → `Tree::diff`, which does
`self.iter().collect()` on both sides (`tree.rs:179‑180`) — `O(n + m)` regardless
of how little changed. The claimed "`O(measure)`, subtree-pruned" backfollow does
not exist.

*Why it is simulation.* The result is right; the mechanism is the one an array
would use. The whole point of a shared measured tree is that an unchanged subtree
is recognisable *without descending into it*.

*Change.* Rewrite `diff` as a co-recursive descent that prunes at **every** node,
not just the root: `Arc::ptr_eq` for in-memory sharing, memoized content key for
cross-version sharing (after G-1), and key-bounds for disjoint spans (after G-4).

*Acceptance.* Over a 10 000-row relation with one row changed between two
editions, `compare` visits `O(log n)` nodes (G-0a counter), and the existing
version-compare test in `tests/language.rs` stays green.

*Depends on.* G-0a. Improves further with G-1 and G-4.

### Wave 1 — the Ent becomes the store for real

#### G-1. Path-only persistence (memoized content keys)

*Gap.* `persist` calls `collect_tree`, which re-serializes **every node of the
whole tree** to recompute content keys, on **every commit** (`granfilade.rs:271‑289`,
`store.rs:299`). On-disk *growth* is path-sized, but per-commit *work* is `O(n)`.

*Why it matters most.* This is the single reason the ent cannot yet replace the
LSM in `grmpld`, the showcase, or the bench — and therefore the reason E7's
cutover stalled at `grmpl run`. Everything downstream in this plan is cheaper
once a commit costs `O(log n)`.

*Why v4's two blockers dissolve.* (i) *Pointer-ABA* — nodes are immutable and
`Arc`-held, so a node's content never changes; caching its key **in the node** is
memoizing a pure function, and there is no A-B-A to observe. Use a
`OnceLock<ContentKey>` in `Node`. (ii) *Layering break* — the tree must not name
the granfilade, and it does not need to: the `Persist` trait already exists, so
`granfilade::collect_nodes` supplies the encoder and fills the node's `OnceLock`.
The remaining subtlety is that a memoized key does **not** imply the node is on
*this* granfilade's disk (an in-memory fork could have built it), so the
granfilade keeps a `HashSet<ContentKey>` of keys known-present in this instance,
populated on `write` and on `load`. A walk stops at the first node whose key is
memoized **and** known-present.

*Change.* `OnceLock<ContentKey>` per node; encoder supplied by the granfilade;
known-present set on the granfilade; `collect_nodes` prunes.

*Acceptance.* Commit `N` times into a `K`-row relation; **frames encoded per
commit is `O(log K)` and flat in `K`** (G-0a counter). Existing conformance,
reopen, and GC suites stay green. A `grmpl-bench` axis shows commit cost flat in
relation size where today it is linear.

*Depends on.* G-0a, G-0b.

#### G-2. The Fact enfilade is persisted; the log stops being the source of truth

*Gap.* On commit, `persist` writes only the touched **Edition-log** roots plus the
clock; Fact roots are written only at `consolidate` (`store.rs:287‑316`). `open`
therefore loads the watermark checkpoint and **replays the entire log tail**,
folding each update back into a Fact root (`store.rs:327‑350`).

*Why it is simulation.* That is an append-only log with checkpoint-and-replay
recovery — precisely the LSM shape the mandate says must not survive underneath
the ent. The versioned Fact roots that make as-of reads `O(depth)` are a
*derived, in-memory* artifact, rebuilt in `O(history)` on every open.

*Change.* Write each touched relation's new Fact root in the same batch as the
Edition-log root and the clock bump (nodes before roots, one `SyncAll`, as today).
`open` becomes a root lookup per relation — no replay, no fold.

*Acceptance.* After `N` commits and a reopen, **zero** replay folds occur
(counter), and open time is flat in history. Conformance against `FjallStore`
across reopen (`tests/conformance.rs`) stays green.

*Depends on.* G-1 (without it, persisting a second enfilade per commit doubles an
already-`O(n)` cost).

#### G-2a. The relation and version indexes become enfilades

*Gap.* `Inner { fact: HashMap<RelId, BTreeMap<u64, FactTree>>, log: HashMap<RelId, LogTree> }`
(`store.rs:51‑58`). The "family of enfilades" is held together by std maps: the
relation directory is a `HashMap` (unordered — the one thing the Determinism
invariant warns about) and the per-relation version chain is a `BTreeMap`.

*Change.* A **Rel enfilade** `Tree<RelId, RelRoots, …>` as the directory of live
relations, and a **Version enfilade** `Tree<Edition, FactRoot, …>` per relation
replacing the `BTreeMap`. Then `Inner` holds one root; as-of is a `≤` search in a
measured tree; the whole store is addressable by a single content key.

*Acceptance.* `Inner` holds exactly one root. "How many relations", "relations in
this id range", and "how many editions since E" are answered as measures. Two
stores driven by the same operation sequence produce the same top-level content
key (see the caveat in G-2b).

*Depends on.* G-2.

#### G-2b. **Decision:** canonical tree shape, or drop the order-independence claim

*The fork.* v4 §1 asserts content keys are order-independent ("a property test
asserts one content key for the same logical subtree under two insertion
orders"). With a weight-balanced tree this is false: shape depends on insertion
order, and the key is closed over children's keys. Two options:

* **(a) Drop the claim.** Content keys identify a *shape*. Structural sharing
  still works within a version lineage (a path-copy shares every untouched
  subtree). Cross-history dedup and a structural `canonical_dump` do not.
* **(b) Adopt a canonically-shaped tree** — a hash-treap keyed on `hash(key)`, or
  a deterministic B-tree with content-determined splits — so *equal content ⇒
  equal shape ⇒ equal root key*, regardless of history.

*Recommendation: (b).* It is contained (only the balance rule in `tree.rs`
changes; measures, persistence, and the store are untouched), it makes dedup real
across branches — which is exactly what G-8's durable fork wants — and it is what
Gold's interning implies. The cost is losing the current shape's cache behaviour,
which G-0a can measure before and after.

*Acceptance.* The property test v4 already specifies: build the same logical map
by two different insertion orders, assert one content key. Plus a
`canonical_dump` defined over the root key rather than over a re-serialized
projection.

*Depends on.* G-1. Should be decided **before** G-8 and G-10.

### Wave 2 — the shelfware modules become load-bearing

#### G-3. WID measures that mean something

*Gap.* `Count` is the only `Measure` in the crate (`measure.rs:27`), though v4 §1
specifies "count, key-bounds, Σdiff, dirty, interest" and §3 specifies trace
membership as an upward measure.

*Change.* Make `Measure` composable (a tuple impl, so a tree can carry several
without new tree code), then add:

* `KeyBounds` (min/max key) — lets `range`/`diff` reject a subtree with one
  comparison instead of descending both boundary spines;
* `SumDiff` (Σ weight) — makes `commit_if` preconditions and `count`/`sum`
  aggregate views answerable **from the measure**, without materializing rows;
* `TraceMembership` on the Edition enfilade — the `HistoryCrum inTrace:` measure
  that makes backfollow subtree-pruned.

*Acceptance.* An aggregate view over a key range returns without materializing
any row (rows-materialized counter = 0). `commit_if` on a precondition visits
`O(log n)` nodes and calls `read_at` zero times.

*Depends on.* G-0a.

#### G-4. The canopy becomes an enfilade — and the watch pump routes through it

*Gap.* Two separate failures. (i) The canopy is a `Vec` with a segment tree
rebuilt in full on every register/unregister (`canopy.rs:187‑195`): not
persistent, not versioned, not shared, not GC'd. (ii) **Nothing calls it.**
`grmpl_proc::watch::pump` runs `eval_delta` over its view on every pump
(`watch.rs:193`) regardless of whether the commit could possibly have touched it.

*Change.*
* Hold interests in a `Tree<(RelId, Tuple), Interest, IntervalMeasure>` whose
  measure is `max hi` joined with the endorsement OR-lattice. Register/unregister
  become `O(log n)` persistent inserts; the canopy versions with the world and
  persists in the granfilade like every other enfilade.
* Wire it **without crossing the bright line**, using the pattern E2b already
  established for `read_range`: declare an `InterestIndex` trait in `grmpl-core`
  (`register` / `unregister` / `route`) whose default implementation routes
  everything (conservative, therefore correct for any store), and override it in
  `EntStore` with the measured canopy. `grmpl-proc`'s pump then asks the trait
  "did `(from, to]` touch this watch's inputs?" and skips `eval_delta` on a no.

*Acceptance.* With 1 000 installed watches and a commit touching one key, the
pump evaluates **1** view, not 1 000 (views-evaluated counter). A seeded property
test asserts the routing is a **superset** of the truly-changed set (the
Snapshot–stream law: conservative, never a subset). `grmpl-proc/tests/on_watch.rs`
— the existing P5 law oracle — runs green over `EntStore`.

*Depends on.* G-0a, G-3.

#### G-5. The context enfilade carries the catalog and the schema registry

*Gap.* Two failures that share one fix. (i) `Context` is a
`BTreeMap<Scope, HashMap<String, Value>>` (`context.rs:24`) with zero callers.
(ii) `EntStore` implements neither `Catalog` nor `SchemaCatalog` — so the durable
name→`RelId` map and the edition-versioned schema registry, both named
load-bearing invariants in `CLAUDE.md`, exist **only** on the LSM, and `grmpl run`
consequently passes `NoSchemas` at every commit boundary (`run.rs:624,745,771,773,782`).

*Why one fix.* v4 §1.3 already says what the context enfilade is for:
"namespace/schema/placement inherited down scope covers." A name→id binding and a
schema version *are* context bindings at a scope. Building the context enfilade
for its own sake and then building a separate catalog would be two simulations
where one substrate is called for.

*Change.* Make `Context` a real `Tree<ScopePath, Bindings, …>` persisted in the
granfilade, then implement `Catalog` and `SchemaCatalog` for `EntStore` on top of
it. Its roots become GC roots (catalog and schemas are GC-exempt by construction).

*Acceptance.* Lift `grmpl-store`'s catalog and schema law tests into a shared
conformance harness and run them against **both** stores. `grmpl run` uses the
real schema registry instead of `NoSchemas`, and schema enforcement at the commit
boundary is exercised in the MOO. Reopen preserves name→id bindings and
`schema_at` answers.

*Depends on.* G-2, G-2a.

### Wave 3 — spend the structure

#### G-6. Durable fork sharing one granfilade, and a persistent DagWood

*Gap.* `fork_at` returns a store with `gran: None` (`store.rs:202`) — forking a
durable store silently produces a non-durable one — and the `Dag` is rebuilt as
`Dag::new()` on every `open` (`store.rs:73,86`), so branch ancestry does not
survive a reopen. GC roots only `log:` and `ckpt:` prefixes (`granfilade.rs:222`),
so it would collect a fork's roots the moment they existed.

*Change.* All branches in one granfilade with roots namespaced by branch
(`root:{branch}:{rel}:{edition}`); the DagWood persisted as its own enfilade; GC
rooted from every live branch. `fork_at` on a durable store becomes `O(#rels)`
meta writes sharing every node on disk.

*Acceptance.* Fork a durable store, commit to both, reopen both: node count grows
by `O(edit)`, not `O(state)` (assert the `node_count` delta). Ancestry survives
reopen. GC after a fork collects nothing reachable from either branch. The P10
fork/replay identity holds over the logical projection.

*Depends on.* G-2, G-2a, and the G-2b decision.

#### G-7. DSP overlays that are actually `O(1)` — and instancing that uses them

*Gap.* `DspEnf` implements exactly the right thing (a shared tree plus a
displacement, with the dsp threaded through the WID walk) and is used by
**nothing**. `instance_template` instead reads every template fact and commits a
relocated copy (`store.rs:271‑283`) — `O(template)` per instance, with no sharing.

*Change.* Give the Fact enfilade a displaced-overlay node (`DspNode(child, dsp)`
in `Tree`, composing the displacement down the descent — `DspEnf`'s `range` and
`measure_range` lift directly), so `instance_template` records **one
displacement** against a shared root, with copy-on-write when an instance
diverges.

*Acceptance.* `enter vault` for `N` players adds `O(N)` nodes in total, not
`O(N · template)` (node-count assertion). The existing MOO instancing test in
`tests/language.rs` stays green. A property test: a displaced overlay reads
identically to the materialized copy under random churn, including after
divergence.

*Depends on.* G-2, G-3.

#### G-8. Derived enfilades — the missing family member

*Gap.* v4 §1.5 lists Derived enfilades as a first-class member of the family;
they are neither built nor listed as deferred. `grmpl-diff`'s arrangements are an
in-memory memo keyed by `(Arc::as_ptr(node), edition)` which, by its own
docstring, cannot outlive an advance.

*Change.* Persist arrangements as Fact-like enfilades keyed by the query's
content key and versioned by edition, sharing the granfilade — so a materialized
view survives reopen and shares structure across editions exactly as the Fact
enfilade does. This is grmpl's differential extension of the `Ent`, and the last
unbuilt member.

*Acceptance.* A materialized view survives close/reopen with **no**
recomputation. Incremental maintenance touches `O(delta · log n)` nodes.

*Depends on.* G-1, G-2a, G-2b.

#### G-9. Multi-order Arrangements

*Gap.* The one v4 deferral that is genuinely just a deferral: `RangeRel` prunes
on the lead column only, so trailing-column preconditions and joins run correct
but unpruned.

*Change.* With G-8 in place this is "N measured trees per relation, one per column
order", plus a `grmpl-lang` lowerer change to emit `RangeRel` from a
trailing-column equality as it already does from a lead-column one.

*Acceptance.* A trailing-column-keyed view prunes at the source (nodes-visited
counter). The `grmpl-bench` arrangement axis shows sublinear where it is linear
today.

*Depends on.* G-8.

### Wave 4 — finish E7

#### G-10. Cut everything over, then delete the LSM

*Gap.* Only `grmpl run` is on the ent. `grmpld` (`grmpld.rs:23`), `grmpl showcase`
(`showcase.rs:21`), `grmpl-bench` (`scenarios/mod.rs:22`), and essentially every
`grmpl-proc` / `grmpl-session` test still open a `FjallStore`. The "full P0–P15
suite green on the ent" claim is therefore not yet demonstrated: the suite mostly
does not run on the ent.

*Change.* Parameterize the `grmpl-proc` / `grmpl-session` / `grmpl-cli` test
harnesses over the store trait and run the whole suite against **both** stores —
which is the honest form of "the LSM is the differential oracle." Then flip the
defaults in `grmpld`, `showcase`, and `bench`. Then, after a soak window with the
ent green on the full suite and at-or-better on the bench, delete `grmpl-store`
and the oracle.

*Acceptance.* Full P0–P15 green on `EntStore`, not just on `FjallStore`.
`grmpl-bench` on the ent at or better than the LSM baseline (G-1 is the
precondition). Then the deletion commit, and a single ent-native on-disk format.

*Depends on.* Everything above; G-1 and G-5 are the hard preconditions.

---

## 3. Sequencing

```
Wave 0  G-0a counters ─┬─ G-0b stable content hash
                       ├─ G-0c doc-truth pass
                       └─ G-0d measured version-compare
                              │
Wave 1  G-1 path-only persistence  ← the unlock for everything below
          └─ G-2 Fact roots persisted (open stops replaying the log)
               └─ G-2a Rel + Version enfilades
                    └─ G-2b DECISION: canonical shape?   (gates G-6, G-8)
                              │
Wave 2  G-3 the WID measure family
          ├─ G-4 canopy as enfilade + pump routes through it
          └─ G-5 context enfilade carries Catalog + SchemaCatalog
                              │
Wave 3  G-6 durable fork + persistent DagWood
        G-7 DSP overlay instancing
        G-8 Derived enfilades ── G-9 multi-order arrangements
                              │
Wave 4  G-10 full cutover, soak, delete grmpl-store
```

**Order rationale.** G-1 comes first among the substantive items because it is
the reason the cutover stalled: while a commit is `O(n)`, the ent cannot replace
the LSM anywhere that cares about throughput, and every later item that persists
*more* per commit makes that worse. G-2 immediately after, because a store whose
recovery is log replay is not the Ent regardless of what the modules are called.
Wave 2 is where the three shelfware modules become load-bearing — the direct
answer to "use it, don't simulate it". Wave 3 spends the structure on the
capabilities that only an Ent can offer cheaply. Wave 4 is the deletion the
mandate has been pointing at since v4.

Each item lands as its own commit, with the invariant checks from `CLAUDE.md`
(`cargo build`, `cargo test`, `cargo clippy --all-targets`, iroh off) green, plus
its own named acceptance test.

---

## 4. Definition of done

The Ent is *used*, not simulated, when:

* every module in `crates/grmpl-ent/src` has a caller outside its own unit tests;
* no `Vec`/`HashMap`/`BTreeMap` stands in for an enfilade in the store's state;
* recovery is a root lookup, not a log replay — there is no LSM shape underneath;
* every sublinear claim in the crate is asserted by a counter, not a docstring;
* the catalog and the schema registry are ent-native, and `grmpl run` uses them;
* the full P0–P15 suite runs green on `EntStore`;
* `grmpl-store` is deleted, and there is one ent-native on-disk format.
