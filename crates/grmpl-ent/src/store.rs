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
//! [`EntStore::open`] backs the store with a [`Granfilade`]. **Everything is in
//! the Ent:** the granfilade's one root record links to the branch DAG and to the
//! **branch enfilade**, `branch → that branch's whole state` — its clock, its
//! Rel enfilade (each relation's versions, log and Arrangements), its context
//! enfilade and its canopy. A commit path-copies its way up to a new root record
//! and writes it, with the new nodes, in one atomic batch. Opening a store reads
//! the root record and pages the rest in as reads reach it; nothing is rebuilt.
//! The **persisted form is the enfilade itself, never a log**. [`EntStore::new`]
//! is a pure in-memory store (used by the conformance oracle).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use grmpl_core::{
    wire, Catalog, Diff, Edition, EditionStore, Entity, Error, RelId, Result, Schema,
    SchemaCatalog, Time, TraceStore, Tuple, Update, Value,
};

use crate::canopy::{Canopy, InterestId};
use crate::context::{self, ContextEnf};
use crate::dag::{BranchId, Dag};
use crate::dsp::Displace;
use crate::granfilade::{content_key, ContentKey, Dec, Enc, Granfilade, Persist, StagedWrite};
use crate::history::{History, Holder};
use crate::measure::{Count, Extent, Measure};
use crate::spanfilade::{GraftSpan, Spanfilade};
use crate::tree::{Layout, Tree};

/// The Fact enfilade: `tuple → net Σdiff` (nonzero only), measured by the entry
/// [`Count`], so "how many rows" over any key span is an `O(log n)` fold of
/// cached summaries, materializing nothing, and by the [`Extent`], so a search
/// on any entity column prunes every subtree whose bounding box misses it.
type FactMeasure = (Count, Extent);
type FactTree = Tree<Tuple, Diff, FactMeasure>;
/// The Edition enfilade: `(edition, submit_index) → entry`, the raw log in
/// commit order.
type LogTree = Tree<(u64, u64), LogEntry, Count>;

/// A version compare with its virtual copies named
/// ([`EntStore::compare_spans`]).
///
/// To rebuild `rel` at `b` from `rel` at `a`: replace each copy's target
/// block, oldest first, with its source block as of the edition before the
/// copy, shifted; then apply `rows`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanCompare {
    /// The grafts into the relation made in `(a, b]`, oldest first.
    pub copies: Vec<GraftSpan>,
    /// Every other difference, `(tuple, weight before, weight at b)` against
    /// `a` with the copies spliced in, tuple-sorted.
    pub rows: Vec<crate::tree::EntryDiff<Tuple, Diff>>,
}

/// **One version of one relation**: its Fact tree in force at `edition` on
/// `branch`, the unit the history index answers in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub branch: BranchId,
    pub rel: RelId,
    pub edition: Edition,
}

/// **What backfollow found** ([`EntStore::backfollow`]): a version whose Fact
/// tree holds some of the queried content, `shift` away from where the query
/// holds it, and how many of the queried rows it holds there.
///
/// `version.edition` is the edition the version was written at (a key of the
/// relation's version directory), so one holding stands for every edition up
/// to the relation's next change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Holding {
    pub version: Version,
    pub shift: i64,
    pub rows: usize,
}

/// **What a merge did** ([`EntStore::merge`]).
pub enum MergeOutcome {
    /// Every patch replayed: the new branch, with both histories behind it.
    Merged(EntStore),
    /// A replayed patch's preconditions no longer held (or its graft was
    /// refused) in the merging state. Nothing was created.
    Conflict(MergeConflict),
}

/// The patch a merge could not replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
    /// Where the patch was first committed.
    pub branch: BranchId,
    pub edition: Edition,
    /// What failed: a precondition that no longer holds, or why a graft was
    /// refused, or a catalog or schema binding the two sides disagree on.
    pub reason: String,
}

/// One record of a branch's **patch log**: what each of its editions was, so
/// another branch can replay it ([`EntStore::merge`]). A commit's updates are
/// not copied here: each relation's Edition log already holds them, indexed by
/// their place in the patch.
#[derive(Clone, PartialEq, Debug)]
enum PatchRecord {
    /// A commit: the tuples it required to hold, and the relations it wrote.
    Commit { pre: Vec<(RelId, Tuple)>, rels: Vec<u32>, origin: Origin },
    /// A graft: the entity block copied, how far, and the relations asked for.
    Graft { rels: Vec<u32>, block: (u64, u64), shift: i64, origin: Origin },
}

/// Where a merge replayed a patch from: the `(branch, edition)` it was first
/// committed at. `None` for a patch first committed here. A later merge skips
/// a copy whose original it reaches, so nothing is replayed twice.
type Origin = Option<(BranchId, u64)>;

impl PatchRecord {
    fn origin(&self) -> Origin {
        match self {
            PatchRecord::Commit { origin, .. } | PatchRecord::Graft { origin, .. } => *origin,
        }
    }
}

/// The patch log: `edition → what it was`.
type PatchTree = Tree<u64, PatchRecord, Count>;

impl Persist for PatchRecord {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        match self {
            PatchRecord::Commit { pre, rels, origin } => {
                0u32.encode(e);
                (pre.len() as u32).encode(e);
                for (rel, t) in pre {
                    rel.0.encode(e);
                    t.encode(e);
                }
                encode_rels(rels, e);
                origin.encode(e);
            }
            PatchRecord::Graft { rels, block, shift, origin } => {
                1u32.encode(e);
                encode_rels(rels, e);
                block.0.encode(e);
                block.1.encode(e);
                shift.encode(e);
                origin.encode(e);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        match u32::decode(d)? {
            0 => {
                let n = u32::decode(d)? as usize;
                let mut pre = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    pre.push((RelId(u32::decode(d)?), Tuple::decode(d)?));
                }
                Ok(PatchRecord::Commit { pre, rels: decode_rels(d)?, origin: Option::decode(d)? })
            }
            1 => Ok(PatchRecord::Graft {
                rels: decode_rels(d)?,
                block: (u64::decode(d)?, u64::decode(d)?),
                shift: i64::decode(d)?,
                origin: Option::decode(d)?,
            }),
            t => Err(Error::Codec(format!("ent: bad patch record tag {t}"))),
        }
    }
}

fn encode_rels(rels: &[u32], e: &mut Enc<'_, '_>) {
    (rels.len() as u32).encode(e);
    for r in rels {
        r.encode(e);
    }
}

fn decode_rels(d: &mut Dec<'_>) -> Result<Vec<u32>> {
    let n = u32::decode(d)? as usize;
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        out.push(u32::decode(d)?);
    }
    Ok(out)
}

/// One record of the Edition enfilade.
#[derive(Clone, PartialEq, Debug)]
enum LogEntry {
    /// An ordinary update, in submit order.
    Update(Tuple, Diff),
    /// A **graft**: this edition's Fact root holds, in `[lo, hi)`, a virtual
    /// copy of a block that was empty before it. The rows are read back from
    /// that root rather than logged one by one, so the log grows by one entry
    /// however large the copy is.
    Graft(Tuple, Tuple),
}

impl Persist for LogEntry {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        match self {
            LogEntry::Update(tuple, diff) => {
                e.put(&[0]);
                tuple.encode(e);
                diff.encode(e);
            }
            LogEntry::Graft(lo, hi) => {
                e.put(&[1]);
                lo.encode(e);
                hi.encode(e);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        let tag = d.take(1)?[0];
        let a = Tuple::decode(d)?;
        match tag {
            0 => Ok(LogEntry::Update(a, Diff::decode(d)?)),
            1 => Ok(LogEntry::Graft(a, Tuple::decode(d)?)),
            other => Err(Error::Codec(format!("granfilade: unknown log entry tag {other}"))),
        }
    }
}
/// The **Version enfilade**: `edition → Fact root`, one persistent root per
/// live as-of edition (G-2a). `last_le` makes an as-of read a descent.
type VersionTree = Tree<u64, FactTree, Count>;
/// The **Rel enfilade**: the directory of live relations (G-2a).
type RelTree = Tree<u32, RelRoots, Count>;
/// The **Arrangement enfilade**: a relation's alternate orderings, by lead column.
type OrderTree = Tree<u32, FactTree, Count>;
/// The **fired-interest enfilade**: `(interest, edition) → ()`.
///
/// Keyed interest-first so "did interest *i* fire anywhere in `(from, to]`" is a
/// single WID range measure — `O(log n)`, no scan of the interval.
type FiredTree = Tree<(u64, u64), (), Count>;

/// The **layout directory**: `relation → the shape of its Fact trees`, for
/// every relation laid out other than by the branch's default. A directory
/// beside the Rel enfilade rather than a field of each relation's roots, so a
/// relation can be laid out before its first fact, and a fork or a merge
/// carries the choice whether or not the relation has rows yet.
type LayoutTree = Tree<u32, Layout, Count>;

/// A layout persists as its one-byte tag.
impl Persist for Layout {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        e.put(&[self.tag()]);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        let tag = d.take(1)?[0];
        Layout::from_tag(tag).ok_or_else(|| Error::Codec(format!("ent: unknown layout tag {tag}")))
    }
}

/// One relation's roots: its versioned Fact enfilade, its Edition log, and any
/// **Arrangements** — alternate orderings of the same facts (G-9).
#[derive(Clone, Default)]
struct RelRoots {
    versions: VersionTree,
    log: LogTree,
    /// The **Arrangement enfilade**: `lead column → the same facts keyed by that
    /// column first`. An enfilade rather than a map, like every other directory
    /// in the store's state, so "which orderings does this relation carry" is a
    /// measure and the iteration order is deterministic.
    ///
    /// The primary order prunes on the lead column only, so a predicate on any
    /// other column has to scan. An Arrangement is one more measured tree over
    /// the same facts, rotated so the column of interest leads — after which
    /// pruning on it is the ordinary WID range walk. Built on first use and
    /// maintained with every commit, so the cost is paid only for columns some
    /// query actually asks about.
    orders: OrderTree,
}

/// A relation's roots persist as three links, so a Rel enfilade leaf names its
/// relations' trees and GC follows them from there.
impl Persist for RelRoots {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        e.link(&self.versions);
        e.link(&self.log);
        e.link(&self.orders);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(RelRoots { versions: d.link()?, log: d.link()?, orders: d.link()? })
    }
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

/// One staged write waiting for its group's `fsync`.
struct Pending {
    /// Its place in the family's staging order.
    seq: u64,
    /// The branch and edition it makes durable.
    branch: BranchId,
    edition: u64,
    write: StagedWrite,
}

/// **The group-commit queue.** Editions applied in memory and encoded, waiting
/// for the `fsync` that makes them durable. One queue serves every branch of a
/// world, because they share one root record.
///
/// The `Mutex<Inner>` serializes *edition allocation*, which is the law
/// (one authority domain, one commit clock). It used to serialize *durability*
/// too, because `commit_if` held it across the ~1 ms `SyncAll`, so N committers
/// paid N fsyncs strictly in series. This queue separates the two: a committer
/// leaves the edition lock as soon as its work is encoded, and one member of the
/// group performs a single batch + `SyncAll` covering every staged edition.
///
/// **What group commit does and does not weaken.** The patch–edition law is
/// about *atomicity*: an edition is allocated and written as one step, or not at
/// all — there is never a state carrying only part of an edition. `write_group`
/// is one batch, so that holds exactly as before. What changes is *when* a write
/// becomes durable, and the gate is on the commit call:
///
/// **`commit`/`commit_if` return only after the edition they return is durable.**
///
/// So no committer ever learns of its own edition before disk does — no player is
/// told "Taken." for a take a crash would erase, which is the property the law is
/// protecting.
///
/// **The clock is the allocated edition, not this watermark**, and that is
/// forced rather than chosen. `commit_if` must validate preconditions against the
/// allocated state — checking them against a lagging watermark would let two
/// committers in one group *both* win a contested precondition, breaking
/// exactly-one-winner. Reads must then agree with the validator, or every
/// optimistic read-modify-write would build its patch on a stale world and be
/// rejected forever. A guarded allocator livelocks within a dozen attempts if
/// these two disagree; there is no version of this that reports the watermark as
/// the clock and still works.
///
/// The residual window is therefore precise and small: between a peer's `stage`
/// and its group's fsync, a *third party* can read an edition that is not yet on
/// disk. It cannot externalize that read through the store, because the queue is
/// FIFO in edition order and a group's fsync covers every edition staged before
/// it — so any commit built on edition `E` is itself durable only once `E` is.
/// A reader that externalizes *outside* the store (straight to a socket) and
/// wants the on-disk frontier should ask for it: see
/// [`EntStore::durable_edition`].
#[derive(Default)]
struct Durable {
    /// Staged writes in **staging order** — the order they must reach disk, and
    /// the order the batch applies them in. Each carries the whole root record
    /// as of its stage, so the last one in a batch is the world the batch leaves
    /// behind. Pushed under the root lock, which is what keeps them ordered.
    pending: VecDeque<Pending>,
    /// The last staging sequence number handed out.
    staged: u64,
    /// The highest staging sequence number proven durable.
    landed: u64,
    /// Each branch's highest edition proven durable.
    durable: BTreeMap<BranchId, u64>,
    /// Whether a leader is inside the batch + fsync right now.
    writing: bool,
    /// A failed group: the highest sequence number it carried, and why it
    /// failed. Sticky, so every committer whose write was in that group learns
    /// of it rather than waiting forever for a flush that will never come.
    failure: Option<(u64, String)>,
}

