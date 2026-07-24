//! The substrate boundary (DESIGN.md §4).
//!
//! These traits are the *only* contract between the semantic core and the
//! physical world. `grmpl-store` implements them with fjall; `grmpl-diff` and
//! the process layer depend on the traits, never on the implementation. The
//! language observes opaque `Edition`s here, never a `SeqNo`.
//!
//! Refinement over DESIGN.md §4: `commit` allocates the next edition *and*
//! writes atomically, returning the new `Edition`. Folding allocation into the
//! write strengthens the Patch–edition law (there is no window in which an
//! edition is allocated but not written).

use crate::error::Result;
use crate::time::{Diff, Edition, Update};
use crate::value::{RelId, Tuple};

/// Read-only access to the current edition clock of an authority domain.
pub trait EditionStore: Send + Sync {
    fn current(&self) -> Edition;
}

/// Persistence for base-relation inputs and (later) arranged traces.
///
/// The diff-accumulation semantics ("sum diffs at edition ≤ E") live in this
/// trait's contract and are implemented *above* the physical KV store — the
/// store need only be an ordered, atomically-batchable byte map.
pub trait TraceStore: EditionStore {
    /// Atomically allocate the next edition and apply every update, or have no
    /// effect (Patch–edition law). Returns the new edition.
    ///
    /// Each update is `(relation, tuple, diff)`; base-relation inputs carry
    /// iteration coordinate 0 implicitly.
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition>;

    /// Optimistic commit (DESIGN.md §5.2): atomically re-check that every
    /// precondition tuple still holds (positive weight) at the *current*
    /// edition and, only if so, apply the updates as one new edition. The
    /// check and apply are one atomic step, so concurrent commits racing the
    /// same precondition resolve to exactly one winner.
    ///
    /// Returns `Some(edition)` on success, `None` if a precondition failed
    /// (no effect — the caller retries against the new edition).
    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>>;

    /// Consolidated contents of a relation as-of an edition (the `find`
    /// primitive): every tuple whose summed diff at editions ≤ `at` is nonzero.
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>>;

    /// Raw updates for a relation whose edition lies in `(from, to]` — the
    /// `watch` delta primitive (used from M2 on).
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>>;
}

/// A durable directory mapping interned relation *names* to their [`RelId`].
///
/// **Store-API boundary decision.** The *catalog contract* is declared here in
/// the core — names are `&str` and ids are `RelId`, both storage-agnostic core
/// types — while the durable map itself is a store concern (`grmpl-store`
/// persists it in its `__meta` keyspace, next to the edition clock). It is kept
/// deliberately separate from [`TraceStore`]: naming is orthogonal to the
/// edition/tuple trace, so a store may implement one without the other, and the
/// language layer resolves/allocates *stable* ids across reopens through this
/// trait without ever naming the storage engine. The catalog is append-only —
/// a name's id, once bound, never silently changes.
pub trait Catalog: Send + Sync {
    /// The [`RelId`] bound to `name`, or `None` if the name is unregistered.
    fn rel_id(&self, name: &str) -> Result<Option<RelId>>;

    /// Bind `name` to `id`. Idempotent when `name` already maps to `id`; errors
    /// if `name` is already bound to a *different* id.
    fn register(&self, name: &str, id: RelId) -> Result<()>;

    /// The whole catalog, sorted by name — for reload and inspection.
    fn entries(&self) -> Result<Vec<(String, RelId)>>;
}
