//! The optimistic commit protocol (DESIGN.md §5.2).
//!
//! `commit_patch` validates that every write lies in the process's authority
//! domain (Authority law), then hands the preconditions and effects to the
//! store's atomic `commit_if`. The precondition re-check and the write happen
//! as one atomic step in the store, so two patches racing the same precondition
//! resolve to exactly one winner; the loser gets `Rejected` (no effect) and
//! retries against the new edition.

use grmpl_core::{Authority, Diff, Edition, Error, Patch, Result, RelId, TraceStore, Tuple};

/// The result of attempting to commit a patch.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommitOutcome {
    /// The patch committed, creating this edition.
    Committed(Edition),
    /// A precondition no longer held; the patch had no effect. Retry.
    Rejected,
}

/// Attempt to commit `patch` under `authority`.
pub fn commit_patch(
    store: &dyn TraceStore,
    patch: &Patch,
    authority: &Authority,
) -> Result<CommitOutcome> {
    // Authority law: every asserted/retracted world fact must be owned.
    for f in patch.asserts.iter().chain(patch.retracts.iter()) {
        if !authority.permits(f) {
            return Err(Error::Authority(format!(
                "write to relation {:?} outside authority domain {:?}",
                f.rel, authority.domain
            )));
        }
    }

    let preconditions: Vec<(RelId, Tuple)> = patch
        .preconditions
        .iter()
        .map(|f| (f.rel, f.tuple.clone()))
        .collect();

    // Translate the patch into signed updates. Asserts/retracts are world
    // facts; emits append to inbox relations; the cursor advance is a
    // retract-then-assert — all in the one atomic commit.
    let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
    for f in &patch.asserts {
        updates.push((f.rel, f.tuple.clone(), 1));
    }
    for f in &patch.retracts {
        updates.push((f.rel, f.tuple.clone(), -1));
    }
    for m in &patch.emits {
        updates.push((m.inbox, m.body.clone(), 1));
    }
    if let Some(cm) = &patch.cursor_advance {
        if let Some(old) = &cm.retract {
            updates.push((cm.rel, old.clone(), -1));
        }
        updates.push((cm.rel, cm.assert.clone(), 1));
    }

    match store.commit_if(&preconditions, &updates)? {
        Some(edition) => Ok(CommitOutcome::Committed(edition)),
        None => Ok(CommitOutcome::Rejected),
    }
}
