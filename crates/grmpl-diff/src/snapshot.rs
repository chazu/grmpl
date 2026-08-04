//! `Snapshot` — an immutable read of the world at one edition (DESIGN.md §2.3 #5).
//!
//! A snapshot **holds a reader**, acquired once when it is pinned. Before that
//! it held only `(edition, &dyn TraceStore)`, so every `read`, every `holds`, and
//! every base relation of every `find` re-entered the store — and on a store
//! whose state sits behind one lock, each of those took that lock, able to block
//! behind a committer inside its `fsync`. A behavior evaluating a three-way join
//! took it three times for durability it had no stake in.
//!
//! Now the store is entered once, at `new`. What that costs afterwards is the
//! substrate's business ([`TraceStore::reader_at`]): the default reader forwards
//! each call back to the store exactly as before, while a store whose state is
//! immutable and versioned by edition captures that edition's roots and answers
//! **lock-free**. Either way a snapshot is what its name says — one view of one
//! edition, not a series of visits to a moving store.

use grmpl_core::{
    Diff, Edition, EditionReader, Fact, RelId, Result, Schema, SchemaCatalog, TraceStore, Tuple,
};

use crate::multiset;
use crate::query::{eval_snapshot_on, Query};

/// An immutable view of every relation as-of one `Edition`.
pub struct Snapshot<'a> {
    pub edition: Edition,
    /// The pinned reader every read goes through. Acquired once, at `new`, and
    /// the snapshot's only channel to the world — it holds no `&dyn TraceStore`,
    /// which is what makes "every read re-enters the store" unrepresentable
    /// rather than merely avoided.
    reader: Box<dyn EditionReader + 'a>,
}

impl<'a> Snapshot<'a> {
    pub fn new(store: &'a dyn TraceStore, edition: Edition) -> Snapshot<'a> {
        Snapshot { edition, reader: store.dyn_reader_at(edition) }
    }

    /// A snapshot pinned at the store's current edition.
    pub fn at_current(store: &'a dyn TraceStore) -> Snapshot<'a> {
        Snapshot::new(store, store.current())
    }

    /// The pinned reader, for evaluation that wants to reuse it rather than
    /// acquire its own ([`Query::find`] does).
    pub fn reader(&self) -> &dyn EditionReader {
        &*self.reader
    }

    /// The consolidated contents of a base relation as-of this edition.
    pub fn read(&self, rel: RelId) -> Result<Vec<(Tuple, Diff)>> {
        self.reader.read(rel)
    }

    /// Precondition check: does this ground fact hold (positive weight) here?
    pub fn holds(&self, fact: &Fact) -> Result<bool> {
        for (t, d) in self.reader.read(fact.rel)? {
            if d > 0 && t == fact.tuple {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The column [`Schema`] of `rel` in effect **as-of this snapshot's edition**
    /// — the P1 schema-at-edition surface paired with as-of `find q at E`, so a
    /// caller reading the world at a past edition sees the *typing that was in
    /// force then*, not today's. `None` if `rel` carried no registered schema by
    /// then. Subject to the same P6 watermark floor as the data: a snapshot
    /// pinned below the consolidation horizon cannot be read (`read`/`find` at it
    /// error), so its as-of schema is only meaningful at or above the watermark.
    pub fn schema(&self, schemas: &dyn SchemaCatalog, rel: RelId) -> Result<Option<Schema>> {
        schemas.schema_at(rel, self.edition)
    }
}

impl Query {
    /// Evaluate against one snapshot (the `find` primitive). Returns a
    /// deterministic, tuple-sorted result.
    ///
    /// Reuses the snapshot's pinned reader rather than acquiring its own, so a
    /// behavior that pins a snapshot and then runs several queries against it
    /// enters the store exactly once in total.
    pub fn find(&self, snap: &Snapshot) -> Result<Vec<(Tuple, Diff)>> {
        Ok(multiset::to_sorted_vec(&eval_snapshot_on(self, snap.reader())?))
    }
}
