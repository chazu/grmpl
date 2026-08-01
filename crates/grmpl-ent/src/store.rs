//! **The ent-native store (E1, Edition + Fact enfilades).**
//!
//! The realization of "the Ent *is* the store" (plan §1), built from the [`Tree`]
//! enfilade primitive over two coordinated enfilades per relation:
//!
//! * the **Fact enfilade** — net-per-tuple state, tuple-keyed, *versioned by
//!   edition* (one persistent root per edition, structurally shared), so
//!   `read_at(rel, at)` is a root lookup + in-order walk (MVCC by root);
//! * the **Edition enfilade** — the raw commit-order delta log, keyed by
//!   `(edition, submit_index)` with the submit index as immutable payload, so
//!   `scan_updates` returns raw, per-multiplicity updates in exact submit order.
//!
//! [`EntStore::open`] backs the store with a [`Granfilade`]: on each commit the
//! touched Edition enfilades are persisted as content-addressed nodes (structural
//! sharing across editions) with the roots + clock in one atomic write; on open
//! the state is rebuilt from the persisted enfilades. The **persisted form is the
//! enfilade itself, never a log** — the Ent is the substrate. [`EntStore::new`]
//! is a pure in-memory store (used by the conformance oracle).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use grmpl_core::{
    wire, Catalog, Diff, Edition, EditionStore, Entity, Error, RelId, Result, Schema,
    SchemaCatalog, Time, TraceStore, Tuple, Update, Value,
};

use crate::context::{self, ContextEnf};
use crate::dag::{BranchId, Dag};
use crate::dsp::{Dsp, DspEnf};
use crate::granfilade::{ContentKey, Granfilade};
use crate::measure::{Count, SumDiff};
use crate::tree::Tree;

/// The Fact enfilade: `tuple → net Σdiff` (nonzero only).
///
/// It carries **two** upward measures (plan v5 §G-3): the entry [`Count`], and
/// the [`SumDiff`] of the weights beneath each node. A product of monoids is a
/// monoid, so this needs no tree machinery — and it means "how many rows" and
/// "what is their total weight" over any key span are both `O(log n)` folds of
/// cached summaries, materializing nothing.
type FactMeasure = (Count, SumDiff);
type FactTree = Tree<Tuple, Diff, FactMeasure>;
/// The Edition enfilade: `(edition, submit_index) → (tuple, diff)` raw log.
type LogTree = Tree<(u64, u64), (Tuple, Diff), Count>;
/// The **Version enfilade**: `edition → Fact root`, one persistent root per
/// live as-of edition (G-2a). `last_le` makes an as-of read a descent.
type VersionTree = Tree<u64, FactTree, Count>;
/// The **Rel enfilade**: the directory of live relations (G-2a).
type RelTree = Tree<u32, RelRoots, Count>;

/// One relation's roots: its versioned Fact enfilade, its Edition log, and any
/// **Arrangements** — alternate orderings of the same facts (G-9).
#[derive(Clone, Default)]
struct RelRoots {
    versions: VersionTree,
    log: LogTree,
    /// `lead column → the same facts keyed by that column first`.
    ///
    /// The primary order prunes on the lead column only, so a predicate on any
    /// other column has to scan. An Arrangement is one more measured tree over
    /// the same facts, rotated so the column of interest leads — after which
    /// pruning on it is the ordinary WID range walk. Built on first use and
    /// maintained with every commit, so the cost is paid only for columns some
    /// query actually asks about.
    orders: BTreeMap<usize, FactTree>,
}

/// Rotate `tuple` so column `col` leads, the rest following in order — or `None`
/// if the tuple has no such column, in which case it simply does not belong to
/// that Arrangement. (Returning it unrotated would be worse than useless: it
/// would sit in the tree keyed by its *column 0* and match spans meant for
/// `col`.) Invertible, and it preserves distinctness, so an Arrangement holds
/// exactly the facts of the primary that have the column.
fn rotate(tuple: &Tuple, col: usize) -> Option<Tuple> {
    let cols = tuple.as_slice();
    if col >= cols.len() {
        return None;
    }
    if col == 0 {
        return Some(tuple.clone());
    }
    let mut out = Vec::with_capacity(cols.len());
    out.push(cols[col].clone());
    out.extend(cols[..col].iter().cloned());
    out.extend(cols[col + 1..].iter().cloned());
    Some(Tuple::new(out))
}

/// Undo [`rotate`].
fn unrotate(tuple: &Tuple, col: usize) -> Tuple {
    let cols = tuple.as_slice();
    if col == 0 || col >= cols.len() {
        return tuple.clone();
    }
    let mut out: Vec<Value> = cols[1..=col].to_vec();
    out.push(cols[0].clone());
    out.extend(cols[col + 1..].iter().cloned());
    Tuple::new(out)
}

/// An ent store: a family of per-relation Fact + Edition enfilades behind one
/// commit clock, optionally durable on a [`Granfilade`].
pub struct EntStore {
    inner: Mutex<Inner>,
    /// The node substrate, **shared with every fork of this store** (G-6): all
    /// branches live in one granfilade with their roots namespaced by branch, so
    /// a durable fork shares nodes with its ancestor instead of copying them.
    gran: Option<Arc<Granfilade>>,
    /// This store's branch in the fulltrace's DagWood.
    branch: BranchId,
    /// The branch DAG shared with every fork of this store (Xanadu's `DagWood` /
    /// fulltrace branch structure) — see [`crate::dag`].
    dag: Arc<Mutex<Dag>>,
}

