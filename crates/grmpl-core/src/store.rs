//! The substrate boundary (DESIGN.md §4).
//!
//! These traits are the *only* contract between the semantic core and the
//! physical world. `grmpl-ent` implements them with the Ent (fjall serves only as its granfilade node store); `grmpl-diff` and
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

    /// Consolidated contents of `rel` as-of `at` **restricted to the tuple-key
    /// half-open range `[lo, hi)`** — the range-read primitive of an *arranged
    /// trace* (DESIGN.md §4, "and (later) arranged traces").
    ///
    /// This is `read_at` composed with a range restriction, and the default
    /// implementation is exactly that: a full read then a filter, so every store
    /// is correct without extra work. A store whose trace is a *measured,
    /// key-ordered* structure (the Ent's WID enfilade) overrides this to answer in
    /// `O(result + log n)`, pruning whole out-of-range subtrees instead of
    /// scanning the relation — the substrate-level fast path the language's join
    /// pushdown ([`grmpl_diff`]'s `RangeRel`) rides on. Naming a tuple range is a
    /// pure value-level contract; the trait still names no storage technology.
    fn read_range(
        &self,
        rel: RelId,
        at: Edition,
        lo: &Tuple,
        hi: &Tuple,
    ) -> Result<Vec<(Tuple, Diff)>> {
        Ok(self
            .read_at(rel, at)?
            .into_iter()
            .filter(|(t, _)| lo <= t && t < hi)
            .collect())
    }

    /// Consolidated contents of `rel` as-of `at` restricted to rows whose
    /// **column `col`** lies in `[lo, hi)` — the trailing-column counterpart of
    /// [`read_range`](Self::read_range).
    ///
    /// `read_range` prunes on the *lead* column, because that is the one the
    /// trace's primary order is keyed by. A predicate on any other column has to
    /// be answered some other way, and the default here is the honest one: read
    /// the relation and filter. A store that maintains **several orderings** of
    /// the same facts (the Ent's Arrangements — one measured tree per column
    /// order) overrides this to prune on `col` too, so a secondary-key view is
    /// sublinear at the source instead of scanning.
    ///
    /// `lo`/`hi` are single-column bounds, compared against the row's `col`th
    /// value. A `col` beyond a row's arity excludes it.
    fn read_range_on(
        &self,
        rel: RelId,
        at: Edition,
        col: usize,
        lo: &crate::value::Value,
        hi: &crate::value::Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        Ok(self
            .read_at(rel, at)?
            .into_iter()
            .filter(|(t, _)| t.as_slice().get(col).is_some_and(|v| lo <= v && v < hi))
            .collect())
    }

    /// **Interest routing.** Could any commit in `(from, to]` have touched one of
    /// `rels`?
    ///
    /// This is the substrate's answer to the canopy's question — "does this
    /// change concern anyone watching here?" — and it exists so a reactive
    /// watcher can be *routed* rather than re-evaluated: a maintained view whose
    /// base relations were not touched over an interval is provably unchanged
    /// across it, so the pump can skip the whole differential evaluation.
    ///
    /// **Conservative, never a subset.** The contract is that a `false` is a
    /// proof of no change, while a `true` merely means "possibly" — so the
    /// default implementation returns `true` unconditionally and every store is
    /// correct without doing anything. That keeps the Snapshot–stream law safe by
    /// construction: an over-eager answer costs an evaluation, an under-eager one
    /// would lose a delta. A store whose commit log is a *measured* structure
    /// (the Ent's Edition enfilade) overrides this to answer in `O(log n)` from
    /// cached subtree measures, without materializing a single update.
    ///
    /// Naming relations and editions is a pure value-level contract; the trait
    /// still names no storage technology.
    fn touched_since(&self, _from: Edition, _to: Edition, _rels: &[RelId]) -> Result<bool> {
        Ok(true)
    }

    /// Could any commit in `(from, to]` have touched rows of `rel` whose key lies
    /// in `[lo, hi)`?
    ///
    /// The key-range refinement of [`touched_since`](Self::touched_since), and
    /// the question a *canopy* exists to answer: two watchers on disjoint parts
    /// of one relation should not wake each other. Same contract — `false` is a
    /// proof of no change, `true` merely means "possibly" — so the default
    /// widens to the whole relation and every store stays correct.
    fn touched_range_since(
        &self,
        from: Edition,
        to: Edition,
        rel: RelId,
        _lo: &Tuple,
        _hi: &Tuple,
    ) -> Result<bool> {
        self.touched_since(from, to, &[rel])
    }

    /// The consolidation **watermark** (P6 history/GC). Every edition ≤ this has
    /// been folded into per-relation checkpoints and its raw updates discarded,
    /// so no intermediate as-of state below it survives. This is the floor of
    /// the four **edition doors**:
    ///
    /// * [`read_at`](Self::read_at) *at* an edition below it, and
    /// * [`scan_updates`](Self::scan_updates) *from* an edition below it,
    ///
    /// are rejected with [`Error::Store`](crate::Error::Store) rather than
    /// answered from truncated history — and the two *computed* doors that build
    /// on them, `grmpl_diff::eval_delta` and the reactive watch-cursor pump,
    /// inherit the same guard for free (they bottom out at `read_at`/
    /// `scan_updates`). A store that retains full history reports
    /// [`Edition::ZERO`], so no door ever closes.
    fn watermark(&self) -> Edition {
        Edition::ZERO
    }

    /// Consolidate history up to `up_to` (clamped to
    /// [`current`](EditionStore::current) — the future is never consolidated):
    /// fold each relation's updates at editions ≤ the new watermark into a
    /// single durable checkpoint, delete the folded raw updates, and persist the
    /// new watermark — **atomically**, so a crash leaves either the old horizon
    /// or the new one, never a half-truncated trace. Returns the new watermark,
    /// which is **monotonic** (never below the current one; a `up_to` at or
    /// below it is a no-op). This is what turns the O(history) as-of read into
    /// an O(checkpoint + tail) one.
    ///
    /// **Retention safety is the caller's.** Consolidating past a frontier a
    /// live reader still scans *from* — most importantly the minimum durable
    /// watch cursor — strands that reader below the watermark; its next read
    /// then errors at the door rather than silently losing updates. The store
    /// cannot see those frontiers (they live above the bright line, as ordinary
    /// trace rows), so the GC *policy* that clamps `up_to` to the minimum
    /// durable watch cursor lives above it (`grmpl_proc::gc`). A store that
    /// retains full history may leave this a no-op.
    fn consolidate(&self, _up_to: Edition) -> Result<Edition> {
        Ok(self.watermark())
    }

    /// **Version compare / backfollow.** How `rel` differs between editions `a`
    /// and `b`: `(tuple, weight_at_a, weight_at_b)` for every tuple whose net
    /// weight differs, **tuple-sorted** (the Determinism invariant — the answer
    /// may not depend on physical scan order).
    ///
    /// This is the *state* difference, not the *log* difference, and the
    /// distinction is the point. `scan_updates` returns every raw update in the
    /// interval, so a tuple asserted and retracted fifty times costs a hundred
    /// entries that sum to nothing. This returns only what actually differs — the
    /// read side of the trace, "what is the same, what moved".
    ///
    /// The default implementation reads both ends and differences them, so every
    /// store is correct without extra work. A store whose state is a **versioned,
    /// content-addressed tree** (the Ent) overrides it to prune whole subtrees
    /// that the two editions share, so the comparison costs the size of the
    /// *edit* rather than the size of the relation — and short-circuits to `O(1)`
    /// when the two editions share a root, which is the common case for a
    /// relation nothing touched.
    ///
    /// Both editions are subject to the same watermark floor as `read_at`:
    /// comparing against a consolidated-away edition errors rather than lying.
    fn compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<(Tuple, Diff, Diff)>> {
        use std::collections::BTreeMap;
        let mut sides: BTreeMap<Tuple, (Diff, Diff)> = BTreeMap::new();
        for (t, d) in self.read_at(rel, a)? {
            sides.entry(t).or_default().0 = d;
        }
        for (t, d) in self.read_at(rel, b)? {
            sides.entry(t).or_default().1 = d;
        }
        // A `BTreeMap` yields keys ascending, so the result is tuple-sorted by
        // construction rather than by a trailing sort.
        Ok(sides
            .into_iter()
            .filter(|(_, (wa, wb))| wa != wb)
            .map(|(t, (wa, wb))| (t, wa, wb))
            .collect())
    }

    /// **An immutable reader pinned at one edition.**
    ///
    /// Every read a query makes at a fixed edition — `read_at`, `read_range`,
    /// `read_range_on` — re-enters the store, and a store that serializes its
    /// state behind a lock therefore takes that lock once *per base-relation
    /// read*. A three-way join takes it three times, and each acquisition can
    /// block behind a committer sitting inside its `fsync`: **readers stalled by
    /// durability they do not care about.**
    ///
    /// A reader is the fix. The caller acquires one, and every read it makes goes
    /// through it. What that buys depends on the substrate, which is exactly why
    /// this is a trait method with a default rather than a fixed mechanism:
    ///
    /// * The **default** returns a reader that simply forwards each call back to
    ///   the store at the pinned edition — identical behavior to reading the
    ///   store directly, so every substrate is correct with no work.
    /// * A store whose state is **immutable and versioned by edition** (the Ent's
    ///   Fact enfilade — the same property that makes forks free) overrides this
    ///   to capture that edition's roots under one brief lock and answer every
    ///   later read **lock-free**.
    ///
    /// Infallible by construction, so pinning a snapshot cannot fail: an
    /// out-of-range edition is reported by the *read*, at the same watermark door
    /// as before.
    ///
    /// The trait stays technology-free — a reader is "an immutable view of the
    /// world at one edition", a pure value-level notion. Only the substrate knows
    /// whether that is a borrowed handle on a persistent tree or a re-entry into
    /// a mutex.
    fn reader_at(&self, at: Edition) -> Box<dyn EditionReader + '_>
    where
        Self: Sized,
    {
        Box::new(ForwardingReader { store: self, at })
    }

    /// Object-safe [`reader_at`](Self::reader_at), for a `&dyn TraceStore`.
    ///
    /// `reader_at` is `Self: Sized` so implementors can return their own reader
    /// type; this is the vtable-reachable entry point, and a store that overrides
    /// `reader_at` must override this too (they are the same method for a
    /// concrete and an erased receiver). The default forwards, so a store that
    /// overrides neither is correct.
    fn dyn_reader_at(&self, at: Edition) -> Box<dyn EditionReader + '_> {
        Box::new(ForwardingReader { store: self, at })
    }
}

