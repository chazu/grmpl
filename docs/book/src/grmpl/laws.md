# The bright line and the seven laws

Two disciplines keep grmpl honest as it grows: an architectural rule about *what
may name what* (the bright line), and seven semantic laws that every phase must
preserve. Both exist so that the `Ent` can be the substrate without leaking its
storage details into the language, and so that "differential, versioned world"
stays a set of checkable promises rather than lore.

## The bright line

The semantic core — `grmpl-core`, `-diff`, `-proc`, `-lang`, `-pattern` — sits
**above the line**. It is pure value types plus the substrate **traits**:
`TraceStore`, `EditionStore`, `Catalog`, `SchemaCatalog`, `Transport`. It names
no storage or network technology.

- Only `grmpl-store` names `fjall` (the LSM stand-in).
- Only `grmpl-ent` names the enfilade/granfilade implementation.
- Only `grmpl-transport` names `iroh`.

Substrate crates depend on the traits; the traits never depend on the substrate.
**The language observes opaque `Edition`s, never physical sequence numbers.**

This is what lets the `Ent` be swapped in underneath a fully-working language.
Because `grmpl-lang` and `grmpl-proc` depend only on the store *traits*, a
completely different store — an LSM log, or the ent-native enfilade family — can
implement those traits and the entire language, every feature, every test, keeps
running unchanged. The bright line is precisely why the Ent migration could
happen without touching the semantics above it.

## The seven laws

These are stated in the founding note as the spec, and each is enforced by a
randomized law-oracle test suite. They are the contract the `Ent` must honor.

**Snapshot–stream law.** `watch(q)` always represents the changing result of
`find(q)`. Registration returns an initial result and subsequent signed deltas
with no race and no missing interval:

```text
initial-result + accumulated-deltas = query(current-edition)
```

**Patch–edition law.** `commit(snapshot, patch)` either creates exactly one new
edition or has no effect. There is no window in which an edition is allocated but
not written. On the Ent this is one transaction over the granfilade writing every
touched enfilade node plus the new roots in a single batch — node writes ordered
before the roots that reference them, so a crash leaves *all-old or all-new*,
never a root pointing at a non-durable node.

**Replay law.** `handler(snapshot, message)` is deterministic. Wall-clock samples
and randomness enter only as committed data, never by a handler reading the
system clock — so replaying the same inputs reproduces the identical world.

**Authority law.** One atomic commit touches one authority domain. Cross-domain
consequences are asynchronous messages, never a distributed transaction.

**Pattern law.** Parsing is matching over ordered data, not a privileged compiler
subsystem — the same pattern algebra runs over tokens, tuples, records, ASTs, and
delta streams.

**Object law.** Entities are values; their properties and behaviors are
relations. No hidden tuple identity, no object heap behind an API.

**Attention law.** Reactivity is a maintained query, not an unrelated callback
mechanism. `on` is `watch` plus a transactional handler; there is no callback
primitive.

## Determinism, concretely

Three determinism rules make the laws testable regardless of the store's physical
scan order:

- `TraceStore::read_at` returns **tuple-sorted** rows (consolidation runs over a
  `HashMap`, whose order is not stable — so the result is sorted before it
  leaves).
- `TraceStore::scan_updates` returns updates in **commit order**
  `(edition, counter)` — the exact order they were written, invariant under
  rebalance, consolidation, and GC.
- The language `find`/`resolve` binds to the **least** matching tuple, never
  whichever the scan happened to surface first.

On the Ent, that middle rule has teeth: the Edition enfilade stores each update's
submit index as an *immutable node payload*, not as a tree rank — so
`scan_updates` returns the raw, per-multiplicity, commit-ordered stream even
after the tree has been rebalanced or garbage-collected underneath it. The
identity that replay and fork must prove is defined over exactly this logical
projection, *not* over raw node bytes — because Xanadu, too, guarantees content
and version identity, never byte-identical node layout.

With the bright line holding the storage details below the line and the seven
laws pinning the semantics above it, the `grmpl-ent` crate can implement the plex
freely. That crate is next.
