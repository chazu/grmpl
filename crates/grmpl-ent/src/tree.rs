//! The enfilade primitive: a **persistent, measured B+ tree**.
//!
//! * **Persistent / structurally shared.** Every node is immutable and `Arc`-held;
//!   an update path-copies only the root→leaf spine and shares every untouched
//!   subtree with the prior version (Okasaki). So an edition is a root and a new
//!   edition costs `O(log n)` new nodes — the substrate of the Ent's cheap
//!   versioning.
//! * **Measured (WID).** Each node caches the [`Measure`] of its subtree, so a
//!   range measure ("how many / what, under this key span") is answered by
//!   summing whole in-range subtrees and descending only at the two boundaries —
//!   Xanadu's *wid* pruning.
//! * **Wide nodes (G-1b).** A node holds up to [`B`] entries (leaf) or [`B`]
//!   children (internal), not one. This is what keeps the *granfilade* honest: a
//!   node is one content-addressed record, so a wide node means one record per
//!   **run** of tuples rather than one record per tuple. Before this the store
//!   paid a full KV record — 16-byte key, two child pointers, an LSM index entry
//!   — for every single fact, and a load was one KV `get` per fact. Arity is the
//!   constant factor that decides whether the Ent can replace the LSM at all.
//! * **Deterministic.** Shape is a pure function of the operation sequence — no
//!   randomness, no clock, no pointer input — so replay reproduces it exactly
//!   (the Replay law).
//!
//! **Identity is logical, not structural** (plan v5 §G-2b, settled). Two
//! histories may reach the same logical map by different shapes, and a content
//! key therefore identifies a *shape*: sharing is **within a version lineage**
//! (path copy), which is what cheap history, forks, and GC actually rest on.
//! Equality the language relies on is at the entry level — [`iter`](Tree::iter)
//! is canonical — exactly as `DESIGN.md` and the store contract define it.

use std::sync::Arc;

use crate::measure::Measure;

/// One entry difference between two tree versions: `(key, left_value,
/// right_value)`, where a `None` side means the key is absent there.
pub type EntryDiff<K, V> = (K, Option<V>, Option<V>);

/// Node arity: the maximum entries in a leaf and the maximum children of an
/// internal node. A node is one granfilade record, so this is the tuples-per-
/// record factor; 64 keeps a leaf in the low kilobytes for typical tuples.
pub const B: usize = 64;

/// Minimum occupancy for a non-root node. Without a floor, repeated removes
/// would thin nodes back towards one entry apiece and undo the whole point of
/// arity.
const MIN: usize = B / 2;

/// A node is either a run of entries or a run of children with separators.
///
/// Internal invariant: `keys.len() + 1 == children.len()`, and `keys[i]` is the
/// least key in `children[i + 1]` — so `children[i]` covers the half-open span
/// `[keys[i - 1], keys[i])`, unbounded at the ends.
enum Kind<K, V, M> {
    Leaf(Vec<(K, V)>),
    Internal { keys: Vec<K>, children: Vec<Tree<K, V, M>> },
}

struct Node<K, V, M> {
    kind: Kind<K, V, M>,
    /// Entries in the whole subtree.
    size: usize,
    /// The subtree's cached [`Measure`] — the upward WID summary.
    measure: M,
}

/// A persistent measured ordered map from `K` to `V` with subtree measure `M`.
pub struct Tree<K, V, M> {
    root: Option<Arc<Node<K, V, M>>>,
}

/// A borrowed view of one node, for persistence and inspection — it lets the
/// granfilade walk the exact tree shape without a rebalancing reconstruction.
pub enum NodeRef<'a, K, V, M> {
    /// A run of `(key, value)` entries, ascending.
    Leaf(&'a [(K, V)]),
    /// Separator keys and the children they divide (`keys.len() + 1` children).
    Internal(&'a [K], &'a [Tree<K, V, M>]),
}

// Manual `Clone`: cloning a tree is a single `Arc` refcount bump (a version
// handle), never a deep copy — regardless of the `K`/`V`/`M` bounds.
impl<K, V, M> Clone for Tree<K, V, M> {
    fn clone(&self) -> Self {
        Tree { root: self.root.clone() }
    }
}

