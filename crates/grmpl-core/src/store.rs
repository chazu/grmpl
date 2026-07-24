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
use crate::schema::Schema;
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

/// A durable registry of relation [`Schema`]s, keyed by [`RelId`] and
/// **versioned by the edition** at which each version took effect.
///
/// **Store-API boundary decision** (mirrors [`Catalog`]). The *schema types*
/// and the *evolution invariant* (additive-only, [`Schema::is_additive_over`])
/// are core; the durable map is a store concern — `grmpl-store` persists each
/// version in its `__meta` keyspace beside the catalog, under a
/// `sch:{rel}:{edition}` key. Kept separate from [`TraceStore`] and [`Catalog`]
/// because schema history is orthogonal to both the tuple trace and the
/// name→id binding.
///
/// **Evolution law.** A relation's schema may only grow additively over
/// editions: [`put_schema`](Self::put_schema) accepts a first schema, an
/// idempotent re-put of the current one, or a strict additive extension at a
/// *later* edition, and rejects any non-additive change ([`Error::Schema`]).
/// Recording the introducing edition is what lets [`schema_at`](Self::schema_at)
/// answer as-of queries (needed before P6 exposes as-of reads).
pub trait SchemaCatalog: Send + Sync {
    /// Record the schema for `rel`, effective as-of `at`. The first call binds
    /// the initial schema. A later call must supply either the identical schema
    /// (idempotent — no new version) or a strict additive extension of the
    /// current one at an edition greater than the current version's; anything
    /// else errors ([`Error::Schema`]).
    fn put_schema(&self, rel: RelId, schema: &Schema, at: Edition) -> Result<()>;

    /// The latest (highest-edition) schema for `rel`, or `None` if unregistered.
    fn schema(&self, rel: RelId) -> Result<Option<Schema>>;

    /// The schema in effect as-of `at`: the newest version whose introducing
    /// edition is `≤ at`, or `None` if `rel` had no schema by then.
    fn schema_at(&self, rel: RelId, at: Edition) -> Result<Option<Schema>>;
}

/// A no-op [`SchemaCatalog`] for callers that have no schema registry — every
/// relation reads as unregistered, so commit-boundary enforcement is a pass
/// (schemas are opt-in). Registering into it is an error: it has nowhere to
/// persist.
pub struct NoSchemas;

impl SchemaCatalog for NoSchemas {
    fn put_schema(&self, _rel: RelId, _schema: &Schema, _at: Edition) -> Result<()> {
        Err(crate::error::Error::Store("no schema registry available".into()))
    }
    fn schema(&self, _rel: RelId) -> Result<Option<Schema>> {
        Ok(None)
    }
    fn schema_at(&self, _rel: RelId, _at: Edition) -> Result<Option<Schema>> {
        Ok(None)
    }
}
