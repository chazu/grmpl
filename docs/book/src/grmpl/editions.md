# Editions and causal frontiers

grmpl's property #2 — *permanent, opaque identity* — insists that editions are
**handles, not integers you do arithmetic on**. This chapter cashes that out:
what an `Edition` actually *is*, why the language is forbidden from treating it as
a number, and how the same handle can be a humble counter on one machine and a
**causal frontier** across many. It is one idea — the frontier of the version DAG
— seen at two scales.

## An edition is a handle, not a number

An **edition** names a specific version of the whole world — a point in history
you can read the world *as of*. You receive one from a commit and hand it back to
the store to read at that point. What you must **not** do is compute on it:
`edition + 1`, or assume editions are consecutive integers, or that "later" is
always a total comparison. The founding note (`idea.md` §10) makes this a law:

> A single-node implementation may use a monotonically increasing commit number.
> A distributed implementation may use a causal frontier. User programs should not
> assume editions are globally consecutive integers.

The reason is that two runtimes represent "which version" in structurally
different ways, and opacity is what lets the *same* program run on both.

## Single machine → a monotonic counter

On one machine there is exactly **one commit clock**. Commits are serialized —
one, then the next — so you can simply number them: edition 1, 2, 3, 4, … A plain
monotonically increasing integer. This is a **total order**: for any two editions
one is unambiguously before the other. It is also what grmpl does today — its
internal `Edition` is a monotonic counter.

## Distributed → a causal frontier

Now spread the world across machines that commit **concurrently**. There is no
single clock handing out the next integer, and you do not *want* one — forcing
every commit through a global counter means a network round-trip per commit and
destroys throughput. Machine A and machine B can commit at the same instant
without talking to each other.

So "which version am I looking at?" can no longer be a single number. It becomes a
**causal frontier**. To define that precisely, we need the order underneath it.

### The causal partial order

Every edition was **computed from** earlier editions — the snapshot its patch read,
the branches a merge combines. Draw an edge from cause to effect and you get the
version DAG: grmpl's **`fulltrace`**, implemented as the `DagWood` in
[`grmpl-ent`'s `dag.rs`](./implementation.md#the-branchedition-dag--dagrs). It
induces a partial order, *happens-before*:

```text
e ≼ f   ≝   e is in f's causal history
            (f directly or transitively descends from e)
```

This is exactly what `descends_from` and `common_ancestor_with` answer. It is a
*partial* order because two commits made concurrently on different branches are
**incomparable** — neither `e ≼ f` nor `f ≼ e`.

### A version is a downward-closed set (a consistent cut)

"The world as far as I am concerned" is the set `H` of all commits I consider to
have happened. For that to be a *coherent* world state it must be
**downward-closed** under `≼`:

```text
if  f ∈ H  and  e ≼ f   then   e ∈ H
```

That is: **you cannot include a commit without including all of its causal
ancestors.** Otherwise you would be reading an effect whose cause you had omitted
— a torn snapshot. A downward-closed set is what distributed systems call a
**consistent cut**.

### The frontier is the leading edge of that set

The **causal frontier** is the set of commits in `H` that nothing else in `H`
supersedes — its maximal elements:

```text
frontier(H)  =  { e ∈ H  :  there is no  f ∈ H  with  e ≺ f }
```

Two facts make this the right definition of "edition":

1. **It is an antichain.** No two frontier elements are causally comparable —
   they are the concurrent "latest tips," one per active branch or source.

2. **It fully determines `H`.** Because `H` is downward-closed, `H` is just the
   downward closure of its frontier — every ancestor of a tip. So the frontier is
   a compact *handle* for the entire version:

```text
H  =  { e : e ≼ some tip in frontier(H) }
```

Putting it together:

> **A causal frontier is an antichain in the version DAG's happens-before order
> whose downward closure is the set of commits that version of the world
> incorporates.**

## Why it looks like a vector clock

When the DAG comes from `N` sequential sources — machine A's commits form a chain,
B's form a chain, and so on — each source contributes exactly one tip to the
frontier: its latest observed commit. Writing the tip from each source as a tuple
gives

```text
{ A: 7,  B: 3,  C: 12 }
```

which is precisely a **vector clock**. The vector clock *is* the frontier, written
per source. `{A:7, B:3, C:12}` names the world = "all of A's commits through #7,
all of B's through #3, all of C's through #12, and nothing else."

## The order on editions is partial

This is why portable code cannot write `edition < other`:

```text
frontier(H₁) ≤ frontier(H₂)   iff   H₁ ⊆ H₂
```

One edition precedes another iff its history is contained in the other's.
Concurrent frontiers — `{A:7, B:3}` versus `{A:5, B:9}` — are **incomparable**;
neither is `≤` the other. A comparison operator that returns a clean boolean on
the single-machine counter returns "these are concurrent" here. Any program that
assumed a total order would break the day the world distributed.

## The single-machine case is the same abstraction, collapsed

The counter and the frontier are not two designs — they are the **same object at
two scales**. On one machine the DAG is a single **chain**. An antichain in a
chain has exactly one element, and the downward closure of "commit `n`" is
"commits `1..n`." So the frontier collapses to the single integer `n`:

```text
frontier of a one-source world  =  its latest commit  =  the counter n
```

The monotonic counter is just the frontier of a world that has only one source.

## Why the Ent makes this cheap — and why opacity is the payoff

The frontier machinery is not free-floating theory; it rides directly on the
`Ent`:

- The `fulltrace` **DagWood** *is* the causal DAG — branches are edges, and a
  `fork_edition(at)` is a new tip sharing structure (an `O(edit)` virtual copy of
  the world) rather than a deep copy.
- **Branch/trace membership is a WID upward measure** (Gold's `HistoryCrum
  inTrace:`), so "is commit `e` in this frontier's history?" — the core question
  `≼` and consistent cuts ask constantly — is `O(measure)`, subtree-pruned, not a
  DAG walk. Reading the world *at* a frontier is folding in the commits under its
  downward closure, and the enfilade prunes that fold by measure.

And this is the whole reason to keep `Edition` opaque. Because the language only
ever holds the *handle* — never a number it computes on — the substrate is free to
represent it as a counter today and a causal frontier under distribution
([P15](../future/index.md#distribution-along-scope-covers-p15)) with **no program
noticing**. The opaque-edition law is a promise: *we have deliberately not told
you what an edition is, so that we can change what it is later without breaking
your code.* Causal frontiers are what "later" turns out to be.
