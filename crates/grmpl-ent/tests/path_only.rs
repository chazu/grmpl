//! **G-1 + G-2 acceptance: a commit does path-sized *work*, and `open` is a
//! root lookup rather than a log replay.**
//!
//! Two claims that used to be prose, now assertions:
//!
//! * **G-1, path-only persistence.** On-disk *growth* was already path-sized —
//!   an untouched subtree hashes to a key already present, so re-inserting it is
//!   idempotent. But the *traversal* was `O(n)`: every commit re-serialized and
//!   re-hashed the whole tree to rediscover keys the store already had. With the
//!   content key memoized per node and per-granfilade knowledge of what is
//!   durable, a commit now visits only the path it copied. That is the
//!   difference between the Ent being able to replace the LSM under the language
//!   and not.
//!
//! * **G-2, the Fact enfilade is durable state.** It was not persisted at all:
//!   `open` loaded a watermark checkpoint and replayed the entire Edition log
//!   back through `fold_fact`. That is checkpoint-and-replay recovery — an LSM
//!   underneath the Ent, which the mandate rules out. Every live as-of root is
//!   now written with its commit, so recovery reads roots.

use grmpl_core::{Diff, Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_ent::EntStore;

const REL: RelId = RelId(1);

fn t(n: i64) -> Tuple {
    Tuple::from([Value::Int(n)])
}

fn fill(store: &EntStore, rows: i64) {
    let updates: Vec<(RelId, Tuple, Diff)> = (0..rows).map(|k| (REL, t(k), 1)).collect();
    store.commit(&updates).unwrap();
}

/// Work per commit must be flat in relation size. A tenfold bigger relation may
/// cost one more level of tree depth — not ten times the frames.
#[test]
fn commit_work_is_path_sized_not_relation_sized() {
    let mut costs = Vec::new();
    for rows in [500i64, 5_000] {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        fill(&store, rows);

        // Measure steady-state commits, after the bulk load.
        let before = store.frames_encoded();
        const COMMITS: u64 = 20;
        for k in 0..COMMITS as i64 {
            store.commit(&[(REL, t(rows + k), 1)]).unwrap();
        }
        let per_commit = (store.frames_encoded() - before) / COMMITS;
        costs.push(per_commit);

        assert!(
            per_commit < 40,
            "{rows} rows: {per_commit} frames encoded per commit — a commit should \
             touch its root-to-leaf path, not the relation"
        );
    }
    // Flat in relation size: 10x the rows must not cost anything like 10x.
    let (small, big) = (costs[0], costs[1]);
    assert!(
        big <= small * 2,
        "commit work grew from {small} to {big} frames when the relation grew 10x — \
         path-only persistence is not in effect"
    );
}

/// Re-persisting an unchanged tree must do **no** work: every node is memoized
/// and known durable, so the walk stops at the root.
#[test]
fn re_persisting_an_unchanged_world_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    fill(&store, 1_000);

    // A commit touching a *different* relation must not re-encode REL's tree.
    let before = store.frames_encoded();
    store.commit(&[(RelId(2), t(1), 1)]).unwrap();
    let cost = store.frames_encoded() - before;
    assert!(
        cost < 40,
        "{cost} frames encoded for a commit to an untouched second relation"
    );
}

/// Every live as-of root is on disk, so recovery is a root lookup. The count of
/// persisted Fact roots tracks the live editions — if `open` were still
/// replaying a log, there would be nothing here to load.
#[test]
fn fact_roots_are_persisted_per_live_edition() {
    let dir = tempfile::tempdir().unwrap();
    let editions = {
        let store = EntStore::open(dir.path()).unwrap();
        for k in 0..8i64 {
            store.commit(&[(REL, t(k), 1)]).unwrap();
        }
        store.current()
    };

    // Reopen and read every intermediate edition: each must show exactly the
    // rows committed by then.
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), editions);
    for e in 1..=editions.0 {
        let rows = store.read_at(REL, Edition(e)).unwrap();
        assert_eq!(
            rows.len(),
            e as usize,
            "as-of edition {e} lost rows across a reopen — the Fact roots did not persist"
        );
    }
}

/// Consolidation retires the roots below the new watermark, so the persisted
/// as-of history is exactly the live window — the store does not accumulate a
/// root per edition forever.
#[test]
fn consolidation_retires_superseded_fact_roots() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    for k in 0..20i64 {
        store.commit(&[(REL, t(k), 1)]).unwrap();
    }
    let cur = store.current();
    store.consolidate(Edition(cur.0 - 5)).unwrap();
    store.gc().unwrap();

    // Below the watermark the door errors; at and above it, reads still answer.
    assert!(store.read_at(REL, Edition(1)).is_err(), "the watermark door must close");
    assert_eq!(store.read_at(REL, cur).unwrap().len(), 20);

    // …and it all survives a reopen, from the retained roots alone.
    drop(store);
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), cur);
    assert_eq!(store.read_at(REL, cur).unwrap().len(), 20);
    assert!(store.read_at(REL, Edition(1)).is_err());
}

/// **G-3: the measure family.** "How many" and "how much" over a key span are
/// both folds of cached subtree summaries — the tree's shape, not its rows.
#[test]
fn count_and_weight_are_answered_from_the_measure() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    // Weights that are not all 1, and some that cancel, so Σ is not the count.
    let updates: Vec<(RelId, Tuple, Diff)> =
        (0..500i64).map(|k| (REL, t(k), if k % 7 == 0 { 3 } else { 1 })).collect();
    store.commit(&updates).unwrap();
    let at = store.current();

    for (lo, hi) in [(0i64, 500i64), (0, 1), (100, 200), (499, 500), (600, 700), (5, 5)] {
        let rows = store.read_range(REL, at, &t(lo), &t(hi)).unwrap();
        assert_eq!(
            store.count_at(REL, at, &t(lo), &t(hi)).unwrap(),
            rows.len() as u64,
            "count over [{lo},{hi}) disagreed with the rows"
        );
        assert_eq!(
            store.weight_at(REL, at, &t(lo), &t(hi)).unwrap(),
            rows.iter().map(|(_, d)| *d).sum::<i64>(),
            "weight over [{lo},{hi}) disagreed with the rows"
        );
    }
}
