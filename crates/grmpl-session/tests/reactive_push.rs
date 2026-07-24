//! P5 follow-on acceptance (TKT-109): reactive push of on-watch activations to
//! a session client.
//!
//! The core is a **seeded randomized-churn law oracle**, not example unit tests:
//! over many seeds it drives a random world (`BASE` churn) through a
//! [`Subscription`] whose pump materializes activations, drains them to a mock
//! socket (a `Vec<u8>` — `grmpl-session` is an edge crate, so naming the socket
//! is fine; the bright line only constrains the core), and asserts two laws each
//! round:
//!
//! 1. **Exactly-once, in-order delivery.** The sequence of activations *streamed
//!    to the socket* equals the sequence of inbox messages the pump *materialized*
//!    — decoded from the durable inbox in seq order, which is the pump's commit
//!    order `(edition, counter)`, never the store's physical scan order — with no
//!    loss and no duplication (contiguous seqs `0..n`, each exactly once).
//!
//! 2. **Durable resume.** After a simulated disconnect + reconnect mid-stream
//!    (with activations materialized but not yet drained), a fresh `Subscription`
//!    over the same store resumes from the durable delivery cursor:
//!    already-drained activations are not re-delivered and none are skipped — the
//!    drain is a prefix-consume of the durable inbox.
//!
//! Two example witnesses (`witness_*`) pin the concrete behaviour; the oracle is
//! the actual coverage.

use std::collections::BTreeSet;

use grmpl_core::{EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_diff::Query;
use grmpl_proc::decode_activation;
use grmpl_session::world::WATCH_INBOX;
use grmpl_session::Subscription;
use grmpl_store::FjallStore;

/// One decoded activation as observed on the wire / in the inbox: `(seq, diff,
/// a)`.
type Act = (i64, i64, i64);

// The random "world" relation the watched view reads: `(a, b)`.
const BASE: RelId = RelId(40);
// Subscription identity: `watch` keys the durable cursors, `player` is the inbox
// addressee. Distinct entities, so a target/watch mix-up would be caught.
const WATCH: Entity = Entity(700);
const PLAYER: Entity = Entity(701);

/// xorshift64* — deterministic, no external `rand` dependency.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn store() -> (tempfile::TempDir, FjallStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    (dir, store)
}

/// The watched view: distinct `a` values with net-positive weight in `BASE`.
/// `project([0]).distinct()` is a non-linear boundary, so deltas are the ±1
/// flips a value makes as its net weight crosses zero — real cancellation.
fn view() -> Query {
    Query::rel(BASE).project([0]).distinct()
}

fn subscription() -> Subscription {
    Subscription::new(view(), WATCH, PLAYER)
}

/// Commit one world change to `BASE` (a raw store commit — the world moving,
/// outside the pump's authority).
fn churn(store: &FjallStore, a: i64, b: i64, diff: i64) {
    store
        .commit(&[(BASE, Tuple::from([Value::Int(a), Value::Int(b)]), diff)])
        .unwrap();
}

/// The activations the pump has *materialized* into `PLAYER`'s durable inbox, in
/// **seq order** (the pump's commit order): `(seq, diff, a)`. Every inbox row
/// must be weight-1 (an exactly-once witness at the source).
fn materialized(store: &FjallStore) -> Vec<Act> {
    let at = store.current();
    let mut v: Vec<Act> = Vec::new();
    for (t, d) in store.read_at(WATCH_INBOX, at).unwrap() {
        let s = t.as_slice();
        if s.first() == Some(&Value::Ent(PLAYER)) {
            assert_eq!(d, 1, "inbox row not weight-1 (duplicate materialization): {t:?}");
            if let (Some(Value::Int(seq)), Some(Value::Tuple(body))) = (s.get(1), s.get(2)) {
                let (diff, row) = decode_activation(&Tuple(body.clone())).expect("activation body");
                let a = match row.as_slice() {
                    [Value::Int(a)] => *a,
                    other => panic!("unexpected activation row {other:?}"),
                };
                v.push((*seq, diff, a));
            }
        }
    }
    v.sort();
    v
}

/// Parse the bytes actually written to the mock socket back into `(seq, diff,
/// a)` — the sequence a real line client would observe. Each activation is one
/// `"{seq} {diff} {a}"` line (see [`grmpl_session::Delivered::render`]).
fn parse_socket(bytes: &[u8]) -> Vec<Act> {
    let text = std::str::from_utf8(bytes).expect("socket bytes are utf-8");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let toks: Vec<i64> =
                l.split_whitespace().map(|t| t.parse().expect("numeric activation token")).collect();
            assert_eq!(toks.len(), 3, "activation line has unexpected arity: {l:?}");
            (toks[0], toks[1], toks[2])
        })
        .collect()
}

