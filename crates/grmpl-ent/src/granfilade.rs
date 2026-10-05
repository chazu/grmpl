//! **The granfilade: content-addressed node persistence.**
//!
//! An enfilade [`Tree`] is made durable by storing each node under its **content
//! key** — the [`sha256`](grmpl_core::hash::sha256) of its frame, which closes over
//! the node's entries and its children's content keys. Because the key is a pure
//! function of *content* (never `phys_id` or allocation order), **equal subtrees
//! store once**: two versions of a tree that differ by one edited path share
//! every untouched node on disk (Xanadu structural sharing / Gold's granfilade,
//! modernised as a content-addressed blob store).
//!
//! Sharing is **within a version lineage** — a content key identifies a *shape*,
//! and two histories reaching the same logical map may build different shapes
//! (plan v5 §G-2b, settled: identity is logical, witnessed by `iter` /
//! `scan_updates`, exactly as `tree.rs` and `DESIGN.md` already had it).
//!
//! **One root, everything beneath it.** The granfilade has a single mutable
//! slot, the **root record** (Gold's turtle): a short list of content keys. Every
//! other durable thing — every relation's versions and log, the directories that
//! name them, the branch DAG, the canopy — is a tree reachable from it, because a
//! leaf may hold **links** to other trees ([`Enc::link`]). A link is a content
//! key plus the linked tree's dsp, size and measure, and the key rides in the
//! frame's reference run beside an internal node's children, so GC follows a
//! link exactly as it follows a child.
//!
//! **Demand paging.** A load reads one frame. An internal node's frame records
//! each child's size and measure as well as its key and dsp, so its children
//! come back as **paged** nodes ([`Tree::paged`]): counted, measured and
//! comparable by key, with their contents still on disk until a read reaches
//! them. Opening a store reads the root record and a handful of frames,
//! whatever the size of the world. Every paged node handed out is remembered
//! weakly, and GC treats the ones still unread as roots — their frames are the
//! only copy of what they hold.
//!
//! **Path-only, in work as well as in bytes (G-1).** A commit adds only the
//! `O(log n)` new nodes on the edited path, *and* only visits them: each node
//! memoizes its content key ([`Tree::ck_cell`]), and a node whose key is both
//! memoized and already durable here is returned without being re-serialized —
//! and so are all its descendants, since a node's key closes over its children's.
//! A paged node is both by construction, so persisting never pages anything in.
//!
//! The memo alone is not enough: a memoized key says "this is its key", not "it
//! is on *this* disk". So each granfilade also tracks the keys it knows are
//! durable, and a subtree is skipped only when both hold.
//!
//! Values are serialized through the single `grmpl_core::wire` value codec
//! ([`Persist`]).

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use fjall::{Database, KeyspaceCreateOptions, PersistMode};
use grmpl_core::{wire, Error, Result, Tuple, Value};

use crate::dsp::Displace;
use crate::measure::{Count, Extent, Measure};
use crate::tree::{Item, NodeRef, Pager, Resident, RunValue, Span, Tree};

pub use grmpl_core::hash::Sha256Digest as ContentKey;

/// The width of a [`ContentKey`] on the wire.
const CK_LEN: usize = 32;

/// Node frames ride the one workspace format version. v6 added links from
/// leaves to other trees, each internal child's size and measure, and the
/// single root record. v7 added the [`Extent`] to every Fact tree's measure
/// and the spanfilade to each branch's state. v8 added each branch's patch log
/// and a second parent to merged branches. v9 added the k-d split frame, each
/// branch's layout default and layout directory, and a count of entity cells
/// in every extent. v10 made a leaf a run of tagged items (rows, runs, holes)
/// and recorded every child's and link's reserved keys. v11 made each
/// branch's layout directory a directory of shapes (layout and runs). Like
/// every cutover before it, v11 is fresh-store-only: a v11 binary rejects
/// every older persisted node before interpreting its payload.
const NODE_FORMAT_VERSION: u8 = wire::FORMAT_VERSION;

/// The meta key of the root record — the granfilade's one mutable slot.
const ROOT_KEY: &[u8] = b"root";

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// A type that can be (de)serialized into a node frame. Payloads reuse the one
/// `grmpl_core::wire` codec, so "one serialization" holds.
///
/// Encoding goes through an [`Enc`] and decoding through a [`Dec`] rather than
/// a bare byte buffer because a value may be a whole tree: [`Enc::link`]
/// persists it and records its key in the frame's reference run, and
/// [`Dec::link`] hands it back paged.
pub trait Persist: Sized {
    fn encode(&self, e: &mut Enc<'_, '_>);
    fn decode(d: &mut Dec<'_>) -> Result<Self>;
}

/// A key the granfilade can store: ordered, displaceable, and shareable across
/// the threads a paged tree may be read from.
pub trait PersistKey: Persist + Displace + Send + Sync + 'static {}
impl<T: Persist + Displace + Send + Sync + 'static> PersistKey for T {}

/// A value the granfilade can store.
pub trait PersistVal: Persist + RunValue + Send + Sync + 'static {}
impl<T: Persist + RunValue + Send + Sync + 'static> PersistVal for T {}

/// A measure the granfilade can store. Internal frames record each child's
/// measure, which is what lets a paged child answer a range measure without
/// being read.
pub trait PersistMeasure<K, V>: Measure<K, V> + Persist + Send + Sync + 'static {}
impl<K, V, T: Measure<K, V> + Persist + Send + Sync + 'static> PersistMeasure<K, V> for T {}

/// Where encoded frames collect during one [`Granfilade::collect_tree`], or
/// during [`content_key`] with no node store behind it.
struct Sink<'g> {
    gran: Option<&'g Granfilade>,
    out: Vec<(ContentKey, Vec<u8>)>,
}

