//! Reactive handlers — `on watch` (P5).
//!
//! An [`OnWatch`] binds a maintained view (a [`grmpl_diff::Query`]) to a
//! [`crate::Process`]'s inbox. Its [`pump`](OnWatch::pump) reads the batch of
//! signed deltas the view has produced since a **durable watch-cursor** and, in
//! *one atomic commit*, materializes each delta as an inbox message **and**
//! advances the cursor. This is the load-bearing idea of P5:
//!
//! * **Activations are messages, not callbacks.** A view firing does not invoke
//!   a handler re-entrantly; it *appends* messages to a persistent inbox that
//!   the target [`Process`](crate::Process) drains on its own later commits.
//!   Every reactive activation is therefore a durable row in the trace — which
//!   is what makes the P10 activity log complete (DESIGN.md §2.4, *Attention*:
//!   "reactivity is a maintained query; `on` = `watch` + handler; no callback
//!   primitive exists").
//!
//! * **Cascades are async message chains, never reentrancy.** If handling an
//!   activation changes a watched view, that change is delivered by a *later*
//!   `pump` as new messages — a chain of separate commits, not a nested call.
//!   The pump never runs a behavior; it only moves deltas into inboxes.
//!
//! * **Exactly-once, replay-deterministic delivery.** The cursor advance is a
//!   `commit_if` **preconditioned on the present cursor row** (the [P4
//!   `SeqAlloc`](crate::SeqAlloc) allocates the inbox seqs in the same commit),
//!   so two pumps racing the same interval resolve to one winner and the batch
//!   delivered is a pure function of committed data — no clock, no scan order.
//!
//! * **Skip-initial by default; `including current` opt-in.** [`install`] seeds
//!   the cursor at the *current* edition, so only changes after installation are
//!   delivered. [`install_including_current`] seeds it at [`Edition::ZERO`], so
//!   the first pump delivers the whole current view as `+` rows (the initial
//!   snapshot), exactly like a fresh [`grmpl_diff::DeltaStream`]'s first poll.
//!
//! [`install`]: OnWatch::install
//! [`install_including_current`]: OnWatch::install_including_current

use grmpl_core::{
    Authority, CursorMove, Diff, Edition, Entity, Fact, Patch, Result, RelId, SchemaCatalog,
    TraceStore, Tuple, Value,
};
use grmpl_diff::{eval_delta, multiset, Query};

use crate::commit::{commit_patch, CommitOutcome};
use crate::process::inbox_fact;

/// A reactive handler: a maintained view whose signed deltas are pumped, as
/// messages, into a [`Process`](crate::Process)'s inbox.
///
/// Relations it touches:
///
/// * `inbox` — the target process's mailbox, `(target: Ent, seq: Int, body:
///   Tuple)` (shared with [`crate::enqueue`] and [`crate::Scheduler`]). Each
///   activation body is `(diff: Int, row: Tuple)` — see [`activation_body`].
/// * `cursor_rel` — the durable watch-cursor, `(watch: Ent, edition: Int)`.
///   One row per `watch`; its edition is the exclusive frontier already
///   delivered.
/// * `seqs` — the per-`(inbox, target)` seq counter the [P4 `SeqAlloc`] shares
///   with every other appender to `inbox`, so activation seqs interleave
///   correctly with scheduled/sent messages.
///
/// [P4 `SeqAlloc`]: crate::SeqAlloc
pub struct OnWatch {
    /// The maintained view whose deltas fire activations.
    pub view: Query,
    /// The cursor key — the stable identity of this on-watch (a process may
    /// install several, each over a different view, each with its own cursor).
    pub watch: Entity,
    /// The process the activation messages are addressed to (inbox key).
    pub target: Entity,
    /// The mailbox activations are appended to.
    pub inbox: RelId,
    /// The durable watch-cursor relation.
    pub cursor_rel: RelId,
    /// The shared per-`(inbox, target)` seq counter relation.
    pub seqs: RelId,
    /// The authority under which the pump commits.
    pub authority: Authority,
}

/// The activation message body for one signed delta: `(diff: Int, row: Tuple)`.
/// The full Z-weight is preserved (a consolidated delta row may have magnitude
/// `> 1` or be negative), so the receiving behavior sees the exact multiset
/// change.
pub fn activation_body(diff: Diff, row: &Tuple) -> Tuple {
    Tuple::from([Value::Int(diff), Value::Tuple(row.0.clone())])
}

/// Decode an [`activation_body`] back into `(diff, row)`.
pub fn decode_activation(body: &Tuple) -> Option<(Diff, Tuple)> {
    match body.as_slice() {
        [Value::Int(diff), Value::Tuple(row)] => Some((*diff, Tuple(row.clone()))),
        _ => None,
    }
}

impl OnWatch {
    /// The seq-counter key for this watch's `(inbox, target)` — identical to the
    /// scheme [`crate::Scheduler`] uses, so activations and scheduled messages
    /// to the same inbox share one monotonic seq space.
    fn seq_key(&self) -> Vec<Value> {
        vec![Value::Int(self.inbox.0 as i64), Value::Ent(self.target)]
    }

    /// The base relations this watch's view reads — the interest it routes on.
    fn view_relations(&self) -> Vec<RelId> {
        self.view.base_relations().into_iter().collect()
    }

    fn cursor_tuple(&self, edition: Edition) -> Tuple {
        Tuple::from([Value::Ent(self.watch), Value::Int(edition.0 as i64)])
    }

