//! **Leaves of items: rows, runs and holes** (Gold's loaves; fidelity gap G9).
//!
//! Gold's content tree ends in loaves of several kinds. A `RegionLoaf` maps a
//! whole region to one shared range element, an `OVirtualLoaf` holds a
//! primitive array and fakes an object per key only when asked, and an
//! `OPartialLoaf` is a region of placeholders whose content arrives later
//! (`udanax-top.st` 9285–9307, 9072–9131, 8779–8871). A grmpl leaf is a run of
//! **items**, each one of:
//!
//! * a **row**, one `(key, value)` entry;
//! * a **run** ([`Span`] and a value): `n` rows, row `i` being the first key
//!   stepped `i` times by a per-column stride ([`Displace::step`]), all with
//!   one value. A run is run-length (`RegionLoaf`) and lazy (`OVirtualLoaf`)
//!   at once: its rows are computed when a read reaches them, and one item
//!   stands for any number of them. Facts are identified by value in grmpl,
//!   so a run is a representation, never an identity: it holds exactly the
//!   rows it computes;
//! * a **hole** (a [`Span`] alone): keys reserved but holding no rows, as an
//!   `OPartialLoaf`'s placeholders are. Reads skip a hole; writing a row at
//!   one of its keys fills that key.
//!
//! **Runs form by themselves, where a tree asks for them.** A relation opts in
//! (`EntStore::set_runs`); every write here takes a `runs` flag, and without
//! it rows never fold. With it, a row that continues the run before it, or
//! starts the one after it, joins it; three rows in a step become a run; a
//! leaf that overflows folds its runs before it splits; a write inside a run
//! splits it around the row. So a block of ids loaded row by row ends as one
//! item, and the tree's balance and arity count items, not rows. A key type
//! whose [`stride_to`](Displace::stride_to) is `None` never forms a run.
//!
//! Runs are opt-in because they coarsen identity, which is node sharing: a
//! relation folded into a few nodes leaves `backfollow` and the identity
//! compare nothing to find of its copies (`docs/ENT-FIDELITY-STEP-7.md`).
//!
//! Every function here takes a leaf's items in their stored frame and an
//! offset carrying them to the caller's, as the tree's walks do: stored keys
//! move up to the query, never the query down.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::dsp::Displace;
use crate::measure::Measure;

/// Items below a key and items at or above it.
pub type Halves<K, V> = (Vec<Item<K, V>>, Vec<Item<K, V>>);

/// The shortest run worth an item: two rows are as cheap as one run.
pub const MIN_RUN: u64 = 3;

/// Values a run's rows may share. Rows form a run only if their values are
/// [`same`](RunValue::same); a value type that never sits in a tree whose
/// keys form runs answers `false`.
pub trait RunValue: Clone {
    fn same(&self, other: &Self) -> bool;
}

/// Equality is sameness.
#[macro_export]
#[doc(hidden)]
macro_rules! run_values_by_eq {
    ($($t:ty),* $(,)?) => {$(
        impl $crate::tree::RunValue for $t {
            fn same(&self, other: &Self) -> bool {
                self == other
            }
        }
    )*};
}

run_values_by_eq!(i64, u64, u32, (), bool, grmpl_core::Value, grmpl_core::Tuple);

impl<A: RunValue, B: RunValue> RunValue for (A, B) {
    fn same(&self, other: &Self) -> bool {
        self.0.same(&other.0) && self.1.same(&other.1)
    }
}

/// A tree held as a value is never the same as another for a run's sake:
/// trees keyed by tuples hold numbers and values, not trees.
impl<K, V, M> RunValue for super::Tree<K, V, M> {
    fn same(&self, _other: &Self) -> bool {
        false
    }
}

/// `n` keys, the `i`-th being `first` stepped `i` times by `stride`, in
/// ascending order. `stride` is in no frame: a displacement moves `first` and
/// every row with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span<K> {
    pub first: K,
    pub stride: K,
    pub n: u64,
}

