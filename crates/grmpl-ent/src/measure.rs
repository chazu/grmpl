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
//! where in the coordinate space a subtree's keys lie, per column, which is what
//! lets a search prune on a column the tree is not ordered by.

use grmpl_core::{Tuple, Value};

use crate::dsp::Displace;

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
    /// Which side of a k-d split on column `col` at `pivot` (a one-column key,
    /// in this measure's frame) every key it summarizes lies on, if it can
    /// tell: `Some(true)` all below, `Some(false)` all at or above. A `Some`
    /// is a proof; `None` claims nothing. This is what lets a cut or a compare
    /// in the k-d layout place a subtree without reading it.
    fn side_of(&self, _col: usize, _pivot: &K) -> Option<bool> {
        None
    }
    /// The measure of a **run** (`tree::leaf`): `n` rows, row `i` being
    /// `first` stepped `i` times by `stride`, each valued `val`. The default
    /// folds the rows; a measure of a run's shape computes it in `O(1)`.
    fn run(first: &K, stride: &K, n: u64, val: &V) -> Self
    where
        K: Displace,
    {
        let mut m = Self::empty();
        for i in 0..n {
            m.absorb_entry(&first.step(stride, i as i64), val);
        }
        m
    }
    /// The measure of a **hole**: keys reserved but holding no rows. Nothing,
    /// by default; a measure of where keys lie counts them, so that what it
    /// proves about a subtree's keys holds for its holes too.
    fn hole(_first: &K, _stride: &K, _n: u64) -> Self
    where
        K: Displace,
    {
        Self::empty()
    }
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
    fn side_of(&self, col: usize, pivot: &K) -> Option<bool> {
        self.0.side_of(col, pivot).or_else(|| self.1.side_of(col, pivot))
    }
    fn run(first: &K, stride: &K, n: u64, val: &V) -> Self
    where
        K: Displace,
    {
        (A::run(first, stride, n, val), B::run(first, stride, n, val))
    }
    fn hole(first: &K, stride: &K, n: u64) -> Self
    where
        K: Displace,
    {
        (A::hole(first, stride, n), B::hole(first, stride, n))
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
    fn run(_first: &K, _stride: &K, n: u64, _val: &V) -> Self
    where
        K: Displace,
    {
        Count(n)
    }
}

/// Per column, the least and greatest entity id in it, if any.
pub type Bounds = Vec<Option<(u64, u64)>>;

/// **The extent of a subtree in entity space.** grmpl's own summary: Gold's
/// content trees cache none (`docs/ENT-GOLD-AUDIT.md` §1.2).
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
///
/// Beside each column's bounds the extent counts the rows holding an entity
/// there, and the rows it summarizes. Where the two agree, every cell of the
/// column is an entity inside the bounds, so the extent can place the whole
/// subtree on one side of a k-d pivot ([`Measure::side_of`]). Where they do
/// not, a cell of another kind could sort anywhere, and it claims nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    /// Per column, the least and greatest entity id, or `None` if no entity
    /// lies in it.
    bounds: Bounds,
    /// Per column, the rows holding an entity there.
    ents: Vec<u64>,
    /// The rows summarized.
    rows: u64,
}

impl Extent {
    /// An extent from its persisted parts.
    pub fn from_parts(bounds: Bounds, ents: Vec<u64>, rows: u64) -> Extent {
        Extent { bounds, ents, rows }
    }

    /// Its persisted parts: the bounds, the per-column entity counts and the
    /// rows.
    pub fn parts(&self) -> (&Bounds, &[u64], u64) {
        (&self.bounds, &self.ents, self.rows)
    }

    /// The bounds of column `col`, or `None` if no entity lies in it.
    pub fn column(&self, col: usize) -> Option<(u64, u64)> {
        self.bounds.get(col).copied().flatten()
    }

    /// Whether column `col` may hold an entity in `[lo, hi)`. A `false` is a
    /// proof that nothing under this subtree matches.
    pub fn meets(&self, col: usize, lo: u64, hi: u64) -> bool {
        self.column(col).is_some_and(|(min, max)| min < hi && lo <= max)
    }

    /// Whether every entity cell, in every column, lies in `[lo, hi)`.
    pub fn within(&self, lo: u64, hi: u64) -> bool {
        self.bounds.iter().flatten().all(|&(min, max)| lo <= min && max < hi)
    }

    /// Fold in `n` rows stepped from `first` by `stride`.
    fn absorb_run(&mut self, first: &Tuple, stride: &Tuple, n: u64) {
        if n == 0 {
            return;
        }
        let last = first.step(stride, n as i64 - 1);
        let (a, b) = (first.as_slice(), last.as_slice());
        if self.bounds.len() < a.len() {
            self.bounds.resize(a.len(), None);
            self.ents.resize(a.len(), 0);
        }
        for (c, (x, y)) in a.iter().zip(b).enumerate() {
            if let (Value::Ent(x), Value::Ent(y)) = (x, y) {
                let (lo, hi) = (x.0.min(y.0), x.0.max(y.0));
                self.bounds[c] = Some(match self.bounds[c] {
                    None => (lo, hi),
                    Some((l, h)) => (l.min(lo), h.max(hi)),
                });
                self.ents[c] += n;
            }
        }
        self.rows += n;
    }

