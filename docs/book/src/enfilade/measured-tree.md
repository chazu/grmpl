# The enfilade: a measured tree

An enfilade is, at heart, a **persistent, balanced, measured tree**. Three
adjectives, each load-bearing.

## Balanced tree

The leaves hold the actual content — characters, tuples, whatever the enfilade is
indexing — in order. Internal nodes hold no content; they exist only to keep the
tree shallow. Because the tree is kept balanced (Gold uses a B-tree-like family
of `Loaf`/`Crum` nodes; grmpl uses a weight-balanced tree), the path from the
root to any leaf has length `O(log n)`. Every fundamental operation — find a
position, split the tree at a point, join two trees — touches only nodes along
such a path, so each is `O(log n)`.

## Measured

This is the idea that makes an enfilade more than "a balanced tree." **Every node
carries a summary — a *measure* — of the entire subtree beneath it.** The measure
is computed by a *monoid*: leaves have a base measure, and an internal node's
measure is the combination (an associative `⊕` with an identity) of its
children's measures.

The simplest useful measure is a **count**: each leaf measures `1`, and each
internal node measures the sum of its children. Now the node at the root of a
subtree advertises "there are 4,213 items below me" without anyone walking the
subtree. That single fact is enough to answer positional questions in `O(log n)`:

> "What is the item at absolute position 3,000,000?"

Descend from the root. At each internal node, look at the child measures in
order — `1,000,000`, `1,500,000`, `2,000,000` — and step into the child whose
range contains the target position, subtracting the measures you skipped. You
reach the right leaf after `O(log n)` steps, having *never looked inside* the
subtrees you passed over. The measure told you their size; that was all you
needed.

Measures compose to anything monoidal, not just counts: a key range
`(min, max)`, a sum of weights, a bounding box, a "does anything dirty live
below me" boolean, a set of subscription interests. Whatever question you can
phrase as "fold this monoid over the leaves in a range," a measured tree answers
in `O(log n)` by reading node summaries instead of leaves. This is the
enfilade's superpower, and Part III shows grmpl using it to turn a linear
precondition check and a linear join into logarithmic ones.

## Persistent (in the immutable sense)

"Persistent" here is the functional-programming sense: **operations do not mutate
the tree; they return a new tree, and the old one is still valid.** This is the
property that lets old document versions keep working.

The trick that makes it affordable is **structural sharing** (also called
*path copying*). To insert a leaf, you do not copy the whole tree. You copy only
the nodes on the path from the root to the point of insertion — `O(log n)` nodes
— and let every subtree *off* that path be **shared, by reference, between the
old version and the new one.**

```text
        root                 root'
       /    \               /    \
      A      B     →       A      B'      B' is new; A is shared verbatim
     / \    / \                  / \
    …   …  C   D                C   D'    C shared; D' new
```

Editing produced `root'`, `B'`, `D'` — three new nodes on the edited path. `A`,
`C`, and all their descendants are physically the *same nodes* the old tree
still points at. So:

- The old version is completely intact (nothing it references was touched).
- The new version cost `O(log n)` new nodes, not `O(n)`.
- A link into an unchanged span of the old version still resolves — it points at
  a shared subtree that both versions agree on.

That is the whole game: **an edit is `O(edit)`, history is free, and sharing is
automatic.** Everything else the enfilade does is an elaboration of this
combination of *measured* (search by summary) and *persistent* (share by path
copy).
