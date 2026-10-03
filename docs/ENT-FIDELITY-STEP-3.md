# Ent fidelity, step 3: context and derived enfilades

**Status:** landed on `prune-to-ent-design` (no format change; scopes and
materialized views are ordinary relations).
**Question it set out to answer:** can the last two members of `idea.md`'s
enfilade family — context inherited down scopes, and derived state kept in the
Ent — be built so the language actually uses them, and what do they cost?

The plan held these back until "the language has something that uses them",
because a mechanism with no consumer measures nothing. So step 3 built each with
its consumer:

* **Derived enfilades** as `materialized view`, used by both shipped worlds for
  the views every look and watch reads (`here`, `contents`, `world`).
* **Context enfilades** as `context` relations and `inherit` atoms, over the one
  kind of scope the language has: a block of entity ids. Shotengai's street,
  dungeon and mirror chamber each get an ambience, and every instanced dungeon
  carries its own.

---

## 1. The finding that shaped the derived half

The first version of `materialized view` stored the view's answer (its distinct
rows) and refreshed it with the engine's ordinary delta. Reads were fast, but
maintenance was not: a refresh after a one-row move took 1.9 s at 100k things,
no better than recomputing.

The cause was not the join. Joins were already being made incremental in this
step: `eval_delta` now reads the unchanged side of a join only where it matches
the change's keys, through a new `TraceStore::lookup` that the Ent answers by
index probes. A bare join's delta fell from 208 ms to 12.6 µs at 100k rows.

The cause was `distinct`. Every compiled view ends in `distinct` over a
projected join, and `distinct` has no delta rule over anything but a stored
relation: whether a row enters or leaves the view depends on how many ways it
was derived before and after, and nothing in a stateless engine knows that. So
`eval_delta` recomputed both ends, and the cheap join beneath it was wasted.

**That is what derived state is for.** The fix was to store the view's *linear*
form: the join's rows before `distinct`, each weighted by its number of
derivations, with the view's parameters as leading columns. Then:

| at 100k things | before (store the answer) | after (store the linear form) |
|---|---|---|
| refresh after a one-row move | 1.9 s | **9.7 ms** (two fsync'd commits) |
| `world`'s delta, for a watch | 272 ms | **12 µs** (`compare` on the copy) |
| `here(viewer)` read | 4.8 µs | 4.7 µs |

A row enters the view when its weight rises from zero and leaves when it falls
to zero, so a watch's delta is `TraceStore::compare` on the stored copy between
two editions, the existing O(edit) path. The weights are the state; the Ent is
where the state lives.

## 2. What was built

### Derived: `materialized view`

* **The copy.** A backing relation `view:<name>` holds the open linear form, and
  a shared cursor relation records the edition each copy reflects. The
  maintainer (`grmpl_proc::Materialized`, built earlier and never called until
  now) folds deltas into it under its own authority. Packages install the
  cursors in their bootstrap edition.
* **Exactness.** `Query::Materialized` means its plan. A read uses the copy only
  when the reader proves, through a new `EditionReader::touched_since`, that none
  of the view's inputs moved since the copy's cursor; a delta uses it only when
  it is current at both ends. Otherwise the plan is evaluated. So whether a
  refresh has run changes what an answer costs, never what it is. A test plants
  a row in the copy that the view could never produce: it is visible while the
  copy is provably current, and gone the moment an input moves.
* **One semantic change.** A refresh now advances its cursor even when the
  view's delta is empty. Before, a move that changed no row of `world` left the
  cursor behind, which left the copy *unprovably* current, so every read and
  delta fell back to evaluation. The original worry, chasing the cursor's own
  commit forever, does not arise on a store that can route: the next refresh
  sees that only the cursor moved.

### Context: `context` and `inherit`

* **The binding.** `context scopes` declares
  `scopes(first: Ent, last: Ent, key: Text, value: Any)`, binding `key` across
  the inclusive span `first..=last`.
* **The lookup.** `inherit scopes(e, "key", v)` gives `e` the value from the most
  specific span containing it: latest start, then earliest end. An inner scope
  overrides an outer one; retracting it lets the outer one show through.
* **The structure.** No new tree. The scope relation is an ordinary Fact tree
  keyed by span start, so it is versioned, forked, watched and persisted like
  everything else. Two things from earlier steps make it an enfilade rather than
  a table:
  * **It is a coordinate the dsps move.** Grafting a template block carries the
    scopes inside it, displaced with its rooms. That is DSP-inherited context in
    Gold's sense: what a subtree receives from where it sits travels with it.
    Spans are inclusive so a block's own scope lies inside the block and passes
    the self-containment check.
  * **The step-2 extents index it.** The bounds of the `first` and `last`
    columns are an interval tree's min-low and max-high, so "which spans contain
    this entity" (`read_containing`) is an extent search: 4–7 frames from a
    reopened store of 1k–100k scopes, 0.6 µs warm.

## 3. Results

Full tables are in `PERFORMANCE-ENT.md` §8.

* **Reads of a materialized view are flat:** `here(viewer)` takes 3.0 / 3.8 /
  4.7 µs at 1k / 10k / 100k things, against 0.35 / 3.0 / 47 ms evaluated.
* **Maintenance costs the change:** refresh is two fsync'd commits at every
  size, and a watch's delta is 3.5 / 7.2 / 12 µs against 1.4 / 14 / 272 ms.
* **The costs are real and up front:** materializing whole took 5.3 s at 100k
  things and stored 500k rows, because `here`'s open form pairs every two things
  in a room. The first incremental refresh builds the Arrangement it probes
  (361 ms at 100k), once.
* **A scope change is the expensive case:** `Inherit` is linear in its input
  while the scopes are unchanged, and recomputes both ends when one changes,
  because one binding can change any entity's answer.

Each mechanism was mutation-checked. All 20 mutants tried were caught by the
final tests:

* 5 on join maintenance and `lookup`;
* 8 on materialized reads, deltas and installs;
* 4 on scope stabbing and inheritance;
* 1 on shotengai's template list;
* 2 more surfaced as the tests were written.

Before this step's tests existed, two mutants had passed the entire 380-test
suite: dropping the join delta's `ΔA⋈ΔB` correction, and an Ent `lookup` that
returned nothing. The derived path had simply never been exercised.

## 4. What step 3 says about the Ent

**Strengths confirmed:**

* **Derived state belongs in the Ent.** A stateless engine cannot maintain
  `distinct` over a join from its changes; the derivation counts can. Kept as an
  ordinary relation, that state is versioned, persisted, forked, readable as-of,
  and comparable between editions in the size of the edit, all for free. The
  Ent's version compare is what turns the stored state into a cheap delta.
* **Context composes with displacement.** Scopes over entity blocks needed no
  inheritance machinery: the graft already carries a block's contents, and the
  scopes are contents. Inheritance is a lookup, and the extents make the lookup
  an interval stab.
* **Indexes become derived state.** Arrangements, built for trailing-column
  reads, turned out to be what join maintenance probes. `DESIGN.md` called them
  the physical realization of the derived enfilade; step 3 is where that became
  true.

**Weaknesses exposed:**

* **The open form can be much larger than the view.** A materialized view stores
  its parameters' whole join; `here` stores every pair of co-located things.
  Fidelity to "materialized view" means paying for every argument at once.
* **First use is expensive everywhere.** Whole materialization, and each
  Arrangement a refresh probes, are built on first use. Steady state is cheap;
  warming up is not.
* **Non-linear context.** A change to a scope can change any entity's
  inheritance, so `Inherit` falls back to a boundary recompute. Making it
  incremental would need the derived-state trick again: store each entity's
  current winner.

**Still not faithful, after step 3:**

* Scopes are entity blocks only. Namespace, authority or schema inherited down
  package scopes is not built, because packages do not nest.
* Version compare across a graft still merges row by row.
* The branch DAG has no merges.
* Green's 2-D enfilade is answered by two 1-D interval trees.

## 5. Open questions

* **Choose materialization automatically?** The numbers say: materialize what is
  read or watched often and whose open form is not much larger than its
  answers. That is checkable from the extents and counts already in the Ent.
* **Incremental `Inherit`** by storing each entity's winning scope, the same move
  that made `distinct` incremental.
* **Aggregates.** A materialized view with an aggregate reads from its copy but
  still takes its deltas by recompute. Storing per-group partial folds is the
  Reduce analogue of storing derivation counts.