impl Sink<'_> {
    /// Whether a node already memoizing `ck` needs no new frame: it is durable
    /// in this granfilade, or (with no granfilade) it has simply been hashed.
    fn known(&self, ck: &ContentKey) -> bool {
        self.gran.is_none_or(|g| g.is_present(ck))
    }
}

/// **The content key of an in-memory tree**, computed and memoized without a
/// node store: every node is framed and hashed exactly as the granfilade would
/// store it, so a key computed here equals the key the node gets on disk. Nodes
/// already memoizing a key are not revisited. `None` for an empty tree.
///
/// The root is keyed **normalized**, as the root record stores it.
pub fn content_key<K, V, M>(tree: &Tree<K, V, M>) -> Option<ContentKey>
where
    K: PersistKey,
    V: PersistVal,
    M: PersistMeasure<K, V>,
{
    let mut sink = Sink { gran: None, out: Vec::new() };
    collect_nodes(&tree.normalized(), &mut sink)
}

/// One node frame's payload under construction: its bytes, and the content keys
/// it references (an internal node's children, a leaf's links), which become
/// the frame's leading reference run.
pub struct Enc<'s, 'g> {
    buf: Vec<u8>,
    refs: Vec<ContentKey>,
    sink: &'s mut Sink<'g>,
}

impl Enc<'_, '_> {
    /// Append raw bytes to the payload.
    pub fn put(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The payload buffer, for codecs that write into a `Vec<u8>`.
    pub fn buf(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Persist `tree` and link to it from this frame: its nodes join the write,
    /// its content key joins the reference run (so GC follows it), and the
    /// payload records its dsp, size and measure (so it reloads paged).
    pub fn link<K, V, M>(&mut self, tree: &Tree<K, V, M>)
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        match collect_nodes(tree, self.sink) {
            None => self.buf.push(0),
            Some(ck) => {
                self.buf.push(1);
                self.refs.push(ck);
                tree.dsp().encode(self);
                (tree.len() as u64).encode(self);
                tree.reserved().encode(self);
                tree.local_measure().expect("a non-empty tree has a measure").encode(self);
            }
        }
    }
}

/// A node frame's payload being read: the bytes, a cursor, and the frame's
/// reference run, consumed in order by [`link`](Self::link).
pub struct Dec<'a> {
    bytes: &'a [u8],
    pos: usize,
    refs: &'a [ContentKey],
    next_ref: usize,
    gran: &'a Arc<Granfilade>,
}

impl<'a> Dec<'a> {
    /// The next `n` payload bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos + n;
        let b = self.bytes.get(self.pos..end).ok_or_else(|| trunc("payload"))?;
        self.pos = end;
        Ok(b)
    }

    /// Decode with a `(bytes, pos) -> (value, pos)` codec such as
    /// `wire::decode_tuple`.
    pub fn with<T>(&mut self, f: impl FnOnce(&[u8], usize) -> Result<(T, usize)>) -> Result<T> {
        let (v, pos) = f(self.bytes, self.pos)?;
        self.pos = pos;
        Ok(v)
    }

    fn next_ref(&mut self) -> Result<ContentKey> {
        let ck = *self.refs.get(self.next_ref).ok_or_else(|| trunc("reference run"))?;
        self.next_ref += 1;
        Ok(ck)
    }

    /// A linked tree, paged: nothing beneath its root is read until used.
    pub fn link<K, V, M>(&mut self) -> Result<Tree<K, V, M>>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        match self.take(1)?[0] {
            0 => Ok(Tree::new()),
            1 => {
                let ck = self.next_ref()?;
                let dsp = i64::decode(self)?;
                let size = u64::decode(self)? as usize;
                let reserved = u64::decode(self)?;
                let measure = M::decode(self)?;
                let pager = self.gran.pager::<K, V, M>();
                Ok(self.gran.stub(ck, size, reserved, measure, &pager).relocate(dsp))
            }
            t => Err(Error::Codec(format!("granfilade: bad link flag {t}"))),
        }
    }
}

macro_rules! fixed_width {
    ($($t:ty),*) => {$(
        impl Persist for $t {
            fn encode(&self, e: &mut Enc<'_, '_>) {
                e.put(&self.to_be_bytes());
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self> {
                let b = d.take(std::mem::size_of::<$t>())?;
                Ok(<$t>::from_be_bytes(b.try_into().unwrap()))
            }
        }
    )*};
}
fixed_width!(i64, u64, u32);

/// A content key held as data (the history index keys nodes by it), not as a
/// link: GC does not follow it.
impl Persist for ContentKey {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        e.put(self);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(d.take(CK_LEN)?.try_into().unwrap())
    }
}

impl Persist for () {
    fn encode(&self, _e: &mut Enc<'_, '_>) {}
    fn decode(_d: &mut Dec<'_>) -> Result<Self> {
        Ok(())
    }
}

