//! **The granfilade: content-addressed node persistence.**
//!
//! An enfilade [`Tree`] is made durable by storing each node under its **content
//! key** — the [`sha256`](crate::hash::sha256) of its frame, which closes over
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
//! **Path-only, in work as well as in bytes (G-1).** A commit adds only the
//! `O(log n)` new nodes on the edited path, *and* only visits them: each node
//! memoizes its content key ([`Tree::ck_cell`]), and a node whose key is both
//! memoized and already durable here is returned without being re-serialized —
//! and so are all its descendants, since a node's key closes over its children's.
//! Previously the *traversal* was `O(n)` per commit even though the growth was
//! path-sized.
//!
//! The memo alone is not enough, which is the subtlety that had this deferred: a
//! memoized key says "this is its key", not "it is on *this* disk". A fork
//! clones nodes whose keys were memoized against its parent's granfilade, so
//! each granfilade also tracks the keys it knows are durable, and a subtree is
//! skipped only when both hold. (There is no pointer-ABA to worry about: nodes
//! are immutable and `Arc`-held, so the key is a pure function of the node and
//! caching it is plain memoization.)
//!
//! A load reconstructs the exact tree shape — and memoizes each key it read — so
//! content keys round-trip and re-persisting a reloaded tree writes nothing.
//! Values are serialized through the single `grmpl_core::wire` value codec
//! ([`Persist`]).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use fjall::{Database, KeyspaceCreateOptions, PersistMode};
use grmpl_core::{wire, Error, Result, Tuple, Value};

use crate::measure::Measure;
use crate::tree::{NodeRef, Tree};

pub use crate::hash::ContentKey;

/// The width of a [`ContentKey`] on the wire.
const CK_LEN: usize = 32;

/// Node frames ride the one workspace format version. The v4 Float/behavior-IR
/// cutover is deliberately fresh-store-only, so a v4 binary rejects every v3
/// (and older) persisted node before attempting to interpret its payload.
const NODE_FORMAT_VERSION: u8 = wire::FORMAT_VERSION;

/// A type that can be (de)serialized into a node frame. Payloads reuse the one
/// `grmpl_core::wire` codec, so "one serialization" holds.
pub trait Persist: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)>;
}

impl Persist for i64 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let end = pos + 8;
        let b = bytes.get(pos..end).ok_or_else(|| trunc("i64"))?;
        Ok((i64::from_be_bytes(b.try_into().unwrap()), end))
    }
}

impl Persist for u64 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let end = pos + 8;
        let b = bytes.get(pos..end).ok_or_else(|| trunc("u64"))?;
        Ok((u64::from_be_bytes(b.try_into().unwrap()), end))
    }
}

impl Persist for u32 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let end = pos + 4;
        let b = bytes.get(pos..end).ok_or_else(|| trunc("u32"))?;
        Ok((u32::from_be_bytes(b.try_into().unwrap()), end))
    }
}

impl<A: Persist, B: Persist, C: Persist> Persist for (A, B, C) {
    fn encode(&self, out: &mut Vec<u8>) {
        self.0.encode(out);
        self.1.encode(out);
        self.2.encode(out);
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let (a, pos) = A::decode(bytes, pos)?;
        let (b, pos) = B::decode(bytes, pos)?;
        let (c, pos) = C::decode(bytes, pos)?;
        Ok(((a, b, c), pos))
    }
}

impl<A: Persist, B: Persist> Persist for (A, B) {
    fn encode(&self, out: &mut Vec<u8>) {
        self.0.encode(out);
        self.1.encode(out);
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        let (a, pos) = A::decode(bytes, pos)?;
        let (b, pos) = B::decode(bytes, pos)?;
        Ok(((a, b), pos))
    }
}

impl Persist for Tuple {
    fn encode(&self, out: &mut Vec<u8>) {
        wire::encode_tuple(self, out);
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        wire::decode_tuple(bytes, pos)
    }
}

/// Also allow a bare `Value` payload (single-column keys, etc.).
impl Persist for Value {
    fn encode(&self, out: &mut Vec<u8>) {
        wire::encode_value(self, out);
    }
    fn decode(bytes: &[u8], pos: usize) -> Result<(Self, usize)> {
        wire::decode_value(bytes, pos)
    }
}