/// An immutable view of the world at one [`Edition`] — the read half of a
/// [`Snapshot`](https://docs.rs/grmpl-diff), acquired once and used many times.
///
/// The three methods mirror the store's three pinned-edition reads. Only
/// [`read`](Self::read) is required; the range reads default to it plus a filter,
/// exactly as they do on [`TraceStore`], so a reader is never *wrong*, only
/// sometimes slower than the substrate could be.
pub trait EditionReader: Send + Sync {
    /// The edition this reader is pinned at. Fixed for its whole life — a reader
    /// is a snapshot, not a cursor.
    fn edition(&self) -> Edition;

    /// Consolidated contents of `rel` as-of this reader's edition.
    fn read(&self, rel: RelId) -> Result<Vec<(Tuple, Diff)>>;

    /// [`read`](Self::read) restricted to the tuple-key half-open range
    /// `[lo, hi)`.
    fn read_range(&self, rel: RelId, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        Ok(self
            .read(rel)?
            .into_iter()
            .filter(|(t, _)| lo <= t && t < hi)
            .collect())
    }

    /// [`read`](Self::read) restricted to rows whose column `col` lies in
    /// `[lo, hi)`.
    fn read_range_on(
        &self,
        rel: RelId,
        col: usize,
        lo: &crate::value::Value,
        hi: &crate::value::Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        Ok(self
            .read(rel)?
            .into_iter()
            .filter(|(t, _)| t.as_slice().get(col).is_some_and(|v| lo <= v && v < hi))
            .collect())
    }
}

