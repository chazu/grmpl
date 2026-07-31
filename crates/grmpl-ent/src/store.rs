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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use grmpl_core::{
    wire, Catalog, Diff, Edition, EditionStore, Entity, Error, RelId, Result, Schema,
    SchemaCatalog, Time, TraceStore, Tuple, Update, Value,
};

use crate::context::{self, ContextEnf};
use crate::dag::{BranchId, Dag};
use crate::dsp::Dsp;
use crate::granfilade::{ContentKey, Granfilade};
use crate::measure::Count;
use crate::tree::Tree;

/// The Fact enfilade: `tuple → net Σdiff` (nonzero only).
type FactTree = Tree<Tuple, Diff, Count>;
/// The Edition enfilade: `(edition, submit_index) → (tuple, diff)` raw log.
type LogTree = Tree<(u64, u64), (Tuple, Diff), Count>;

/// An ent store: a family of per-relation Fact + Edition enfilades behind one
/// commit clock, optionally durable on a [`Granfilade`].
pub struct EntStore {
    inner: Mutex<Inner>,
    gran: Option<Granfilade>,
    /// This store's branch in the fulltrace's DagWood.
    branch: BranchId,
    /// The branch DAG shared with every fork of this store (Xanadu's `DagWood` /
    /// fulltrace branch structure) — see [`crate::dag`].
    dag: Arc<Mutex<Dag>>,
}

struct Inner {
    current: u64,
    watermark: u64,
    /// Fact enfilade per rel: `edition → net-state root` (versioned for as-of).
    fact: HashMap<RelId, BTreeMap<u64, FactTree>>,
    /// Edition enfilade per rel: the raw commit-order log.
    log: HashMap<RelId, LogTree>,
    /// Context enfilade: inherited scope bindings, plus the durable catalog and
    /// the edition-versioned schema registry ([`crate::context`]).
    ctx: ContextEnf,
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
        let inner = Inner::rebuild(&gran)?;
        Ok(EntStore {
            inner: Mutex::new(inner),
            gran: Some(gran),
            branch: Dag::ROOT,
            dag: Arc::new(Mutex::new(Dag::new())),
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
        Ok(inner
            .fact
            .get(&rel)
            .and_then(|f| f.range(..=at.0).next_back())
            .map(|(_, t)| t.range_collect(lo, hi))
            .unwrap_or_default())
    }

    /// **WID measure (E2).** How many live tuples of `rel` lie in `[lo, hi)`
    /// as-of `at` — answered in `O(log n)` from the cached subtree measures,
    /// without materializing the rows (Xanadu wid pruning).
    pub fn count_at(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<u64> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("count_at", at.0, inner.watermark));
        }
        Ok(inner
            .fact
            .get(&rel)
            .and_then(|f| f.range(..=at.0).next_back())
            .map(|(_, t)| t.measure_range(lo, hi).0)
            .unwrap_or(0))
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
        let root_at = |ed: u64| {
            inner
                .fact
                .get(&rel)
                .and_then(|f| f.range(..=ed).next_back())
                .map(|(_, t)| t.clone())
                .unwrap_or_default()
        };
        Ok(root_at(a.0).diff(&root_at(b.0)))
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

