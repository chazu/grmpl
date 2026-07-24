//! # grmpl-store
//!
//! fjall-backed implementation of the `TraceStore`/`EditionStore` boundary
//! (DESIGN.md §4.1). This is the only crate that names fjall. Nothing here
//! escapes above the bright line: callers see `Edition`s, never `SeqNo`s.
//!
//! ## Physical layout
//!
//! * One fjall keyspace per relation, named `rel_{id}`.
//! * One meta keyspace `__meta` holding the current edition under key `edition`
//!   and the name→`RelId` catalog under keys `cat:{name}` → `RelId(4, BE)`
//!   (the [`Catalog`] boundary — see `grmpl_core::store`).
//! * Within a relation keyspace: `key = edition(8, BE) || counter(8, BE)`,
//!   `value = version(1) || diff(8, LE) || encoded_tuple` (record framing in
//!   `codec`; the version byte is `grmpl_core::wire::FORMAT_VERSION`).
//!
//! The diff-accumulation ("sum diffs at edition ≤ E") lives here, above the KV
//! store — fjall is used purely as an ordered, atomically-batchable byte map
//! (DESIGN.md §10, risk 3). An edition's data write and the meta edition-bump
//! land in a single atomic `batch()`, so the Patch–edition law holds.

mod codec;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use fjall::{Database, KeyspaceCreateOptions, PersistMode};
use grmpl_core::{
    Catalog, Diff, Edition, EditionStore, Error, Result, RelId, TraceStore, Tuple, Update,
};

const META_KS: &str = "__meta";
const EDITION_KEY: &[u8] = b"edition";
/// Key prefix under `__meta` for catalog entries: `cat:{name}` → `RelId(4, BE)`.
const CAT_PREFIX: &[u8] = b"cat:";

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// A fjall-backed trace store for a single authority domain.
pub struct FjallStore {
    db: Database,
    meta: fjall::Keyspace,
    /// Cache of relation keyspace handles.
    rels: Mutex<HashMap<RelId, fjall::Keyspace>>,
    /// The current edition. Guards edition allocation so commits are serialized
    /// within the domain (Authority law: one domain, one commit clock).
    current: Mutex<u64>,
}

impl FjallStore {
    /// Open (or create) a store rooted at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<FjallStore> {
        let db = Database::builder(path.as_ref())
            .open()
            .map_err(map_err)?;
        let meta = db
            .keyspace(META_KS, KeyspaceCreateOptions::default)
            .map_err(map_err)?;
        let current = match meta.get(EDITION_KEY).map_err(map_err)? {
            Some(bytes) => u64_be(bytes.as_ref()),
            None => 0,
        };
        Ok(FjallStore {
            db,
            meta,
            rels: Mutex::new(HashMap::new()),
            current: Mutex::new(current),
        })
    }

    fn keyspace_for(&self, rel: RelId) -> Result<fjall::Keyspace> {
        let mut rels = self.rels.lock().unwrap();
        if let Some(ks) = rels.get(&rel) {
            return Ok(ks.clone());
        }
        let name = format!("rel_{}", rel.0);
        let ks = self
            .db
            .keyspace(&name, KeyspaceCreateOptions::default)
            .map_err(map_err)?;
        rels.insert(rel, ks.clone());
        Ok(ks)
    }
}

impl EditionStore for FjallStore {
    fn current(&self) -> Edition {
        Edition(*self.current.lock().unwrap())
    }
}

impl FjallStore {
    /// Whether `tuple` has positive accumulated weight in `rel` as-of edition
    /// `at`. Used by the atomic precondition re-check.
    fn holds_at(&self, rel: RelId, tuple: &Tuple, at: u64) -> Result<bool> {
        let ks = self.keyspace_for(rel)?;
        let mut sum: Diff = 0;
        for kv in ks.iter() {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            if key_edition(k.as_ref()) > at {
                continue;
            }
            let (diff, t) = codec::decode_record(v.as_ref())?;
            if &t == tuple {
                sum += diff;
            }
        }
        Ok(sum > 0)
    }

