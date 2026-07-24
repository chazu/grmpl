//! The optimistic commit protocol (DESIGN.md §5.2).
//!
//! `commit_patch` validates that every write lies in the process's authority
//! domain (Authority law) **and** conforms to its relation's schema (P1
//! arity/type enforcement), then hands the preconditions and effects to the
//! store's atomic `commit_if`. The precondition re-check and the write happen
//! as one atomic step in the store, so two patches racing the same precondition
//! resolve to exactly one winner; the loser gets `Rejected` (no effect) and
//! retries against the new edition.

use grmpl_core::{
    Authority, BehaviorChecker, Diff, Edition, Error, Fact, NoBehaviorCheck, Patch, Result, RelId,
    SchemaCatalog, TraceStore, Tuple,
};

/// Commit-boundary schema enforcement (P1), applied beside the Authority check:
/// every asserted/retracted world fact whose relation has a registered schema
/// must match it (arity + column types). A relation with no registered schema
/// passes — schemas are opt-in. Errors with [`Error::Schema`] on the first
/// violation.
pub fn check_schema<'a>(
    schemas: &dyn SchemaCatalog,
    facts: impl IntoIterator<Item = &'a Fact>,
) -> Result<()> {
    for f in facts {
        if let Some(schema) = schemas.schema(f.rel)? {
            schema.check(&f.tuple).map_err(|e| {
                Error::Schema(format!("write to relation {}: {e}", f.rel.0))
            })?;
        }
    }
    Ok(())
}

/// The result of attempting to commit a patch.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommitOutcome {
    /// The patch committed, creating this edition.
    Committed(Edition),
    /// A precondition no longer held; the patch had no effect. Retry.
    Rejected,
}

/// Attempt to commit `patch` under `authority`, enforcing `schemas` at the
/// commit boundary. Pass [`grmpl_core::NoSchemas`] to opt out of schema
/// enforcement.
///
/// This is [`commit_patch_checked`] with the no-op [`NoBehaviorCheck`]: it does
/// not re-check stored code. Use `commit_patch_checked` with a real
/// [`BehaviorChecker`] (e.g. `grmpl_type::EffectChecker`) to enforce the P12
/// commit-boundary re-check when a patch may install behaviors.
pub fn commit_patch(
    store: &dyn TraceStore,
    schemas: &dyn SchemaCatalog,
    patch: &Patch,
    authority: &Authority,
) -> Result<CommitOutcome> {
    commit_patch_checked(store, schemas, &NoBehaviorCheck, patch, authority)
}

/// Attempt to commit `patch` under `authority`, enforcing `schemas` **and**
/// re-checking any stored code the patch installs via `checker` (P12 — behaviors
/// as relations). For every asserted fact carrying a [`Value::Code`] cell,
/// `checker` re-runs the P8b effect/authority check on the decoded behavior; a
/// failure rejects the whole commit with [`Error::Authority`] *before* the
/// atomic `commit_if`, so no edition is allocated (the patch–edition law holds).
/// This is the commit boundary the ROADMAP's P12 "committing a behavior re-runs
/// the effect/authority checker" clause names. Pass [`NoBehaviorCheck`] to opt
/// out (that is exactly [`commit_patch`]).
///
/// [`Value::Code`]: grmpl_core::Value::Code
/// [`BehaviorChecker`]: grmpl_core::BehaviorChecker
pub fn commit_patch_checked(
    store: &dyn TraceStore,
    schemas: &dyn SchemaCatalog,
    checker: &dyn BehaviorChecker,
    patch: &Patch,
    authority: &Authority,
) -> Result<CommitOutcome> {
    // Scheduled timers land as durable world writes (timer rows) in this same
    // commit, so they are subject to the Authority and Schema laws too.
    let sched: Vec<Fact> = patch
        .scheduled
        .iter()
        .map(|s| Fact::new(s.timers, crate::schedule::timer_row(s)))
        .collect();

    // Authority law: every asserted/retracted world fact must be owned.
    for f in patch.asserts.iter().chain(patch.retracts.iter()).chain(sched.iter()) {
        if !authority.permits(f) {
            return Err(Error::Authority(format!(
                "write to relation {:?} outside authority domain {:?}",
                f.rel, authority.domain
            )));
        }
    }

    // Schema law (P1): every asserted/retracted world fact must conform to its
    // relation's registered schema (arity + column types). Beside the Authority
    // check so no world write bypasses either.
    check_schema(
        schemas,
        patch.asserts.iter().chain(patch.retracts.iter()).chain(sched.iter()),
    )?;

    // P12 code law: any behavior this patch installs (a `Value::Code` cell in an
    // asserted fact) is re-checked against `authority` before it can be stored.
    // A relation-level authority violation in the stored code rejects the whole
    // commit here, before `commit_if` — so unsafe live code never lands.
    for f in &patch.asserts {
        checker.check_fact(f, authority)?;
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
    for f in &sched {
        updates.push((f.rel, f.tuple.clone(), 1));
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