/// One commit's worth of durable work, **already encoded and hashed** but not
/// yet written: the node frames it adds, the meta entries (enfilade roots, the
/// clock) that name them, and any meta keys it retires.
///
/// Encoding is pure and needs only the immutable trees, so it happens inside the
/// store's edition lock; the batch and its `fsync` happen outside, where a group
/// of them can share one. Splitting the two is what [`write_group`] exists for.
///
/// [`write_group`]: Granfilade::write_group
pub struct StagedWrite {
    pub nodes: Vec<(ContentKey, Vec<u8>)>,
    pub meta: Vec<(Vec<u8>, Vec<u8>)>,
    pub drop_meta: Vec<Vec<u8>>,
}

/// The content-addressed node store for one or more enfilades, plus a small
/// `meta` keyspace for enfilade roots and the edition clock.
pub struct Granfilade {
    db: Database,
    nodes: fjall::Keyspace,
    meta: fjall::Keyspace,
    /// **Keys known to be durable in *this* granfilade (G-1).** A memoized
    /// content key on a node says "this is its key", not "it is on this disk" —
    /// a fork clones nodes whose keys were memoized against the *parent's*
    /// granfilade. Without this set, a commit would skip writing them and leave
    /// a root pointing at a node that was never stored. Populated on every write
    /// and every load.
    present: Mutex<HashSet<ContentKey>>,
    /// **Ops counter (G-0a).** Node frames serialized+hashed since this handle
    /// was opened. Sublinear claims about the commit path are otherwise only
    /// prose; this is what lets a test *fail* when an `O(log n)` walk quietly
    /// becomes a scan. One relaxed atomic add per node, against a SHA-256 — not
    /// measurable.
    encoded: AtomicU64,
    /// **Durability ops counter (group commit).** `SyncAll`s issued since this
    /// handle was opened.
    ///
    /// The same discipline as `encoded`, applied to what actually costs: every
    /// axis of `docs/PERFORMANCE-ENT.md` bottoms out on this ~1 ms call. Without
    /// a counter, "N committers now share one fsync" is prose; with it, a test
    /// can *fail* when grouping silently stops happening and the write path
    /// quietly returns to one fsync per committer.
    syncs: AtomicU64,
}

