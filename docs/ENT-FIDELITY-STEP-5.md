# Ent fidelity, step 5: merges in the version trace

**Status:** landed on `main`, format v8 (a fresh-store cutover: each branch
now keeps a patch log, and a merged branch records a second parent).
**Question it set out to answer:** Gold's trace has merges.
`newSuccessorAfter:` makes a position with two parents, and `combine:` uses it
whenever two editions are combined (`udanax-top.st` 62999–63009, 2534).
grmpl's branches formed a tree. What should a merge mean in a store whose
writes are guarded patches, and what does it cost?

This closes gap G4's merge half (see [`ENT-FIDELITY-GAPS.md`](ENT-FIDELITY-GAPS.md)).

---

## 1. The semantics, and why

Gold's `combine:` is a disjoint union: it refuses content that overlaps. The
faithful translation would be a three-way merge refused wherever both sides
changed the same tuple.

grmpl chose instead to **replay one branch's patches onto the other,
re-checking each one's preconditions**. The reason is the concurrency model.
Every write in grmpl is a patch that commits only if its preconditions still
hold (`commit_if`), and a patch that loses a race is retried against the new
state, never silently merged (`CONCURRENCY.md`). Merging two branches is the
same situation at a larger scale: the other side's patches were decided
against a world that has since moved. Replaying them through the same
precondition check is what makes a merge re-decide rather than paper over a
lost race. So a guarded move of a thing the other side deleted conflicts,
while an unguarded insert simply adds its weight.

The DAG shape follows Gold: a merge is a **new branch with two parents**,
like Gold's `DagBranch`. Neither input branch changes.

## 2. What was built

* **A patch log per branch** (`Inner.patches`). Each edition of the branch's
  own records its preconditions and the relations it wrote; a graft records its
  block and shift. A commit's updates are not copied: each relation's Edition
  log already holds them, indexed by their place in the patch, so a patch is
  rebuilt in its original order. Consolidation retires patches with the
  versions they made.
* **A second parent** (`Branch.merged = (other, at, since)`). Lineage now
  follows both parents. The second parent's history counts as flowing in from
  edition `since`, where the replay ended.
* **`EntStore::merge(&self, other)`.**
  1. Find the patches to replay. They are the difference of the two lineages:
     for each branch in `other`'s history, its own editions past what `self`'s
     history already holds of it. No single merge base is needed.
  2. Order them branch by branch in id order (an ancestor before its
     descendants), and each branch's in edition order.
  3. Starting from `self`'s present, replay each one as an edition of the new
     branch. A commit's preconditions are checked against the merging state; a
     graft re-runs its own checks.
  4. Unite the context enfilades (catalog and schemas). A key bound two ways
     is a conflict.
  5. **All or nothing**: on the first failure, return
     `MergeOutcome::Conflict` naming the patch, and create nothing.
* **Copies are never replayed twice.** A merge branch's own editions are
  copies, and each records its original `(branch, edition)`. A later merge
  skips a copy whose original it reaches, and treats a patch it already holds
  as a copy as already present. A copy is replayed only when its original is
  out of reach: a fork taken partway through a merge, before the second parent
  flows in.

## 3. Results

**Laws** (`grmpl-ent/tests/merge.rs`, 13 tests):
* **The replay law**, against an independent model, over 60 random histories
  in which both sides contend for a small world. The merge equals `self`'s
  state with `other`'s patches applied in order, or it is refused at exactly
  the first patch whose preconditions fail in the model, leaving the DAG
  unchanged. Both outcomes occur more than ten times.
* **Only what is missing is replayed**: merging again replays nothing; a later
  merge replays only the new patches; merging an ancestor replays nothing; a
  grandchild's merge carries its parent's patches up to its fork; merging a
  merge applies nothing twice; a copy-of-a-copy is not replayed from its
  original.
* An ancestor's patch replays before a descendant's guarded patch that needs
  it.
* Grafts replay, and conflict when the target block is taken.
* Catalogs unite, and a name bound two ways conflicts.
* Consolidated history cannot be replayed.
* A merge survives a reopen, second parent and patch logs included.

The history laws from step 4 now include merges in their random histories (35
merges across the seeds), and backfollow and identity compare stay exact
across two-parent branches.

**Mutation testing:** 17 of 20 mutants are caught. The three survivors are
equivalent:
* a fork copying its parent's patch log;
* consolidation keeping retired patches. The replay range and the watermark
  check already exclude both kinds of patch, so they only waste space;
* recording a copy's immediate source rather than its true original. The
  chain of copies is always reachable through the lineage, so both resolve the
  same way.

Writing the mutants found two real gaps, both fixed:
* merging a merge replayed every patch twice, once from its original branch and
  once as the merge branch's copy;
* a patch held only as a copy was replayed again from its original.

**Cost** (durable store, 100k-row relation, release build):

| merge | time | frames written |
|---|---|---|
| replaying 100 guarded patches | 21 ms | 726 |
| replaying 1,000 guarded patches | 143 ms | 7,169 |
| refused at the last of 100 patches | 1.8 ms | 0 |
| refused at the last of 1,000 patches | 15 ms | 0 |

Each replayed patch is a full edition of the new branch, with its own version
roots: about 7 frames apiece, written in one batch with one `fsync`. A refused
merge costs the checks up to the conflict and writes nothing.

## 4. What step 5 says about the Ent

* **Merges fit the trace without strain.** A second parent and a lineage that
  follows it are all the DAG needed. Gold's trace was a partial order with
  two-parent positions, and grmpl's is now the same.
* **Replay needs the log to keep preconditions**, which Gold never needed,
  because Gold's combine compares content rather than re-deciding writes. That
  is the price of the concurrency model: a branch must remember *why* each
  write was allowed, not only what it wrote.
* **The lineage difference is the merge base.** With a log per branch and a
  lineage that says how much of each branch flows into a point, "what has
  `self` not seen" is a per-branch range. It handles criss-cross merges
  without a merge-base search, at the cost of tracking copies.
* **Materialized views will conflict.** A view's refresh cursor advances by a
  guarded commit, so two branches that both refreshed the same view conflict on
  merge. That is correct under replay (both advanced one cursor), and it means
  a merge-heavy world wants its views refreshed after merging, not before.

**Not done:** a trace position per derived operation. Gold gives every copy,
transform and combine of an edition a new trace position, because its
editions are values. grmpl's versions are branch commits, so its "derived
operations" are commits and already get editions. That half of G4 is a
representation difference rather than a gap.
