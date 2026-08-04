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
    Authority, CursorMove, Diff, Edition, Entity, Error, Fact, Patch, Result, RelId, SchemaCatalog,
    TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;

use crate::commit::{commit_patch, CommitOutcome};
use crate::SeqAlloc;

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

    /// The message body at inbox position `seq` **as-of edition `at`**, if
    /// present.
    fn message_at(&self, store: &dyn TraceStore, seq: i64, at: Edition) -> Result<Option<Tuple>> {
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

    /// Build the patch this process proposes for the message at inbox position
    /// `seq`, reading the world **as-of edition `at`**, with the cursor advance
    /// attached — exactly the patch a step committing against edition `at`
    /// proposes. `None` if no message sits at `seq` as-of `at`.
    ///
    /// This is the **replay primitive** (P10): because the behavior is pure in
    /// `(Snapshot, body)` and both are recovered from committed data, re-deriving
    /// a historical step's patch against its as-of edition reproduces it exactly
    /// (the Replay law). [`prepare`](Self::prepare) is just this at
    /// `(position, current)`; [`replay_from`](crate::replay::replay_from) calls
    /// it at each consumed message's historical edition.
    pub fn patch_at(&self, store: &dyn TraceStore, seq: i64, at: Edition) -> Result<Option<Patch>> {
        let body = match self.message_at(store, seq, at)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let snap = Snapshot::new(store, at);
        let mut patch = (self.behavior)(&snap, &body)?;
        patch.cursor_advance = Some(CursorMove {
            rel: self.cursor_rel,
            retract: if seq > 0 { Some(cursor_tuple(self.entity, seq)) } else { None },
            assert: cursor_tuple(self.entity, seq + 1),
        });
        Ok(Some(patch))
    }

    /// Build the patch for the next pending message (with its cursor advance)
    /// without committing. `None` if the inbox is idle.
    pub fn prepare(&self, store: &dyn TraceStore) -> Result<Option<Prepared>> {
        let pos = self.position(store)?;
        match self.patch_at(store, pos, store.current())? {
            Some(patch) => Ok(Some(Prepared { seq: pos, patch })),
            None => Ok(None),
        }
    }

    /// Process at most one pending message, committing atomically under
    /// `schemas` (commit-boundary arity/type enforcement — pass
    /// [`grmpl_core::NoSchemas`] to opt out). `None` if idle; `Some(outcome)`
    /// otherwise (a `Rejected` outcome leaves the cursor unmoved so the message
    /// is retried).
    pub fn step(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<Option<CommitOutcome>> {
        match self.prepare(store)? {
            Some(prepared) => {
                Ok(Some(commit_patch(store, schemas, &prepared.patch, &self.authority)?))
            }
            None => Ok(None),
        }
    }

    /// Drain the inbox until idle, **stopping** at the first rejection. Returns
    /// how many messages were committed.
    ///
    /// This is the single-pass form: a rejected message cannot make progress
    /// against the state this pass read, so the pass ends with the cursor
    /// unmoved and the message redelivered later. Use
    /// [`run_to_idle_retrying`](Self::run_to_idle_retrying) when the caller is
    /// one of several concurrent writers and a rejection means *contention*
    /// rather than *an answer*.
    pub fn run_to_idle(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<usize> {
        let mut n = 0;
        while let Some(outcome) = self.step(store, schemas)? {
            match outcome {
                CommitOutcome::Committed(_) => n += 1,
                // A rejected message cannot make progress on this pass; stop to
                // avoid spinning. A real scheduler would retry after new input.
                CommitOutcome::Rejected => break,
            }
        }
        Ok(n)
    }

    /// Drain the inbox until idle, **retrying** a rejected message under
    /// `policy` instead of stopping. Returns how many messages were committed.
    ///
    /// This is the concurrent-writer form. A retry is not a re-submission of the
    /// same patch: [`step`](Self::step) rebuilds it from the store's *current*
    /// state, so the loser of a race re-runs its behavior against the winner's
    /// world — which is exactly how a contested `take lamp` turns into "you
    /// don't see that here" rather than a lost command. Both kinds of rejection
    /// take this path, and both want it: contention on an allocator counter
    /// resolves on the next attempt, and a genuinely-false world precondition
    /// re-decides against the state that falsified it.
    ///
    /// Exhausting `policy` is an [`Error::Store`](grmpl_core::Error::Store) —
    /// livelock is surfaced, never spun on.
    pub fn run_to_idle_retrying(
        &self,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
        policy: crate::retry::Backoff,
    ) -> Result<usize> {
        let mut n = 0;
        loop {
            // `step` returns `None` only when the inbox is idle; a rejection
            // leaves the cursor unmoved, so the next `step` re-prepares the same
            // message against fresh state.
            let mut attempt = policy.clone();
            loop {
                match self.step(store, schemas)? {
                    None => return Ok(n),
                    Some(CommitOutcome::Committed(_)) => {
                        n += 1;
                        break;
                    }
                    Some(CommitOutcome::Rejected) => {
                        if !attempt.wait() {
                            return Err(grmpl_core::Error::Store(format!(
                                "process {:?}: inbox message rejected on every \
                                 attempt (contention or a permanently-false \
                                 precondition)",
                                self.entity
                            )));
                        }
                    }
                }
            }
        }
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

/// Seed the seq counter for `process` in `seqs` once, on an un-raced path (e.g.
/// the single spawn commit that creates the process), so its first
/// [`enqueue_seq`] allocates fully race-safe. Idempotent — a no-op once the
/// counter row exists.
pub fn seed_seq(store: &dyn TraceStore, seqs: RelId, process: Entity) -> Result<()> {
    let alloc = SeqAlloc::read(store, seqs, vec![Value::Ent(process)])?;
    let patch = alloc.seed(Patch::new());
    if !patch.asserts.is_empty() {
        let updates: Vec<(RelId, Tuple, Diff)> =
            patch.asserts.iter().map(|f| (f.rel, f.tuple.clone(), 1)).collect();
        store.commit(&updates)?;
    }
    Ok(())
}

/// Append a message to `process`'s slice of `inbox` at the next monotonic seq,
/// drawn from the durable, race-safe [`SeqAlloc`] over `seqs` (keyed by
/// `process`). This is the concurrency-safe replacement for the P3 interim
/// scheme [`enqueue`] used behind: concurrent appends to the same key resolve to
/// **distinct, strictly increasing** seqs — a lost race re-reads the counter and
/// retries against the winner. Seed the key once on an un-raced path
/// ([`seed_seq`]); the first allocation has no counter row to precondition on.
/// Returns the seq assigned.
///
/// Like [`enqueue`] this is the authless "sender" side (a message may cross
/// authority domains), so it commits through the store's guarded `commit_if`
/// directly rather than an authority-checked patch.
pub fn enqueue_seq(
    store: &dyn TraceStore,
    inbox: RelId,
    seqs: RelId,
    process: Entity,
    body: Tuple,
) -> Result<i64> {
    for _ in 0..256 {
        let mut alloc = SeqAlloc::read(store, seqs, vec![Value::Ent(process)])?;
        let seq = alloc.fresh();
        let patch = alloc.seal(Patch::new().assert(inbox_fact(inbox, process, seq, body.clone())));

        let preconditions: Vec<(RelId, Tuple)> =
            patch.preconditions.iter().map(|f| (f.rel, f.tuple.clone())).collect();
        let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for f in &patch.asserts {
            updates.push((f.rel, f.tuple.clone(), 1));
        }
        for f in &patch.retracts {
            updates.push((f.rel, f.tuple.clone(), -1));
        }

        if store.commit_if(&preconditions, &updates)?.is_some() {
            return Ok(seq);
        }
    }
    Err(Error::Store("inbox seq allocation did not settle within retry cap".into()))
}

// Re-export so tests can build inbox facts without duplicating the layout.
pub fn inbox_fact(inbox: RelId, process: Entity, seq: i64, body: Tuple) -> Fact {
    Fact::new(
        inbox,
        Tuple::from([Value::Ent(process), Value::Int(seq), Value::Tuple(body.0.clone())]),
    )
}