impl<K, V, M> Default for Tree<K, V, M> {
    fn default() -> Self {
        Tree { root: None }
    }
}

/// The result of inserting into a subtree: either a replacement node, or a node
/// that overflowed and split into two with a separator promoted to the parent.
enum Ins<K, V, M> {
    Done(Tree<K, V, M>),
    Split(Tree<K, V, M>, K, Tree<K, V, M>),
}

/// A sibling pair's contents, owned so `remove`'s rebalance can shuffle units
/// between them. Siblings are always the same kind.
enum Pair<K, V, M> {
    Leaves(Vec<(K, V)>, Vec<(K, V)>),
    Inners(Vec<K>, Vec<Tree<K, V, M>>, Vec<K>, Vec<Tree<K, V, M>>),
}

impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Clone,
    V: Clone,
    M: Measure<K, V>,
{
    /// The empty tree.
    pub fn new() -> Self {
        Tree { root: None }
    }

    /// Entry count — `O(1)` from the cached size.
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |n| n.size)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The measure of the whole tree — `O(1)`.
    pub fn measure(&self) -> M {
        match &self.root {
            Some(n) => n.measure.clone(),
            None => M::empty(),
        }
    }

    /// The value bound to `key`, or `None`. `O(log n)` descents, each a binary
    /// search within one wide node.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut cur = self.root.as_deref();
        while let Some(n) = cur {
            match &n.kind {
                Kind::Leaf(entries) => {
                    return entries.binary_search_by(|(k, _)| k.cmp(key)).ok().map(|i| &entries[i].1)
                }
                Kind::Internal { keys, children } => {
                    cur = children[child_index(keys, key)].root.as_deref();
                }
            }
        }
        None
    }

    /// A new tree with `key → val` inserted or replaced. Persistent: the prior
    /// tree is unchanged and shares every untouched subtree.
    pub fn insert(&self, key: K, val: V) -> Self {
        match &self.root {
            None => Self::leaf(vec![(key, val)]),
            Some(n) => match Self::ins(n, key, val) {
                Ins::Done(t) => t,
                Ins::Split(l, sep, r) => Self::internal(vec![sep], vec![l, r]),
            },
        }
    }

    /// A new tree with `key` removed. Absent keys are a true no-op — the same
    /// shared version is returned, not a rebuilt copy.
    pub fn remove(&self, key: &K) -> Self {
        let n = match &self.root {
            None => return self.clone(),
            Some(n) => n,
        };
        let t = match Self::rem(n, key) {
            None => return self.clone(),
            Some(t) => t,
        };
        // The root is exempt from the occupancy floor, but it may need to shrink:
        // an internal root down to one child becomes that child, and an emptied
        // leaf root becomes the empty tree.
        match t.root.as_deref() {
            Some(Node { kind: Kind::Internal { children, .. }, .. }) if children.len() == 1 => {
                children[0].clone()
            }
            Some(Node { kind: Kind::Leaf(e), .. }) if e.is_empty() => Tree::new(),
            _ => t,
        }
    }

    /// The measure of entries whose key lies in `[lo, hi)`. Whole in-range
    /// subtrees contribute their cached measure without descent; only the two
    /// boundary spines are walked. This is the WID-pruned range query.
    pub fn measure_range(&self, lo: &K, hi: &K) -> M {
        if lo >= hi {
            return M::empty();
        }
        Self::fold_range(self, lo, hi, None, None, &mut |m, sub| m.combine(sub), M::empty())
    }

    /// The entries with key in `[lo, hi)`, cloned, in order. `O(result + depth)`
    /// — subtrees wholly outside the span are pruned. Cheap when values are
    /// `Arc`-backed (a clone is a refcount bump).
    pub fn range_collect(&self, lo: &K, hi: &K) -> Vec<(K, V)> {
        let mut out = Vec::new();
        if lo < hi {
            Self::range_into(self, lo, hi, None, None, &mut out);
        }
        out
    }

    /// Whether two trees are the **same shared version** — an `O(1)` pointer
    /// check (both empty, or the same root `Arc`). A `true` result means no entry
    /// differs; the backfollow / version-compare fast path (an unchanged relation
    /// between two editions shares its root).
    pub fn same_version(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// The entries that differ between `self` and `other`, as
    /// `(key, self_value, other_value)` (a `None` side means absent). Short-
    /// circuits to empty when the two are the same shared version.
    pub fn diff(&self, other: &Self) -> Vec<EntryDiff<K, V>>
    where
        V: PartialEq,
    {
        if self.same_version(other) {
            return Vec::new();
        }
        let a: Vec<(&K, &V)> = self.iter().collect();
        let b: Vec<(&K, &V)> = other.iter().collect();
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            match (a.get(i), b.get(j)) {
                (Some((ak, av)), Some((bk, bv))) => match ak.cmp(bk) {
                    std::cmp::Ordering::Less => {
                        out.push(((*ak).clone(), Some((*av).clone()), None));
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        out.push(((*bk).clone(), None, Some((*bv).clone())));
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if av != bv {
                            out.push(((*ak).clone(), Some((*av).clone()), Some((*bv).clone())));
                        }
                        i += 1;
                        j += 1;
                    }
                },
                (Some((ak, av)), None) => {
                    out.push(((*ak).clone(), Some((*av).clone()), None));
                    i += 1;
                }
                (None, Some((bk, bv))) => {
                    out.push(((*bk).clone(), None, Some((*bv).clone())));
                    j += 1;
                }
                (None, None) => break,
            }
        }
        out
    }

    /// In-order `(key, value)` iterator — the **canonical** ordering of the map,
    /// independent of tree shape. This is the identity view.
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        let mut it = Iter { stack: Vec::new(), leaf: None };
        it.descend(self);
        it
    }

    // --- persistence surface ------------------------------------------------

    /// A borrowed view of this tree's root node, or `None` if empty. For the
    /// granfilade: it walks the exact persisted shape without reconstructing it.
    pub fn node(&self) -> Option<NodeRef<'_, K, V, M>> {
        match &self.root.as_deref()?.kind {
            Kind::Leaf(entries) => Some(NodeRef::Leaf(entries)),
            Kind::Internal { keys, children } => Some(NodeRef::Internal(keys, children)),
        }
    }

    /// Rebuild a leaf from its exact persisted entries — no rebalancing, so a
    /// load round-trips the stored shape and content keys stay stable.
    pub fn leaf_of(entries: Vec<(K, V)>) -> Self {
        Self::leaf(entries)
    }

    /// Rebuild an internal node from its exact persisted separators and children.
    /// `keys.len() + 1 == children.len()` must hold, as it does for anything this
    /// module produced.
    pub fn internal_of(keys: Vec<K>, children: Vec<Self>) -> Self {
        Self::internal(keys, children)
    }

    // --- construction -------------------------------------------------------

    fn leaf(entries: Vec<(K, V)>) -> Self {
        let mut measure = M::empty();
        for (k, v) in &entries {
            measure = measure.combine(&M::entry(k, v));
        }
        Tree { root: Some(Arc::new(Node { size: entries.len(), measure, kind: Kind::Leaf(entries) })) }
    }

    fn internal(keys: Vec<K>, children: Vec<Self>) -> Self {
        let mut measure = M::empty();
        let mut size = 0;
        for c in &children {
            measure = measure.combine(&c.measure());
            size += c.len();
        }
        Tree { root: Some(Arc::new(Node { size, measure, kind: Kind::Internal { keys, children } })) }
    }

    // --- insert -------------------------------------------------------------

    fn ins(n: &Node<K, V, M>, key: K, val: V) -> Ins<K, V, M> {
        match &n.kind {
            Kind::Leaf(entries) => {
                let mut e = entries.clone();
                match e.binary_search_by(|(k, _)| k.cmp(&key)) {
                    Ok(i) => {
                        e[i] = (key, val);
                        Ins::Done(Self::leaf(e))
                    }
                    Err(i) => {
                        e.insert(i, (key, val));
                        if e.len() <= B {
                            Ins::Done(Self::leaf(e))
                        } else {
                            let right = e.split_off(e.len() / 2);
                            let sep = right[0].0.clone();
                            Ins::Split(Self::leaf(e), sep, Self::leaf(right))
                        }
                    }
                }
            }
            Kind::Internal { keys, children } => {
                let i = child_index(keys, &key);
                let child = children[i].root.as_deref().expect("internal child is never empty");
                match Self::ins(child, key, val) {
                    Ins::Done(c) => {
                        let mut ch = children.clone();
                        ch[i] = c;
                        Ins::Done(Self::internal(keys.clone(), ch))
                    }
                    Ins::Split(l, sep, r) => {
                        let mut ch = children.clone();
                        ch[i] = l;
                        ch.insert(i + 1, r);
                        let mut ks = keys.clone();
                        ks.insert(i, sep);
                        if ch.len() <= B {
                            Ins::Done(Self::internal(ks, ch))
                        } else {
                            // Split the child run in half and promote the
                            // separator that divided the two halves.
                            let mid = ch.len() / 2;
                            let rch = ch.split_off(mid);
                            let rks = ks.split_off(mid);
                            let promoted = ks.pop().expect("a split internal node has separators");
                            Ins::Split(Self::internal(ks, ch), promoted, Self::internal(rks, rch))
                        }
                    }
                }
            }
        }
    }

    // --- remove -------------------------------------------------------------

    /// Remove `key` from the subtree at `n`, or `None` if it was absent (so the
    /// caller keeps sharing the existing version). The returned node may be below
    /// the occupancy floor; the *parent* repairs it.
    fn rem(n: &Node<K, V, M>, key: &K) -> Option<Self> {
        match &n.kind {
            Kind::Leaf(entries) => {
                let i = entries.binary_search_by(|(k, _)| k.cmp(key)).ok()?;
                let mut e = entries.clone();
                e.remove(i);
                Some(Self::leaf(e))
            }
            Kind::Internal { keys, children } => {
                let i = child_index(keys, key);
                let child = children[i].root.as_deref().expect("internal child is never empty");
                let newc = Self::rem(child, key)?;
                let mut ks = keys.clone();
                let mut ch = children.clone();
                ch[i] = newc;
                Self::repair(&mut ks, &mut ch, i);
                Some(Self::internal(ks, ch))
            }
        }
    }

    /// Restore the occupancy floor for `ch[i]`: borrow one entry/child from a
    /// sibling that can spare it, else merge with a sibling.
    fn repair(ks: &mut Vec<K>, ch: &mut Vec<Self>, i: usize) {
        if !ch[i].underflows() {
            return;
        }
        if i > 0 && ch[i - 1].can_lend() {
            Self::borrow_from_left(ks, ch, i);
        } else if i + 1 < ch.len() && ch[i + 1].can_lend() {
            Self::borrow_from_right(ks, ch, i);
        } else if i > 0 {
            Self::merge(ks, ch, i - 1);
        } else if ch.len() > 1 {
            Self::merge(ks, ch, i);
        }
        // A lone child that cannot borrow or merge stays underfull; the root
        // collapse in `remove` (or this node's own parent) resolves it.
    }

    /// Occupancy below the floor — entries for a leaf, children for an internal.
    fn underflows(&self) -> bool {
        match self.root.as_deref().map(|n| &n.kind) {
            Some(Kind::Leaf(e)) => e.len() < MIN,
            Some(Kind::Internal { children, .. }) => children.len() < MIN,
            None => true,
        }
    }

    /// Occupancy strictly above the floor, so one unit can be lent away.
    fn can_lend(&self) -> bool {
        match self.root.as_deref().map(|n| &n.kind) {
            Some(Kind::Leaf(e)) => e.len() > MIN,
            Some(Kind::Internal { children, .. }) => children.len() > MIN,
            None => false,
        }
    }

    /// Owned copies of a sibling pair's contents, ready to be rebalanced. The
    /// two are always the same kind — siblings sit at the same depth.
    fn pair(l: &Self, r: &Self) -> Pair<K, V, M> {
        match (l.node(), r.node()) {
            (Some(NodeRef::Leaf(le)), Some(NodeRef::Leaf(re))) => {
                Pair::Leaves(le.to_vec(), re.to_vec())
            }
            (Some(NodeRef::Internal(lk, lc)), Some(NodeRef::Internal(rk, rc))) => {
                Pair::Inners(lk.to_vec(), lc.to_vec(), rk.to_vec(), rc.to_vec())
            }
            _ => unreachable!("siblings are the same kind and never empty"),
        }
    }

    /// Move the last unit of `ch[i-1]` onto the front of `ch[i]`, and reseat the
    /// separator `ks[i-1]` that divides them.
    fn borrow_from_left(ks: &mut [K], ch: &mut [Self], i: usize) {
        match Self::pair(&ch[i - 1], &ch[i]) {
            Pair::Leaves(mut left, mut right) => {
                let moved = left.pop().expect("a lending leaf is non-empty");
                ks[i - 1] = moved.0.clone();
                right.insert(0, moved);
                ch[i - 1] = Self::leaf(left);
                ch[i] = Self::leaf(right);
            }
            Pair::Inners(mut lkeys, mut lchildren, mut rkeys, mut rchildren) => {
                let moved = lchildren.pop().expect("a lending node has children");
                let up = lkeys.pop().expect("a lending internal node has separators");
                // The parent separator descends to head `ch[i]`'s keys; the
                // sibling's last separator rises to replace it.
                rkeys.insert(0, std::mem::replace(&mut ks[i - 1], up));
                rchildren.insert(0, moved);
                ch[i - 1] = Self::internal(lkeys, lchildren);
                ch[i] = Self::internal(rkeys, rchildren);
            }
        }
    }

    /// Move the first unit of `ch[i+1]` onto the end of `ch[i]`, and reseat the
    /// separator `ks[i]` that divides them.
    fn borrow_from_right(ks: &mut [K], ch: &mut [Self], i: usize) {
        match Self::pair(&ch[i], &ch[i + 1]) {
            Pair::Leaves(mut left, mut right) => {
                left.push(right.remove(0));
                ks[i] = right[0].0.clone();
                ch[i] = Self::leaf(left);
                ch[i + 1] = Self::leaf(right);
            }
            Pair::Inners(mut lkeys, mut lchildren, mut rkeys, mut rchildren) => {
                let moved = rchildren.remove(0);
                let up = rkeys.remove(0);
                lkeys.push(std::mem::replace(&mut ks[i], up));
                lchildren.push(moved);
                ch[i] = Self::internal(lkeys, lchildren);
                ch[i + 1] = Self::internal(rkeys, rchildren);
            }
        }
    }

    /// Fuse `ch[j]` and `ch[j+1]` into one node, consuming the separator `ks[j]`.
    /// Both are at or below the floor, so the result cannot exceed [`B`].
    fn merge(ks: &mut Vec<K>, ch: &mut Vec<Self>, j: usize) {
        let sep = ks.remove(j);
        let right = ch.remove(j + 1);
        match Self::pair(&ch[j], &right) {
            Pair::Leaves(mut entries, rest) => {
                entries.extend(rest);
                ch[j] = Self::leaf(entries);
            }
            Pair::Inners(mut keys, mut children, rkeys, rchildren) => {
                // The separator that divided them becomes an ordinary separator
                // between the two child runs.
                keys.push(sep);
                keys.extend(rkeys);
                children.extend(rchildren);
                ch[j] = Self::internal(keys, children);
            }
        }
    }

    // --- range walks --------------------------------------------------------

    /// Fold the measures of everything in `[lo, hi)`. `nlo`/`nhi` are the key
    /// bounds this subtree is known to sit inside, threaded down from the
    /// separators of its ancestors — that is what lets a wholly-contained
    /// subtree answer from its cached measure without being entered.
    fn fold_range(
        t: &Self,
        lo: &K,
        hi: &K,
        nlo: Option<&K>,
        nhi: Option<&K>,
        combine: &mut impl FnMut(M, &M) -> M,
        acc: M,
    ) -> M {
        let n = match t.root.as_deref() {
            None => return acc,
            Some(n) => n,
        };
        if nlo.is_some_and(|l| l >= hi) || nhi.is_some_and(|h| h <= lo) {
            return acc; // disjoint
        }
        if nlo.is_some_and(|l| l >= lo) && nhi.is_some_and(|h| h <= hi) {
            return combine(acc, &n.measure); // wholly contained — no descent
        }
        match &n.kind {
            Kind::Leaf(entries) => {
                let mut acc = acc;
                for (k, v) in entries {
                    if k >= lo && k < hi {
                        acc = combine(acc, &M::entry(k, v));
                    }
                }
                acc
            }
            Kind::Internal { keys, children } => {
                let mut acc = acc;
                for (idx, c) in span(keys, children, lo, hi) {
                    let clo = if idx == 0 { nlo } else { Some(&keys[idx - 1]) };
                    let chi = if idx == children.len() - 1 { nhi } else { Some(&keys[idx]) };
                    acc = Self::fold_range(c, lo, hi, clo, chi, combine, acc);
                }
                acc
            }
        }
    }

    fn range_into(t: &Self, lo: &K, hi: &K, nlo: Option<&K>, nhi: Option<&K>, out: &mut Vec<(K, V)>) {
        let n = match t.root.as_deref() {
            None => return,
            Some(n) => n,
        };
        if nlo.is_some_and(|l| l >= hi) || nhi.is_some_and(|h| h <= lo) {
            return;
        }
        match &n.kind {
            Kind::Leaf(entries) => {
                for (k, v) in entries {
                    if k >= lo && k < hi {
                        out.push((k.clone(), v.clone()));
                    }
                }
            }
            Kind::Internal { keys, children } => {
                for (idx, c) in span(keys, children, lo, hi) {
                    let clo = if idx == 0 { nlo } else { Some(&keys[idx - 1]) };
                    let chi = if idx == children.len() - 1 { nhi } else { Some(&keys[idx]) };
                    Self::range_into(c, lo, hi, clo, chi, out);
                }
            }
        }
    }
}

