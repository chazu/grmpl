//! **Seeded randomized contention law oracle** for the P4 `SeqAlloc` swap
//! (TKT-107).
//!
//! The two seq sites this ticket moved onto [`SeqAlloc`] — the cross-domain
//! outbox counter ([`Domain::commit`]) and the per-process inbox seq
//! ([`enqueue_seq`]) — must stay correct under real concurrency. Rather than a
//! handful of example unit tests, this is a **law oracle**: for each of a fixed
//! set of seeds, a self-contained xorshift64\* PRNG shapes a randomized workload,
//! many threads race the same key across whatever interleaving the OS scheduler
//! produces, and the invariants the swap must uphold are asserted over the
//! committed trace. Every assertion prints the seed, so a failure is a
//! one-command reproduction.
//!
//! The laws (per site):
//!   1. **Monotonic + distinct.** Every allocated seq for a key is one of
//!      `0..n` exactly once — strictly increasing, no two messages share a seq,
//!      under contention.
//!   2. **No loss / no dup.** The multiset of durable rows after the race equals
//!      exactly the set of committed allocations: nothing dropped, nothing
//!      duplicated.
//!   3. **Observational equivalence.** The durable-outbox flush ships and the
//!      receiver applies exactly the committed set, once each — the pre-swap
//!      exactly-once delivery semantics are preserved.
//!
//! Modeled on the P4 `seq_alloc_is_race_safe` test and grmpl-bench's contention
//! axis (both share a `FjallStore` across a `thread::scope`).

use std::collections::HashMap;
use std::thread;

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, Message, NoSchemas, Patch, RelId, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_proc::{enqueue_seq, outbox_len, seed_seq, CommitOutcome, Domain};
use grmpl_store::FjallStore;
use grmpl_transport::InProcessNet;

// ---------------------------------------------------------------------------
// A self-contained xorshift64* PRNG — no external `rand` dependency. Deterministic
// per seed, so a failing seed reproduces the exact workload shape (the thread
// interleaving is the OS's, which is where the real contention comes from).
// ---------------------------------------------------------------------------

struct Rng {
    state: u64,
}

impl Rng {
    /// Seed must be nonzero for xorshift; callers pass `seed | 1`.
    fn new(seed: u64) -> Rng {
        Rng { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A draw in `[lo, hi]` inclusive (`hi >= lo`).
    fn in_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
}

/// The seeds the oracle sweeps. 24 in `[16, 32)` — a mix of workload shapes.
const SEEDS: std::ops::Range<u64> = 1..25;

// ---------------------------------------------------------------------------
// Site 1 — the cross-domain outbox counter (Domain.commit racing `outseq`).
// ---------------------------------------------------------------------------

const A: DomainId = DomainId(1);
const B: DomainId = DomainId(2);
const A_OUTBOX: RelId = RelId(90);
const A_OUTSEQ: RelId = RelId(91);
const A_SEEN: RelId = RelId(92);
const B_INBOX: RelId = RelId(10); // owned by B
const WORLD: RelId = RelId(1); // A's world relation

fn a_routes() -> HashMap<RelId, DomainId> {
    let mut routes = HashMap::new();
    routes.insert(B_INBOX, B);
    routes
}

fn a_authority() -> Authority {
    Authority::new(A, vec![Scope::whole(WORLD)])
}

fn wave() -> Patch {
    Patch::new()
        .assert(Fact::new(WORLD, Tuple::from([Value::Ent(Entity(1)), Value::text("did wave")])))
        .emit(Message {
            inbox: B_INBOX,
            body: Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]),
        })
}