struct Inner {
    current: u64,
    watermark: u64,
    /// **The Rel enfilade (G-2a).** The directory of live relations, each
    /// holding its Version enfilade and its Edition log.
    ///
    /// This used to be `HashMap<RelId, BTreeMap<u64, FactTree>>` beside
    /// `HashMap<RelId, LogTree>` — the "family of enfilades" held together by
    /// std maps, with the relation directory in unordered iteration order (the
    /// one thing the Determinism invariant warns about). Now the store's whole
    /// state is one root, ordered all the way down, and "how many relations" or
    /// "how many live editions" are measures.
    rels: RelTree,
    /// Context enfilade: inherited scope bindings, plus the durable catalog and
    /// the edition-versioned schema registry ([`crate::context`]).
    ctx: ContextEnf,
}

impl Inner {
    fn roots(&self, rel: RelId) -> Option<&RelRoots> {
        self.rels.get(&rel.0)
    }

    /// The Fact root in force at `at` — an `O(log n)` descent of the Version
    /// enfilade, not a scan of the versions below it.
    fn fact_at(&self, rel: RelId, at: u64) -> Option<&FactTree> {
        self.roots(rel).and_then(|r| r.versions.last_le(&at)).map(|(_, t)| t)
    }

    fn log_of(&self, rel: RelId) -> Option<&LogTree> {
        self.roots(rel).map(|r| &r.log)
    }

    /// Replace `rel`'s roots, creating the entry if it is new.
    fn put(&mut self, rel: RelId, roots: RelRoots) {
        self.rels = self.rels.insert(rel.0, roots);
    }

    /// Every live relation, in id order — deterministic, unlike a `HashMap`.
    fn rel_ids(&self) -> Vec<RelId> {
        self.rels.iter().map(|(r, _)| RelId(*r)).collect()
    }
}

impl Default for EntStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EntStore {
    /// A pure in-memory ent store (no durability).
    pub fn new() -> EntStore {
        EntStore {
            inner: Mutex::new(Inner::empty()),
            gran: None,
            branch: Dag::ROOT,
            dag: Arc::new(Mutex::new(Dag::new())),
        }
    }