impl<T: Persist> Persist for Option<T> {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        match self {
            None => e.put(&[0]),
            Some(v) => {
                e.put(&[1]);
                v.encode(e);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        match d.take(1)?[0] {
            0 => Ok(None),
            1 => Ok(Some(T::decode(d)?)),
            t => Err(Error::Codec(format!("granfilade: bad option flag {t}"))),
        }
    }
}

impl<A: Persist, B: Persist> Persist for (A, B) {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.0.encode(e);
        self.1.encode(e);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok((A::decode(d)?, B::decode(d)?))
    }
}

impl<A: Persist, B: Persist, C: Persist> Persist for (A, B, C) {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.0.encode(e);
        self.1.encode(e);
        self.2.encode(e);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok((A::decode(d)?, B::decode(d)?, C::decode(d)?))
    }
}

impl Persist for Tuple {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        wire::encode_tuple(self, e.buf());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        d.with(wire::decode_tuple)
    }
}

/// Also allow a bare `Value` payload (single-column keys, etc.).
impl Persist for Value {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        wire::encode_value(self, e.buf());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        d.with(wire::decode_value)
    }
}

impl Persist for Count {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.0.encode(e);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(Count(u64::decode(d)?))
    }
}

/// An extent persists as its row count and column count, then each column's
/// bounds behind a presence flag and its count of entity cells.
impl Persist for Extent {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        let (bounds, ents, rows) = self.parts();
        rows.encode(e);
        (bounds.len() as u32).encode(e);
        for (col, n) in bounds.iter().zip(ents) {
            col.encode(e);
            n.encode(e);
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        let rows = u64::decode(d)?;
        let n = u32::decode(d)? as usize;
        let (mut bounds, mut ents) = (Vec::with_capacity(n.min(256)), Vec::with_capacity(n.min(256)));
        for _ in 0..n {
            bounds.push(Option::<(u64, u64)>::decode(d)?);
            ents.push(u64::decode(d)?);
        }
        Ok(Extent::from_parts(bounds, ents, rows))
    }
}

