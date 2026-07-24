//! Replay (P10) — re-deriving a process's patches from committed history.
//!
//! The Replay law (DESIGN.md §2.2): a behavior is pure in `(Snapshot, body)`,
//! and every source of nondeterminism (wall time, randomness, message arrivals)
//! enters the world only as *committed data* — so the world is a deterministic
//! function of its inputs, and re-running the same inputs reproduces it exactly.
//! P0 determinism (tuple-sorted [`read_at`], commit-order [`scan_updates`]) and
//! P4 wall-time-as-data are the enablers; this module makes the re-run explicit.
//!
//! ## The log is the trace
//!
//! There is no separate "session/schedule log": the trace *is* the log. Every
//! message arrival is a durable inbox row, and [`scan_updates`] returns a
//! relation's updates in **commit order** `(edition, counter)` — the exact order
//! in which the domain wrote them. A process's slice of that log is the sequence
//! of cursor advances it committed, each recoverable as-of its edition. This
//! also covers **P5 reactive activations for free**: an `on watch` activation is
//! an ordinary inbox message (a durable row, never a callback — `grmpl_proc::
//! OnWatch`), so replaying the inbox replays activations with no special path.
//!
//! ## Two harnesses
//!
//! * [`record_run`] drives a process to idle exactly like
//!   [`Process::run_to_idle`], but records each committed step's
//!   `(seq, patch, edition)` — the live log.
//! * [`replay_from`] reconstructs that same log **purely from committed history**
//!   (no re-commit), re-deriving each consumed message's patch against the
//!   historical snapshot it committed against. Over the whole history the two
//!   agree tuple-for-tuple: replay reproduces the identical patches.
//!
//! [`scan_updates`]: grmpl_core::TraceStore::scan_updates
//! [`Process::run_to_idle`]: crate::Process::run_to_idle

use grmpl_core::{Edition, Error, Patch, Result, SchemaCatalog, TraceStore, Value};

use crate::commit::{commit_patch, CommitOutcome};
use crate::process::{Prepared, Process};

/// One step of a process's commit-order log: the inbox position it consumed, the
/// patch its behavior produced, and the edition the commit created.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Step {
    /// The inbox seq the message sat at.
    pub seq: i64,
    /// The patch the behavior proposed (with its cursor advance attached).
    pub patch: Patch,
    /// The edition this step committed.
    pub edition: Edition,
}

/// Drive `process` to idle, recording each committed step — the per-process
/// slice of the domain's commit-order log. Identical to
/// [`Process::run_to_idle`](crate::Process::run_to_idle) but it captures the
/// ordered `(seq, patch, edition)` sequence. Stops on the first `Rejected`
/// outcome (as `run_to_idle` does), leaving the cursor unmoved for a retry.
pub fn record_run(
    store: &dyn TraceStore,
    process: &Process,
    schemas: &dyn SchemaCatalog,
) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    while let Some(Prepared { seq, patch }) = process.prepare(store)? {
        match commit_patch(store, schemas, &patch, &process.authority)? {
            CommitOutcome::Committed(edition) => steps.push(Step {
                seq,
                patch,
                edition,
            }),
            CommitOutcome::Rejected => break,
        }
    }
    Ok(steps)
}

/// Re-derive, **purely from committed history**, the patch `process` produced
/// for every inbox message it consumed at an edition greater than `from`.
///
/// Reads the cursor relation's updates in `(from, current]` via
/// [`scan_updates`](grmpl_core::TraceStore::scan_updates) (commit order): each
/// advance to position `k` committed at some edition `e`, so the behavior ran
/// against the world *just before* that commit — the snapshot at `e - 1`.
/// Re-running the behavior there ([`Process::patch_at`]) reproduces the patch,
/// because it is pure in `(Snapshot, body)` and both are committed data.
///
/// Over the full history (`from = Edition::ZERO`) the result equals what
/// [`record_run`] recorded during the live run, step for step — replay yields
/// **identical patches**.
///
/// `from` must be at or above the store's consolidation watermark: the snapshots
/// this reads back live at editions ≥ `from`, and history below the watermark
/// has been folded into checkpoints and discarded (the P6 edition door).
///
/// [`Process::patch_at`]: crate::Process::patch_at
pub fn replay_from(store: &dyn TraceStore, process: &Process, from: Edition) -> Result<Vec<Step>> {
    let watermark = store.watermark();
    if from.0 < watermark.0 {
        return Err(Error::Store(format!(
            "replay from edition {} below the consolidation watermark {} \
             (that history has been folded into a checkpoint and discarded)",
            from.0, watermark.0
        )));
    }
    let current = store.current();

    // Each cursor advance for this process in (from, current], paired with the
    // edition it committed. Only the positive-weight assert (the new cursor
    // tuple) marks an advance; the paired retract of the old cursor is ignored.
    let mut advances: Vec<(i64, u64)> = Vec::new();
    for u in store.scan_updates(process.cursor_rel, from, current)? {
        if u.diff <= 0 {
            continue;
        }
        let s = u.tuple.as_slice();
        if s.first() == Some(&Value::Ent(process.entity)) {
            if let Some(Value::Int(k)) = s.get(1) {
                advances.push((*k, u.time.edition));
            }
        }
    }
    // scan_updates already yields commit order; sort by the reached position so
    // the log reads in message order regardless (advances are strictly monotone).
    advances.sort();

    let mut out = Vec::with_capacity(advances.len());
    for (k, edition) in advances {
        // Advancing the cursor to `k` consumed the message at `k - 1`, and the
        // behavior saw the world at the edition just before its commit.
        let seq = k - 1;
        let before = Edition(edition - 1);
        if let Some(patch) = process.patch_at(store, seq, before)? {
            out.push(Step {
                seq,
                patch,
                edition: Edition(edition),
            });
        }
    }
    Ok(out)
}
