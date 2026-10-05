//! **The k-d layout: splits on any column** (Gold's `SplitLoaf`; fidelity
//! gap G8).
//!
//! Gold's content tree is binary, and each internal node divides its region
//! on one dimension, so the tree partitions a cross space the way a k-d tree
//! does (`udanax-top.st` 8102–8107, *spaces* 9805). The B+ layout divides
//! only in key order, which is why an [`Extent`](crate::measure::Extent)
//! prunes well on the lead column and on nothing scattered. A tree in this
//! layout is built of [`Kind::Split`] nodes over wide leaves instead:
//!
//! * **Splits on the column of widest spread, at the median.** A node chooses
//!   the numeric column (entity, integer or float) whose values span the
//!   widest range, so a column the ancestors have narrowed gives way to one
//!   they have not. A column holding anything else is chosen only when no
//!   numeric column varies, by its count of distinct values. Ranges are
//!   compared raw, so an entity column and an integer column are measured in
//!   their own units.
//! * **Balanced by scapegoat rebuild**, standing in for Gold's splay, which
//!   rewrites shared nodes in place and so cannot be used on immutable,
//!   content-addressed nodes. An insert whose path has grown too deep for the
//!   size of a subtree rebuilds that subtree by median splits. A rebuild costs
//!   the subtree, amortized `O(log n)` per insert, and gives up the sharing
//!   under it.
//! * **Joined by rotation on one column.** Splits on the same column form a
//!   binary search tree over that column, and rotations among them are valid,
//!   so a join or a cut on column `0` (what a graft does) rebalances by
//!   weight-balanced rotation, as a join-based search tree does. That keeps a
//!   graft `O(log n)` in new nodes on the column-`0` levels and shares every
//!   subtree it does not cut. A split on another column cannot be rotated past,
//!   so a cut costs every node whose region straddles it: about
//!   `n^(1 - 1/k)` for `k` columns that alternate.
//!
//! Reads need no layout: a split on column `0` divides keys as a separator
//! does, and a walk visits both children of any other split (`tree.rs`,
//! `View`). Only writes need to know which layout a tree is in, since a lone
//! leaf is valid in both.

use std::cmp::Ordering;

use grmpl_core::{Tuple, Value};

use super::*;

/// Which shape a relation's Fact trees take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layout {
    /// The B+ tree ordered by the whole key: the default.
    Ordered,
    /// Binary splits on any column ([`Kind::Split`]).
    Kd,
}

crate::run_values_by_eq!(Layout);

/// [`Layout::Ordered`], unless the `kd-default` feature makes every law run
/// against the k-d layout.
impl Default for Layout {
    fn default() -> Layout {
        if cfg!(feature = "kd-default") {
            Layout::Kd
        } else {
            Layout::Ordered
        }
    }
}

impl Layout {
    /// The layout's persisted tag.
    pub fn tag(self) -> u8 {
        match self {
            Layout::Ordered => 0,
            Layout::Kd => 1,
        }
    }

    /// The layout a persisted tag names, if any.
    pub fn from_tag(tag: u8) -> Option<Layout> {
        match tag {
            0 => Some(Layout::Ordered),
            1 => Some(Layout::Kd),
            _ => None,
        }
    }
}

/// A pivot: the lone coordinate a split compares a key's column against.
fn pivot_of(v: Value) -> Tuple {
    Tuple::from([v])
}

/// The cell a key holds in column `col`; a missing cell sorts below any value.
fn cell(k: &Tuple, col: usize) -> Option<&Value> {
    k.as_slice().get(col)
}

/// Whether `levels` of splits above a leaf is too deep for a subtree of `size`
/// entries. A balanced subtree has about `log2(2·size / B)` levels (leaves
/// hold `B/2` to `B` entries); this allows `1 + log_{4/3}` of that, the
/// scapegoat bound for weight balance `3/4`. Integer arithmetic, so the shape
/// is the same on every platform.
fn too_deep(levels: usize, size: usize) -> bool {
    let leaves = ((2 * size) / B).max(1) as u128;
    let (mut p4, mut p3) = (1u128, 1u128);
    for _ in 1..levels {
        p4 = p4.saturating_mul(4);
        p3 = p3.saturating_mul(3);
        if p4 > p3.saturating_mul(leaves) {
            return true;
        }
    }
    false
}

/// Weight balance for joins (`α = 2/7`, just under `1 - 1/√2`, the bound
/// under which single and double rotations restore balance).
fn like(a: usize, b: usize) -> bool {
    let (wa, wb) = (a as u128 + 1, b as u128 + 1);
    7 * wa >= 2 * (wa + wb) && 7 * wb >= 2 * (wa + wb)
}