    /// Open (or create) a durable ent store on a granfilade at `path`, rebuilding
    /// its state from the persisted enfilades.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<EntStore> {
        let gran = Granfilade::open(path)?;
        let inner = Inner::rebuild(&gran, Dag::ROOT)?;
        // The branch graph is durable too: a reopened store that forgot its
        // forks would have forgotten its history.
        let dag = gran
            .meta_get(DAG_KEY)?
            .and_then(|b| Dag::decode(&b))
            .unwrap_or_else(Dag::new);
        Ok(EntStore {
            inner: Mutex::new(inner),
            gran: Some(Arc::new(gran)),
            branch: Dag::ROOT,
            dag: Arc::new(Mutex::new(dag)),
        })
    }

    /// **WID range read (E2).** The rows of `rel` whose tuple key lies in
    /// `[lo, hi)`, as-of `at` — an `O(result + log n)` walk of the Fact enfilade
    /// that prunes whole out-of-range subtrees, where the LSM must scan the
    /// relation. (Lead-prefix pruning on the primary order; per-column
    /// Arrangements for trailing-column spans are the next increment.)
    pub fn range_at(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("range_at", at.0, inner.watermark));
        }
        Ok(inner.fact_at(rel, at.0).map(|t| t.range_collect(lo, hi)).unwrap_or_default())
    }

    /// **WID measure (E2).** How many live tuples of `rel` lie in `[lo, hi)`
    /// as-of `at` — answered in `O(log n)` from the cached subtree measures,
    /// without materializing the rows (Xanadu wid pruning).
    pub fn count_at(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<u64> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("count_at", at.0, inner.watermark));
        }
        Ok(inner.fact_at(rel, at.0).map(|t| t.measure_range(lo, hi).0 .0).unwrap_or(0))
    }

    /// **WID weight measure (G-3).** The total net weight of `rel`'s tuples in
    /// `[lo, hi)` as-of `at` — folded from cached subtree summaries in
    /// `O(log n)`, without materializing a single row. Where `count_at` answers
    /// "how many", this answers "how much": the aggregate reads the tree's shape
    /// rather than its contents.
    pub fn weight_at(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<i64> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("weight_at", at.0, inner.watermark));
        }
        Ok(inner.fact_at(rel, at.0).map(|t| t.measure_range(lo, hi).1 .0).unwrap_or(0))
    }

    /// **Arrangements (G-9).** Ensure `rel` has an ordering led by column `col`,
    /// building it from the current primary order if this is its first use.
    fn ensure_order(inner: &mut Inner, rel: RelId, col: usize) {
        let Some(roots) = inner.roots(rel) else { return };
        if roots.orders.contains_key(&col) {
            return;
        }
        let mut roots = roots.clone();
        let mut arr = FactTree::new();
        if let Some(primary) = roots.versions.last_le(&inner.current) {
            for (k, v) in primary.1.iter() {
                if let Some(key) = rotate(k, col) {
                    arr = arr.insert(key, *v);
                }
            }
        }
        roots.orders.insert(col, arr);
        inner.put(rel, roots);
    }

    /// **Version-compare / backfollow (E6).** How `rel` differs between editions
    /// `a` and `b`, as `(tuple, weight_at_a, weight_at_b)` for every tuple whose
    /// net weight differs. An unchanged relation shares its Fact root across the
    /// two editions, so the comparison short-circuits in `O(1)` — the read side of
    /// the trace ("what is the same, what moved").
    pub fn compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<crate::tree::EntryDiff<Tuple, Diff>>> {
        let inner = self.inner.lock().unwrap();
        for at in [a, b] {
            if at.0 < inner.watermark {
                return Err(door("compare", at.0, inner.watermark));
            }
        }
        let root_at = |ed: u64| inner.fact_at(rel, ed).cloned().unwrap_or_default();
        Ok(root_at(a.0).diff(&root_at(b.0)))
    }

    /// Node frames serialized+hashed since this store was opened — the G-0a ops
    /// counter, surfaced so tests can assert the commit path stays path-sized.
    /// `0` for an in-memory store.
    pub fn frames_encoded(&self) -> u64 {
        self.gran.as_ref().map_or(0, |g| g.frames_encoded())
    }

    /// **Reachability GC (E3).** Collect granfilade nodes no longer reachable
    /// from a live enfilade root (accumulated as commits path-copy and
    /// `consolidate` truncates). Serialized with commits (holds the edition
    /// lock). A no-op in-memory. Returns the number of nodes collected.
    pub fn gc(&self) -> Result<usize> {
        let _guard = self.inner.lock().unwrap();
        match &self.gran {
            Some(g) => g.gc(),
            None => Ok(0),
        }
    }

    /// **Structural-sharing fork (E3, made durable in G-6).** A new independent
    /// store whose state is this store's as-of `at`, **sharing every enfilade
    /// node** with the parent — the versioned Fact roots are `Arc`-cloned in
    /// memory and, on a durable store, the child's roots are written into the
    /// *same granfilade* pointing at the *same node keys*. Forking is therefore
    /// `O(#roots)` meta writes and **zero** node writes: the cheap virtual copy
    /// at the heart of the Ent, where the LSM must copy `O(state)` bytes.
    ///
    /// The fork is a new branch in the shared DagWood, so ancestry stays
    /// queryable across the whole family, and it survives a reopen
    /// ([`open_branch`](Self::open_branch)).
    pub fn fork_at(&self, at: Edition) -> Result<EntStore> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("fork_at", at.0, inner.watermark));
        }
        // Graft a new branch onto this store's branch at the fork edition in the
        // shared DagWood; the child carries that id and the same registry, so
        // ancestry is queryable across the whole fork family (the fulltrace).
        let child_branch = self.dag.lock().unwrap().fork(self.branch, at.0);
        // Keep each relation's versioned roots up to `at` (their trees are
        // shared Arcs), and its log — whole when forking at current, else the
        // prefix ≤ `at`.
        let mut rels = RelTree::new();
        for (rel, roots) in inner.rels.iter() {
            let mut versions = VersionTree::new();
            for (e, t) in roots.versions.range_collect(&0, &(at.0 + 1)) {
                versions = versions.insert(e, t);
            }
            let log = if at.0 >= inner.current {
                roots.log.clone()
            } else {
                let mut t = LogTree::new();
                for (k, v) in roots.log.range_collect(&(0, 0), &(at.0 + 1, 0)) {
                    t = t.insert(k, v);
                }
                t
            };
            if !versions.is_empty() || !log.is_empty() {
                // Arrangements are derived from the primary order, so a fork
                // rebuilds them on demand rather than carrying them.
                rels = rels.insert(*rel, RelRoots { versions, log, orders: BTreeMap::new() });
            }
        }
        let child = EntStore {
            inner: Mutex::new(Inner {
                current: at.0,
                watermark: inner.watermark,
                rels,
                ctx: inner.ctx.clone(),
            }),
            gran: self.gran.clone(),
            branch: child_branch,
            dag: Arc::clone(&self.dag),
        };
        // Write the child's roots and its branch record. Every node they name is
        // already durable and memoized, so `collect_tree` returns their keys
        // without re-encoding anything: the fork writes roots, never nodes.
        if let Some(gran) = &self.gran {
            let child_inner = child.inner.lock().unwrap();
            let rels: Vec<RelId> = child_inner.rel_ids();
            child.persist(&child_inner, &rels, None)?;
            child.persist_ctx(&child_inner)?;
            gran.write(
                Vec::new(),
                vec![(DAG_KEY.to_vec(), self.dag.lock().unwrap().encode())],
            )?;
        }
        Ok(child)
    }

    /// A handle on another **branch of this same store**, sharing its granfilade
    /// and DagWood.
    ///
    /// Prefer this over [`open_branch`](Self::open_branch) whenever the parent is
    /// still live: a granfilade takes an exclusive lock on its directory, so two
    /// branches of one world are two handles onto *one* granfilade, never two
    /// opens of the same path. That is the same constraint that makes all
    /// branches sharing one node store the right design in the first place.
    pub fn branch(&self, branch: BranchId) -> Result<EntStore> {
        if self.dag.lock().unwrap().get(branch).is_none() {
            return Err(Error::Store(format!("unknown branch {branch}")));
        }
        let inner = match &self.gran {
            Some(gran) => Inner::rebuild(gran, branch)?,
            None => return Err(Error::Store("in-memory store has no persisted branches".into())),
        };
        Ok(EntStore {
            inner: Mutex::new(inner),
            gran: self.gran.clone(),
            branch,
            dag: Arc::clone(&self.dag),
        })
    }

    /// Reopen a specific branch of a granfilade — the durable counterpart of
    /// [`fork_at`](Self::fork_at). [`open`](Self::open) is this at
    /// [`Dag::ROOT`]. The parent must not be open: use
    /// [`branch`](Self::branch) when it is.
    pub fn open_branch(path: impl AsRef<std::path::Path>, branch: BranchId) -> Result<EntStore> {
        let gran = Granfilade::open(path)?;
        let inner = Inner::rebuild(&gran, branch)?;
        let dag = gran
            .meta_get(DAG_KEY)?
            .and_then(|b| Dag::decode(&b))
            .unwrap_or_else(Dag::new);
        Ok(EntStore {
            inner: Mutex::new(inner),
            gran: Some(Arc::new(gran)),
            branch,
            dag: Arc::new(Mutex::new(dag)),
        })
    }

    /// This store's branch in the fulltrace's DagWood ([`Dag::ROOT`] unless it is
    /// a fork).
    pub fn branch_id(&self) -> BranchId {
        self.branch
    }

    /// A snapshot of the branch DAG shared with this store's fork family — the
    /// fulltrace's branch structure (Xanadu's `DagWood`).
    pub fn dag(&self) -> Dag {
        self.dag.lock().unwrap().clone()
    }

    /// **Backfollow across branches (E3).** Does this store's current point
    /// descend from `ancestor`'s point as-of `ancestor_at` — i.e. did that history
    /// flow into this branch? Forks share the DagWood, so this answers across the
    /// whole family; two stores that never shared a fork return `false` (disjoint
    /// DagWoods). Reflexive on the same branch (earlier editions are ancestors).
    pub fn descends_from(&self, ancestor: &EntStore, ancestor_at: Edition) -> bool {
        if !Arc::ptr_eq(&self.dag, &ancestor.dag) {
            return false;
        }
        let here = self.inner.lock().unwrap().current;
        self.dag
            .lock()
            .unwrap()
            .is_ancestor(ancestor.branch, ancestor_at.0, self.branch, here)
    }

    /// The merge base of this store and `other` in the shared DagWood — the latest
    /// `(branch, edition)` their histories both descend from, or `None` if they
    /// belong to disjoint DagWoods.
    pub fn common_ancestor_with(&self, other: &EntStore) -> Option<(BranchId, u64)> {
        if !Arc::ptr_eq(&self.dag, &other.dag) {
            return None;
        }
        let here = self.inner.lock().unwrap().current;
        let there = other.inner.lock().unwrap().current;
        self.dag
            .lock()
            .unwrap()
            .common_ancestor(self.branch, here, other.branch, there)
    }

    /// **DSP template instancing (E6).** Read every fact of `rels` whose lead
    /// entity lies in the template block `[block_lo, block_hi)` — a WID range read
    /// (E2) over each relation — relocate **all** of its entity coordinates by
    /// `shift` ([`Dsp::apply_all`]), and commit the relocated facts as one new
    /// edition: a private, independently-mutable copy of the template sub-world,
    /// its rooms/exits/items renamed only in *coordinate* (their text and weights
    /// preserved). Returns the new edition.
    ///
    /// Distinct `shift`s give disjoint instances that never collide, so N players
    /// can each `enter` the same template into their own block. Building an
    /// instance is `O(template facts)` — a self-contained vault is a handful of
    /// facts, so it is effectively instant; an `O(1)` displaced-overlay that shares
    /// the template until an instance diverges (true copy-on-write) is the next
    /// increment on this same DSP relocation.
    ///
    /// Precondition: the template is self-contained — every template fact is keyed
    /// by an in-block lead entity (so the lead-column WID range collects it), and
    /// all of a fact's entity columns lie in the block (so the relocation keeps it
    /// internally connected). The target block `[block_lo+shift, block_hi+shift)`
    /// must be otherwise unused.
    pub fn instance_template(&self, rels: &[RelId], block_lo: u64, block_hi: u64, shift: i64) -> Result<Edition> {
        let at = self.current();
        let dsp = Dsp::by(shift);
        // The *target* block: the instance's own coordinates.
        let lo = Tuple::from([Value::Ent(Entity(block_lo.wrapping_add(shift as u64)))]);
        let hi = Tuple::from([Value::Ent(Entity(block_hi.wrapping_add(shift as u64)))]);
        let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        {
            let inner = self.inner.lock().unwrap();
            if at.0 < inner.watermark {
                return Err(door("instance_template", at.0, inner.watermark));
            }
            for &rel in rels {
                let Some(facts) = inner.fact_at(rel, at.0) else { continue };
                // **The DSP overlay (E6/G-7).** Relocating the relation is `O(1)`
                // and shares every node — no copy is made here. The instance's
                // rows are then read *out of the displaced view*, which
                // transforms the query back into the shared tree's coordinates
                // and prunes there (the `DspLoaf` discipline), rather than
                // materializing the template and mapping over it.
                let moved = DspEnf::relocate(facts.clone(), dsp);
                for (tuple, diff) in moved.range_all(&lo, &hi) {
                    updates.push((rel, tuple, diff));
                }
            }
        }
        self.commit(&updates)
    }

    /// Persist the context enfilade (catalog, schemas, scoped bindings) as one
    /// atomic granfilade write. A no-op for an in-memory store.
    ///
    /// Kept separate from [`persist`](Self::persist) because context is written
    /// on its own occasions — `register`, `put_schema` — not on the commit path,
    /// and it is exempt from consolidation: the catalog is append-only for the
    /// life of the world.
    fn persist_ctx(&self, inner: &Inner) -> Result<()> {
        let gran = match &self.gran {
            Some(g) => g,
            None => return Ok(()),
        };
        let (ck, nodes) = gran.collect_tree(&inner.ctx);
        gran.write(nodes, vec![(ctx_key(self.branch), opt_ck_bytes(ck))])
    }

    /// Persist the clock plus the touched relations' Edition enfilades (roots +
    /// nodes) in one atomic granfilade write. A no-op for an in-memory store.
    fn persist(&self, inner: &Inner, touched: &[RelId], drop_below: Option<u64>) -> Result<()> {
        let gran = match &self.gran {
            Some(g) => g,
            None => return Ok(()),
        };
        let mut nodes = Vec::new();
        let mut meta = vec![
            (cur_key(self.branch), inner.current.to_be_bytes().to_vec()),
            (wm_key(self.branch), inner.watermark.to_be_bytes().to_vec()),
        ];
        let mut drop_meta = Vec::new();
        for rel in touched {
            // The Edition enfilade (the raw commit-order log).
            let log = inner.log_of(*rel).cloned().unwrap_or_default();
            let (ck, ns) = gran.collect_tree(&log);
            nodes.extend(ns);
            meta.push((log_key(self.branch, *rel), opt_ck_bytes(ck)));

            // **The Fact enfilade, versioned by edition (G-2).** Every live
            // as-of root is persisted, so `open` is a root lookup rather than a
            // replay of the log — the Fact enfilade is durable state in its own
            // right, not an index derived from a log underneath it.
            if let Some(roots) = inner.roots(*rel) {
                for (edition, tree) in roots.versions.iter() {
                    let (ck, ns) = gran.collect_tree(tree);
                    nodes.extend(ns);
                    meta.push((fact_key(self.branch, *rel, *edition), opt_ck_bytes(ck)));
                }
            }
        }
        // Consolidation retires every Fact root below the new watermark, in the
        // same batch as the checkpoint that replaces them: a crash leaves the
        // old horizon or the new one, never a half-cut history.
        if let Some(wm) = drop_below {
            for (key, _) in gran.meta_prefix(&branch_key(b"fact:", self.branch))? {
                if fact_edition(&key).is_some_and(|e| e < wm) {
                    drop_meta.push(key);
                }
            }
        }
        gran.write_full(nodes, meta, drop_meta)
    }
}

