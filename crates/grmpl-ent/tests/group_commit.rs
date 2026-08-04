//! **Group commit: one `fsync` per group, not per committer.**
//!
//! `EntStore::commit_if` used to hold the edition lock across the ~1 ms
//! `SyncAll` in `Granfilade::write_full`, so N committers paid N fsyncs strictly
//! in series — the floor every axis of `docs/PERFORMANCE-ENT.md` bottoms out on.
//! Now the edition lock covers only allocation, apply and *encoding*; the write
//! happens outside it, and one member of a group performs a single batch +
//! `SyncAll` covering every edition staged behind it.
//!
//! The law that makes this legal rather than a shortcut: **the patch–edition law
//! demands one atomic durable write per *edition*, not per *committer*.** A
//! batch is still atomic, so no state ever carries part of an edition. What
//! changes is *when* a write is durable, and the gate is on the commit call:
//! **`commit`/`commit_if` return only after the edition they return is durable**,
//! so no committer learns of its own edition before disk does.
//!
//! The clock stays the *allocated* edition (`EntStore::durable_edition` is the
//! on-disk frontier), because `commit_if` validates preconditions against the
//! allocated state and reads must agree with the validator — see the `Durable`
//! docs, and `a_guarded_allocator_does_not_livelock` below, which is the test
//! that caught it.
//!
//! These tests pin both halves: that the amortization actually happens (via the
//! `syncs()` ops counter, in the same spirit as `frames_encoded`), and that
//! everything a commit hands back is on disk.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use grmpl_core::{Diff, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;

const R: RelId = RelId(1);

fn row(i: i64) -> Tuple {
    Tuple::from([Value::Int(i), Value::text("x")])
}

