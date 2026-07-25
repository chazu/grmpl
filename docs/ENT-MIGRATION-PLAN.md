# Making the Ent the heart of grmpl — plan (v4, no-compromise)

**Mandate.** The end state is a **faithful Xanadu-`Ent` as the authoritative
substrate of the language — no compromises.** The store *is* a coordinated family
of persistent, measured, versioned **enfilades** over a shared **granfilade**
(node substrate), with full parity to Gold's `Ent`: WID upward measures, **DSP
coordinate transforms**, structural sharing, an edition/branch DAG (`fulltrace`),
a canopy interest index, and **backfollow/version-compare provenance**. There is
**no LSM log underneath it** in the end state.

**What is explicitly NOT a constraint (per the owner's directive):** backwards
compatibility, on-disk migration from the LSM, keeping `grmpl-store`, additive-only
trait evolution, "measured need" gating, and any performance hedge that would keep
the ent from being the heart. The LSM (`grmpl-store`) survives **only** as a
throwaway construction-time differential oracle and is **deleted at completion**.

**The one thing preserved (also per the original directive — "*while preserving
the systems features*"):** the **seven design laws** and the **P0–P15 language
features**. The bright line makes this free: the language and the process layer
depend only on the store **traits** (`EditionStore`/`TraceStore`/`Catalog`/
`SchemaCatalog`, evolved as the ent needs), never on the implementation — so a
fully ent-native store keeps every feature and law by construction, and the whole
existing test suite is the acceptance oracle.

> This v4 supersedes v3. v3 was hardened through three adversarial rounds to be a
> *safe, backwards-compatible* migration; its findings about **how to preserve the
> laws** carry over verbatim and are kept below. Its *compromises* — LSM-as-truth,
> ent-as-derived-index, three-legs-forever, additive hedging, measured-need gating
> — are removed. The reviewers' hard technical constraints are met **faithfully by
> the ent**, not by hedging (see §2).

---

## 1. The Ent is the store: the enfilade family

The authoritative representation is a family of measured enfilades sharing one
node substrate. Each is a first-class citizen (mirroring Gold's `Ent` = `oroots`
*content* + `fulltrace` *history*), all committed atomically together:

1. **Edition enfilade** — grmpl's linear commit-order delta log (a *differential
   extension* of the Ent, distinct from Gold's version-DAG `fulltrace`; see below),
   keyed by `(edition, submit_index)` and measured. Each update is an entry whose
   `submit_index` is the **immutable per-batch submit order stored as node
   payload** — *not* derived from tree rank — so `scan_updates(from,to)` returns
   the *raw, per-multiplicity, commit-ordered* `Update`s exactly as the contract
   requires, and that order is **invariant under rebalance, consolidate, and GC**.
   `scan_updates` runs along a **single totally-ordered branch ancestry** (one
   root-to-leaf DAG path); within any branch path `(edition, submit_index)` is
   total. The **`fulltrace`** proper is the Gold-faithful `DagWood` version/branch
   structure (§3), with per-node history membership — the log is *threaded through*
   it, not identical to it.
2. **Fact enfilade** — net-per-tuple state, tuple-keyed, measured (WID: count,
   key-bounds, Σdiff, dirty, interest). Serves `read_at`/`holds`/joins with WID
   pruning. Multiple **Arrangements** (one measured tree per column order) make
   trailing-column preconditions/joins sublinear.
3. **Context enfilade** — namespace/schema/placement inherited down scope covers;
   authority lives in the canopy endorsement lattice (Gold-faithful, not a dsp).
4. **Canopy enfilade** — standing interests as a flag OR-lattice + catch-all
   bucket, pointer-stable-rebalanced and refcounted (Gold's `CanopyCrum`).
5. **Derived enfilades** — persistent, shared materialized views / incremental
   query state (grmpl's differential extension of the Ent).

The common substrate (`idea.md` §1):

```
persistent measured action tree over a shared granfilade
  + stable node identities        (content-interned; structural sharing)
  + cheap split/join
  + WIDative subtree summaries     (upward measures; O(depth) range/measure)
  + DSPative coordinate transforms (downward displacement; O(edit) relocation/copy)
  + historical editions            (the fulltrace DAG; branches)
  + canopy indexes                 (interest routing)
```

**Granfilade.** A content-addressed node table (fjall as a node blob store), with
**structural sharing**: equal subtrees intern once; a new version allocates new
nodes only along the edited path; a **DSP** relocates a subtree by sharing its
nodes and storing one displacement (O(edit) *relocating* virtual copy — the thing
content-hashing alone cannot do). GC is reachability from retained roots ≥ the
watermark, native to the substrate.

**Node identity (three roles, three mechanisms — kept from v3, the one place a
naïve single id fails):** a **content key** interns nodes for
dedup/sharing/canonicity, computed as
`hash(kind, measure-relevant fields, child content_keys)` — **closed over child
*content-keys* only, with `phys_id` and allocation order excluded by
construction**, so two logically-identical subtrees built in different orders
intern to one node (structural sharing is order-independent; a property test
asserts one content key for the same logical subtree under two insertion orders).
A **monotonic `phys_id`** places nodes for locality; equality is at the
**leaf/query level**, not tree shape (heuristic balance-on-insertion means
internal shape legitimately varies — Gold does the same). The `phys_id` cursor
lives in a derived keyspace, never in the identity of the world.

---

## 2. Preserving the seven laws faithfully (non-negotiable; met by the ent, not by a hedge)

Each is a conformance test against the LSM oracle (during construction) **and** the
full P0–P15 suite on the ent every step:

* **`scan_updates` — raw, commit-ordered, per-multiplicity** → served by the
  **Edition enfilade** range query. `assert;retract;assert` is three entries, not a
  net. Byte-equal `Vec<Update>` incl. same-edition counter order. *(This is why the
  ent does not "net away" the log: the Edition enfilade IS the log, faithfully.)*
* **`read_at` — tuple-sorted net** → Fact enfilade in-order traversal; zero-weight
  tuples absent.
* **Patch–edition law — atomic next edition** → one transaction over the granfilade
  writes all touched enfilade nodes + the new roots (Edition, Fact, …) + the edition
  bump in a **single batch**, one `SyncAll` — with **node writes ordered before (or
  in the same batch as) the roots that reference them**, so a crash may strand
  unreachable nodes (GC reclaims them) but a root can never point at a
  non-durable node. Crash leaves all-old or all-new.
* **Replay / determinism** → the enfilades compute deterministic **logical content**
  (sort-before-hash; no HashMap-order, RNG, or pointer input to the content key).
* **`canonical_dump` / fork identity** → defined over the **logical projection in
  `(edition, submit_index)` order per rel** — precisely the concatenation of
  `scan_updates(rel, ZERO, current)` over all rels — **not** raw node bytes and
  **not** re-sorted by `(tuple, diff)` (which would drop same-edition order and
  stop witnessing Determinism). So "logically equal" ⟺ "`scan_updates`-equal for
  every range": tree-shape-independent yet commit-order-faithful — exactly the
  identity replay/fork must prove. *(Faithful: Gold guarantees content/version
  identity, never byte-identical node layout.)* P10 fork/replay tests are
  re-expressed over this projection.
* **Edition doors** → the watermark check runs first, before any index consult;
  `read_at`/`scan_updates`/`fork_edition` below the watermark **error**.
* **Authority law** → one commit, one authority domain (unchanged).
* **One serialization** → node/measure frames reuse `grmpl_core::wire::encode_tuple`
  for all payloads under a node-frame version; the value `FORMAT_VERSION` is the
  single value codec. Since there is no backwards-compat migration, the on-disk
  format is chosen once, cleanly, for the ent.

---

## 3. Full Ent parity (committed, faithful — the goal, no gating)

Grounded in the Gold source (`udanax-top.st`, verified):

* **WID measures** — a monoidal upward summary at every node; multiple Arrangements
  / box-region (`CrossSpace`/`BoxRegion`) measures for multi-column pruning.
* **DSP coordinate transforms** — `DspNode(child, displacement)` (`DspLoaf`
  `{myDsp, myO}`): shares the child, composes displacement down the descent →
  O(edit) relocation and virtual copy.
* **Structural sharing** — Okasaki path-copy over the interned granfilade.
* **Edition/branch DAG** — a `DagWood`-faithful TracePosition partial order +
  `BranchDescription`; `fork_edition(at) → opaque Edition`, `ancestry`/merge-base.
* **Canopy** — flag OR-lattice + `OtherEndorsements`-style catch-all, pointer-stable
  rebalanced, refcounted; conservative interest routing (superset of the pump).
* **Backfollow / version-compare** — `Loaf»compare:with:`-style provenance: "this
  content is the same as edition E, relocated" — the read side of the trace.
  **Trace membership is carried as a WID upward measure** (Gold's `HistoryCrum
  inTrace:`), so version-compare is `O(measure)` (subtree-pruned), not a scan.

---

## 4. Build order (bottom-up; the whole ent is the goal; each step validated)

The LSM is the differential oracle throughout construction, plus a **spec-derived,
mutation-tested reference model** (from DESIGN/`idea.md`), plus the full P0–P15
suite on the ent. No phase is gated on "measured need" — the goal is the ent.

* **E0 — granfilade + conformance harness.** Content-interned node table on fjall;
  a persistent, measured, ordered tree with structural sharing; the store-contract
  law oracle (commit/read_at/scan_updates/commit_if/watermark/consolidate + catalog
  + schemas) running against LSM (oracle) and a naïve ent. Freeze the node/measure
  frame; measure intern-write cost.
* **E1 — Edition + Fact enfilades = a store.** `grmpl-ent` implements the store
  traits: Edition enfilade serves `scan_updates` (raw/ordered), Fact enfilade serves
  `read_at`/`holds` (net/sorted), one atomic multi-enfilade commit. **Green on the
  full P0–P15 suite.** *This is the first ent-native store.*
* **E2 — Arrangements + WID pruning.** Per-column measured trees; trailing-column
  `range_at`/`holds` sublinear; `commit_if` via the Fact point path.
* **E3 — edition/branch DAG + `fork_edition` + reachability GC.** Structural
  sharing across versions; O(edit) localized branch; GC serialized with commit.
  `canonical_dump` (logical projection) fork/replay identity holds.
* **E4 — canopy.** Interest lattice + conservative routing; `on watch` over the
  canopy; pump-equivalent on P5/P12.
* **E5 — context enfilade.** Namespace/schema/placement down scopes; catalog/schema
  remain durable, GC-exempt.
* **E6 — DSP coordinate transforms + backfollow.** The loaf-algebra rework: dsp
  threading through traversal/measure; provenance/version-compare. **Full parity.**
* **E7 — cut over and delete the LSM.** Default the runtime to `grmpl-ent`; once the
  ent is green on the full suite + the model for a soak window, **delete
  `grmpl-store` and the differential oracle.** End state: ent-native, single format,
  no LSM.

---

## 5. Down-payment already shipped
**A1 (committed `4911e6d`)** replaced `commit_if`'s O(history) `holds_at` scan with
an O(1) net-weight index — the Fact enfilade's *net-state role* in a first, in-memory
cut (precondition curve now flat in history: 10.5M→4.06M ns/probe at hist=10k;
full suite green). It is the seed of the Fact enfilade; E1 makes it persistent and
measured and adds the Edition enfilade beside it.

---

## 6. Success = a faithful Ent, laws intact
* The store **is** the enfilade family (§1); no LSM in the end state.
* Full Gold-`Ent` parity, each property tested (§3).
* Seven laws + P0–P15 green on `grmpl-ent`; `grmpl run`/`showcase` unchanged.
* `grmpl-store` deleted at E7; single ent-native on-disk format.