/// One item of a leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item<K, V> {
    /// One row.
    One(K, V),
    /// Rows stepped along a span, each holding the value.
    Run(Span<K>, V),
    /// Keys reserved, holding no rows.
    Hole(Span<K>),
}

impl<K: Displace> Span<K> {
    /// Row `i`, in the span's frame.
    pub fn row(&self, i: u64) -> K {
        if i == 0 {
            self.first.clone()
        } else {
            self.first.step(&self.stride, i as i64)
        }
    }

    pub fn last(&self) -> K {
        self.row(self.n - 1)
    }

    /// Rows `[lo, hi)` of this span.
    pub fn slice(&self, lo: u64, hi: u64) -> Span<K> {
        Span { first: self.row(lo), stride: self.stride.clone(), n: hi - lo }
    }

    fn displaced(&self, by: i64) -> Span<K> {
        Span { first: self.first.displace(by), stride: self.stride.clone(), n: self.n }
    }

    /// How many rows, moved up by `off`, lie below `key`.
    fn below(&self, off: i64, key: &K) -> u64 {
        let (mut lo, mut hi) = (0u64, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row(mid).cmp_displaced(off, key) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// How many rows, moved up by `off`, lie at or below `key`.
    fn at_or_below(&self, off: i64, key: &K) -> u64 {
        let i = self.below(off, key);
        if i < self.n && self.row(i).cmp_displaced(off, key) == Ordering::Equal {
            i + 1
        } else {
            i
        }
    }

    /// The index of `key` among the rows, moved up by `off`.
    fn find(&self, off: i64, key: &K) -> Option<u64> {
        let i = self.below(off, key);
        (i < self.n && self.row(i).cmp_displaced(off, key) == Ordering::Equal).then_some(i)
    }

    /// The rows in `[lo, hi)`, as an index range.
    fn within(&self, off: i64, lo: &K, hi: &K) -> (u64, u64) {
        let a = self.below(off, lo);
        let b = self.below(off, hi).max(a);
        (a, b)
    }
}

impl<K: Displace, V> Item<K, V> {
    /// The item's least key, rows and holes alike.
    pub fn lo_key(&self) -> &K {
        match self {
            Item::One(k, _) => k,
            Item::Run(s, _) | Item::Hole(s) => &s.first,
        }
    }

    /// The item's greatest key.
    pub fn hi_key(&self) -> Cow<'_, K> {
        match self {
            Item::One(k, _) => Cow::Borrowed(k),
            Item::Run(s, _) | Item::Hole(s) => Cow::Owned(s.last()),
        }
    }

    /// The rows it holds.
    pub fn rows(&self) -> u64 {
        match self {
            Item::One(..) => 1,
            Item::Run(s, _) => s.n,
            Item::Hole(_) => 0,
        }
    }

    /// The keys it reserves without rows.
    pub fn reserved(&self) -> u64 {
        match self {
            Item::Hole(s) => s.n,
            _ => 0,
        }
    }

    /// The one row it is, if it is one.
    pub fn as_one(&self) -> Option<(&K, &V)> {
        match self {
            Item::One(k, v) => Some((k, v)),
            _ => None,
        }
    }

    /// The same item with every key moved by `by`.
    pub fn displaced(&self, by: i64) -> Item<K, V>
    where
        V: Clone,
    {
        if by == 0 {
            return self.clone();
        }
        match self {
            Item::One(k, v) => Item::One(k.displace(by), v.clone()),
            Item::Run(s, v) => Item::Run(s.displaced(by), v.clone()),
            Item::Hole(s) => Item::Hole(s.displaced(by)),
        }
    }

    /// Its measure, in its own frame.
    pub fn measure<M: Measure<K, V>>(&self) -> M {
        match self {
            Item::One(k, v) => M::entry(k, v),
            Item::Run(s, v) => M::run(&s.first, &s.stride, s.n, v),
            Item::Hole(s) => M::hole(&s.first, &s.stride, s.n),
        }
    }

    /// Every row, moved up by `off`.
    pub fn each_row(&self, off: i64) -> impl Iterator<Item = (K, &V)> + '_ {
        let (span, val, n): (Option<&Span<K>>, Option<&V>, u64) = match self {
            Item::One(_, v) => (None, Some(v), 1),
            Item::Run(s, v) => (Some(s), Some(v), s.n),
            Item::Hole(_) => (None, None, 0),
        };
        (0..n).map(move |i| {
            let k = match (self, span) {
                (Item::One(k, _), _) => k.displace(off),
                (_, Some(s)) => s.row(i).displace(off),
                _ => unreachable!(),
            };
            (k, val.expect("a row has a value"))
        })
    }
}