/// A tree as a value is a **link**: see [`Enc::link`].
impl<K, V, M> Persist for Tree<K, V, M>
where
    K: PersistKey,
    V: PersistVal,
    M: PersistMeasure<K, V>,
{
    fn encode(&self, e: &mut Enc<'_, '_>) {
        e.link(self);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        d.link()
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// One durable step, **already encoded and hashed** but not yet written: the
/// node frames it adds and the root record that names them.
///
/// Encoding is pure and needs only the immutable trees, so it happens inside the
/// store's locks; the batch and its `fsync` happen outside, where a group of
/// them can share one. Splitting the two is what [`write_group`] exists for.
///
/// [`write_group`]: Granfilade::write_group
pub struct StagedWrite {
    pub nodes: Vec<(ContentKey, Vec<u8>)>,
    pub root: Vec<Option<ContentKey>>,
}

/// Weak handles on every paged node handed out, for GC.
struct Remembered {
    nodes: Vec<(ContentKey, Weak<dyn Resident>)>,
    /// Prune when the list reaches this length, then double it, so pruning is
    /// amortized `O(1)` per paged node.
    prune_at: usize,
}

/// The content-addressed node store for the Ent, plus the one root record.
pub struct Granfilade {
    db: Database,
    nodes: fjall::Keyspace,
    meta: fjall::Keyspace,
    /// **Keys known to be durable in *this* granfilade (G-1).** A memoized
    /// content key on a node says "this is its key", not "it is on this disk".
    /// Without this set, a commit could skip writing a node and leave a root
    /// pointing at a frame that was never stored. Populated on every write, load
    /// and page-in; GC removes what it sweeps.
    present: Mutex<HashSet<ContentKey>>,
    /// Paged nodes still reachable from memory (see [`Resident`]).
    remembered: Mutex<Remembered>,
    /// **Ops counter (G-0a).** Node frames serialized+hashed since this handle
    /// was opened. Sublinear claims about the commit path are otherwise only
    /// prose; this is what lets a test *fail* when an `O(log n)` walk quietly
    /// becomes a scan.
    encoded: AtomicU64,
    /// Bytes in the frames [`encoded`](Self::encoded) counts: what the commit
    /// path writes, before the node store compresses it.
    encoded_bytes: AtomicU64,
    /// **Durability ops counter (group commit).** `SyncAll`s issued since this
    /// handle was opened.
    syncs: AtomicU64,
    /// **Paging ops counter.** Node frames read from disk since this handle was
    /// opened — what lets a test fail if `open` goes back to reading the world.
    paged_in: AtomicU64,
}

impl Granfilade {
    /// Open (or create) a granfilade rooted at `path`.
    ///
    /// A store written before the root record existed is refused: its roots
    /// live under keys this version does not read, so opening it would look
    /// like an empty world.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Arc<Granfilade>> {
        let db = Database::builder(path.as_ref()).open().map_err(store_err)?;
        let nodes = db
            .keyspace("nodes", KeyspaceCreateOptions::default)
            .map_err(store_err)?;
        let meta = db
            .keyspace("meta", KeyspaceCreateOptions::default)
            .map_err(store_err)?;
        let gran = Granfilade {
            db,
            nodes,
            meta,
            present: Mutex::new(HashSet::new()),
            remembered: Mutex::new(Remembered { nodes: Vec::new(), prune_at: 1024 }),
            encoded: AtomicU64::new(0),
            encoded_bytes: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            paged_in: AtomicU64::new(0),
        };
        if gran.meta_get(ROOT_KEY)?.is_none() && gran.meta.iter().next().is_some() {
            return Err(Error::Codec(format!(
                "granfilade: store has no root record — it predates format v{NODE_FORMAT_VERSION}, \
                 which requires a fresh store and has no migrator"
            )));
        }
        Ok(Arc::new(gran))
    }

    /// The root record: the content keys every durable tree hangs from, or an
    /// empty list for a fresh store.
    pub fn root(&self) -> Result<Vec<Option<ContentKey>>> {
        match self.meta_get(ROOT_KEY)? {
            None => Ok(Vec::new()),
            Some(bytes) => decode_root(&bytes),
        }
    }

    /// Collect a tree's new nodes as `(content_key, frame)` pairs (children
    /// first) and its root key — pure, no I/O. Linked trees are collected with
    /// it. The caller batches these with the root record so nodes land
    /// **with** the root that references them (crash-safety).
    pub fn collect_tree<K, V, M>(&self, tree: &Tree<K, V, M>) -> (Option<ContentKey>, Vec<(ContentKey, Vec<u8>)>)
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let mut sink = Sink { gran: Some(self), out: Vec::new() };
        // The root record holds bare content keys, so a displaced root is
        // written normalized: the root node opened into this frame, every
        // subtree beneath it shared. Below the root, dsps ride the edges.
        let ck = collect_nodes(&tree.normalized(), &mut sink);
        self.encoded.fetch_add(sink.out.len() as u64, Ordering::Relaxed);
        let bytes: usize = sink.out.iter().map(|(_, f)| f.len()).sum();
        self.encoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        (ck, sink.out)
    }

    /// **Group commit (the durability half).** Apply several already-encoded
    /// writes as **one** batch and **one** `SyncAll`.
    ///
    /// Every axis of `docs/PERFORMANCE-ENT.md` bottoms out on that single
    /// ~1 ms fsync, so N committers writing one at a time pay N fsyncs strictly
    /// in series. Amortising one fsync across a group is the largest single win
    /// available, and it does **not** weaken the patch–edition law: the law
    /// demands one atomic durable write *per edition*, not one *per committer*.
    ///
    /// `group` must be in **staging order**. Each entry's root record is the
    /// whole world as of that stage, so the last one wins, which is exactly
    /// right; writing them out of order would durably record a stale root.
    pub fn write_group(&self, group: Vec<StagedWrite>) -> Result<()> {
        let Some(root) = group.last().map(|s| encode_root(&s.root)) else {
            return Ok(());
        };
        let mut batch = self.db.batch();
        let mut written = Vec::new();
        for staged in group {
            for (k, v) in staged.nodes {
                batch.insert(&self.nodes, k.to_vec(), v);
                written.push(k);
            }
        }
        batch.insert(&self.meta, ROOT_KEY.to_vec(), root);
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        // Only after the batch is durable may these count as present — otherwise
        // a crash mid-write would leave the memo claiming a node is on disk.
        self.present.lock().unwrap().extend(written);
        Ok(())
    }

    fn meta_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.meta.get(key).map_err(store_err)?.map(|s| s.as_ref().to_vec()))
    }

    /// Persist `tree`'s nodes, returning its root content key (`None` if
    /// empty), **without** naming it from the root record — so the next GC
    /// collects it. For tests and tools that exercise the node store directly.
    pub fn persist<K, V, M>(&self, tree: &Tree<K, V, M>) -> Result<Option<ContentKey>>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let (ck, nodes) = self.collect_tree(tree);
        let mut batch = self.db.batch();
        for (k, v) in &nodes {
            batch.insert(&self.nodes, k.to_vec(), v.clone());
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.present.lock().unwrap().extend(nodes.into_iter().map(|(k, _)| k));
        Ok(ck)
    }

    /// The tree rooted at `ck`, reading **one** frame: its root node is
    /// resident and everything beneath it is paged.
    pub fn load<K, V, M>(self: &Arc<Self>, ck: Option<ContentKey>) -> Result<Tree<K, V, M>>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let Some(ck) = ck else { return Ok(Tree::new()) };
        let tree = self.read_node::<K, V, M>(&ck)?;
        if let Some(cell) = tree.ck_cell() {
            let _ = cell.set(ck);
        }
        self.present.lock().unwrap().insert(ck);
        Ok(tree)
    }

    /// Decode the frame under `ck` into a resident node whose children (and
    /// links) are paged.
    fn read_node<K, V, M>(self: &Arc<Self>, ck: &ContentKey) -> Result<Tree<K, V, M>>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let frame = self
            .nodes
            .get(ck)
            .map_err(store_err)?
            .ok_or_else(|| Error::Store("granfilade: node key not found".into()))?;
        self.paged_in.fetch_add(1, Ordering::Relaxed);
        let bytes = frame.as_ref();
        let (tag, refs, pos) = decode_header(bytes)?;
        let mut d = Dec { bytes, pos, refs: &refs, next_ref: 0, gran: self };
        let count = u32::decode(&mut d)? as usize;
        match tag {
            TAG_LEAF => {
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(match d.take(1)?[0] {
                        ITEM_ROW => Item::One(K::decode(&mut d)?, V::decode(&mut d)?),
                        ITEM_RUN => {
                            let n = u64::decode(&mut d)?;
                            let (first, stride) = (K::decode(&mut d)?, K::decode(&mut d)?);
                            Item::Run(Span { first, stride, n }, V::decode(&mut d)?)
                        }
                        ITEM_HOLE => {
                            let n = u64::decode(&mut d)?;
                            let (first, stride) = (K::decode(&mut d)?, K::decode(&mut d)?);
                            Item::Hole(Span { first, stride, n })
                        }
                        t => return Err(Error::Codec(format!("granfilade: unknown leaf item tag {t}"))),
                    });
                }
                if d.next_ref != refs.len() {
                    return Err(Error::Codec("granfilade: leaf links disagree with its references".into()));
                }
                Ok(Tree::leaf_of(items))
            }
            TAG_INTERNAL => {
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(K::decode(&mut d)?);
                }
                if keys.len() + 1 != refs.len() {
                    return Err(Error::Codec("granfilade: malformed internal node".into()));
                }
                let pager = self.pager::<K, V, M>();
                let mut children = Vec::with_capacity(refs.len());
                for c in &refs {
                    let dsp = i64::decode(&mut d)?;
                    let size = u64::decode(&mut d)? as usize;
                    let reserved = u64::decode(&mut d)?;
                    let measure = M::decode(&mut d)?;
                    children.push(self.stub(*c, size, reserved, measure, &pager).relocate(dsp));
                }
                Ok(Tree::internal_of(keys, children))
            }
            TAG_SPLIT => {
                let pivot = K::decode(&mut d)?;
                let [lo, hi] = refs[..] else {
                    return Err(Error::Codec("granfilade: malformed split node".into()));
                };
                let pager = self.pager::<K, V, M>();
                let mut child = |ck: ContentKey| -> Result<Tree<K, V, M>> {
                    let dsp = i64::decode(&mut d)?;
                    let size = u64::decode(&mut d)? as usize;
                    let reserved = u64::decode(&mut d)?;
                    let measure = M::decode(&mut d)?;
                    Ok(self.stub(ck, size, reserved, measure, &pager).relocate(dsp))
                };
                let (lo, hi) = (child(lo)?, child(hi)?);
                Ok(Tree::split_of(count, pivot, lo, hi))
            }
            _ => Err(Error::Codec(format!("granfilade: unknown node tag {tag}"))),
        }
    }

    fn pager<K, V, M>(self: &Arc<Self>) -> Arc<dyn Pager<K, V, M>>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        Arc::new(GranPager { gran: Arc::clone(self), _types: PhantomData })
    }

    /// A paged node for the frame under `ck`, which is durable (a durable frame
    /// references it), and remembered so GC keeps its frame while it is unread.
    fn stub<K, V, M>(&self, ck: ContentKey, size: usize, reserved: u64, measure: M, pager: &Arc<dyn Pager<K, V, M>>) -> Tree<K, V, M>
    where
        K: PersistKey,
        V: PersistVal,
        M: PersistMeasure<K, V>,
    {
        let tree = Tree::paged(ck, size, reserved, measure, Arc::clone(pager));
        self.present.lock().unwrap().insert(ck);
        if let Some(weak) = tree.residency() {
            let mut rem = self.remembered.lock().unwrap();
            rem.nodes.push((ck, weak));
            if rem.nodes.len() >= rem.prune_at {
                rem.nodes.retain(|(_, w)| w.upgrade().is_some_and(|n| !n.resident()));
                rem.prune_at = (rem.nodes.len() * 2).max(1024);
            }
        }
        tree
    }

    /// Node frames serialized and hashed since this handle was opened — the
    /// [`encoded`](Self::encoded) ops counter.
    pub fn frames_encoded(&self) -> u64 {
        self.encoded.load(Ordering::Relaxed)
    }

    /// Bytes in the frames [`frames_encoded`](Self::frames_encoded) counts.
    pub fn bytes_encoded(&self) -> u64 {
        self.encoded_bytes.load(Ordering::Relaxed)
    }

    /// Node frames read from disk since this handle was opened.
    pub fn frames_paged(&self) -> u64 {
        self.paged_in.load(Ordering::Relaxed)
    }

    /// `SyncAll`s issued since this handle was opened — the durability cost of
    /// the world so far. Under group commit this is **fewer** than the number of
    /// commits whenever writers overlap; equal to it when they do not.
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    /// Number of distinct nodes stored — for structural-sharing verification.
    pub fn node_count(&self) -> Result<usize> {
        Ok(self.nodes.iter().count())
    }

    /// **Reachability GC (E3).** Collect every node unreachable from the root
    /// record and from every paged node still unread in memory: mark by walking
    /// each frame's reference run (children and links alike), then sweep the
    /// rest. Returns the number of nodes collected. Type-agnostic — it reads only
    /// the leading reference run of each frame.
    ///
    /// The caller must keep new roots from being written while this runs (the
    /// store holds its root lock).
    pub fn gc(&self) -> Result<usize> {
        let mut stack: Vec<ContentKey> = self.root()?.into_iter().flatten().collect();
        {
            let mut rem = self.remembered.lock().unwrap();
            rem.nodes.retain(|(_, w)| w.upgrade().is_some_and(|n| !n.resident()));
            stack.extend(rem.nodes.iter().map(|(ck, _)| *ck));
        }
        let mut marked: HashSet<ContentKey> = HashSet::new();
        while let Some(ck) = stack.pop() {
            if !marked.insert(ck) {
                continue;
            }
            if let Some(frame) = self.nodes.get(ck).map_err(store_err)? {
                stack.extend(refs_of(frame.as_ref())?);
            }
        }
        let mut swept = Vec::new();
        let mut batch = self.db.batch();
        for kv in self.nodes.iter() {
            let (k, _v) = kv.into_inner().map_err(store_err)?;
            let key: ContentKey = k.as_ref().try_into().map_err(|_| trunc("node key"))?;
            if !marked.contains(&key) {
                batch.remove(&self.nodes, k.as_ref().to_vec());
                swept.push(key);
            }
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        // A swept key is no longer on disk; a resident node still holding it
        // must be written again if a later root reaches it.
        let mut present = self.present.lock().unwrap();
        for k in &swept {
            present.remove(k);
        }
        Ok(swept.len())
    }

    fn is_present(&self, ck: &ContentKey) -> bool {
        self.present.lock().unwrap().contains(ck)
    }
}

