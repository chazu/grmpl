//! **WID — the monoidal subtree measure.**
//!
//! Every enfilade node carries a *measure*: a summary of the subtree beneath it
//! that combines associatively up the tree (Xanadu's *wid*). A measure lets any
//! "what / where / how-many under here" question be answered in `O(depth)` by
//! pruning subtrees whose summary cannot contribute — the mechanism behind range
//! reads and precondition checks.
//!
//! A `Measure` is a monoid over entries: an `empty` identity, an `entry` injection
//! for a single `(key, value)`, and an associative `combine`. **`combine` must be
//! associative and `empty` its identity**, or upward summaries diverge from the
//! contents. It need not be commutative (the tree preserves key order).

/// A monoidal summary of a subtree of `(K, V)` entries.
pub trait Measure<K, V>: Clone {
    /// The identity: the measure of the empty subtree.
    fn empty() -> Self;
    /// The measure of a single entry.
    fn entry(key: &K, val: &V) -> Self;
    /// Associative combination of two adjacent subtree measures (left ∘ right).
    fn combine(&self, right: &Self) -> Self;
}

/// The trivial measure — just the entry count. Useful on its own (size), and as
/// the identity building block; every enfilade tracks at least this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Count(pub u64);

impl<K, V> Measure<K, V> for Count {
    fn empty() -> Self {
        Count(0)
    }
    fn entry(_k: &K, _v: &V) -> Self {
        Count(1)
    }
    fn combine(&self, right: &Self) -> Self {
        Count(self.0 + right.0)
    }
}
