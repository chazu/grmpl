//! # grmpl-store
//!
//! fjall-backed implementation of the `TraceStore`/`EditionStore` boundary
//! (DESIGN.md §4.1). This is the only crate that names fjall. Nothing here
//! escapes above the bright line: callers see `Edition`s, never `SeqNo`s.
//!
//! ## Physical layout
//!
//! * One fjall keyspace per relation, named `rel_{id}`.
//! * One meta keyspace `__meta` holding the current edition under key `edition`,
//!   the consolidation watermark under key `watermark` → `u64(8, BE)` (the P6
//!   history/GC horizon), the name→`RelId` catalog under keys `cat:{name}` →
//!   `RelId(4, BE)` (the [`Catalog`] boundary), and the schema registry under
//!   keys `sch:{rel(4, BE)}{edition(8, BE)}` → `encode_schema` bytes — one row
//!   per schema version, versioned by the introducing edition (the
//!   [`SchemaCatalog`] boundary). See `grmpl_core::store`.
//! * Within a relation keyspace: `key = edition(8, BE) || counter(8, BE)`,
//!   `value = version(1) || diff(8, LE) || encoded_tuple` (record framing in
//!   `codec`; the version byte is `grmpl_core::wire::FORMAT_VERSION`).
//!
//! ## History, checkpoints and GC (P6)
//!
//! Editions are allocated `1, 2, …`; edition `0` is the empty world and is
//! **never** a real commit. That leaves the `edition = 0` key range
//! (`0u64(8, BE) || seq(8, BE)`) reserved and disjoint from every raw update —
//! we use it as the per-relation **checkpoint**: the consolidated `(tuple,
//! diff)` state as-of the watermark, one row per surviving tuple.
//! [`consolidate`](TraceStore::consolidate) folds a relation's checkpoint plus
//! its raw updates in `(watermark, target]` into a fresh checkpoint, **deletes**
//! the folded raw rows, and bumps the watermark — all in one atomic `batch()`.
//! An as-of [`read_at`](TraceStore::read_at) is then `checkpoint + tail`
//! (`O(state + updates since the watermark)`) rather than a full-history scan,
//! and reads/scans below the watermark are rejected loudly — the four *edition
//! doors* (`grmpl_core::store`).
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
    wire, Catalog, Diff, Edition, EditionStore, Error, Result, RelId, Schema, SchemaCatalog,
    TraceStore, Tuple, Update,
};

const META_KS: &str = "__meta";
const EDITION_KEY: &[u8] = b"edition";
/// Key under `__meta` for the P6 consolidation watermark: `u64(8, BE)`.
const WATERMARK_KEY: &[u8] = b"watermark";
/// Key prefix under `__meta` for catalog entries: `cat:{name}` → `RelId(4, BE)`.
const CAT_PREFIX: &[u8] = b"cat:";
/// Key prefix under `__meta` for schema versions:
/// `sch:{rel(4, BE)}{edition(8, BE)}` → `encode_schema` bytes.
const SCH_PREFIX: &[u8] = b"sch:";
/// Reserved key prefix for checkpoint rows inside a relation keyspace: the
/// `edition = 0` range (`0u64(8, BE) || seq(8, BE)`), disjoint from every raw
/// update (whose edition is ≥ 1). See the module-level "History" note.
const CKPT_PREFIX: [u8; 8] = [0u8; 8];

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// One physical row of a [`FjallStore::canonical_dump`]: `(keyspace, key,
/// value)`, all raw bytes. Two stores are bit-identical iff their dumps
/// (sorted vectors of these) are equal.
pub type DumpRow = (String, Vec<u8>, Vec<u8>);