impl Granfilade {
    /// Open (or create) a granfilade rooted at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Granfilade> {
        let db = Database::builder(path.as_ref()).open().map_err(store_err)?;
        let nodes = db
            .keyspace("nodes", KeyspaceCreateOptions::default)
            .map_err(store_err)?;
        let meta = db
            .keyspace("meta", KeyspaceCreateOptions::default)
            .map_err(store_err)?;
        Ok(Granfilade {
            db,
            nodes,
            meta,
            present: Mutex::new(HashSet::new()),
            encoded: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        })
    }

    /// Collect a tree's nodes as `(content_key, frame)` pairs (children first) and
    /// its root key — pure, no I/O. The caller batches these with meta so nodes
    /// land **before/with** the roots that reference them (crash-safety).
    pub fn collect_tree<K, V, M>(
        &self,
        tree: &Tree<K, V, M>,
    ) -> (Option<ContentKey>, Vec<(ContentKey, Vec<u8>)>)
    where
        K: Persist + Ord + Clone,
        V: Persist + Clone,
        M: Measure<K, V>,
    {
        let mut out = Vec::new();
        let ck = {
            let present = self.present.lock().unwrap();
            collect_nodes(tree, &mut out, &present)
        };
        self.encoded.fetch_add(out.len() as u64, Ordering::Relaxed);
        (ck, out)
    }

    /// One atomic write of node frames + meta entries (nodes and the roots that
    /// reference them land together — the Patch–edition law for the store).
    pub fn write(
        &self,
        nodes: Vec<(ContentKey, Vec<u8>)>,
        meta: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        self.write_full(nodes, meta, Vec::new())
    }

    /// [`write`](Self::write), plus meta keys to drop in the same batch — used by
    /// consolidation to retire the Fact roots below the new watermark atomically
    /// with the checkpoint that replaces them.
    pub fn write_full(
        &self,
        nodes: Vec<(ContentKey, Vec<u8>)>,
        meta: Vec<(Vec<u8>, Vec<u8>)>,
        drop_meta: Vec<Vec<u8>>,
    ) -> Result<()> {
        self.write_group(vec![StagedWrite {
            nodes,
            meta,
            drop_meta,
        }])
    }

    /// **Group commit (the durability half).** Apply several already-encoded
    /// writes as **one** batch and **one** `SyncAll`.
    ///
    /// Every axis of `docs/PERFORMANCE-ENT.md` bottoms out on that single
    /// ~1 ms fsync, so N committers writing one at a time pay N fsyncs strictly
    /// in series. Amortising one fsync across a group is the largest single win
    /// available, and it does **not** weaken the patch–edition law: the law
    /// demands one atomic durable write *per edition*, not one *per committer*.
    /// A batch containing editions `e..=e+k` still lands entirely or not at all.
    ///
    /// `group` must be in **edition order**. Later entries overwrite earlier ones
    /// for a repeated meta key (the clock, a relation's log root), which is
    /// exactly right: the last edition in the group is the state the batch
    /// leaves behind. Writing them out of order would durably record a stale
    /// clock; skipping one would leave a hole in the history a reopen could not
    /// see past.
    pub fn write_group(&self, group: Vec<StagedWrite>) -> Result<()> {
        if group.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        let mut written = Vec::new();
        for staged in group {
            for (k, v) in staged.nodes {
                batch.insert(&self.nodes, k.to_vec(), v);
                written.push(k);
            }
            for (k, v) in staged.meta {
                batch.insert(&self.meta, k, v);
            }
            for k in staged.drop_meta {
                batch.remove(&self.meta, k);
            }
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        // Only after the batch is durable may these count as present — otherwise
        // a crash mid-write would leave the memo claiming a node is on disk.
        self.present.lock().unwrap().extend(written);
        Ok(())
    }

    /// A meta value.
    pub fn meta_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .meta
            .get(key)
            .map_err(store_err)?
            .map(|s| s.as_ref().to_vec()))
    }

    /// All meta `(key, value)` pairs whose key starts with `prefix`.
    pub fn meta_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for kv in self.meta.prefix(prefix) {
            let (k, v) = kv.into_inner().map_err(store_err)?;
            out.push((k.as_ref().to_vec(), v.as_ref().to_vec()));
        }
        Ok(out)
    }

    /// Persist `tree`, returning its root content key (`None` if empty). Every
    /// node is content-keyed, so a node already present by content is re-inserted
    /// idempotently and unchanged subtrees of a prior version are shared on disk;
    /// the store grows by only the edited path. One atomic batch + `SyncAll`.
    pub fn persist<K, V, M>(&self, tree: &Tree<K, V, M>) -> Result<Option<ContentKey>>
    where
        K: Persist + Ord + Clone,
        V: Persist + Clone,
        M: Measure<K, V>,
    {
        let (ck, out) = self.collect_tree(tree);
        self.write(out, Vec::new())?;
        Ok(ck)
    }

    /// Reconstruct the tree rooted at `ck` (its exact persisted shape). `O(state)`
    /// eager load; lazy paging is a later refinement.
    pub fn load<K, V, M>(&self, ck: Option<ContentKey>) -> Result<Tree<K, V, M>>
    where
        K: Persist + Ord + Clone,
        V: Persist + Clone,
        M: Measure<K, V>,
    {
        let ck = match ck {
            None => return Ok(Tree::new()),
            Some(ck) => ck,
        };
        let bytes = self
            .nodes
            .get(ck)
            .map_err(store_err)?
            .ok_or_else(|| Error::Store("granfilade: node key not found".into()))?;
        let bytes = bytes.as_ref();
        let (tag, child_keys, pos) = decode_header(bytes)?;
        let (count, mut pos) = decode_u32(bytes, pos)?;
        match tag {
            TAG_LEAF => {
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let (key, p) = K::decode(bytes, pos)?;
                    let (val, p) = V::decode(bytes, p)?;
                    entries.push((key, val));
                    pos = p;
                }
                let tree = Tree::leaf_of(entries);
                self.note_loaded(&tree, ck);
                Ok(tree)
            }
            TAG_INTERNAL => {
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    let (key, p) = K::decode(bytes, pos)?;
                    keys.push(key);
                    pos = p;
                }
                let mut children = Vec::with_capacity(child_keys.len());
                for c in child_keys {
                    children.push(self.load(Some(c))?);
                }
                if keys.len() + 1 != children.len() {
                    return Err(Error::Codec("granfilade: malformed internal node".into()));
                }
                let tree = Tree::internal_of(keys, children);
                self.note_loaded(&tree, ck);
                Ok(tree)
            }
            _ => Err(Error::Codec(format!("granfilade: unknown node tag {tag}"))),
        }
    }

    /// A just-loaded node already has a content key and is by definition durable:
    /// memoize both, so re-persisting a reloaded tree writes nothing.
    fn note_loaded<K, V, M>(&self, tree: &Tree<K, V, M>, ck: ContentKey)
    where
        K: Persist + Ord + Clone,
        V: Persist + Clone,
        M: Measure<K, V>,
    {
        if let Some(cell) = tree.ck_cell() {
            let _ = cell.set(ck);
        }
        self.present.lock().unwrap().insert(ck);
    }

    /// Node frames serialized and hashed since this handle was opened — the
    /// [`encoded`](Self::encoded) ops counter. A commit's delta is the *work* it
    /// did, as distinct from the nodes it added: before G-1 the two diverged
    /// wildly, because an untouched subtree was re-serialized to rediscover a
    /// key the store already had.
    pub fn frames_encoded(&self) -> u64 {
        self.encoded.load(Ordering::Relaxed)
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

    /// **Reachability GC (E3).** Collect every node unreachable from a live root
    /// (the `log:*`, `fact:*` and `ctx:*` roots recorded in meta): mark reachable nodes by
    /// walking their child keys from the roots, then sweep the rest. Returns the
    /// number of nodes collected. Type-agnostic — it reads only the leading child
    /// keys of each frame. (Serialize this with commits at the store level.)
    pub fn gc(&self) -> Result<usize> {
        // Roots: every content key referenced by a meta entry.
        let mut stack: Vec<ContentKey> = Vec::new();
        // `ctx:` is a root too: the catalog and schema registry live in the
        // context enfilade, and collecting them would lose the world's names.
        for prefix in [b"log:".as_ref(), b"fact:".as_ref(), b"ctx:".as_ref()] {
            for (_k, v) in self.meta_prefix(prefix)? {
                if v.first() == Some(&1) {
                    if let Some(b) = v.get(1..1 + CK_LEN) {
                        stack.push(b.try_into().unwrap());
                    }
                }
            }
        }
        // Mark.
        let mut marked: std::collections::HashSet<ContentKey> = std::collections::HashSet::new();
        while let Some(ck) = stack.pop() {
            if !marked.insert(ck) {
                continue;
            }
            if let Some(frame) = self.nodes.get(ck).map_err(store_err)? {
                stack.extend(children_of(frame.as_ref())?);
            }
        }
        // Sweep.
        let mut collected = 0usize;
        let mut batch = self.db.batch();
        for kv in self.nodes.iter() {
            let (k, _v) = kv.into_inner().map_err(store_err)?;
            let key: ContentKey = k.as_ref().try_into().map_err(|_| trunc("node key"))?;
            if !marked.contains(&key) {
                batch.remove(&self.nodes, k.as_ref().to_vec());
                collected += 1;
            }
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(collected)
    }
}

/// A leaf frame: a run of `(key, value)` entries, no children.
const TAG_LEAF: u8 = 0;
/// An internal frame: child content keys plus the separators dividing them.
const TAG_INTERNAL: u8 = 1;

/// The child content keys of a node frame, read **without decoding the payload**
/// — the frame puts the tag and the child-key run first precisely so GC can walk
/// references without knowing `K` or `V`. Used by [`Granfilade::gc`].
fn children_of(frame: &[u8]) -> Result<Vec<ContentKey>> {
    let (_tag, children, _pos) = decode_header(frame)?;
    Ok(children)
}

/// Frame header: `version(1) || tag(1) || n_children(u32 BE) || [content_key]*n`.
/// Returns the tag, the child keys, and the offset where the payload count
/// begins. The child-key run leads the frame so GC can walk references without
/// knowing `K` or `V`.
fn decode_header(frame: &[u8]) -> Result<(u8, Vec<ContentKey>, usize)> {
    match frame.first() {
        Some(&NODE_FORMAT_VERSION) => {}
        Some(v) => {
            return Err(Error::Codec(format!(
                "granfilade: unsupported node format version {v} (expected {}; \
                 v4 requires a fresh store and has no migrator)",
                NODE_FORMAT_VERSION
            )))
        }
        None => return Err(trunc("node version")),
    }
    let tag = *frame.get(1).ok_or_else(|| trunc("node tag"))?;
    let (n, mut pos) = decode_u32(frame, 2)?;
    let mut children = Vec::with_capacity(n);
    for _ in 0..n {
        let end = pos + CK_LEN;
        let b = frame.get(pos..end).ok_or_else(|| trunc("content key"))?;
        children.push(b.try_into().unwrap());
        pos = end;
    }
    Ok((tag, children, pos))
}

fn decode_u32(bytes: &[u8], pos: usize) -> Result<(usize, usize)> {
    let end = pos + 4;
    let b = bytes.get(pos..end).ok_or_else(|| trunc("count"))?;
    Ok((u32::from_be_bytes(b.try_into().unwrap()) as usize, end))
}

fn encode_header(out: &mut Vec<u8>, tag: u8, children: &[ContentKey]) {
    out.push(NODE_FORMAT_VERSION);
    out.push(tag);
    out.extend_from_slice(&(children.len() as u32).to_be_bytes());
    for c in children {
        out.extend_from_slice(c);
    }
}

/// Recurse the tree, appending each node's `(content_key, frame_bytes)` and
/// returning the root key. Children first, so a node's frame carries its
/// children's content keys.
///
/// One frame is one **node**, and a node holds a whole run of entries
/// ([`grmpl_ent::tree::B`](crate::tree::B) of them), so the store keeps one
/// record per run rather than one per tuple.
fn collect_nodes<K, V, M>(
    tree: &Tree<K, V, M>,
    out: &mut Vec<(ContentKey, Vec<u8>)>,
    present: &HashSet<ContentKey>,
) -> Option<ContentKey>
where
    K: Persist + Ord + Clone,
    V: Persist + Clone,
    M: Measure<K, V>,
{
    let cell = tree.ck_cell()?;
    // **Path-only work (G-1).** A node whose key is memoized *and* already
    // durable here needs neither re-serializing nor revisiting — and neither do
    // any of its descendants, since a node's key closes over its children's.
    // That turns a commit from an O(n) re-walk of the whole tree into O(log n):
    // only the path copied by the edit is new.
    if let Some(ck) = cell.get() {
        if present.contains(ck) {
            return Some(*ck);
        }
    }
    let mut bytes = Vec::new();
    match tree.node()? {
        NodeRef::Leaf(entries) => {
            encode_header(&mut bytes, TAG_LEAF, &[]);
            bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
            for (k, v) in entries {
                k.encode(&mut bytes);
                v.encode(&mut bytes);
            }
        }
        NodeRef::Internal(keys, children) => {
            let cks: Vec<ContentKey> = children
                .iter()
                .filter_map(|c| collect_nodes(c, out, present))
                .collect();
            encode_header(&mut bytes, TAG_INTERNAL, &cks);
            bytes.extend_from_slice(&(keys.len() as u32).to_be_bytes());
            for k in keys {
                k.encode(&mut bytes);
            }
        }
    }
    let ck = crate::hash::sha256(&bytes);
    let _ = cell.set(ck);
    out.push((ck, bytes));
    Some(ck)
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
    use crate::measure::Count;

    type FactTree = Tree<Tuple, i64, Count>;

    fn t(n: i64) -> Tuple {
        Tuple::from([Value::Int(n)])
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
        let orig: Vec<(Tuple, i64)> = v0.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let back: Vec<(Tuple, i64)> = reloaded.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(orig, back, "reload did not reproduce the tree's contents");

        // A new version shares every untouched subtree: persisting it adds only
        // the O(log n) nodes on the edited path, not a full copy.
        let before = gran.node_count().unwrap();
        let v1 = v0.insert(t(100), 999);
        gran.persist(&v1).unwrap();
        let added = gran.node_count().unwrap() - before;
        assert!(added > 0, "nothing was stored for the new version");
        assert!(
            added < v1.len(),
            "no structural sharing: added {added} nodes for a {}-node tree",
            v1.len()
        );
    }

    #[test]
    fn pre_v4_node_is_rejected_with_fresh_store_guidance() {
        let old = [3, TAG_LEAF, 0, 0, 0, 0];
        let err = decode_header(&old).unwrap_err().to_string();
        assert!(err.contains("unsupported node format version 3"));
        assert!(err.contains("fresh store"));
        assert!(err.contains("no migrator"));
    }
}
