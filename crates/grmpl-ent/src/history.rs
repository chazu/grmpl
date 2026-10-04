//! **History: Gold's H-tree, over immutable nodes.**
//!
//! In Gold every node of a content tree has a history crum holding its
//! **O-parents**, the nodes that contain it, and the crum at an orgl's root
//! holds the editions using it (`udanax-top.st` 27104–27692). The H-tree is the
//! content DAG inverted, and walking it upward answers what Gold's backend is
//! built around: which editions hold this content (backfollow), and what two
//! editions share wherever it sits (`sharedRegion`, `mapSharedTo`).
//!
//! grmpl's nodes are immutable and content-addressed, so they cannot carry
//! back-pointers that change as new versions arrive. The H-tree is therefore an
//! **index beside them**, keyed by content key:
//!
//! * `parents`: `(child, parent, the child's dsp in the parent's frame)` — the
//!   O-parent sets. A node sharing one child twice at different displacements
//!   (a block grafted beside its template) has two edges.
//! * `holders`: `(root, branch, (relation, edition))` — the versions whose Fact
//!   root a node is: Gold's bottom crum and its editions.
//! * `born`: `(node, branch) → the first edition it was indexed at on that
//!   branch` — Gold's history cut. A node can be in a version only if it was
//!   born on that version's lineage by then, which prunes upward searches the
//!   way Gold's `isLE:` does. Content addressing means one node can be born
//!   independently on two branches, so the cut is kept per branch.
//! * `cursor`: `branch → the edition indexed through`.
//!
//! The index records content keys as data, not links, so it never keeps
//! content alive: GC sweeps a node whatever the index says, and a query ignores
//! versions that consolidation has retired.
//!
//! **Indexing is deferred work** in the spirit of Gold's Agenda: a commit never
//! touches the index. [`History::index_version`] walks a new version from its
//! root and stops at every node already born on the version's lineage, so it
//! visits only the version's new nodes, and records an edge for each child of
//! a node seen for the first time anywhere. That is the H-tree's price: every
//! child of every new node gains a parent, so a commit that copies a spine of
//! `h` nodes adds about `h × B` edges.

use std::collections::HashMap;

use crate::dag::{BranchId, Dag};
use crate::granfilade::{content_key, ContentKey, PersistKey, PersistMeasure, PersistVal};
use crate::measure::Count;
use crate::tree::{NodeRef, Tree};

type Ck = ContentKey;

/// `(child, parent, child's dsp in the parent's frame)`.
type Parents = Tree<(Ck, Ck, i64), (), Count>;
/// `(root, branch, (relation, edition))`.
type Holders = Tree<(Ck, u64, (u64, u64)), (), Count>;
/// `(node, branch) → the first edition it was indexed at on that branch`.
type Born = Tree<(Ck, u64), u64, Count>;
/// `branch → the edition indexed through`.
type Cursor = Tree<u64, u64, Count>;

/// One version a root is the Fact root of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Holder {
    pub branch: BranchId,
    pub rel: u32,
    pub edition: u64,
}

/// The history index (see the module docs).
#[derive(Clone, Default)]
pub(crate) struct History {
    parents: Parents,
    holders: Holders,
    born: Born,
    cursor: Cursor,
}

/// The least key strictly above `ck`, so `[ck, succ(ck))` covers every entry
/// keyed by `ck`. `None` past the greatest key.
fn succ(ck: &Ck) -> Option<Ck> {
    let mut out = *ck;
    for b in out.iter_mut().rev() {
        if *b == u8::MAX {
            *b = 0;
        } else {
            *b += 1;
            return Some(out);
        }
    }
    None
}

impl History {
    /// The edition `branch` is indexed through, if it has been indexed at all.
    pub fn cursor(&self, branch: BranchId) -> Option<u64> {
        self.cursor.get(&branch).copied()
    }

    pub fn set_cursor(&mut self, branch: BranchId, edition: u64) {
        self.cursor = self.cursor.insert(branch, edition);
    }