impl Inner {
    fn empty() -> Inner {
        Inner { current: 0, watermark: 0, rels: RelTree::new(), ctx: ContextEnf::new() }
    }

    /// Rebuild in-memory state from a granfilade: the clock, then each
    /// relation's persisted Edition-log root and its **versioned Fact roots**.
    ///
    /// This is a root lookup per relation-version — no fold, no replay. Before
    /// G-2 the Fact enfilade was not persisted at all: `open` loaded a
    /// watermark checkpoint and replayed the whole log tail back through
    /// `fold_fact`, which is checkpoint-and-replay recovery — the LSM shape the
    /// mandate rules out from underneath the Ent.
    fn rebuild(gran: &Granfilade, branch: BranchId) -> Result<Inner> {
        let current = gran.meta_get(&cur_key(branch))?.map(|b| u64_be(&b)).unwrap_or(0);
        let watermark = gran.meta_get(&wm_key(branch))?.map(|b| u64_be(&b)).unwrap_or(0);
        let ctx: ContextEnf = gran.load(bytes_opt_ck(&gran.meta_get(&ctx_key(branch))?.unwrap_or_default()))?;
        let mut inner = Inner { current, watermark, rels: RelTree::new(), ctx };

        for (key, val) in gran.meta_prefix(&branch_key(b"fact:", branch))? {
            let rel = rel_from_key(&key, b"fact:");
            let edition = fact_edition(&key)
                .ok_or_else(|| Error::Codec("granfilade: malformed fact root key".into()))?;
            let tree: FactTree = gran.load(bytes_opt_ck(&val))?;
            let mut roots = inner.roots(rel).cloned().unwrap_or_default();
            roots.versions = roots.versions.insert(edition, tree);
            inner.put(rel, roots);
        }
        for (key, val) in gran.meta_prefix(&branch_key(b"log:", branch))? {
            let rel = rel_from_key(&key, b"log:");
            let mut roots = inner.roots(rel).cloned().unwrap_or_default();
            roots.log = gran.load(bytes_opt_ck(&val))?;
            inner.put(rel, roots);
        }
        Ok(inner)
    }

