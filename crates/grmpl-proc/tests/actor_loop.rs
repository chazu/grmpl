//! M5 acceptance (DESIGN.md §9): exactly-once actor processing under simulated
//! crash before/after commit, because the inbox-cursor advance rides in the
//! same atomic batch as the message's effects. Plus: a behavior that writes
//! outside its authority is rejected (Authority law).

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, Patch, RelId, Result, Scope, TraceStore,
    Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_proc::{enqueue, Behavior, CommitOutcome, Process};
use grmpl_store::FjallStore;

const INBOX: RelId = RelId(10);
const CURSOR: RelId = RelId(11);
const RECEIPTS: RelId = RelId(12); // (process, n) — one per message, exactly once

const PROC: Entity = Entity(42);

/// Behavior: message body `[Int(n)]` → assert a unique receipt `(proc, n)`.
/// A second processing of the same message would push the receipt's weight to
/// 2, so weight==1 is the exactly-once witness.
fn tally_behavior() -> Behavior {
    Box::new(|_snap: &Snapshot, body: &Tuple| -> Result<Patch> {
        let n = match body.as_slice().first() {
            Some(Value::Int(n)) => *n,
            _ => return Ok(Patch::new()),
        };
        Ok(Patch::new().assert(Fact::new(
            RECEIPTS,
            Tuple::from([Value::Ent(PROC), Value::Int(n)]),
        )))
    })
}

fn make_process(behavior: Behavior) -> Process {
    Process {
        entity: PROC,
        authority: Authority::new(DomainId(1), vec![Scope::whole(RECEIPTS), Scope::whole(CURSOR)]),
        inbox: INBOX,
        cursor_rel: CURSOR,
        behavior,
    }
}

fn seed_three(store: &FjallStore) {
    for (seq, n) in [(0, 100), (1, 101), (2, 102)] {
        enqueue(store, INBOX, PROC, seq, Tuple::from([Value::Int(n)])).unwrap();
    }
}

fn receipts(store: &FjallStore) -> Vec<(Tuple, i64)> {
    let cur = store.current();
    let mut v = store.read_at(RECEIPTS, cur).unwrap();
    v.sort();
    v
}

fn expected_receipts() -> Vec<(Tuple, i64)> {
    let mut v = vec![
        (Tuple::from([Value::Ent(PROC), Value::Int(100)]), 1),
        (Tuple::from([Value::Ent(PROC), Value::Int(101)]), 1),
        (Tuple::from([Value::Ent(PROC), Value::Int(102)]), 1),
    ];
    v.sort();
    v
}

#[test]
fn crash_after_commit_processes_each_message_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FjallStore::open(dir.path()).unwrap();
        seed_three(&store);
        let proc = make_process(tally_behavior());

        // Process exactly one message, then "crash" (drop the store).
        let out = proc.step(&store).unwrap();
        assert!(matches!(out, Some(CommitOutcome::Committed(_))));
        assert_eq!(proc.position(&store).unwrap(), 1);
    }

    // Reopen and drain: the cursor already advanced past message 0, so it is
    // not reprocessed; messages 1 and 2 complete.
    let store = FjallStore::open(dir.path()).unwrap();
    let proc = make_process(tally_behavior());
    proc.run_to_idle(&store).unwrap();

    assert_eq!(receipts(&store), expected_receipts());
    assert_eq!(proc.position(&store).unwrap(), 3);

    // Re-draining is a no-op (idle) — no duplicates.
    assert_eq!(proc.run_to_idle(&store).unwrap(), 0);
    assert_eq!(receipts(&store), expected_receipts());
}

#[test]
fn crash_before_commit_loses_nothing_and_duplicates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FjallStore::open(dir.path()).unwrap();
        seed_three(&store);
        let proc = make_process(tally_behavior());

        // Prepare message 0's patch but "crash" before committing (drop it).
        let prepared = proc.prepare(&store).unwrap();
        assert!(prepared.is_some());
        assert_eq!(proc.position(&store).unwrap(), 0, "cursor unmoved before commit");
        // (prepared dropped here — nothing was written)
    }

    // Reopen: message 0 was never committed, so it is re-delivered and the whole
    // inbox drains, each message recorded exactly once.
    let store = FjallStore::open(dir.path()).unwrap();
    let proc = make_process(tally_behavior());
    proc.run_to_idle(&store).unwrap();

    assert_eq!(receipts(&store), expected_receipts());
    assert_eq!(proc.position(&store).unwrap(), 3);
}

#[test]
fn behavior_writing_outside_authority_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    enqueue(&store, INBOX, PROC, 0, Tuple::from([Value::Int(1)])).unwrap();

    // Authority owns only CURSOR, not RECEIPTS — the tally write is illegal.
    let proc = Process {
        entity: PROC,
        authority: Authority::new(DomainId(1), vec![Scope::whole(CURSOR)]),
        inbox: INBOX,
        cursor_rel: CURSOR,
        behavior: tally_behavior(),
    };
    let err = proc.step(&store).unwrap_err();
    assert!(matches!(err, grmpl_core::Error::Authority(_)));
}
