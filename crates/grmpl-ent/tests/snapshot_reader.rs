//! **Snapshot handles: a reader is acquired once, and reads are lock-free.**
//!
//! `Snapshot` used to be `(edition, &dyn TraceStore)`, so every `read`, every
//! `holds`, and every base relation of every `find` re-entered the store and
//! took its global mutex — each acquisition able to block behind a committer
//! sitting inside its ~1 ms `fsync`. **Readers stalled by durability they have no
//! stake in.**
//!
//! A `Snapshot` now holds an `EditionReader`, acquired once. On the Ent that
//! reader captures the Rel enfilade's root — one `Arc` bump, because the Fact
//! enfilade is immutable and versioned by edition, the same property that makes
//! `fork_at` free — and answers every later read by descending it, touching no
//! shared state.
//!
//! Two things need pinning, and `EntStore::read_locks()` is what makes the first
//! falsifiable rather than merely claimed (the same ops-counter discipline as
//! `frames_encoded` and `syncs`):
//!
//! 1. **One acquisition per snapshot, not one per read.** A three-way join over
//!    a pinned snapshot costs exactly one.
//! 2. **Real snapshot isolation.** A pinned reader keeps answering for its
//!    edition no matter how far the store moves on, because the version it holds
//!    cannot change.

use std::sync::Arc;
use std::thread;

use grmpl_core::{Diff, Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{Query, Snapshot};
use grmpl_ent::EntStore;

const A: RelId = RelId(1);
const B: RelId = RelId(2);
const C: RelId = RelId(3);

fn pair(x: i64, y: i64) -> Tuple {
    Tuple::from([Value::Int(x), Value::Int(y)])
}

fn seed(store: &EntStore) {
    let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
    for i in 0..8 {
        ups.push((A, pair(i, i + 1), 1));
        ups.push((B, pair(i + 1, i + 2), 1));
        ups.push((C, pair(i + 2, i + 3), 1));
    }
    store.commit(&ups).unwrap();
}

/// `a(x,y) ⋈ b(y,z) ⋈ c(z,w)` — three base relations, so three store reads under
/// the old shape.
fn three_way_join() -> Query {
    let ab = Query::rel(A).join(Query::rel(B), vec![1], vec![0]);
    ab.join(Query::rel(C), vec![3], vec![0])
}

/// Law 1 — one acquisition per snapshot, however many relations are read.
#[test]
fn a_snapshot_enters_the_store_once_not_once_per_relation() {
    let store = EntStore::new();
    seed(&store);
    let at = store.current();

    let before = store.read_locks();
    let snap = Snapshot::new(&store, at);
    let after_pin = store.read_locks();
    assert_eq!(after_pin - before, 1, "pinning a snapshot enters the store once");

    // A three-way join, plus two direct reads and a `holds` — six reads that
    // each used to take the lock.
    let rows = three_way_join().find(&snap).unwrap();
    assert!(!rows.is_empty(), "the join produced rows to read");
    snap.read(A).unwrap();
    snap.read(B).unwrap();
    snap.holds(&grmpl_core::Fact::new(A, pair(0, 1))).unwrap();

    assert_eq!(
        store.read_locks(),
        after_pin,
        "every read through a pinned snapshot is lock-free — the store is not \
         re-entered once the reader exists"
    );
}

/// The same plan evaluated *without* a pinned snapshot still acquires once per
/// evaluation — not once per relation. `eval_snapshot` takes a reader for the
/// whole plan, which is the same fix one level down.
#[test]
fn a_bare_evaluation_acquires_once_for_the_whole_plan() {
    let store = EntStore::new();
    seed(&store);
    let at = store.current();

    let before = store.read_locks();
    grmpl_diff::eval_snapshot(&three_way_join(), &store, at).unwrap();
    assert_eq!(
        store.read_locks() - before,
        1,
        "one reader for the plan, not one per base relation"
    );
}

/// Law 2 — a pinned reader is isolated from everything that happens after it.
///
/// This is what "snapshot" is supposed to mean and what the old shape only
/// approximated: it re-read a moving store at a fixed edition, which gave the
/// right *answer* but took the lock to get it. The reader holds the version
/// itself, so concurrent committers are irrelevant to it in both senses.
#[test]
fn a_pinned_reader_is_unmoved_by_concurrent_commits() {
    let store = Arc::new(EntStore::new());
    seed(&store);
    let at = store.current();
    let snap = Snapshot::new(&*store, at);
    let before = snap.read(A).unwrap();
    assert_eq!(before.len(), 8);

    // Hammer the store from other threads while the snapshot is live.
    thread::scope(|scope| {
        for t in 0..4i64 {
            let store = Arc::clone(&store);
            scope.spawn(move || {
                for k in 0..25i64 {
                    store.commit(&[(A, pair(1000 + t * 100 + k, 0), 1)]).unwrap();
                }
            });
        }

        // ...and read from the snapshot throughout. Its answers never move.
        for _ in 0..50 {
            assert_eq!(snap.read(A).unwrap(), before, "a pinned reader does not drift");
        }
    });

    assert_eq!(snap.edition, at);
    assert_eq!(snap.read(A).unwrap(), before, "still its own edition afterwards");
    // The store, meanwhile, moved on.
    assert_eq!(store.current().0, at.0 + 100);
    assert_eq!(store.read_at(A, store.current()).unwrap().len(), 108);
}

/// The reader honours the consolidation watermark at the same door the store
/// does — a reader is not a way around the P6 guard.
#[test]
fn a_reader_below_the_watermark_errors_at_the_door() {
    let store = EntStore::new();
    seed(&store);
    let early = store.current();
    store.commit(&[(A, pair(99, 99), 1)]).unwrap();
    let now = store.current();
    store.consolidate(now).unwrap();

    let snap = Snapshot::new(&store, early);
    assert!(
        snap.read(A).is_err(),
        "reading below the watermark must error, reader or not"
    );
    // And a reader at or above it still answers.
    let ok = Snapshot::new(&store, now);
    assert_eq!(ok.read(A).unwrap().len(), 9);
}

/// A reader on a substrate that does not override `reader_at` still works: the
/// default forwards each call back to the store. Checked through the in-memory
/// Ent by pinning an edition that is *not* current, which the forwarding path
/// and the captured-root path must agree on.
#[test]
fn a_reader_agrees_with_reading_the_store_directly() {
    let store = EntStore::new();
    seed(&store);
    let first = store.current();
    store.commit(&[(A, pair(500, 500), 1)]).unwrap();
    let second = store.current();

    for at in [first, second, Edition(0)] {
        let snap = Snapshot::new(&store, at);
        let mut via_reader = snap.read(A).unwrap();
        let mut via_store = store.read_at(A, at).unwrap();
        via_reader.sort();
        via_store.sort();
        assert_eq!(via_reader, via_store, "reader and store agree at {at:?}");
    }
}