    /// The durable cursor edition for this watch, or `None` if not installed.
    pub fn cursor(&self, store: &dyn TraceStore) -> Result<Option<Edition>> {
        let at = store.current();
        for (t, d) in store.read_at(self.cursor_rel, at)? {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(self.watch)) {
                if let Some(Value::Int(e)) = t.as_slice().get(1) {
                    return Ok(Some(Edition((*e).max(0) as u64)));
                }
            }
        }
        Ok(None)
    }

    /// Install this on-watch with the **skip-initial** default: the cursor is
    /// seeded at the current edition, so only changes committed *after* this
    /// call are delivered. Seeds the shared inbox seq counter too (a no-op if
    /// already present), so every subsequent pump allocates race-safely.
    ///
    /// A one-time setup commit (like a spawn); not meant to be raced.
    pub fn install(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<CommitOutcome> {
        self.install_from(store, schemas, store.current())
    }

    /// Install with the **`including current`** opt-in: the cursor is seeded at
    /// [`Edition::ZERO`] (the empty world), so the first [`pump`](Self::pump)
    /// delivers the entire current view as `+` rows — the initial snapshot —
    /// before streaming subsequent deltas.
    pub fn install_including_current(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<CommitOutcome> {
        self.install_from(store, schemas, Edition::ZERO)
    }

    fn install_from(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
        from: Edition,
    ) -> Result<CommitOutcome> {
        let alloc = crate::SeqAlloc::read(store, self.seqs, self.seq_key())?;
        let patch = Patch::new().assert(Fact::new(self.cursor_rel, self.cursor_tuple(from)));
        let patch = alloc.seed(patch);
        commit_patch(store, schemas, &patch, &self.authority)
    }

    /// Deliver the batch of signed deltas the view has produced since the
    /// durable cursor, as inbox messages, advancing the cursor over that batch
    /// — **all in one atomic commit**. Returns the number of activation messages
    /// appended (`0` if nothing changed or the watch is not installed).
    ///
    /// When the view is unchanged over `[from, current)` the pump is a pure
    /// read: it commits nothing (so the cursor never chases the pump's own
    /// non-view commits). The cursor therefore tracks the last edition whose
    /// view *change* was delivered, not `current`; `eval_delta` from a lagging
    /// cursor is empty until the next real view change, so no interval is lost.
    ///
    /// The commit is preconditioned on the present cursor row, so a pump racing
    /// a peer over the same interval is rejected and retries against the new
    /// cursor; the delivered messages are a deterministic function of committed
    /// data alone (`eval_delta` over `[from, to)`, tuple-sorted, at seqs from the
    /// shared counter).
    pub fn pump(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<usize> {
        // Bounded retry: a rejected commit means a peer advanced the cursor or
        // the shared seq counter under us; re-read both and resolve. A later
        // pump retries if contention does not settle within the cap.
        for _ in 0..256 {
            let from = match self.cursor(store)? {
                Some(e) => e,
                None => return Ok(0), // not installed
            };
            let to = store.current();
            if to <= from {
                return Ok(0); // nothing new
            }

            // **Interest routing (G-4).** A view can only change when one of its
            // base relations does, so ask the substrate whether anything in
            // `(from, to]` touched them before doing any differential work. The
            // answer is conservative — `false` is a proof of no change, `true`
            // merely means "possibly" — so this can only ever save an evaluation,
            // never lose a delta. On a store with a measured commit log (the
            // Ent's Edition enfilade) it is an `O(log n)` measure; on any other
            // it is the default `true` and the pump behaves exactly as before.
            // When the view reads one relation through one key span — the shape
            // the E2b pushdown produces for an entity-keyed view — ask the
            // *narrow* question, so a commit elsewhere in that relation does not
            // wake this watcher. Otherwise fall back to relation-wide routing.
            let quiet = match self.view.key_span() {
                Some((rel, lo, hi)) => !store.touched_range_since(from, to, rel, &lo, &hi)?,
                None => !store.touched_since(from, to, &self.view_relations())?,
            };
            if quiet {
                return Ok(0);
            }

            let delta = eval_delta(&self.view, store, from, to)?;
            let rows = multiset::to_sorted_vec(&delta);

            // Nothing to deliver: the interval `(from, to]` holds no view change
            // (only non-view commits — including the pump's own — advanced the
            // edition). Do NOT commit a bare cursor advance: that commit's edition
            // would itself become a new empty interval to chase, forever. The
            // cursor legitimately lags by non-view editions; `eval_delta` over
            // `[from, current)` stays correct — empty until a real view change
            // lands, at which point that whole interval is delivered at once.
            if rows.is_empty() {
                return Ok(0);
            }

            let mut alloc = crate::SeqAlloc::read(store, self.seqs, self.seq_key())?;
            let cursor_from = Fact::new(self.cursor_rel, self.cursor_tuple(from));
            let mut patch = Patch::new()
                // Exactly-once guard: the cursor must still be at `from`.
                .expect(cursor_from)
                .advance_cursor(CursorMove {
                    rel: self.cursor_rel,
                    retract: Some(self.cursor_tuple(from)),
                    assert: self.cursor_tuple(to),
                });
            for (row, diff) in &rows {
                let seq = alloc.fresh();
                patch = patch.assert(inbox_fact(
                    self.inbox,
                    self.target,
                    seq,
                    activation_body(*diff, row),
                ));
            }
            let patch = alloc.seal(patch);

            match commit_patch(store, schemas, &patch, &self.authority)? {
                CommitOutcome::Committed(_) => return Ok(rows.len()),
                CommitOutcome::Rejected => continue,
            }
        }
        // Contention did not settle within the cap; a later pump retries.
        Ok(0)
    }
}
