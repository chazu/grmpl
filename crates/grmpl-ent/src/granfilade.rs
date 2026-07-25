//! **The granfilade: content-addressed node persistence.**
//!
//! An enfilade [`Tree`] is made durable by storing each node under its **content
//! key** — a hash of `(key, value, left-child-key, right-child-key)`. Because the
//! key is a pure function of *content* (never `phys_id` or allocation order, per
//! plan v4.1), **equal subtrees store once**: two versions of a tree that differ
//! by one edited path share every untouched node on disk (Xanadu structural
//! sharing / Gold's granfilade, modernised as a content-addressed blob store).
//!
//! A commit persists only the `O(log n)` new nodes on the edited path; a load
//! reconstructs the exact tree shape (so content keys round-trip). Values are
//! serialized through the single `grmpl_core::wire` value codec ([`Persist`]).

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use fjall::{Database, KeyspaceCreateOptions, PersistMode};
use grmpl_core::{wire, Error, Result, Tuple, Value};

use crate::measure::Measure;
use crate::tree::Tree;

/// A 128-bit content key (two salted `SipHash` passes; `DefaultHasher` has fixed
/// keys, so the key is deterministic across processes — required to reload a
/// persisted store).
pub type ContentKey = [u8; 16];

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

/// The content-addressed node store for one or more enfilades, plus a small
/// `meta` keyspace for enfilade roots and the edition clock.
pub struct Granfilade {
    db: Database,
    nodes: fjall::Keyspace,
    meta: fjall::Keyspace,
}

impl Granfilade {
    /// Open (or create) a granfilade rooted at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Granfilade> {
        let db = Database::builder(path.as_ref()).open().map_err(store_err)?;
        let nodes = db.keyspace("nodes", KeyspaceCreateOptions::default).map_err(store_err)?;
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).map_err(store_err)?;
        Ok(Granfilade { db, nodes, meta })
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
        let ck = collect_nodes(tree, &mut out);
        (ck, out)
    }

    /// One atomic write of node frames + meta entries (nodes and the roots that
    /// reference them land together — the Patch–edition law for the store).
    pub fn write(&self, nodes: Vec<(ContentKey, Vec<u8>)>, meta: Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
        let mut batch = self.db.batch();
        for (k, v) in nodes {
            batch.insert(&self.nodes, k.to_vec(), v);
        }
        for (k, v) in meta {
            batch.insert(&self.meta, k, v);
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
        Ok(())
    }

    /// A meta value.
    pub fn meta_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.meta.get(key).map_err(store_err)?.map(|s| s.as_ref().to_vec()))
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

    /// Persist `tree`, returning its root content key (`None` if empty). Writes
    /// only nodes not already present-by-content; unchanged subtrees of a prior
    /// version are shared automatically. One atomic batch + `SyncAll`.
    pub fn persist<K, V, M>(&self, tree: &Tree<K, V, M>) -> Result<Option<ContentKey>>
    where
        K: Persist + Ord + Clone,
        V: Persist + Clone,
        M: Measure<K, V>,
    {
        let mut out: Vec<(ContentKey, Vec<u8>)> = Vec::new();
        let ck = collect_nodes(tree, &mut out);
        let mut batch = self.db.batch();
        for (k, v) in out {
            batch.insert(&self.nodes, k.to_vec(), v);
        }
        batch.commit().map_err(store_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(store_err)?;
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
        let (key, pos) = K::decode(bytes, 0)?;
        let (val, pos) = V::decode(bytes, pos)?;
        let (lck, pos) = decode_ck(bytes, pos)?;
        let (rck, _pos) = decode_ck(bytes, pos)?;
        let left = self.load(lck)?;
        let right = self.load(rck)?;
        Ok(Tree::from_parts(key, val, left, right))
    }

    /// Number of distinct nodes stored — for structural-sharing verification.
    pub fn node_count(&self) -> Result<usize> {
        Ok(self.nodes.iter().count())
    }
}

/// Recurse the tree, appending each node's `(content_key, frame_bytes)` and
/// returning the root key. Children first, so a node's frame carries its
/// children's content keys.
fn collect_nodes<K, V, M>(tree: &Tree<K, V, M>, out: &mut Vec<(ContentKey, Vec<u8>)>) -> Option<ContentKey>
where
    K: Persist + Ord + Clone,
    V: Persist + Clone,
    M: Measure<K, V>,
{
    let (key, val, left, right) = tree.root_parts()?;
    let lck = collect_nodes(left, out);
    let rck = collect_nodes(right, out);
    let mut bytes = Vec::new();
    key.encode(&mut bytes);
    val.encode(&mut bytes);
    encode_ck(&mut bytes, lck);
    encode_ck(&mut bytes, rck);
    let ck = hash128(&bytes);
    out.push((ck, bytes));
    Some(ck)
}

fn encode_ck(out: &mut Vec<u8>, ck: Option<ContentKey>) {
    match ck {
        None => out.push(0),
        Some(k) => {
            out.push(1);
            out.extend_from_slice(&k);
        }
    }
}

fn decode_ck(bytes: &[u8], pos: usize) -> Result<(Option<ContentKey>, usize)> {
    match bytes.get(pos) {
        Some(0) => Ok((None, pos + 1)),
        Some(1) => {
            let end = pos + 1 + 16;
            let b = bytes.get(pos + 1..end).ok_or_else(|| trunc("content key"))?;
            Ok((Some(b.try_into().unwrap()), end))
        }
        _ => Err(trunc("content-key flag")),
    }
}

/// 128-bit content hash: two `SipHash` passes over the frame with distinct salts.
fn hash128(bytes: &[u8]) -> ContentKey {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&salted(0xA1, bytes).to_be_bytes());
    out[8..].copy_from_slice(&salted(0xB2, bytes).to_be_bytes());
    out
}

fn salted(salt: u8, bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write_u8(salt);
    h.write(bytes);
    h.finish()
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
}