/// Measure, rows and reserved keys of a run of items, in their frame.
pub fn summary<K: Displace, V, M: Measure<K, V>>(items: &[Item<K, V>]) -> (M, usize, u64) {
    let mut m = M::empty();
    let (mut rows, mut reserved) = (0usize, 0u64);
    for it in items {
        match it {
            Item::One(k, v) => m.absorb_entry(k, v),
            _ => m.absorb(&it.measure::<M>()),
        }
        rows += it.rows() as usize;
        reserved += it.reserved();
    }
    (m, rows, reserved)
}

// --- reads ----------------------------------------------------------------

/// The index of the item that could hold `key` (the last whose least key,
/// moved up by `off`, is at or below it), if any.
fn holder<K: Displace, V>(items: &[Item<K, V>], off: i64, key: &K) -> Option<usize> {
    let p = items.partition_point(|it| it.lo_key().cmp_displaced(off, key) != Ordering::Greater);
    p.checked_sub(1)
}

/// The value at `key`.
pub fn get<'a, K: Displace, V>(items: &'a [Item<K, V>], off: i64, key: &K) -> Option<&'a V> {
    match &items[holder(items, off, key)?] {
        Item::One(k, v) => (k.cmp_displaced(off, key) == Ordering::Equal).then_some(v),
        Item::Run(s, v) => s.find(off, key).map(|_| v),
        Item::Hole(_) => None,
    }
}

/// Whether any key, a row or a hole's, is exactly `key`.
pub fn holds<K: Displace, V>(items: &[Item<K, V>], off: i64, key: &K) -> bool {
    let Some(i) = holder(items, off, key) else { return false };
    match &items[i] {
        Item::One(k, _) => k.cmp_displaced(off, key) == Ordering::Equal,
        Item::Run(s, _) | Item::Hole(s) => s.find(off, key).is_some(),
    }
}

/// `k` (at offset `off`) lies in `[lo, hi)`.
fn in_span<K: Displace>(k: &K, off: i64, lo: &K, hi: &K) -> bool {
    k.cmp_displaced(off, lo) != Ordering::Less && k.cmp_displaced(off, hi) == Ordering::Less
}

/// How many rows lie in `[lo, hi)`.
pub fn count_in<K: Displace, V>(items: &[Item<K, V>], off: i64, lo: &K, hi: &K) -> usize {
    items
        .iter()
        .map(|it| match it {
            Item::One(k, _) => usize::from(in_span(k, off, lo, hi)),
            Item::Run(s, _) => {
                let (a, b) = s.within(off, lo, hi);
                (b - a) as usize
            }
            Item::Hole(_) => 0,
        })
        .sum()
}

/// Fold the measure of the rows in `[lo, hi)` into `acc`, in the query frame.
pub fn fold_in<K: Displace, V, M: Measure<K, V>>(items: &[Item<K, V>], off: i64, lo: &K, hi: &K, acc: &mut M) {
    for it in items {
        match it {
            Item::One(k, v) => {
                if in_span(k, off, lo, hi) {
                    acc.absorb_entry(&k.displace(off), v);
                }
            }
            Item::Run(s, v) => {
                let (a, b) = s.within(off, lo, hi);
                if b > a {
                    acc.absorb(&M::run(&s.row(a).displace(off), &s.stride, b - a, v));
                }
            }
            Item::Hole(_) => {}
        }
    }
}