/// The tree type a [`GranPager`] decodes, held as a function pointer so the
/// pager is `Send + Sync` whatever `K`, `V` and `M` are.
type TreeType<K, V, M> = PhantomData<fn() -> (K, V, M)>;

/// Pages nodes of one tree type in from a granfilade.
struct GranPager<K, V, M> {
    gran: Arc<Granfilade>,
    _types: TreeType<K, V, M>,
}

impl<K, V, M> Pager<K, V, M> for GranPager<K, V, M>
where
    K: PersistKey,
    V: PersistVal,
    M: PersistMeasure<K, V>,
{
    fn page(&self, ck: &ContentKey) -> Tree<K, V, M> {
        self.gran
            .read_node(ck)
            .unwrap_or_else(|e| panic!("granfilade: paging in node {}: {e}", hex(ck)))
    }
}

/// A leaf frame: a run of items, each behind its tag ([`ITEM_ROW`] then key
/// and value; [`ITEM_RUN`] then count, first key, stride and value;
/// [`ITEM_HOLE`] then count, first key and stride); its references are the
/// links its values hold, in order.
const TAG_LEAF: u8 = 0;
const ITEM_ROW: u8 = 0;
const ITEM_RUN: u8 = 1;
const ITEM_HOLE: u8 = 2;
/// An internal frame: separators in the node's local frame, then for each child
/// its dsp, size, reserved keys and measure; its references are the children.
const TAG_INTERNAL: u8 = 1;
/// A k-d split frame: the column (in the count field), the pivot in the
/// node's local frame, then for each of the two children its dsp, size and
/// measure; its references are the two children, below the pivot first.
const TAG_SPLIT: u8 = 2;