    /// Fold one update into the Fact enfilade at edition `e`: build a fresh root
    /// from the latest root ≤ `e`, netting the tuple's weight; drop it at 0.
    fn fold_fact(&mut self, e: u64, rel: RelId, tuple: &Tuple, diff: Diff) {
        let mut roots = self.roots(rel).cloned().unwrap_or_default();
        let base = roots.versions.last_le(&e).map(|(_, t)| t.clone()).unwrap_or_default();
        let cur = base.get(tuple).copied().unwrap_or(0);
        let net = cur + diff;
        let root = if net == 0 { base.remove(tuple) } else { base.insert(tuple.clone(), net) };
        roots.versions = roots.versions.insert(e, root);
        // Keep every existing Arrangement in step with the primary order.
        for (col, arr) in roots.orders.iter_mut() {
            if let Some(key) = rotate(tuple, *col) {
                *arr = if net == 0 { arr.remove(&key) } else { arr.insert(key, net) };
            }
        }
        self.put(rel, roots);
    }

    /// Apply `updates` as edition `e`: append to each Edition enfilade in submit
    /// order and fold each into the Fact enfilade.
    fn apply(&mut self, e: u64, updates: &[(RelId, Tuple, Diff)]) {
        for (i, (rel, tuple, diff)) in updates.iter().enumerate() {
            let mut roots = self.roots(*rel).cloned().unwrap_or_default();
            roots.log = roots.log.insert((e, i as u64), (tuple.clone(), *diff));
            self.put(*rel, roots);
            self.fold_fact(e, *rel, tuple, *diff);
        }
        self.current = e;
    }

