//! Scheduling & simulation time (P4).
//!
//! Three pieces, all obeying the three-times separation (DESIGN.md §2.2:
//! *simulation time and wall time are never engine coordinates — they ride as
//! `Value`s*, so replay is deterministic — the Replay law):
//!
//! 1. [`SeqAlloc`] — a durable, race-safe monotonic sequence allocator over a
//!    counter relation keyed by an arbitrary tuple prefix. It generalizes the
//!    P3 interim single-writer `Domain.outseq`: the counter advance is folded
//!    into the committing patch *with a precondition on the present counter
//!    row*, so two commits racing the same key resolve to exactly one winner
//!    (the loser retries against a fresh value).
//!
//! 2. [`Scheduler`] — fires due timer rows. A [`grmpl_core::Scheduled`] entry
//!    on a patch is recorded, in the *same atomic commit*, as a durable timer
//!    row (see [`timer_row`]); [`Scheduler::fire_due`] later delivers it. Each
//!    fire is one `commit_if` **preconditioned on the due timer row**: it
//!    retracts the timer and appends the inbox message in one batch, so a timer
//!    fires exactly once even if several drivers race (the first to retract the
//!    row wins; the rest are rejected).
//!
//! 3. [`ClockDriver`] — samples the wall clock and randomness and commits them
//!    as ordinary **data** rows. This is the one sanctioned home for
//!    nondeterminism: everything downstream (a timer's `now`, a behavior's
//!    "current time" or "random roll") reads the sample back from the trace, so
//!    replaying the trace reproduces identical results.

use grmpl_core::{
    Authority, Entity, Fact, Patch, RelId, Result, Scheduled, Tuple, Value,
};

use crate::commit::{commit_patch, CommitOutcome};
use crate::process::inbox_fact;

// ---------------------------------------------------------------------------
// Timer rows
// ---------------------------------------------------------------------------

/// Canonical durable timer row: `(due: Int, inbox: Int, target: Ent, body:
/// Tuple)`. This is what a [`Scheduled`] entry becomes at commit time and what
/// [`Scheduler::fire_due`] decodes.
pub fn timer_row(s: &Scheduled) -> Tuple {
    Tuple::from([
        Value::Int(s.due),
        Value::Int(s.inbox.0 as i64),
        Value::Ent(s.target),
        Value::Tuple(s.body.0.clone()),
    ])
}

