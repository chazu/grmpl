//! Time and diffs — the differential substrate (DESIGN.md §2.2).
//!
//! The engine timestamp is `(edition, iter)`. `edition` is the commit clock of
//! an authority domain (a total order in v1). `iter` is the internal fixpoint
//! coordinate for recursive views; it is engine-private and never surfaces to
//! the language. Simulation time and wall time are deliberately **not** here —
//! they ride as `Value`s in tuples and messages (Replay law).

use crate::value::Tuple;

/// Engine timestamp = (edition coordinate, iteration coordinate).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Time {
    pub edition: u64,
    pub iter: u32,
}

impl Time {
    /// A base-relation input timestamp: iteration coordinate 0.
    pub fn input(edition: u64) -> Time {
        Time { edition, iter: 0 }
    }
}

/// An opaque edition — a point in the (v1: totally ordered) commit clock.
/// In the distributed future this becomes a causal frontier; the language
/// treats it as opaque either way (DESIGN.md §10). `Edition(0)` is the empty
/// world, before any commit.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Edition(pub u64);

impl Edition {
    pub const ZERO: Edition = Edition(0);
}

/// A Z-weight. Multiset by default: `+1` assert, `-1` retract, sums cancel.
/// Generalized later to any abelian group (WID measures ride here).
pub type Diff = i64;

/// One differential update: at `time`, `tuple`'s multiplicity changed by `diff`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub tuple: Tuple,
    pub time: Time,
    pub diff: Diff,
}
