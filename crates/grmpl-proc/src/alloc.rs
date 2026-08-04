//! Entity-id allocation as a counter relation (DESIGN.md §5, P3).
//!
//! A fresh entity id comes from a single-row counter relation `(next: Int)`.
//! Allocation is **replay-safe**: the id is read from the committed counter and
//! the counter bump rides in the *same* patch as the effects that use the id, so
//! re-running a behavior from the same edition reproduces the same ids — no
//! wall-clock, no `Math.random`, nothing outside the trace (Replay law).
//!
//! It is also **race-safe**: [`Alloc::seal`] preconditions the present counter
//! row, so two patches allocating from the same counter value resolve to exactly
//! one winner and the loser retries against the winner's value. This is the same
//! guard [`SeqAlloc::seal`](crate::SeqAlloc::seal) uses for inbox seqs; the P3
//! interim scheme was an unguarded retract/assert that required the session layer
//! to serialize every commit behind one writer. It no longer does.
//!
//! The one unguarded case is the very first allocation, when no counter row
//! exists yet: there is no present tuple to precondition on (the substrate offers
//! no "assert-if-absent"). Seed the counter once, on a path that is not raced,
//! with [`seed`](Alloc::seed) — or simply assert the row in the world's `init`
//! commit; every allocation thereafter is fully race-safe.

use grmpl_core::{Entity, Patch, RelId, Result, TraceStore, Tuple, Value};
use grmpl_diff::Snapshot;

/// A replay-safe, race-safe id allocator over a counter relation.
///
/// Build one from the world it will commit against ([`read`](Self::read) /
/// [`from_snapshot`](Self::from_snapshot)), pull ids with [`fresh`](Self::fresh),
/// then fold the counter bump into the committing patch with
/// [`seal`](Self::seal). The counter relation must lie within the committing
/// authority's scope (the bump is an ordinary owned write).
pub struct Alloc {
    rel: RelId,
    /// The counter's value when this allocator was built (the first id it hands
    /// out).
    start: i64,
    /// Whether a positive counter row existed — determines whether `seal`
    /// guards and retracts the prior value.
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
        Alloc {
            rel,
            start,
            present,
            n: 0,
        }
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

    /// Whether the counter row already exists. A seeded counter allocates fully
    /// race-safe; an unseeded one has nothing to precondition on.
    pub fn is_seeded(&self) -> bool {
        self.present
    }

    /// Seed the counter row at the allocator's base if it is absent — a no-op
    /// once the counter exists. Intended for the single, un-raced commit that
    /// creates the world, and it must be *its own* commit: `seed` and
    /// [`seal`](Self::seal) both assert a counter row, so folding them into one
    /// patch would leave two.
    pub fn seed(&self, patch: Patch) -> Patch {
        if self.present {
            return patch;
        }
        patch.assert(grmpl_core::Fact::new(
            self.rel,
            Tuple::from([Value::Int(self.start)]),
        ))
    }

    /// Fold the counter advance into `patch`: **precondition** the present
    /// counter row, retract it, and assert the bumped value, so the whole
    /// allocation commits atomically with the effects that consumed the ids. A
    /// no-op when nothing was allocated — the invariant is that a present counter
    /// row always has weight exactly 1.
    ///
    /// The precondition is what makes concurrent allocation resolve to one
    /// winner: the loser is `Rejected` with zero effect and retries with a fresh
    /// [`read`](Self::read) against the winner's value, so no two commits ever
    /// hand out the same entity id.
    pub fn seal(&self, mut patch: Patch) -> Patch {
        if self.n == 0 {
            return patch;
        }
        if self.present {
            // Guard: the counter must still hold `start` at commit time.
            patch = patch.expect(grmpl_core::Fact::new(
                self.rel,
                Tuple::from([Value::Int(self.start)]),
            ));
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
