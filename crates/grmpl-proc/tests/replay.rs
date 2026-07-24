//! Replay (P10): a process's patches are a pure function of committed history.
//!
//! The behavior here is deliberately *stateful* — each message reads a running
//! total from the world and rewrites it — so a faithful replay must read each
//! historical step against the exact edition it committed against. If replay
//! read the world at `current` instead, every re-derived patch would retract the
//! wrong old total and the assertion below would fail. That is the whole point:
//! `replay_from` reproduces the identical patches only because it reads as-of.

use grmpl_core::{
    Authority, DomainId, Edition, EditionStore, Entity, Fact, NoSchemas, Patch, RelId, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_proc::{enqueue, record_run, replay_from, Behavior, Process};
use grmpl_store::FjallStore;

const INBOX: RelId = RelId(1);
const CURSOR: RelId = RelId(2);
const TOTAL: RelId = RelId(3);

const ACTOR: Entity = Entity(7);

/// The running total held in `TOTAL` as-of `snap`: the single positive row's
/// value, or `None` if the relation is empty here.
fn total_of(snap: &Snapshot) -> Option<i64> {
    snap.read(TOTAL)
        .unwrap()
        .into_iter()
        .find(|(_, d)| *d > 0)
        .and_then(|(t, _)| match t.as_slice().first() {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        })
}

/// Accumulate `body[0]` into the running total: retract the old row (if any),
/// assert the new one. Pure in `(snap, body)` — the Replay law.
fn ledger_behavior() -> Behavior {
    Box::new(
        |snap: &Snapshot, body: &Tuple| -> grmpl_core::Result<Patch> {
            let n = match body.as_slice().first() {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let old = total_of(snap);
            let new = old.unwrap_or(0) + n;
            let mut patch = Patch::new().assert(Fact::new(TOTAL, Tuple::from([Value::Int(new)])));
            if let Some(old) = old {
                patch = patch.retract(Fact::new(TOTAL, Tuple::from([Value::Int(old)])));
            }
            Ok(patch)
        },
    )
}

fn actor() -> Process {
    Process {
        entity: ACTOR,
        authority: Authority::new(DomainId(1), vec![Scope::whole(TOTAL)]),
        inbox: INBOX,
        cursor_rel: CURSOR,
        behavior: ledger_behavior(),
    }
}

/// Enqueue `ns` as ledger messages at contiguous seqs starting from `start`.
fn feed(store: &dyn TraceStore, start: i64, ns: &[i64]) {
    for (i, n) in ns.iter().enumerate() {
        enqueue(
            store,
            INBOX,
            ACTOR,
            start + i as i64,
            Tuple::from([Value::Int(*n)]),
        )
        .unwrap();
    }
}

#[test]
fn replay_reproduces_identical_patches() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let proc = actor();

    feed(&store, 0, &[10, 5, -3, 20, 1]);
    let live = record_run(&store, &proc, &NoSchemas).unwrap();
    assert_eq!(live.len(), 5, "all five messages committed");

    // Re-derived purely from history, over the whole trace: identical patches.
    let replayed = replay_from(&store, &proc, Edition::ZERO).unwrap();
    assert_eq!(
        live, replayed,
        "replay reproduces the live log step for step"
    );

    // Replay is itself deterministic — a second re-derivation matches.
    let again = replay_from(&store, &proc, Edition::ZERO).unwrap();
    assert_eq!(replayed, again, "replay is deterministic");

    // Spot-check the stateful shape: the third message (-3) retracts the total
    // 15 that stood before it and asserts 12. This is only right because replay
    // read `TOTAL` as-of that step's edition, not the final edition.
    let third = &replayed[2];
    assert_eq!(third.seq, 2);
    assert_eq!(
        third.patch.retracts,
        vec![Fact::new(TOTAL, Tuple::from([Value::Int(15)]))]
    );
    assert_eq!(
        third.patch.asserts,
        vec![Fact::new(TOTAL, Tuple::from([Value::Int(12)]))]
    );

    // The world's final total is the plain sum.
    let snap = Snapshot::at_current(&store);
    assert_eq!(total_of(&snap), Some(33));
}

#[test]
fn replay_from_a_midpoint_covers_only_later_steps() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let proc = actor();

    // First batch, remembering the edition after it.
    feed(&store, 0, &[10, 5]);
    record_run(&store, &proc, &NoSchemas).unwrap();
    let mid = store.current();

    // Second batch.
    feed(&store, 2, &[100, 200]);
    let later = record_run(&store, &proc, &NoSchemas).unwrap();

    // Replaying from `mid` re-derives exactly the second batch's steps.
    let replayed = replay_from(&store, &proc, mid).unwrap();
    assert_eq!(
        later, replayed,
        "midpoint replay covers only the post-`mid` steps"
    );
    assert_eq!(
        replayed.iter().map(|s| s.seq).collect::<Vec<_>>(),
        vec![2, 3]
    );
}

#[test]
fn replay_below_watermark_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let proc = actor();

    feed(&store, 0, &[1, 2, 3, 4]);
    record_run(&store, &proc, &NoSchemas).unwrap();

    // Consolidate away early history, then a replay reaching below the watermark
    // must error at the door rather than answer from truncated history.
    let wm = store.consolidate(store.current()).unwrap();
    assert!(wm.0 > 0, "consolidation advanced the watermark");
    let err = replay_from(&store, &proc, Edition::ZERO).unwrap_err();
    assert!(
        format!("{err}").contains("watermark"),
        "replay below the watermark is rejected loudly: {err}"
    );
}