    /// Index relation `rel`'s Fact root at `edition` on `branch`. Versions must
    /// be indexed in edition order per branch, and a branch's ancestors through
    /// its fork point before it, so `born` only ever grows backwards in time
    /// along a lineage. Returns the nodes visited.
    pub fn index_version<K, V, M>(&mut self, dag: &Dag, at: Holder, root: &Tree<K, V, M>) -> usize
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let root = root.normalized();
        let Some(rck) = content_key(&root) else { return 0 };
        self.holders = self.holders.insert((rck, at.branch, (at.rel as u64, at.edition)), ());
        let lineage = dag.lineage(at.branch, at.edition);
        self.visit(&root, rck, &lineage, at.branch, at.edition)
    }

    fn visit<K, V, M>(&mut self, t: &Tree<K, V, M>, ck: Ck, lineage: &[(BranchId, u64)], b: BranchId, e: u64) -> usize
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        // Already in this lineage by `e`: so is everything beneath it.
        if self.visible(&ck, lineage) {
            return 0;
        }
        self.born = self.born.insert((ck, b), e);
        let mut visited = 1;
        if let Some(NodeRef::Internal(_, children)) = t.node() {
            for c in children {
                let cck = *c.ck_cell().and_then(|cell| cell.get()).expect("a keyed node's children are keyed");
                // Edges are a set: a node built again on another branch adds
                // nothing new.
                self.parents = self.parents.insert((cck, ck, c.dsp()), ());
                visited += self.visit(c, cck, lineage, b, e);
            }
        }
        visited
    }

    /// Whether `ck` was born on `lineage` (newest first, each branch with the
    /// highest edition of it that flows into the point) by that bound.
    pub fn visible(&self, ck: &Ck, lineage: &[(BranchId, u64)]) -> bool {
        lineage.iter().any(|(b, bound)| self.born.get(&(*ck, *b)).is_some_and(|e| e <= bound))
    }

    /// The O-parents of `ck`: `(parent, ck's dsp in the parent's frame)`.
    fn parents_of(&self, ck: &Ck) -> Vec<(Ck, i64)> {
        let lo = (*ck, [0; 32], i64::MIN);
        let rows = match succ(ck) {
            Some(next) => self.parents.range_collect(&lo, &(next, [0; 32], i64::MIN)),
            None => self.parents.range_collect(&lo, &(*ck, [u8::MAX; 32], i64::MAX)),
        };
        rows.into_iter().map(|((_, p, d), ())| (p, d)).collect()
    }

    /// The versions whose Fact root `ck` is.
    fn holders_of(&self, ck: &Ck) -> Vec<Holder> {
        let lo = (*ck, 0, (0, 0));
        let rows = match succ(ck) {
            Some(next) => self.holders.range_collect(&lo, &(next, 0, (0, 0))),
            None => self.holders.range_collect(&lo, &(*ck, u64::MAX, (u64::MAX, u64::MAX))),
        };
        rows.into_iter()
            .map(|((_, branch, (rel, edition)), ())| Holder { branch, rel: rel as u32, edition })
            .collect()
    }

    /// **Backfollow from one node**: every version whose Fact tree holds `ck`,
    /// as `(version, offset of ck in that version's frame, that version's
    /// root)`. Climbs the O-parent sets, memoizing each node's answer, so the
    /// leaves of one query share the climb above their common ancestors, as
    /// Gold's northward walk shares its crum cache.
    pub fn reach(&self, ck: Ck, memo: &mut HashMap<Ck, Vec<(Holder, i64, Ck)>>) -> Vec<(Holder, i64, Ck)> {
        if let Some(v) = memo.get(&ck) {
            return v.clone();
        }
        let mut out: Vec<(Holder, i64, Ck)> = self.holders_of(&ck).into_iter().map(|h| (h, 0, ck)).collect();
        for (p, d) in self.parents_of(&ck) {
            out.extend(self.reach(p, memo).into_iter().map(|(h, o, r)| (h, o.wrapping_add(d), r)));
        }
        out.sort_unstable();
        out.dedup();
        memo.insert(ck, out.clone());
        out
    }

    /// **The offsets of `ck` in the tree rooted at `target`**, by climbing
    /// from `ck` towards `target` through parents `admit` accepts. Memoized
    /// per node across calls, as one compare asks it of many nodes.
    pub fn offsets_in(
        &self,
        ck: Ck,
        target: &Ck,
        admit: &dyn Fn(&Ck) -> bool,
        memo: &mut HashMap<Ck, Vec<i64>>,
    ) -> Vec<i64> {
        if let Some(v) = memo.get(&ck) {
            return v.clone();
        }
        let mut out = Vec::new();
        if ck == *target {
            out.push(0);
        }
        for (p, d) in self.parents_of(&ck) {
            // A parent already resolved was admitted; only a new one is tested.
            if memo.contains_key(&p) || admit(&p) {
                out.extend(self.offsets_in(p, target, admit, memo).into_iter().map(|o| o.wrapping_add(d)));
            }
        }
        out.sort_unstable();
        out.dedup();
        memo.insert(ck, out.clone());
        out
    }

    /// Entries in the index, for measuring what it costs.
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.parents.len(), self.holders.len(), self.born.len())
    }

    /// The index's trees, for the root record.
    pub fn trees(&self) -> (&Parents, &Holders, &Born, &Cursor) {
        (&self.parents, &self.holders, &self.born, &self.cursor)
    }

    pub fn from_trees(parents: Parents, holders: Holders, born: Born, cursor: Cursor) -> History {
        History { parents, holders, born, cursor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grmpl_core::{Tuple, Value};

    type T = Tree<Tuple, i64, Count>;

    fn t(n: i64) -> Tuple {
        Tuple::from([Value::Int(n)])
    }

    /// A version's root can be an inner node of another version: a small
    /// relation's one leaf, later shared under a bigger root. Backfollow must
    /// find both versions, not only the roots at the top of the climb.
    #[test]
    fn a_root_that_is_also_an_inner_node_is_found_as_both() {
        let leaf = T::leaf_of(vec![(t(1), 1), (t(2), 1)]);
        let other = T::leaf_of(vec![(t(5), 1)]);
        let parent = T::internal_of(vec![t(5)], vec![leaf.clone(), other]);
        let dag = Dag::new();
        let mut h = History::default();
        h.index_version(&dag, Holder { branch: 0, rel: 1, edition: 1 }, &leaf);
        h.index_version(&dag, Holder { branch: 0, rel: 1, edition: 2 }, &parent);
        let ck = content_key(&leaf).unwrap();
        let found: Vec<u64> = h.reach(ck, &mut HashMap::new()).into_iter().map(|(h, _, _)| h.edition).collect();
        assert_eq!(found, vec![1, 2]);
    }
}