fn update(i: i64) -> (RelId, Tuple, Diff) {
    (R, row(i), 1)
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("grmpl-group-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// **Amortization.** Concurrent committers share fsyncs: the number of
/// `SyncAll`s is strictly fewer than the number of commits.
///
/// This is the claim the whole change exists to make, and it is stated as a
/// counter rather than a wall-clock number so it is portable and so it *fails*
/// if grouping silently stops happening. It is deliberately weak (`<`, not a
/// ratio): how many editions land in one group depends on the OS scheduler, and
/// a law that depended on that would be flaky. What cannot happen under a
/// working implementation is one fsync per commit.
#[test]
fn concurrent_committers_share_fsyncs() {
    let path = tmp("amortize");
    let store = Arc::new(EntStore::open(&path).unwrap());

    const THREADS: i64 = 8;
    const PER_THREAD: i64 = 24;
    const COMMITS: u64 = (THREADS * PER_THREAD) as u64;

    let before = store.syncs();
    thread::scope(|scope| {
        for t in 0..THREADS {
            let store = Arc::clone(&store);
            scope.spawn(move || {
                for k in 0..PER_THREAD {
                    store.commit(&[update(t * 1000 + k)]).unwrap();
                }
            });
        }
    });
    let syncs = store.syncs() - before;

    // Every commit landed, each as its own edition.
    assert_eq!(store.current().0, COMMITS, "every commit allocated an edition");
    assert_eq!(
        store.read_at(R, store.current()).unwrap().len(),
        COMMITS as usize,
        "every committed row is present"
    );

    assert!(
        syncs < COMMITS,
        "concurrent committers must share fsyncs: {syncs} syncs for {COMMITS} commits"
    );
    // A sanity floor: grouping can never be free, and a zero here would mean the
    // counter is not wired to the real durability call.
    assert!(syncs > 0, "durability still happens");

    let _ = std::fs::remove_dir_all(&path);
}

/// **A lone committer is not penalized.** With no contention there is nobody to
/// group with, so each commit is its own group of one — the pre-change cost, and
/// no added handoff latency from a dedicated writer thread (there isn't one:
/// whoever needs the write performs it).
#[test]
fn a_sequential_committer_pays_one_fsync_per_commit() {
    let path = tmp("solo");
    let store = EntStore::open(&path).unwrap();

    let before = store.syncs();
    for i in 0..10 {
        store.commit(&[update(i)]).unwrap();
    }
    assert_eq!(
        store.syncs() - before,
        10,
        "uncontended commits form groups of one — no batching to wait for"
    );

    let _ = std::fs::remove_dir_all(&path);
}

/// **The gate.** A commit returns only once its edition is durable, so a
/// returned edition survives a reopen. This is what keeps group commit inside
/// the patch–edition law: a committer never learns of an edition disk does not
/// already have.
#[test]
fn a_returned_edition_is_durable() {
    let path = tmp("gate");
    {
        let store = Arc::new(EntStore::open(&path).unwrap());
        // Commit concurrently so the returned editions really do come out of
        // multi-member groups.
        let seen = Arc::new(AtomicU64::new(0));
        thread::scope(|scope| {
            for t in 0..4i64 {
                let store = Arc::clone(&store);
                let seen = Arc::clone(&seen);
                scope.spawn(move || {
                    for k in 0..8i64 {
                        let e = store.commit(&[update(t * 100 + k)]).unwrap();
                        // The gate: the durable frontier has already passed the
                        // edition we were handed. A caller acting on `e` is
                        // acting on something a crash cannot take back.
                        assert!(
                            store.durable_edition().0 >= e.0,
                            "a returned edition must already be durable"
                        );
                        // And the durable frontier never runs ahead of the clock.
                        assert!(store.durable_edition().0 <= store.current().0);
                        seen.fetch_max(e.0, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(seen.load(Ordering::Relaxed), 32);
    }

    // Reopen: every returned edition is on disk, with its rows.
    let reopened = EntStore::open(&path).unwrap();
    assert_eq!(reopened.current().0, 32, "the durable clock survived");
    assert_eq!(
        reopened.read_at(R, reopened.current()).unwrap().len(),
        32,
        "every row a commit returned for is durable"
    );

    let _ = std::fs::remove_dir_all(&path);
}

/// **Grouping preserves the log.** Editions in one batch must reach disk in
/// edition order and none may be skipped, or a reopen would read a stale clock
/// or a history with a hole. Checked over `scan_updates`, which is defined to be
/// in commit order `(edition, counter)`.
#[test]
fn a_grouped_history_reopens_contiguous_and_in_order() {
    let path = tmp("order");
    const THREADS: i64 = 6;
    const PER_THREAD: i64 = 10;
    {
        let store = Arc::new(EntStore::open(&path).unwrap());
        thread::scope(|scope| {
            for t in 0..THREADS {
                let store = Arc::clone(&store);
                scope.spawn(move || {
                    for k in 0..PER_THREAD {
                        store.commit(&[update(t * 100 + k)]).unwrap();
                    }
                });
            }
        });
    }

    let reopened = EntStore::open(&path).unwrap();
    let total = (THREADS * PER_THREAD) as u64;
    assert_eq!(reopened.current().0, total);

    let updates = reopened
        .scan_updates(R, grmpl_core::Edition(0), reopened.current())
        .unwrap();
    assert_eq!(updates.len(), total as usize, "no update lost in a group");

    // One update per edition, editions 1..=total, strictly ascending — the
    // Determinism invariant's commit order, unbroken by batching.
    let editions: Vec<u64> = updates.iter().map(|u| u.time.edition).collect();
    let want: Vec<u64> = (1..=total).collect();
    assert_eq!(editions, want, "history is contiguous and in commit order");

    let _ = std::fs::remove_dir_all(&path);
}

/// **Consolidation still sees a settled world.** `consolidate` is the one
/// occasion that rewrites existing roots and retires the ones below the new
/// watermark, so it drains the queue first — a staged commit landing afterwards
/// would re-insert a root the sweep just retired.
#[test]
fn consolidation_drains_the_queue_first() {
    let path = tmp("consolidate");
    {
        let store = Arc::new(EntStore::open(&path).unwrap());
        thread::scope(|scope| {
            for t in 0..4i64 {
                let store = Arc::clone(&store);
                scope.spawn(move || {
                    for k in 0..10i64 {
                        store.commit(&[update(t * 100 + k)]).unwrap();
                    }
                });
            }
        });
        let at = store.current();
        assert_eq!(store.consolidate(at).unwrap(), at);
        assert_eq!(store.watermark(), at);
        assert_eq!(store.read_at(R, at).unwrap().len(), 40);
    }

    // The checkpoint is what a reopen finds, with nothing left over from a
    // late-landing group.
    let reopened = EntStore::open(&path).unwrap();
    assert_eq!(reopened.current().0, 40);
    assert_eq!(reopened.watermark().0, 40);
    assert_eq!(reopened.read_at(R, reopened.current()).unwrap().len(), 40);

    let _ = std::fs::remove_dir_all(&path);
}

/// **The clock must be the state `commit_if` validates against.** This is the
/// test that caught the first cut of group commit, which reported the durability
/// watermark as `current()`.
///
/// A guarded allocator is the minimal read-modify-write: read the counter at
/// `current()`, then commit preconditioned on the value read. If `current()`
/// lags what `commit_if` checks against, every attempt builds its patch on a
/// stale counter and is rejected — not occasionally, but systematically, because
/// under load the watermark lags for as long as any group is in flight. The
/// first implementation livelocked here within a dozen attempts while reporting
/// no error at all until the retry budget ran out.
///
/// The law: **N threads each allocating from one counter hand out N distinct
/// contiguous ids and all of them terminate.**
#[test]
fn a_guarded_allocator_does_not_livelock() {
    let path = tmp("livelock");
    let store = Arc::new(EntStore::open(&path).unwrap());
    const COUNTER: RelId = RelId(9);

    // Seed the counter on the un-raced path.
    store
        .commit(&[(COUNTER, Tuple::from([Value::Int(0)]), 1)])
        .unwrap();

    const THREADS: i64 = 4;
    const PER_THREAD: i64 = 15;

    thread::scope(|scope| {
        for _ in 0..THREADS {
            let store = Arc::clone(&store);
            scope.spawn(move || {
                for _ in 0..PER_THREAD {
                    // Bounded: if reads and validation disagree this fails
                    // rather than spinning forever.
                    let mut ok = false;
                    for _ in 0..64 {
                        let at = store.current();
                        let n = store
                            .read_at(COUNTER, at)
                            .unwrap()
                            .into_iter()
                            .find(|(_, d)| *d > 0)
                            .and_then(|(t, _)| match t.as_slice().first() {
                                Some(Value::Int(v)) => Some(*v),
                                _ => None,
                            })
                            .expect("counter row present");
                        let cur = Tuple::from([Value::Int(n)]);
                        let next = Tuple::from([Value::Int(n + 1)]);
                        if store
                            .commit_if(
                                &[(COUNTER, cur.clone())],
                                &[(COUNTER, cur, -1), (COUNTER, next, 1)],
                            )
                            .unwrap()
                            .is_some()
                        {
                            ok = true;
                            break;
                        }
                    }
                    assert!(ok, "a guarded allocation must settle — reads and \
                                 precondition checks must see the same edition");
                }
            });
        }
    });

    // The counter advanced exactly once per allocation, and rests on one row.
    let at = store.current();
    let rows: Vec<_> = store
        .read_at(COUNTER, at)
        .unwrap()
        .into_iter()
        .filter(|(_, d)| *d != 0)
        .collect();
    assert_eq!(
        rows,
        vec![(Tuple::from([Value::Int(THREADS * PER_THREAD)]), 1)],
        "every allocation landed exactly once"
    );

    let _ = std::fs::remove_dir_all(&path);
}
