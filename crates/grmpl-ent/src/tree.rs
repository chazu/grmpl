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
//! * **Demand-paged.** A node read back from the granfilade is resident, but
//!   its children are [`paged`](Tree::paged): known by content key, size and
//!   measure, read from disk only when a walk enters them. Counts, measures,
//!   version identity and persistence never page anything in; a lookup pages
//!   in one path. See [`Pager`].
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

mod kd;
#[cfg(test)]
mod kd_laws;
pub mod leaf;
#[cfg(test)]
mod leaf_laws;

pub use kd::Layout;
pub use leaf::{Item, RunValue, Span};

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
/// Internal invariant: `keys.len() + 1 == children.len()`, and `children[i]`
/// holds only keys in the half-open span `[keys[i - 1], keys[i])` (in this
/// node's frame, i.e. after that child's dsp), unbounded at the ends. A
/// separator is written as the least key of the child to its right, but a
/// removal can leave it below that child's least key, so it is a bound, not
/// the key.
enum Kind<K, V, M> {
    /// A run of items in key order: rows, runs and holes ([`leaf`]).
    Leaf(Vec<Item<K, V>>),
    Internal { keys: Vec<K>, children: Vec<Tree<K, V, M>> },
    /// **A k-d split** (Gold's `SplitLoaf`): a binary node dividing its
    /// entries on one column. `children[0]` holds every key whose column
    /// `col` lies below `pivot`'s lone coordinate (a key without the column
    /// counts as below), `children[1]` the rest. Only the k-d layout
    /// ([`kd`]) builds these, and a tree is all one layout: B+ internal
    /// nodes and splits never mix.
    ///
    /// A split on column `0` divides keys exactly as the separator `pivot`
    /// would in an internal node, so every key-range walk treats it as one.
    /// A split on any other column leaves the two children's keys
    /// interleaved, and a walk in key order must visit both.
    Split { col: usize, pivot: K, children: [Tree<K, V, M>; 2] },
}

/// How a read walk sees a node: a run of entries, children divided by
/// separators (a B+ internal node, or a k-d split on column `0`), or children
/// whose keys interleave (a k-d split on any other column).
enum View<'a, K, V, M> {
    Leaf(&'a [Item<K, V>]),
    Sep(&'a [K], &'a [Tree<K, V, M>]),
    Mixed(&'a [Tree<K, V, M>]),
}

impl<K, V, M> Kind<K, V, M> {
    fn view(&self) -> View<'_, K, V, M> {
        match self {
            Kind::Leaf(entries) => View::Leaf(entries),
            Kind::Internal { keys, children } => View::Sep(keys, children),
            Kind::Split { col: 0, pivot, children } => View::Sep(std::slice::from_ref(pivot), children),
            Kind::Split { children, .. } => View::Mixed(children),
        }
    }
}

struct Node<K, V, M> {
    /// The node's contents. Empty only for a **paged** node — one known by its
    /// content key, size and measure, whose frame is still on disk; the first
    /// read that needs its contents pages them in through `pager`.
    kind: OnceLock<Kind<K, V, M>>,
    /// Where a paged node's contents come from. `None` for a node built in
    /// memory, which is resident from birth.
    pager: Option<Arc<dyn Pager<K, V, M>>>,
    /// Rows in the whole subtree.
    size: usize,
    /// Keys reserved by holes in the whole subtree, which hold no rows.
    reserved: u64,
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

impl<K, V, M> Node<K, V, M> {
    /// The node's contents, paging them in on first use.
    ///
    /// A page-in that fails panics: a paged node's frame is referenced by a
    /// durable parent and protected from GC while any handle can still reach
    /// it, so a missing frame means the store is damaged, not that a read raced.
    fn kind(&self) -> &Kind<K, V, M> {
        self.kind.get_or_init(|| {
            let pager = self.pager.as_ref().expect("a non-resident node has a pager");
            let ck = self.ck.get().expect("a paged node knows its content key");
            let loaded = pager.page(ck);
            debug_assert_eq!(
                loaded.root.as_ref().map_or(0, |n| n.size),
                self.size,
                "paged node size disagrees with its parent"
            );
            let node = loaded.root.and_then(|n| Arc::try_unwrap(n).ok());
            node.and_then(|n| n.kind.into_inner()).expect("a pager returns a fresh, resident node")
        })
    }
}

/// **Demand paging.** The source a paged node's contents are read from — the
/// granfilade, which decodes the frame under a content key into a node whose
/// own children are paged in turn. This is how a tree larger than memory is
/// walked: only the nodes a read reaches are ever decoded.
pub trait Pager<K, V, M>: Send + Sync {
    /// The node stored under `ck`, resident, at displacement `0`.
    fn page(&self, ck: &ContentKey) -> Tree<K, V, M>;
}

/// Whether a node's contents are in memory. The granfilade keeps a weak handle
/// on every paged node it hands out and treats the ones still unread as GC
/// roots: their frames are the only copy of what they hold.
pub trait Resident: Send + Sync {
    fn resident(&self) -> bool;
}

impl<K: Send + Sync, V: Send + Sync, M: Send + Sync> Resident for Node<K, V, M> {
    fn resident(&self) -> bool {
        self.kind.get().is_some()
    }
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
    /// A run of items (rows, runs and holes), ascending.
    Leaf(&'a [Item<K, V>]),
    /// Separator keys and the children they divide (`keys.len() + 1` children).
    Internal(&'a [K], &'a [Tree<K, V, M>]),
    /// A k-d split: its column, its pivot (a one-column key, in the node's
    /// local frame) and its two children, below the pivot and at or above it.
    Split(usize, &'a K, &'a [Tree<K, V, M>]),
}

impl<'a, K, V, M> NodeRef<'a, K, V, M> {
    /// The node's children: none for a leaf.
    pub fn children(&self) -> &'a [Tree<K, V, M>] {
        match self {
            NodeRef::Leaf(_) => &[],
            NodeRef::Internal(_, children) | NodeRef::Split(_, _, children) => children,
        }
    }
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
    Leaf(Vec<Item<K, V>>),
    Internal(Vec<K>, Vec<Tree<K, V, M>>),
    Split(usize, K, [Tree<K, V, M>; 2]),
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
    V: RunValue,
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