/// The reference run of a node frame, read **without decoding the payload** —
/// the frame puts it first precisely so GC can walk references without knowing
/// `K`, `V` or `M`.
fn refs_of(frame: &[u8]) -> Result<Vec<ContentKey>> {
    let (_tag, refs, _pos) = decode_header(frame)?;
    Ok(refs)
}

/// Frame header: `version(1) || tag(1) || n_refs(u32 BE) || [content_key]*n`.
/// Returns the tag, the references, and the offset where the payload begins.
fn decode_header(frame: &[u8]) -> Result<(u8, Vec<ContentKey>, usize)> {
    check_version(frame.first().copied())?;
    let tag = *frame.get(1).ok_or_else(|| trunc("node tag"))?;
    let n = u32::from_be_bytes(frame.get(2..6).ok_or_else(|| trunc("count"))?.try_into().unwrap()) as usize;
    let mut pos = 6;
    let mut refs = Vec::with_capacity(n);
    for _ in 0..n {
        let end = pos + CK_LEN;
        let b = frame.get(pos..end).ok_or_else(|| trunc("content key"))?;
        refs.push(b.try_into().unwrap());
        pos = end;
    }
    Ok((tag, refs, pos))
}

fn check_version(v: Option<u8>) -> Result<()> {
    match v {
        Some(NODE_FORMAT_VERSION) => Ok(()),
        Some(v) => Err(Error::Codec(format!(
            "granfilade: unsupported node format version {v} (expected {NODE_FORMAT_VERSION}; \
             v{NODE_FORMAT_VERSION} requires a fresh store and has no migrator)"
        ))),
        None => Err(trunc("node version")),
    }
}

/// Root record: `version(1) || n(u8) || [present(1) || content_key?]*n`.
fn encode_root(slots: &[Option<ContentKey>]) -> Vec<u8> {
    let mut out = vec![NODE_FORMAT_VERSION, slots.len() as u8];
    for slot in slots {
        match slot {
            None => out.push(0),
            Some(ck) => {
                out.push(1);
                out.extend_from_slice(ck);
            }
        }
    }
    out
}

fn decode_root(bytes: &[u8]) -> Result<Vec<Option<ContentKey>>> {
    check_version(bytes.first().copied())?;
    let n = *bytes.get(1).ok_or_else(|| trunc("root record"))? as usize;
    let mut pos = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        match bytes.get(pos) {
            Some(0) => {
                out.push(None);
                pos += 1;
            }
            Some(1) => {
                let b = bytes.get(pos + 1..pos + 1 + CK_LEN).ok_or_else(|| trunc("root record"))?;
                out.push(Some(b.try_into().unwrap()));
                pos += 1 + CK_LEN;
            }
            _ => return Err(trunc("root record")),
        }
    }
    Ok(out)
}