/// Whether any key, row or hole, lies in `[lo, hi)`: what makes a span
/// occupied.
pub fn any_in<K: Displace, V>(items: &[Item<K, V>], off: i64, lo: &K, hi: &K) -> bool {
    items.iter().any(|it| match it {
        Item::One(k, _) => in_span(k, off, lo, hi),
        Item::Run(s, _) | Item::Hole(s) => {
            let (a, b) = s.within(off, lo, hi);
            b > a
        }
    })
}

/// The rows in `[lo, hi)`, keys in the query frame, appended to `out`.
pub fn range_in<K: Displace, V: Clone>(items: &[Item<K, V>], off: i64, lo: &K, hi: &K, out: &mut Vec<(K, V)>) {
    for it in items {
        match it {
            Item::One(k, v) => {
                if in_span(k, off, lo, hi) {
                    out.push((k.displace(off), v.clone()));
                }
            }
            Item::Run(s, v) => {
                let (a, b) = s.within(off, lo, hi);
                out.extend((a..b).map(|i| (s.row(i).displace(off), v.clone())));
            }
            Item::Hole(_) => {}
        }
    }
}

/// The greatest row at or below `key`.
pub fn last_le<'a, K: Displace, V>(items: &'a [Item<K, V>], off: i64, key: &K) -> Option<(K, &'a V)> {
    let mut i = holder(items, off, key)?;
    loop {
        match &items[i] {
            Item::One(k, v) => return Some((k.displace(off), v)),
            Item::Run(s, v) => {
                let j = s.at_or_below(off, key);
                if j > 0 {
                    return Some((s.row(j - 1).displace(off), v));
                }
            }
            Item::Hole(_) => {}
        }
        i = i.checked_sub(1)?;
    }
}

/// The least and greatest keys, rows and holes alike, in the query frame.
pub fn extreme<K: Displace, V>(items: &[Item<K, V>], off: i64, greatest: bool) -> K {
    let it = if greatest { &items[items.len() - 1] } else { &items[0] };
    let k = if greatest { it.hi_key().into_owned() } else { it.lo_key().clone() };
    k.displace(off)
}

// --- writes (items in the caller's frame) ---------------------------------

/// A span of `n` keys as items: a run or hole if long enough, else rows (a
/// short hole is still a hole).
fn span_items<K: Displace, V: Clone>(s: Span<K>, val: Option<&V>) -> Vec<Item<K, V>> {
    match val {
        None if s.n > 0 => vec![Item::Hole(s)],
        None => Vec::new(),
        Some(v) if s.n >= MIN_RUN => vec![Item::Run(s, v.clone())],
        Some(v) => (0..s.n).map(|i| Item::One(s.row(i), v.clone())).collect(),
    }
}

/// Item `i` with the row or hole key at index `j` cut out: what stays below
/// it and above it.
fn cut_out<K: Displace, V: Clone>(item: &Item<K, V>, j: u64) -> Halves<K, V> {
    let (s, v) = match item {
        Item::Run(s, v) => (s, Some(v)),
        Item::Hole(s) => (s, None),
        Item::One(..) => return (Vec::new(), Vec::new()),
    };
    (span_items(s.slice(0, j), v), span_items(s.slice(j + 1, s.n), v))
}

