# DRed — incremental deletion for recursive views

Status: **implemented** (`grmpl-diff::recursive`, `IncrementalFixpoint`). This
document explains what DRed is, why grmpl needs it, how the implementation works,
and the one precondition it rests on — **linear recursion** — including whether we
should add a guard/diagnostic that enforces that precondition. It complements the
inline module docs in `crates/grmpl-diff/src/recursive.rs` and the summary in
`DESIGN.md` §14; read this for the *why*, read those for the *what*.

---

## 1. The setting: recursive views must be maintained, not recomputed

grmpl's world model is relational (`DESIGN.md` §3). Delegation/inheritance is a
**recursive view** — the canonical example from `idea.md` §3 is `implements`:

```
implements(entity, behavior) :=
      direct(entity, behavior)                         -- base case (init)
    | prototype(entity, parent), implements(parent, behavior)   -- recursive step
```

An entity implements a behavior if it has it directly, or if it inherits it from
a prototype that (recursively) implements it. In the engine this is a
`Query::Iterate { init, step }` whose least fixpoint is
`distinct(init ∪ step(Recur))`, evaluated to convergence
(`crates/grmpl-diff/src/query.rs`).

The **Attention law** (`DESIGN.md` §12) says reactivity is a *maintained query*:
when base facts change we must produce the signed delta to every watching query,
not force observers to re-poll. For a non-recursive view that is mechanical (the
differential operators push deltas through). For a **recursive** view it is the
one genuinely hard core piece — flagged as such in `DESIGN.md` §10, risk 1.

The correct-but-expensive baseline already exists: `eval_delta`'s `Iterate` arm
does a **boundary recompute** — evaluate the whole fixpoint at the old edition and
the new edition, and subtract (`Δ = fixpoint(to) − fixpoint(from)`). It is always
right, including under retraction, but it throws away all prior work on every
change. On a large world with a small edit that is wildly wasteful.

`IncrementalFixpoint` replaces that with a **materialized fixpoint maintained in
place**. The question DRed answers is: *given the previous fixpoint and a base
change, how do we compute the new fixpoint without recomputing from ∅?*

---

## 2. The monotone / non-monotone split (CALM in miniature)

Base changes come in two flavours, and they are not symmetric:

