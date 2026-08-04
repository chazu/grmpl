//! **Derived enfilades (plan v5 §G-8): materialized views that survive a
//! reopen.**
//!
//! Plan v4 §1.5 lists the Derived enfilade as a first-class member of the
//! family — "persistent, shared materialized views / incremental query state" —
//! and calls it grmpl's differential extension of the `Ent`, the one enfilade
//! with no Xanadu ancestor. It was neither built nor listed as deferred. What
//! existed was `grmpl-diff`'s arrangement memo, keyed by
//! `(Arc::as_ptr(node), edition)`, which by its own docstring cannot outlive a
//! single advance: a view's incremental state died with the process.
//!
//! The realization here is deliberately *not* a new storage mechanism. A
//! materialized view **is an ordinary relation** — one the engine maintains
//! rather than one a handler writes — so it lands in the Fact enfilade and gets
//! everything that comes with it for free: versioned by edition, structurally
//! shared across editions, persisted with the commit that produced it, reachable
//! from GC, carried by a fork, and readable as-of any live edition. That is
//! exactly the plan's "persistent, shared derived tree … like the Fact enfilade
//! does", and it needs no substrate change at all.
//!
//! Maintenance mirrors [`OnWatch`](crate::OnWatch): a durable cursor marks the
//! frontier already folded in, [`refresh`](Materialized::refresh) evaluates the
//! view's delta over `(cursor, current]` and commits it into the derived
//! relation as one atomic step, and the commit is preconditioned on the cursor
//! so two maintainers racing the same interval resolve to one winner. It is
//! routed by the same `touched_since` gate, so a refresh over an interval that
//! could not have touched the view does no differential work.
//!
//! **The law.** After any sequence of churn and refreshes, the derived relation
//! equals `find(view, cursor)` — the view materialized as-of the frontier it has
//! folded in. It lags by editions it has not yet seen, never disagrees about the
//! ones it has.

use grmpl_core::{
    Authority, CursorMove, Diff, Edition, Entity, Fact, Patch, RelId, Result, SchemaCatalog,
    TraceStore, Tuple, Value,
};
use grmpl_diff::{eval_delta, multiset, Query};

use crate::commit::{commit_patch, CommitOutcome};

/// A maintained materialization of `view` into the relation `into`.
///
/// The derived relation is an ordinary relation in every respect — `find`, `at`,
/// `watch`, schema enforcement and GC all treat it as one — so a materialized
/// view is queryable, joinable, and as-of readable like any other, and survives
/// a reopen because the substrate persisted it.
pub struct Materialized {
    /// The view being maintained.
    pub view: Query,
    /// The relation its rows are written into.
    pub into: RelId,
    /// The stable identity of this materialization (one process may maintain
    /// several), used as the cursor key.
    pub key: Entity,
    /// The durable maintenance cursor relation, `(key: Ent, edition: Int)`.
    pub cursor_rel: RelId,
    /// The authority the maintenance commits run under.
    pub authority: Authority,
}

impl Materialized {
    fn cursor_tuple(&self, edition: Edition) -> Tuple {
        Tuple::from([Value::Ent(self.key), Value::Int(edition.0 as i64)])
    }

    /// The durable cursor edition, or `None` if this materialization is not
    /// installed.
    pub fn cursor(&self, store: &dyn TraceStore) -> Result<Option<Edition>> {
        let at = store.current();
        for (t, d) in store.read_at(self.cursor_rel, at)? {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(self.key)) {
                if let Some(Value::Int(e)) = t.as_slice().get(1) {
                    return Ok(Some(Edition((*e).max(0) as u64)));
                }
            }
        }
        Ok(None)
    }

    /// Install the materialization with its cursor at [`Edition::ZERO`], so the
    /// first [`refresh`](Self::refresh) folds in the **whole** current view — the
    /// initial materialization — and later ones fold in deltas.
    pub fn install(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<CommitOutcome> {
        let patch =
            Patch::new().assert(Fact::new(self.cursor_rel, self.cursor_tuple(Edition::ZERO)));
        commit_patch(store, schemas, &patch, &self.authority)
    }

    /// Fold the view's changes since the cursor into the derived relation, and
    /// advance the cursor over them — **one atomic commit**. Returns the number
    /// of derived rows written (`0` if nothing changed or it is not installed).
    ///
    /// Deltas are signed, so a retraction lands as a negative weight and the
    /// substrate's own consolidation nets it away: the derived relation holds the
    /// view's multiset, not an append log of it.
    pub fn refresh(&self, store: &dyn TraceStore, schemas: &dyn SchemaCatalog) -> Result<usize> {
        // Bounded retry, as for the watch pump: a rejected commit means a peer
        // advanced the cursor under us, so re-read and resolve.
        for _ in 0..256 {
            let from = match self.cursor(store)? {
                Some(e) => e,
                None => return Ok(0), // not installed
            };
            let to = store.current();
            if to <= from {
                return Ok(0);
            }
            // Routed like the pump: a view whose inputs were not touched over the
            // interval cannot have changed, so skip the differential work
            // entirely. Conservative, so this can only save work.
            let quiet = match self.view.key_span() {
                Some((rel, lo, hi)) => !store.touched_range_since(from, to, rel, &lo, &hi)?,
                None => !store.touched_since(from, to, &self.view_relations())?,
            };
            if quiet {
                return Ok(0);
            }

            let delta = eval_delta(&self.view, store, from, to)?;
            let rows = multiset::to_sorted_vec(&delta);
            if rows.is_empty() {
                // Do not commit a bare cursor advance: that commit's own edition
                // would become a fresh empty interval to chase forever. The
                // cursor legitimately lags by non-view editions.
                return Ok(0);
            }

            let cursor_from = Fact::new(self.cursor_rel, self.cursor_tuple(from));
            let mut patch = Patch::new().expect(cursor_from).advance_cursor(CursorMove {
                rel: self.cursor_rel,
                retract: Some(self.cursor_tuple(from)),
                assert: self.cursor_tuple(to),
            });
            for (row, diff) in &rows {
                // A `Patch` carries assert/retract, not weights, so a delta of
                // magnitude k lands as k of them. Consolidated view deltas are
                // almost always ±1; a larger magnitude means the view genuinely
                // gained or lost that many copies of the row, and the substrate
                // nets them into one weight.
                let fact = Fact::new(self.into, row.clone());
                for _ in 0..diff.unsigned_abs() {
                    patch = if *diff > 0 {
                        patch.assert(fact.clone())
                    } else {
                        patch.retract(fact.clone())
                    };
                }
            }

            match commit_patch(store, schemas, &patch, &self.authority)? {
                CommitOutcome::Committed(_) => return Ok(rows.len()),
                CommitOutcome::Rejected => continue,
            }
        }
        Ok(0)
    }

    /// The materialized rows as-of `at` — an ordinary relation read, so it is a
    /// root lookup on a store whose Fact enfilade is versioned by edition.
    pub fn rows(&self, store: &dyn TraceStore, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        store.read_at(self.into, at)
    }

    /// The base relations the view reads — what the routing gate consults.
    fn view_relations(&self) -> Vec<RelId> {
        self.view.base_relations().into_iter().collect()
    }
}
