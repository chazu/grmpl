//! **The branch DAG — Xanadu's `DagWood` / the Ent's `fulltrace` branch structure.**
//!
//! The Edition enfilade ([`crate::store`]) records the *linear* commit history
//! *within* one branch: editions `1, 2, 3, …` in submit order. The **DagWood**
//! records the history *between* branches: every branch remembers its parent and
//! the exact edition it forked from. Together — the linear enfilade per branch
//! plus this branch graph — they are the Ent's **fulltrace**, the full versioned
//! provenance of the world.
//!
//! A version *point* is a pair `(branch, edition)`. A [`fork`](Dag::fork) grafts a
//! new branch onto its parent at a fork point; the child then evolves its own
//! linear editions from there. The DagWood answers the provenance questions the
//! linear enfilade cannot on its own:
//!
//! * [`is_ancestor`](Dag::is_ancestor) — does point *a* lie on the history that
//!   flows into point *b*? (the backfollow reachability relation);
//! * [`common_ancestor`](Dag::common_ancestor) — the latest point shared by two
//!   divergent branches (the merge base / where two histories parted).
//!
//! The DAG is deterministic — branch ids are allocated by a monotone counter, no
//! clock or randomness — so it is Replay-safe like every other enfilade.

use std::collections::BTreeMap;

/// A branch identity. The root world is [`Dag::ROOT`] (`0`); each fork allocates
/// the next id.
pub type BranchId = u64;

/// One branch's provenance: its id and, unless it is the root, the point it was
/// forked from — `(parent branch, edition on the parent at the fork)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Branch {
    pub id: BranchId,
    /// `None` for the root; `Some((parent, at))` for a fork.
    pub parent: Option<(BranchId, u64)>,
}

/// The branch DAG. A monotone registry of branches, each pointing at its parent
/// fork point — a tree of branches (each has ≤1 parent), the linear per-branch
/// enfilades hanging off it forming the whole fulltrace.
#[derive(Clone, Debug, Default)]
pub struct Dag {
    branches: BTreeMap<BranchId, Branch>,
    next: BranchId,
}

impl Dag {
    /// The root branch — the world's original, parentless line.
    pub const ROOT: BranchId = 0;

    /// A fresh DAG with just the root branch.
    pub fn new() -> Dag {
        let mut branches = BTreeMap::new();
        branches.insert(Self::ROOT, Branch { id: Self::ROOT, parent: None });
        Dag { branches, next: 1 }
    }

    /// Register a new branch forked from `parent` at edition `at`, returning its
    /// fresh id. Panics if `parent` is unknown (a fork off a branch this DAG never
    /// minted is a programming error, not a runtime condition).
    pub fn fork(&mut self, parent: BranchId, at: u64) -> BranchId {
        assert!(self.branches.contains_key(&parent), "fork off unknown branch {parent}");
        let id = self.next;
        self.next += 1;
        self.branches.insert(id, Branch { id, parent: Some((parent, at)) });
        id
    }

    /// The branch record, if known.
    pub fn get(&self, b: BranchId) -> Option<Branch> {
        self.branches.get(&b).copied()
    }

    /// This branch's fork point — `(parent, at)` — or `None` at the root.
    pub fn parent(&self, b: BranchId) -> Option<(BranchId, u64)> {
        self.branches.get(&b).and_then(|br| br.parent)
    }

    /// The lineage of point `(b, at)`: the chain of `(branch, edition)` segments
    /// from the point back to the root, newest first. Each entry is the *highest*
    /// edition of that branch that flows into the point — `at` on `b` itself, then
    /// each fork edition walking up. This is the exact set of history the point
    /// descends from, one entry per branch.
    pub fn lineage(&self, b: BranchId, at: u64) -> Vec<(BranchId, u64)> {
        let mut out = Vec::new();
        let mut cur = Some((b, at));
        while let Some((br, ed)) = cur {
            out.push((br, ed));
            cur = self.parent(br);
        }
        out
    }

    /// Does point `(ba, ea)` lie on the history that flows into point `(bb, eb)`?
    ///
    /// True iff, walking `bb`'s lineage to the root, we pass through branch `ba`
    /// at an edition `≥ ea` — i.e. `ea` had already happened on `ba` by the time
    /// `bb`'s history left `ba`. Reflexive (a point is its own ancestor) and
    /// transitive; the reachability relation of the fulltrace.
    pub fn is_ancestor(&self, ba: BranchId, ea: u64, bb: BranchId, eb: u64) -> bool {
        for (br, ed) in self.lineage(bb, eb) {
            if br == ba {
                return ea <= ed;
            }
        }
        false
    }

