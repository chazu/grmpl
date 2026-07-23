//! `Process` — an actor: a concurrency domain, not an object (DESIGN.md §2.3 #6,
//! §5.3).
//!
//! The loop watches an ordered inbox and, for each pending message, runs the
//! pure `Behavior` to build a patch, attaches the inbox-cursor advance, and
//! commits — **cursor move and effects in the same atomic batch**. That single
//! atomicity gives exactly-once processing without distributed transactions:
//!
//! * crash *before* commit → nothing was written, the cursor is unmoved, the
//!   message is re-delivered and the (deterministic) behavior re-runs;
//! * crash *after* commit → the cursor already advanced, the message is skipped.

use grmpl_core::{
    Authority, CursorMove, Edition, Entity, Fact, Patch, Result, RelId, TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;

use crate::commit::{commit_patch, CommitOutcome};

/// A pure handler: `Snapshot × message-body → Patch` (Replay law — deterministic
/// in its inputs; wall-clock and randomness must arrive as message data).
pub type Behavior = Box<dyn Fn(&Snapshot, &Tuple) -> Result<Patch> + Send + Sync>;

/// The next message a process would handle, prepared but not yet committed.
/// Exposed so a caller can simulate a crash *before* commit.
pub struct Prepared {
    pub seq: i64,
    pub patch: Patch,
}

/// An actor bound to an ordered inbox and a cursor, both persistent relations.
///
/// Inbox layout: `(process: Entity, seq: Int, body: Tuple)`.
/// Cursor layout: `(process: Entity, next_seq: Int)` — absent means position 0.
pub struct Process {
    pub entity: Entity,
    pub authority: Authority,
    pub inbox: RelId,
    pub cursor_rel: RelId,
    pub behavior: Behavior,
}

fn cursor_tuple(process: Entity, seq: i64) -> Tuple {
    Tuple::from([Value::Ent(process), Value::Int(seq)])
}

impl Process {
    /// The next unprocessed inbox position (0 if the cursor is absent).
    pub fn position(&self, store: &dyn TraceStore) -> Result<i64> {
        let at = store.current();
        for (t, d) in store.read_at(self.cursor_rel, at)? {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(self.entity)) {
                if let Some(Value::Int(pos)) = t.as_slice().get(1) {
                    return Ok(*pos);
                }
            }
        }
        Ok(0)
    }

    /// The message body at inbox position `seq`, if present.
    fn message_at(&self, store: &dyn TraceStore, seq: i64) -> Result<Option<Tuple>> {
        let at = store.current();
        for (t, d) in store.read_at(self.inbox, at)? {
            let s = t.as_slice();
            if d > 0
                && s.first() == Some(&Value::Ent(self.entity))
                && s.get(1) == Some(&Value::Int(seq))
            {
                if let Some(Value::Tuple(body)) = s.get(2) {
                    return Ok(Some(Tuple(body.clone())));
                }
            }
        }
        Ok(None)
    }

    /// Build the patch for the next pending message (with its cursor advance)
    /// without committing. `None` if the inbox is idle.
    pub fn prepare(&self, store: &dyn TraceStore) -> Result<Option<Prepared>> {
        let pos = self.position(store)?;
        let body = match self.message_at(store, pos)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let snap = Snapshot::at_current(store);
        let mut patch = (self.behavior)(&snap, &body)?;
        patch.cursor_advance = Some(CursorMove {
            rel: self.cursor_rel,
            retract: if pos > 0 { Some(cursor_tuple(self.entity, pos)) } else { None },
            assert: cursor_tuple(self.entity, pos + 1),
        });
        Ok(Some(Prepared { seq: pos, patch }))
    }

    /// Process at most one pending message, committing atomically. `None` if
    /// idle; `Some(outcome)` otherwise (a `Rejected` outcome leaves the cursor
    /// unmoved so the message is retried).
    pub fn step(&self, store: &dyn TraceStore) -> Result<Option<CommitOutcome>> {
        match self.prepare(store)? {
            Some(prepared) => Ok(Some(commit_patch(store, &prepared.patch, &self.authority)?)),
            None => Ok(None),
        }
    }

    /// Drain the inbox until idle. Returns how many messages were committed.
    pub fn run_to_idle(&self, store: &dyn TraceStore) -> Result<usize> {
        let mut n = 0;
        while let Some(outcome) = self.step(store)? {
            match outcome {
                CommitOutcome::Committed(_) => n += 1,
                // A rejected message cannot make progress on this pass; stop to
                // avoid spinning. A real scheduler would retry after new input.
                CommitOutcome::Rejected => break,
            }
        }
        Ok(n)
    }
}

/// Append a message to a process's inbox at `seq` (the "sender" side; a separate
/// commit, as it belongs to a different authority in general).
pub fn enqueue(
    store: &dyn TraceStore,
    inbox: RelId,
    process: Entity,
    seq: i64,
    body: Tuple,
) -> Result<Edition> {
    let tuple = Tuple::from([
        Value::Ent(process),
        Value::Int(seq),
        Value::Tuple(body.0.clone()),
    ]);
    store.commit(&[(inbox, tuple, 1)])
}

// Re-export so tests can build inbox facts without duplicating the layout.
pub fn inbox_fact(inbox: RelId, process: Entity, seq: i64, body: Tuple) -> Fact {
    Fact::new(
        inbox,
        Tuple::from([Value::Ent(process), Value::Int(seq), Value::Tuple(body.0.clone())]),
    )
}
