//! The enfilade primitive: a **persistent, measured, weight-balanced ordered
//! tree**.
//!
//! * **Persistent / structurally shared.** Every node is immutable and `Arc`-held;
//!   an update path-copies only the root→leaf spine and shares every untouched
//!   subtree with the prior version (Okasaki). So an edition is a root and a new
//!   edition costs `O(log n)` new nodes — the substrate of the Ent's cheap
//!   versioning.
//! * **Measured (WID).** Each node caches the [`Measure`] of its subtree, so a
//!   range measure ("how many / what, under this key span") is answered in
//!   `O(log n)` by summing whole in-range subtrees and recursing only at the two
//!   boundaries — Xanadu's *wid* pruning.
//! * **Weight-balanced, deterministically.** Balance is decided by subtree
//!   *sizes* (Adams' bounded-balance tree, δ=3, γ=2) — **no randomness**, so the
//!   tree shape is a pure function of the operation sequence and replay is
//!   deterministic (the Replay law). Two histories may reach the same *logical*
//!   map by different shapes; identity is defined at the entry level, never the
//!   node level (`iter` is canonical).

use std::sync::Arc;

use crate::measure::Measure;

/// Adams' bounded-balance parameters. A subtree may be at most `DELTA`× the
/// weight of its sibling; `GAMMA` chooses single vs double rotation.
const DELTA: usize = 3;
const GAMMA: usize = 2;

struct Node<K, V, M> {
    key: K,
    val: V,
    left: Tree<K, V, M>,
    right: Tree<K, V, M>,
    size: usize,
    measure: M,
}