impl<V, M> Tree<Tuple, V, M>
where
    V: RunValue,
    M: Measure<Tuple, V>,
{
    // --- building -----------------------------------------------------------

    /// **A balanced k-d tree** over `entries`, which must have distinct keys:
    /// each node splits on the column of widest spread at its median, down to
    /// leaves of at most [`B`] items. Rows that form runs are folded first.
    pub fn kd_build(mut entries: Vec<(Tuple, V)>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut items: Vec<Item<Tuple, V>> = entries.into_iter().map(|(k, v)| Item::One(k, v)).collect();
        leaf::compress(&mut items);
        Self::build(items)
    }

    /// A balanced tree over `items` (in key order, disjoint). A run is cut
    /// only where a pivot falls inside it, so it keeps its compression.
    pub(super) fn build(items: Vec<Item<Tuple, V>>) -> Self {
        if items.is_empty() {
            return Tree::new();
        }
        let items = leaf::disjoint(items);
        if items.len() <= B {
            return Self::leaf(items);
        }
        let col = widest(&items);
        let pivot = pivot_of(median(&items, col));
        let (lo, hi) = leaf::split_on(items, col, &pivot);
        Self::split_node(col, pivot, Self::build(lo), Self::build(hi))
    }

    /// Every item of the subtree under `t`, keys in `t`'s parent frame, in
    /// key order.
    pub(super) fn items_of(t: &Self) -> Vec<Item<Tuple, V>> {
        fn walk<V: RunValue, M: Measure<Tuple, V>>(t: &Tree<Tuple, V, M>, off: i64, out: &mut Vec<Item<Tuple, V>>) {
            let Some(n) = t.root.as_deref() else { return };
            let off = off.wrapping_add(t.dsp);
            match n.kind() {
                Kind::Leaf(items) => out.extend(items.iter().map(|it| it.displaced(off))),
                Kind::Split { children, .. } => {
                    for c in children {
                        walk(c, off, out);
                    }
                }
                Kind::Internal { children, .. } => {
                    for c in children {
                        walk(c, off, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(t, 0, &mut out);
        out.sort_by(|a, b| a.lo_key().cmp(b.lo_key()));
        out
    }

    /// A split over `lo` and `hi` that tolerates an empty side, and folds two
    /// leaves small enough for one back into one.
    fn make(col: usize, pivot: Tuple, lo: Self, hi: Self) -> Self {
        if lo.is_empty() {
            return hi;
        }
        if hi.is_empty() {
            return lo;
        }
        let leaf_items = |t: &Self| match t.root.as_deref().map(|n| n.kind()) {
            Some(Kind::Leaf(items)) => Some(items.len()),
            _ => None,
        };
        if let (Some(a), Some(b)) = (leaf_items(&lo), leaf_items(&hi)) {
            // Siblings split on another column may interleave in key order;
            // only two whose keys do not are folded, side by side.
            let apart = lo.max_key() < hi.min_key() || hi.max_key() < lo.min_key();
            if a + b <= MIN && apart {
                let (mut x, mut y) = (Self::items_of(&lo), Self::items_of(&hi));
                if x[0].lo_key() > y[0].lo_key() {
                    std::mem::swap(&mut x, &mut y);
                }
                x.extend(y);
                leaf::compress(&mut x);
                return Self::leaf(x);
            }
        }
        Self::split_node(col, pivot, lo, hi)
    }

    // --- insert and remove --------------------------------------------------

    /// `key → val` inserted or replaced, in the k-d layout.
    pub fn kd_insert(&self, key: Tuple, val: V) -> Self {
        if self.is_empty() {
            return Self::leaf(vec![Item::One(key, val)]);
        }
        Self::kd_ins(self, key, val).0
    }

    /// Insert into the subtree under `t` (`key` and the result in `t`'s
    /// parent frame), returning it with the number of split levels from its
    /// root down to the leaf written.
    fn kd_ins(t: &Self, key: Tuple, val: V) -> (Self, usize) {
        match Self::open(t) {
            Open::Leaf(mut e) => {
                leaf::insert(&mut e, key, val);
                if e.len() > B {
                    leaf::compress(&mut e);
                }
                if e.len() <= B {
                    (Self::leaf(e), 0)
                } else {
                    let built = Self::build(e);
                    let depth = kd_height_of(&built);
                    (built, depth)
                }
            }
            Open::Split(col, pivot, [lo, hi]) => {
                let below = key.cmp_column(col, &pivot, 0) == Ordering::Less;
                let (lo, hi, levels) = if below {
                    let (c, d) = Self::kd_ins(&lo, key, val);
                    (c, hi, d)
                } else {
                    let (c, d) = Self::kd_ins(&hi, key, val);
                    (lo, c, d)
                };
                let node = Self::split_node(col, pivot, lo, hi);
                let levels = levels + 1;
                if too_deep(levels, node.len()) {
                    // The scapegoat: rebuild this subtree balanced.
                    let rebuilt = Self::build(Self::items_of(&node));
                    let depth = kd_height_of(&rebuilt);
                    (rebuilt, depth)
                } else {
                    (node, levels)
                }
            }
            Open::Internal(..) => unreachable!("a k-d insert met a B+ internal node"),
        }
    }

    /// `key` removed, in the k-d layout. An absent key returns the same
    /// shared version.
    pub fn kd_remove(&self, key: &Tuple) -> Self {
        Self::kd_rem(self, key, 0).unwrap_or_else(|| self.clone())
    }

    /// Remove `key` from the subtree under `t`, or `None` if absent. `off`
    /// carries `t`'s parent frame to `key`'s; the result is in `t`'s parent
    /// frame. A split left with one child becomes it, and two small leaves
    /// become one.
    fn kd_rem(t: &Self, key: &Tuple, off: i64) -> Option<Self> {
        let n = t.root.as_deref()?;
        let off = off.wrapping_add(t.dsp);
        match n.kind() {
            Kind::Leaf(items) => {
                let (i, j) = leaf::find_row(items, off, key)?;
                let Open::Leaf(mut e) = Self::open(t) else { unreachable!() };
                leaf::remove_at(&mut e, i, j);
                Some(if e.is_empty() { Tree::new() } else { Self::leaf(e) })
            }
            Kind::Split { col, pivot, children } => {
                let side = usize::from(key.cmp_column(*col, pivot, off) != Ordering::Less);
                let newc = Self::kd_rem(&children[side], key, off)?;
                let Open::Split(col, pivot, mut ch) = Self::open(t) else { unreachable!() };
                ch[side] = newc.relocate(t.dsp);
                let [lo, hi] = ch;
                Some(Self::make(col, pivot, lo, hi))
            }
            Kind::Internal { .. } => unreachable!("a k-d remove met a B+ internal node"),
        }
    }

    /// **Reserve `span`'s keys as a hole**, in the k-d layout (see
    /// [`reserve`](Tree::reserve)). The hole is routed down the splits, cut at
    /// any pivot that falls inside it, and merged into the leaves it reaches.
    pub fn kd_reserve(&self, span: Span<Tuple>) -> Option<Self> {
        if span.n == 0 {
            return Some(self.clone());
        }
        if !self.free(&span) {
            return None;
        }
        Some(Self::kd_put(self, vec![Item::Hole(span)]))
    }

    /// `items`, in `t`'s parent frame and holding no key `t` holds, merged
    /// into the subtree under `t`.
    fn kd_put(t: &Self, items: Vec<Item<Tuple, V>>) -> Self {
        if items.is_empty() {
            return t.clone();
        }
        if t.is_empty() {
            return Self::build(items);
        }
        match Self::open(t) {
            Open::Leaf(e) => Self::build(leaf::merge(e, items)),
            Open::Split(col, pivot, [lo, hi]) => {
                let (a, b) = leaf::split_on(items, col, &pivot);
                Self::split_node(col, pivot, Self::kd_put(&lo, a), Self::kd_put(&hi, b))
            }
            Open::Internal(..) => unreachable!("a k-d reserve met a B+ internal node"),
        }
    }

    // --- join and cut on one column ---------------------------------------

    /// The two children and pivot of `t`, opened into `t`'s parent frame, if
    /// `t` is a split on column `col`.
    fn expose(col: usize, t: &Self) -> Option<(Self, Tuple, Self)> {
        match t.root.as_deref()?.kind() {
            Kind::Split { col: c, .. } if *c == col => match Self::open(t) {
                Open::Split(_, pivot, [lo, hi]) => Some((lo, pivot, hi)),
                _ => unreachable!(),
            },
            _ => None,
        }
    }

    /// **Join on one column**: every key of `l` has column `col` below
    /// `pivot`'s coordinate and every key of `r` at or above it. Balanced by
    /// weight among the splits on `col` (a join-based search tree's join,
    /// rotating only past splits on the same column); a heavier side whose
    /// root splits on another column is hung unbalanced, since no rotation
    /// can pass it. Everything not on the rotated path stays shared.
    fn join_on(col: usize, l: Self, pivot: Tuple, r: Self) -> Self {
        if l.is_empty() {
            return r;
        }
        if r.is_empty() {
            return l;
        }
        if like(l.len(), r.len()) {
            return Self::make(col, pivot, l, r);
        }
        if l.len() > r.len() {
            Self::kd_join_right(col, l, pivot, r)
        } else {
            Self::kd_join_left(col, l, pivot, r)
        }
    }

    /// `join_on` with `tl` the heavier side: descend its right spine.
    fn kd_join_right(col: usize, tl: Self, pivot: Tuple, tr: Self) -> Self {
        let Some((l, k1, c)) = Self::expose(col, &tl) else {
            return Self::make(col, pivot, tl, tr);
        };
        let t2 = Self::join_on(col, c, pivot, tr);
        if like(l.len(), t2.len()) {
            return Self::make(col, k1, l, t2);
        }
        match Self::expose(col, &t2) {
            Some((l1, k2, r1)) if like(l.len(), l1.len()) && like(l.len() + l1.len(), r1.len()) => {
                // Single rotation left.
                Self::make(col, k2, Self::make(col, k1, l, l1), r1)
            }
            Some((l1, k2, r1)) => match Self::expose(col, &l1) {
                // Double rotation: right at `t2`, then left.
                Some((l2, k3, r2)) => {
                    Self::make(col, k3, Self::make(col, k1, l, l2), Self::make(col, k2, r2, r1))
                }
                None => Self::make(col, k1, l, Self::make(col, k2, l1, r1)),
            },
            None => Self::make(col, k1, l, t2),
        }
    }

    /// The mirror of [`kd_join_right`](Self::kd_join_right).
    fn kd_join_left(col: usize, tl: Self, pivot: Tuple, tr: Self) -> Self {
        let Some((c, k1, r)) = Self::expose(col, &tr) else {
            return Self::make(col, pivot, tl, tr);
        };
        let t2 = Self::join_on(col, tl, pivot, c);
        if like(r.len(), t2.len()) {
            return Self::make(col, k1, t2, r);
        }
        match Self::expose(col, &t2) {
            Some((l1, k2, r1)) if like(r.len(), r1.len()) && like(r.len() + r1.len(), l1.len()) => {
                Self::make(col, k2, l1, Self::make(col, k1, r1, r))
            }
            Some((l1, k2, r1)) => match Self::expose(col, &r1) {
                Some((l2, k3, r2)) => {
                    Self::make(col, k3, Self::make(col, k2, l1, l2), Self::make(col, k1, r2, r))
                }
                None => Self::make(col, k1, Self::make(col, k2, l1, r1), r),
            },
            None => Self::make(col, k1, t2, r),
        }
    }

    /// **Join at a column-`0` pivot**: every key of `left` lies below the
    /// one-column key `at` and every key of `right` at or above it, as a
    /// graft's pieces do around its target block.
    pub fn kd_join_at(left: &Self, at: &Tuple, right: &Self) -> Self {
        debug_assert_eq!(at.as_slice().len(), 1, "a pivot is a one-column key");
        Self::join_on(0, left.clone(), at.clone(), right.clone())
    }

    /// The keys below `key` and those at or above it, in the k-d layout.
    /// Splits on column `0` are cut by join, so they stay balanced and share
    /// everything off the cut; a split on another column is cut on both
    /// sides. A subtree wholly on one side is shared as it is.
    pub fn kd_split(&self, key: &Tuple) -> (Self, Self) {
        Self::kd_split_at(self, key)
    }

    fn kd_split_at(t: &Self, key: &Tuple) -> (Self, Self) {
        if t.is_empty() {
            return (Tree::new(), Tree::new());
        }
        // A one-column key is a pivot on column 0, so the subtree's summary
        // may place it whole, unread.
        if key.as_slice().len() == 1 {
            match t.measure().side_of(0, key) {
                Some(true) => return (t.clone(), Tree::new()),
                Some(false) => return (Tree::new(), t.clone()),
                None => {}
            }
        }
        match Self::open(t) {
            Open::Leaf(e) => {
                let (l, r) = leaf::split_at(e, key);
                if l.is_empty() {
                    return (Tree::new(), t.clone());
                }
                if r.is_empty() {
                    return (t.clone(), Tree::new());
                }
                (Self::leaf(l), Self::leaf(r))
            }
            Open::Split(0, pivot, [lo, hi]) => {
                if *key <= pivot {
                    // All of `hi` is at or above the pivot, so at or above `key`.
                    let (a, b) = Self::kd_split_at(&lo, key);
                    if a.is_empty() {
                        return (Tree::new(), t.clone());
                    }
                    (a, Self::join_on(0, b, pivot, hi))
                } else {
                    // All of `lo` is below the pivot, so below `key`.
                    let (a, b) = Self::kd_split_at(&hi, key);
                    if b.is_empty() {
                        return (t.clone(), Tree::new());
                    }
                    (Self::join_on(0, lo, pivot, a), b)
                }
            }
            Open::Split(col, pivot, [lo, hi]) => {
                let (a1, b1) = Self::kd_split_at(&lo, key);
                let (a2, b2) = Self::kd_split_at(&hi, key);
                if b1.is_empty() && b2.is_empty() {
                    return (t.clone(), Tree::new());
                }
                if a1.is_empty() && a2.is_empty() {
                    return (Tree::new(), t.clone());
                }
                (Self::make(col, pivot.clone(), a1, a2), Self::make(col, pivot, b1, b2))
            }
            Open::Internal(..) => unreachable!("a k-d split met a B+ internal node"),
        }
    }

    /// **Virtual copy in the k-d layout** ([`graft`](Tree::graft)'s
    /// counterpart): copy the entries in `[lo, hi)` to the same keys displaced
    /// by `by`. `lo` and `hi` are one-column keys, a block of the lead column.
    /// The block is cut out, relocated in `O(1)`, and joined in at column-`0`
    /// pivots, so the copy shares every node it was not cut from.
    ///
    /// `None`, changing nothing, if the target is occupied or the shift does
    /// not carry the block cleanly into it.
    pub fn kd_graft(&self, lo: &Tuple, hi: &Tuple, by: i64) -> Option<Self> {
        debug_assert!(lo.as_slice().len() == 1 && hi.as_slice().len() == 1, "a k-d graft moves a block of the lead column");
        if lo >= hi {
            return Some(self.clone());
        }
        let (_, rest) = self.kd_split(lo);
        let (span, _) = rest.kd_split(hi);
        if span.is_empty() {
            return Some(self.clone());
        }
        let (tlo, thi) = (lo.displace(by), hi.displace(by));
        if tlo >= thi || self.any_in(&tlo, &thi) {
            return None;
        }
        let copy = span.relocate(by);
        if copy.min_key() < tlo || copy.max_key() >= thi {
            return None;
        }
        let (below, above) = self.kd_split(&tlo);
        Some(Self::join_on(0, Self::join_on(0, below, tlo, copy), thi, above))
    }

    // --- reads that only the k-d layout can prune -------------------------

    /// **A range on any column**: the entries whose column `col` holds a value
    /// in `[lo, hi)`, in key order. Splits on `col` narrow the walk as a
    /// separator does (Gold's `limitRegion` narrowed on the way down); any
    /// other split is entered on both sides.
    pub fn kd_range_on(&self, col: usize, lo: &Value, hi: &Value) -> Vec<(Tuple, V)> {
        let mut out = Vec::new();
        if lo < hi {
            let (lo, hi) = (pivot_of(lo.clone()), pivot_of(hi.clone()));
            Self::range_on_into(self, col, &lo, &hi, 0, &mut out);
            in_key_order(&mut out);
        }
        out
    }

    fn range_on_into(t: &Self, col: usize, lo: &Tuple, hi: &Tuple, off: i64, out: &mut Vec<(Tuple, V)>) {
        let Some(n) = t.root.as_deref() else { return };
        let off = off.wrapping_add(t.dsp);
        // A subtree whose summary puts it wholly below `lo` or at or above
        // `hi` holds nothing in the range.
        let m = if off == 0 { n.measure.clone() } else { n.measure.displace(off) };
        if m.side_of(col, lo) == Some(true) || m.side_of(col, hi) == Some(false) {
            return;
        }
        match n.kind() {
            Kind::Leaf(items) => {
                for it in items {
                    if it.rows() > 1 {
                        // A run is placed by its own extent before its rows.
                        let m = it.measure::<M>().displace(off);
                        if m.side_of(col, lo) == Some(true) || m.side_of(col, hi) == Some(false) {
                            continue;
                        }
                    }
                    for (k, v) in it.each_row(off) {
                        let Some(x) = cell(&k, col) else { continue };
                        let x = pivot_of(x.clone());
                        if *lo <= x && x < *hi {
                            out.push((k, v.clone()));
                        }
                    }
                }
            }
            Kind::Split { col: c, pivot, children: [below, above] } => {
                let p = moved(pivot, off);
                // `below` holds cells under the pivot, `above` cells at or over it.
                if *c != col || *lo < *p {
                    Self::range_on_into(below, col, lo, hi, off, out);
                }
                if *c != col || *hi > *p {
                    Self::range_on_into(above, col, lo, hi, off, out);
                }
            }
            Kind::Internal { children, .. } => {
                for c in children {
                    Self::range_on_into(c, col, lo, hi, off, out);
                }
            }
        }
    }
}

/// The number of split levels above the deepest leaf of a k-d tree.
pub(super) fn kd_height_of<K, V, M>(t: &Tree<K, V, M>) -> usize {
    match t.root.as_deref().map(|n| n.kind()) {
        Some(Kind::Split { children, .. }) => 1 + children.iter().map(kd_height_of).max().unwrap_or(0),
        _ => 0,
    }
}

/// What an item weighs in a median: its rows, or the keys a hole reserves.
fn weight<V>(it: &Item<Tuple, V>) -> u64 {
    it.rows() + it.reserved()
}

/// The column of widest spread among `items` (at least two keys, distinct).
///
/// A numeric column (every cell an entity, every cell an integer, or every
/// cell a float) spreads as far as its range; the widest positive range wins,
/// the lower column on a tie. Only if none varies does a column of anything
/// else count, by its number of distinct cells (a missing cell is one). A run
/// moves linearly, so its first and last keys bound it in every column.
fn widest<V>(items: &[Item<Tuple, V>]) -> usize {
    let ends: Vec<Tuple> = items.iter().flat_map(|it| [it.lo_key().clone(), it.hi_key().into_owned()]).collect();
    let arity = ends.iter().map(|k| k.as_slice().len()).max().unwrap_or(0);
    let mut best: Option<(usize, f64)> = None;
    for col in 0..arity {
        if let Some(range) = numeric_range(&ends, col) {
            if range > 0.0 && best.is_none_or(|(_, r)| range > r) {
                best = Some((col, range));
            }
        }
    }
    if let Some((col, _)) = best {
        return col;
    }
    let mut best: Option<(usize, usize)> = None;
    for col in 0..arity {
        let mut cells: Vec<Option<&Value>> = ends.iter().map(|k| cell(k, col)).collect();
        cells.sort();
        cells.dedup();
        let distinct = cells.len();
        if distinct > 1 && best.is_none_or(|(_, d)| distinct > d) {
            best = Some((col, distinct));
        }
    }
    best.map(|(col, _)| col).expect("distinct keys differ in some column")
}

/// The range of column `col` if every key holds a number of one kind there.
fn numeric_range(keys: &[Tuple], col: usize) -> Option<f64> {
    let mut cells = keys.iter().map(|k| cell(k, col));
    match cells.next()?? {
        Value::Ent(first) => {
            let (mut lo, mut hi) = (first.0, first.0);
            for c in cells {
                let Some(Value::Ent(e)) = c else { return None };
                lo = lo.min(e.0);
                hi = hi.max(e.0);
            }
            Some((hi - lo) as f64)
        }
        Value::Int(first) => {
            let (mut lo, mut hi) = (*first, *first);
            for c in cells {
                let Some(Value::Int(n)) = c else { return None };
                lo = lo.min(*n);
                hi = hi.max(*n);
            }
            Some((hi as i128 - lo as i128) as f64)
        }
        Value::Float(first) => {
            let (mut lo, mut hi) = (first.get(), first.get());
            for c in cells {
                let Some(Value::Float(x)) = c else { return None };
                lo = lo.min(x.get());
                hi = hi.max(x.get());
            }
            Some(hi - lo)
        }
        _ => None,
    }
}

/// How much of `items` weighs below a pivot `p` on column `col`. A run moves
/// linearly, so the keys below the pivot are a prefix of it or a suffix,
/// found by bisection.
fn weight_below<V>(items: &[Item<Tuple, V>], col: usize, p: &Tuple) -> u64 {
    let below = |k: &Tuple| k.cmp_column(col, p, 0) == Ordering::Less;
    items
        .iter()
        .map(|it| match it {
            Item::One(k, _) => u64::from(below(k)),
            Item::Run(s, _) | Item::Hole(s) => {
                let first = below(&s.first);
                let (mut a, mut b) = (0u64, s.n);
                while a < b {
                    let mid = a + (b - a) / 2;
                    if below(&s.row(mid)) == first {
                        a = mid + 1;
                    } else {
                        b = mid;
                    }
                }
                if first { a } else { s.n - a }
            }
        })
        .sum()
}

/// A pivot on column `col` nearest the weighted median of `items` that leaves
/// weight on both sides. The column must hold at least two distinct cells.
fn median<V>(items: &[Item<Tuple, V>], col: usize) -> Value {
    let total: u64 = items.iter().map(weight).sum();
    // Every cell a run or hole can step is an entity or integer: find the
    // pivot by bisection on the value. Anything else is constant per item.
    let ends: Vec<Tuple> = items.iter().flat_map(|it| [it.lo_key().clone(), it.hi_key().into_owned()]).collect();
    let ints: Option<Vec<(bool, i128)>> = ends
        .iter()
        .map(|k| match cell(k, col)? {
            Value::Ent(e) => Some((true, e.0 as i128)),
            Value::Int(n) => Some((false, *n as i128)),
            _ => None,
        })
        .collect();
    if let Some(ints) = ints.filter(|v| v.iter().all(|(e, _)| *e == v[0].0)) {
        let ent = ints[0].0;
        let at = |x: i128| if ent { Value::Ent(grmpl_core::Entity(x as u64)) } else { Value::Int(x as i64) };
        let (lo, hi) = (ints.iter().map(|x| x.1).min().unwrap(), ints.iter().map(|x| x.1).max().unwrap());
        // The least pivot in (lo, hi] with half the weight below it.
        let (mut a, mut b) = (lo + 1, hi);
        while a < b {
            let mid = a + (b - a) / 2;
            if 2 * weight_below(items, col, &pivot_of(at(mid))) >= total {
                b = mid;
            } else {
                a = mid + 1;
            }
        }
        return at(a);
    }
    // Constant cells: the weighted median among them, nudged so both sides
    // keep weight.
    let mut cells: Vec<(Option<Value>, u64)> = items.iter().map(|it| (cell(it.lo_key(), col).cloned(), weight(it))).collect();
    cells.sort_by(|a, b| a.0.cmp(&b.0));
    let mut acc = 0;
    let mut pick = None;
    for (i, (c, w)) in cells.iter().enumerate() {
        if i > 0 && c.is_some() && cells[i - 1].0 != *c && acc > 0 {
            pick = c.clone();
            if 2 * acc >= total {
                break;
            }
        }
        acc += w;
    }
    pick.expect("the column holds at least two distinct cells")
}

/// One item of a [`kd_diff_into`](Tree::kd_diff_into) side: a whole subtree
/// under a parent frame with the bounds its ancestors' splits put on it, or
/// one entry with its key in the absolute frame.
enum Piece<'a, K, V, M> {
    Node(&'a Tree<K, V, M>, i64, Region<K>),
    Entry(K, &'a V),
}

/// What the splits above a piece say about it (Gold's `limitRegion`): for a
/// column, every key's cell is at or above `lo` and below `hi` (pivots in the
/// absolute frame).
type Region<K> = Vec<(usize, Option<K>, Option<K>)>;

/// `region` narrowed to the side of a split on `col` at `pivot`.
fn narrowed<K: Clone + Ord>(region: &Region<K>, col: usize, pivot: &K, below: bool) -> Region<K> {
    let mut r = region.clone();
    let slot = match r.iter().position(|(c, ..)| *c == col) {
        Some(i) => i,
        None => {
            r.push((col, None, None));
            r.len() - 1
        }
    };
    let (_, lo, hi) = &mut r[slot];
    if below {
        if hi.as_ref().is_none_or(|h| pivot < h) {
            *hi = Some(pivot.clone());
        }
    } else if lo.as_ref().is_none_or(|l| pivot > l) {
        *lo = Some(pivot.clone());
    }
    r
}

/// Which side of a split on `col` at `pivot` a region lies wholly on, if it
/// is known: `Some(true)` below, `Some(false)` at or above.
fn side_of<K: Ord>(region: &Region<K>, col: usize, pivot: &K) -> Option<bool> {
    let (_, lo, hi) = region.iter().find(|(c, ..)| *c == col)?;
    if hi.as_ref().is_some_and(|h| h <= pivot) {
        return Some(true);
    }
    if lo.as_ref().is_some_and(|l| l >= pivot) {
        return Some(false);
    }
    None
}

impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Displace,
    V: RunValue,
    M: Measure<K, V>,
{
    /// [`diff`](Tree::diff) for k-d trees, **shape-independent** as the B+
    /// frontier walk is. Each side is a set of pieces covering one region.
    /// Pieces that are the same node at the same absolute position cancel.
    /// Otherwise the largest split on either side divides both sides at its
    /// pivot: its own children go one to each side, a piece whose bounds put
    /// it wholly on one side goes there unread, and only a piece straddling
    /// the pivot is opened. So two versions are compared down their differing
    /// paths and their seams, however differently their splits were rebuilt.
    /// Leaves and entries left over are compared entry by entry. The caller
    /// sorts the result.
    pub(super) fn kd_diff_into(a: &Self, ia: i64, b: &Self, ib: i64, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        Self::diff_pieces(side(a, ia), side(b, ib), out);
    }

    fn diff_pieces<'a>(xs: Vec<Piece<'a, K, V, M>>, ys: Vec<Piece<'a, K, V, M>>, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        let (xs, ys) = cancel_shared(xs, ys);
        if xs.is_empty() && ys.is_empty() {
            return;
        }
        // The largest split on either side divides both.
        let splitter = xs
            .iter()
            .chain(ys.iter())
            .filter_map(|p| match p {
                Piece::Node(t, off, _) => match t.root.as_deref()?.kind() {
                    Kind::Split { col, pivot, .. } => Some((t.len(), *col, pivot.displace(off.wrapping_add(t.dsp)))),
                    _ => None,
                },
                Piece::Entry(..) => None,
            })
            .max_by_key(|(size, ..)| *size);
        let Some((_, col, pivot)) = splitter else {
            return Self::diff_entries(xs, ys, out);
        };
        let (xl, xh) = Self::partition(xs, col, &pivot);
        let (yl, yh) = Self::partition(ys, col, &pivot);
        Self::diff_pieces(xl, yl, out);
        Self::diff_pieces(xh, yh, out);
    }

    /// Divide `pieces` at a split on `col` whose pivot is `pivot` (absolute):
    /// those below it and those at or above it.
    #[allow(clippy::type_complexity)]
    fn partition<'a>(
        pieces: Vec<Piece<'a, K, V, M>>,
        col: usize,
        pivot: &K,
    ) -> (Vec<Piece<'a, K, V, M>>, Vec<Piece<'a, K, V, M>>) {
        let (mut lo, mut hi) = (Vec::new(), Vec::new());
        for p in pieces {
            match p {
                Piece::Entry(k, v) => {
                    if k.cmp_column(col, pivot, 0) == Ordering::Less {
                        lo.push(Piece::Entry(k, v));
                    } else {
                        hi.push(Piece::Entry(k, v));
                    }
                }
                Piece::Node(t, off, region) => Self::partition_node(t, off, region, col, pivot, &mut lo, &mut hi),
            }
        }
        (lo, hi)
    }

    #[allow(clippy::too_many_arguments)]
    fn partition_node<'a>(
        t: &'a Self,
        poff: i64,
        region: Region<K>,
        col: usize,
        pivot: &K,
        lo: &mut Vec<Piece<'a, K, V, M>>,
        hi: &mut Vec<Piece<'a, K, V, M>>,
    ) {
        let Some(n) = t.root.as_deref() else { return };
        let off = poff.wrapping_add(t.dsp);
        // The bounds the splits above it set, then its own summary: either can
        // place the piece without reading it.
        let placed = side_of(&region, col, pivot).or_else(|| {
            if off == 0 {
                n.measure.side_of(col, pivot)
            } else {
                n.measure.displace(off).side_of(col, pivot)
            }
        });
        match placed {
            Some(true) => return lo.push(Piece::Node(t, poff, region)),
            Some(false) => return hi.push(Piece::Node(t, poff, region)),
            None => {}
        }
        match n.kind() {
            Kind::Leaf(items) => {
                // A run moves linearly, so its two ends say which side it is on
                // unless the pivot falls inside it.
                let below = |k: &K| k.displace(off).cmp_column(col, pivot, 0) == Ordering::Less;
                let sides: Vec<(bool, bool)> = items.iter().map(|it| (below(it.lo_key()), below(&it.hi_key()))).collect();
                if sides.iter().all(|&(a, b)| a && b) {
                    lo.push(Piece::Node(t, poff, narrowed(&region, col, pivot, true)));
                } else if sides.iter().all(|&(a, b)| !a && !b) {
                    hi.push(Piece::Node(t, poff, narrowed(&region, col, pivot, false)));
                } else {
                    for it in items {
                        for (k, v) in it.each_row(off) {
                            let e = Piece::Entry(k, v);
                            match &e {
                                Piece::Entry(k, _) if k.cmp_column(col, pivot, 0) == Ordering::Less => lo.push(e),
                                _ => hi.push(e),
                            }
                        }
                    }
                }
            }
            Kind::Split { col: c, pivot: q, children: [below, above] } => {
                let q = q.displace(off);
                let (rb, ra) = (narrowed(&region, *c, &q, true), narrowed(&region, *c, &q, false));
                Self::partition_node(below, off, rb, col, pivot, lo, hi);
                Self::partition_node(above, off, ra, col, pivot, lo, hi);
            }
            Kind::Internal { children, .. } => {
                for c in children {
                    Self::partition_node(c, off, region.clone(), col, pivot, lo, hi);
                }
            }
        }
    }

    /// Compare what is left entry by entry.
    fn diff_entries(xs: Vec<Piece<'_, K, V, M>>, ys: Vec<Piece<'_, K, V, M>>, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        let (mut xs, mut ys) = (flat(xs).into_iter().peekable(), flat(ys).into_iter().peekable());
        loop {
            let ord = match (xs.peek(), ys.peek()) {
                (None, None) => return,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(x), Some(y)) => x.0.cmp(&y.0),
            };
            match ord {
                Ordering::Less => {
                    let (k, v) = xs.next().unwrap();
                    out.push((k, Some(v.clone()), None));
                }
                Ordering::Greater => {
                    let (k, v) = ys.next().unwrap();
                    out.push((k, None, Some(v.clone())));
                }
                Ordering::Equal => {
                    let ((k, va), (_, vb)) = (xs.next().unwrap(), ys.next().unwrap());
                    if va != vb {
                        out.push((k, Some(va.clone()), Some(vb.clone())));
                    }
                }
            }
        }
    }
}


/// A side with one piece for `t`, or none if it is empty.
fn side<K, V, M>(t: &Tree<K, V, M>, off: i64) -> Vec<Piece<'_, K, V, M>> {
    if t.root.is_none() {
        Vec::new()
    } else {
        vec![Piece::Node(t, off, Vec::new())]
    }
}

/// Every entry of `ps`, keys in the absolute frame, in key order.
fn flat<'a, K: Displace, V, M>(ps: Vec<Piece<'a, K, V, M>>) -> Vec<(K, &'a V)> {
    let mut all = Vec::new();
    for p in ps {
        match p {
            Piece::Entry(k, v) => all.push((k, v)),
            Piece::Node(t, off, _) => collect_all(t, off, &mut all),
        }
    }
    all.sort_by(|p, q| p.0.cmp(&q.0));
    all
}

