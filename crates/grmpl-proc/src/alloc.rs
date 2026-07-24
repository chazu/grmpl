//! Entity-id allocation as a counter relation (DESIGN.md §5, P3).
//!
//! A fresh entity id comes from a single-row counter relation `(next: Int)`.
//! Allocation is **replay-safe**: the id is read from the committed counter and
//! the counter bump rides in the *same* patch as the effects that use the id, so
//! re-running a behavior from the same edition reproduces the same ids — no
//! wall-clock, no `Math.random`, nothing outside the trace (Replay law).
//!
//! This is the interim **single-writer** scheme: because the counter bump is a
//! plain retract/assert (not guarded by a precondition), two patches allocating
//! from the same counter value must not commit concurrently. P4 replaces it with
//! a durable, concurrency-safe allocator; until then the session layer serializes
//! commits behind one writer.

use grmpl_core::{Entity, Patch, RelId, Result, TraceStore, Tuple, Value};
use grmpl_diff::Snapshot;

/// A replay-safe id allocator over a counter relation.
///
/// Build one from the world it will commit against ([`read`](Self::read) /
/// [`from_snapshot`](Self::from_snapshot)), pull ids with [`next`](Self::next),
/// then fold the counter bump into the committing patch with
/// [`seal`](Self::seal). The counter relation must lie within the committing
/// authority's scope (the bump is an ordinary owned write).
pub struct Alloc {
    rel: RelId,
    /// The counter's value when this allocator was built (the first id it hands
    /// out).
    start: i64,
    /// Whether a positive counter row existed — determines whether `seal`
    /// retracts the prior value.
    present: bool,
    /// How many ids have been handed out.
    n: i64,
}

impl Alloc {
    fn from_rows(rel: RelId, rows: Vec<(Tuple, i64)>, base: i64) -> Alloc {
        let mut start = base;
        let mut present = false;
        for (t, d) in rows {
            if d > 0 {
                if let Some(Value::Int(v)) = t.as_slice().first() {
                    start = *v;
                    present = true;
                }
            }
        }
        Alloc { rel, start, present, n: 0 }
    }

    /// Build from the store's current edition. If the counter row is absent,
    /// allocation begins at `base` — choose a base above any hand-seeded fixed
    /// entities so allocated ids never collide with them.
    pub fn read(store: &dyn TraceStore, rel: RelId, base: i64) -> Result<Alloc> {
        let at = store.current();
        Ok(Alloc::from_rows(rel, store.read_at(rel, at)?, base))
    }

    /// Build from a pinned snapshot — the form a `Behavior` uses, so the id it
    /// reads matches the edition it is reasoning about.
    pub fn from_snapshot(snap: &Snapshot, rel: RelId, base: i64) -> Result<Alloc> {
        Ok(Alloc::from_rows(rel, snap.read(rel)?, base))
    }

    /// Allocate the next fresh entity id.
    pub fn fresh(&mut self) -> Entity {
        let id = self.start + self.n;
        self.n += 1;
        Entity(id as u64)
    }

    /// How many ids have been handed out from this allocator.
    pub fn allocated(&self) -> i64 {
        self.n
    }

    /// Fold the counter advance into `patch`: retract the prior counter row (if
    /// one existed) and assert the new one, so the whole allocation commits
    /// atomically with the effects that consumed the ids. A no-op when nothing
    /// was allocated — the invariant is that a present counter row always has
    /// weight exactly 1.
    pub fn seal(&self, mut patch: Patch) -> Patch {
        if self.n == 0 {
            return patch;
        }
        if self.present {
            patch = patch.retract(grmpl_core::Fact::new(
                self.rel,
                Tuple::from([Value::Int(self.start)]),
            ));
        }
        patch.assert(grmpl_core::Fact::new(
            self.rel,
            Tuple::from([Value::Int(self.start + self.n)]),
        ))
    }
}