/// Item `i` split at key `key` (not one of its rows): what lies below and
/// what lies above.
fn split_item<K: Displace, V: Clone>(item: &Item<K, V>, key: &K) -> Halves<K, V> {
    match item {
        Item::One(k, _) => {
            if k < key {
                (vec![item.clone()], Vec::new())
            } else {
                (Vec::new(), vec![item.clone()])
            }
        }
        Item::Run(s, v) => {
            let j = s.below(0, key);
            (span_items(s.slice(0, j), Some(v)), span_items(s.slice(j, s.n), Some(v)))
        }
        Item::Hole(s) => {
            let j = s.below(0, key);
            (span_items::<K, V>(s.slice(0, j), None), span_items::<K, V>(s.slice(j, s.n), None))
        }
    }
}

/// `key → val` written into `items`: a row replaced, a run or hole split
/// around it (writing a hole's key fills it), or a new row; then joined with
/// the runs around it.
pub fn insert<K: Displace, V: RunValue>(items: &mut Vec<Item<K, V>>, key: K, val: V, runs: bool) {
    /// What the write does to the item that could hold the key.
    enum Act {
        Front,
        Replace(usize),
        Same,
        CutOut(usize, u64),
        SplitAt(usize),
        After(usize),
    }
    let act = match holder(items, 0, &key) {
        None => Act::Front,
        Some(i) => match &items[i] {
            Item::One(k, _) if *k == key => Act::Replace(i),
            Item::Run(s, v) => match s.find(0, &key) {
                Some(_) if v.same(&val) => Act::Same,
                Some(j) => Act::CutOut(i, j),
                None if *items[i].hi_key() > key => Act::SplitAt(i),
                None => Act::After(i),
            },
            Item::Hole(s) => match s.find(0, &key) {
                Some(j) => Act::CutOut(i, j),
                None if *items[i].hi_key() > key => Act::SplitAt(i),
                None => Act::After(i),
            },
            Item::One(..) => Act::After(i),
        },
    };
    let at = match act {
        Act::Same => return,
        Act::Front => {
            items.insert(0, Item::One(key, val));
            0
        }
        Act::Replace(i) => {
            items[i] = Item::One(key, val);
            i
        }
        Act::After(i) => {
            items.insert(i + 1, Item::One(key, val));
            i + 1
        }
        Act::CutOut(i, j) => {
            let (below, above) = cut_out(&items[i], j);
            let at = i + below.len();
            items.splice(i..=i, below.into_iter().chain([Item::One(key, val)]).chain(above));
            at
        }
        Act::SplitAt(i) => {
            let (below, above) = split_item(&items[i], &key);
            let at = i + below.len();
            items.splice(i..=i, below.into_iter().chain([Item::One(key, val)]).chain(above));
            at
        }
    };
    join_around(items, at, runs);
}

/// Where the row at `key` (items moved up by `off`) is: its item, and its
/// index in that item if the item is a run.
pub fn find_row<K: Displace, V>(items: &[Item<K, V>], off: i64, key: &K) -> Option<(usize, u64)> {
    let i = holder(items, off, key)?;
    match &items[i] {
        Item::One(k, _) => (k.cmp_displaced(off, key) == Ordering::Equal).then_some((i, 0)),
        Item::Run(s, _) => s.find(off, key).map(|j| (i, j)),
        Item::Hole(_) => None,
    }
}

/// The row `find_row` located, removed.
pub fn remove_at<K: Displace, V: RunValue>(items: &mut Vec<Item<K, V>>, i: usize, j: u64) {
    match &items[i] {
        Item::One(..) => {
            items.remove(i);
        }
        _ => {
            let (below, above) = cut_out(&items[i], j);
            items.splice(i..=i, below.into_iter().chain(above));
        }
    }
}

/// `key` removed from `items`, or `None` if it holds no row there.
pub fn remove<K: Displace, V: RunValue>(items: &mut Vec<Item<K, V>>, key: &K) -> Option<()> {
    let (i, j) = find_row(items, 0, key)?;
    remove_at(items, i, j);
    Some(())
}