/// Race `threads` concurrent committers, each emitting `per_thread` remote waves
/// into A's outbox, then assert the three laws over the drained outbox.
fn outbox_law(seed: u64, threads: u64, per_thread: u64) {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let store_a = FjallStore::open(dir_a.path()).unwrap();
    let store_b = FjallStore::open(dir_b.path()).unwrap();

    let net = InProcessNet::new();
    let tx_a = net.endpoint(A);
    let tx_b = net.endpoint(B);

    // Seed the outbox counter ONCE here, on this un-raced path: the very first
    // allocation has no counter row to precondition on, so without a seed two
    // racing first-commits would both allocate seq 0.
    {
        let da = Domain {
            id: A,
            store: &store_a,
            transport: &tx_a,
            routes: a_routes(),
            outbox: A_OUTBOX,
            outseq: A_OUTSEQ,
            seen: A_SEEN,
        };
        da.seed_outseq().unwrap();
    }

    let store_ref = &store_a;
    let tx_ref = &tx_a;
    // Each thread returns how many of its commits actually committed.
    let committed: Vec<u64> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let da = Domain {
                        id: A,
                        store: store_ref,
                        transport: tx_ref,
                        routes: a_routes(),
                        outbox: A_OUTBOX,
                        outseq: A_OUTSEQ,
                        seen: A_SEEN,
                    };
                    let mut n = 0u64;
                    for _ in 0..per_thread {
                        // `wave` carries no caller precondition, so a lost seq
                        // race retries internally: every commit must succeed.
                        match da.commit(&wave(), &a_authority(), &NoSchemas).unwrap() {
                            CommitOutcome::Committed(_) => n += 1,
                            CommitOutcome::Rejected => {}
                        }
                    }
                    n
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let total: u64 = committed.iter().sum();
    let expected = threads * per_thread;
    assert_eq!(
        total, expected,
        "seed={seed}: every uncontended-precondition commit must succeed \
         (got {total}, expected {expected})"
    );

    // Collect the outbox seqs (column 0). Each row must have weight exactly 1.
    let at = store_a.current();
    let mut seqs: Vec<i64> = Vec::new();
    for (t, d) in store_a.read_at(A_OUTBOX, at).unwrap() {
        assert!(d == 1, "seed={seed}: every outbox row has weight 1, saw {d}");
        match t.as_slice().first() {
            Some(Value::Int(s)) => seqs.push(*s),
            other => panic!("seed={seed}: malformed outbox row, col0={other:?}"),
        }
    }
    seqs.sort_unstable();

    // Law 1 (monotonic + distinct) & Law 2 (no loss / no dup): the seqs are
    // exactly 0..expected, each present once.
    let want: Vec<i64> = (0..expected as i64).collect();
    assert_eq!(
        seqs, want,
        "seed={seed}: outbox seqs must be exactly 0..{expected} distinct (contiguous, no dup, no gap)"
    );

    // Law 3 (observational equivalence): the durable-outbox flush ships every
    // row once, the receiver applies each exactly once (dedup by (sender, seq)),
    // and the outbox drains — the pre-swap exactly-once delivery semantics.
    let da = Domain {
        id: A,
        store: &store_a,
        transport: &tx_a,
        routes: a_routes(),
        outbox: A_OUTBOX,
        outseq: A_OUTSEQ,
        seen: A_SEEN,
    };
    let db = Domain {
        id: B,
        store: &store_b,
        transport: &tx_b,
        routes: HashMap::new(),
        outbox: RelId(90),
        outseq: RelId(91),
        seen: A_SEEN,
    };
    let shipped = da.flush_outbox().unwrap();
    assert_eq!(shipped as u64, expected, "seed={seed}: flush ships every committed emit once");
    assert_eq!(
        outbox_len(&store_a, A_OUTBOX, store_a.current()).unwrap(),
        0,
        "seed={seed}: the outbox drains after flush"
    );

    let applied = db.receive().unwrap();
    assert_eq!(applied as u64, expected, "seed={seed}: receiver applies every message once");
    // The wave body arrives with weight == number of committed emits (each a
    // distinct seq, so none deduped).
    let inbox = store_b.read_at(B_INBOX, store_b.current()).unwrap();
    let weight: i64 = inbox
        .iter()
        .filter(|(t, _)| t.as_slice().get(1) == Some(&Value::text("wave")))
        .map(|(_, d)| *d)
        .sum();
    assert_eq!(
        weight, expected as i64,
        "seed={seed}: no loss / no dup across the flush — inbox weight == committed emits"
    );

    // A redelivery of the same envelopes applies nothing new (idempotent).
    let reapplied = db.receive().unwrap();
    assert_eq!(reapplied, 0, "seed={seed}: nothing left to deliver");
}