/// A fjall-backed trace store for a single authority domain.
pub struct FjallStore {
    db: Database,
    meta: fjall::Keyspace,
    /// Cache of relation keyspace handles.
    rels: Mutex<HashMap<RelId, fjall::Keyspace>>,
    /// The current edition. Guards edition allocation so commits are serialized
    /// within the domain (Authority law: one domain, one commit clock).
    current: Mutex<u64>,
    /// The consolidation watermark (P6): editions ≤ this are collapsed into
    /// checkpoints and their raw updates discarded. Monotonic. Read on the door
    /// paths (`read_at`/`scan_updates`); advanced only by `consolidate`.
    watermark: Mutex<u64>,
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
        let watermark = match meta.get(WATERMARK_KEY).map_err(map_err)? {
            Some(bytes) => u64_be(bytes.as_ref()),
            None => 0,
        };
        Ok(FjallStore {
            db,
            meta,
            rels: Mutex::new(HashMap::new()),
            current: Mutex::new(current),
            watermark: Mutex::new(watermark),
        })
    }

    /// Fork the whole domain into a new store rooted at `path` (P10 **forks
    /// v1**).
    ///
    /// Copies every relation keyspace (`rel_*`) and the `__meta` keyspace — the
    /// edition clock, the consolidation watermark, the name→`RelId` catalog and
    /// the schema registry — verbatim, so the fork is **bit-identical** to this
    /// store at the moment of the copy (`canonical_dump` agrees) and evolves
    /// independently thereafter. Cost is **O(domain)**: the physical size of the
    /// store (per-relation checkpoints plus the live tail), not O(history) — a
    /// consolidated store forks in proportion to its *state*, not its past.
    ///
    /// The copy is taken under the edition lock, so it is a consistent cut: no
    /// commit lands between reading one keyspace and the next.
    ///
    /// **Deferred to P15 (fork identity / clock semantics).** The fork inherits
    /// this store's edition numbering and catalog ids unchanged — it is a plain
    /// *copy*, not a re-baselined child. Whether a fork receives a fresh domain
    /// identity, how its clock relates to the parent's, and cross-fork merge are
    /// all P15 concerns; v1 is deliberately the flat O(domain) copy, stated
    /// plainly.
    pub fn fork(&self, path: impl AsRef<Path>) -> Result<FjallStore> {
        // Hold the edition lock for the whole copy so it is a consistent cut
        // (no commit interleaves). Lock order current → rels, as in `commit`.
        let _current = self.current.lock().unwrap();
        let dest = Database::builder(path.as_ref()).open().map_err(map_err)?;
        for name in self.db.list_keyspace_names() {
            let name = name.as_ref();
            let src = self.db.keyspace(name, KeyspaceCreateOptions::default).map_err(map_err)?;
            let dst = dest.keyspace(name, KeyspaceCreateOptions::default).map_err(map_err)?;
            let mut batch = dest.batch();
            for kv in src.iter() {
                let (k, v) = kv.into_inner().map_err(map_err)?;
                batch.insert(&dst, k.as_ref().to_vec(), v.as_ref().to_vec());
            }
            batch.commit().map_err(map_err)?;
        }
        dest.persist(PersistMode::SyncAll).map_err(map_err)?;
        drop(dest);
        // Reopen through the normal path so the fork's edition/watermark caches
        // are seeded from its copied `__meta`, exactly like any other store.
        FjallStore::open(path)
    }

    /// A canonical, fully-ordered dump of the entire physical store — every
    /// keyspace, key and value — for replay / fork verification.
    ///
    /// Two stores are **bit-identical** iff their dumps are equal: the value
    /// bytes are the `grmpl_core::wire` records (a single deterministic codec)
    /// and the keys are `edition || counter`, so equal dumps witness equal
    /// editions down to the byte. Used by the P10 tests to assert that replaying
    /// a session reproduces the world exactly and that a fork copies it exactly.
    /// O(domain).
    pub fn canonical_dump(&self) -> Result<Vec<DumpRow>> {
        let mut out = Vec::new();
        for name in self.db.list_keyspace_names() {
            let name = name.as_ref().to_string();
            let ks = self.db.keyspace(&name, KeyspaceCreateOptions::default).map_err(map_err)?;
            for kv in ks.iter() {
                let (k, v) = kv.into_inner().map_err(map_err)?;
                out.push((name.clone(), k.as_ref().to_vec(), v.as_ref().to_vec()));
            }
        }
        out.sort();
        Ok(out)
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
    /// `at`. Used by the atomic precondition re-check, always with `at =
    /// current` (≥ watermark), so no door check is needed here.
    fn holds_at(&self, rel: RelId, tuple: &Tuple, at: u64) -> Result<bool> {
        let ks = self.keyspace_for(rel)?;
        let wm = *self.watermark.lock().unwrap();
        let mut sum: Diff = 0;
        // Checkpoint (consolidated state ≤ watermark) then the tail (watermark, at].
        for kv in ks.prefix(CKPT_PREFIX) {
            let (_k, v) = kv.into_inner().map_err(map_err)?;
            let (diff, t) = codec::decode_record(v.as_ref())?;
            if &t == tuple {
                sum += diff;
            }
        }
        for kv in ks.range(tail_start(wm)..=tail_end(at)) {
            let (_k, v) = kv.into_inner().map_err(map_err)?;
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
        // `counter` is load-bearing: (edition, counter) is the commit-order key
        // that makes `scan_updates` deterministic, so it must track the exact
        // submit order of `updates`. `enumerate` yields that same 0,1,2… order.
        for (counter, (rel, tuple, diff)) in updates.iter().enumerate() {
            let ks = self.keyspace_for(*rel)?;
            let key = encode_key(next, counter as u64);
            let val = codec::encode_record(*diff, tuple);
            batch.insert(&ks, key, val);
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

    fn watermark(&self) -> Edition {
        Edition(*self.watermark.lock().unwrap())
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        let wm = *self.watermark.lock().unwrap();
        // Edition door: below the watermark the intermediate as-of state is gone
        // (folded into the checkpoint), so answer loudly rather than wrongly.
        if at.0 < wm {
            return Err(below_watermark_read(at.0, wm));
        }
        let ks = self.keyspace_for(rel)?;
        let mut acc: HashMap<Tuple, Diff> = HashMap::new();
        // Checkpoint: the consolidated state as-of `wm` (edition-0 key range).
        // O(checkpoint).
        for kv in ks.prefix(CKPT_PREFIX) {
            let (_k, v) = kv.into_inner().map_err(map_err)?;
            let (diff, tuple) = codec::decode_record(v.as_ref())?;
            *acc.entry(tuple).or_insert(0) += diff;
        }
        // Tail: raw updates in (wm, at]. O(updates since the watermark).
        for kv in ks.range(tail_start(wm)..=tail_end(at.0)) {
            let (_k, v) = kv.into_inner().map_err(map_err)?;
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
        let wm = *self.watermark.lock().unwrap();
        // Edition door: the raw updates in (from, wm] have been discarded, so a
        // scan that starts below the watermark cannot be answered.
        if from.0 < wm {
            return Err(below_watermark_scan(from.0, wm));
        }
        let ks = self.keyspace_for(rel)?;
        let mut out = Vec::new();
        // The range gives exactly editions (from, to]; checkpoint rows (edition
        // 0) fall below `from + 1` and are never scanned as deltas.
        for kv in ks.range(tail_start(from.0)..=tail_end(to.0)) {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            let edition = key_edition(k.as_ref());
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

    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        // Serialize with commits: hold the edition lock for the whole operation
        // so no new edition lands between choosing the horizon and truncating
        // below it. Lock order current → watermark → rels (via keyspace_for),
        // consistent with commit (current → rels) and read_at (watermark → rels).
        let current = self.current.lock().unwrap();
        let mut wm = self.watermark.lock().unwrap();
        // Never consolidate the future; monotonic (a target ≤ wm is a no-op).
        let target = up_to.0.min(*current);
        if target <= *wm {
            return Ok(Edition(*wm));
        }
        let old_wm = *wm;
        let mut batch = self.db.batch();
        for rel in self.all_rel_ids()? {
            let ks = self.keyspace_for(rel)?;
            // New checkpoint = consolidated state as-of `target` = old checkpoint
            // (edition-0 range) folded with the raw tail (old_wm, target].
            let mut acc: HashMap<Tuple, Diff> = HashMap::new();
            for kv in ks.prefix(CKPT_PREFIX) {
                let (k, v) = kv.into_inner().map_err(map_err)?;
                let (diff, tuple) = codec::decode_record(v.as_ref())?;
                *acc.entry(tuple).or_insert(0) += diff;
                batch.remove(&ks, k.as_ref().to_vec()); // clear the old checkpoint
            }
            for kv in ks.range(tail_start(old_wm)..=tail_end(target)) {
                let (k, v) = kv.into_inner().map_err(map_err)?;
                let (diff, tuple) = codec::decode_record(v.as_ref())?;
                *acc.entry(tuple).or_insert(0) += diff;
                batch.remove(&ks, k.as_ref().to_vec()); // discard the folded raw row
            }
            // Write the fresh checkpoint. Deterministic seq order over sorted,
            // net-nonzero tuples — the same consolidation `read_at` performs.
            let mut rows: Vec<(Tuple, Diff)> =
                acc.into_iter().filter(|(_, d)| *d != 0).collect();
            rows.sort();
            for (seq, (tuple, diff)) in rows.into_iter().enumerate() {
                batch.insert(&ks, ckpt_key(seq as u64), codec::encode_record(diff, &tuple));
            }
        }
        // Bump the watermark in the SAME atomic batch as the truncation, so a
        // crash leaves either the old horizon or the new one, never a half-cut
        // trace.
        batch.insert(&self.meta, WATERMARK_KEY, target.to_be_bytes().to_vec());
        batch.commit().map_err(map_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(map_err)?;
        *wm = target;
        Ok(Edition(target))
    }
}

/// A read below the consolidation watermark — its history has been discarded.
fn below_watermark_read(at: u64, wm: u64) -> Error {
    Error::Store(format!(
        "as-of read below the consolidation watermark: edition {at} < watermark {wm} \
         (that history has been folded into a checkpoint and discarded)"
    ))
}

/// A delta scan starting below the consolidation watermark.
fn below_watermark_scan(from: u64, wm: u64) -> Error {
    Error::Store(format!(
        "delta scan from below the consolidation watermark: from {from} < watermark {wm} \
         (the raw updates in ({from}, {wm}] have been folded into a checkpoint and discarded)"
    ))
}

/// Inclusive lower bound of the raw tail above `wm`: the first key of edition
/// `wm + 1`. Editions are ≤ `u64::MAX`; saturating add keeps the bound valid at
/// the (unreachable) ceiling.
fn tail_start(wm: u64) -> Vec<u8> {
    encode_key(wm.saturating_add(1), 0)
}

/// Inclusive upper bound of the raw tail at edition `at`: its last possible key.
fn tail_end(at: u64) -> Vec<u8> {
    encode_key(at, u64::MAX)
}

impl FjallStore {
    /// Every relation id that has a keyspace on disk (`rel_{id}`), recovered
    /// from the fjall keyspace directory — not just the ones cached this
    /// session — so [`consolidate`](TraceStore::consolidate) covers a reopened
    /// store completely.
    fn all_rel_ids(&self) -> Result<Vec<RelId>> {
        let mut out = Vec::new();
        for name in self.db.list_keyspace_names() {
            if let Some(id) = name.as_ref().strip_prefix("rel_") {
                let id: u32 = id
                    .parse()
                    .map_err(|_| Error::Store(format!("malformed relation keyspace `{}`", name.as_ref())))?;
                out.push(RelId(id));
            }
        }
        Ok(out)
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

impl SchemaCatalog for FjallStore {
    fn put_schema(&self, rel: RelId, schema: &Schema, at: Edition) -> Result<()> {
        // Compare against the current (highest-edition) version, if any.
        if let Some((cur_edition, current)) = self.latest_schema_version(rel)? {
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
        let mut batch = self.db.batch();
        batch.insert(&self.meta, sch_key(rel, at.0), wire::encode_schema(schema));
        batch.commit().map_err(map_err)?;
        self.db.persist(PersistMode::SyncAll).map_err(map_err)?;
        Ok(())
    }

    fn schema(&self, rel: RelId) -> Result<Option<Schema>> {
        Ok(self.latest_schema_version(rel)?.map(|(_, s)| s))
    }

    fn schema_at(&self, rel: RelId, at: Edition) -> Result<Option<Schema>> {
        // The newest version whose introducing edition is ≤ `at`.
        let mut best: Option<(u64, Schema)> = None;
        for (edition, schema) in self.schema_versions(rel)? {
            if edition <= at.0 && best.as_ref().is_none_or(|(e, _)| edition > *e) {
                best = Some((edition, schema));
            }
        }
        Ok(best.map(|(_, s)| s))
    }
}

impl FjallStore {
    /// All schema versions recorded for `rel`, as `(introducing_edition, schema)`.
    fn schema_versions(&self, rel: RelId) -> Result<Vec<(u64, Schema)>> {
        let prefix = sch_prefix(rel);
        let mut out = Vec::new();
        for kv in self.meta.iter() {
            let (k, v) = kv.into_inner().map_err(map_err)?;
            if k.as_ref().starts_with(&prefix) {
                let edition = u64_be(&k.as_ref()[prefix.len()..prefix.len() + 8]);
                out.push((edition, wire::decode_schema(v.as_ref())?));
            }
        }
        Ok(out)
    }

    /// The highest-edition schema version for `rel`, if any.
    fn latest_schema_version(&self, rel: RelId) -> Result<Option<(u64, Schema)>> {
        Ok(self.schema_versions(rel)?.into_iter().max_by_key(|(e, _)| *e))
    }
}

/// Key prefix for every schema version of `rel`: `sch:{rel(4, BE)}`.
fn sch_prefix(rel: RelId) -> Vec<u8> {
    let mut k = Vec::with_capacity(SCH_PREFIX.len() + 4);
    k.extend_from_slice(SCH_PREFIX);
    k.extend_from_slice(&rel.0.to_be_bytes());
    k
}

/// Full key for one schema version: `sch:{rel(4, BE)}{edition(8, BE)}`.
fn sch_key(rel: RelId, edition: u64) -> Vec<u8> {
    let mut k = sch_prefix(rel);
    k.extend_from_slice(&edition.to_be_bytes());
    k
}

fn cat_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(CAT_PREFIX.len() + name.len());
    k.extend_from_slice(CAT_PREFIX);
    k.extend_from_slice(name.as_bytes());
    k
}

/// A checkpoint row key: the reserved `edition = 0` range, `0u64 || seq(8, BE)`.
fn ckpt_key(seq: u64) -> Vec<u8> {
    encode_key(0, seq)
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

#[cfg(test)]
mod tests {
    use super::*;
    use grmpl_core::{Entity, Value};

    fn ent(a: u64) -> Tuple {
        Tuple::from([Value::Ent(Entity(a))])
    }

    /// The read path is O(checkpoint + tail), not O(history): after
    /// `consolidate` to watermark `W`, the relation keyspace physically holds
    /// only the checkpoint rows (one per surviving tuple as-of `W`) plus the raw
    /// updates in `(W, current]` — the collapsed history is *deleted*, not
    /// merely shadowed. This white-box test counts the physical rows to prove it
    /// (the black-box `history.rs` oracle proves the *values* are preserved).
    #[test]
    fn consolidate_truncates_keyspace_to_checkpoint_plus_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let r = RelId(1);

        // 6 editions of churn over 3 tuples, with cancellation.
        store.commit(&[(r, ent(1), 1), (r, ent(2), 1)]).unwrap(); // e1
        store.commit(&[(r, ent(3), 1)]).unwrap(); // e2
        store.commit(&[(r, ent(1), -1)]).unwrap(); // e3  net so far: {2,3}
        store.commit(&[(r, ent(1), 1)]).unwrap(); // e4  net: {1,2,3}
        store.commit(&[(r, ent(4), 1)]).unwrap(); // e5
        store.commit(&[(r, ent(2), -1)]).unwrap(); // e6  net: {1,3,4}

        let ks = store.keyspace_for(r).unwrap();
        let before = ks.iter().count();
        assert_eq!(before, 7, "6 editions produced 7 raw update rows");

        // Consolidate through edition 4: state as-of 4 is {1,2,3} → 3 checkpoint
        // rows; tail is editions 5,6 → 2 raw rows. Total physical rows == 5,
        // strictly fewer than the 7 pre-GC rows (history 1..=4 collapsed).
        assert_eq!(store.consolidate(Edition(4)).unwrap().0, 4);
        let after = ks.iter().count();
        assert_eq!(after, 5, "checkpoint (3) + tail rows in (4,6] (2) == 5");
        assert!(after < before, "consolidation must shrink the keyspace");

        // Checkpoint rows live in the reserved edition-0 range; the tail above.
        let ckpt = ks.prefix(CKPT_PREFIX).count();
        assert_eq!(ckpt, 3, "one checkpoint row per surviving tuple as-of the watermark");

        // And the values are still exactly right across the horizon.
        assert_eq!(
            store.read_at(r, Edition(4)).unwrap(),
            vec![(ent(1), 1), (ent(2), 1), (ent(3), 1)]
        );
        assert_eq!(
            store.read_at(r, Edition(6)).unwrap(),
            vec![(ent(1), 1), (ent(3), 1), (ent(4), 1)]
        );
    }
}