/// Drop every pair of pieces, one from each side, that are the same node at
/// the same absolute position: by pointer, or by memoized content key.
#[allow(clippy::type_complexity)]
fn cancel_shared<'a, K, V, M>(
    xs: Vec<Piece<'a, K, V, M>>,
    ys: Vec<Piece<'a, K, V, M>>,
) -> (Vec<Piece<'a, K, V, M>>, Vec<Piece<'a, K, V, M>>) {
    use std::collections::HashMap;
    let mut by_ptr: HashMap<(usize, i64), usize> = HashMap::new();
    let mut by_ck: HashMap<(ContentKey, i64), usize> = HashMap::new();
    for (j, y) in ys.iter().enumerate() {
        if let Piece::Node(t, off, _) = y {
            if let Some(n) = t.root.as_ref() {
                let at = off.wrapping_add(t.dsp);
                by_ptr.insert((Arc::as_ptr(n) as usize, at), j);
                if let Some(ck) = n.ck.get() {
                    by_ck.insert((*ck, at), j);
                }
            }
        }
    }
    if by_ptr.is_empty() {
        return (xs, ys);
    }
    let mut gone = vec![false; ys.len()];
    let mut kept = Vec::with_capacity(xs.len());
    for x in xs {
        if let Piece::Node(t, off, _) = &x {
            if let Some(n) = t.root.as_ref() {
                let at = off.wrapping_add(t.dsp);
                let hit = by_ptr
                    .get(&(Arc::as_ptr(n) as usize, at))
                    .or_else(|| n.ck.get().and_then(|ck| by_ck.get(&(*ck, at))))
                    .copied();
                if let Some(j) = hit.filter(|j| !gone[*j]) {
                    gone[j] = true;
                    continue;
                }
            }
        }
        kept.push(x);
    }
    let ys = ys.into_iter().zip(gone).filter(|(_, g)| !g).map(|(y, _)| y).collect();
    (kept, ys)
}