    fn holds_now(&self, rel: RelId, tuple: &Tuple) -> bool {
        self.fact_at(rel, self.current).and_then(|t| t.get(tuple)).is_some_and(|n| *n > 0)
    }
}

impl EditionStore for EntStore {
    fn current(&self) -> Edition {
        Edition(self.inner.lock().unwrap().current)
    }
}

impl TraceStore for EntStore {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        let mut inner = self.inner.lock().unwrap();
        let e = inner.current + 1;
        inner.apply(e, updates);
        self.persist(&inner, &touched(updates), None)?;
        Ok(Edition(e))
    }

    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        let mut inner = self.inner.lock().unwrap();
        for (rel, tuple) in preconditions {
            if !inner.holds_now(*rel, tuple) {
                return Ok(None);
            }
        }
        let e = inner.current + 1;
        inner.apply(e, updates);
        self.persist(&inner, &touched(updates), None)?;
        Ok(Some(Edition(e)))
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("read_at", at.0, inner.watermark));
        }
        Ok(inner
            .fact_at(rel, at.0)
            .map(|t| t.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default())
    }

    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        let inner = self.inner.lock().unwrap();
        if from.0 < inner.watermark {
            return Err(door("scan_updates", from.0, inner.watermark));
        }
        let mut out = Vec::new();
        if let Some(log) = inner.log_of(rel) {
            for ((edition, _submit), (tuple, diff)) in
                log.range_collect(&(from.0 + 1, 0), &(to.0 + 1, 0))
            {
                out.push(Update { tuple, time: Time::input(edition), diff });
            }
        }
        Ok(out)
    }

    /// **WID-pruned range read (E2b).** The Ent's override of the substrate
    /// range-read primitive: instead of the default full-scan-then-filter, walk
    /// the Fact enfilade pruning whole out-of-range subtrees by their cached
    /// measures — `O(result + log n)`. This is the same fast path as
    /// [`EntStore::range_at`], now reachable through the store trait so
    /// `grmpl-diff`'s `RangeRel` operator prunes at the source.
    fn read_range(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.range_at(rel, at, lo, hi)
    }

    /// **Measured interest routing (G-4).** The Edition enfilade is keyed by
    /// `(edition, submit_index)`, so "did anything land in `(from, to]`?" is a
    /// **WID range measure** over that span — `O(log n)` from cached subtree
    /// counts, materializing nothing. The default implementation must answer
    /// `true` for every store; the Ent can answer *no* and prove it, which is
    /// what lets the reactive pump skip a view it cannot have changed.
    fn touched_since(&self, from: Edition, to: Edition, rels: &[RelId]) -> Result<bool> {
        let inner = self.inner.lock().unwrap();
        if from.0 < inner.watermark {
            // Below the door we cannot prove anything; stay conservative and let
            // the caller's own read hit the door with a proper error.
            return Ok(true);
        }
        for rel in rels {
            if let Some(log) = inner.log_of(*rel) {
                if log.measure_range(&(from.0 + 1, 0), &(to.0 + 1, 0)).0 > 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// **Trailing-column WID pruning (G-9).** Answered from the Arrangement led
    /// by `col` — one more measured tree over the same facts, so the predicate
    /// becomes an ordinary lead-column range walk and prunes whole subtrees,
    /// where the default must read the relation and filter.
    ///
    /// The Arrangement is built on first use for that column and maintained by
    /// every later commit, so a query that never asks about a column never pays
    /// for one. It is *derived* state — the primary order is the truth — so it is
    /// rebuilt on demand after a reopen rather than persisted.
    fn read_range_on(
        &self,
        rel: RelId,
        at: Edition,
        col: usize,
        lo: &Value,
        hi: &Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        let mut inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("read_range_on", at.0, inner.watermark));
        }
        // Arrangements track the *current* order; an as-of read below it falls
        // back to the primary order, which is always exact.
        if col == 0 || at.0 != inner.current {
            drop(inner);
            let rows = self.read_at(rel, at)?;
            return Ok(rows
                .into_iter()
                .filter(|(t, _)| t.as_slice().get(col).is_some_and(|v| lo <= v && v < hi))
                .collect());
        }
        Self::ensure_order(&mut inner, rel, col);
        let Some(arr) = inner.roots(rel).and_then(|r| r.orders.get(&col)) else {
            return Ok(Vec::new());
        };
        // A rotated key leads with `col`, so the span is a lead-column range.
        let (klo, khi) = (Tuple::from([lo.clone()]), Tuple::from([hi.clone()]));
        Ok(arr
            .range_collect(&klo, &khi)
            .into_iter()
            .map(|(k, v)| (unrotate(&k, col), v))
            .collect())
    }

    fn watermark(&self) -> Edition {
        Edition(self.inner.lock().unwrap().watermark)
    }

    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        let mut inner = self.inner.lock().unwrap();
        let new_wm = up_to.0.min(inner.current);
        if new_wm <= inner.watermark {
            return Ok(Edition(inner.watermark));
        }
        for rel in inner.rel_ids() {
            let roots = inner.roots(rel).cloned().unwrap_or_default();
            // Fold everything at or below the new watermark into one checkpoint,
            // and keep the versions above it.
            let mut versions = VersionTree::new();
            if let Some((_, t)) = roots.versions.last_le(&new_wm) {
                versions = versions.insert(new_wm, t.clone());
            }
            for (e, t) in roots.versions.range_collect(&(new_wm + 1), &u64::MAX) {
                versions = versions.insert(e, t);
            }
            let mut log = LogTree::new();
            for (k, v) in roots.log.range_collect(&(new_wm + 1, 0), &(u64::MAX, u64::MAX)) {
                log = log.insert(k, v);
            }
            // Consolidation retires versions; the Arrangements rebuild on demand.
            inner.put(rel, RelRoots { versions, log, orders: BTreeMap::new() });
        }
        inner.watermark = new_wm;
        // Every rel's Fact versions and log tail changed — persist all, and
        // retire the roots below the new watermark in the same batch.
        let all_rels: Vec<RelId> = inner.rel_ids();
        self.persist(&inner, &all_rels, Some(new_wm))?;
        Ok(Edition(new_wm))
    }
}

