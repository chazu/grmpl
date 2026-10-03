//! **Everything in the Ent, paged on demand.**
//!
//! The granfilade has one mutable slot, the root record, and every durable thing
//! hangs from it as a tree: the branch DAG, the branch enfilade, each branch's
//! Rel enfilade, every relation's versions, log and Arrangements, the context
//! enfilade and the canopy. Three claims follow, and each is pinned here with an
//! ops counter or an observable consequence rather than left as prose:
//!
//! 1. **Opening a world costs the same however large it is.** `open` reads the
//!    root record and a couple of frames; a read pages in only the path it
//!    walks (`frames_paged`).
//! 2. **A reader keeps its version through GC.** A pinned snapshot holds paged
//!    nodes whose frames the current root no longer names; GC must keep them
//!    until the reader is done.
//! 3. **Interest is durable.** A watcher's registration and the commits routed
//!    to it are part of the root, so after a reopen the canopy still answers
//!    precisely instead of widening to the whole relation.

use grmpl_core::{Diff, Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;

fn t(n: i64) -> Tuple {
    Tuple::from([Value::Int(n)])
}

/// Build a world: `rels` relations of `rows` rows each, loaded over `commits`
/// editions, so every directory level has some depth.
fn build(store: &EntStore, rels: u32, rows: i64, commits: i64) {
    let per = rows / commits;
    for c in 0..commits {
        let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for r in 1..=rels {
            for k in c * per..(c + 1) * per {
                ups.push((RelId(r), t(k), 1));
            }
        }
        store.commit(&ups).unwrap();
    }
}

#[test]
fn opening_a_world_reads_a_few_frames_whatever_its_size() {
    let mut opens = Vec::new();
    for rows in [1_000i64, 40_000] {
        let dir = tempfile::tempdir().unwrap();
        let cur = {
            let store = EntStore::open(dir.path()).unwrap();
            build(&store, 4, rows, 100);
            store.current()
        };

        let store = EntStore::open(dir.path()).unwrap();
        assert_eq!(store.current(), cur);
        let at_open = store.frames_paged();
        opens.push(at_open);

        // A narrow read pages in one path through each directory level, not the
        // relation.
        let rows_read = store.read_range(RelId(3), cur, &t(500), &t(510)).unwrap();
        assert_eq!(rows_read.len(), 10);
        let narrow = store.frames_paged() - at_open;
        assert!(narrow <= 12, "{rows} rows: a 10-row read paged in {narrow} frames");

        // A full read of one relation is exact, and still leaves the others
        // unread.
        let all = store.read_at(RelId(2), cur).unwrap();
        assert_eq!(all.len(), rows as usize);
        let one_rel = store.frames_paged() - at_open;
        let leaves = rows as u64 / 64;
        assert!(one_rel < 4 * leaves, "{rows} rows: reading one relation paged in {one_rel} frames");
    }
    assert!(opens[0] <= 4, "open read {} frames", opens[0]);
    assert_eq!(opens[0], opens[1], "open cost grew with the world: {opens:?}");
}

#[test]
fn a_pinned_reader_keeps_its_version_through_consolidation_and_gc() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = EntStore::open(dir.path()).unwrap();
        for k in 0..400i64 {
            store.commit(&[(RelId(1), t(k), 1)]).unwrap();
        }
    }
    // Reopen, so everything the reader reaches starts out paged.
    let store = EntStore::open(dir.path()).unwrap();
    let pinned = Edition(150);
    let reader = store.reader_at(pinned);

    // Retire every version below 300 and collect what the root no longer names.
    store.consolidate(Edition(300)).unwrap();
    let collected = store.gc().unwrap();
    assert!(collected > 0, "GC collected nothing — the test would prove nothing");
    assert!(store.read_at(RelId(1), pinned).is_err(), "the door must close for new reads");

    // The reader still reads its edition: its unread frames were GC roots.
    let rows = reader.read(RelId(1)).unwrap();
    assert_eq!(rows.len(), 150, "the pinned reader lost its version to GC");
    assert_eq!(rows[149], (t(149), 1));
    drop(reader);

    // And the world itself survives a reopen from the root alone.
    drop(store);
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.read_at(RelId(1), store.current()).unwrap().len(), 400);
}

#[test]
fn a_watchers_interest_survives_a_reopen() {
    const REL: RelId = RelId(7);
    let dir = tempfile::tempdir().unwrap();
    let (lo, hi) = (t(0), t(10));
    let registered = {
        let store = EntStore::open(dir.path()).unwrap();
        store.commit(&[(REL, t(100), 1)]).unwrap();
        let from = store.current();
        // Registering is a read; the next commit makes it durable with the
        // routing it does.
        assert!(!store.touched_range_since(from, from, REL, &lo, &hi).unwrap());
        store.commit(&[(REL, t(50), 1)]).unwrap();
        assert!(!store.touched_range_since(from, store.current(), REL, &lo, &hi).unwrap());
        from
    };

    let store = EntStore::open(dir.path()).unwrap();
    // A change outside the interest, after the reopen. A canopy rebuilt empty
    // would have to widen to "the relation changed"; the persisted one knows.
    store.commit(&[(REL, t(60), 1)]).unwrap();
    assert!(
        !store.touched_range_since(registered, store.current(), REL, &lo, &hi).unwrap(),
        "the interest did not survive the reopen, so routing widened to the relation"
    );
    // And a change inside it is still delivered.
    store.commit(&[(REL, t(5), 1)]).unwrap();
    assert!(store.touched_range_since(registered, store.current(), REL, &lo, &hi).unwrap());
}