#[test]
fn outbox_seq_is_race_safe_across_seeds() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        let threads = rng.in_range(2, 4);
        let per_thread = rng.in_range(6, 16);
        outbox_law(seed, threads, per_thread);
    }
}

// ---------------------------------------------------------------------------
// Site 2 — the per-process inbox seq (`enqueue_seq` racing per-key).
// ---------------------------------------------------------------------------

const INBOX: RelId = RelId(10);
const INBOX_SEQ: RelId = RelId(12);

fn player(i: u64) -> Entity {
    Entity(100 + i)
}

/// Race `threads` concurrent appenders over `players` inbox keys, each thread
/// doing `per_thread` appends to PRNG-chosen keys, then assert the seq laws
/// per key.
fn inbox_law(seed: u64, threads: u64, per_thread: u64, players: u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    // Seed every key once, on this un-raced path.
    for i in 0..players {
        seed_seq(&store, INBOX_SEQ, player(i)).unwrap();
    }

    let store_ref = &store;
    // Each thread returns the (player_index, seq) it was handed for every append.
    let per_thread_out: Vec<Vec<(u64, i64)>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|tid| {
                // A distinct, deterministic RNG per thread so the workload shape
                // is a function of the seed alone (the interleaving is the OS's).
                let mut rng = Rng::new(seed ^ (tid.wrapping_mul(0x9E37_79B9) + 1));
                let targets: Vec<u64> =
                    (0..per_thread).map(|_| rng.in_range(0, players - 1)).collect();
                scope.spawn(move || {
                    let mut out = Vec::new();
                    for (k, pidx) in targets.into_iter().enumerate() {
                        let body =
                            Tuple::from([Value::Int(tid as i64), Value::Int(k as i64)]);
                        let seq =
                            enqueue_seq(store_ref, INBOX, INBOX_SEQ, player(pidx), body).unwrap();
                        out.push((pidx, seq));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Per key: the seqs handed out, and the seqs actually present in the inbox.
    let mut handed: HashMap<u64, Vec<i64>> = HashMap::new();
    for thread_out in &per_thread_out {
        for (pidx, seq) in thread_out {
            handed.entry(*pidx).or_default().push(*seq);
        }
    }

    let at = store.current();
    let rows = store.read_at(INBOX, at).unwrap();
    for i in 0..players {
        let p = player(i);
        // Seqs handed out for this key, sorted.
        let mut given = handed.remove(&i).unwrap_or_default();
        let count = given.len();
        given.sort_unstable();

        // Law 1 (monotonic + distinct): the returned seqs are exactly 0..count.
        let want: Vec<i64> = (0..count as i64).collect();
        assert_eq!(
            given, want,
            "seed={seed}: inbox seqs handed out for player {i} must be 0..{count} distinct"
        );

        // Law 2 (no loss / no dup): the durable inbox rows for this key are
        // exactly those seqs, each weight 1.
        let mut present: Vec<i64> = Vec::new();
        for (t, d) in &rows {
            let s = t.as_slice();
            if s.first() == Some(&Value::Ent(p)) {
                assert!(*d == 1, "seed={seed}: inbox row for player {i} has weight {d}, want 1");
                match s.get(1) {
                    Some(Value::Int(seq)) => present.push(*seq),
                    other => panic!("seed={seed}: malformed inbox row seq col={other:?}"),
                }
            }
        }
        present.sort_unstable();
        assert_eq!(
            present, want,
            "seed={seed}: durable inbox rows for player {i} must match the {count} handed-out seqs"
        );

        // The counter landed at exactly `count`.
        let counter: Vec<i64> = store
            .read_at(INBOX_SEQ, at)
            .unwrap()
            .into_iter()
            .filter(|(t, d)| *d > 0 && t.as_slice().first() == Some(&Value::Ent(p)))
            .filter_map(|(t, _)| match t.as_slice().get(1) {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(
            counter,
            vec![count as i64],
            "seed={seed}: player {i} counter must rest at {count} (single row)"
        );
    }
}

#[test]
fn inbox_seq_is_race_safe_across_seeds() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        let threads = rng.in_range(2, 4);
        let per_thread = rng.in_range(6, 16);
        let players = rng.in_range(1, 3);
        inbox_law(seed, threads, per_thread, players);
    }
}
