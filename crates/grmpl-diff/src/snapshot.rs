//! `Snapshot` — an immutable read of the world at one edition (DESIGN.md §2.3 #5).

use grmpl_core::{Diff, Edition, Fact, Result, RelId, Schema, SchemaCatalog, TraceStore, Tuple};

use crate::multiset;
use crate::query::{eval_snapshot, Query};

/// An immutable view of every relation as-of one `Edition`.
pub struct Snapshot<'a> {
    pub edition: Edition,
    pub(crate) store: &'a dyn TraceStore,
}

impl<'a> Snapshot<'a> {
    pub fn new(store: &'a dyn TraceStore, edition: Edition) -> Snapshot<'a> {
        Snapshot { edition, store }
    }

    /// A snapshot pinned at the store's current edition.
    pub fn at_current(store: &'a dyn TraceStore) -> Snapshot<'a> {
        Snapshot { edition: store.current(), store }
    }

    /// The consolidated contents of a base relation as-of this edition.
    pub fn read(&self, rel: RelId) -> Result<Vec<(Tuple, Diff)>> {
        self.store.read_at(rel, self.edition)
    }

    /// Precondition check: does this ground fact hold (positive weight) here?
    pub fn holds(&self, fact: &Fact) -> Result<bool> {
        for (t, d) in self.store.read_at(fact.rel, self.edition)? {
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
    pub fn find(&self, snap: &Snapshot) -> Result<Vec<(Tuple, Diff)>> {
        Ok(multiset::to_sorted_vec(&eval_snapshot(self, snap.store, snap.edition)?))
    }
}