impl Durable {
    /// The failure that dooms a committer waiting for `target` (or, for a
    /// flusher, any failure at all).
    fn doom(&self, target: Option<u64>) -> Option<String> {
        let (through, msg) = self.failure.as_ref()?;
        match target {
            Some(s) if s > *through => None,
            _ => Some(msg.clone()),
        }
    }
}

/// **The Ent's root**: the branch DAG (the fulltrace's DagWood), the branch
/// enfilade (one state per branch), and the history index over every branch
/// (Gold's H-tree, [`crate::history`]). The granfilade's root record links to
/// these trees.
struct EntRoot {
    dag: Dag,
    branches: BranchStates,
    history: History,
}

/// The branch enfilade: `branch → that branch's whole state`.
type BranchStates = Tree<BranchId, Inner, Count>;

impl EntRoot {
    fn empty() -> EntRoot {
        EntRoot { dag: Dag::new(), branches: BranchStates::new(), history: History::default() }
    }

    /// The root record's trees, paged: one frame read for each. A record
    /// written before the history index existed has no history slots, and the
    /// index is rebuilt from the versions on its first catch-up.
    fn load(gran: &Arc<Granfilade>) -> Result<EntRoot> {
        let slots = gran.root()?;
        let slot = |i: usize| slots.get(i).copied().flatten();
        Ok(EntRoot {
            dag: Dag::from_tree(gran.load(slot(0))?),
            branches: gran.load(slot(1))?,
            history: History::from_trees(
                gran.load(slot(2))?,
                gran.load(slot(3))?,
                gran.load(slot(4))?,
                gran.load(slot(5))?,
            ),
        })
    }

    /// **Bring the history index up to date**, indexing at most `budget`
    /// versions: the deferred work Gold runs on its Agenda. Branches go in id
    /// order, so a branch's ancestors are indexed through its fork point before
    /// it is, and each branch's versions in edition order. A branch starts
    /// after its fork point: what it inherited is its ancestors' and found
    /// through the DAG. Returns the versions indexed.
    fn catch_up(&mut self, budget: usize) -> usize {
        let mut done = 0;
        let ids: Vec<BranchId> = self.dag.tree().iter().map(|(b, _)| b).collect();
        for b in ids {
            let Some(state) = self.branches.get(&b).cloned() else { continue };
            let start = self.start_of(b);
            if state.current <= start {
                continue;
            }
            let mut todo: Vec<(u64, RelId, FactTree)> = Vec::new();
            for (rel, roots) in state.rels.iter() {
                for (e, t) in roots.versions.range_collect(&(start + 1), &(state.current + 1)) {
                    todo.push((e, RelId(rel), t));
                }
            }
            todo.sort_by_key(|(e, rel, _)| (*e, *rel));
            let mut through = start;
            let mut i = 0;
            while i < todo.len() {
                if done >= budget {
                    self.history.set_cursor(b, through);
                    return done;
                }
                // A whole edition at a time, so the cursor never splits one.
                let e = todo[i].0;
                while i < todo.len() && todo[i].0 == e {
                    let (_, rel, t) = &todo[i];
                    self.history.index_version(&self.dag, Holder { branch: b, rel: rel.0, edition: e }, t);
                    done += 1;
                    i += 1;
                }
                through = e;
            }
            self.history.set_cursor(b, state.current);
        }
        done
    }

    /// The edition the history of `branch` is indexed through: its cursor, or
    /// its fork point before it has been indexed at all.
    fn start_of(&self, b: BranchId) -> u64 {
        self.history.cursor(b).unwrap_or_else(|| self.dag.parent(b).map_or(0, |(_, at)| at))
    }

    /// Versions not yet indexed.
    fn backlog(&self) -> usize {
        let mut n = 0;
        for (b, state) in self.branches.iter() {
            let start = self.start_of(b);
            for (_, roots) in state.rels.iter() {
                n += roots.versions.count_range(&(start + 1), &(state.current + 1));
            }
        }
        n
    }

    /// The state of `branch` as last staged, or a fresh one for a branch that
    /// has never committed.
    fn state(&self, branch: BranchId) -> Result<Inner> {
        if self.dag.get(branch).is_none() {
            return Err(Error::Store(format!("unknown branch {branch}")));
        }
        Ok(self.branches.get(&branch).cloned().unwrap_or_else(Inner::empty))
    }
}

/// Everything the branches of one world share: the node store, the Ent's
/// root, and the group-commit queue that writes it.
///
/// The root record is one slot for the whole world, so every branch's commit
/// rewrites it; staging under one lock and writing in staging order is what
/// keeps a later root from being overwritten by an earlier one.
struct Family {
    /// The node substrate; `None` for an in-memory world.
    gran: Option<Arc<Granfilade>>,
    /// The Ent's root as of the latest stage.
    root: Mutex<EntRoot>,
    /// Staged root records not yet fsynced (see [`Durable`]).
    dur: Mutex<Durable>,
    /// Signalled whenever a group lands (or fails), waking its followers.
    flushed: Condvar,
}

/// An ent store: one branch of a world, its relations held as Fact + Edition
/// enfilades behind one commit clock, optionally durable on a [`Granfilade`].
pub struct EntStore {
    inner: Mutex<Inner>,
    /// **Read-lock ops counter.** How many times a pinned-edition read has had to
    /// take the edition lock.
    ///
    /// The third counter in the same discipline as `frames_encoded` (is the
    /// commit path still path-sized?) and `syncs` (are committers still sharing
    /// fsyncs?). This one pins the reader claim: a `Snapshot` acquires an
    /// `EntReader` once and every read through it is lock-free, so a plan with N
    /// base relations costs **one** acquisition rather than N. Without a counter
    /// that is prose; with it, a test fails the day a read quietly re-enters the
    /// store.
    read_locks: std::sync::atomic::AtomicU64,
    /// What this branch shares with every other branch of its world (G-6): one
    /// granfilade and one root, so a durable fork shares nodes with its ancestor
    /// instead of copying them.
    family: Arc<Family>,
    /// This store's branch in the fulltrace's DagWood.
    branch: BranchId,
}

/// **One branch's whole state** — the value the branch enfilade holds for it,
/// and what the store mutates under its edition lock.
#[derive(Clone)]
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
    /// **The canopy** ([`crate::canopy`]): standing `(rel, key-range)` interests,
    /// held in a measured interval enfilade.
    canopy: Canopy,
    /// Which interests each commit stabbed. Routing happens **once, at commit
    /// time** — the canopy is stabbed with the updates as they land — and the
    /// answer is then a measure, so a watcher asking "did anything of mine
    /// change?" never re-reads the interval.
    fired: FiredTree,
    /// The interest already registered for a given `(rel, lo, hi)`, so repeated
    /// asks reuse one rather than minting a new interest per call.
    interests: Tree<(u32, Tuple, Tuple), InterestId, Count>,
    /// The edition each interest was registered at. Commits before it were never
    /// routed to it, so an interval reaching back past it must widen to the
    /// relation-wide answer rather than read an empty fired-set as "no change".
    registered: Tree<u64, u64, Count>,
    /// **The spanfilade** ([`crate::spanfilade`]): every graft this branch has
    /// made, by source span and by target span, so a template knows its
    /// instances and an instance its template.
    grafts: Spanfilade,
    /// **The patch log**: what every edition of this branch's own was, with
    /// its preconditions, so a merge can replay it on another branch.
    patches: PatchTree,
    /// The layout a relation's Fact trees take unless `layouts` names one.
    layout: Layout,
    /// **The layout directory**: relations laid out otherwise.
    layouts: LayoutTree,
}