/// Assert a streamed sequence is a clean exactly-once, in-order stream: seqs are
/// exactly `0, 1, …, n-1` (contiguous, unique — no loss, no duplication).
fn assert_clean_stream(streamed: &[Act], ctx: &str) {
    let seqs: Vec<i64> = streamed.iter().map(|(s, ..)| *s).collect();
    let expected: Vec<i64> = (0..streamed.len() as i64).collect();
    assert_eq!(seqs, expected, "{ctx}: streamed seqs not contiguous/unique (loss or duplication)");
}

// ---------------------------------------------------------------------------
// Law 1: exactly-once, in-order delivery under random churn
// ---------------------------------------------------------------------------

/// One seeded scenario: install `including current`, then interleave random
/// `BASE` churn with `pump_and_drain` into a single mock socket (sometimes
/// several churns before a drain, so a drain hands over a batch of deltas).
/// Returns `(streamed-from-socket, materialized-inbox)`.
fn run_delivery(seed: u64) -> (Vec<Act>, Vec<Act>) {
    let (_d, store) = store();
    let sub = subscription();
    sub.install_including_current(&store).unwrap();

    let mut socket: Vec<u8> = Vec::new();
    let mut st = seed ^ 0x9E37_79B9_7F4A_7C15;
    let rounds = 20 + (next_rand(&mut st) % 30) as i64; // 20..49 churns
    for _ in 0..rounds {
        // Small value space forces crossings of zero (real ± cancellation).
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(&store, a, b, diff);
        // Drain most rounds, but not always — so some drains carry a multi-delta
        // batch pumped across several churns.
        if !next_rand(&mut st).is_multiple_of(3) {
            sub.pump_and_drain(&store, &mut socket).unwrap();
        }
    }
    // Final drain so the whole net change has been streamed, then confirm we are
    // caught up (a second pump-and-drain streams nothing).
    sub.pump_and_drain(&store, &mut socket).unwrap();
    assert!(
        sub.pump_and_drain(&store, &mut socket).unwrap().is_empty(),
        "seed {seed}: not caught up after final drain"
    );

    (parse_socket(&socket), materialized(&store))
}

#[test]
fn delivery_is_exactly_once_and_in_order_under_random_churn() {
    for seed in 0..32u64 {
        let (streamed, materialized) = run_delivery(seed);

        // Exactly-once + in-order + no loss: the bytes on the socket are exactly
        // the materialized inbox, in seq (commit) order.
        assert_eq!(
            streamed, materialized,
            "seed {seed}: socket stream != materialized inbox (loss/dup/reorder)"
        );
        assert_clean_stream(&streamed, &format!("seed {seed}"));

        // Replay determinism: the same committed churn reproduces the identical
        // socket stream (the pump and drain read no clock/random — pure functions
        // of the committed trace). Checked on a subset — each replay is a full
        // fjall store — since the law above already runs on every seed.
        if seed < 8 {
            let (streamed2, _) = run_delivery(seed);
            assert_eq!(streamed, streamed2, "seed {seed}: delivery is not replay-deterministic");
        }
    }
}

// ---------------------------------------------------------------------------
// Law 2: durable resume across a mid-stream disconnect + reconnect
// ---------------------------------------------------------------------------

/// One seeded scenario: subscription A delivers a while, then some activations
/// are **materialized but NOT drained** (the mid-stream disconnect state), A is
/// dropped ("disconnect"), and a fresh subscription B over the same store
/// ("reconnect") resumes from the durable delivery cursor. Returns the bytes A
/// streamed, the bytes B streamed, and the final materialized inbox (ground
/// truth), all separately.
fn run_resume(seed: u64) -> (Vec<u8>, Vec<u8>, Vec<Act>) {
    let (_d, store) = store();
    let mut st = seed ^ 0x1234_5678_9ABC_DEF0;

    // ---- connection A ----
    let sub_a = subscription();
    sub_a.install_including_current(&store).unwrap();
    let mut socket_a: Vec<u8> = Vec::new();

    let a_rounds = 10 + (next_rand(&mut st) % 15) as i64;
    for _ in 0..a_rounds {
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(&store, a, b, diff);
        if !next_rand(&mut st).is_multiple_of(3) {
            sub_a.pump_and_drain(&store, &mut socket_a).unwrap();
        }
    }
    // Mid-stream: churn more and PUMP (materialize) without draining, so the
    // durable inbox now holds activations A never delivered to its socket.
    for _ in 0..(3 + (next_rand(&mut st) % 6)) {
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(&store, a, b, diff);
    }
    sub_a.pump(&store).unwrap(); // materialize; delivery cursor NOT advanced
    // "disconnect": A goes away with undrained activations pending.
    drop(sub_a);

    // ---- connection B (reconnect) ----
    // A brand-new subscription over the SAME store + watch identity. It must NOT
    // reinstall (that would double the on-watch cursor); it just resumes.
    let sub_b = subscription();
    assert!(sub_b.is_installed(&store).unwrap(), "seed {seed}: watch should already be installed");
    let mut socket_b: Vec<u8> = Vec::new();

    let b_rounds = 10 + (next_rand(&mut st) % 15) as i64;
    for _ in 0..b_rounds {
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(&store, a, b, diff);
        if !next_rand(&mut st).is_multiple_of(3) {
            sub_b.pump_and_drain(&store, &mut socket_b).unwrap();
        }
    }
    // Final drain on B so everything materialized has been streamed.
    sub_b.pump_and_drain(&store, &mut socket_b).unwrap();
    assert!(
        sub_b.pump_and_drain(&store, &mut socket_b).unwrap().is_empty(),
        "seed {seed}: B not caught up after final drain"
    );

    (socket_a, socket_b, materialized(&store))
}