    /// Encode the whole branch graph: `count || [id, has_parent, parent, at]*`.
    /// The DagWood is the fulltrace's branch structure, so it is durable like
    /// every other part of the world — a reopened store that forgot its forks
    /// would have forgotten its history.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.branches.len() as u32).to_be_bytes());
        for b in self.branches.values() {
            out.extend_from_slice(&b.id.to_be_bytes());
            match b.parent {
                None => out.push(0),
                Some((p, at)) => {
                    out.push(1);
                    out.extend_from_slice(&p.to_be_bytes());
                    out.extend_from_slice(&at.to_be_bytes());
                }
            }
        }
        out
    }

    /// Decode a graph written by [`encode`](Self::encode). An empty or absent
    /// blob is a fresh DAG holding only the root branch.
    pub fn decode(bytes: &[u8]) -> Option<Dag> {
        if bytes.is_empty() {
            return Some(Dag::new());
        }
        let n = u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?) as usize;
        let mut branches = BTreeMap::new();
        let mut next = 0u64;
        let mut pos = 4;
        for _ in 0..n {
            let id = u64::from_be_bytes(bytes.get(pos..pos + 8)?.try_into().ok()?);
            pos += 8;
            let parent = match bytes.get(pos)? {
                0 => {
                    pos += 1;
                    None
                }
                _ => {
                    let p = u64::from_be_bytes(bytes.get(pos + 1..pos + 9)?.try_into().ok()?);
                    let at = u64::from_be_bytes(bytes.get(pos + 9..pos + 17)?.try_into().ok()?);
                    pos += 17;
                    Some((p, at))
                }
            };
            branches.insert(id, Branch { id, parent });
            next = next.max(id + 1);
        }
        Some(Dag { branches, next })
    }

    /// The latest point shared by `(ba, ea)` and `(bb, eb)` — their merge base:
    /// the branch nearest both where their histories last coincided, at the lesser
    /// of the two editions there. `None` only if the two points are in disjoint
    /// DAGs (never, for branches minted by the same [`Dag`], since all descend
    /// from [`ROOT`](Dag::ROOT)).
    pub fn common_ancestor(&self, ba: BranchId, ea: u64, bb: BranchId, eb: u64) -> Option<(BranchId, u64)> {
        // Map each branch in a's lineage to the edition a's history holds there.
        let a_line: BTreeMap<BranchId, u64> = self.lineage(ba, ea).into_iter().collect();
        // Walk b's lineage newest-first; the first branch also in a's lineage is
        // the nearest shared branch — the join is at the min edition of the two.
        for (br, eb_here) in self.lineage(bb, eb) {
            if let Some(&ea_here) = a_line.get(&br) {
                return Some((br, ea_here.min(eb_here)));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small fixture DAG (fork() allocates 1, 2, 3 in call order):
    //   root(0) --- fork@5 --> a(1) --- fork@3 --> c(3)
    //          \--- fork@8 --> b(2)
    fn fixture() -> Dag {
        let mut d = Dag::new();
        let a = d.fork(Dag::ROOT, 5);
        assert_eq!(a, 1);
        let b = d.fork(Dag::ROOT, 8);
        assert_eq!(b, 2);
        let c = d.fork(a, 3);
        assert_eq!(c, 3);
        d
    }

    #[test]
    fn lineage_walks_to_root() {
        let d = fixture();
        // c(3) at edition 4: c@4, then a at its fork edition 3, then root at 5.
        assert_eq!(d.lineage(3, 4), vec![(3, 4), (1, 3), (0, 5)]);
        // root at edition 10 is just itself.
        assert_eq!(d.lineage(Dag::ROOT, 10), vec![(0, 10)]);
    }

    #[test]
    fn ancestry_is_reflexive_and_follows_forks() {
        let d = fixture();
        // Reflexive.
        assert!(d.is_ancestor(3, 4, 3, 4));
        // Earlier edition on the same branch is an ancestor; later is not.
        assert!(d.is_ancestor(3, 2, 3, 4));
        assert!(!d.is_ancestor(3, 6, 3, 4));
        // root@5 flows into c: root -> a(fork@5) -> c. Yes, since a forked at 5 ≥ 5.
        assert!(d.is_ancestor(Dag::ROOT, 5, 3, 100));
        // root@6 does NOT flow into a (a forked at edition 5, so it never saw 6).
        assert!(!d.is_ancestor(Dag::ROOT, 6, 1, 100));
        // a and b are cousins: neither is the other's ancestor.
        assert!(!d.is_ancestor(1, 1, 2, 100));
        assert!(!d.is_ancestor(2, 1, 1, 100));
    }

    #[test]
    fn common_ancestor_is_the_merge_base() {
        let d = fixture();
        // a(1) and b(2) both fork from root; a@5, b@8 → shared at root, min(5,8)=5.
        assert_eq!(d.common_ancestor(1, 100, 2, 100), Some((Dag::ROOT, 5)));
        // c(3) descends from a(1): merge base is a at min(fork-edition 3, a's ed).
        assert_eq!(d.common_ancestor(3, 100, 1, 4), Some((1, 3)));
        assert_eq!(d.common_ancestor(3, 100, 1, 2), Some((1, 2)));
        // c(3) vs b(2): nearest shared is root, at min(5, 8) = 5.
        assert_eq!(d.common_ancestor(3, 100, 2, 100), Some((Dag::ROOT, 5)));
        // A point with itself.
        assert_eq!(d.common_ancestor(3, 7, 3, 4), Some((3, 4)));
    }
}
