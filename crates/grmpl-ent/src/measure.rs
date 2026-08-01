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

/// Tuple measures compose: a tree may carry several upward summaries at once
/// without any new tree code, since a product of monoids is a monoid.
impl<K, V, A: Measure<K, V>, B: Measure<K, V>> Measure<K, V> for (A, B) {
    fn empty() -> Self {
        (A::empty(), B::empty())
    }
    fn entry(key: &K, val: &V) -> Self {
        (A::entry(key, val), B::entry(key, val))
    }
    fn combine(&self, right: &Self) -> Self {
        (self.0.combine(&right.0), self.1.combine(&right.1))
    }
}

/// **Σ of the entries' weights** under a subtree.
///
/// The Fact enfilade's values *are* net weights, so this makes "what is the
/// total weight over this key span" an `O(log n)` fold of cached summaries
/// rather than a materialize-then-sum — the difference between an aggregate
/// reading the tree's shape and reading its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SumDiff(pub i64);

impl<K> Measure<K, i64> for SumDiff {
    fn empty() -> Self {
        SumDiff(0)
    }
    fn entry(_k: &K, v: &i64) -> Self {
        SumDiff(*v)
    }
    fn combine(&self, right: &Self) -> Self {
        SumDiff(self.0.wrapping_add(right.0))
    }
}

/// The least and greatest **key** under a subtree.
///
/// The bounds a range walk needs are already carried by a B+ node's separators,
/// so this is not what prunes an ordinary range read. It earns its place in
/// comparisons *between two versions*: a subtree whose key span is disjoint from
/// the other side's cannot contribute a difference, so it can be dismissed
/// without descent (plan v5 §G-0d).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBounds<K> {
    /// `None` for the empty subtree.
    pub span: Option<(K, K)>,
}

impl<K: Ord + Clone, V> Measure<K, V> for KeyBounds<K> {
    fn empty() -> Self {
        KeyBounds { span: None }
    }
    fn entry(key: &K, _v: &V) -> Self {
        KeyBounds { span: Some((key.clone(), key.clone())) }
    }
    fn combine(&self, right: &Self) -> Self {
        let span = match (&self.span, &right.span) {
            (None, r) => r.clone(),
            (l, None) => l.clone(),
            (Some((llo, lhi)), Some((rlo, rhi))) => {
                Some((llo.min(rlo).clone(), lhi.max(rhi).clone()))
            }
        };
        KeyBounds { span }
    }
}

impl<K: Ord> KeyBounds<K> {
    /// Whether this subtree's keys can overlap `[lo, hi)`.
    pub fn overlaps(&self, lo: &K, hi: &K) -> bool {
        match &self.span {
            None => false,
            Some((slo, shi)) => slo < hi && shi >= lo,
        }
    }
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
