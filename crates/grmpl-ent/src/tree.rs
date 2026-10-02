//! The enfilade primitive: a **persistent, measured, displaced B+ tree**.
//!
//! * **Persistent / structurally shared.** Every node is immutable and `Arc`-held;
//!   an update path-copies only the root→leaf spine and shares every untouched
//!   subtree with the prior version (Okasaki). So an edition is a root and a new
//!   edition costs `O(log n)` new nodes — the substrate of the Ent's cheap
//!   versioning.
//! * **Displaced (DSP).** A [`Tree`] is a *handle*: a shared node plus a
//!   displacement — the node's position relative to whatever points at it, as on
//!   Gold's `DspLoaf`. A node stores its keys and separators in its own **local
//!   frame**, and a descent accumulates the dsps to recover absolute keys. So
//!   [`relocate`](Tree::relocate) is `O(1)` and shares the whole subtree, and
//!   [`graft`](Tree::graft) virtually copies a key span into a new position in
//!   `O(log n)` new nodes, sharing every interior node with the original. What a
//!   displacement does to a key is [`Displace`].
//! * **Measured (WID).** Each node caches the [`Measure`] of its subtree, so a
//!   range measure ("how many / what, under this key span") is answered by
//!   summing whole in-range subtrees and descending only at the two boundaries —
//!   Xanadu's *wid* pruning.
//! * **Wide nodes (G-1b).** A node holds up to [`B`] entries (leaf) or [`B`]
//!   children (internal), not one. A node is one content-addressed granfilade
//!   record, so a wide node means one record per **run** of tuples rather than
//!   one record per tuple.
//! * **Deterministic.** Shape is a pure function of the operation sequence — no
//!   randomness, no clock, no pointer input — so replay reproduces it exactly
//!   (the Replay law).
//!
//! **Path copies push dsps down.** An operation that rewrites a node first
//! *opens* it into its parent's frame — keys displaced, each child's dsp
//! composed with the node's. That is lazy propagation: the copied spine ends up
//! with zero dsps, while every subtree it did not touch keeps its own and stays
//! shared. Read paths never open anything; they carry the accumulated offset.
//!
//! **Identity is logical, not structural** (plan v5 §G-2b, settled). Two
//! histories may reach the same logical map by different shapes, and a content
//! key therefore identifies a *shape*: sharing is **within a version lineage**
//! (path copy and graft), which is what cheap history, forks, and GC rest on.
//! Equality the language relies on is at the entry level — [`iter`](Tree::iter)
//! is canonical — exactly as `DESIGN.md` and the store contract define it.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::{Arc, OnceLock};

use grmpl_core::hash::Sha256Digest as ContentKey;

use crate::dsp::Displace;
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

/// A node is either a run of entries or a run of children with separators, all
/// in the node's **local frame**.
///
/// Internal invariant: `keys.len() + 1 == children.len()`, and `keys[i]` is the
/// least key in `children[i + 1]` (in this node's frame, i.e. after that child's
/// dsp) — so `children[i]` covers the half-open span `[keys[i - 1], keys[i])`,
/// unbounded at the ends.
enum Kind<K, V, M> {
    Leaf(Vec<(K, V)>),
    Internal { keys: Vec<K>, children: Vec<Tree<K, V, M>> },
}

struct Node<K, V, M> {
    kind: Kind<K, V, M>,
    /// Entries in the whole subtree.
    size: usize,
    /// The subtree's cached [`Measure`], in the node's local frame — the upward
    /// WID summary.
    measure: M,
    /// **Memoized content key (G-1).** A node is immutable and `Arc`-held, so
    /// its content key is a pure function of it and caching it here is just
    /// memoizing that function. Filling it is the granfilade's job (this module
    /// names no hash of a *frame*, only the cell), and it is what lets a commit
    /// skip subtrees it has already persisted instead of re-serializing them.
    ck: OnceLock<ContentKey>,
}

/// A persistent measured ordered map from `K` to `V` with subtree measure `M`:
/// a shared root node and the displacement it sits at.
pub struct Tree<K, V, M> {
    root: Option<Arc<Node<K, V, M>>>,
    /// The root node's position relative to the frame this handle lives in.
    /// Always `0` for the empty tree.
    dsp: i64,
}

/// A borrowed view of one node, for persistence and inspection — it lets the
/// granfilade walk the exact tree shape without a rebalancing reconstruction.
/// Keys are in the node's local frame; each child carries its own
/// [`dsp`](Tree::dsp) relative to it.
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
        Tree { root: self.root.clone(), dsp: self.dsp }
    }
}

impl<K, V, M> Default for Tree<K, V, M> {
    fn default() -> Self {
        Tree { root: None, dsp: 0 }
    }
}

/// The result of rewriting a subtree: either a replacement, or a run that
/// overflowed and split into two with a separator promoted to the parent.
enum Ins<K, V, M> {
    Done(Tree<K, V, M>),
    Split(Tree<K, V, M>, K, Tree<K, V, M>),
}

/// A node's contents, owned and expressed in its **parent's** frame — what an
/// operation that rewrites the node works on.
enum Open<K, V, M> {
    Leaf(Vec<(K, V)>),
    Internal(Vec<K>, Vec<Tree<K, V, M>>),
}