/// Items in key order with none inside another's range: any run another
/// item falls inside is cut there. Leaves of a k-d tree are divided by
/// column, not by key, so a run in one leaf can enclose keys of another; a
/// rebuild that brings them into one leaf separates them this way. Two runs
/// that interleave row by row come apart into rows.
pub fn disjoint<K: Displace, V: Clone>(mut items: Vec<Item<K, V>>) -> Vec<Item<K, V>> {
    items.sort_by(|a, b| a.lo_key().cmp(b.lo_key()));
    let mut i = 0;
    while i + 1 < items.len() {
        if *items[i].hi_key() < *items[i + 1].lo_key() {
            i += 1;
            continue;
        }
        let at = items[i + 1].lo_key().clone();
        let cut = items.remove(i);
        let (head, tail) = split_item(&cut, &at);
        let n = head.len();
        for (j, it) in head.into_iter().enumerate() {
            items.insert(i + j, it);
        }
        for it in tail {
            let p = items.partition_point(|x| x.lo_key() < it.lo_key());
            items.insert(p, it);
        }
        i = (i + n).saturating_sub(1);
    }
    items
}

/// `b`'s items placed among `a`'s, and the result joined. Each item of `b`
/// must lie in a key range holding none of `a`'s keys, though it may fall
/// between two rows of one of `a`'s runs, which is cut around it.
pub fn merge<K: Displace, V: RunValue>(a: Vec<Item<K, V>>, b: Vec<Item<K, V>>, runs: bool) -> Vec<Item<K, V>> {
    let mut out = a;
    for it in b {
        let (mut lo, hi) = split_at(out, it.lo_key());
        lo.push(it);
        lo.extend(hi);
        out = lo;
    }
    compress(&mut out, runs);
    out
}

/// Items `a` then `b` as one item, if they continue one another.
fn joined<K: Displace, V: RunValue>(a: &Item<K, V>, b: &Item<K, V>) -> Option<Item<K, V>> {
    match (a, b) {
        (Item::Run(s, v), Item::One(k, w)) if v.same(w) && s.row(s.n) == *k => {
            Some(Item::Run(Span { first: s.first.clone(), stride: s.stride.clone(), n: s.n + 1 }, v.clone()))
        }
        (Item::One(k, w), Item::Run(s, v)) if v.same(w) && s.first.step(&s.stride, -1) == *k => {
            Some(Item::Run(Span { first: k.clone(), stride: s.stride.clone(), n: s.n + 1 }, v.clone()))
        }
        (Item::Run(s, v), Item::Run(t, w)) if v.same(w) && s.stride == t.stride && s.row(s.n) == t.first => {
            Some(Item::Run(Span { first: s.first.clone(), stride: s.stride.clone(), n: s.n + t.n }, v.clone()))
        }
        (Item::Hole(s), Item::Hole(t)) if s.stride == t.stride && s.row(s.n) == t.first => {
            Some(Item::Hole(Span { first: s.first.clone(), stride: s.stride.clone(), n: s.n + t.n }))
        }
        _ => None,
    }
}

/// Three rows in one step with one value, as a run.
fn triple<K: Displace, V: RunValue>(a: &Item<K, V>, b: &Item<K, V>, c: &Item<K, V>) -> Option<Item<K, V>> {
    let ((k1, v1), (k2, v2), (k3, v3)) = (a.as_one()?, b.as_one()?, c.as_one()?);
    if !(v1.same(v2) && v2.same(v3)) {
        return None;
    }
    let stride = k1.stride_to(k2)?;
    (k2.step(&stride, 1) == *k3).then(|| Item::Run(Span { first: k1.clone(), stride, n: 3 }, v1.clone()))
}