    /// **Structural-sharing fork (E3).** A new independent store whose state is
    /// this store's as-of `at`, **sharing every enfilade node** with the parent
    /// (the versioned Fact roots are `Arc`-cloned, not copied). Forking at the
    /// current edition is `O(#relations)`, not the LSM's `O(state)` verbatim copy
    /// — the cheap virtual copy at the heart of the Ent. The fork then evolves
    /// independently. (In-memory; a persistent fork sharing the granfilade node
    /// store is a later increment.)
    pub fn fork_at(&self, at: Edition) -> Result<EntStore> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("fork_at", at.0, inner.watermark));
        }
        // Graft a new branch onto this store's branch at the fork edition in the
        // shared DagWood; the child carries that id and the same registry, so
        // ancestry is queryable across the whole fork family (the fulltrace).
        let child_branch = self.dag.lock().unwrap().fork(self.branch, at.0);
        // Fact: keep the versioned roots up to `at` (their trees are shared Arcs).
        let mut fact: HashMap<RelId, BTreeMap<u64, FactTree>> = HashMap::new();
        for (rel, versions) in &inner.fact {
            let kept: BTreeMap<u64, FactTree> =
                versions.range(..=at.0).map(|(e, t)| (*e, t.clone())).collect();
            if !kept.is_empty() {
                fact.insert(*rel, kept);
            }
        }
        // Edition log: the whole tree when forking at current (shared Arc), else
        // the prefix ≤ `at`.
        let mut log: HashMap<RelId, LogTree> = HashMap::new();
        for (rel, l) in &inner.log {
            if at.0 >= inner.current {
                log.insert(*rel, l.clone());
            } else {
                let mut t = LogTree::new();
                for (k, v) in l.range_collect(&(0, 0), &(at.0 + 1, 0)) {
                    t = t.insert(k, v);
                }
                if !t.is_empty() {
                    log.insert(*rel, t);
                }
            }
        }
        Ok(EntStore {
            inner: Mutex::new(Inner {
                current: at.0,
                watermark: inner.watermark,
                fact,
                log,
                ctx: inner.ctx.clone(),
            }),
            gran: None,
            branch: child_branch,
            dag: Arc::clone(&self.dag),
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
        let lo = Tuple::from([Value::Ent(Entity(block_lo))]);
        let hi = Tuple::from([Value::Ent(Entity(block_hi))]);
        let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for &rel in rels {
            for (tuple, diff) in self.range_at(rel, at, &lo, &hi)? {
                updates.push((rel, dsp.apply_all(&tuple), diff));
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
        gran.write(nodes, vec![(CTX_KEY.to_vec(), opt_ck_bytes(ck))])
    }

    /// Persist the clock plus the touched relations' Edition enfilades (roots +
    /// nodes) in one atomic granfilade write. A no-op for an in-memory store.
    fn persist(&self, inner: &Inner, touched: &[RelId], all_ckpts: bool) -> Result<()> {
        let gran = match &self.gran {
            Some(g) => g,
            None => return Ok(()),
        };
        let mut nodes = Vec::new();
        let mut meta = vec![
            (b"cur".to_vec(), inner.current.to_be_bytes().to_vec()),
            (b"wm".to_vec(), inner.watermark.to_be_bytes().to_vec()),
        ];
        for rel in touched {
            let log = inner.log.get(rel).cloned().unwrap_or_default();
            let (ck, ns) = gran.collect_tree(&log);
            nodes.extend(ns);
            meta.push((log_key(*rel), opt_ck_bytes(ck)));
        }
        // On consolidate the Fact checkpoint (@ watermark) changes for every rel.
        if all_ckpts {
            for (rel, facts) in &inner.fact {
                let ckpt = facts.range(..=inner.watermark).next_back().map(|(_, t)| t.clone());
                let ck = ckpt.map(|t| {
                    let (ck, ns) = gran.collect_tree(&t);
                    nodes.extend(ns);
                    ck
                });
                meta.push((ckpt_key(*rel), opt_ck_bytes(ck.flatten())));
            }
        }
        gran.write(nodes, meta)
    }
}

impl Inner {
    fn empty() -> Inner {
        Inner {
            current: 0,
            watermark: 0,
            fact: HashMap::new(),
            log: HashMap::new(),
            ctx: ContextEnf::new(),
        }
    }

    /// Rebuild in-memory state from a granfilade: the clock, each rel's Fact
    /// checkpoint (@ watermark) and Edition-log tail, then replay the tail to
    /// reconstruct the versioned Fact roots above the watermark.
    fn rebuild(gran: &Granfilade) -> Result<Inner> {
        let current = gran.meta_get(b"cur")?.map(|b| u64_be(&b)).unwrap_or(0);
        let watermark = gran.meta_get(b"wm")?.map(|b| u64_be(&b)).unwrap_or(0);
        let ctx: ContextEnf = gran.load(bytes_opt_ck(&gran.meta_get(CTX_KEY)?.unwrap_or_default()))?;
        let mut inner =
            Inner { current, watermark, fact: HashMap::new(), log: HashMap::new(), ctx };

        // Fact checkpoints (state as-of the watermark).
        for (key, val) in gran.meta_prefix(b"ckpt:")? {
            let rel = rel_from_key(&key, b"ckpt:");
            if let Some(ck) = bytes_opt_ck(&val) {
                let tree: FactTree = gran.load(Some(ck))?;
                inner.fact.entry(rel).or_default().insert(watermark, tree);
            }
        }
        // Edition-log tails, then replay to rebuild the Fact roots above wm.
        for (key, val) in gran.meta_prefix(b"log:")? {
            let rel = rel_from_key(&key, b"log:");
            let log: LogTree = gran.load(bytes_opt_ck(&val))?;
            for ((e, _submit), (tuple, diff)) in log.iter().map(|(k, v)| (*k, v.clone())) {
                inner.fold_fact(e, rel, &tuple, diff);
            }
            inner.log.insert(rel, log);
        }
        Ok(inner)
    }

    /// Fold one update into the Fact enfilade at edition `e`: build a fresh root
    /// from the latest root ≤ `e`, netting the tuple's weight; drop it at 0.
    fn fold_fact(&mut self, e: u64, rel: RelId, tuple: &Tuple, diff: Diff) {
        let facts = self.fact.entry(rel).or_default();
        let base = facts.range(..=e).next_back().map(|(_, t)| t.clone()).unwrap_or_default();
        let cur = base.get(tuple).copied().unwrap_or(0);
        let net = cur + diff;
        let root = if net == 0 { base.remove(tuple) } else { base.insert(tuple.clone(), net) };
        facts.insert(e, root);
    }

    /// Apply `updates` as edition `e`: append to each Edition enfilade in submit
    /// order and fold each into the Fact enfilade.
    fn apply(&mut self, e: u64, updates: &[(RelId, Tuple, Diff)]) {
        for (i, (rel, tuple, diff)) in updates.iter().enumerate() {
            {
                let log = self.log.entry(*rel).or_default();
                *log = log.insert((e, i as u64), (tuple.clone(), *diff));
            }
            self.fold_fact(e, *rel, tuple, *diff);
        }
        self.current = e;
    }

    fn holds_now(&self, rel: RelId, tuple: &Tuple) -> bool {
        self.fact
            .get(&rel)
            .and_then(|f| f.range(..=self.current).next_back())
            .and_then(|(_, t)| t.get(tuple))
            .is_some_and(|n| *n > 0)
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
        self.persist(&inner, &touched(updates), false)?;
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
        self.persist(&inner, &touched(updates), false)?;
        Ok(Some(Edition(e)))
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("read_at", at.0, inner.watermark));
        }
        let rows = inner
            .fact
            .get(&rel)
            .and_then(|f| f.range(..=at.0).next_back())
            .map(|(_, t)| t.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        Ok(rows)
    }

    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        let inner = self.inner.lock().unwrap();
        if from.0 < inner.watermark {
            return Err(door("scan_updates", from.0, inner.watermark));
        }
        let mut out = Vec::new();
        if let Some(log) = inner.log.get(&rel) {
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

    fn watermark(&self) -> Edition {
        Edition(self.inner.lock().unwrap().watermark)
    }

    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        let mut inner = self.inner.lock().unwrap();
        let new_wm = up_to.0.min(inner.current);
        if new_wm <= inner.watermark {
            return Ok(Edition(inner.watermark));
        }
        for facts in inner.fact.values_mut() {
            let checkpoint = facts.range(..=new_wm).next_back().map(|(_, t)| t.clone());
            let mut next: BTreeMap<u64, FactTree> = BTreeMap::new();
            if let Some(t) = checkpoint {
                next.insert(new_wm, t);
            }
            for (e, t) in facts.range((new_wm + 1)..) {
                next.insert(*e, t.clone());
            }
            *facts = next;
        }
        for log in inner.log.values_mut() {
            let tail = log.range_collect(&(new_wm + 1, 0), &(u64::MAX, u64::MAX));
            let mut next = LogTree::new();
            for (k, v) in tail {
                next = next.insert(k, v);
            }
            *log = next;
        }
        inner.watermark = new_wm;
        // Every rel's checkpoint and log tail changed — persist all.
        let all_rels: Vec<RelId> = inner.log.keys().copied().collect();
        self.persist(&inner, &all_rels, true)?;
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

/// The meta key holding the context enfilade's root (catalog + schemas + scoped
/// bindings). GC roots from it, so the catalog is never collected.
const CTX_KEY: &[u8] = b"ctx:";

fn log_key(rel: RelId) -> Vec<u8> {
    let mut k = b"log:".to_vec();
    k.extend_from_slice(&rel.0.to_be_bytes());
    k
}

fn ckpt_key(rel: RelId) -> Vec<u8> {
    let mut k = b"ckpt:".to_vec();
    k.extend_from_slice(&rel.0.to_be_bytes());
    k
}

fn rel_from_key(key: &[u8], prefix: &[u8]) -> RelId {
    let b = &key[prefix.len()..prefix.len() + 4];
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