/// `k` moved by `by`, borrowing when there is nothing to move.
fn moved<K: Displace>(k: &K, by: i64) -> Cow<'_, K> {
    if by == 0 {
        Cow::Borrowed(k)
    } else {
        Cow::Owned(k.displace(by))
    }
}

impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Displace,
    V: Clone,
    M: Measure<K, V>,
{
    /// The empty tree.
    pub fn new() -> Self {
        Tree::default()
    }

    /// Entry count — `O(1)` from the cached size.
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |n| n.size)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// This handle's displacement relative to the frame it lives in.
    pub fn dsp(&self) -> i64 {
        self.dsp
    }

    /// The measure of the whole tree — `O(1)`.
    pub fn measure(&self) -> M {
        match &self.root {
            Some(n) => n.measure.displace(self.dsp),
            None => M::empty(),
        }
    }

    /// **Relocation, `O(1)`.** The same tree with every key displaced by `by`:
    /// the same shared root under a new displacement. Nothing is copied.
    pub fn relocate(&self, by: i64) -> Self {
        match &self.root {
            None => Tree::new(),
            Some(_) => Tree { root: self.root.clone(), dsp: self.dsp.wrapping_add(by) },
        }
    }

    /// The same tree with its root at displacement `0`: the root node is opened
    /// into this frame (`O(B)`) and everything beneath stays shared. The
    /// granfilade persists roots in this form, so a root pointer is just a
    /// content key.
    pub fn normalized(&self) -> Self {
        if self.dsp == 0 || self.root.is_none() {
            return self.clone();
        }
        match Self::open(self) {
            Open::Leaf(entries) => Self::leaf(entries),
            Open::Internal(keys, children) => Self::internal(keys, children),
        }
    }

    /// The value bound to `key`, or `None`. `O(log n)` descents, each a binary
    /// search within one wide node.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut cur = self;
        let mut off = 0i64;
        loop {
            let n = cur.root.as_deref()?;
            off = off.wrapping_add(cur.dsp);
            match &n.kind {
                Kind::Leaf(entries) => {
                    return entries
                        .binary_search_by(|(x, _)| x.cmp_displaced(off, key))
                        .ok()
                        .map(|i| &entries[i].1)
                }
                Kind::Internal { keys, children } => cur = &children[child_index(keys, off, key)],
            }
        }
    }

    /// A new tree with `key → val` inserted or replaced. Persistent: the prior
    /// tree is unchanged and shares every untouched subtree.
    pub fn insert(&self, key: K, val: V) -> Self {
        if self.root.is_none() {
            return Self::leaf(vec![(key, val)]);
        }
        match Self::ins(self, key, val) {
            Ins::Done(t) => t,
            Ins::Split(l, sep, r) => Self::internal(vec![sep], vec![l, r]),
        }
    }

    /// A new tree with `key` removed. Absent keys are a true no-op — the same
    /// shared version is returned, not a rebuilt copy.
    pub fn remove(&self, key: &K) -> Self {
        match Self::rem(self, key, 0) {
            None => self.clone(),
            Some(t) => Self::shrink_root(t),
        }
    }

    /// The measure of entries whose key lies in `[lo, hi)`. Whole in-range
    /// subtrees contribute their cached measure without descent; only the two
    /// boundary spines are walked. This is the WID-pruned range query.
    pub fn measure_range(&self, lo: &K, hi: &K) -> M {
        if lo >= hi {
            return M::empty();
        }
        Self::fold_range(self, lo, hi, None, None, 0, M::empty())
    }

    /// The entries with key in `[lo, hi)`, cloned, in order. `O(result + depth)`
    /// — subtrees wholly outside the span are pruned. Cheap when values are
    /// `Arc`-backed (a clone is a refcount bump).
    pub fn range_collect(&self, lo: &K, hi: &K) -> Vec<(K, V)> {
        let mut out = Vec::new();
        if lo < hi {
            Self::range_into(self, lo, hi, 0, &mut out);
        }
        out
    }

    /// Whether any entry has a key in `[lo, hi)` — `O(log n)`, stopping at the
    /// first subtree that must hold one.
    pub fn any_in(&self, lo: &K, hi: &K) -> bool {
        lo < hi && Self::any_into(self, lo, hi, None, None, 0)
    }

    /// The greatest entry whose key is `<= key`, or `None`. `O(log n)`.
    ///
    /// This is the as-of lookup: a Version enfilade keyed by edition answers
    /// "the state in force at `at`" with it, so an as-of read is a descent
    /// rather than a scan of the versions below `at`.
    pub fn last_le(&self, key: &K) -> Option<(K, &V)> {
        Self::last_le_in(self, key, 0)
    }

    /// Whether two trees are the **same shared version** — an `O(1)` check (both
    /// empty, or the same root `Arc` at the same displacement). A `true` result
    /// means no entry differs; the version-compare fast path (an unchanged
    /// relation between two editions shares its root).
    pub fn same_version(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) && self.dsp == other.dsp,
            _ => false,
        }
    }

    /// The entries that differ between `self` and `other`, as
    /// `(key, self_value, other_value)` (a `None` side means absent).
    ///
    /// **Subtree-pruned (plan v5 §G-0d).** Two versions from one lineage share
    /// every subtree the edit did not touch, so the walk dismisses them without
    /// descending: by pointer when the two are the same in-memory node, and by
    /// **memoized content key** when they are the same node reached through
    /// different handles (a reloaded version, a fork) — in both cases only at
    /// the same displacement. Pruning happens at *every* level, not just the
    /// root, which is what makes version-compare cost the edit rather than the
    /// relation.
    ///
    /// The descent pairs children only when the two nodes sit at the same
    /// displacement and carry the **same separators** — then each pair covers
    /// exactly the same key span and can be compared independently. That is the
    /// ordinary case for a path copy. Otherwise *that subtree pair* falls back to
    /// an in-order merge, which is always correct.
    pub fn diff(&self, other: &Self) -> Vec<EntryDiff<K, V>>
    where
        V: PartialEq,
    {
        let mut out = Vec::new();
        Self::diff_into(self, other, 0, &mut out);
        out
    }

    /// In-order `(key, value)` iterator — the **canonical** ordering of the map,
    /// independent of tree shape. This is the identity view. Keys come out in
    /// the absolute frame, owned (a clone, or a displaced copy under a dsp).
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        let mut it = Iter { stack: Vec::new(), leaf: None };
        it.descend(self, 0);
        it
    }

    // --- split, join, graft -------------------------------------------------

    /// Split into the entries below `key` and those at or above it. Persistent:
    /// `O(log n)` new nodes along the cut, everything else shared.
    pub fn split(&self, key: &K) -> (Self, Self) {
        Self::split_at(self, key)
    }

    /// Concatenate two trees whose key spans do not interleave: every key of
    /// `left` must be below every key of `right`. Persistent: `O(log n)` new
    /// nodes along the seam, everything else shared.
    pub fn join(left: &Self, right: &Self) -> Self {
        if left.is_empty() {
            return right.clone();
        }
        if right.is_empty() {
            return left.clone();
        }
        let (hl, hr) = (left.height(), right.height());
        let res = if hl == hr {
            Self::join_same(left, right)
        } else if hl > hr {
            Self::join_right(left, right, hl, hr)
        } else {
            Self::join_left(left, right, hl, hr)
        };
        match res {
            Ins::Done(t) => t,
            Ins::Split(l, sep, r) => Self::internal(vec![sep], vec![l, r]),
        }
    }

    /// **Virtual copy (Gold's DSP graft).** Copy the entries in `[lo, hi)` to
    /// the same keys displaced by `by`, leaving the originals in place.
    ///
    /// The span is split out, relocated in `O(1)`, and joined back in at its new
    /// position, so the copy shares every interior node with the original and
    /// costs `O(log n)` new nodes however large the span is. Later edits to
    /// either copy path-copy away from the shared nodes as usual.
    ///
    /// Returns `None`, changing nothing, if the target span is already occupied
    /// or if the displacement does not carry the span cleanly into it (an
    /// identity coordinate, or entity ids that would wrap). An empty source span
    /// returns the tree unchanged.
    pub fn graft(&self, lo: &K, hi: &K, by: i64) -> Option<Self> {
        if lo >= hi {
            return Some(self.clone());
        }
        let (_, rest) = self.split(lo);
        let (span, _) = rest.split(hi);
        if span.is_empty() {
            return Some(self.clone());
        }
        let (tlo, thi) = (lo.displace(by), hi.displace(by));
        if tlo >= thi || self.any_in(&tlo, &thi) {
            return None;
        }
        let copy = span.relocate(by);
        let first = copy.min_key();
        let last = copy.max_key();
        if first < tlo || last >= thi {
            return None;
        }
        let (below, above) = self.split(&tlo);
        Some(Self::join(&Self::join(&below, &copy), &above))
    }

    // --- persistence surface ------------------------------------------------

    /// This node's memoized content-key cell, or `None` if the tree is empty.
    /// The granfilade reads it to skip an already-persisted subtree, and fills
    /// it after hashing.
    pub fn ck_cell(&self) -> Option<&OnceLock<ContentKey>> {
        Some(&self.root.as_deref()?.ck)
    }

    /// A borrowed view of this tree's root node, or `None` if empty. For the
    /// granfilade: it walks the exact persisted shape without reconstructing it.
    /// The view is in the node's local frame — this handle's [`dsp`](Self::dsp)
    /// is not applied.
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

    /// Rebuild an internal node from its exact persisted separators and children
    /// (each already at its persisted dsp). `keys.len() + 1 == children.len()`
    /// must hold, as it does for anything this module produced.
    pub fn internal_of(keys: Vec<K>, children: Vec<Self>) -> Self {
        Self::internal(keys, children)
    }

    // --- construction -------------------------------------------------------

    fn leaf(entries: Vec<(K, V)>) -> Self {
        let mut measure = M::empty();
        for (k, v) in &entries {
            measure = measure.combine(&M::entry(k, v));
        }
        Tree {
            root: Some(Arc::new(Node {
                size: entries.len(),
                measure,
                kind: Kind::Leaf(entries),
                ck: OnceLock::new(),
            })),
            dsp: 0,
        }
    }

    fn internal(keys: Vec<K>, children: Vec<Self>) -> Self {
        let mut measure = M::empty();
        let mut size = 0;
        for c in &children {
            measure = measure.combine(&c.measure());
            size += c.len();
        }
        Tree {
            root: Some(Arc::new(Node {
                size,
                measure,
                kind: Kind::Internal { keys, children },
                ck: OnceLock::new(),
            })),
            dsp: 0,
        }
    }

    /// A tree from a run of children and the separators between them: empty,
    /// the lone child itself, or a (possibly underfull) root over them.
    fn from_parts(keys: Vec<K>, mut children: Vec<Self>) -> Self {
        match children.len() {
            0 => Tree::new(),
            1 => children.pop().unwrap(),
            _ => Self::internal(keys, children),
        }
    }

    /// A rewritten run of children, split in two if it overflowed.
    fn finish(mut keys: Vec<K>, mut children: Vec<Self>) -> Ins<K, V, M> {
        if children.len() <= B {
            return Ins::Done(Self::internal(keys, children));
        }
        // Split the child run in half and promote the separator that divided
        // the two halves.
        let mid = children.len() / 2;
        let rch = children.split_off(mid);
        let rks = keys.split_off(mid);
        let promoted = keys.pop().expect("a split internal node has separators");
        Ins::Split(Self::internal(keys, children), promoted, Self::internal(rks, rch))
    }

    /// The node under `t`, opened into `t`'s parent frame: keys displaced by
    /// `t`'s dsp and each child's dsp composed with it. This pushes the dsp down
    /// one level; the children themselves stay shared.
    fn open(t: &Self) -> Open<K, V, M> {
        let n = t.root.as_deref().expect("open on a non-empty tree");
        let d = t.dsp;
        match &n.kind {
            Kind::Leaf(entries) => Open::Leaf(if d == 0 {
                entries.clone()
            } else {
                entries.iter().map(|(k, v)| (k.displace(d), v.clone())).collect()
            }),
            Kind::Internal { keys, children } => Open::Internal(
                if d == 0 { keys.clone() } else { keys.iter().map(|k| k.displace(d)).collect() },
                children.iter().map(|c| c.relocate(d)).collect(),
            ),
        }
    }

    /// The number of levels: `0` empty, `1` a leaf. Every leaf sits at the same
    /// depth, so the leftmost spine says it.
    fn height(&self) -> usize {
        let mut h = 0;
        let mut cur = self;
        while let Some(n) = cur.root.as_deref() {
            h += 1;
            match &n.kind {
                Kind::Leaf(_) => break,
                Kind::Internal { children, .. } => cur = &children[0],
            }
        }
        h
    }

    /// The least key, in this handle's frame. The tree must be non-empty.
    fn min_key(&self) -> K {
        let mut cur = self;
        let mut off = 0i64;
        loop {
            off = off.wrapping_add(cur.dsp);
            match &cur.root.as_deref().expect("min_key of an empty tree").kind {
                Kind::Leaf(entries) => return entries[0].0.displace(off),
                Kind::Internal { children, .. } => cur = &children[0],
            }
        }
    }

    /// The greatest key, in this handle's frame. The tree must be non-empty.
    fn max_key(&self) -> K {
        let mut cur = self;
        let mut off = 0i64;
        loop {
            off = off.wrapping_add(cur.dsp);
            match &cur.root.as_deref().expect("max_key of an empty tree").kind {
                Kind::Leaf(entries) => return entries[entries.len() - 1].0.displace(off),
                Kind::Internal { children, .. } => cur = &children[children.len() - 1],
            }
        }
    }

    // --- insert -------------------------------------------------------------

    /// Insert into the subtree under `t`; `key` and the result are in `t`'s
    /// parent frame.
    fn ins(t: &Self, key: K, val: V) -> Ins<K, V, M> {
        match Self::open(t) {
            Open::Leaf(mut e) => match e.binary_search_by(|(k, _)| k.cmp(&key)) {
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
            },
            Open::Internal(mut ks, mut ch) => {
                let i = child_index(&ks, 0, &key);
                match Self::ins(&ch[i], key, val) {
                    Ins::Done(c) => {
                        ch[i] = c;
                        Ins::Done(Self::internal(ks, ch))
                    }
                    Ins::Split(l, sep, r) => {
                        ch[i] = l;
                        ch.insert(i + 1, r);
                        ks.insert(i, sep);
                        Self::finish(ks, ch)
                    }
                }
            }
        }
    }

    // --- remove -------------------------------------------------------------

    /// Remove `key` from the subtree under `t`, or `None` if it was absent (so the
    /// caller keeps sharing the existing version). `off` carries `t`'s parent
    /// frame to `key`'s; the result is in `t`'s parent frame. It may be below the
    /// occupancy floor; the *parent* repairs it.
    fn rem(t: &Self, key: &K, off: i64) -> Option<Self> {
        let n = t.root.as_deref()?;
        let off = off.wrapping_add(t.dsp);
        match &n.kind {
            Kind::Leaf(entries) => {
                let i = entries.binary_search_by(|(k, _)| k.cmp_displaced(off, key)).ok()?;
                let Open::Leaf(mut e) = Self::open(t) else { unreachable!() };
                e.remove(i);
                Some(Self::leaf(e))
            }
            Kind::Internal { keys, children } => {
                let i = child_index(keys, off, key);
                let newc = Self::rem(&children[i], key, off)?;
                let Open::Internal(mut ks, mut ch) = Self::open(t) else { unreachable!() };
                ch[i] = newc.relocate(t.dsp);
                Self::fix(&mut ks, &mut ch, i);
                Some(Self::internal(ks, ch))
            }
        }
    }

    /// The root is exempt from the occupancy floor, but it may need to shrink:
    /// an internal root down to one child becomes that child, and an emptied
    /// leaf root becomes the empty tree.
    fn shrink_root(t: Self) -> Self {
        match t.root.as_deref().map(|n| &n.kind) {
            Some(Kind::Internal { children, .. }) if children.len() == 1 => {
                children[0].relocate(t.dsp)
            }
            Some(Kind::Leaf(e)) if e.is_empty() => Tree::new(),
            _ => t,
        }
    }

    /// Restore the occupancy floor for `ch[i]` by fusing it with a sibling.
    fn fix(ks: &mut Vec<K>, ch: &mut Vec<Self>, i: usize) {
        if !ch[i].underflows() {
            return;
        }
        if i > 0 {
            Self::fuse(ks, ch, i - 1);
        } else if ch.len() > 1 {
            Self::fuse(ks, ch, i);
        }
        // A lone child that cannot fuse stays underfull; the root collapse in
        // `remove` (or this node's own parent) resolves it.
    }

    /// Fuse siblings `ch[j]` and `ch[j+1]` (consuming the separator `ks[j]`), and
    /// if the result overflows, split it evenly back in two. One of the pair may
    /// be arbitrarily small — a removed-from child, or a whole tree being joined
    /// in — and the other at least [`MIN`], so either outcome meets the floor.
    fn fuse(ks: &mut Vec<K>, ch: &mut Vec<Self>, j: usize) {
        let sep = ks.remove(j);
        let right = ch.remove(j + 1);
        match (Self::open(&ch[j]), Self::open(&right)) {
            (Open::Leaf(mut entries), Open::Leaf(rest)) => {
                entries.extend(rest);
                if entries.len() <= B {
                    ch[j] = Self::leaf(entries);
                } else {
                    let r = entries.split_off(entries.len() / 2);
                    ks.insert(j, r[0].0.clone());
                    ch[j] = Self::leaf(entries);
                    ch.insert(j + 1, Self::leaf(r));
                }
            }
            (Open::Internal(mut keys, mut children), Open::Internal(rkeys, rchildren)) => {
                // The separator that divided them becomes an ordinary separator
                // between the two child runs.
                keys.push(sep);
                keys.extend(rkeys);
                children.extend(rchildren);
                match Self::finish(keys, children) {
                    Ins::Done(t) => ch[j] = t,
                    Ins::Split(l, s, r) => {
                        ch[j] = l;
                        ch.insert(j + 1, r);
                        ks.insert(j, s);
                    }
                }
            }
            _ => unreachable!("siblings are the same kind and never empty"),
        }
    }

    /// Occupancy below the floor — entries for a leaf, children for an internal.
    fn underflows(&self) -> bool {
        match self.root.as_deref().map(|n| &n.kind) {
            Some(Kind::Leaf(e)) => e.len() < MIN,
            Some(Kind::Internal { children, .. }) => children.len() < MIN,
            None => true,
        }
    }

    // --- split / join internals -----------------------------------------------

    /// Split the subtree under `t` at `key`; `key` and both halves are in `t`'s
    /// parent frame.
    fn split_at(t: &Self, key: &K) -> (Self, Self) {
        if t.is_empty() {
            return (Tree::new(), Tree::new());
        }
        match Self::open(t) {
            Open::Leaf(mut e) => {
                let i = e.partition_point(|(k, _)| k < key);
                let r = e.split_off(i);
                let side = |v: Vec<(K, V)>| if v.is_empty() { Tree::new() } else { Self::leaf(v) };
                (side(e), side(r))
            }
            Open::Internal(mut ks, mut ch) => {
                // Children before `i` lie wholly below `key`, children after it
                // wholly above; only `ch[i]` straddles the cut.
                let i = child_index(&ks, 0, key);
                let right_ch = ch.split_off(i + 1);
                let mid = ch.pop().expect("child_index is in range");
                // `ks[i - 1]` and `ks[i]` bounded the straddling child; the runs
                // either side keep the separators between their own children.
                let right_ks = ks.split_off((i + 1).min(ks.len()));
                ks.truncate(i.saturating_sub(1));
                let (ml, mr) = Self::split_at(&mid, key);
                let left = Self::from_parts(ks, ch);
                let right = Self::from_parts(right_ks, right_ch);
                (Self::join(&left, &ml), Self::join(&mr, &right))
            }
        }
    }

    /// Join two trees of equal height: fuse their roots as if they were siblings.
    fn join_same(l: &Self, r: &Self) -> Ins<K, V, M> {
        let mut ks = vec![r.min_key()];
        let mut ch = vec![l.clone(), r.clone()];
        Self::fuse(&mut ks, &mut ch, 0);
        let right = ch.pop().unwrap();
        match ch.pop() {
            None => Ins::Done(right),
            Some(left) => Ins::Split(left, ks.pop().unwrap(), right),
        }
    }

    /// Hang `r` (height `hr`) off the right spine of `t` (height `ht > hr`).
    fn join_right(t: &Self, r: &Self, ht: usize, hr: usize) -> Ins<K, V, M> {
        let Open::Internal(mut ks, mut ch) = Self::open(t) else {
            unreachable!("a taller tree is internal")
        };
        let last = ch.len() - 1;
        if ht - 1 == hr {
            ks.push(r.min_key());
            ch.push(r.clone());
            Self::fix(&mut ks, &mut ch, last + 1);
        } else {
            match Self::join_right(&ch[last], r, ht - 1, hr) {
                Ins::Done(c) => ch[last] = c,
                Ins::Split(a, s, b) => {
                    ch[last] = a;
                    ch.push(b);
                    ks.push(s);
                }
            }
        }
        Self::finish(ks, ch)
    }

    /// Hang `l` (height `hl`) off the left spine of `t` (height `ht > hl`).
    fn join_left(l: &Self, t: &Self, hl: usize, ht: usize) -> Ins<K, V, M> {
        let Open::Internal(mut ks, mut ch) = Self::open(t) else {
            unreachable!("a taller tree is internal")
        };
        if ht - 1 == hl {
            ks.insert(0, ch[0].min_key());
            ch.insert(0, l.clone());
            Self::fix(&mut ks, &mut ch, 0);
        } else {
            match Self::join_left(l, &ch[0], hl, ht - 1) {
                Ins::Done(c) => ch[0] = c,
                Ins::Split(a, s, b) => {
                    ch[0] = a;
                    ch.insert(1, b);
                    ks.insert(0, s);
                }
            }
        }
        Self::finish(ks, ch)
    }

    // --- read walks -----------------------------------------------------------
    //
    // A walk's query (`lo`, `hi`, `key`) stays in the frame it was asked in, and
    // `off` carries the current node's local frame to it: stored keys are moved
    // *up* to the query, never the query down (see [`Displace`]). A bound
    // threaded down from an ancestor's separators travels with its own offset.

    /// Fold the measures of everything in `[lo, hi)`. `nlo`/`nhi` bound the
    /// subtree, threaded down from its ancestors' separators — that is what lets
    /// a wholly-contained subtree answer from its cached measure without being
    /// entered.
    fn fold_range(
        t: &Self,
        lo: &K,
        hi: &K,
        nlo: Bound<'_, K>,
        nhi: Bound<'_, K>,
        off: i64,
        acc: M,
    ) -> M {
        let n = match t.root.as_deref() {
            None => return acc,
            Some(n) => n,
        };
        if disjoint(nlo, nhi, lo, hi) {
            return acc;
        }
        let off = off.wrapping_add(t.dsp);
        if contained(nlo, nhi, lo, hi) {
            return acc.combine(&n.measure.displace(off));
        }
        match &n.kind {
            Kind::Leaf(entries) => {
                let mut acc = acc;
                for (k, v) in entries {
                    if in_span(k, off, lo, hi) {
                        acc = acc.combine(&M::entry(&moved(k, off), v));
                    }
                }
                acc
            }
            Kind::Internal { keys, children } => {
                let mut acc = acc;
                for (idx, c) in span(keys, children, off, lo, hi) {
                    let clo = if idx == 0 { nlo } else { Some((&keys[idx - 1], off)) };
                    let chi = if idx == children.len() - 1 { nhi } else { Some((&keys[idx], off)) };
                    acc = Self::fold_range(c, lo, hi, clo, chi, off, acc);
                }
                acc
            }
        }
    }

    fn range_into(t: &Self, lo: &K, hi: &K, off: i64, out: &mut Vec<(K, V)>) {
        let n = match t.root.as_deref() {
            None => return,
            Some(n) => n,
        };
        let off = off.wrapping_add(t.dsp);
        match &n.kind {
            Kind::Leaf(entries) => {
                for (k, v) in entries {
                    if in_span(k, off, lo, hi) {
                        out.push((moved(k, off).into_owned(), v.clone()));
                    }
                }
            }
            Kind::Internal { keys, children } => {
                for (_, c) in span(keys, children, off, lo, hi) {
                    Self::range_into(c, lo, hi, off, out);
                }
            }
        }
    }

    fn any_into(t: &Self, lo: &K, hi: &K, nlo: Bound<'_, K>, nhi: Bound<'_, K>, off: i64) -> bool {
        let n = match t.root.as_deref() {
            None => return false,
            Some(n) => n,
        };
        if disjoint(nlo, nhi, lo, hi) {
            return false;
        }
        if contained(nlo, nhi, lo, hi) {
            return n.size > 0;
        }
        let off = off.wrapping_add(t.dsp);
        match &n.kind {
            Kind::Leaf(entries) => entries.iter().any(|(k, _)| in_span(k, off, lo, hi)),
            Kind::Internal { keys, children } => span(keys, children, off, lo, hi).any(|(idx, c)| {
                let clo = if idx == 0 { nlo } else { Some((&keys[idx - 1], off)) };
                let chi = if idx == children.len() - 1 { nhi } else { Some((&keys[idx], off)) };
                Self::any_into(c, lo, hi, clo, chi, off)
            }),
        }
    }

    fn last_le_in<'a>(t: &'a Self, key: &K, off: i64) -> Option<(K, &'a V)> {
        let n = t.root.as_deref()?;
        let off = off.wrapping_add(t.dsp);
        match &n.kind {
            Kind::Leaf(entries) => {
                let i = entries.partition_point(|(k, _)| k.cmp_displaced(off, key) != Ordering::Greater);
                (i > 0).then(|| {
                    let (k, v) = &entries[i - 1];
                    (moved(k, off).into_owned(), v)
                })
            }
            Kind::Internal { keys, children } => {
                // Descend the child whose span holds `key`; if that subtree has
                // nothing at or below it, the answer is the greatest entry of a
                // preceding sibling.
                let i = child_index(keys, off, key);
                (0..=i).rev().find_map(|j| Self::last_le_in(&children[j], key, off))
            }
        }
    }

    fn diff_into(a: &Self, b: &Self, off: i64, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        // The same in-memory node at the same position: nothing beneath differs.
        if a.same_version(b) {
            return;
        }
        // The same *content* at the same position, reached by different handles
        // — the cross-version form of the same fact, available because nodes
        // memoize their key.
        if a.dsp == b.dsp {
            if let (Some(x), Some(y)) =
                (a.ck_cell().and_then(|c| c.get()), b.ck_cell().and_then(|c| c.get()))
            {
                if x == y {
                    return;
                }
            }
        }
        match (a.root.as_deref().map(|n| &n.kind), b.root.as_deref().map(|n| &n.kind)) {
            (
                Some(Kind::Internal { keys: ak, children: ac }),
                Some(Kind::Internal { keys: bk, children: bc }),
            ) if a.dsp == b.dsp && ak == bk && ac.len() == bc.len() => {
                // Identical frames and separators ⇒ child `i` of each covers the
                // same span.
                let local_off = off.wrapping_add(a.dsp);
                for (x, y) in ac.iter().zip(bc.iter()) {
                    Self::diff_into(x, y, local_off, out);
                }
            }
            _ => Self::merge_diff(a, b, off, out),
        }
    }

    /// The in-order merge of two subtrees' entries — the always-correct base the
    /// pruned descent falls back to.
    fn merge_diff(a: &Self, b: &Self, off: i64, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        fn walk<K: Displace, V, M>(t: &Tree<K, V, M>, off: i64) -> Vec<(K, &V)> {
            let mut it = Iter { stack: Vec::new(), leaf: None };
            it.descend(t, off);
            it.collect()
        }
        let (av, bv) = (walk(a, off), walk(b, off));
        let (mut i, mut j) = (0, 0);
        while i < av.len() || j < bv.len() {
            match (av.get(i), bv.get(j)) {
                (Some((ak, aval)), Some((bk, bval))) => match ak.cmp(bk) {
                    std::cmp::Ordering::Less => {
                        out.push((ak.clone(), Some((*aval).clone()), None));
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        out.push((bk.clone(), None, Some((*bval).clone())));
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if aval != bval {
                            out.push((ak.clone(), Some((*aval).clone()), Some((*bval).clone())));
                        }
                        i += 1;
                        j += 1;
                    }
                },
                (Some((ak, aval)), None) => {
                    out.push((ak.clone(), Some((*aval).clone()), None));
                    i += 1;
                }
                (None, Some((bk, bval))) => {
                    out.push((bk.clone(), None, Some((*bval).clone())));
                    j += 1;
                }
                (None, None) => break,
            }
        }
    }
}