* **Monotone — insertions only.** The fixpoint can only grow. We warm-start
  semi-naïve iteration from the previous fixpoint and let only the *new*
  derivations propagate (`grow` in `recursive.rs`). This is cheap: work is
  proportional to what actually changed, not to the size of the world. This is
  the CALM-monotone case (`DESIGN.md` §10 #4): monotone logic needs no
  coordination and no rework.

* **Non-monotone — any retraction.** The fixpoint can shrink, and *this is the
  hard direction*. When you retract a base fact you cannot simply "stop deriving"
  — you must find every derived fact that depended on it, **and** be careful not
  to delete facts that had *other*, still-valid derivations. Deletion in a
  recursive setting is where naïve approaches go wrong.

Before this work, the non-monotone case fell back to the boundary recompute. DRed
replaces that fallback with a genuinely incremental deletion algorithm.

---

## 3. What DRed is

**DRed = Delete and Re-derive** (Gupta, Mumick & Subrahmanian, *"Maintaining
Views Incrementally"*, SIGMOD 1993). It is the classic algorithm for maintaining
a recursive (Datalog) view under deletion. It runs in two passes:

1. **Overdeletion (delete).** Delete *every* derived fact that had **a**
   derivation using a deleted fact — even if that fact also had other
   derivations. This deliberately deletes too much (hence "over"): it is an
   over-approximation of what truly must go.

2. **Rederivation (re-derive).** For each over-deleted fact, check whether it is
   still derivable from what survived; if so, put it back.

The reason it deletes-too-much-then-repairs, rather than trying to delete exactly
the right set, is **derivation cycles**. Consider two facts that support each
other. If you only delete facts that have lost *all* support, neither is ever
deleted — each still "supports" the other — even after the real grounding that
justified both is gone. Overdeletion breaks the cycle by keying on *broken
derivations* rather than *loss of support*; rederivation then rebuilds whatever
was genuinely still grounded. This is exactly the failure a "still one-step
supported?" test falls into, and exactly why DRed exists.

---

## 4. How grmpl's DRed works

The maintainer's `advance(store, to)` decides the path by asking `is_monotone`
whether any referenced base tuple was retracted between the current edition and
`to`:

```
advance(to):
    if monotone:  start = current fixpoint          (Maintenance::Grow)
    else:         start = overdelete(to)            (Maintenance::DeleteRederive)
    new_fixpoint = grow(start, to)      # semi-naïve regrowth, shared by both paths
    delta = set_difference(new_fixpoint, old_fixpoint)
```

`grow` is the shared semi-naïve regrowth: from a starting set (which is always a
*subset* of the target fixpoint) it derives everything still reachable under the
new base and adds it. On the monotone path `start` is the old fixpoint; on the
DRed path `start` is the overdeletion survivors. Because `grow` re-derives from
`init` as well as `step`, it is self-repairing: even if overdeletion removed too
much, anything still grounded comes back.

### 4.1 Overdeletion, concretely

The subtle part is `overdelete`. It must find every fact that had a derivation
resting on a retracted tuple. It does this **without any per-tuple derivation
provenance** — grmpl stores no "why" annotations. The trick is to re-run `step`
against *only the deleted tuples*:

1. **Seed.** For each base relation `R` that lost tuples, evaluate `init` and
   `step` (over the old fixpoint) with `R` **overridden to hold only its deleted
   tuples**. The result is exactly the facts whose derivation used a deleted `R`
   tuple. Union over all changed relations to cover "used *any* deleted tuple".
   This is what `eval_with` (the base-relation override primitive added to the
   evaluator, `crates/grmpl-diff/src/query.rs`) is for.

2. **Propagate.** A fact just added to the overdeletion set is itself a recursion
   input, so anything derivable *from it* (via `step` over the old base) also had
   a broken derivation. Iterate this frontier to a fixpoint. Each pass is one
   `last_overdeletion_rounds`.

3. **Survivors.** Remove the accumulated overdeletion set from the old fixpoint
   and hand what remains to `grow`.

Keying the seed on *the deleted tuples themselves* (not on "which facts lost all
support") is the whole game — see the worked example below.

### 4.2 Why provenance-free works

Two properties make the black-box, provenance-free approach exact:

* **Restriction gives us "used a deleted tuple".** Overriding relation `R` to its
  deleted subset and evaluating `step` yields precisely the derivations that
  rested on a now-gone `R` tuple. No need to have recorded, per derived fact,
  which tuples produced it.

* **Over-approximation is safe.** Overdeletion may remove facts that actually
  survive (they had another, valid derivation). `grow` re-derives them from the
  surviving grounded set. So the union of "overdelete too much" + "regrow what's
  grounded" lands exactly on `lfp(step)` over the new base.

---

## 5. Worked example: the non-well-founded cycle

This is the case that a naïve implementation gets wrong, and the reason DRed's
shape is what it is. (It is pinned by the test
`deleting_a_cycles_grounding_collapses_it` and was originally surfaced by the
200-round random-churn oracle at a specific seed.)

Setup — entity `2` has a behavior directly; a prototype cycle `0 ↔ 1` pulls it in
via the edge `0 → 2`:

```
direct(2, swim)
prototype(0, 2)      -- 0 inherits from 2   (the grounding edge)
prototype(1, 0)      -- 1 inherits from 0
prototype(0, 1)      -- 0 inherits from 1   (closes the 0 ↔ 1 cycle)
```

Fixpoint: `implements = { (2,swim), (0,swim), (1,swim) }`. Both `0` and `1` have
`swim`, grounded through `0 → 2`, and they *also* re-derive it from each other
around the cycle.

Now **retract `prototype(0, 2)`** — the grounding edge. The correct new fixpoint
is `{ (2, swim) }`: with nothing bringing `swim` into the cycle, `0` and `1` lose
it.

* **A naïve "still one-step supported?" pass keeps them forever.** After the
  deletion, `implements(0, swim)` is still derivable in one step —
  `prototype(0,1) ⋈ implements(1, swim)` — and `implements(1, swim)` is still
  derivable via `prototype(1,0) ⋈ implements(0, swim)`. Each is "supported" by
  the other. The stale pair mutually justifies itself and never leaves. **Wrong.**

* **DRed collapses it.** The seed evaluates `step` with `prototype` restricted to
  the *deleted* edge `{(0,2)}`: `prototype(0,2) ⋈ implements(2,swim)` →
  `implements(0, swim)`. Overdeletion set = `{(0,swim)}`. Propagate: from
  `{(0,swim)}`, `step` over the old base derives `prototype(1,0) ⋈ (0,swim)` →
  `(1,swim)`. Overdeletion set = `{(0,swim), (1,swim)}`. Propagate again:
  `prototype(0,1) ⋈ (1,swim)` → `(0,swim)`, already present → done. Survivors =
  `{(2,swim)}`. Regrowth finds no new grounding. New fixpoint = `{(2,swim)}`.
  **Right.**

The difference is entirely in the seed: keying on the *broken derivation* (the
deleted edge) rather than on *loss of support* is what lets the cascade reach the
cycle members that a support test would protect.

---

## 6. Correctness & instrumentation

**Correctness is enforced by an oracle, not by argument alone.** The project's
methodology (`DESIGN.md` §14) is to validate every incremental path against the
authoritative boundary recompute. `incremental_matches_recompute_under_churn`
runs 200 rounds of random insert/retract over a 5-entity world (which readily
produces prototype cycles) and asserts, every round, that the maintained fixpoint
equals `find(iterate(init, step))`. Two named tests pin specific behaviours:
`extending_a_chain_is_incremental_and_cheap` (monotone growth stays cheap) and
`deleting_a_cycles_grounding_collapses_it` (the §5 cycle case).

**Instrumentation** on `IncrementalFixpoint` reports what each `advance` did:

| field | meaning |
|-------|---------|
| `last_path: Maintenance` | `Grow` (insertion) or `DeleteRederive` (retraction) |
| `last_overdeletion_rounds` | propagation passes overdeletion took (`0` on the monotone path) |
| `last_iterations` | semi-naïve regrowth iterations |

These are how tests assert *which* path ran, and are useful for profiling churn.

---

## 7. The precondition: linear recursion

Both fast paths — `grow` and `overdelete` — assume the recursion is **linear**.
This is the one place the implementation trades generality for speed, and it is
worth stating precisely because violating it produces *silently wrong* results,
not a crash.

### 7.1 What "linear" means

A recursive `step` is **linear** when it references the recursion variable
(`Recur`) **at most once** along any derivation — the new relation is built by
combining the recursion variable with *base* relations only. The canonical shape
is transitive-closure-like:

```
step = base ⋈ recur            -- e.g. prototype ⋈ implements   (LINEAR)
```

Here every derived fact comes from *one* recur fact joined with base data. That is
why semi-naïve iteration can seed purely from the **frontier** (the newly-added
recur tuples): a genuinely new derivation must involve a genuinely new recur
tuple, so deriving from `step(frontier)` finds everything.

### 7.2 What "non-linear" means, and why it breaks the fast paths

A `step` is **non-linear** when a single derivation combines the recursion
variable with *itself*:

```
step = recur ⋈ recur           -- e.g. same-generation, or
sg(x, z) :- sg(x, y), sg(y, z) -- non-linear transitive closure   (NON-LINEAR)
```

Now a new derivation can arise from **two old** recur facts, or old×new, or
new×new. The correct semi-naïve rule needs all cross terms
(`Δ(R⋈R) = ΔR⋈R + R⋈ΔR + ΔR⋈ΔR`). But `eval_with_recur` binds *every* `Recur`
node to the same multiset, so `grow` deriving from `step(frontier)` computes only
`frontier ⋈ frontier` — it **misses** `frontier ⋈ old` and `old ⋈ frontier`.
Result: the incremental fixpoint under-derives and silently diverges from the
truth. The same gap exists in `overdelete`'s propagation. (Recur beneath
`Negate`/`Reduce` is worse still — that is non-monotone recursion, which needs
stratification and is out of scope entirely.)

Note the asymmetry: the **initial materialization** in `IncrementalFixpoint::new`
delegates to `Query::Iterate`, whose evaluator does *naïve* iteration (`step` over
the **full** relation each round) and is therefore correct for non-linear
recursion. Only the **incremental** `advance` fast paths assume linearity. So a
non-linear view would materialize correctly and then drift on the first edit —
the worst kind of bug to catch by eye.

### 7.3 Why this is not a live problem *today*

Currently there is **no path in the codebase that can construct a non-linear
`step`.** Recursion enters the engine in exactly one place: the hand-built
`implements` plan in `crates/grmpl-lang/src/behavior.rs`, which is the linear
`direct ∪ (prototype ⋈ recur)`. The surface parser/compiler does **not** yet
lower general recursive `view` definitions — recursion is a built-in, not
user-authorable. So the linearity precondition holds *by construction*, and the
oracle test covers the only recursive view that exists.

The precondition becomes a real hazard the moment the surface grows general
recursive views (a `view` that references itself), because a user could then write
`sg(x,z) :- sg(x,y), sg(y,z)` and get a fast path that is quietly wrong.

---

## 8. Open question: should we add a linearity guard/diagnostic?

Because a non-linear `step` fails *silently*, the safety question is worth
deciding deliberately rather than by default. The options:

**A. Do nothing; document the precondition (status quo).**
Cheapest. Correct *today* because non-linear steps are unconstructible (§7.3).
Risk: a future surface change that emits recursive views turns this into a silent
correctness bug with no signal. Acceptable only while recursion stays built-in.

**B. Static linearity check that *rejects*.**
Walk the `step` AST when building an `IncrementalFixpoint`; if `Recur` occurs more
than once on any root-to-leaf path, or occurs beneath `Negate`/`Reduce`, return an
error from `new`. Simple, ~30 lines, no runtime cost. Turns a silent wrong answer
into a loud construction failure. Downside: it *refuses* non-linear views rather
than handling them — fine as a v1 stance, surprising later.

**C. Static linearity check that *falls back to recompute* (recommended).**
Same analysis as B, but on failure the maintainer routes non-linear views through
the existing boundary-recompute path (`eval_delta`'s `Iterate` arm) instead of the
semi-naïve fast paths. Correctness becomes **unconditional** — every recursive
view is maintained correctly, linear ones cheaply, non-linear ones at recompute
cost — with the same one-time static check. This matches `DESIGN.md` §10 risk 1's
own contingency: *"if incremental deletion is too costly, fall back to
recompute-on-change for recursive views only, behind the same interface."* The
interface already supports it; only the routing is new.

**D. Debug-only self-check (complement to any of the above).**
In debug builds, have `advance` compare its result against a boundary recompute
and `debug_assert!` equality. This is what the oracle test does, generalized to
run in-process during development/CI against *any* view. Catches divergence early,
costs nothing in release, and does not by itself make release builds safe — so
it pairs with C rather than replacing it.

**Recommendation:** ship **C** (static check → recompute fallback) *when general
recursive views land on the surface*, optionally with **D** for defence in depth.
Until then, **A** is defensible because the hazard is unreachable — but adding the
**B/C** static check now is cheap insurance and makes the linearity assumption
executable rather than merely documented. This is a judgement call about how much
to pay now for a hazard that is currently latent; it is left as the user's
decision.

---

## 9. Pointers

* Implementation: `crates/grmpl-diff/src/recursive.rs`
  (`IncrementalFixpoint`, `overdelete`, `grow`, `Maintenance`).
* Override primitive: `eval_with` in `crates/grmpl-diff/src/query.rs`.
* Tests: `crates/grmpl-diff/tests/incremental_recursion.rs`.
* The recursive view itself: `crates/grmpl-lang/src/behavior.rs`.
* Design context: `DESIGN.md` §3.1 (operators/diff rules), §10 (risk 1), §14
  (incremental recursion summary); `idea.md` §3 (the `implements` view).
* Original algorithm: Gupta, Mumick & Subrahmanian, *"Maintaining Views
  Incrementally"*, SIGMOD 1993.