/// A persistent measured ordered map from `K` to `V` with subtree measure `M`.
pub struct Tree<K, V, M> {
    root: Option<Arc<Node<K, V, M>>>,
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
        self.root.is_none()
    }

    /// The measure of the whole tree — `O(1)`.
    pub fn measure(&self) -> M {
        match &self.root {
            Some(n) => n.measure.clone(),
            None => M::empty(),
        }
    }

    /// The value bound to `key`, or `None`. `O(log n)`.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut cur = &self.root;
        while let Some(n) = cur {
            match key.cmp(&n.key) {
                std::cmp::Ordering::Less => cur = &n.left.root,
                std::cmp::Ordering::Greater => cur = &n.right.root,
                std::cmp::Ordering::Equal => return Some(&n.val),
            }
        }
        None
    }

    /// A new tree with `key → val` inserted or replaced. Persistent: the prior
    /// tree is unchanged and shares all untouched subtrees. `O(log n)`.
    pub fn insert(&self, key: K, val: V) -> Self {
        match &self.root {
            None => Self::singleton(key, val),
            Some(n) => match key.cmp(&n.key) {
                std::cmp::Ordering::Less => {
                    Self::balance(n.key.clone(), n.val.clone(), n.left.insert(key, val), n.right.clone())
                }
                std::cmp::Ordering::Greater => {
                    Self::balance(n.key.clone(), n.val.clone(), n.left.clone(), n.right.insert(key, val))
                }
                std::cmp::Ordering::Equal => Self::mk(key, val, n.left.clone(), n.right.clone()),
            },
        }
    }

    /// A new tree with `key` removed (a no-op if absent). Persistent. `O(log n)`.
    pub fn remove(&self, key: &K) -> Self {
        match &self.root {
            None => self.clone(),
            Some(n) => match key.cmp(&n.key) {
                std::cmp::Ordering::Less => {
                    Self::balance(n.key.clone(), n.val.clone(), n.left.remove(key), n.right.clone())
                }
                std::cmp::Ordering::Greater => {
                    Self::balance(n.key.clone(), n.val.clone(), n.left.clone(), n.right.remove(key))
                }
                std::cmp::Ordering::Equal => Self::glue(&n.left, &n.right),
            },
        }
    }

    /// The measure of entries whose key lies in `[lo, hi)`. `O(log n)` — whole
    /// in-range subtrees contribute their cached measure without descent; only
    /// the two boundary spines are walked. This is the WID-pruned range query.
    pub fn measure_range(&self, lo: &K, hi: &K) -> M {
        match &self.root {
            None => M::empty(),
            Some(n) => {
                if &n.key < lo {
                    n.right.measure_range(lo, hi)
                } else if &n.key >= hi {
                    n.left.measure_range(lo, hi)
                } else {
                    // lo <= key < hi: this node is in range; the left subtree
                    // contributes its `>= lo` part, the right its `< hi` part.
                    let l = n.left.measure_ge(lo);
                    let r = n.right.measure_lt(hi);
                    l.combine(&M::entry(&n.key, &n.val)).combine(&r)
                }
            }
        }
    }

    /// The entries with key in `[lo, hi)`, cloned, in order. `O(result + depth)`
    /// — subtrees wholly outside the span are pruned. Cheap when values are
    /// `Arc`-backed (a clone is a refcount bump).
    pub fn range_collect(&self, lo: &K, hi: &K) -> Vec<(K, V)> {
        let mut out = Vec::new();
        self.range_into(lo, hi, &mut out);
        out
    }

    fn range_into(&self, lo: &K, hi: &K, out: &mut Vec<(K, V)>) {
        if let Some(n) = &self.root {
            if &n.key >= lo {
                n.left.range_into(lo, hi, out);
            }
            if lo <= &n.key && &n.key < hi {
                out.push((n.key.clone(), n.val.clone()));
            }
            if &n.key < hi {
                n.right.range_into(lo, hi, out);
            }
        }
    }

    /// In-order `(key, value)` iterator — the **canonical** ordering of the map,
    /// independent of tree shape. This is the identity view.
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        let mut stack = Vec::new();
        let mut cur = &self.root;
        while let Some(n) = cur {
            stack.push(n.as_ref());
            cur = &n.left.root;
        }
        Iter { stack }
    }

    // --- internals --------------------------------------------------------

    fn singleton(key: K, val: V) -> Self {
        let measure = M::entry(&key, &val);
        Tree { root: Some(Arc::new(Node { key, val, left: Tree::new(), right: Tree::new(), size: 1, measure })) }
    }

    /// Smart constructor: build a node computing its size and measure from the
    /// children. Does **not** rebalance (callers that may unbalance use `balance`).
    fn mk(key: K, val: V, left: Self, right: Self) -> Self {
        let size = 1 + left.len() + right.len();
        let measure = left.measure().combine(&M::entry(&key, &val)).combine(&right.measure());
        Tree { root: Some(Arc::new(Node { key, val, left, right, size, measure })) }
    }

    /// Rebuild a node, rotating if the weight-balance invariant is violated.
    fn balance(key: K, val: V, left: Self, right: Self) -> Self {
        let ln = left.len();
        let rn = right.len();
        if ln + rn <= 1 {
            Self::mk(key, val, left, right)
        } else if rn > DELTA * ln {
            let rnode = right.root.as_ref().unwrap();
            if rnode.left.len() < GAMMA * rnode.right.len() {
                Self::rotate_single_l(key, val, left, &right)
            } else {
                Self::rotate_double_l(key, val, left, &right)
            }
        } else if ln > DELTA * rn {
            let lnode = left.root.as_ref().unwrap();
            if lnode.right.len() < GAMMA * lnode.left.len() {
                Self::rotate_single_r(key, val, &left, right)
            } else {
                Self::rotate_double_r(key, val, &left, right)
            }
        } else {
            Self::mk(key, val, left, right)
        }
    }

    fn rotate_single_l(key: K, val: V, left: Self, right: &Self) -> Self {
        let r = right.root.as_ref().unwrap();
        Self::mk(
            r.key.clone(),
            r.val.clone(),
            Self::mk(key, val, left, r.left.clone()),
            r.right.clone(),
        )
    }

    fn rotate_double_l(key: K, val: V, left: Self, right: &Self) -> Self {
        let r = right.root.as_ref().unwrap();
        let rl = r.left.root.as_ref().unwrap();
        Self::mk(
            rl.key.clone(),
            rl.val.clone(),
            Self::mk(key, val, left, rl.left.clone()),
            Self::mk(r.key.clone(), r.val.clone(), rl.right.clone(), r.right.clone()),
        )
    }

    fn rotate_single_r(key: K, val: V, left: &Self, right: Self) -> Self {
        let l = left.root.as_ref().unwrap();
        Self::mk(
            l.key.clone(),
            l.val.clone(),
            l.left.clone(),
            Self::mk(key, val, l.right.clone(), right),
        )
    }

    fn rotate_double_r(key: K, val: V, left: &Self, right: Self) -> Self {
        let l = left.root.as_ref().unwrap();
        let lr = l.right.root.as_ref().unwrap();
        Self::mk(
            lr.key.clone(),
            lr.val.clone(),
            Self::mk(l.key.clone(), l.val.clone(), l.left.clone(), lr.left.clone()),
            Self::mk(key, val, lr.right.clone(), right),
        )
    }

    /// Join two subtrees with all keys in `left` < all keys in `right` (used by
    /// `remove` to fill the hole): promote an extreme of the heavier side.
    fn glue(left: &Self, right: &Self) -> Self {
        if left.is_empty() {
            return right.clone();
        }
        if right.is_empty() {
            return left.clone();
        }
        if left.len() > right.len() {
            let (k, v, rest) = left.delete_max();
            Self::balance(k, v, rest, right.clone())
        } else {
            let (k, v, rest) = right.delete_min();
            Self::balance(k, v, left.clone(), rest)
        }
    }

    /// Remove and return the least entry (tree is non-empty).
    fn delete_min(&self) -> (K, V, Self) {
        let n = self.root.as_ref().unwrap();
        if n.left.is_empty() {
            (n.key.clone(), n.val.clone(), n.right.clone())
        } else {
            let (k, v, rest) = n.left.delete_min();
            (k, v, Self::balance(n.key.clone(), n.val.clone(), rest, n.right.clone()))
        }
    }

    /// Remove and return the greatest entry (tree is non-empty).
    fn delete_max(&self) -> (K, V, Self) {
        let n = self.root.as_ref().unwrap();
        if n.right.is_empty() {
            (n.key.clone(), n.val.clone(), n.left.clone())
        } else {
            let (k, v, rest) = n.right.delete_max();
            (k, v, Self::balance(n.key.clone(), n.val.clone(), n.left.clone(), rest))
        }
    }

    /// Measure of entries with key `>= lo`. `O(log n)`.
    fn measure_ge(&self, lo: &K) -> M {
        match &self.root {
            None => M::empty(),
            Some(n) => {
                if &n.key < lo {
                    n.right.measure_ge(lo)
                } else {
                    // key >= lo: node + whole right subtree included; left's >= lo part
                    n.left.measure_ge(lo).combine(&M::entry(&n.key, &n.val)).combine(&n.right.measure())
                }
            }
        }
    }

    /// Measure of entries with key `< hi`. `O(log n)`.
    fn measure_lt(&self, hi: &K) -> M {
        match &self.root {
            None => M::empty(),
            Some(n) => {
                if &n.key >= hi {
                    n.left.measure_lt(hi)
                } else {
                    // key < hi: whole left subtree + node + right's < hi part
                    n.left.measure().combine(&M::entry(&n.key, &n.val)).combine(&n.right.measure_lt(hi))
                }
            }
        }
    }
}

/// In-order iterator over `(key, value)` references.
pub struct Iter<'a, K, V, M> {
    stack: Vec<&'a Node<K, V, M>>,
}

impl<'a, K, V, M> Iterator for Iter<'a, K, V, M> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.stack.pop()?;
        // After yielding `n`, descend the left spine of its right child.
        let mut cur = &n.right.root;
        while let Some(m) = cur {
            self.stack.push(m.as_ref());
            cur = &m.left.root;
        }
        Some((&n.key, &n.val))
    }
}