    /// Keys reserved by holes, which hold no rows — `O(1)`.
    pub fn reserved(&self) -> u64 {
        self.root.as_ref().map_or(0, |n| n.reserved)
    }

    /// No rows and no holes: no node at all.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
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
            Open::Split(col, pivot, [lo, hi]) => Self::split_node(col, pivot, lo, hi),
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
            match n.kind() {
                Kind::Leaf(items) => return leaf::get(items, off, key),
                Kind::Internal { keys, children } => cur = &children[child_index(keys, off, key)],
                Kind::Split { col, pivot, children } => {
                    cur = &children[usize::from(key.cmp_column(*col, pivot, off) != Ordering::Less)]
                }
            }
        }
    }

    /// A new tree with `key → val` inserted or replaced. Persistent: the prior
    /// tree is unchanged and shares every untouched subtree.
    pub fn insert(&self, key: K, val: V) -> Self {
        self.insert_with(key, val, false)
    }

    /// [`insert`](Self::insert), folding rows into runs if `runs` ([`leaf`]):
    /// what a relation that opted into runs writes with.
    pub fn insert_with(&self, key: K, val: V, runs: bool) -> Self {
        debug_assert!(!self.is_kd(), "a B+ insert into a k-d tree");
        if self.root.is_none() {
            return Self::leaf(vec![Item::One(key, val)]);
        }
        match Self::ins(self, key, val, runs) {
            Ins::Done(t) => t,
            Ins::Split(l, sep, r) => Self::internal(vec![sep], vec![l, r]),
        }
    }

    /// Whether any key, a row or a hole's, is exactly `key`.
    pub fn holds_key(&self, key: &K) -> bool {
        let mut cur = self;
        let mut off = 0i64;
        loop {
            let Some(n) = cur.root.as_deref() else { return false };
            off = off.wrapping_add(cur.dsp);
            match n.kind() {
                Kind::Leaf(items) => return leaf::holds(items, off, key),
                Kind::Internal { keys, children } => cur = &children[child_index(keys, off, key)],
                Kind::Split { col, pivot, children } => {
                    cur = &children[usize::from(key.cmp_column(*col, pivot, off) != Ordering::Less)]
                }
            }
        }
    }

    /// Whether `span`'s keys are all free: no row and no hole lies anywhere
    /// from its first key to its last.
    fn free(&self, span: &Span<K>) -> bool {
        let last = span.last();
        !self.any_in(&span.first, &last) && !self.holds_key(&last)
    }

    /// **Reserve `span`'s keys as a hole** (Gold's `OPartialLoaf`), in the B+
    /// layout: keys that exist but hold no rows yet. Reads skip them; writing
    /// a row at one fills it. `None`, changing nothing, if a row or hole
    /// already lies anywhere between the span's first key and its last. The
    /// hole goes in by a cut and a join, as a graft's copy does.
    pub fn reserve(&self, span: Span<K>) -> Option<Self> {
        debug_assert!(!self.is_kd(), "a B+ reserve in a k-d tree");
        if span.n == 0 {
            return Some(self.clone());
        }
        if !self.free(&span) {
            return None;
        }
        let last = span.last();
        let (below, rest) = self.split(&span.first);
        let (_, above) = rest.split(&last);
        let hole = Self::leaf(vec![Item::Hole(span)]);
        Some(Self::join(&Self::join(&below, &hole), &above))
    }

    /// A new tree with `key` removed. Absent keys are a true no-op — the same
    /// shared version is returned, not a rebuilt copy.
    pub fn remove(&self, key: &K) -> Self {
        debug_assert!(!self.is_kd(), "a B+ remove from a k-d tree");
        match Self::rem(self, key, 0) {
            None => self.clone(),
            Some(Ins::Done(t)) => Self::shrink_root(t),
            Some(Ins::Split(l, sep, r)) => Self::internal(vec![sep], vec![l, r]),
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

    /// **WID search.** The entries `keep` accepts, in key order, visiting
    /// only subtrees whose cached measure `admit` accepts.
    ///
    /// This is the search a measure exists for. Key order prunes on the lead
    /// of the key; a measure such as [`Extent`](crate::measure::Extent) prunes
    /// on anything it summarizes, and a subtree it rules out is never paged in.
    ///
    /// The two tests must agree: if `keep` accepts an entry, `admit` accepts
    /// the measure of every subtree holding it — at the least, `admit(&M::entry(k,
    /// v))` whenever `keep(k, v)`, and `admit` monotone under `combine`.
    /// Otherwise a match can hide under a rejected subtree. Both see the
    /// tree's absolute frame, with every dsp above them applied. `keep` is
    /// separate so the per-entry test need not build a measure.
    pub fn search(&self, admit: impl Fn(&M) -> bool, keep: impl Fn(&K, &V) -> bool) -> Vec<(K, V)> {
        let mut out = Vec::new();
        Self::search_into(self, 0, &admit, &keep, &mut out);
        in_key_order(&mut out);
        out
    }

    fn search_into(
        t: &Self,
        off: i64,
        admit: &impl Fn(&M) -> bool,
        keep: &impl Fn(&K, &V) -> bool,
        out: &mut Vec<(K, V)>,
    ) {
        let Some(n) = t.root.as_deref() else { return };
        let off = off.wrapping_add(t.dsp);
        // Test the cached measure before touching the contents, so a subtree
        // ruled out stays on disk.
        let admitted = if off == 0 { admit(&n.measure) } else { admit(&n.measure.displace(off)) };
        if !admitted {
            return;
        }
        match n.kind().view() {
            View::Leaf(items) => {
                for it in items {
                    if let Item::One(k, v) = it {
                        let k = moved(k, off);
                        if keep(&k, v) {
                            out.push((k.into_owned(), v.clone()));
                        }
                        continue;
                    }
                    // A run is a subtree of its own: its measure may rule it out.
                    if it.rows() == 0 || !admit(&it.measure::<M>().displace(off)) {
                        continue;
                    }
                    for (k, v) in it.each_row(off) {
                        if keep(&k, v) {
                            out.push((k, v.clone()));
                        }
                    }
                }
            }
            View::Sep(_, children) | View::Mixed(children) => {
                for c in children {
                    Self::search_into(c, off, admit, keep, out);
                }
            }
        }
    }

    /// How many entries have a key in `[lo, hi)` — `O(log n)` from the cached
    /// subtree sizes, whatever the measure. A tree whose measure carries more
    /// than a count answers "how many" without folding the rest of it.
    pub fn count_range(&self, lo: &K, hi: &K) -> usize {
        if lo >= hi {
            return 0;
        }
        Self::count_into(self, lo, hi, None, None, 0)
    }

    /// The entries with key in `[lo, hi)`, cloned, in order. `O(result + depth)`
    /// — subtrees wholly outside the span are pruned. Cheap when values are
    /// `Arc`-backed (a clone is a refcount bump).
    pub fn range_collect(&self, lo: &K, hi: &K) -> Vec<(K, V)> {
        let mut out = Vec::new();
        if lo < hi {
            Self::range_into(self, lo, hi, 0, &mut out);
            in_key_order(&mut out);
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
    /// the same *absolute* position. Pruning happens at *every* level, not just
    /// the root, which is what makes version-compare cost the edit rather than
    /// the relation.
    ///
    /// **Shape-independent.** Each side is walked as a *frontier*: its
    /// unvisited entries as a key-ordered run of whole subtrees. Two heads that
    /// are the same node at the same position are dropped together, whatever
    /// their parents look like; two that differ are both opened. So a shared
    /// subtree is found however the spines above it were rebuilt: by a split,
    /// a join, or a graft's seams. Comparing across a graft costs the copy and
    /// its seams, not the relation.
    pub fn diff(&self, other: &Self) -> Vec<EntryDiff<K, V>>
    where
        V: PartialEq,
    {
        let mut out = Vec::new();
        if self.is_kd() || other.is_kd() {
            Self::kd_diff_into(self, 0, other, 0, &mut out);
            out.sort_by(|a, b| a.0.cmp(&b.0));
        } else {
            Self::diff_into(self, other, &mut out);
        }
        out
    }

    /// In-order `(key, value)` iterator — the **canonical** ordering of the map,
    /// independent of tree shape. This is the identity view. Keys come out in
    /// the absolute frame, owned (a clone, or a displaced copy under a dsp).
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        let mut it = Iter { stack: Vec::new(), leaf: None, sorted: None };
        it.descend(self, 0);
        it
    }

    /// Whether this tree is in the k-d layout: its root is a split. A lone
    /// leaf is valid in either layout and reads the same in both.
    pub fn is_kd(&self) -> bool {
        matches!(self.root.as_deref().map(|n| n.kind()), Some(Kind::Split { .. }))
    }

    // --- split, join, graft -------------------------------------------------

    /// Split into the entries below `key` and those at or above it. Persistent:
    /// `O(log n)` new nodes along the cut, everything else shared.
    ///
    /// A cut outside the tree's span shares the whole tree and writes nothing.
    pub fn split(&self, key: &K) -> (Self, Self) {
        debug_assert!(!self.is_kd(), "a B+ split of a k-d tree");
        if self.is_empty() {
            return (Tree::new(), Tree::new());
        }
        if self.max_key() < *key {
            return (self.clone(), Tree::new());
        }
        if self.min_key() >= *key {
            return (Tree::new(), self.clone());
        }
        Self::split_at(self, key)
    }

    /// Concatenate two trees whose key spans do not interleave: every key of
    /// `left` must be below every key of `right`. Persistent: `O(log n)` new
    /// nodes along the seam, everything else shared.
    pub fn join(left: &Self, right: &Self) -> Self {
        debug_assert!(!left.is_kd() && !right.is_kd(), "a B+ join of k-d trees");
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
        match self.root.as_deref()?.kind() {
            Kind::Leaf(entries) => Some(NodeRef::Leaf(entries)),
            Kind::Internal { keys, children } => Some(NodeRef::Internal(keys, children)),
            Kind::Split { col, pivot, children } => Some(NodeRef::Split(*col, pivot, children)),
        }
    }

    /// The root node's measure in its own local frame (this handle's dsp not
    /// applied) — what a parent frame records for a child.
    pub fn local_measure(&self) -> Option<&M> {
        Some(&self.root.as_deref()?.measure)
    }

    /// A **paged** tree: a node known by its content key, entry count and local
    /// measure, whose contents stay on disk until a read reaches them. Length,
    /// measure, content key and version identity are all answerable without
    /// paging it in.
    pub fn paged(ck: ContentKey, size: usize, reserved: u64, measure: M, pager: Arc<dyn Pager<K, V, M>>) -> Self {
        Tree {
            root: Some(Arc::new(Node {
                kind: OnceLock::new(),
                pager: Some(pager),
                size,
                reserved,
                measure,
                ck: OnceLock::from(ck),
            })),
            dsp: 0,
        }
    }

    /// A weak handle on the root node's residency, for the granfilade's GC
    /// roots (see [`Resident`]).
    pub fn residency(&self) -> Option<std::sync::Weak<dyn Resident>>
    where
        K: Send + Sync + 'static,
        V: Send + Sync + 'static,
        M: Send + Sync + 'static,
    {
        let node = self.root.as_ref()?;
        let weak: std::sync::Weak<Node<K, V, M>> = Arc::downgrade(node);
        Some(weak)
    }

    /// Rebuild a leaf from its exact persisted items — no rebalancing, so a
    /// load round-trips the stored shape and content keys stay stable.
    pub fn leaf_of(items: Vec<Item<K, V>>) -> Self {
        Self::leaf(items)
    }

    /// Rebuild an internal node from its exact persisted separators and children
    /// (each already at its persisted dsp). `keys.len() + 1 == children.len()`
    /// must hold, as it does for anything this module produced.
    pub fn internal_of(keys: Vec<K>, children: Vec<Self>) -> Self {
        Self::internal(keys, children)
    }

    /// Rebuild a k-d split from its exact persisted column, pivot and two
    /// children (each already at its persisted dsp).
    pub fn split_of(col: usize, pivot: K, lo: Self, hi: Self) -> Self {
        Self::split_node(col, pivot, lo, hi)
    }

    // --- construction -------------------------------------------------------

    fn leaf(items: Vec<Item<K, V>>) -> Self {
        let (measure, size, reserved) = leaf::summary::<K, V, M>(&items);
        Tree {
            root: Some(Arc::new(Node {
                size,
                reserved,
                measure,
                kind: OnceLock::from(Kind::Leaf(items)),
                pager: None,
                ck: OnceLock::new(),
            })),
            dsp: 0,
        }
    }

    fn internal(keys: Vec<K>, children: Vec<Self>) -> Self {
        let mut measure = M::empty();
        let (mut size, mut reserved) = (0, 0);
        for c in &children {
            match (c.dsp, c.local_measure()) {
                (0, Some(m)) => measure.absorb(m),
                _ => measure.absorb(&c.measure()),
            }
            size += c.len();
            reserved += c.reserved();
        }
        Tree {
            root: Some(Arc::new(Node {
                size,
                reserved,
                measure,
                kind: OnceLock::from(Kind::Internal { keys, children }),
                pager: None,
                ck: OnceLock::new(),
            })),
            dsp: 0,
        }
    }

    /// A k-d split node over two non-empty children, both in its frame.
    fn split_node(col: usize, pivot: K, lo: Self, hi: Self) -> Self {
        let mut measure = M::empty();
        for c in [&lo, &hi] {
            match (c.dsp, c.local_measure()) {
                (0, Some(m)) => measure.absorb(m),
                _ => measure.absorb(&c.measure()),
            }
        }
        Tree {
            root: Some(Arc::new(Node {
                size: lo.len() + hi.len(),
                reserved: lo.reserved() + hi.reserved(),
                measure,
                kind: OnceLock::from(Kind::Split { col, pivot, children: [lo, hi] }),
                pager: None,
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

    /// A rewritten leaf, its runs folded and split in two if it overflowed:
    /// a block written row by row ends as one item, not a chain of full
    /// leaves.
    fn finish_leaf(mut e: Vec<Item<K, V>>, runs: bool) -> Ins<K, V, M> {
        if e.len() > B {
            leaf::compress(&mut e, runs);
        }
        if e.len() <= B {
            return Ins::Done(if e.is_empty() { Tree::new() } else { Self::leaf(e) });
        }
        let right = e.split_off(e.len() / 2);
        let sep = right[0].lo_key().clone();
        Ins::Split(Self::leaf(e), sep, Self::leaf(right))
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
        match n.kind() {
            Kind::Leaf(items) => Open::Leaf(items.iter().map(|it| it.displaced(d)).collect()),
            Kind::Internal { keys, children } => Open::Internal(
                if d == 0 { keys.clone() } else { keys.iter().map(|k| k.displace(d)).collect() },
                children.iter().map(|c| c.relocate(d)).collect(),
            ),
            Kind::Split { col, pivot, children: [lo, hi] } => {
                Open::Split(*col, moved(pivot, d).into_owned(), [lo.relocate(d), hi.relocate(d)])
            }
        }
    }

    /// The number of levels: `0` empty, `1` a leaf. Every leaf sits at the same
    /// depth, so the leftmost spine says it.
    fn height(&self) -> usize {
        let mut h = 0;
        let mut cur = self;
        while let Some(n) = cur.root.as_deref() {
            h += 1;
            match n.kind().view() {
                View::Leaf(_) => break,
                View::Sep(_, children) | View::Mixed(children) => cur = &children[0],
            }
        }
        h
    }

    /// The least key, in this handle's frame. The tree must be non-empty.
    /// Down a separator spine that is one path; under a split on another
    /// column both children are asked.
    fn min_key(&self) -> K {
        Self::extreme_key(self, 0, false)
    }

    /// The greatest key, in this handle's frame. The tree must be non-empty.
    fn max_key(&self) -> K {
        Self::extreme_key(self, 0, true)
    }

    fn extreme_key(t: &Self, off: i64, greatest: bool) -> K {
        let off = off.wrapping_add(t.dsp);
        match t.root.as_deref().expect("an extreme key of an empty tree").kind().view() {
            View::Leaf(items) => leaf::extreme(items, off, greatest),
            View::Sep(_, children) => {
                let c = if greatest { &children[children.len() - 1] } else { &children[0] };
                Self::extreme_key(c, off, greatest)
            }
            View::Mixed(children) => {
                let keys = children.iter().map(|c| Self::extreme_key(c, off, greatest));
                if greatest { keys.max() } else { keys.min() }.expect("a split has children")
            }
        }
    }

    // --- insert -------------------------------------------------------------

    /// Insert into the subtree under `t`; `key` and the result are in `t`'s
    /// parent frame.
    fn ins(t: &Self, key: K, val: V, runs: bool) -> Ins<K, V, M> {
        match Self::open(t) {
            Open::Leaf(mut e) => {
                leaf::insert(&mut e, key, val, runs);
                Self::finish_leaf(e, runs)
            }
            Open::Internal(mut ks, mut ch) => {
                let i = child_index(&ks, 0, &key);
                match Self::ins(&ch[i], key, val, runs) {
                    Ins::Done(c) => {
                        ch[i] = c;
                        // Joining runs can leave a leaf fewer items than before.
                        Self::fix(&mut ks, &mut ch, i);
                        Ins::Done(Self::from_parts(ks, ch))
                    }
                    Ins::Split(l, sep, r) => {
                        ch[i] = l;
                        ch.insert(i + 1, r);
                        ks.insert(i, sep);
                        Self::finish(ks, ch)
                    }
                }
            }
            Open::Split(..) => unreachable!("a B+ insert met a k-d split"),
        }
    }

    // --- remove -------------------------------------------------------------

    /// Remove `key` from the subtree under `t`, or `None` if it was absent (so the
    /// caller keeps sharing the existing version). `off` carries `t`'s parent
    /// frame to `key`'s; the result is in `t`'s parent frame. It may be below the
    /// occupancy floor; the *parent* repairs it. It may also split: cutting a
    /// row out of a run leaves two items where there was one, so a full leaf
    /// can overflow on a remove.
    fn rem(t: &Self, key: &K, off: i64) -> Option<Ins<K, V, M>> {
        let n = t.root.as_deref()?;
        let off = off.wrapping_add(t.dsp);
        match n.kind() {
            Kind::Leaf(items) => {
                let (i, j) = leaf::find_row(items, off, key)?;
                let Open::Leaf(mut e) = Self::open(t) else { unreachable!() };
                leaf::remove_at(&mut e, i, j);
                Some(Self::finish_leaf(e, false))
            }
            Kind::Internal { keys, children } => {
                let i = child_index(keys, off, key);
                let newc = Self::rem(&children[i], key, off)?;
                let Open::Internal(mut ks, mut ch) = Self::open(t) else { unreachable!() };
                match newc {
                    Ins::Done(c) => {
                        ch[i] = c.relocate(t.dsp);
                        Self::fix(&mut ks, &mut ch, i);
                        Some(Ins::Done(Self::internal(ks, ch)))
                    }
                    Ins::Split(l, sep, r) => {
                        ch[i] = l.relocate(t.dsp);
                        ch.insert(i + 1, r.relocate(t.dsp));
                        ks.insert(i, sep.displace(t.dsp));
                        Some(Self::finish(ks, ch))
                    }
                }
            }
            Kind::Split { .. } => unreachable!("a B+ remove met a k-d split"),
        }
    }

    /// The root is exempt from the occupancy floor, but it may need to shrink:
    /// an internal root down to one child becomes that child, and an emptied
    /// leaf root becomes the empty tree.
    fn shrink_root(t: Self) -> Self {
        match t.root.as_deref().map(|n| n.kind()) {
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
                    ks.insert(j, r[0].lo_key().clone());
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
        match self.root.as_deref().map(|n| n.kind()) {
            Some(Kind::Leaf(e)) => e.len() < MIN,
            Some(Kind::Internal { children, .. }) => children.len() < MIN,
            Some(Kind::Split { .. }) => unreachable!("B+ occupancy of a k-d split"),
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
            Open::Leaf(e) => {
                let (l, r) = leaf::split_at(e, key);
                let side = |v: Vec<Item<K, V>>| if v.is_empty() { Tree::new() } else { Self::leaf(v) };
                (side(l), side(r))
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
            Open::Split(..) => unreachable!("a B+ split met a k-d split"),
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
        let mut acc = acc;
        if contained(nlo, nhi, lo, hi) {
            if off == 0 {
                acc.absorb(&n.measure);
            } else {
                acc.absorb(&n.measure.displace(off));
            }
            return acc;
        }
        match n.kind().view() {
            View::Leaf(items) => {
                leaf::fold_in(items, off, lo, hi, &mut acc);
                acc
            }
            View::Sep(keys, children) => {
                for (idx, c) in span(keys, children, off, lo, hi) {
                    let clo = if idx == 0 { nlo } else { Some((&keys[idx - 1], off)) };
                    let chi = if idx == children.len() - 1 { nhi } else { Some((&keys[idx], off)) };
                    acc = Self::fold_range(c, lo, hi, clo, chi, off, acc);
                }
                acc
            }
            View::Mixed(children) => {
                for c in children {
                    acc = Self::fold_range(c, lo, hi, nlo, nhi, off, acc);
                }
                acc
            }
        }
    }

    /// [`fold_range`](Self::fold_range) for the entry count alone, from the
    /// cached sizes: no measure is built, however rich the tree's measure is.
    fn count_into(t: &Self, lo: &K, hi: &K, nlo: Bound<'_, K>, nhi: Bound<'_, K>, off: i64) -> usize {
        let Some(n) = t.root.as_deref() else { return 0 };
        if disjoint(nlo, nhi, lo, hi) {
            return 0;
        }
        if contained(nlo, nhi, lo, hi) {
            return n.size;
        }
        let off = off.wrapping_add(t.dsp);
        match n.kind().view() {
            View::Leaf(items) => leaf::count_in(items, off, lo, hi),
            View::Sep(keys, children) => span(keys, children, off, lo, hi)
                .map(|(idx, c)| {
                    let clo = if idx == 0 { nlo } else { Some((&keys[idx - 1], off)) };
                    let chi = if idx == children.len() - 1 { nhi } else { Some((&keys[idx], off)) };
                    Self::count_into(c, lo, hi, clo, chi, off)
                })
                .sum(),
            View::Mixed(children) => children.iter().map(|c| Self::count_into(c, lo, hi, nlo, nhi, off)).sum(),
        }
    }

    fn range_into(t: &Self, lo: &K, hi: &K, off: i64, out: &mut Vec<(K, V)>) {
        let n = match t.root.as_deref() {
            None => return,
            Some(n) => n,
        };
        let off = off.wrapping_add(t.dsp);
        match n.kind().view() {
            View::Leaf(items) => leaf::range_in(items, off, lo, hi, out),
            View::Sep(keys, children) => {
                for (_, c) in span(keys, children, off, lo, hi) {
                    Self::range_into(c, lo, hi, off, out);
                }
            }
            View::Mixed(children) => {
                for c in children {
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
            return n.size > 0 || n.reserved > 0;
        }
        let off = off.wrapping_add(t.dsp);
        match n.kind().view() {
            View::Leaf(items) => leaf::any_in(items, off, lo, hi),
            View::Sep(keys, children) => span(keys, children, off, lo, hi).any(|(idx, c)| {
                let clo = if idx == 0 { nlo } else { Some((&keys[idx - 1], off)) };
                let chi = if idx == children.len() - 1 { nhi } else { Some((&keys[idx], off)) };
                Self::any_into(c, lo, hi, clo, chi, off)
            }),
            View::Mixed(children) => children.iter().any(|c| Self::any_into(c, lo, hi, nlo, nhi, off)),
        }
    }

    fn last_le_in<'a>(t: &'a Self, key: &K, off: i64) -> Option<(K, &'a V)> {
        let n = t.root.as_deref()?;
        let off = off.wrapping_add(t.dsp);
        match n.kind().view() {
            View::Leaf(items) => leaf::last_le(items, off, key),
            View::Sep(keys, children) => {
                // Descend the child whose span holds `key`; if that subtree has
                // nothing at or below it, the answer is the greatest entry of a
                // preceding sibling.
                let i = child_index(keys, off, key);
                (0..=i).rev().find_map(|j| Self::last_le_in(&children[j], key, off))
            }
            View::Mixed(children) => children
                .iter()
                .filter_map(|c| Self::last_le_in(c, key, off))
                .max_by(|a, b| a.0.cmp(&b.0)),
        }
    }

    fn diff_into(a: &Self, b: &Self, out: &mut Vec<EntryDiff<K, V>>)
    where
        V: PartialEq,
    {
        let (mut xs, mut ys) = (Self::frontier(a), Self::frontier(b));
        loop {
            let step = match (xs.last(), ys.last()) {
                (None, None) => return,
                (Some(_), None) => Step::Left,
                (None, Some(_)) => Step::Right,
                (Some(x), Some(y)) => Self::step(x, y),
            };
            match step {
                Step::Skip => {
                    xs.pop();
                    ys.pop();
                }
                Step::Left => Self::emit_one(&mut xs, true, out),
                Step::Right => Self::emit_one(&mut ys, false, out),
                Step::Pair => {
                    // Two runs at one key with one stride hold the same keys for
                    // as long as both last: compare them as one step.
                    let stretch = match (xs.last(), ys.last()) {
                        (Some(Head::Run(a, _, _, i)), Some(Head::Run(b, _, _, j))) if a.stride == b.stride => {
                            (a.n - i).min(b.n - j)
                        }
                        _ => 1,
                    };
                    let ((k, va), (_, vb)) = (head_row(xs.last().expect("a head")), head_row(ys.last().expect("a head")));
                    if va != vb {
                        let stride = match xs.last() {
                            Some(Head::Run(s, ..)) => Some(&s.stride),
                            _ => None,
                        };
                        for t in 0..stretch {
                            let k = if t == 0 { k.clone() } else { k.step(stride.expect("a stretch is a run"), t as i64) };
                            out.push((k, Some(va.clone()), Some(vb.clone())));
                        }
                    }
                    advance(&mut xs, stretch);
                    advance(&mut ys, stretch);
                }
                Step::OpenLeft => Self::open_onto(&mut xs),
                Step::OpenRight => Self::open_onto(&mut ys),
                Step::OpenBoth => {
                    Self::open_onto(&mut xs);
                    Self::open_onto(&mut ys);
                }
            }
        }
    }

    /// A whole tree as a one-item frontier.
    fn frontier(t: &Self) -> Vec<Head<'_, K, V, M>> {
        if t.is_empty() {
            return Vec::new();
        }
        vec![Head::Node(t, 0, None)]
    }

    /// What to do with the two frontiers' heads. Every answer either consumes a
    /// head or opens one, and only claims a key is absent from a side when
    /// every key left on that side is greater.
    ///
    /// Opening every node that is not shared may open a subtree the other side
    /// holds one level deeper, but the walk realigns by itself: once both sides
    /// have opened down to a shared leaf, the right siblings on each side are
    /// the same nodes again. So a misstep costs one path, never a subtree.
    fn step(x: &Head<'_, K, V, M>, y: &Head<'_, K, V, M>) -> Step {
        match (x, y) {
            (Head::Node(a, i, _), Head::Node(b, j, _)) if same_at(a, *i, b, *j) => Step::Skip,
            (Head::Node(..), Head::Node(..)) => Step::OpenBoth,
            // A node whose separator bound lies above the row holds nothing
            // at or below it, which spares opening the leaf after an edit.
            (_, Head::Node(.., Some((lo, o)))) if above_key(*lo, *o, &head_row(x).0) => Step::Left,
            (Head::Node(.., Some((lo, o))), _) if above_key(*lo, *o, &head_row(y).0) => Step::Right,
            (_, Head::Node(..)) => Step::OpenRight,
            (Head::Node(..), _) => Step::OpenLeft,
            _ => match head_row(x).0.cmp(&head_row(y).0) {
                Ordering::Less => Step::Left,
                Ordering::Greater => Step::Right,
                Ordering::Equal => Step::Pair,
            },
        }
    }

    /// Report the head as present on one side only: a whole node, or one row.
    fn emit_one(stack: &mut Vec<Head<'_, K, V, M>>, left: bool, out: &mut Vec<EntryDiff<K, V>>) {
        let mut push = |k: K, v: &V| {
            out.push(if left { (k, Some(v.clone()), None) } else { (k, None, Some(v.clone())) });
        };
        match stack.last().expect("a head") {
            Head::Node(t, off, _) => {
                let mut it = Iter { stack: Vec::new(), leaf: None, sorted: None };
                it.descend(t, *off);
                for (k, v) in it {
                    push(k, v);
                }
                stack.pop();
            }
            head => {
                let (k, v) = head_row(head);
                push(k, v);
                advance(stack, 1);
            }
        }
    }

    /// Replace the frontier's head node with its contents, in key order:
    /// entries for a leaf, children (each with its separator bound) for an
    /// internal node.
    fn open_onto<'a>(stack: &mut Vec<Head<'a, K, V, M>>) {
        let Some(Head::Node(t, off, lo)) = stack.pop() else { unreachable!("only a node is opened") };
        let off = off.wrapping_add(t.dsp);
        match t.root.as_deref().expect("no empty node on a frontier").kind() {
            Kind::Leaf(items) => {
                stack.extend(items.iter().rev().filter_map(|it| match it {
                    Item::One(k, v) => Some(Head::Entry(k, off, v)),
                    Item::Run(s, v) => Some(Head::Run(s, v, off, 0)),
                    Item::Hole(_) => None,
                }));
            }
            Kind::Internal { keys, children } => {
                for (i, c) in children.iter().enumerate().rev() {
                    let lo = if i == 0 { lo } else { Some((&keys[i - 1], off)) };
                    stack.push(Head::Node(c, off, lo));
                }
            }
            Kind::Split { .. } => unreachable!("a k-d tree is compared by `kd_diff_into`"),
        }
    }
}

/// One item on a [`Tree::diff`] frontier: an entry, or a whole subtree not yet
/// opened. A frontier lists its items in key order, each wholly below the next.
enum Head<'a, K, V, M> {
    /// An entry, with the offset carrying its key to the absolute frame.
    Entry(&'a K, i64, &'a V),
    /// A run from its row `i` on, with the offset carrying it to the absolute
    /// frame.
    Run(&'a Span<K>, &'a V, i64, u64),
    /// A subtree, the offset of its parent's frame, and the separator that
    /// bounds it below (with the offset carrying it to the absolute frame).
    /// Removals leave separators stale, so the bound may lie below the
    /// subtree's least key, never above it.
    Node(&'a Tree<K, V, M>, i64, Bound<'a, K>),
}

/// Separator `lo` (at offset `o`) lies above the absolute key `k`.
fn above_key<K: Displace>(lo: &K, o: i64, k: &K) -> bool {
    lo.cmp_displaced(o, k) == Ordering::Greater
}

/// The row a row head stands at, key in the absolute frame.
fn head_row<'a, K: Displace, V, M>(h: &Head<'a, K, V, M>) -> (K, &'a V) {
    match h {
        Head::Entry(k, off, v) => (k.displace(*off), v),
        Head::Run(s, v, off, i) => (s.row(*i).displace(*off), v),
        Head::Node(..) => unreachable!("a node has no row of its own"),
    }
}

/// Consume `by` rows from the head row of `stack`.
fn advance<K: Displace, V, M>(stack: &mut Vec<Head<'_, K, V, M>>, by: u64) {
    match stack.last_mut() {
        Some(Head::Run(s, _, _, i)) if *i + by < s.n => *i += by,
        _ => {
            stack.pop();
        }
    }
}

/// `a` (under a parent frame at `i`) and `b` (at `j`) are the same node at the
/// same absolute position, so hold the same entries: by pointer, or by
/// memoized content key for one node reached through two handles (a reloaded
/// version, a fork).
fn same_at<K, V, M>(a: &Tree<K, V, M>, i: i64, b: &Tree<K, V, M>, j: i64) -> bool {
    if i.wrapping_add(a.dsp) != j.wrapping_add(b.dsp) {
        return false;
    }
    match (a.root.as_deref(), b.root.as_deref()) {
        (Some(x), Some(y)) => std::ptr::eq(x, y) || matches!((x.ck.get(), y.ck.get()), (Some(p), Some(q)) if p == q),
        _ => false,
    }
}

/// What [`Tree::diff`] does with its two heads.
enum Step {
    /// The same subtree on both sides: nothing under it differs.
    Skip,
    /// The left head lies below every key left on the right, so it is absent
    /// there.
    Left,
    /// The mirror of `Left`.
    Right,
    /// Two entries with one key.
    Pair,
    /// Replace a head node with its contents.
    OpenLeft,
    OpenRight,
    OpenBoth,
}

#[cfg(test)]
impl<K, V, M> Tree<K, V, M>
where
    K: Ord + Displace + std::fmt::Debug,
    V: RunValue,
    M: Measure<K, V>,
{
    /// Assert every structural invariant, through the displaced frames: keys in
    /// order and inside their separators' spans, uniform leaf depth, occupancy
    /// within `[MIN, B]` below the root, and cached sizes that match.
    pub(crate) fn check(&self) {
        if self.is_kd() {
            return self.kd_check();
        }
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
        match n.kind() {
            Kind::Leaf(items) => {
                assert!(!items.is_empty(), "empty leaf");
                assert!(items.len() <= B, "leaf over arity");
                assert!(is_root || items.len() >= MIN, "leaf under the floor: {}", items.len());
                leaf::check_items(items);
                for it in items {
                    assert!(within(it.lo_key()), "leaf key {:?} outside its span", abs(it.lo_key()));
                    assert!(within(&it.hi_key()), "leaf key {:?} outside its span", abs(&it.hi_key()));
                }
                let (_, size, reserved) = leaf::summary::<K, V, M>(items);
                assert_eq!((n.size, n.reserved), (size, reserved), "cached counts disagree with the items");
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
            Kind::Split { .. } => panic!("a k-d split inside a B+ tree"),
        }
    }
}

/// A known bound on a subtree's keys: a separator and the offset that carries
/// its frame to the query's.
type Bound<'a, K> = Option<(&'a K, i64)>;

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

/// The leaf the iterator is draining: its items, the item and row next, and
/// its offset.
type LeafCursor<'a, K, V> = (&'a [Item<K, V>], usize, u64, i64);

/// In-order iterator over `(key, value)`, keys in the absolute frame.
///
/// A k-d split on a column other than `0` interleaves its children's keys, so
/// the iterator reads such a subtree whole and sorts it: key order costs a
/// sort wherever the tree is not divided in key order.
pub struct Iter<'a, K, V, M> {
    /// Internal nodes on the descent, each with the next child to visit.
    stack: Vec<Frame<'a, K, V, M>>,
    /// The leaf being drained, how far into it we are, and its offset.
    leaf: Option<LeafCursor<'a, K, V>>,
    /// A sorted subtree being drained.
    sorted: Option<std::vec::IntoIter<(K, &'a V)>>,
}

impl<'a, K: Displace, V, M> Iter<'a, K, V, M> {
    /// Walk to the leftmost leaf of `t`, recording the internal nodes passed.
    /// `off` is the offset of `t`'s parent frame.
    fn descend(&mut self, t: &'a Tree<K, V, M>, off: i64) {
        let mut cur = t;
        let mut off = off;
        while let Some(n) = cur.root.as_deref() {
            off = off.wrapping_add(cur.dsp);
            match n.kind().view() {
                View::Leaf(items) => {
                    self.leaf = Some((items, 0, 0, off));
                    return;
                }
                View::Sep(_, children) => {
                    self.stack.push((children, 1, off));
                    cur = &children[0];
                }
                View::Mixed(children) => {
                    let mut all = Vec::with_capacity(n.size);
                    for c in children {
                        collect_all(c, off, &mut all);
                    }
                    all.sort_by(|a, b| a.0.cmp(&b.0));
                    self.sorted = Some(all.into_iter());
                    return;
                }
            }
        }
    }
}

/// Every entry under `t`, keys moved to the frame `off` carries `t`'s parent
/// to, in no particular order.
fn collect_all<'a, K: Displace, V, M>(t: &'a Tree<K, V, M>, off: i64, out: &mut Vec<(K, &'a V)>) {
    let Some(n) = t.root.as_deref() else { return };
    let off = off.wrapping_add(t.dsp);
    match n.kind().view() {
        View::Leaf(items) => {
            for it in items {
                out.extend(it.each_row(off));
            }
        }
        View::Sep(_, children) | View::Mixed(children) => {
            for c in children {
                collect_all(c, off, out);
            }
        }
    }
}

/// Sort `out` by key unless it already is: a walk over separators emits key
/// order, and only a k-d split on another column interleaves.
fn in_key_order<K: Ord, X>(out: &mut [(K, X)]) {
    if !out.windows(2).all(|w| w[0].0 <= w[1].0) {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
}

impl<'a, K: Displace, V, M> Iterator for Iter<'a, K, V, M> {
    type Item = (K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((items, i, j, off)) = &mut self.leaf {
                let items: &'a [Item<K, V>] = items;
                while *i < items.len() {
                    match &items[*i] {
                        Item::One(k, v) => {
                            *i += 1;
                            return Some((moved(k, *off).into_owned(), v));
                        }
                        Item::Run(s, v) if *j < s.n => {
                            *j += 1;
                            return Some((s.row(*j - 1).displace(*off), v));
                        }
                        _ => {
                            *i += 1;
                            *j = 0;
                        }
                    }
                }
                self.leaf = None;
            }
            if let Some(sorted) = &mut self.sorted {
                if let Some(item) = sorted.next() {
                    return Some(item);
                }
                self.sorted = None;
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