#[cfg(test)]
impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Displace + std::fmt::Debug,
    V: Clone,
    M: Measure<K, V>,
{
    /// Assert every structural invariant, through the displaced frames: keys in
    /// order and inside their separators' spans, uniform leaf depth, occupancy
    /// within `[MIN, B]` below the root, and cached sizes that match.
    pub(crate) fn check(&self) {
        if self.root.is_some() {
            Self::check_node(self, None, None, 0, true);
        }
        let keys: Vec<K> = self.iter().map(|(k, _)| k).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys out of order: {keys:?}");
        assert_eq!(keys.len(), self.len(), "cached size disagrees with the entries");
    }

    /// Returns the subtree's height. `lo`/`hi` bound its keys, each with the
    /// offset carrying its frame to the absolute one; `off` is `t`'s parent's.
    fn check_node(t: &Self, lo: Bound<'_, K>, hi: Bound<'_, K>, off: i64, is_root: bool) -> usize {
        let n = t.root.as_deref().expect("no empty subtree below the root");
        let off = off.wrapping_add(t.dsp);
        let abs = |k: &K| k.displace(off);
        let within = |k: &K| {
            let k = abs(k);
            lo.is_none_or(|(l, o)| l.cmp_displaced(o, &k) != Ordering::Greater)
                && hi.is_none_or(|(h, o)| h.cmp_displaced(o, &k) == Ordering::Greater)
        };
        match &n.kind {
            Kind::Leaf(entries) => {
                assert!(!entries.is_empty(), "empty leaf");
                assert!(entries.len() <= B, "leaf over arity");
                assert!(is_root || entries.len() >= MIN, "leaf under the floor: {}", entries.len());
                for (k, _) in entries {
                    assert!(within(k), "leaf key {:?} outside its span", abs(k));
                }
                assert_eq!(n.size, entries.len());
                1
            }
            Kind::Internal { keys, children } => {
                assert_eq!(keys.len() + 1, children.len(), "separator count");
                assert!(children.len() <= B, "internal over arity");
                assert!(children.len() >= if is_root { 2 } else { MIN }, "internal under the floor");
                assert!(keys.windows(2).all(|w| w[0] < w[1]), "separators out of order");
                for k in keys {
                    assert!(within(k), "separator {:?} outside its span", abs(k));
                }
                let mut depth = None;
                let mut size = 0;
                for (i, c) in children.iter().enumerate() {
                    let clo = if i == 0 { lo } else { Some((&keys[i - 1], off)) };
                    let chi = if i == children.len() - 1 { hi } else { Some((&keys[i], off)) };
                    let d = Self::check_node(c, clo, chi, off, false);
                    assert!(depth.is_none_or(|x| x == d), "leaves at different depths");
                    depth = Some(d);
                    size += c.len();
                }
                assert_eq!(n.size, size, "cached size disagrees with the children");
                depth.unwrap() + 1
            }
        }
    }
}