/// **The durable catalog (G-5)** — bindings in the context enfilade at the root
/// scope, so the name→id map versions, persists, and is GC-rooted exactly like
/// the world's facts, rather than living in a private side table.
impl Catalog for EntStore {
    fn rel_id(&self, name: &str) -> Result<Option<RelId>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.ctx.get(&context::catalog_key(name)).and_then(as_rel))
    }

    fn register(&self, name: &str, id: RelId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let key = context::catalog_key(name);
        // Append-only: rebinding a name to a different id is a hard error.
        if let Some(existing) = inner.ctx.get(&key).and_then(as_rel) {
            if existing != id {
                return Err(Error::Store(format!(
                    "catalog conflict: `{name}` already bound to {} (cannot rebind to {})",
                    existing.0, id.0
                )));
            }
            return Ok(());
        }
        inner.ctx = inner.ctx.insert(key, Value::Int(id.0 as i64));
        self.persist_ctx(&inner)
    }

    fn entries(&self) -> Result<Vec<(String, RelId)>> {
        let inner = self.inner.lock().unwrap();
        let (lo, hi) = context::catalog_span();
        // The catalog is one contiguous span of the enfilade, already in name
        // order — a WID range walk, not a scan of every binding.
        Ok(inner
            .ctx
            .range_collect(&lo, &hi)
            .into_iter()
            .filter_map(|(k, v)| Some((as_name(&k)?, as_rel(&v)?)))
            .collect())
    }
}

/// **The durable schema registry (G-5)**, versioned by the edition each version
/// took effect. Because a version's key is `(rel, edition)`, `schema_at` is a
/// **WID range walk** over `[(rel, 0), (rel, at + 1))` and takes the last row —
/// the as-of query is answered by the enfilade's own ordering.
impl SchemaCatalog for EntStore {
    fn put_schema(&self, rel: RelId, schema: &Schema, at: Edition) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some((cur_edition, current)) = latest_schema(&inner.ctx, rel)? {
            if &current == schema {
                return Ok(()); // idempotent re-put — no new version
            }
            // Evolution law: additive-only, and strictly after the current
            // version's edition (a version's edition is when it took effect).
            if !schema.is_additive_over(&current) {
                return Err(Error::Schema(format!(
                    "non-additive schema change for relation {}: a new version may only \
                     append columns to the current one",
                    rel.0
                )));
            }
            if at.0 <= cur_edition {
                return Err(Error::Schema(format!(
                    "schema evolution for relation {} must take effect after edition {} \
                     (got {})",
                    rel.0, cur_edition, at.0
                )));
            }
        }
        let bytes = wire::encode_schema(schema);
        inner.ctx = inner.ctx.insert(context::schema_key(rel, at.0), Value::Bytes(bytes.into()));
        self.persist_ctx(&inner)
    }

    fn schema(&self, rel: RelId) -> Result<Option<Schema>> {
        let inner = self.inner.lock().unwrap();
        Ok(latest_schema(&inner.ctx, rel)?.map(|(_, s)| s))
    }

    fn schema_at(&self, rel: RelId, at: Edition) -> Result<Option<Schema>> {
        let inner = self.inner.lock().unwrap();
        // The newest version whose introducing edition is ≤ `at` — the last row
        // of the pruned span, no scan of other relations' versions.
        let (lo, hi) = context::schema_span(rel, 0, at.0.saturating_add(1));
        match inner.ctx.range_collect(&lo, &hi).last() {
            None => Ok(None),
            Some((_, v)) => decode_schema_value(v).map(Some),
        }
    }
}

