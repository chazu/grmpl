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
//!
//! Two measures are built in. [`Count`] is the size of a subtree. [`Extent`] is
//! Gold's wid proper: where in the coordinate space a subtree's keys lie, per
//! column, which is what lets a search prune on a column the tree is not
//! ordered by.

use grmpl_core::{Tuple, Value};

/// A monoidal summary of a subtree of `(K, V)` entries.
pub trait Measure<K, V>: Clone {
    /// The identity: the measure of the empty subtree.
    fn empty() -> Self;
    /// The measure of a single entry.
    fn entry(key: &K, val: &V) -> Self;
    /// Associative combination of two adjacent subtree measures (left ∘ right).
    fn combine(&self, right: &Self) -> Self;
    /// `*self = self.combine(right)`, for folding a node's run. A measure that
    /// owns heap state overrides it to fold in place.
    fn absorb(&mut self, right: &Self) {
        *self = self.combine(right);
    }
    /// `self.absorb(&Self::entry(key, val))`, for folding a leaf. A measure
    /// that owns heap state overrides it to skip building the entry's.
    fn absorb_entry(&mut self, key: &K, val: &V) {
        self.absorb(&Self::entry(key, val));
    }
    /// The measure of the same subtree with every **key** displaced by `by`
    /// (values never move). A measure that ignores keys returns itself.
    fn displace(&self, by: i64) -> Self;
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
    fn absorb(&mut self, right: &Self) {
        self.0.absorb(&right.0);
        self.1.absorb(&right.1);
    }
    fn absorb_entry(&mut self, key: &K, val: &V) {
        self.0.absorb_entry(key, val);
        self.1.absorb_entry(key, val);
    }
    fn displace(&self, by: i64) -> Self {
        (self.0.displace(by), self.1.displace(by))
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
    fn displace(&self, _by: i64) -> Self {
        *self
    }
}

/// **The extent of a subtree in entity space** — Gold's wid.
///
/// For each column, the least and greatest entity id among the subtree's
/// entity cells in that column (`None` where the column holds no entity). It
/// is the subtree's bounding box in the coordinate space a dsp moves, so it
/// displaces exactly as the keys do: every bound shifts by the same amount.
///
/// A tree is ordered by its whole key, so its separators prune only on the
/// lead column. The extent prunes on every column: a search for facts whose
/// third column lies in a span can skip any subtree whose box misses the span,
/// without reading it. How much that skips depends on how well the column
/// tracks the key order. A fact keyed by a room whose other columns name
/// nearby rooms (a template's exits) has tight boxes; a column of ids drawn
/// from all over the world has boxes as wide as the world.
///
/// Only entity cells are summarized. They are the coordinates the dsp acts on,
/// and they are fixed-size, so a frame's measures stay small; text and number
/// columns are left to Arrangements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extent(pub Vec<Option<(u64, u64)>>);

impl Extent {
    /// The bounds of column `col`, or `None` if no entity lies in it.
    pub fn column(&self, col: usize) -> Option<(u64, u64)> {
        self.0.get(col).copied().flatten()
    }

    /// Whether column `col` may hold an entity in `[lo, hi)`. A `false` is a
    /// proof that nothing under this subtree matches.
    pub fn meets(&self, col: usize, lo: u64, hi: u64) -> bool {
        self.column(col).is_some_and(|(min, max)| min < hi && lo <= max)
    }

    /// Whether every entity cell, in every column, lies in `[lo, hi)`.
    pub fn within(&self, lo: u64, hi: u64) -> bool {
        self.0.iter().flatten().all(|&(min, max)| lo <= min && max < hi)
    }
}

impl<V> Measure<Tuple, V> for Extent {
    fn empty() -> Self {
        Extent(Vec::new())
    }
    fn entry(key: &Tuple, _v: &V) -> Self {
        Extent(
            key.as_slice()
                .iter()
                .map(|cell| match cell {
                    Value::Ent(e) => Some((e.0, e.0)),
                    _ => None,
                })
                .collect(),
        )
    }
    fn combine(&self, right: &Self) -> Self {
        let mut out = self.clone();
        Measure::<Tuple, V>::absorb(&mut out, right);
        out
    }
    fn absorb(&mut self, right: &Self) {
        if self.0.len() < right.0.len() {
            self.0.resize(right.0.len(), None);
        }
        for (mine, theirs) in self.0.iter_mut().zip(&right.0) {
            *mine = match (*mine, *theirs) {
                (None, b) => b,
                (a, None) => a,
                (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
            };
        }
    }
    fn absorb_entry(&mut self, key: &Tuple, _val: &V) {
        let cells = key.as_slice();
        if self.0.len() < cells.len() {
            self.0.resize(cells.len(), None);
        }
        for (mine, cell) in self.0.iter_mut().zip(cells) {
            if let Value::Ent(e) = cell {
                *mine = Some(match *mine {
                    None => (e.0, e.0),
                    Some((lo, hi)) => (lo.min(e.0), hi.max(e.0)),
                });
            }
        }
    }
    /// Every bound moves with the entity cells it summarizes. The tree only
    /// displaces subtrees whose keys do not wrap, so the order of each pair
    /// survives.
    fn displace(&self, by: i64) -> Self {
        if by == 0 {
            return self.clone();
        }
        let by = by as u64;
        Extent(
            self.0
                .iter()
                .map(|b| b.map(|(lo, hi)| (lo.wrapping_add(by), hi.wrapping_add(by))))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grmpl_core::Entity;

    fn ent(n: u64) -> Value {
        Value::Ent(Entity(n))
    }

    fn of(rows: &[Tuple]) -> Extent {
        rows.iter().fold(Extent::default(), |acc, r| {
            Measure::<Tuple, ()>::combine(&acc, &Measure::<Tuple, ()>::entry(r, &()))
        })
    }

    #[test]
    fn extent_bounds_each_entity_column_and_skips_the_rest() {
        let rows = [
            Tuple::from([ent(10), Value::text("north"), ent(11)]),
            Tuple::from([ent(12), Value::text("south"), ent(7)]),
            Tuple::from([ent(15), Value::Int(3)]),
        ];
        let x = of(&rows);
        assert_eq!(x.column(0), Some((10, 15)));
        assert_eq!(x.column(1), None, "text and numbers are not summarized");
        assert_eq!(x.column(2), Some((7, 11)));
        assert!(x.meets(2, 0, 8) && !x.meets(2, 12, 20) && !x.meets(1, 0, 100));
        assert!(x.within(7, 16) && !x.within(8, 16));
    }

    #[test]
    fn extent_displaces_with_its_keys_and_is_a_monoid() {
        use crate::dsp::Displace;
        let rows = [
            Tuple::from([ent(10), ent(20)]),
            Tuple::from([ent(11), Value::Int(1), ent(4)]),
            Tuple::from([ent(13)]),
        ];
        let moved: Vec<Tuple> = rows.iter().map(|r| r.displace(1_000)).collect();
        assert_eq!(Measure::<Tuple, ()>::displace(&of(&rows), 1_000), of(&moved));
        // Associative, with the empty extent as identity.
        let (a, b, c) = (of(&rows[..1]), of(&rows[1..2]), of(&rows[2..]));
        let ab_c = Measure::<Tuple, ()>::combine(&Measure::<Tuple, ()>::combine(&a, &b), &c);
        let a_bc = Measure::<Tuple, ()>::combine(&a, &Measure::<Tuple, ()>::combine(&b, &c));
        assert_eq!(ab_c, a_bc);
        assert_eq!(Measure::<Tuple, ()>::combine(&Extent::default(), &a), a);
    }
}