/// Decode a timer row into `(due, inbox, target, body)`.
fn decode_timer_row(t: &Tuple) -> Option<(i64, RelId, Entity, Tuple)> {
    match t.as_slice() {
        [Value::Int(due), Value::Int(inbox), Value::Ent(target), Value::Tuple(body)] => {
            Some((*due, RelId(*inbox as u32), *target, Tuple(body.clone())))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// SeqAlloc — durable per-key monotonic sequence allocator
// ---------------------------------------------------------------------------

/// A durable monotonic sequence allocator over a counter relation, keyed by an
/// arbitrary tuple prefix. Row layout: `key ++ [next: Int]`.
///
/// Build one against the world it will commit into with [`read`](Self::read),
/// hand out ids with [`fresh`](Self::fresh), then fold the counter advance into
/// the committing patch with [`seal`](Self::seal). Unlike the P3 [`crate::Alloc`]
/// the advance is **guarded**: `seal` preconditions the present counter row, so
/// a commit that races another allocation of the same key is rejected and
/// retries against the winner's value — a durable, concurrency-safe allocator.
///
/// The one unguarded case is the very first allocation of a key, when no
/// counter row exists yet: there is no present tuple to precondition on (the
/// substrate offers no "assert-if-absent"). Seed the counter once, on a path
/// that is not raced — e.g. the single spawn-commit that creates the inbox —
/// with [`seed`](Self::seed); every allocation thereafter is fully race-safe.
pub struct SeqAlloc {
    rel: RelId,
    key: Vec<Value>,
    /// The counter's value when read (the first seq handed out).
    start: i64,
    /// Whether a positive counter row existed.
    present: bool,
    /// How many seqs have been handed out.
    n: i64,
}

impl SeqAlloc {
    fn from_rows(rel: RelId, key: Vec<Value>, rows: Vec<(Tuple, i64)>) -> SeqAlloc {
        let mut start = 0;
        let mut present = false;
        let k = key.len();
        for (t, d) in rows {
            if d > 0 && t.as_slice().len() == k + 1 && t.as_slice()[..k] == key[..] {
                if let Some(Value::Int(v)) = t.as_slice().get(k) {
                    start = *v;
                    present = true;
                }
            }
        }
        SeqAlloc { rel, key, start, present, n: 0 }
    }

    /// Build from the store's current edition, keyed by `key`.
    pub fn read(store: &dyn grmpl_core::TraceStore, rel: RelId, key: Vec<Value>) -> Result<SeqAlloc> {
        let at = store.current();
        Ok(SeqAlloc::from_rows(rel, key, store.read_at(rel, at)?))
    }

    /// The seq that the next [`fresh`](Self::fresh) would hand out.
    pub fn peek(&self) -> i64 {
        self.start + self.n
    }

    /// Whether the counter row already exists (i.e. was seeded / previously
    /// bumped). A seeded key allocates fully race-safe.
    pub fn is_seeded(&self) -> bool {
        self.present
    }

    /// Allocate the next monotonic seq for this key.
    pub fn fresh(&mut self) -> i64 {
        let s = self.start + self.n;
        self.n += 1;
        s
    }

    fn counter_tuple(&self, v: i64) -> Tuple {
        let mut cols = self.key.clone();
        cols.push(Value::Int(v));
        Tuple::new(cols)
    }

    /// Seed the counter row at 0 if it is absent. Intended for the single,
    /// un-raced commit that first creates the key (e.g. a spawn-commit); a no-op
    /// once the counter exists. After seeding, every [`seal`](Self::seal) is
    /// race-safe.
    pub fn seed(&self, patch: Patch) -> Patch {
        if self.present {
            return patch;
        }
        patch.assert(Fact::new(self.rel, self.counter_tuple(0)))
    }

    /// Fold the counter advance into `patch`: precondition the present counter
    /// row, retract it, and assert the bumped value, so allocation commits
    /// atomically with the effects that consumed the seqs. A no-op when nothing
    /// was allocated. The precondition is what makes concurrent allocation of
    /// the same key resolve to one winner; a rejected commit retries with a
    /// fresh [`read`](Self::read).
    pub fn seal(&self, mut patch: Patch) -> Patch {
        if self.n == 0 {
            return patch;
        }
        if self.present {
            // Guard: the counter must still hold `start` at commit time.
            patch = patch.expect(Fact::new(self.rel, self.counter_tuple(self.start)));
            patch = patch.retract(Fact::new(self.rel, self.counter_tuple(self.start)));
        }
        patch.assert(Fact::new(self.rel, self.counter_tuple(self.start + self.n)))
    }
}

// ---------------------------------------------------------------------------
// Scheduler — fires due timers
// ---------------------------------------------------------------------------

/// Fires timers whose simulation-time due point has been reached.
///
/// Bound to the durable timer relation `timers` (where [`Scheduled`] entries
/// land) and the seq-counter relation `seqs` (per `(inbox, target)` inbox
/// ordering). [`fire_due`](Self::fire_due) delivers every due timer exactly
/// once.
pub struct Scheduler {
    pub timers: RelId,
    pub seqs: RelId,
    pub authority: Authority,
}

impl Scheduler {
    pub fn new(timers: RelId, seqs: RelId, authority: Authority) -> Scheduler {
        Scheduler { timers, seqs, authority }
    }

    /// The seq-counter key for an `(inbox, target)` pair.
    fn seq_key(inbox: RelId, target: Entity) -> Vec<Value> {
        vec![Value::Int(inbox.0 as i64), Value::Ent(target)]
    }

    /// Is `row` still a live (positive-weight) timer at the current edition?
    fn timer_live(&self, store: &dyn grmpl_core::TraceStore, row: &Tuple) -> Result<bool> {
        let at = store.current();
        Ok(store
            .read_at(self.timers, at)?
            .into_iter()
            .any(|(t, d)| d > 0 && &t == row))
    }

    /// Fire every timer due at simulation time `now` (`due <= now`), each as one
    /// atomic `commit_if`. Returns how many timers this call delivered.
    ///
    /// Determinism: due timers are fired in tuple-sorted order, so the inbox
    /// seqs a batch produces are a function of the committed data alone. A fire
    /// that loses the timer-row race (a peer delivered it) is skipped; a fire
    /// that loses only the seq race retries with a fresh seq.
    pub fn fire_due(
        &self,
        store: &dyn grmpl_core::TraceStore,
        schemas: &dyn grmpl_core::SchemaCatalog,
        now: i64,
    ) -> Result<usize> {
        let at = store.current();
        let mut due: Vec<Tuple> = store
            .read_at(self.timers, at)?
            .into_iter()
            .filter(|(t, d)| *d > 0 && decode_timer_row(t).is_some_and(|(due, ..)| due <= now))
            .map(|(t, _)| t)
            .collect();
        due.sort();

        let mut fired = 0;
        for row in due {
            if self.fire_one(store, schemas, &row)? {
                fired += 1;
            }
        }
        Ok(fired)
    }

    /// Fire a single timer row exactly once. `Ok(true)` if this call delivered
    /// it, `Ok(false)` if it was already gone (a peer delivered it). Retries on
    /// a lost seq race; a spin cap bounds pathological contention.
    fn fire_one(
        &self,
        store: &dyn grmpl_core::TraceStore,
        schemas: &dyn grmpl_core::SchemaCatalog,
        row: &Tuple,
    ) -> Result<bool> {
        let (_, inbox, target, body) = match decode_timer_row(row) {
            Some(t) => t,
            None => return Ok(false), // not a timer row we understand; leave it
        };
        let timer = Fact::new(self.timers, row.clone());

        for _ in 0..64 {
            // The timer is the exactly-once guard: if a peer already delivered
            // it, stop — we did not fire it.
            if !self.timer_live(store, row)? {
                return Ok(false);
            }
            let mut alloc = SeqAlloc::read(store, self.seqs, Self::seq_key(inbox, target))?;
            let seq = alloc.fresh();
            let patch = Patch::new()
                .expect(timer.clone())
                .retract(timer.clone())
                .assert(inbox_fact(inbox, target, seq, body.clone()));
            let patch = alloc.seal(patch);
            match commit_patch(store, schemas, &patch, &self.authority)? {
                CommitOutcome::Committed(_) => return Ok(true),
                // Either the timer vanished or the seq counter moved under us;
                // loop re-reads both and resolves.
                CommitOutcome::Rejected => continue,
            }
        }
        // Contention did not settle within the cap; a later tick retries.
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// ClockDriver — records wall-clock / randomness as data
// ---------------------------------------------------------------------------

/// Records wall-clock and randomness samples as committed **data**, so every
/// downstream read is deterministic on replay (Replay law, DESIGN.md §2.2).
///
/// This is the single sanctioned home for nondeterminism: sample once here,
/// commit it, and all scheduling / behavior logic reads the sample back from
/// the trace. Clock relation layout: `(seq: Int, wall_ms: Int, rand: Int)`.
pub struct ClockDriver {
    pub clock: RelId,
    pub authority: Authority,
}

impl ClockDriver {
    pub fn new(clock: RelId, authority: Authority) -> ClockDriver {
        ClockDriver { clock, authority }
    }

    /// The latest committed sample `(seq, wall_ms, rand)`, if any.
    pub fn latest(&self, store: &dyn grmpl_core::TraceStore) -> Result<Option<(i64, i64, i64)>> {
        let at = store.current();
        let mut best: Option<(i64, i64, i64)> = None;
        for (t, d) in store.read_at(self.clock, at)? {
            if d <= 0 {
                continue;
            }
            if let [Value::Int(seq), Value::Int(ms), Value::Int(r)] = t.as_slice() {
                if best.map(|(s, ..)| *seq > s).unwrap_or(true) {
                    best = Some((*seq, *ms, *r));
                }
            }
        }
        Ok(best)
    }

    /// Commit one sample as data at the next seq. The caller supplies the
    /// nondeterministic values — inject [`wall_clock_millis`] and a PRNG in
    /// production, or fixed values in tests / replay. A driver owns its clock
    /// (one simulation clock per domain, like one commit clock), so seq append
    /// is single-writer. Returns the committed `(seq, wall_ms, rand)`.
    pub fn sample(
        &self,
        store: &dyn grmpl_core::TraceStore,
        schemas: &dyn grmpl_core::SchemaCatalog,
        wall_ms: i64,
        rand: i64,
    ) -> Result<(i64, i64, i64)> {
        let seq = self.latest(store)?.map(|(s, ..)| s + 1).unwrap_or(0);
        let row = Tuple::from([Value::Int(seq), Value::Int(wall_ms), Value::Int(rand)]);
        let patch = Patch::new().assert(Fact::new(self.clock, row));
        match commit_patch(store, schemas, &patch, &self.authority)? {
            CommitOutcome::Committed(_) => Ok((seq, wall_ms, rand)),
            CommitOutcome::Rejected => {
                Err(grmpl_core::Error::Store("clock sample rejected (unexpected: clock is single-writer)".into()))
            }
        }
    }
}

/// The OS wall clock as integer milliseconds since the Unix epoch — the actual
/// nondeterminism source. Only a driver whose job is to *record* the sample as
/// data should call it; nothing else in the substrate reads a clock.
pub fn wall_clock_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