/// A branch's state persists as its clock and links to its trees. The canopy
/// and its fired-set ride along, so an interest and the commits routed to it
/// land in the same atomic root — a reopen sees both or neither.
impl Persist for Inner {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.current.encode(e);
        self.watermark.encode(e);
        e.link(&self.rels);
        e.link(&self.ctx);
        self.canopy.encode(e);
        e.link(&self.fired);
        e.link(&self.interests);
        e.link(&self.registered);
        self.grafts.encode(e);
        e.link(&self.patches);
        self.layout.encode(e);
        e.link(&self.layouts);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(Inner {
            current: u64::decode(d)?,
            watermark: u64::decode(d)?,
            rels: d.link()?,
            ctx: d.link()?,
            canopy: Canopy::decode(d)?,
            fired: d.link()?,
            interests: d.link()?,
            registered: d.link()?,
            grafts: Spanfilade::decode(d)?,
            patches: d.link()?,
            layout: Layout::decode(d)?,
            layouts: d.link()?,
        })
    }
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

    /// The layout `rel`'s Fact trees take.
    fn layout_of(&self, rel: RelId) -> Layout {
        self.layouts.get(&rel.0).copied().unwrap_or(self.layout)
    }

    /// Replace `rel`'s roots, creating the entry if it is new.
    fn put(&mut self, rel: RelId, roots: RelRoots) {
        self.rels = self.rels.insert(rel.0, roots);
    }

    /// Every live relation, in id order — deterministic, unlike a `HashMap`.
    fn rel_ids(&self) -> Vec<RelId> {
        self.rels.iter().map(|(r, _)| RelId(r)).collect()
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
        let family = Family {
            gran: None,
            root: Mutex::new(EntRoot::empty()),
            dur: Mutex::new(Durable::default()),
            flushed: Condvar::new(),
        };
        EntStore::on(Arc::new(family), Dag::ROOT, Inner::empty())
    }

    /// Open (or create) a durable ent store on a granfilade at `path`.
    ///
    /// This reads the root record and the few frames above the root branch's
    /// state; every relation, version and log beneath it stays on disk until a
    /// read reaches it. Opening a world costs the same however large it is.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<EntStore> {
        Self::open_branch(path, Dag::ROOT)
    }

    /// Reopen a specific branch of a granfilade — the durable counterpart of
    /// [`fork_at`](Self::fork_at). [`open`](Self::open) is this at
    /// [`Dag::ROOT`]. The granfilade must not already be open: use
    /// [`branch`](Self::branch) when it is.
    pub fn open_branch(path: impl AsRef<std::path::Path>, branch: BranchId) -> Result<EntStore> {
        let gran = Granfilade::open(path)?;
        let root = EntRoot::load(&gran)?;
        let inner = root.state(branch)?;
        let family = Family {
            gran: Some(gran),
            root: Mutex::new(root),
            dur: Mutex::new(Durable::default()),
            flushed: Condvar::new(),
        };
        Ok(EntStore::on(Arc::new(family), branch, inner))
    }

    /// A handle on `branch` of `family`, whose state is `inner`.
    fn on(family: Arc<Family>, branch: BranchId, inner: Inner) -> EntStore {
        family.dur.lock().unwrap().durable.insert(branch, inner.current);
        EntStore {
            inner: Mutex::new(inner),
            read_locks: std::sync::atomic::AtomicU64::new(0),
            family,
            branch,
        }
    }

    /// **WID range read (E2).** The rows of `rel` whose tuple key lies in
    /// `[lo, hi)`, as-of `at` — an `O(result + log n)` walk of the Fact enfilade
    /// that prunes whole out-of-range subtrees, where the LSM must scan the
    /// relation. (Lead-prefix pruning on the primary order; per-column
    /// Arrangements for trailing-column spans are the next increment.)
    pub fn range_at(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.note_read_lock();
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
        Ok(inner.fact_at(rel, at.0).map(|t| t.count_range(lo, hi) as u64).unwrap_or(0))
    }

    /// **WID search over entity columns.** The rows of `rel` as-of `at` whose
    /// column `col` holds an entity in `[lo, hi)`, for every `(col, lo, hi)` in
    /// `bounds` — a box in entity space.
    ///
    /// Answered from each subtree's [`Extent`]: a subtree whose bounding box
    /// misses any of the spans is skipped unread. No Arrangement is built or
    /// needed, so it works at any live edition and on any column, and it takes
    /// the store lock only to find the root. What it costs depends on how well
    /// the columns track the key order: a tight box prunes to the matches, a
    /// column scattered across the world prunes nothing and reads the relation.
    pub fn search_at(&self, rel: RelId, at: Edition, bounds: &[(usize, u64, u64)]) -> Result<Vec<(Tuple, Diff)>> {
        let facts = {
            let inner = self.inner.lock().unwrap();
            if at.0 < inner.watermark {
                return Err(door("search_at", at.0, inner.watermark));
            }
            inner.fact_at(rel, at.0).cloned()
        };
        Ok(facts.map(|t| search_box(&t, bounds)).unwrap_or_default())
    }

    /// **The layout of `rel`'s Fact trees** ([`Layout`]): the B+ tree ordered
    /// by the whole key, or binary splits on any column (Gold's `SplitLoaf`,
    /// fidelity gap G8).
    pub fn layout(&self, rel: RelId) -> Layout {
        self.inner.lock().unwrap().layout_of(rel)
    }

    /// **Lay `rel` out** as `layout`. A relation's layout is fixed once it has
    /// a version, since its trees are built in it, so this is refused for a
    /// relation that has ever been written. Durable when it returns, and
    /// carried by forks and merges.
    pub fn set_layout(&self, rel: RelId, layout: Layout) -> Result<()> {
        let seq = {
            let mut inner = self.inner.lock().unwrap();
            if inner.roots(rel).is_some_and(|r| !r.versions.is_empty() || !r.log.is_empty()) {
                if inner.layout_of(rel) == layout {
                    return Ok(());
                }
                return Err(Error::Store(format!("set_layout: relation {} already has versions", rel.0)));
            }
            inner.layouts = inner.layouts.insert(rel.0, layout);
            self.stage(&inner)?
        };
        self.await_durable(seq)
    }

    /// **The layout of every relation not laid out by
    /// [`set_layout`](Self::set_layout)**, including relations already written:
    /// so this too is refused once any relation that would change has a
    /// version. Durable when it returns.
    pub fn set_default_layout(&self, layout: Layout) -> Result<()> {
        let seq = {
            let mut inner = self.inner.lock().unwrap();
            if inner.layout == layout {
                return Ok(());
            }
            for (rel, roots) in inner.rels.iter() {
                if inner.layouts.get(&rel).is_none() && (!roots.versions.is_empty() || !roots.log.is_empty()) {
                    return Err(Error::Store(format!("set_default_layout: relation {rel} already has versions")));
                }
            }
            inner.layout = layout;
            self.stage(&inner)?
        };
        self.await_durable(seq)
    }

    /// **Where a block was copied to** (the spanfilade direction): every graft
    /// made at or before `at` whose source overlaps entity ids `[lo, hi)`.
    pub fn copies_of(&self, lo: u64, hi: u64, at: Edition) -> Vec<GraftSpan> {
        self.inner.lock().unwrap().grafts.copies_of(lo, hi, at)
    }

    /// **Where a block was copied from** (the POOM direction): every graft made
    /// at or before `at` whose target overlaps entity ids `[lo, hi)`.
    pub fn sources_of(&self, lo: u64, hi: u64, at: Edition) -> Vec<GraftSpan> {
        self.inner.lock().unwrap().grafts.sources_of(lo, hi, at)
    }

    /// **The entity `e` started as**, following grafts back as of `at`, with
    /// the grafts that carried it, most recent first. See
    /// [`Spanfilade::origin`].
    pub fn origin_of(&self, e: Entity, at: Edition) -> (Entity, Vec<GraftSpan>) {
        let (start, chain) = self.inner.lock().unwrap().grafts.origin(e.0, at);
        (Entity(start), chain)
    }

    /// **Arrangements (G-9).** Ensure `rel` has an ordering led by column `col`,
    /// building it from the current primary order if this is its first use.
    fn ensure_order(inner: &mut Inner, rel: RelId, col: usize) {
        let Some(roots) = inner.roots(rel) else { return };
        if roots.orders.get(&(col as u32)).is_some() {
            return;
        }
        let mut roots = roots.clone();
        let mut arr = FactTree::new();
        if let Some(primary) = roots.versions.last_le(&inner.current) {
            for (k, v) in primary.1.iter() {
                if let Some(key) = rotate(&k, col) {
                    arr = arr.insert(key, *v);
                }
            }
        }
        roots.orders = roots.orders.insert(col as u32, arr);
        inner.put(rel, roots);
    }

    /// **Version-compare / backfollow (E6).** How `rel` differs between editions
    /// `a` and `b`, as `(tuple, weight_at_a, weight_at_b)` for every tuple whose
    /// net weight differs. An unchanged relation shares its Fact root across the
    /// two editions, so the comparison short-circuits in `O(1)` — the read side of
    /// the trace ("what is the same, what moved").
    ///
    /// This is the substrate-native form, with absent sides as `None`. It is
    /// reachable above the bright line through
    /// [`TraceStore::compare`](grmpl_core::TraceStore::compare), which is what
    /// puts it on the running system's path: `grmpl-diff` routes the non-linear
    /// `distinct` delta through it, so a maintained `distinct` costs the edit
    /// rather than the relation.
    pub fn version_compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<crate::tree::EntryDiff<Tuple, Diff>>> {
        let inner = self.inner.lock().unwrap();
        for at in [a, b] {
            if at.0 < inner.watermark {
                return Err(door("compare", at.0, inner.watermark));
            }
        }
        let root_at = |ed: u64| inner.fact_at(rel, ed).cloned().unwrap_or_default();
        Ok(root_at(a.0).diff(&root_at(b.0)))
    }

    /// **Version compare that names copies (Green's compare).** How `rel`
    /// differs between editions `a < b`, with every graft made in `(a, b]`
    /// reported as the span it copied rather than as its rows.
    ///
    /// The relation's log finds the grafts (each logs its target span), and the
    /// [`Spanfilade`] says where each came from. Each copy is spliced into `a`'s
    /// version from the source block of the edition before the graft: the same
    /// nodes the graft shared, at the same displacement. So the row comparison
    /// that follows recognizes every copied subtree as unchanged, and the whole
    /// call costs `O(log n)` per graft plus the edits around it, however large
    /// the copies are. [`compare`](TraceStore::compare) lists the copied rows.
    pub fn compare_spans(&self, rel: RelId, a: Edition, b: Edition) -> Result<SpanCompare> {
        let inner = self.inner.lock().unwrap();
        for at in [a, b] {
            if at.0 < inner.watermark {
                return Err(door("compare", at.0, inner.watermark));
            }
        }
        if a > b {
            return Err(Error::Store(format!("compare_spans: {a:?} is after {b:?}")));
        }
        let root_at = |ed: u64| inner.fact_at(rel, ed).cloned().unwrap_or_default();
        let mut base = root_at(a.0);
        let mut copies = Vec::new();
        if let Some(log) = inner.log_of(rel) {
            for ((g, _), entry) in log.range_collect(&(a.0 + 1, 0), &(b.0 + 1, 0)) {
                let LogEntry::Graft(tlo, thi) = entry else { continue };
                let Some(Value::Ent(Entity(to))) = tlo.as_slice().first() else {
                    return Err(Error::Store(format!("graft at edition {g} has no entity target")));
                };
                let copy = inner
                    .grafts
                    .sources_of(*to, to.wrapping_add(1), Edition(g))
                    .into_iter()
                    .find(|s| s.edition.0 == g && s.target.0 == *to && s.rels.contains(&rel))
                    .ok_or_else(|| Error::Store(format!("graft at edition {g} is not in the spanfilade")))?;
                let (slo, shi) =
                    (Tuple::from([Value::Ent(Entity(copy.source.0))]), Tuple::from([Value::Ent(Entity(copy.source.1))]));
                base = match inner.layout_of(rel) {
                    Layout::Ordered => {
                        let block = root_at(g - 1).split(&slo).1.split(&shi).0.relocate(copy.shift());
                        let (below, rest) = base.split(&tlo);
                        FactTree::join(&FactTree::join(&below, &block), &rest.split(&thi).1)
                    }
                    Layout::Kd => {
                        let block = root_at(g - 1).kd_split(&slo).1.kd_split(&shi).0.relocate(copy.shift());
                        let (below, rest) = base.kd_split(&tlo);
                        FactTree::kd_join_at(&FactTree::kd_join_at(&below, &tlo, &block), &thi, &rest.kd_split(&thi).1)
                    }
                };
                copies.push(copy);
            }
        }
        Ok(SpanCompare { copies, rows: base.diff(&root_at(b.0)) })
    }

    /// **Backfollow (Gold's `rangeTranscluders`, over versions).** Every
    /// version, on any branch of this world, whose Fact tree holds some of the
    /// content of `rel` at `at` in `[lo, hi)`, with where it holds it.
    ///
    /// As in Gold, the query walks south to the leaves holding the span, then
    /// climbs north from each leaf through the history index's O-parent sets to
    /// the roots above it, and from each root to the versions using it. A
    /// version a fork inherited is found through the DAG. `rows` counts the
    /// queried rows in the leaves a version shares.
    /// So this finds **shared nodes**, not equal values: a version that rebuilt
    /// a leaf around an edit holds that leaf's rows again only by value, and is
    /// not reported for them. Identity is sharing, as in Gold.
    ///
    /// Brings the history index up to date first, so the answer is exact.
    pub fn backfollow(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<Holding>> {
        let tree = {
            let inner = self.inner.lock().unwrap();
            if at.0 < inner.watermark {
                return Err(door("backfollow", at.0, inner.watermark));
            }
            inner.fact_at(rel, at.0).cloned().unwrap_or_default().normalized()
        };
        let (history, dag, branches) = self.caught_up();
        let mut found: BTreeMap<(Version, i64), usize> = BTreeMap::new();
        let mut memo = HashMap::new();
        let mut retained: HashMap<(Holder, ContentKey), Vec<Version>> = HashMap::new();
        for (ck, at_off, rows) in pieces(&tree, lo, hi) {
            // Consolidation folds several versions into one checkpoint, so two
            // holders can stand for one retained version: count it once.
            let mut here: BTreeSet<(Version, i64)> = BTreeSet::new();
            for (holder, off, root) in history.reach(ck, &mut memo) {
                let versions =
                    retained.entry((holder, root)).or_insert_with(|| versions_of(&dag, &branches, holder, &root));
                for version in versions.iter() {
                    here.insert((*version, off.wrapping_sub(at_off)));
                }
            }
            for key in here {
                *found.entry(key).or_insert(0) += rows;
            }
        }
        Ok(found.into_iter().map(|((version, shift), rows)| Holding { version, shift, rows }).collect())
    }

    /// **Identity compare (Gold's `sharedRegion` / `mapSharedTo`).** What
    /// version `a` shares with version `b`, wherever it sits: for each
    /// displacement at which some of `a`'s content appears in `b`, how many of
    /// `a`'s rows appear there. Sharing means the same nodes, as for
    /// [`backfollow`](Self::backfollow).
    ///
    /// Walks `a` from its root. A node is looked for in `b` by climbing the
    /// history index towards `b`'s root, admitting only nodes born on `b`'s
    /// lineage by `b`'s edition (Gold's `isLE:` pruning), and memoized across
    /// the walk. A node found is reported whole; one not found is opened.
    pub fn shared_region(&self, a: Version, b: Version) -> Result<Vec<(i64, usize)>> {
        let (history, dag, branches) = self.caught_up();
        let tree_at = |v: Version| -> Result<FactTree> {
            let state = branches
                .get(&v.branch)
                .ok_or_else(|| Error::Store(format!("unknown branch {}", v.branch)))?;
            if v.edition.0 < state.watermark {
                return Err(door("shared_region", v.edition.0, state.watermark));
            }
            Ok(state.fact_at(v.rel, v.edition.0).cloned().unwrap_or_default().normalized())
        };
        let (ta, tb) = (tree_at(a)?, tree_at(b)?);
        let mut out: BTreeMap<i64, usize> = BTreeMap::new();
        let (Some(_), Some(target)) = (content_key(&ta), content_key(&tb)) else { return Ok(Vec::new()) };
        let lineage = dag.lineage(b.branch, b.edition.0);
        let admit = |ck: &ContentKey| history.visible(ck, &lineage);
        let mut memo = HashMap::new();
        let mut stack = vec![(&ta, 0i64)];
        while let Some((t, parent_off)) = stack.pop() {
            let off = parent_off.wrapping_add(t.dsp());
            let ck = *t.ck_cell().and_then(|c| c.get()).expect("keyed above");
            if admit(&ck) {
                let at = history.offsets_in(ck, &target, &admit, &mut memo);
                if !at.is_empty() {
                    for o in at {
                        *out.entry(o.wrapping_sub(off)).or_insert(0) += t.len();
                    }
                    continue;
                }
            }
            if let Some(children) = t.node().map(|n| n.children()) {
                stack.extend(children.iter().map(|c| (c, off)));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// **The same answer as [`shared_region`](Self::shared_region), without
    /// the history index.** Collects every node key of `b` from its interior
    /// frames (a leaf's key is in its parent's frame, so leaves are not read),
    /// then walks `a` from its root, reporting each node found whole. It costs
    /// the two versions' interior nodes however long the history is, where the
    /// upward search costs every later version of a node it climbs from. Kept
    /// beside Gold's method to measure the two.
    pub fn shared_region_by_descent(&self, a: Version, b: Version) -> Result<Vec<(i64, usize)>> {
        let branches = self.family.root.lock().unwrap().branches.clone();
        let tree_at = |v: Version| -> Result<FactTree> {
            let state = branches
                .get(&v.branch)
                .ok_or_else(|| Error::Store(format!("unknown branch {}", v.branch)))?;
            if v.edition.0 < state.watermark {
                return Err(door("shared_region", v.edition.0, state.watermark));
            }
            Ok(state.fact_at(v.rel, v.edition.0).cloned().unwrap_or_default().normalized())
        };
        let (ta, tb) = (tree_at(a)?, tree_at(b)?);
        let mut out: BTreeMap<i64, usize> = BTreeMap::new();
        if content_key(&ta).is_none() || content_key(&tb).is_none() {
            return Ok(Vec::new());
        }
        let key = |t: &FactTree| *t.ck_cell().and_then(|c| c.get()).expect("keyed above");
        let mut in_b: HashMap<ContentKey, Vec<i64>> = HashMap::new();
        // A k-d tree's leaves sit at different depths, so its nodes are all
        // read; a B+ tree's leaves are named by their parents.
        let top = if tb.is_kd() { usize::MAX } else { height(&tb) };
        let mut stack = vec![(&tb, top, 0i64)];
        while let Some((t, h, parent_off)) = stack.pop() {
            let off = parent_off.wrapping_add(t.dsp());
            in_b.entry(key(t)).or_default().push(off);
            // Reading an interior node names its children, so a leaf (height
            // 1) is recorded from its parent's frame and never read.
            if h > 1 {
                if let Some(children) = t.node().map(|n| n.children()) {
                    for c in children {
                        if h == 2 {
                            in_b.entry(key(c)).or_default().push(off.wrapping_add(c.dsp()));
                        } else {
                            stack.push((c, h - 1, off));
                        }
                    }
                }
            }
        }
        let mut stack = vec![(&ta, 0i64)];
        while let Some((t, parent_off)) = stack.pop() {
            let off = parent_off.wrapping_add(t.dsp());
            if let Some(offs) = in_b.get(&key(t)) {
                let mut offs = offs.clone();
                offs.sort_unstable();
                offs.dedup();
                for o in offs {
                    *out.entry(o.wrapping_sub(off)).or_insert(0) += t.len();
                }
                continue;
            }
            if let Some(children) = t.node().map(|n| n.children()) {
                stack.extend(children.iter().map(|c| (c, off)));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// **Merge (Gold's `newSuccessorAfter:`, by replay).** A new branch that
    /// starts from this store's present and replays, in order, every patch of
    /// `other` that has not already flowed into this store, re-checking each
    /// one's preconditions against the merging state, as a racing commit would
    /// be. A graft re-runs its own checks. The new branch's DAG record has
    /// both parents, so everything behind either store is behind it.
    ///
    /// Which patches to replay is the difference of the two lineages: for each
    /// branch in `other`'s history, its own editions past what this store's
    /// history already holds of it. So a patch that reached this store through
    /// an earlier merge is not replayed again, and merging an ancestor replays
    /// nothing. Patches go branch by branch in id order (an ancestor before its
    /// descendants), each branch's in edition order, and each becomes one
    /// edition of the new branch.
    ///
    /// **All or nothing**: if any replayed patch fails, nothing is created and
    /// the conflict names it. The context enfilades (catalog and schemas) are
    /// united; a key the two sides bind differently is a conflict too. A patch
    /// below its branch's watermark has been consolidated away and cannot be
    /// replayed, which is an error.
    pub fn merge(&self, other: &EntStore) -> Result<MergeOutcome> {
        if !Arc::ptr_eq(&self.family, &other.family) {
            return Err(Error::Store("merge: the stores are not branches of one world".into()));
        }
        // Two edition locks, always in branch order, so merges cannot deadlock.
        let (mine, theirs);
        let (a, b) = if self.branch <= other.branch { (self, other) } else { (other, self) };
        let ga = a.inner.lock().unwrap();
        let gb = if a.branch == b.branch { None } else { Some(b.inner.lock().unwrap()) };
        if self.branch <= other.branch {
            mine = ga.clone();
            theirs = gb.as_ref().map_or_else(|| ga.clone(), |g| (*g).clone());
        } else {
            theirs = ga.clone();
            mine = gb.as_ref().map_or_else(|| ga.clone(), |g| (*g).clone());
        }
        let mut root = self.family.root.lock().unwrap();
        let state_of = |br: BranchId| -> Option<Inner> {
            if br == self.branch {
                Some(mine.clone())
            } else if br == other.branch {
                Some(theirs.clone())
            } else {
                root.branches.get(&br).cloned()
            }
        };
        // The patches of `other`'s history this store's history lacks.
        let have: BTreeMap<BranchId, u64> = root.dag.lineage(self.branch, mine.current).into_iter().collect();
        let theirs_line: BTreeMap<BranchId, u64> =
            root.dag.lineage(other.branch, theirs.current).into_iter().collect();
        let reaches = |line: &BTreeMap<BranchId, u64>, (b, e): (BranchId, u64)| line.get(&b).is_some_and(|x| *x >= e);
        // Patches this store already holds as copies, by their originals: a
        // fork taken partway through a merge holds copies of patches whose
        // branch is not in its lineage. Copies live only in merge branches'
        // own logs.
        let mut copied: BTreeSet<(BranchId, u64)> = BTreeSet::new();
        for (&br, &bound) in &have {
            if root.dag.get(br).is_some_and(|b| b.merged.is_some()) {
                let state = state_of(br).ok_or_else(|| Error::Store(format!("merge: branch {br} has no state")))?;
                let start = root.dag.parent(br).map_or(0, |(_, at)| at);
                for (_, patch) in state.patches.range_collect(&(start + 1), &(bound + 1)) {
                    copied.extend(patch.origin());
                }
            }
        }
        let here = |o: (BranchId, u64)| reaches(&have, o) || copied.contains(&o);
        let mut todo: Vec<(BranchId, u64, PatchRecord, Inner)> = Vec::new();
        for (&br, &bound) in &theirs_line {
            let start = root.dag.parent(br).map_or(0, |(_, at)| at);
            let from = have.get(&br).copied().unwrap_or(0).max(start);
            if bound <= from {
                continue;
            }
            let state = state_of(br).ok_or_else(|| Error::Store(format!("merge: branch {br} has no state")))?;
            // Every edition of a branch's own is a patch, so one at or below
            // its watermark has been consolidated away.
            if from < state.watermark {
                return Err(door("merge", from + 1, state.watermark));
            }
            for (e, patch) in state.patches.range_collect(&(from + 1), &(bound + 1)) {
                // A copy a merge made: its original is replayed from its own
                // branch, or is already here. Only an unreachable original's
                // copy stands in for it.
                let original = patch.origin().unwrap_or((br, e));
                if here(original) || (patch.origin().is_some() && reaches(&theirs_line, original)) {
                    continue;
                }
                todo.push((br, e, patch, state.clone()));
            }
        }
        todo.sort_by_key(|(br, e, _, _)| (*br, *e));
        // The merging state: this store's present, as a fork would start.
        let mut merged = Inner {
            canopy: Canopy::new(),
            fired: FiredTree::new(),
            interests: Tree::new(),
            registered: Tree::new(),
            ..mine.clone()
        };
        let conflict = |br: BranchId, e: u64, reason: String| {
            Ok(MergeOutcome::Conflict(MergeConflict { branch: br, edition: Edition(e), reason }))
        };
        // Catalog and schemas: united, refusing a key bound two ways.
        for (k, v) in theirs.ctx.iter() {
            match merged.ctx.get(&k) {
                Some(mv) if mv != v => {
                    return conflict(other.branch, theirs.current, format!("context key {k:?} is bound differently"));
                }
                Some(_) => {}
                None => merged.ctx = merged.ctx.insert(k, v.clone()),
            }
        }
        // Layouts: united too, so a relation the other side laid out keeps its
        // shape here.
        for (rel, l) in theirs.layouts.iter() {
            match merged.layouts.get(&rel) {
                Some(ml) if *ml != *l => {
                    return conflict(other.branch, theirs.current, format!("relation {rel} is laid out differently"));
                }
                Some(_) => {}
                None if merged.layout_of(RelId(rel)) != *l => merged.layouts = merged.layouts.insert(rel, *l),
                None => {}
            }
        }
        for (br, e, patch, state) in todo {
            // A copy keeps naming the true original.
            let origin = Some(patch.origin().unwrap_or((br, e)));
            match patch {
                PatchRecord::Commit { pre, rels, .. } => {
                    if let Some((rel, t)) = pre.iter().find(|(rel, t)| !merged.holds_now(*rel, t)) {
                        return conflict(br, e, format!("precondition {t:?} in relation {} no longer holds", rel.0));
                    }
                    let updates = patch_updates(&state, e, &rels);
                    let at = merged.current + 1;
                    merged.apply(at, &pre, &updates, origin);
                }
                PatchRecord::Graft { rels, block, shift, .. } => {
                    let rels: Vec<RelId> = rels.into_iter().map(RelId).collect();
                    if let Err(err) = merged.graft(&rels, block.0, block.1, shift, origin) {
                        return conflict(br, e, err.to_string());
                    }
                }
            }
        }
        let branch = root.dag.merge(self.branch, mine.current, other.branch, theirs.current, merged.current);
        let seq = self.stage_in(&mut root, branch, &merged)?;
        drop(root);
        drop(gb);
        drop(ga);
        self.await_durable(seq)?;
        Ok(MergeOutcome::Merged(EntStore::on(Arc::clone(&self.family), branch, merged)))
    }

    /// **Run the deferred history work** (Gold's Agenda, for the one job grmpl
    /// has so far): index at most `budget` versions not yet in the history
    /// index, and make the progress durable. Queries catch the index up on
    /// their own, so this only moves the work off their path. Returns the
    /// versions indexed.
    pub fn step_history(&self, budget: usize) -> Result<usize> {
        let inner = self.inner.lock().unwrap();
        let (done, seq) = {
            let mut root = self.family.root.lock().unwrap();
            let done = root.catch_up(budget);
            (done, self.stage_in(&mut root, self.branch, &inner)?)
        };
        drop(inner);
        self.await_durable(seq)?;
        Ok(done)
    }

    /// Versions written but not yet in the history index.
    pub fn history_backlog(&self) -> usize {
        self.family.root.lock().unwrap().backlog()
    }

    /// Entries in the history index: `(parent edges, root holders, births)`.
    pub fn history_size(&self) -> (usize, usize, usize) {
        self.family.root.lock().unwrap().history.sizes()
    }

    /// Catch the history index up, and take what a query reads from the root.
    fn caught_up(&self) -> (History, Dag, BranchStates) {
        let mut root = self.family.root.lock().unwrap();
        root.catch_up(usize::MAX);
        (root.history.clone(), root.dag.clone(), root.branches.clone())
    }

    /// Node frames serialized+hashed since this store was opened — the G-0a ops
    /// counter, surfaced so tests can assert the commit path stays path-sized.
    /// `0` for an in-memory store.
    pub fn frames_encoded(&self) -> u64 {
        self.family.gran.as_ref().map_or(0, |g| g.frames_encoded())
    }

    /// Bytes in the frames [`frames_encoded`](Self::frames_encoded) counts.
    /// `0` for an in-memory store.
    pub fn bytes_encoded(&self) -> u64 {
        self.family.gran.as_ref().map_or(0, |g| g.bytes_encoded())
    }

    /// Node frames read from disk since this store was opened — the paging ops
    /// counter, surfaced so tests can assert that opening a world and reading a
    /// little of it reads a little. `0` for an in-memory store.
    pub fn frames_paged(&self) -> u64 {
        self.family.gran.as_ref().map_or(0, |g| g.frames_paged())
    }

    /// **The durable frontier**: the highest edition proven on disk.
    ///
    /// Always `<= current()`, and equal to it whenever no commit is in flight.
    /// A commit does not return until its own edition is durable, so a caller
    /// never needs this to trust an edition it was handed; it is for an observer
    /// that reads the world *without* committing and externalizes what it saw
    /// (streaming to a socket, say) and wants to send only what a crash could not
    /// take back. Group commit is the only reason the two can differ, and they
    /// differ only for the length of one `fsync`.
    pub fn durable_edition(&self) -> Edition {
        if self.family.gran.is_none() {
            return self.current();
        }
        Edition(self.family.dur.lock().unwrap().durable.get(&self.branch).copied().unwrap_or(0))
    }

    /// Count one edition-lock acquisition on a pinned-edition read path.
    fn note_read_lock(&self) {
        self.read_locks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Times a pinned-edition read has taken the edition lock since this store
    /// was opened — the reader ops counter. A `Snapshot` costs **one** (acquiring
    /// its reader) however many relations its queries touch.
    pub fn read_locks(&self) -> u64 {
        self.read_locks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `SyncAll`s issued since this store was opened — the group-commit ops
    /// counter, surfaced so tests can assert that concurrent committers *share*
    /// their durability cost rather than each paying it. `0` for an in-memory
    /// store.
    pub fn syncs(&self) -> u64 {
        self.family.gran.as_ref().map_or(0, |g| g.syncs())
    }

    /// Distinct nodes currently stored in the granfilade — the on-disk size of
    /// the world in nodes, for measuring how history accumulates and what GC
    /// reclaims. `0` for an in-memory store.
    pub fn stored_nodes(&self) -> Result<usize> {
        match &self.family.gran {
            Some(g) => g.node_count(),
            None => Ok(0),
        }
    }

    /// **Reachability GC (E3).** Collect granfilade nodes no longer reachable
    /// from a live enfilade root (accumulated as commits path-copy and
    /// `consolidate` truncates). Serialized with commits (holds the edition
    /// lock). A no-op in-memory. Returns the number of nodes collected.
    pub fn gc(&self) -> Result<usize> {
        // Hold the root so no branch stages a new one mid-sweep, and land what
        // is staged: reachability is computed from the root record on disk.
        let _root = self.family.root.lock().unwrap();
        self.flush_pending()?;
        match &self.family.gran {
            Some(g) => g.gc(),
            None => Ok(0),
        }
    }

    /// **Structural-sharing fork (E3, made durable in G-6).** A new independent
    /// store whose state is this store's as-of `at`, **sharing every enfilade
    /// node** with the parent. Forking at the present shares the whole Rel
    /// enfilade; forking into the past splits each relation's versions and log
    /// at `at` (`O(log n)` new nodes apiece). Either way the child's state joins
    /// the branch enfilade of the *same* root, so the only frames written are
    /// the new spines of the DAG and branch enfilades and of any split — the
    /// cheap virtual copy at the heart of the Ent, where the LSM must copy
    /// `O(state)` bytes.
    ///
    /// The fork is a new branch in the shared DagWood, so ancestry stays
    /// queryable across the whole family, and it survives a reopen
    /// ([`open_branch`](Self::open_branch)).
    pub fn fork_at(&self, at: Edition) -> Result<EntStore> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("fork_at", at.0, inner.watermark));
        }
        // Land the parent's queue first, so every node the child names is
        // already durable and the fork encodes directory nodes only.
        self.flush_pending()?;
        let rels = if at.0 == inner.current {
            // Forking at the present shares the whole Rel enfilade.
            inner.rels.clone()
        } else {
            let mut rels = RelTree::new();
            for (rel, roots) in inner.rels.iter() {
                // A persistent split: `O(log n)` new nodes, every root shared.
                let (versions, _) = roots.versions.split(&(at.0 + 1));
                let (log, _) = roots.log.split(&(at.0 + 1, 0));
                if !versions.is_empty() || !log.is_empty() {
                    // Arrangements track the present; a fork into the past
                    // rebuilds them on demand.
                    rels = rels.insert(rel, RelRoots { versions, log, orders: OrderTree::new() });
                }
            }
            rels
        };
        // The spanfilade is keyed by span, not edition: forking into the past
        // keeps the grafts made by then.
        let grafts = if at.0 == inner.current { inner.grafts.clone() } else { inner.grafts.as_of(at) };
        let child = Inner {
            current: at.0,
            watermark: inner.watermark,
            rels,
            ctx: inner.ctx.clone(),
            canopy: Canopy::new(),
            fired: FiredTree::new(),
            interests: Tree::new(),
            registered: Tree::new(),
            grafts,
            // The child's patch log starts empty: what it inherited is its
            // parent's, reached through the DAG.
            patches: PatchTree::new(),
            layout: inner.layout,
            layouts: inner.layouts.clone(),
        };
        // Graft a new branch onto this one at the fork edition in the shared
        // DagWood and stage its state, under one root lock so no root record
        // ever names the branch without its state.
        let (branch, seq) = {
            let mut root = self.family.root.lock().unwrap();
            let branch = root.dag.fork(self.branch, at.0);
            let seq = self.stage_in(&mut root, branch, &child)?;
            (branch, seq)
        };
        drop(inner);
        self.await_durable(seq)?;
        Ok(EntStore::on(Arc::clone(&self.family), branch, child))
    }

    /// A handle on another **branch of this same world**, sharing its
    /// granfilade and root.
    ///
    /// Prefer this over [`open_branch`](Self::open_branch) whenever the parent is
    /// still live: a granfilade takes an exclusive lock on its directory, so two
    /// branches of one world are two handles onto *one* granfilade, never two
    /// opens of the same path. That is the same constraint that makes all
    /// branches sharing one node store the right design in the first place.
    pub fn branch(&self, branch: BranchId) -> Result<EntStore> {
        let inner = self.family.root.lock().unwrap().state(branch)?;
        Ok(EntStore::on(Arc::clone(&self.family), branch, inner))
    }

    /// This store's branch in the fulltrace's DagWood ([`Dag::ROOT`] unless it is
    /// a fork).
    pub fn branch_id(&self) -> BranchId {
        self.branch
    }

    /// A snapshot of the branch DAG shared with this store's fork family — the
    /// fulltrace's branch structure (Xanadu's `DagWood`).
    pub fn dag(&self) -> Dag {
        self.family.root.lock().unwrap().dag.clone()
    }

    /// **Backfollow across branches (E3).** Does this store's current point
    /// descend from `ancestor`'s point as-of `ancestor_at` — i.e. did that history
    /// flow into this branch? Forks share the DagWood, so this answers across the
    /// whole family; two stores that never shared a fork return `false` (disjoint
    /// DagWoods). Reflexive on the same branch (earlier editions are ancestors).
    pub fn descends_from(&self, ancestor: &EntStore, ancestor_at: Edition) -> bool {
        if !Arc::ptr_eq(&self.family, &ancestor.family) {
            return false;
        }
        let here = self.inner.lock().unwrap().current;
        self.dag().is_ancestor(ancestor.branch, ancestor_at.0, self.branch, here)
    }

    /// The merge base of this store and `other` in the shared DagWood — the latest
    /// `(branch, edition)` their histories both descend from, or `None` if they
    /// belong to disjoint DagWoods.
    pub fn common_ancestor_with(&self, other: &EntStore) -> Option<(BranchId, u64)> {
        if !Arc::ptr_eq(&self.family, &other.family) {
            return None;
        }
        let here = self.inner.lock().unwrap().current;
        let there = other.inner.lock().unwrap().current;
        self.dag().common_ancestor(self.branch, here, other.branch, there)
    }

    /// **DSP template instancing (E6): a virtual copy.** Copy every fact of
    /// `rels` whose lead entity lies in the template block `[block_lo,
    /// block_hi)` into the block displaced by `shift`, moving **all** of each
    /// fact's entity coordinates together, as one new edition: a private,
    /// independently-mutable copy of the template sub-world, its rooms, exits
    /// and items renamed only in *coordinate* (their text and weights
    /// preserved). Returns the new edition.
    ///
    /// Each relation's block is [grafted](Tree::graft): split out of the Fact
    /// enfilade, relocated by one dsp, and joined back in at its new position.
    /// The instance shares every interior node with the template — the commit
    /// writes `O(log n)` new nodes per relation and one log entry, however large
    /// the template is — and diverges copy-on-write as either side is edited.
    /// Readers still see the copy as ordinary updates: [`scan_updates`]
    /// expands the log entry from the edition's Fact root.
    ///
    /// Distinct `shift`s give disjoint instances, so N players can each `enter`
    /// the same template into their own block. The target block must be empty
    /// in every relation, or the call fails without committing anything; it also
    /// fails if the shift would wrap an entity id.
    ///
    /// The template must be self-contained: every entity cell of every fact in
    /// the block lies in the block, so the relocation keeps it internally
    /// connected. This is checked, not assumed — the [`Extent`] of the block's
    /// span says it in `O(log n)` — and a template that names an outside entity
    /// is refused.
    ///
    /// The graft is recorded in the branch's [`Spanfilade`], so
    /// [`copies_of`](Self::copies_of) finds the instances of a template and
    /// [`origin_of`](Self::origin_of) the template of an instance.
    ///
    /// [`scan_updates`]: TraceStore::scan_updates
    pub fn instance_template(&self, rels: &[RelId], block_lo: u64, block_hi: u64, shift: i64) -> Result<Edition> {
        let seq = {
            let mut inner = self.inner.lock().unwrap();
            let e = inner.graft(rels, block_lo, block_hi, shift, None)?;
            (self.stage(&inner)?, e)
        };
        self.await_durable(seq.0)?;
        Ok(Edition(seq.1))
    }

    /// Stage this branch's state: put it in the branch enfilade and, on a
    /// durable store, encode the new root record and queue it for the group.
    /// Returns the staging sequence number to [await](Self::await_durable).
    ///
    /// **Called under the edition lock**, so a branch's stages queue in edition
    /// order; the root lock orders them against every other branch's.
    fn stage(&self, inner: &Inner) -> Result<Option<u64>> {
        let mut root = self.family.root.lock().unwrap();
        self.stage_in(&mut root, self.branch, inner)
    }

    /// [`stage`](Self::stage) for `branch`, with the root already locked.
    ///
    /// Encoding is pure with respect to the store (it only reads immutable
    /// trees), and it is path-sized: every subtree a previous stage wrote is
    /// memoized and durable, so only the copied spines — the edited Fact and
    /// log paths, the directories above them, this branch's entry in the
    /// branch enfilade — are serialized.
    fn stage_in(&self, root: &mut MutexGuard<'_, EntRoot>, branch: BranchId, inner: &Inner) -> Result<Option<u64>> {
        root.branches = root.branches.insert(branch, inner.clone());
        let Some(gran) = &self.family.gran else { return Ok(None) };
        let (dag, mut nodes) = gran.collect_tree(root.dag.tree());
        let (branches, more) = gran.collect_tree(&root.branches);
        nodes.extend(more);
        let (parents, holders, born, cursor) = root.history.trees();
        let mut slots = vec![dag, branches];
        for (ck, more) in [
            gran.collect_tree(parents),
            gran.collect_tree(holders),
            gran.collect_tree(born),
            gran.collect_tree(cursor),
        ] {
            slots.push(ck);
            nodes.extend(more);
        }
        let mut d = self.family.dur.lock().unwrap();
        d.staged += 1;
        let seq = d.staged;
        d.pending.push_back(Pending {
            seq,
            branch,
            edition: inner.current,
            write: StagedWrite { nodes, root: slots },
        });
        Ok(Some(seq))
    }

    // -----------------------------------------------------------------------
    // Group commit
    // -----------------------------------------------------------------------

    /// Wait until stage `seq` is durable, joining or leading a group along the
    /// way. **Must not be called holding the edition lock** on the commit path
    /// — that is the serialization this exists to remove. `None` (an in-memory
    /// store) returns at once.
    fn await_durable(&self, seq: Option<u64>) -> Result<()> {
        match seq {
            Some(s) => self.drive_durability(Some(s)),
            None => Ok(()),
        }
    }

    /// Drive every staged edition to disk and return once nothing is pending.
    ///
    /// Safe to call while holding the edition or root lock: the flush touches
    /// only the granfilade and the durability queue. Locks are always taken in
    /// the order edition → root → durability, so the order is total.
    fn flush_pending(&self) -> Result<()> {
        self.drive_durability(None)
    }

    /// The group-commit loop. `Some(s)` waits for stage `s` to be durable;
    /// `None` waits for the queue to drain.
    ///
    /// A thread either **leads** — takes everything staged so far, writes it as
    /// one batch + one `SyncAll`, and wakes the rest — or **follows**, waiting on
    /// the condvar for the leader's group to land. Which role it plays is
    /// whichever is free, so there is no dedicated writer thread and no handoff
    /// latency when there is no contention (a lone committer simply leads its own
    /// group of one, exactly as before).
    fn drive_durability(&self, target: Option<u64>) -> Result<()> {
        let fam = &*self.family;
        let gran = match &fam.gran {
            Some(g) => g,
            None => return Ok(()),
        };
        let mut d = fam.dur.lock().unwrap();
        loop {
            if let Some(msg) = d.doom(target) {
                return Err(Error::Store(msg));
            }
            let done = match target {
                Some(s) => d.landed >= s,
                None => d.pending.is_empty() && !d.writing,
            };
            if done {
                return Ok(());
            }
            if d.writing {
                // Someone else is inside the fsync; our edition may be in their
                // group. Wait for it to land and re-check.
                d = fam.flushed.wait(d).unwrap();
                continue;
            }
            // Lead: take the whole queue. Later stages arriving mid-write simply
            // form the next group.
            let group: Vec<Pending> = d.pending.drain(..).collect();
            let Some(hi) = group.last().map(|p| p.seq) else {
                return Err(Error::Store(format!(
                    "group commit: stage {target:?} is neither pending nor durable \
                     (landed={})",
                    d.landed
                )));
            };
            let editions: Vec<(BranchId, u64)> = group.iter().map(|p| (p.branch, p.edition)).collect();
            d.writing = true;
            drop(d);

            let res = gran.write_group(group.into_iter().map(|p| p.write).collect());

            d = fam.dur.lock().unwrap();
            d.writing = false;
            match res {
                Ok(()) => {
                    d.landed = d.landed.max(hi);
                    for (branch, edition) in editions {
                        let e = d.durable.entry(branch).or_insert(0);
                        *e = (*e).max(edition);
                    }
                }
                Err(err) => {
                    // The group is gone from `pending` and never reached disk.
                    // Record it so its members error instead of waiting forever.
                    d.failure = Some((hi, format!("{err:?}")));
                    fam.flushed.notify_all();
                    return Err(err);
                }
            }
            fam.flushed.notify_all();
        }
    }
}

impl Inner {
    fn empty() -> Inner {
        Inner {
            current: 0,
            watermark: 0,
            rels: RelTree::new(),
            ctx: ContextEnf::new(),
            canopy: Canopy::new(),
            fired: FiredTree::new(),
            interests: Tree::new(),
            registered: Tree::new(),
            grafts: Spanfilade::new(),
            patches: PatchTree::new(),
            layout: Layout::default(),
            layouts: LayoutTree::new(),
        }
    }

    /// Fold one update into the Fact enfilade at edition `e`: build a fresh root
    /// from the latest root ≤ `e`, netting the tuple's weight; drop it at 0.
    fn fold_fact(&mut self, e: u64, rel: RelId, tuple: &Tuple, diff: Diff) {
        let mut roots = self.roots(rel).cloned().unwrap_or_default();
        let base = roots.versions.last_le(&e).map(|(_, t)| t.clone()).unwrap_or_default();
        let cur = base.get(tuple).copied().unwrap_or(0);
        let net = cur + diff;
        let root = match (self.layout_of(rel), net) {
            (Layout::Ordered, 0) => base.remove(tuple),
            (Layout::Ordered, _) => base.insert(tuple.clone(), net),
            (Layout::Kd, 0) => base.kd_remove(tuple),
            (Layout::Kd, _) => base.kd_insert(tuple.clone(), net),
        };
        roots.versions = roots.versions.insert(e, root);
        // Keep every existing Arrangement in step with the primary order.
        let cols: Vec<u32> = roots.orders.iter().map(|(c, _)| c).collect();
        for col in cols {
            let arr = roots.orders.get(&col).cloned().unwrap_or_default();
            if let Some(key) = rotate(tuple, col as usize) {
                let next = if net == 0 { arr.remove(&key) } else { arr.insert(key, net) };
                roots.orders = roots.orders.insert(col, next);
            }
        }
        self.put(rel, roots);
    }

    /// Apply `updates` as edition `e`: append to each Edition enfilade in submit
    /// order and fold each into the Fact enfilade.
    /// Stab the canopy with this commit's updates and record which interests it
    /// touched — the routing work, done once, when the change lands.
    fn route(&mut self, e: u64, updates: &[(RelId, Tuple, Diff)]) {
        if self.canopy.is_empty() {
            return;
        }
        for id in self.canopy.route(updates) {
            self.fired = self.fired.insert((id.0, e), ());
        }
    }

    /// Apply a commit as edition `e`, recording it in the patch log (with the
    /// original it was replayed from, for a merge).
    fn apply(&mut self, e: u64, pre: &[(RelId, Tuple)], updates: &[(RelId, Tuple, Diff)], origin: Origin) {
        let mut rels: Vec<u32> = updates.iter().map(|(r, _, _)| r.0).collect();
        rels.sort_unstable();
        rels.dedup();
        self.patches = self.patches.insert(e, PatchRecord::Commit { pre: pre.to_vec(), rels, origin });
        for (i, (rel, tuple, diff)) in updates.iter().enumerate() {
            let mut roots = self.roots(*rel).cloned().unwrap_or_default();
            roots.log = roots.log.insert((e, i as u64), LogEntry::Update(tuple.clone(), *diff));
            self.put(*rel, roots);
            self.fold_fact(e, *rel, tuple, *diff);
        }
        self.route(e, updates);
        self.current = e;
    }

    /// Commit `grafts` — each relation's new Fact root — as edition `e`, every
    /// copy landing in `[lo, hi)`: one Graft entry in each relation's log, and
    /// the canopy stabbed with the span rather than row by row.
    fn apply_grafts(&mut self, e: u64, grafts: &[(RelId, FactTree)], lo: &Tuple, hi: &Tuple) {
        for (rel, root) in grafts {
            let mut roots = self.roots(*rel).cloned().unwrap_or_default();
            roots.versions = roots.versions.insert(e, root.clone());
            roots.log = roots.log.insert((e, 0), LogEntry::Graft(lo.clone(), hi.clone()));
            // An Arrangement orders by another column, where the copy is not one
            // contiguous span; it is rebuilt on demand from the new primary.
            roots.orders = OrderTree::new();
            self.put(*rel, roots);
            if !self.canopy.is_empty() {
                for id in self.canopy.route_span(*rel, lo, hi) {
                    self.fired = self.fired.insert((id.0, e), ());
                }
            }
        }
        self.current = e;
    }

    /// **The graft behind [`EntStore::instance_template`]**, as the next
    /// edition: refused, changing nothing, if a template fact names an entity
    /// outside the block or the target block is occupied. Recorded in the
    /// spanfilade and the patch log. Returns the edition.
    fn graft(&mut self, rels: &[RelId], block_lo: u64, block_hi: u64, shift: i64, origin: Origin) -> Result<u64> {
        let src_lo = Tuple::from([Value::Ent(Entity(block_lo))]);
        let src_hi = Tuple::from([Value::Ent(Entity(block_hi))]);
        let (tlo, thi) = (src_lo.displace(shift), src_hi.displace(shift));
        let mut rels = rels.to_vec();
        rels.sort();
        rels.dedup();
        let at = self.current;
        let mut grafts = Vec::new();
        for rel in &rels {
            let Some(facts) = self.fact_at(*rel, at) else { continue };
            if !facts.any_in(&src_lo, &src_hi) {
                continue;
            }
            // A graft moves every entity cell, so a template fact naming an
            // entity outside the block would land pointing at the wrong one.
            // The span's extent proves it does not, reading two spines.
            if !facts.measure_range(&src_lo, &src_hi).1.within(block_lo, block_hi) {
                return Err(Error::Store(format!(
                    "instance_template: relation {} has template facts naming entities outside \
                     [{block_lo}, {block_hi}); a graft would move them too",
                    rel.0
                )));
            }
            let grafted = match self.layout_of(*rel) {
                Layout::Ordered => facts.graft(&src_lo, &src_hi, shift),
                Layout::Kd => facts.kd_graft(&src_lo, &src_hi, shift),
            };
            let grafted = grafted.ok_or_else(|| {
                Error::Store(format!(
                    "instance_template: relation {} already has facts in the target block \
                     [{block_lo}+{shift}, {block_hi}+{shift}), or the shift wraps the id space",
                    rel.0
                ))
            })?;
            grafts.push((*rel, grafted));
        }
        self.apply_grafts(at + 1, &grafts, &tlo, &thi);
        if !grafts.is_empty() {
            let to = block_lo.wrapping_add(shift as u64);
            self.grafts.record(&GraftSpan {
                source: (block_lo, block_hi),
                target: (to, to.wrapping_add(block_hi - block_lo)),
                edition: Edition(at + 1),
                rels: grafts.iter().map(|(rel, _)| *rel).collect(),
            });
        }
        self.patches = self.patches.insert(
            at + 1,
            PatchRecord::Graft { rels: rels.iter().map(|r| r.0).collect(), block: (block_lo, block_hi), shift, origin },
        );
        Ok(at + 1)
    }

    fn holds_now(&self, rel: RelId, tuple: &Tuple) -> bool {
        self.fact_at(rel, self.current).and_then(|t| t.get(tuple)).is_some_and(|n| *n > 0)
    }
}

/// **The Ent's lock-free reader.**
///
/// `Inner` sits behind one mutex, so before this every read of a pinned edition
/// took that mutex — and could block behind a committer inside its `fsync`. But
/// the Fact enfilade is *immutable and versioned by edition*: a commit inserts a
/// new root beside the old one and never edits it. That is the same property
/// that makes `fork_at` free and that G-2's persist fix turns on, and it means a
/// reader needs nothing from the store after it has the roots.
///
/// So this captures the Rel enfilade's root — **one `Arc` bump**, the whole
/// relation directory, under one brief lock — and answers every later read by
/// descending it. No lock, no contention with committers, and real snapshot
/// isolation: the reader keeps reading its edition no matter how far the store
/// moves on, because the version it holds cannot change.
struct EntReader<'a> {
    /// The store, for the one read that cannot be lock-free (see
    /// [`EntReader::read_range_on`]).
    store: &'a EntStore,
    /// The Rel enfilade as of construction: relation → its versioned Fact roots.
    rels: RelTree,
    at: u64,
    watermark: u64,
}

impl EntReader<'_> {
    /// The Fact root in force at this reader's edition — the same `O(log n)`
    /// descent `Inner::fact_at` makes, over the captured directory.
    fn facts(&self, rel: RelId) -> Option<&FactTree> {
        self.rels.get(&rel.0).and_then(|r| r.versions.last_le(&self.at)).map(|(_, t)| t)
    }

    fn door(&self, op: &str) -> Result<()> {
        if self.at < self.watermark {
            return Err(door(op, self.at, self.watermark));
        }
        Ok(())
    }
}

impl grmpl_core::EditionReader for EntReader<'_> {
    fn edition(&self) -> Edition {
        Edition(self.at)
    }

    fn read(&self, rel: RelId) -> Result<Vec<(Tuple, Diff)>> {
        self.door("read_at")?;
        Ok(self
            .facts(rel)
            .map(|t| t.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default())
    }

    fn read_range(&self, rel: RelId, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.door("read_range")?;
        // The WID range walk (E2), pruning out-of-range subtrees by their cached
        // measures — unchanged except that it needs no lock to do it.
        Ok(self.facts(rel).map(|t| t.range_collect(lo, hi)).unwrap_or_default())
    }

    fn read_range_on(
        &self,
        rel: RelId,
        col: usize,
        lo: &Value,
        hi: &Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        // **The one read that keeps the lock, and why.** An Arrangement is built
        // on first use and maintained thereafter, so answering this can *write*
        // to the store's state — which a reader over a captured immutable root
        // cannot do. Delegating keeps the build-on-first-use behavior intact at
        // the cost of one lock per `RangeRelOn` node, exactly as before. Every
        // other read in a plan is lock-free.
        self.store.read_range_on(rel, Edition(self.at), col, lo, hi)
    }

    fn read_containing(&self, rel: RelId, first: usize, last: usize, points: &[Value]) -> Result<Vec<(Tuple, Diff)>> {
        self.door("read_containing")?;
        let Some(facts) = self.facts(rel) else { return Ok(Vec::new()) };
        Ok(stab(facts, first, last, points).unwrap_or_else(|| {
            facts
                .iter()
                .filter(|(k, _)| grmpl_core::store::contains_any(k, first, last, points))
                .map(|(k, v)| (k, *v))
                .collect()
        }))
    }

    /// Answered from the store's Edition enfilade, a measure over `(from, at]`.
    fn touched_since(&self, from: Edition, rels: &[RelId]) -> Result<bool> {
        if from.0 >= self.at {
            return Ok(false);
        }
        self.store.touched_since(from, Edition(self.at), rels)
    }
}

impl EditionStore for EntStore {
    /// The world's clock: the **allocated** edition.
    ///
    /// This is the state `commit_if` validates preconditions against, so it must
    /// also be the state reads see — see [`Durable`] for why reporting the
    /// durability watermark here livelocks every guarded read-modify-write.
    /// [`EntStore::durable_edition`] is the on-disk frontier.
    fn current(&self) -> Edition {
        Edition(self.inner.lock().unwrap().current)
    }
}

impl TraceStore for EntStore {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        let (seq, e) = {
            // The short critical section: allocate the edition, apply in memory,
            // encode. No `fsync` is held here, so the next committer may enter as
            // soon as this one's work is staged.
            let mut inner = self.inner.lock().unwrap();
            let e = inner.current + 1;
            inner.apply(e, &[], updates, None);
            (self.stage(&inner)?, e)
        };
        // Durability, shared with everyone else staged behind us. Returning only
        // once `e` is durable is what makes the returned edition safe to act on.
        self.await_durable(seq)?;
        Ok(Edition(e))
    }

    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        let (seq, e) = {
            let mut inner = self.inner.lock().unwrap();
            // Preconditions are checked against the *allocated* state, not the
            // durable one: two committers in the same group must still serialize
            // against each other, or both could win a contested precondition.
            for (rel, tuple) in preconditions {
                if !inner.holds_now(*rel, tuple) {
                    return Ok(None);
                }
            }
            let e = inner.current + 1;
            inner.apply(e, preconditions, updates, None);
            (self.stage(&inner)?, e)
        };
        self.await_durable(seq)?;
        Ok(Some(Edition(e)))
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.note_read_lock();
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
        let Some(roots) = inner.roots(rel) else { return Ok(out) };
        for ((edition, _submit), entry) in roots.log.range_collect(&(from.0 + 1, 0), &(to.0 + 1, 0)) {
            match entry {
                LogEntry::Update(tuple, diff) => {
                    out.push(Update { tuple, time: Time::input(edition), diff })
                }
                // A graft's rows are the copy in its edition's own Fact root;
                // the block was empty before, so each row is one update.
                LogEntry::Graft(lo, hi) => {
                    let root = roots.versions.get(&edition).ok_or_else(|| {
                        Error::Store(format!("graft at edition {edition} has no Fact root"))
                    })?;
                    for (tuple, diff) in root.range_collect(&lo, &hi) {
                        out.push(Update { tuple, time: Time::input(edition), diff });
                    }
                }
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
    /// for one. It is *derived* state — the primary order is the truth — but it
    /// lives in the relation's roots like everything else, so once a commit has
    /// written it, it survives a reopen. A graft or a fork into the past drops
    /// it, to be rebuilt on demand.
    fn read_range_on(
        &self,
        rel: RelId,
        at: Edition,
        col: usize,
        lo: &Value,
        hi: &Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        self.note_read_lock();
        let mut inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("read_range_on", at.0, inner.watermark));
        }
        // The lead column is a key range on the primary order.
        if col == 0 {
            let (klo, khi) = (Tuple::from([lo.clone()]), Tuple::from([hi.clone()]));
            return Ok(inner.fact_at(rel, at.0).map(|t| t.range_collect(&klo, &khi)).unwrap_or_default());
        }
        // A k-d tree splits on every column, so it is its own index for any of
        // them, at any edition: no Arrangement is built.
        if inner.layout_of(rel) == Layout::Kd {
            let facts = inner.fact_at(rel, at.0).cloned();
            drop(inner);
            return Ok(facts.map(|t| t.kd_range_on(col, lo, hi)).unwrap_or_default());
        }
        // Arrangements track the *current* order. Below it, an entity span is a
        // search on the extents; anything else reads the primary order and
        // filters, which is always exact.
        if at.0 != inner.current {
            let facts = inner.fact_at(rel, at.0).cloned();
            drop(inner);
            if let (Value::Ent(l), Value::Ent(h)) = (lo, hi) {
                return Ok(facts.map(|t| search_box(&t, &[(col, l.0, h.0)])).unwrap_or_default());
            }
            let in_span = |k: &Tuple| k.as_slice().get(col).is_some_and(|v| lo <= v && v < hi);
            return Ok(facts
                .map(|t| t.iter().filter(|(k, _)| in_span(k)).map(|(k, v)| (k, *v)).collect())
                .unwrap_or_default());
        }
        Self::ensure_order(&mut inner, rel, col);
        let Some(arr) = inner.roots(rel).and_then(|r| r.orders.get(&(col as u32))) else {
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

    /// **Keyed lookup through the Ent's indexes.** Each key is a probe, not a
    /// scan:
    ///
    /// * column 0 is the primary order, so a key is a range read of the rows
    ///   that start with it, at any edition;
    /// * another column at the current edition is a range read of its
    ///   Arrangement — built on first use and maintained after, so it is the
    ///   persisted derived state that join maintenance leans on;
    /// * another column below the current edition, with entity keys, is one
    ///   extent search for all of them together.
    ///
    /// Anything else (a key with no successor, a non-entity key in the past)
    /// reads the relation once and filters, as the default does.
    fn lookup(&self, rel: RelId, at: Edition, col: usize, keys: &[Value]) -> Result<Vec<(Tuple, Diff)>> {
        // A repeated key would read its rows twice.
        let keys: Vec<Value> = keys.iter().cloned().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let keys = &keys[..];
        let spans: Option<Vec<(Tuple, Tuple)>> =
            keys.iter().map(|k| Some((Tuple::from([k.clone()]), Tuple::from([successor(k)?])))).collect();
        let mut inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("lookup", at.0, inner.watermark));
        }
        let probe = |t: &FactTree, spans: &[(Tuple, Tuple)]| -> Vec<(Tuple, Diff)> {
            spans.iter().flat_map(|(lo, hi)| t.range_collect(lo, hi)).collect()
        };
        if let Some(spans) = &spans {
            if col == 0 {
                return Ok(inner.fact_at(rel, at.0).map(|t| probe(t, spans)).unwrap_or_default());
            }
            // A k-d tree probes any column through its own splits.
            if inner.layout_of(rel) == Layout::Kd {
                let facts = inner.fact_at(rel, at.0).cloned();
                drop(inner);
                let Some(facts) = facts else { return Ok(Vec::new()) };
                let mut out: Vec<(Tuple, Diff)> = spans
                    .iter()
                    .flat_map(|(lo, hi)| facts.kd_range_on(col, &lo.as_slice()[0], &hi.as_slice()[0]))
                    .collect();
                out.sort();
                return Ok(out);
            }
            if at.0 == inner.current {
                Self::ensure_order(&mut inner, rel, col);
                let Some(arr) = inner.roots(rel).and_then(|r| r.orders.get(&(col as u32))) else {
                    return Ok(Vec::new());
                };
                return Ok(probe(arr, spans).into_iter().map(|(k, v)| (unrotate(&k, col), v)).collect());
            }
        }
        let facts = inner.fact_at(rel, at.0).cloned();
        drop(inner);
        let Some(facts) = facts else { return Ok(Vec::new()) };
        let ids: Option<std::collections::BTreeSet<u64>> =
            keys.iter().map(|k| if let Value::Ent(e) = k { Some(e.0) } else { None }).collect();
        if let Some(ids) = ids {
            // One pass for every key: a subtree is entered only if some key
            // falls inside its box for the column.
            return Ok(facts.search(
                |(_, x)| x.column(col).is_some_and(|(lo, hi)| ids.range(lo..=hi).next().is_some()),
                |k, _| matches!(k.as_slice().get(col), Some(Value::Ent(e)) if ids.contains(&e.0)),
            ));
        }
        let keys: std::collections::BTreeSet<&Value> = keys.iter().collect();
        Ok(facts
            .iter()
            .filter(|(k, _)| k.as_slice().get(col).is_some_and(|v| keys.contains(v)))
            .map(|(k, v)| (k, *v))
            .collect())
    }

    /// **Span stabbing through the extents** (the private `stab`). Non-entity points
    /// read and filter, as the default does.
    fn read_containing(
        &self,
        rel: RelId,
        at: Edition,
        first: usize,
        last: usize,
        points: &[Value],
    ) -> Result<Vec<(Tuple, Diff)>> {
        let facts = {
            let inner = self.inner.lock().unwrap();
            if at.0 < inner.watermark {
                return Err(door("read_containing", at.0, inner.watermark));
            }
            inner.fact_at(rel, at.0).cloned()
        };
        let Some(facts) = facts else { return Ok(Vec::new()) };
        Ok(stab(&facts, first, last, points).unwrap_or_else(|| {
            facts
                .iter()
                .filter(|(k, _)| grmpl_core::store::contains_any(k, first, last, points))
                .map(|(k, v)| (k, *v))
                .collect()
        }))
    }

    /// **Key-range interest routing, through the canopy (G-4).**
    ///
    /// The relation-wide [`touched_since`](TraceStore::touched_since) wakes every
    /// watcher of a relation whatever changed in it. This answers the narrower
    /// question the canopy exists for: two watchers on disjoint key ranges of one
    /// relation do not wake each other.
    ///
    /// The interest is registered on first ask and kept, so the **routing work
    /// happens once per commit** — the canopy is stabbed as the change lands —
    /// and the answer here is a WID range measure over the fired-interest
    /// enfilade, `O(log n)`, with no re-reading of the interval.
    ///
    /// Interests registered *after* a commit cannot have been routed by it, so
    /// this widens to the relation-wide answer for any interval that predates the
    /// registration: conservative, never a false negative.
    fn touched_range_since(
        &self,
        from: Edition,
        to: Edition,
        rel: RelId,
        lo: &Tuple,
        hi: &Tuple,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        if from.0 < inner.watermark {
            // Below the door we cannot prove anything; stay conservative and let
            // the caller's own read report the error properly.
            return Ok(true);
        }
        let key = (rel.0, lo.clone(), hi.clone());
        let (id, fresh) = match inner.interests.get(&key) {
            Some(id) => (*id, false),
            None => {
                let id = inner.canopy.register(rel, lo.clone(), hi.clone());
                inner.interests = inner.interests.insert(key, id);
                // Nothing was routed to it before now.
                inner.registered = inner.registered.insert(id.0, inner.current);
                (id, true)
            }
        };
        // An empty interval is provably quiet — consistent with `touched_since`,
        // which measures `(from, to]` and finds nothing. Registration above still
        // happened, so this doubles as the way a watcher declares its interest
        // before any commit it wants routed.
        if to <= from {
            return Ok(false);
        }
        let since = inner.registered.get(&id.0).copied().unwrap_or(0);
        if fresh || from.0 < since {
            // The interval predates this interest; fall back to the relation.
            drop(inner);
            return self.touched_since(from, to, &[rel]);
        }
        Ok(inner.fired.measure_range(&(id.0, from.0 + 1), &(id.0, to.0 + 1)).0 > 0)
    }

    fn watermark(&self) -> Edition {
        Edition(self.inner.lock().unwrap().watermark)
    }

    /// **Subtree-pruned version compare (E6).** The Ent's override of the
    /// substrate's state-difference primitive: two editions that share a Fact
    /// root compare in `O(1)`, and below that the descent prunes on shared
    /// content keys, so the cost is the size of the
    /// difference rather than the size of the relation — where the default must
    /// read both ends in full.
    ///
    /// `Tree::diff` walks both versions in key order, so the result is
    /// tuple-sorted as the contract requires, with no sort of its own.
    fn compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<(Tuple, Diff, Diff)>> {
        Ok(self
            .version_compare(rel, a, b)?
            .into_iter()
            .map(|(t, wa, wb)| (t, wa.unwrap_or(0), wb.unwrap_or(0)))
            .collect())
    }

    /// **Lock-free reads (see [`EntReader`]).** One brief lock to capture the
    /// edition's roots; every read after that touches no shared state.
    fn reader_at(&self, at: Edition) -> Box<dyn grmpl_core::EditionReader + '_> {
        self.dyn_reader_at(at)
    }

    fn dyn_reader_at(&self, at: Edition) -> Box<dyn grmpl_core::EditionReader + '_> {
        self.note_read_lock();
        let (rels, watermark) = {
            let inner = self.inner.lock().unwrap();
            // Cloning the Rel enfilade is one `Arc` refcount bump: it is a
            // persistent tree, so this hands out the whole directory as it stands
            // without copying any of it.
            (inner.rels.clone(), inner.watermark)
        };
        Box::new(EntReader { store: self, rels, at: at.0, watermark })
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
            let (_, mut versions) = roots.versions.split(&(new_wm + 1));
            if let Some((_, t)) = roots.versions.last_le(&new_wm) {
                versions = versions.insert(new_wm, t.clone());
            }
            let (_, log) = roots.log.split(&(new_wm + 1, 0));
            // Consolidation retires versions; the Arrangements rebuild on demand.
            inner.put(rel, RelRoots { versions, log, orders: OrderTree::new() });
        }
        // Patches at or below the watermark can no longer be replayed.
        inner.patches = inner.patches.split(&(new_wm + 1)).1;
        inner.watermark = new_wm;
        // The retired versions simply leave the Version enfilades; the root
        // record that no longer names them lands atomically, and GC reclaims
        // their nodes.
        let seq = self.stage(&inner)?;
        drop(inner);
        self.await_durable(seq)?;
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
        let seq = self.stage(&inner)?;
        drop(inner);
        self.await_durable(seq)
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
        let seq = self.stage(&inner)?;
        drop(inner);
        self.await_durable(seq)
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

/// The rows of `facts` whose inclusive span `[row[first], row[last]]` holds one
/// of the entity `points` — or `None` if a point is not an entity, which only a
/// filter can answer.
///
/// A subtree is entered only if some point lies between its least `first` and
/// its greatest `last`: the extent's two columns are the interval tree's
/// min-low and max-high, so nested scopes are stabbed the way the canopy stabs
/// interests.
fn stab(facts: &FactTree, first: usize, last: usize, points: &[Value]) -> Option<Vec<(Tuple, Diff)>> {
    let ids: std::collections::BTreeSet<u64> =
        points.iter().map(|p| if let Value::Ent(e) = p { Some(e.0) } else { None }).collect::<Option<_>>()?;
    Some(facts.search(
        |(_, x)| match (x.column(first), x.column(last)) {
            (Some((lo, _)), Some((_, hi))) => lo <= hi && ids.range(lo..=hi).next().is_some(),
            _ => false,
        },
        |k, _| grmpl_core::store::contains_any(k, first, last, points),
    ))
}

/// The least value above `v` in [`Value`]'s order, for the values where that is
/// a plain next value: `[v, successor(v))` then holds exactly the tuples whose
/// cell is `v`. `None` where it is not (text, floats, the top of a range).
fn successor(v: &Value) -> Option<Value> {
    match v {
        Value::Ent(e) => e.0.checked_add(1).map(|n| Value::Ent(Entity(n))),
        Value::Int(n) => n.checked_add(1).map(Value::Int),
        _ => None,
    }
}

/// The facts whose entity cells fall in every `(col, lo, hi)` span, pruned by
/// [`Extent`]. A box can only shrink as it descends, so the test is monotone.
fn search_box(facts: &FactTree, bounds: &[(usize, u64, u64)]) -> Vec<(Tuple, Diff)> {
    facts.search(
        |(_, x)| bounds.iter().all(|&(col, lo, hi)| x.meets(col, lo, hi)),
        |k, _| {
            bounds.iter().all(|&(col, lo, hi)| {
                matches!(k.as_slice().get(col), Some(Value::Ent(e)) if lo <= e.0 && e.0 < hi)
            })
        },
    )
}

/// **The content of `[lo, hi)` in a Fact tree, as leaves**: each leaf holding
/// rows of the span, as `(content key, offset of the leaf's frame, rows in the
/// span)`. Interior nodes are walked only to find the leaves, and a leaf
/// wholly inside the span (by its separators) is counted without being read.
/// The tree must be normalized; its keys are memoized here.
fn pieces(t: &FactTree, lo: &Tuple, hi: &Tuple) -> Vec<(ContentKey, i64, usize)> {
    /// `height` is 1 at a leaf: every leaf sits at one depth.
    fn walk(
        t: &FactTree,
        height: usize,
        parent_off: i64,
        bounds: (Option<Tuple>, Option<Tuple>),
        span: (&Tuple, &Tuple),
        out: &mut Vec<(ContentKey, i64, usize)>,
    ) {
        let off = parent_off.wrapping_add(t.dsp());
        let ck = *t.ck_cell().and_then(|c| c.get()).expect("keyed by content_key");
        let (nlo, nhi) = bounds;
        let (lo, hi) = span;
        if height == 1 {
            if nlo.as_ref().is_some_and(|l| l >= lo) && nhi.as_ref().is_some_and(|h| h <= hi) {
                out.push((ck, off, t.len()));
                return;
            }
            if let Some(crate::tree::NodeRef::Leaf(entries)) = t.node() {
                let n = entries
                    .iter()
                    .filter(|(k, _)| {
                        let k = k.displace(off);
                        &k >= lo && &k < hi
                    })
                    .count();
                if n > 0 {
                    out.push((ck, off, n));
                }
            }
            return;
        }
        if let Some(crate::tree::NodeRef::Internal(keys, children)) = t.node() {
            let last = children.len() - 1;
            for (i, c) in children.iter().enumerate() {
                let clo = if i == 0 { nlo.clone() } else { Some(keys[i - 1].displace(off)) };
                let chi = if i == last { nhi.clone() } else { Some(keys[i].displace(off)) };
                if chi.as_ref().is_some_and(|h| h <= lo) || clo.as_ref().is_some_and(|l| l >= hi) {
                    continue;
                }
                walk(c, height - 1, off, (clo, chi), span, out);
            }
        }
    }
    /// A k-d tree's leaves sit at different depths, so every node on the way
    /// is read. A split on column `0` bounds its children as a separator does.
    fn walk_kd(
        t: &FactTree,
        parent_off: i64,
        span: (&Tuple, &Tuple),
        out: &mut Vec<(ContentKey, i64, usize)>,
    ) {
        let off = parent_off.wrapping_add(t.dsp());
        let ck = *t.ck_cell().and_then(|c| c.get()).expect("keyed by content_key");
        let (lo, hi) = span;
        // A subtree whose extent puts it outside a one-column span is skipped
        // unread.
        let m = <FactMeasure as Measure<Tuple, Diff>>::displace(&t.measure(), parent_off);
        let outside = |k: &Tuple, below: bool| {
            k.as_slice().len() == 1 && <FactMeasure as Measure<Tuple, Diff>>::side_of(&m, 0, k) == Some(below)
        };
        if outside(lo, true) || outside(hi, false) {
            return;
        }
        match t.node() {
            Some(crate::tree::NodeRef::Leaf(entries)) => {
                let n = entries
                    .iter()
                    .filter(|(k, _)| {
                        let k = k.displace(off);
                        &k >= lo && &k < hi
                    })
                    .count();
                if n > 0 {
                    out.push((ck, off, n));
                }
            }
            Some(crate::tree::NodeRef::Split(col, pivot, children)) => {
                let p = pivot.displace(off);
                // Below a column-0 pivot every key is under it; above, at or over it.
                if col != 0 || *lo < p {
                    walk_kd(&children[0], off, span, out);
                }
                if col != 0 || *hi > p {
                    walk_kd(&children[1], off, span, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if content_key(t).is_some() {
        if t.is_kd() {
            walk_kd(t, 0, (lo, hi), &mut out);
        } else {
            walk(t, height(t), 0, (None, None), (lo, hi), &mut out);
        }
    }
    out
}

/// A non-empty tree's height, `1` for a single leaf: every leaf sits at one
/// depth, so the left spine says it.
fn height(t: &FactTree) -> usize {
    let mut h = 1;
    let mut cur = t.clone();
    while let Some(crate::tree::NodeRef::Internal(_, children)) = cur.node() {
        let first = children[0].clone();
        cur = first;
        h += 1;
    }
    h
}

/// A committed patch's updates, in submit order, rebuilt from the Edition logs
/// of the relations it wrote (each update logged at `(edition, its index)`).
fn patch_updates(state: &Inner, e: u64, rels: &[u32]) -> Vec<(RelId, Tuple, Diff)> {
    let mut indexed: Vec<(u64, RelId, Tuple, Diff)> = Vec::new();
    for rel in rels {
        let Some(log) = state.log_of(RelId(*rel)) else { continue };
        for ((_, i), entry) in log.range_collect(&(e, 0), &(e + 1, 0)) {
            if let LogEntry::Update(t, d) = entry {
                indexed.push((i, RelId(*rel), t, d));
            }
        }
    }
    indexed.sort_by_key(|(i, _, _, _)| *i);
    indexed.into_iter().map(|(_, rel, t, d)| (rel, t, d)).collect()
}

/// The versions a history holder stands for, still retained: the holder's own
/// version on its branch (or, if consolidation folded it into the watermark
/// checkpoint, that checkpoint), and the same version on every branch forked
/// from it after it was written.
fn versions_of(dag: &Dag, branches: &BranchStates, h: Holder, root: &ContentKey) -> Vec<Version> {
    let mut out = Vec::new();
    let mut stack = vec![h.branch];
    while let Some(b) = stack.pop() {
        if let Some(state) = branches.get(&b) {
            if let Some(versions) = state.roots(RelId(h.rel)).map(|r| &r.versions) {
                let key = if versions.get(&h.edition).is_some() {
                    Some(h.edition)
                } else if h.edition < state.watermark
                    && versions.get(&state.watermark).is_some_and(|t| content_key(t).as_ref() == Some(root))
                {
                    Some(state.watermark)
                } else {
                    None
                };
                if let Some(e) = key {
                    out.push(Version { branch: b, rel: RelId(h.rel), edition: Edition(e) });
                }
            }
        }
        // A fork after the version was written inherited it.
        for (c, br) in dag.tree().iter() {
            if br.parent.is_some_and(|(p, at)| p == b && at >= h.edition) {
                stack.push(c);
            }
        }
    }
    out
}

fn door(op: &str, at: u64, watermark: u64) -> Error {
    Error::Store(format!("{op} at edition {at} below watermark {watermark}"))
}

#[cfg(test)]
#[path = "history_laws.rs"]
mod history_laws;