/// Join the items around index `at` while any pair or triple there continues
/// one another. Without `runs`, only holes join: rows never fold.
fn join_around<K: Displace, V: RunValue>(items: &mut Vec<Item<K, V>>, at: usize, runs: bool) {
    let mut at = at.min(items.len().saturating_sub(1));
    loop {
        let lo = at.saturating_sub(2);
        let hi = (at + 2).min(items.len().saturating_sub(1));
        let mut changed = false;
        for i in lo..=hi {
            if runs && i + 2 < items.len() {
                if let Some(r) = triple(&items[i], &items[i + 1], &items[i + 2]) {
                    items.splice(i..i + 3, [r]);
                    at = i;
                    changed = true;
                    break;
                }
            }
            if i + 1 < items.len() {
                if let Some(r) = joined(&items[i], &items[i + 1]).filter(|r| runs || matches!(r, Item::Hole(_))) {
                    items.splice(i..i + 2, [r]);
                    at = i;
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            return;
        }
    }
}

/// Fold every run the items hold (with `runs`; else join only holes): what a
/// leaf does before it splits.
pub fn compress<K: Displace, V: RunValue>(items: &mut Vec<Item<K, V>>, runs: bool) {
    let mut i = 0;
    while i < items.len() {
        join_around(items, i, runs);
        i += 1;
    }
}

/// The items below `key` and those at or above it.
pub fn split_at<K: Displace, V: Clone>(items: Vec<Item<K, V>>, key: &K) -> Halves<K, V> {
    let p = items.partition_point(|it| it.lo_key() < key);
    let mut lo = items;
    let mut hi = lo.split_off(p);
    // The last item below may run past the key.
    if let Some(last) = lo.pop_if(|last| *last.hi_key() >= *key) {
        let (a, b) = split_item(&last, key);
        lo.extend(a);
        hi.splice(0..0, b);
    }
    (lo, hi)
}

/// The items whose keys lie below a k-d pivot on column `col` and those at or
/// above it. A run moves linearly in every column, so its rows below the
/// pivot are a prefix of it or a suffix.
pub fn split_on<K: Displace, V: Clone>(items: Vec<Item<K, V>>, col: usize, pivot: &K) -> Halves<K, V> {
    let below = |k: &K| k.cmp_column(col, pivot, 0) == Ordering::Less;
    let (mut lo, mut hi) = (Vec::new(), Vec::new());
    for it in items {
        let (s, v) = match &it {
            Item::One(k, _) => {
                if below(k) {
                    lo.push(it);
                } else {
                    hi.push(it);
                }
                continue;
            }
            Item::Run(s, v) => (s.clone(), Some(v.clone())),
            Item::Hole(s) => (s.clone(), None),
        };
        let (first, last) = (below(&s.first), below(&s.last()));
        if first == last {
            if first { lo.push(it) } else { hi.push(it) }
            continue;
        }
        // The boundary between the two sides, by bisection on the index.
        let (mut a, mut b) = (0u64, s.n);
        while a < b {
            let mid = a + (b - a) / 2;
            if below(&s.row(mid)) == first {
                a = mid + 1;
            } else {
                b = mid;
            }
        }
        let (head, tail) = (span_items(s.slice(0, a), v.as_ref()), span_items(s.slice(a, s.n), v.as_ref()));
        if first {
            lo.extend(head);
            hi.extend(tail);
        } else {
            hi.extend(head);
            lo.extend(tail);
        }
    }
    (lo, hi)
}

/// Assert a leaf's items are well formed: in key order without overlap, and
/// every run long enough to be one.
#[cfg(test)]
pub fn check_items<K: Displace + std::fmt::Debug, V>(items: &[Item<K, V>]) {
    for it in items {
        if let Item::Run(s, _) = it {
            assert!(s.n >= MIN_RUN, "a run of {} rows", s.n);
            assert!(s.first < s.row(1), "a run that does not ascend: {:?} by {:?}", s.first, s.stride);
        }
        if let Item::Hole(s) = it {
            assert!(s.n > 0, "an empty hole");
        }
    }
    for w in items.windows(2) {
        assert!(*w[0].hi_key() < *w[1].lo_key(), "items overlap: {:?} then {:?}", w[0].hi_key(), w[1].lo_key());
    }
}