/// A known bound on a subtree's keys: a separator and the offset that carries
/// its frame to the query's.
type Bound<'a, K> = Option<(&'a K, i64)>;

/// `k` (at offset `off`) lies in `[lo, hi)`.
fn in_span<K: Displace>(k: &K, off: i64, lo: &K, hi: &K) -> bool {
    k.cmp_displaced(off, lo) != Ordering::Less && k.cmp_displaced(off, hi) == Ordering::Less
}

/// A subtree bounded by `[nlo, nhi)` cannot meet `[lo, hi)`.
fn disjoint<K: Displace>(nlo: Bound<'_, K>, nhi: Bound<'_, K>, lo: &K, hi: &K) -> bool {
    nlo.is_some_and(|(k, o)| k.cmp_displaced(o, hi) != Ordering::Less)
        || nhi.is_some_and(|(k, o)| k.cmp_displaced(o, lo) != Ordering::Greater)
}

/// A subtree bounded by `[nlo, nhi)` lies wholly inside `[lo, hi)`.
fn contained<K: Displace>(nlo: Bound<'_, K>, nhi: Bound<'_, K>, lo: &K, hi: &K) -> bool {
    nlo.is_some_and(|(k, o)| k.cmp_displaced(o, lo) != Ordering::Less)
        && nhi.is_some_and(|(k, o)| k.cmp_displaced(o, hi) != Ordering::Greater)
}