#[cfg(test)]
impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Displace + std::fmt::Debug,
    V: RunValue,
    M: Measure<K, V>,
{
    /// Assert the k-d invariants through the displaced frames: no B+ internal
    /// node, no empty leaf or child, leaves within arity and sorted, every key
    /// on the side of each ancestor's pivot its split sends it, and cached
    /// sizes that match.
    pub(crate) fn kd_check(&self) {
        let mut sides = Vec::new();
        if self.root.is_some() {
            Self::kd_check_node(self, 0, &mut sides);
        }
        let keys: Vec<K> = self.iter().map(|(k, _)| k).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys out of order: {keys:?}");
        assert_eq!(keys.len(), self.len(), "cached size disagrees with the entries");
    }

    /// `sides` holds each ancestor split as `(col, pivot at its absolute
    /// position, whether this subtree is below it)`.
    fn kd_check_node(t: &Self, off: i64, sides: &mut Vec<(usize, K, bool)>) {
        let n = t.root.as_deref().expect("no empty subtree in a k-d tree");
        let off = off.wrapping_add(t.dsp);
        match n.kind() {
            Kind::Leaf(items) => {
                assert!(!items.is_empty(), "empty leaf");
                assert!(items.len() <= B, "leaf over arity");
                leaf::check_items(items);
                // A run moves linearly: its ends bound it on every side.
                for it in items {
                    for k in [it.lo_key().displace(off), it.hi_key().displace(off)] {
                        for (col, pivot, below) in sides.iter() {
                            let is_below = k.cmp_column(*col, pivot, 0) == Ordering::Less;
                            assert_eq!(is_below, *below, "key {k:?} on the wrong side of a split on {col} at {pivot:?}");
                        }
                    }
                }
                let (_, size, reserved) = leaf::summary::<K, V, M>(items);
                assert_eq!((n.size, n.reserved), (size, reserved));
            }
            Kind::Split { col, pivot, children } => {
                let p = pivot.displace(off);
                for (i, c) in children.iter().enumerate() {
                    sides.push((*col, p.clone(), i == 0));
                    Self::kd_check_node(c, off, sides);
                    sides.pop();
                }
                assert_eq!(n.size, children[0].len() + children[1].len(), "cached size disagrees with the children");
                assert_eq!(n.reserved, children[0].reserved() + children[1].reserved(), "cached holes disagree");
            }
            Kind::Internal { .. } => panic!("a B+ internal node inside a k-d tree"),
        }
    }
}