// --- helpers --------------------------------------------------------------

fn as_rel(v: &Value) -> Option<RelId> {
    match v {
        Value::Int(i) => Some(RelId(*i as u32)),
        _ => None,
    }
}

/// The relation name from a catalog binding key `(scope, NS_CATALOG, name)`.
fn as_name(k: &Tuple) -> Option<String> {
    match k.as_slice().get(2) {
        Some(Value::Text(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn decode_schema_value(v: &Value) -> Result<Schema> {
    match v {
        Value::Bytes(b) => wire::decode_schema(b),
        _ => Err(Error::Codec("context: schema binding is not bytes".into())),
    }
}

/// The highest-edition schema version for `rel`, with the edition it took
/// effect — the last row of the relation's contiguous version span.
fn latest_schema(ctx: &ContextEnf, rel: RelId) -> Result<Option<(u64, Schema)>> {
    let (lo, hi) = context::schema_all_span(rel);
    match ctx.range_collect(&lo, &hi).last() {
        None => Ok(None),
        Some((k, v)) => {
            let edition = match k.as_slice().get(3) {
                Some(Value::Int(e)) => *e as u64,
                _ => return Err(Error::Codec("context: schema key has no edition".into())),
            };
            Ok(Some((edition, decode_schema_value(v)?)))
        }
    }
}

fn touched(updates: &[(RelId, Tuple, Diff)]) -> Vec<RelId> {
    let mut rels: Vec<RelId> = updates.iter().map(|(r, _, _)| *r).collect();
    rels.sort();
    rels.dedup();
    rels
}

/// The meta key holding the serialized branch graph (the fulltrace's DagWood).
const DAG_KEY: &[u8] = b"dag";

/// Meta keys are **namespaced by branch** (G-6), so every branch's roots live in
/// one granfilade and a fork shares nodes with its ancestor rather than copying
/// them. GC prefix-scans `log:`/`fact:`/`ctx:` and therefore roots from every
/// live branch automatically.
fn branch_key(prefix: &[u8], branch: BranchId) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(&branch.to_be_bytes());
    k
}

/// `ctx:{branch}` — the context enfilade root (catalog + schemas + bindings).
fn ctx_key(branch: BranchId) -> Vec<u8> {
    branch_key(b"ctx:", branch)
}

/// `cur:{branch}` / `wm:{branch}` — this branch's clock and watermark.
fn cur_key(branch: BranchId) -> Vec<u8> {
    branch_key(b"cur:", branch)
}

fn wm_key(branch: BranchId) -> Vec<u8> {
    branch_key(b"wm:", branch)
}

/// `log:{branch}{rel}` — the Edition enfilade root.
fn log_key(branch: BranchId, rel: RelId) -> Vec<u8> {
    let mut k = branch_key(b"log:", branch);
    k.extend_from_slice(&rel.0.to_be_bytes());
    k
}

/// `fact:{branch}{rel}{edition}` — one Fact root per live as-of edition.
fn fact_key(branch: BranchId, rel: RelId, edition: u64) -> Vec<u8> {
    let mut k = branch_key(b"fact:", branch);
    k.extend_from_slice(&rel.0.to_be_bytes());
    k.extend_from_slice(&edition.to_be_bytes());
    k
}

/// The edition a `fact:` key names.
fn fact_edition(key: &[u8]) -> Option<u64> {
    let start = b"fact:".len() + 8 + 4;
    key.get(start..start + 8).map(u64_be)
}

/// The relation a branch-namespaced key names (`prefix || branch || rel || …`).
fn rel_from_key(key: &[u8], prefix: &[u8]) -> RelId {
    let at = prefix.len() + 8;
    let b = &key[at..at + 4];
    RelId(u32::from_be_bytes(b.try_into().unwrap()))
}

fn opt_ck_bytes(ck: Option<ContentKey>) -> Vec<u8> {
    match ck {
        None => vec![0],
        Some(c) => {
            let mut v = vec![1];
            v.extend_from_slice(&c);
            v
        }
    }
}

fn bytes_opt_ck(bytes: &[u8]) -> Option<ContentKey> {
    match bytes.first() {
        Some(1) => bytes.get(1..1 + std::mem::size_of::<ContentKey>()).map(|b| b.try_into().unwrap()),
        _ => None,
    }
}

fn u64_be(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

fn door(op: &str, at: u64, watermark: u64) -> Error {
    Error::Store(format!("{op} at edition {at} below watermark {watermark}"))
}
