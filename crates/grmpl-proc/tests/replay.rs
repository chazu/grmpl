//! Replay (P10): a process's patches are a pure function of committed history.
//!
//! The behavior here is deliberately *stateful* — each message reads a running
//! total from the world and rewrites it — so a faithful replay must read each
//! historical step against the exact edition it committed against. If replay
//! read the world at `current` instead, every re-derived patch would retract the
//! wrong old total and the assertion below would fail. That is the whole point:
//! `replay_from` reproduces the identical patches only because it reads as-of.

use grmpl_core::{
    Authority, DomainId, Edition, Entity, Fact, NoSchemas, Patch, RelId, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_proc::{enqueue, record_run, replay_from, Behavior, Process, Step};

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
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let proc = actor();

        feed(store, 0, &[10, 5, -3, 20, 1]);
        let live = record_run(store, &proc, &NoSchemas).unwrap();
        assert_eq!(live.len(), 5, "all five messages committed");

        // Re-derived purely from history, over the whole trace: identical patches.
        let replayed = replay_from(store, &proc, Edition::ZERO).unwrap();
        assert_eq!(
            live, replayed,
            "replay reproduces the live log step for step"
        );

        // Replay is itself deterministic — a second re-derivation matches.
        let again = replay_from(store, &proc, Edition::ZERO).unwrap();
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
        let snap = Snapshot::at_current(store);
        assert_eq!(total_of(&snap), Some(33));
    });
}

#[test]
fn replay_from_a_midpoint_covers_only_later_steps() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let proc = actor();

        // First batch, remembering the edition after it.
        feed(store, 0, &[10, 5]);
        record_run(store, &proc, &NoSchemas).unwrap();
        let mid = store.current();

        // Second batch.
        feed(store, 2, &[100, 200]);
        let later = record_run(store, &proc, &NoSchemas).unwrap();

        // Replaying from `mid` re-derives exactly the second batch's steps.
        let replayed = replay_from(store, &proc, mid).unwrap();
        assert_eq!(
            later, replayed,
            "midpoint replay covers only the post-`mid` steps"
        );
        assert_eq!(
            replayed.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
    });
}

#[test]
fn replay_below_watermark_is_rejected() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let proc = actor();

        feed(store, 0, &[1, 2, 3, 4]);
        record_run(store, &proc, &NoSchemas).unwrap();

        // Consolidate away early history, then a replay reaching below the watermark
        // must error at the door rather than answer from truncated history.
        let wm = store.consolidate(store.current()).unwrap();
        assert!(wm.0 > 0, "consolidation advanced the watermark");
        let err = replay_from(store, &proc, Edition::ZERO).unwrap_err();
        assert!(
            format!("{err}").contains("watermark"),
            "replay below the watermark is rejected loudly: {err}"
        );
    });
}

// --- Randomized-churn law oracle (the Replay law under arbitrary history) -----
//
// The example tests above pin *one* fixed trace each. The steward bar for a
// determinism/replay law (CLAUDE.md §Determinism) is that it holds under
// *arbitrary* committed history, so this oracle churns random histories across
// several interleaved processes and re-asserts the Replay law every seed:
//
//   * full replay — `replay_from(ZERO)` reproduces the live `record_run` log
//     step-for-step, for every process;
//   * idempotence — replaying again yields the identical log;
//   * midpoint   — `replay_from(mid)` for a random edition equals exactly the
//     live tail committed after `mid`.
//
// The three processes share the inbox, cursor and world (`TOTAL`) relations,
// keyed by entity, and their commits interleave, so a process's cursor advances
// land at *non-contiguous* editions and its behavior reads a `TOTAL` that its
// peers also mutate — exactly the case `replay_from` must reconstruct from
// commit-order history (not a fixed trace), reading each step as-of its own
// pre-commit edition. Runs alternate with fresh enqueues so a single
// `record_run` consumes several messages at once. A seedable xorshift64* PRNG
// (mirroring grmpl-store/tests/determinism.rs) keeps each round reproducible and
// every assertion prints its `seed` so a failure replays directly.

/// Deterministic, seedable PRNG (xorshift64*). Reproducible churn without an
/// external `rand`/`proptest` dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point of xorshift.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

const NPROC: usize = 3;
const PROCS: [Entity; NPROC] = [Entity(1), Entity(2), Entity(3)];

/// A ledger process for `entity`, sharing the inbox/cursor/world relations with
/// its peers (all keyed by entity).
fn proc_for(entity: Entity) -> Process {
    Process {
        entity,
        authority: Authority::new(DomainId(1), vec![Scope::whole(TOTAL)]),
        inbox: INBOX,
        cursor_rel: CURSOR,
        behavior: ledger_behavior(),
    }
}

#[test]
fn replay_reproduces_history_under_random_churn() {
    grmpl_conformance::for_each_store(|c| {
        for seed in 1..=24u64 {
            churn_replay_round(c, seed);
        }
    });
}

fn churn_replay_round(case: &grmpl_conformance::Case, seed: u64) {
    // Fresh store of the same substrate: the round replays its own history.
    let sib = case.sibling();
    let store = sib.store();
    let mut rng = Rng::new(seed);

    let procs: Vec<Process> = PROCS.iter().map(|e| proc_for(*e)).collect();
    let mut next_seq = [0i64; NPROC];
    // The live commit-order log per process, accumulated across every run.
    let mut live: Vec<Vec<Step>> = vec![Vec::new(); PROCS.len()];

    let actions = 30 + rng.below(30); // 30..=59 interleaved actions
    for _ in 0..actions {
        let pi = rng.below(PROCS.len() as u64) as usize;
        if rng.below(3) != 0 {
            // Enqueue a random batch (2/3 of the time) — keep a backlog so the
            // next run of this process consumes several messages at once.
            let batch = 1 + rng.below(4); // 1..=4 messages
            for _ in 0..batch {
                let n = rng.below(11) as i64 - 5; // -5..=5, negatives force retraction
                enqueue(store, INBOX, PROCS[pi], next_seq[pi], Tuple::from([Value::Int(n)]))
                    .unwrap();
                next_seq[pi] += 1;
            }
        } else {
            // Drive this process to idle, recording its slice of the log.
            let steps = record_run(store, &procs[pi], &NoSchemas).unwrap();
            live[pi].extend(steps);
        }
    }

    // Drain every process so all enqueued messages are committed history.
    for (i, p) in procs.iter().enumerate() {
        let steps = record_run(store, p, &NoSchemas).unwrap();
        live[i].extend(steps);
    }

    let current = store.current();
    for (i, p) in procs.iter().enumerate() {
        // Full replay reproduces the live log step for step, purely from history.
        let replayed = replay_from(store, p, Edition::ZERO).unwrap();
        assert_eq!(
            live[i], replayed,
            "seed {seed}: full replay differs from the live log for {:?}",
            PROCS[i]
        );

        // Replay is deterministic — a second re-derivation matches.
        let again = replay_from(store, p, Edition::ZERO).unwrap();
        assert_eq!(replayed, again, "seed {seed}: replay not idempotent for {:?}", PROCS[i]);

        // A replay from a random midpoint covers exactly the steps this process
        // committed after that edition — the live tail, nothing before it.
        let mid = Edition(rng.below(current.0 + 1));
        let tail = replay_from(store, p, mid).unwrap();
        let want_tail: Vec<Step> =
            live[i].iter().filter(|s| s.edition.0 > mid.0).cloned().collect();
        assert_eq!(
            tail, want_tail,
            "seed {seed}: midpoint replay (mid={}) differs from the live tail for {:?}",
            mid.0, PROCS[i]
        );
    }
}