/// The default [`EditionReader`]: every read re-enters the store at the pinned
/// edition. Correct for any substrate, and exactly the behavior callers had
/// before readers existed.
struct ForwardingReader<'a, S: ?Sized> {
    store: &'a S,
    at: Edition,
}

impl<S: TraceStore + ?Sized> EditionReader for ForwardingReader<'_, S> {
    fn edition(&self) -> Edition {
        self.at
    }

    fn read(&self, rel: RelId) -> Result<Vec<(Tuple, Diff)>> {
        self.store.read_at(rel, self.at)
    }

    fn read_range(&self, rel: RelId, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.store.read_range(rel, self.at, lo, hi)
    }

    fn read_range_on(
        &self,
        rel: RelId,
        col: usize,
        lo: &crate::value::Value,
        hi: &crate::value::Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        self.store.read_range_on(rel, self.at, col, lo, hi)
    }
}

/// A durable directory mapping interned relation *names* to their [`RelId`].
///
/// **Store-API boundary decision.** The *catalog contract* is declared here in
/// the core — names are `&str` and ids are `RelId`, both storage-agnostic core
/// types — while the durable map itself is a store concern (`grmpl-ent`
/// persists it as a binding in the context enfilade, beside the edition clock). It is kept
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
/// are core; the durable map is a store concern — `grmpl-ent` persists each
/// version in its `__meta` keyspace beside the catalog, under a
/// `(rel, edition)` binding. Kept separate from [`TraceStore`] and [`Catalog`]
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

/// Everything the public world runtime needs from a substrate: the editioned
/// tuple trace, durable relation-name catalog, and edition-versioned schemas.
///
/// Keeping this bundle in core lets the runtime accept one store handle without
/// naming the Ent implementation, while the three constituent interfaces remain
/// independently usable by lower modules.
pub trait WorldStore: TraceStore + Catalog + SchemaCatalog {}

impl<T: TraceStore + Catalog + SchemaCatalog> WorldStore for T {}

/// A no-op [`SchemaCatalog`] for callers that have no schema registry — every
/// relation reads as unregistered, so commit-boundary enforcement is a pass
/// (schemas are opt-in). Registering into it is an error: it has nowhere to
/// persist.
pub struct NoSchemas;

impl SchemaCatalog for NoSchemas {
    fn put_schema(&self, _rel: RelId, _schema: &Schema, _at: Edition) -> Result<()> {
        Err(crate::error::Error::Store(
            "no schema registry available".into(),
        ))
    }
    fn schema(&self, _rel: RelId) -> Result<Option<Schema>> {
        Ok(None)
    }
    fn schema_at(&self, _rel: RelId, _at: Edition) -> Result<Option<Schema>> {
        Ok(None)
    }
}
