# Wids and Dsps: measures that flow both ways

The previous chapter described *one* measure flowing *up* the tree. The `Ent`'s
real power comes from having measures flow in **both directions**, with two
different families that answer two different questions. Gold calls them **wids**
and **dsps**.

## Wids — summaries that flow up

A **wid** (short for *width*) is the upward measure: every node advertises the
*range of addresses its subtree covers*, and more generally any monoidal summary
of what lies below it. Wids are exactly the measures of the last chapter. They
flow **up**: a parent's wid is the combination of its children's wids.

Wids are what make a sparse, effectively **transfinite** address space
searchable. Xanadu wanted document addresses that could always be subdivided —
you can always insert new material "between" address 5 and address 6 — which
means the address space is not the integers but something dense and unbounded.
You cannot afford to represent every address; almost all of them are empty. The
wid at each node says "the occupied addresses below me span this range and there
are this many of them," so a search prunes entire empty regions in `O(depth)`
without materializing them. **Summaries flow up so that search can prune from the
top.**

## Dsps — context that flows down

A **dsp** (short for *displacement*) is the downward transform. A subtree does
not store absolute addresses for its contents. Instead, **a subtree's key is its
parent's key *plus a displacement***. The absolute position of a leaf is the sum
of the displacements along the path from the root down to it. Context flows
**down** the tree by composition.

This sounds like a small bookkeeping choice; it is the source of two of the
`Ent`'s most striking abilities.

### Relocation is one key change

Because positions are relative, **moving an entire subtree to a new location is a
single displacement change at its top** — `O(1)`, or `O(edit)` counting the path
copy to record it. You do not rewrite the addresses of the (possibly enormous)
contents. You change the one dsp that sits above them, and every absolute address
below shifts by exactly that amount as descents recompute it. Insert a paragraph
on page 1 of a million-page book and everything after it slides down by one dsp
adjustment, not a million rewrites.

### The virtual copy

Combine dsps with structural sharing and you get the **virtual copy** — the move
Xanadu is famous for. To place a copy of some span of document B into document A:

1. **Share** B's subtree by reference (structural sharing — no bytes copied).
2. Put it under a **new dsp** in A that displaces it to its new home.

The result is a region of A that *is* B's content, relocated, at `O(edit)` cost
and near-zero storage. It is not a snapshot that drifts out of date and it is not
a duplicate that doubles the storage — it is the same shared nodes, seen through
a different displacement. This is transclusion, and it is why editing and
"copying" in Xanadu are both cheap.

## The two directions, together

The slogan is worth memorizing because Part III returns to it constantly:

> **Wids summarize content upward; dsps carry context downward.**

- **Up (wids):** count, key range, bounding box, dirty flag, interest set —
  everything you need to *prune a search* or *route an update* from the top of
  the tree.
- **Down (dsps):** displacement, and by generalization any inherited context —
  namespace, authority, schema, placement — that a subtree should receive from
  its position in the whole.

A node, then, is a little two-faced thing: it tells its parent a *wid* about what
it contains, and it receives a *dsp* from its parent about where and in what
context it sits. An enfilade is a tree of these. The `Ent` is what you build when
you let that tree hold versions.