/// The child index whose span contains `key`: the first `i` with
/// `key < keys[i]`, the separators taken at offset `off`.
fn child_index<K: Displace>(keys: &[K], off: i64, key: &K) -> usize {
    keys.partition_point(|k| k.cmp_displaced(off, key) != Ordering::Greater)
}

/// The children whose spans can overlap `[lo, hi)`, with their indices — the
/// pruning step: everything outside this window is skipped without a descent.
fn span<'a, K: Displace, T>(
    keys: &[K],
    children: &'a [T],
    off: i64,
    lo: &K,
    hi: &K,
) -> impl Iterator<Item = (usize, &'a T)> {
    let first = keys.partition_point(|k| k.cmp_displaced(off, lo) != Ordering::Greater);
    let last = keys
        .partition_point(|k| k.cmp_displaced(off, hi) == Ordering::Less)
        .min(children.len() - 1);
    (first..=last.max(first)).zip(children[first..=last.max(first)].iter())
}

/// One frame of the iterator's descent: an internal node's children, the index
/// of the next one to visit, and the node's offset to the absolute frame.
type Frame<'a, K, V, M> = (&'a [Tree<K, V, M>], usize, i64);

/// The leaf the iterator is draining, how far into it, and its offset.
type LeafCursor<'a, K, V> = (&'a [(K, V)], usize, i64);