#[test]
fn durable_resume_across_reconnect_never_loses_or_duplicates() {
    for seed in 0..32u64 {
        let (socket_a, socket_b, materialized) = run_resume(seed);
        let stream_a = parse_socket(&socket_a);
        let stream_b = parse_socket(&socket_b);

        let seqs_a: BTreeSet<i64> = stream_a.iter().map(|(s, ..)| *s).collect();
        let seqs_b: BTreeSet<i64> = stream_b.iter().map(|(s, ..)| *s).collect();

        // No re-delivery: B never re-streams a seq A already streamed (the
        // durable delivery cursor A advanced excludes them).
        assert!(
            seqs_a.is_disjoint(&seqs_b),
            "seed {seed}: reconnect re-delivered activations A already streamed: {:?}",
            &seqs_a & &seqs_b
        );

        // No loss, no skip: A's stream then B's stream, concatenated, is the whole
        // materialized inbox in seq order — a clean 0..n prefix-consume split
        // across the disconnect.
        let mut combined = stream_a.clone();
        combined.extend(stream_b.iter().cloned());
        combined.sort();
        assert_eq!(
            combined, materialized,
            "seed {seed}: A ++ B != materialized inbox (loss or skip across reconnect)"
        );
        assert_clean_stream(&combined, &format!("seed {seed}: combined"));
    }
}

// ---------------------------------------------------------------------------
// Example witnesses (concrete, human-readable; the oracles above are coverage)
// ---------------------------------------------------------------------------

/// Skip-initial: a view with pre-existing content pushes *nothing* on the first
/// drain; a post-install change then streams as one activation line.
#[test]
fn witness_skip_initial_then_pushes_a_change() {
    let (_d, store) = store();
    churn(&store, 1, 10, 1); // pre-existing (would be the initial snapshot)

    let sub = subscription();
    sub.install(&store).unwrap();

    let mut socket: Vec<u8> = Vec::new();
    // First drain: skip-initial — the pre-existing snapshot is not delivered.
    assert!(sub.pump_and_drain(&store, &mut socket).unwrap().is_empty());
    assert!(socket.is_empty());

    // A new value streams as one `+1` activation line "0 1 2".
    churn(&store, 2, 20, 1);
    let delivered = sub.pump_and_drain(&store, &mut socket).unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(parse_socket(&socket), vec![(0, 1, 2)]);
}

/// Concrete durable resume: A delivers one activation, then a second is
/// materialized-but-undrained; B (reconnect) streams exactly the second, never
/// the first.
#[test]
fn witness_reconnect_resumes_from_cursor() {
    let (_d, store) = store();
    let sub_a = subscription();
    sub_a.install(&store).unwrap();

    let mut socket_a: Vec<u8> = Vec::new();
    churn(&store, 5, 0, 1);
    assert_eq!(sub_a.pump_and_drain(&store, &mut socket_a).unwrap().len(), 1);
    assert_eq!(parse_socket(&socket_a), vec![(0, 1, 5)]); // A got seq 0

    // A second change is materialized but NOT drained, then A disconnects.
    churn(&store, 6, 0, 1);
    sub_a.pump(&store).unwrap();
    drop(sub_a);

    // B reconnects and resumes from the durable cursor: it streams only seq 1.
    let sub_b = subscription();
    let mut socket_b: Vec<u8> = Vec::new();
    let delivered = sub_b.pump_and_drain(&store, &mut socket_b).unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(parse_socket(&socket_b), vec![(1, 1, 6)]); // seq 1 only — seq 0 not re-sent
}