/// The child index whose span contains `key`: the first `i` with `key < keys[i]`.
fn child_index<K: Ord>(keys: &[K], key: &K) -> usize {
    keys.partition_point(|k| k <= key)
}

/// The children whose spans can overlap `[lo, hi)`, with their indices — the
/// pruning step: everything outside this window is skipped without a descent.
fn span<'a, K: Ord, T>(
    keys: &[K],
    children: &'a [T],
    lo: &K,
    hi: &K,
) -> impl Iterator<Item = (usize, &'a T)> {
    let first = keys.partition_point(|k| k <= lo);
    let last = keys.partition_point(|k| k < hi).min(children.len() - 1);
    (first..=last.max(first)).zip(children[first..=last.max(first)].iter())
}

/// One frame of the iterator's descent: an internal node's children and the
/// index of the next one to visit.
type Frame<'a, K, V, M> = (&'a [Tree<K, V, M>], usize);

/// In-order iterator over `(key, value)` references.
pub struct Iter<'a, K, V, M> {
    /// Internal nodes on the descent, each with the next child to visit.
    stack: Vec<Frame<'a, K, V, M>>,
    /// The leaf being drained, and how far into it we are.
    leaf: Option<(&'a [(K, V)], usize)>,
}

impl<'a, K, V, M> Iter<'a, K, V, M> {
    /// Walk to the leftmost leaf of `t`, recording the internal nodes passed.
    fn descend(&mut self, t: &'a Tree<K, V, M>) {
        let mut cur = t.root.as_deref();
        while let Some(n) = cur {
            match &n.kind {
                Kind::Leaf(entries) => {
                    self.leaf = Some((entries, 0));
                    return;
                }
                Kind::Internal { children, .. } => {
                    self.stack.push((children, 1));
                    cur = children[0].root.as_deref();
                }
            }
        }
    }
}

impl<'a, K, V, M> Iterator for Iter<'a, K, V, M> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((entries, i)) = &mut self.leaf {
                if *i < entries.len() {
                    let (k, v) = &entries[*i];
                    *i += 1;
                    return Some((k, v));
                }
                self.leaf = None;
            }
            // The leaf is drained: take the next unvisited child of the nearest
            // ancestor that still has one, and descend its left spine.
            let next = self.stack.last_mut().and_then(|(children, idx)| {
                let children: &'a [Tree<K, V, M>] = children;
                let picked = children.get(*idx);
                *idx += 1;
                picked
            });
            match next {
                Some(t) => self.descend(t),
                None => {
                    self.stack.pop()?;
                }
            }
        }
    }
}