    /// The shared write path: apply `updates` at edition `next` in one atomic
    /// batch (data + edition bump together).
    fn write_batch(&self, next: u64, updates: &[(RelId, Tuple, Diff)]) -> Result<()> {
        let mut batch = self.db.batch();
        let mut counter: u64 = 0;
        for (rel, tuple, diff) in updates {
            let ks = self.keyspace_for(*rel)?;
            let key = encode_key(next, counter);
            let val = codec::encode_record(*diff, tuple);
            batch.insert(&ks, key, val);
            counter += 1;
        }
        // Bump the edition in the same atomic batch.
        batch.insert(&self.meta, EDITION_KEY, next.to_be_bytes().to_vec());
        batch.commit().map_err(map_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(map_err)?;
        Ok(())
    }
}

impl TraceStore for FjallStore {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        // Serialize edition allocation with the write (Patch–edition law).
        let mut current = self.current.lock().unwrap();
        let next = *current + 1;
        self.write_batch(next, updates)?;
        *current = next;
        Ok(Edition(next))
    }

    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        // Hold the edition lock across BOTH the precondition re-check and the
        // write, so the compare-and-commit is atomic (DESIGN.md §5.2).
        let mut current = self.current.lock().unwrap();
        let at = *current;
        for (rel, tuple) in preconditions {
            if !self.holds_at(*rel, tuple, at)? {
                return Ok(None); // precondition failed → no effect
            }
        }
        let next = at + 1;
        self.write_batch(next, updates)?;
        *current = next;
        Ok(Some(Edition(next)))
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        let ks = self.keyspace_for(rel)?;
        let mut acc: HashMap<Tuple, Diff> = HashMap::new();
        for kv in ks.iter() {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            let edition = key_edition(k.as_ref());
            if edition > at.0 {
                continue;
            }
            let (diff, tuple) = codec::decode_record(v.as_ref())?;
            *acc.entry(tuple).or_insert(0) += diff;
        }
        // Consolidation runs over a HashMap, whose iteration order is not
        // stable; sort so `read_at` (the `find` primitive) is deterministic.
        let mut out: Vec<(Tuple, Diff)> = acc.into_iter().filter(|(_, d)| *d != 0).collect();
        out.sort();
        Ok(out)
    }

    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        let ks = self.keyspace_for(rel)?;
        let mut out = Vec::new();
        for kv in ks.iter() {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            let edition = key_edition(k.as_ref());
            if edition <= from.0 || edition > to.0 {
                continue;
            }
            let counter = key_counter(k.as_ref());
            let (diff, tuple) = codec::decode_record(v.as_ref())?;
            out.push((
                edition,
                counter,
                Update {
                    tuple,
                    time: grmpl_core::Time::input(edition),
                    diff,
                },
            ));
        }
        // Deterministic order: by edition, then by the physical write counter —
        // the exact order in which the updates were committed. Sorting on
        // edition alone would leave same-edition updates in fjall/scan order.
        out.sort_by_key(|(edition, counter, _)| (*edition, *counter));
        Ok(out.into_iter().map(|(_, _, u)| u).collect())
    }
}

impl Catalog for FjallStore {
    fn rel_id(&self, name: &str) -> Result<Option<RelId>> {
        match self.meta.get(cat_key(name)).map_err(map_err)? {
            Some(bytes) => Ok(Some(RelId(u32_be(bytes.as_ref())))),
            None => Ok(None),
        }
    }

    fn register(&self, name: &str, id: RelId) -> Result<()> {
        // Append-only: rebinding a name to a different id is a hard error.
        if let Some(existing) = Catalog::rel_id(self, name)? {
            if existing != id {
                return Err(Error::Store(format!(
                    "catalog conflict: `{name}` already bound to {} (cannot rebind to {})",
                    existing.0, id.0
                )));
            }
            return Ok(());
        }
        let mut batch = self.db.batch();
        batch.insert(&self.meta, cat_key(name), id.0.to_be_bytes().to_vec());
        batch.commit().map_err(map_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(map_err)?;
        Ok(())
    }

    fn entries(&self) -> Result<Vec<(String, RelId)>> {
        let mut out = Vec::new();
        for kv in self.meta.iter() {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            if let Some(name_bytes) = k.as_ref().strip_prefix(CAT_PREFIX) {
                let name = std::str::from_utf8(name_bytes).map_err(map_err)?.to_string();
                out.push((name, RelId(u32_be(v.as_ref()))));
            }
        }
        out.sort();
        Ok(out)
    }
}

fn cat_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(CAT_PREFIX.len() + name.len());
    k.extend_from_slice(CAT_PREFIX);
    k.extend_from_slice(name.as_bytes());
    k
}

fn encode_key(edition: u64, counter: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16);
    k.extend_from_slice(&edition.to_be_bytes());
    k.extend_from_slice(&counter.to_be_bytes());
    k
}

fn key_edition(k: &[u8]) -> u64 {
    u64_be(&k[..8])
}

fn key_counter(k: &[u8]) -> u64 {
    u64_be(&k[8..16])
}

fn u64_be(b: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&b[..8]);
    u64::from_be_bytes(buf)
}

fn u32_be(b: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&b[..4]);
    u32::from_be_bytes(buf)
}