    /// The bounds of column `col` if every row summarized holds an entity
    /// there.
    fn full(&self, col: usize) -> Option<(u64, u64)> {
        (self.rows > 0 && self.ents.get(col) == Some(&self.rows)).then(|| self.column(col)).flatten()
    }
}

impl<V> Measure<Tuple, V> for Extent {
    fn empty() -> Self {
        Extent::default()
    }
    fn entry(key: &Tuple, val: &V) -> Self {
        let mut x = Extent::default();
        Measure::<Tuple, V>::absorb_entry(&mut x, key, val);
        x
    }
    fn combine(&self, right: &Self) -> Self {
        let mut out = self.clone();
        Measure::<Tuple, V>::absorb(&mut out, right);
        out
    }
    fn absorb(&mut self, right: &Self) {
        if self.bounds.len() < right.bounds.len() {
            self.bounds.resize(right.bounds.len(), None);
            self.ents.resize(right.bounds.len(), 0);
        }
        for (mine, theirs) in self.bounds.iter_mut().zip(&right.bounds) {
            *mine = match (*mine, *theirs) {
                (None, b) => b,
                (a, None) => a,
                (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
            };
        }
        for (mine, theirs) in self.ents.iter_mut().zip(&right.ents) {
            *mine += theirs;
        }
        self.rows += right.rows;
    }
    fn absorb_entry(&mut self, key: &Tuple, _val: &V) {
        let cells = key.as_slice();
        if self.bounds.len() < cells.len() {
            self.bounds.resize(cells.len(), None);
            self.ents.resize(cells.len(), 0);
        }
        for ((mine, n), cell) in self.bounds.iter_mut().zip(self.ents.iter_mut()).zip(cells) {
            if let Value::Ent(e) = cell {
                *mine = Some(match *mine {
                    None => (e.0, e.0),
                    Some((lo, hi)) => (lo.min(e.0), hi.max(e.0)),
                });
                *n += 1;
            }
        }
        self.rows += 1;
    }
    /// Every bound moves with the entity cells it summarizes. The tree only
    /// displaces subtrees whose keys do not wrap, so the order of each pair
    /// survives.
    fn displace(&self, by: i64) -> Self {
        if by == 0 {
            return self.clone();
        }
        let by = by as u64;
        Extent {
            bounds: self.bounds.iter().map(|b| b.map(|(lo, hi)| (lo.wrapping_add(by), hi.wrapping_add(by)))).collect(),
            ents: self.ents.clone(),
            rows: self.rows,
        }
    }
    /// A run's rows move linearly in every column, so each entity column's
    /// bounds are its first and last rows'.
    fn run(first: &Tuple, stride: &Tuple, n: u64, _val: &V) -> Self {
        let mut x = Extent::default();
        x.absorb_run(first, stride, n);
        x
    }
    /// A hole's keys are bounded as rows are, so a placement the extent
    /// proves holds for them too.
    fn hole(first: &Tuple, stride: &Tuple, n: u64) -> Self {
        let mut x = Extent::default();
        x.absorb_run(first, stride, n);
        x
    }
    /// Only a column every row holds an entity in, against an entity pivot.
    fn side_of(&self, col: usize, pivot: &Tuple) -> Option<bool> {
        let Some(Value::Ent(p)) = pivot.as_slice().first() else { return None };
        let (min, max) = self.full(col)?;
        if max < p.0 {
            Some(true)
        } else if min >= p.0 {
            Some(false)
        } else {
            None
        }
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

    #[test]
    fn an_extent_places_a_subtree_only_when_every_cell_is_an_entity() {
        let side = |rows: &[Tuple], col: usize, p: u64| Measure::<Tuple, ()>::side_of(&of(rows), col, &Tuple::from([ent(p)]));
        let rooms = [Tuple::from([ent(10), ent(3)]), Tuple::from([ent(12), ent(7)])];
        assert_eq!(side(&rooms, 0, 13), Some(true));
        assert_eq!(side(&rooms, 0, 10), Some(false));
        assert_eq!(side(&rooms, 0, 11), None, "the pivot falls inside the bounds");
        assert_eq!(side(&rooms, 1, 8), Some(true));
        // A number in the column could sort anywhere against an entity pivot,
        // and so could a missing cell: neither column claims a side.
        let mixed = [Tuple::from([ent(10), ent(3)]), Tuple::from([ent(12), Value::Int(1)])];
        assert_eq!(side(&mixed, 1, 100), None);
        let short = [Tuple::from([ent(10), ent(3)]), Tuple::from([ent(12)])];
        assert_eq!(side(&short, 1, 100), None);
        assert_eq!(side(&short, 0, 100), Some(true));
        // A displaced extent places as its keys do.
        let moved = Measure::<Tuple, ()>::displace(&of(&rooms), 1_000);
        assert_eq!(Measure::<Tuple, ()>::side_of(&moved, 0, &Tuple::from([ent(1_011)])), None);
        assert_eq!(Measure::<Tuple, ()>::side_of(&moved, 0, &Tuple::from([ent(1_013)])), Some(true));
    }
}