/// Recurse the tree, appending each new node's `(content_key, frame_bytes)` to
/// the sink and returning the root key. Children (and links) first, so a node's
/// frame carries their content keys. Collects the *node* under `tree`; the
/// handle's own dsp belongs to the edge that points at it.
///
/// One frame is one **node**, and a node holds a whole run of entries
/// ([`grmpl_ent::tree::B`](crate::tree::B) of them), so the store keeps one
/// record per run rather than one per tuple.
fn collect_nodes<K, V, M>(tree: &Tree<K, V, M>, sink: &mut Sink<'_>) -> Option<ContentKey>
where
    K: PersistKey,
    V: PersistVal,
    M: PersistMeasure<K, V>,
{
    let cell = tree.ck_cell()?;
    // **Path-only work (G-1).** A node whose key is memoized *and* already
    // durable here needs neither re-serializing nor revisiting — and neither do
    // any of its descendants, since a node's key closes over its children's.
    // Every paged node qualifies, so persisting never pages anything in.
    if let Some(ck) = cell.get() {
        if sink.known(ck) {
            return Some(*ck);
        }
    }
    let mut e = Enc { buf: Vec::new(), refs: Vec::new(), sink };
    let tag = match tree.node()? {
        NodeRef::Leaf(items) => {
            (items.len() as u32).encode(&mut e);
            for it in items {
                match it {
                    Item::One(k, v) => {
                        e.put(&[ITEM_ROW]);
                        k.encode(&mut e);
                        v.encode(&mut e);
                    }
                    Item::Run(s, v) => {
                        e.put(&[ITEM_RUN]);
                        s.n.encode(&mut e);
                        s.first.encode(&mut e);
                        s.stride.encode(&mut e);
                        v.encode(&mut e);
                    }
                    Item::Hole(s) => {
                        e.put(&[ITEM_HOLE]);
                        s.n.encode(&mut e);
                        s.first.encode(&mut e);
                        s.stride.encode(&mut e);
                    }
                }
            }
            TAG_LEAF
        }
        NodeRef::Internal(keys, children) => {
            for c in children {
                let ck = collect_nodes(c, e.sink).expect("a child is never empty");
                e.refs.push(ck);
            }
            (keys.len() as u32).encode(&mut e);
            for k in keys {
                k.encode(&mut e);
            }
            for c in children {
                c.dsp().encode(&mut e);
                (c.len() as u64).encode(&mut e);
                c.reserved().encode(&mut e);
                c.local_measure().expect("a child is never empty").encode(&mut e);
            }
            TAG_INTERNAL
        }
        NodeRef::Split(col, pivot, children) => {
            for c in children {
                let ck = collect_nodes(c, e.sink).expect("a child is never empty");
                e.refs.push(ck);
            }
            (col as u32).encode(&mut e);
            pivot.encode(&mut e);
            for c in children {
                c.dsp().encode(&mut e);
                (c.len() as u64).encode(&mut e);
                c.reserved().encode(&mut e);
                c.local_measure().expect("a child is never empty").encode(&mut e);
            }
            TAG_SPLIT
        }
    };
    let Enc { buf, refs, sink } = e;
    let mut bytes = Vec::with_capacity(6 + refs.len() * CK_LEN + buf.len());
    bytes.push(NODE_FORMAT_VERSION);
    bytes.push(tag);
    bytes.extend_from_slice(&(refs.len() as u32).to_be_bytes());
    for r in &refs {
        bytes.extend_from_slice(r);
    }
    bytes.extend_from_slice(&buf);
    let ck = grmpl_core::hash::sha256(&bytes);
    let _ = cell.set(ck);
    if sink.gran.is_some() {
        sink.out.push((ck, bytes));
    }
    Some(ck)
}

fn hex(ck: &ContentKey) -> String {
    ck.iter().map(|b| format!("{b:02x}")).collect()
}

fn trunc(what: &str) -> Error {
    Error::Codec(format!("granfilade: truncated {what}"))
}

fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    type FactTree = Tree<Tuple, i64, Count>;
    /// A directory of trees, as the store keeps them: each value is a link.
    type DirTree = Tree<u64, FactTree, Count>;

    fn t(n: i64) -> Tuple {
        Tuple::from([Value::Int(n)])
    }

    fn contents(t: &FactTree) -> Vec<(Tuple, i64)> {
        t.iter().map(|(k, v)| (k, *v)).collect()
    }

    #[test]
    fn persist_reload_roundtrip_and_structural_sharing() {
        let dir = tempfile::tempdir().unwrap();
        let gran = Granfilade::open(dir.path()).unwrap();

        // Persist a Fact tree and reload it — the logical content round-trips.
        let mut v0 = FactTree::new();
        for k in 0..20i64 {
            v0 = v0.insert(t(k), k * 10);
        }
        let ck0 = gran.persist(&v0).unwrap();
        let reloaded: FactTree = gran.load(ck0).unwrap();
        assert_eq!(contents(&v0), contents(&reloaded), "reload did not reproduce the tree's contents");

        // A new version shares every untouched subtree: persisting it adds only
        // the O(log n) nodes on the edited path, not a full copy.
        let before = gran.node_count().unwrap();
        let v1 = v0.insert(t(100), 999);
        gran.persist(&v1).unwrap();
        let added = gran.node_count().unwrap() - before;
        assert!(added > 0, "nothing was stored for the new version");
        assert!(added < v1.len(), "no structural sharing: added {added} nodes for a {}-node tree", v1.len());
    }

    /// A load reads one frame; the rest pages in only as a read reaches it, and
    /// counts and measures of untouched subtrees come from their parents.
    #[test]
    fn a_load_pages_in_only_what_a_read_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let gran = Granfilade::open(dir.path()).unwrap();
        let mut tree = FactTree::new();
        for k in 0..50_000i64 {
            tree = tree.insert(t(k), 1);
        }
        let ck = gran.persist(&tree).unwrap();

        let before = gran.frames_paged();
        let back: FactTree = gran.load(ck).unwrap();
        assert_eq!(gran.frames_paged() - before, 1, "a load read more than its root");
        assert_eq!(back.len(), 50_000);
        assert_eq!(back.measure(), Count(50_000));
        assert_eq!(gran.frames_paged() - before, 1, "a whole-tree measure paged nodes in");

        // A range measure walks only the two boundary spines: every subtree
        // wholly inside the span answers from the measure its parent recorded.
        assert_eq!(back.measure_range(&t(1_000), &t(40_000)), Count(39_000));
        let range = gran.frames_paged() - before;
        assert!(range <= 5, "a range measure paged in {range} frames");

        // A point read pages in one root-to-leaf path.
        assert_eq!(back.get(&t(31_337)), Some(&1));
        let point = gran.frames_paged() - before - range;
        assert!(point <= 2, "a point read paged in {point} frames");

        // Re-persisting a paged tree writes and pages nothing.
        let (encoded, paged) = (gran.frames_encoded(), gran.frames_paged());
        gran.persist(&back).unwrap();
        assert_eq!((gran.frames_encoded(), gran.frames_paged()), (encoded, paged));

        // A full walk reads it all, and it is the same map.
        assert_eq!(contents(&back), contents(&tree));
    }

    /// Trees may hold trees: a directory's values are links, persisted with it,
    /// followed by GC, and reloaded paged.
    #[test]
    fn linked_trees_persist_reload_and_survive_gc() {
        let dir = tempfile::tempdir().unwrap();
        let gran = Granfilade::open(dir.path()).unwrap();
        let mut d = DirTree::new();
        for e in 0..200u64 {
            let mut f = FactTree::new();
            for k in 0..(e as i64 % 7) * 40 {
                f = f.insert(t(k), e as i64);
            }
            d = d.insert(e, f.relocate(0));
        }
        let (ck, nodes) = gran.collect_tree(&d);
        gran.write_group(vec![StagedWrite { nodes, root: vec![ck] }]).unwrap();
        // Everything is reachable through the links: GC keeps it all.
        assert_eq!(gran.gc().unwrap(), 0, "GC collected a linked tree");

        let back: DirTree = gran.load(gran.root().unwrap()[0]).unwrap();
        assert_eq!(back.len(), 200);
        for e in [0u64, 6, 13, 199] {
            assert_eq!(contents(back.get(&e).unwrap()), contents(d.get(&e).unwrap()), "dir entry {e}");
        }
    }

    /// GC keeps the frames of paged nodes that are still reachable from memory
    /// but not from the root, so a reader holding an old version can keep
    /// reading it after the root has moved on.
    #[test]
    fn gc_keeps_unread_paged_nodes_alive() {
        let dir = tempfile::tempdir().unwrap();
        let gran = Granfilade::open(dir.path()).unwrap();
        let mut tree = FactTree::new();
        // Squares: no step repeats, so no run folds the tree into one node.
        for k in 0..5_000i64 {
            tree = tree.insert(t(k * k), 1);
        }
        let (ck, nodes) = gran.collect_tree(&tree);
        gran.write_group(vec![StagedWrite { nodes, root: vec![ck] }]).unwrap();
        let held: FactTree = gran.load(ck).unwrap();

        // The root moves to an empty world; only `held` still reaches the old one.
        gran.write_group(vec![StagedWrite { nodes: Vec::new(), root: vec![None] }]).unwrap();
        gran.gc().unwrap();
        assert_eq!(held.len(), 5_000);
        assert_eq!(held.iter().count(), 5_000, "GC swept a frame a live handle still needed");

        // Once nothing holds it, it goes.
        drop(held);
        assert!(gran.gc().unwrap() > 0);
        assert_eq!(gran.node_count().unwrap(), 0);
    }

    /// A store from before the root record keeps its roots under keys this
    /// version never reads; opening it must fail loudly, not look empty.
    #[test]
    fn a_store_without_a_root_record_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Database::builder(dir.path()).open().unwrap();
            let meta = db.keyspace("meta", KeyspaceCreateOptions::default).unwrap();
            meta.insert(b"cur:\0\0\0\0\0\0\0\0".to_vec(), 7u64.to_be_bytes().to_vec()).unwrap();
            db.persist(PersistMode::SyncAll).unwrap();
        }
        let err = Granfilade::open(dir.path()).err().expect("an old store opened").to_string();
        assert!(err.contains("no root record"), "{err}");
        assert!(err.contains("fresh store"), "{err}");
    }

    #[test]
    fn pre_v8_node_is_rejected_with_fresh_store_guidance() {
        let old = [7, TAG_LEAF, 0, 0, 0, 0];
        let err = decode_header(&old).unwrap_err().to_string();
        assert!(err.contains("unsupported node format version 7"));
        assert!(err.contains("fresh store"));
        assert!(err.contains("no migrator"));
    }
}