/// In-order iterator over `(key, value)`, keys in the absolute frame.
pub struct Iter<'a, K, V, M> {
    /// Internal nodes on the descent, each with the next child to visit.
    stack: Vec<Frame<'a, K, V, M>>,
    /// The leaf being drained, how far into it we are, and its offset.
    leaf: Option<LeafCursor<'a, K, V>>,
}

impl<'a, K, V, M> Iter<'a, K, V, M> {
    /// Walk to the leftmost leaf of `t`, recording the internal nodes passed.
    /// `off` is the offset of `t`'s parent frame.
    fn descend(&mut self, t: &'a Tree<K, V, M>, off: i64) {
        let mut cur = t;
        let mut off = off;
        while let Some(n) = cur.root.as_deref() {
            off = off.wrapping_add(cur.dsp);
            match &n.kind {
                Kind::Leaf(entries) => {
                    self.leaf = Some((entries, 0, off));
                    return;
                }
                Kind::Internal { children, .. } => {
                    self.stack.push((children, 1, off));
                    cur = &children[0];
                }
            }
        }
    }
}

impl<'a, K: Displace, V, M> Iterator for Iter<'a, K, V, M> {
    type Item = (K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((entries, i, off)) = &mut self.leaf {
                if *i < entries.len() {
                    let (k, v) = &entries[*i];
                    *i += 1;
                    return Some((moved(k, *off).into_owned(), v));
                }
                self.leaf = None;
            }
            // The leaf is drained: take the next unvisited child of the nearest
            // ancestor that still has one, and descend its left spine.
            let next = self.stack.last_mut().and_then(|(children, idx, off)| {
                let children: &'a [Tree<K, V, M>] = children;
                let picked = children.get(*idx);
                *idx += 1;
                picked.map(|c| (c, *off))
            });
            match next {
                Some((t, off)) => self.descend(t, off),
                None => {
                    self.stack.pop()?;
                }
            }
        }
    }
}
