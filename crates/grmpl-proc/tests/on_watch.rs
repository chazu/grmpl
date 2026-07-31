//! P5 acceptance (DESIGN.md §2.3 #6, §2.4 *Attention*, §5.3): reactive handlers
//! (`on watch`).
//!
//! The laws under test:
//!
//! * **Activations are messages (the Attention law).** A view firing does not
//!   run a callback; the pump *appends* one inbox message per signed delta. The
//!   witness is that the target's inbox carries the deltas as ordinary rows a
//!   [`Process`] can drain — reactivity is a maintained query, not a callback
//!   primitive. (This durable materialization is what makes the P10 log
//!   complete.)
//!
//! * **Batch-in-one-commit + durable cursor.** Each pump delivers the whole
//!   batch of deltas since a *durable* watch-cursor and advances that cursor in
//!   the **same atomic commit**, allocating inbox seqs from the shared P4
//!   [`SeqAlloc`].
//!
//! * **Snapshot–stream law over the pump.** `initial + Σ(delivered deltas) =
//!   find(view, current)`. Skip-initial (default) omits `initial`; `including
//!   current` includes it. Checked against an independent hand-computed model.
//!
//! * **Exactly-once, replay-deterministic delivery.** The cursor advance is a
//!   `commit_if` preconditioned on the present cursor row, so racing pumps
//!   resolve to one winner (each activation appears once, at a unique seq); the
//!   delivered stream is a pure function of committed data (replay reproduces
//!   the identical inbox).
//!
//! * **Cascades are async message chains, never reentrancy.** A change made by
//!   handling an activation is delivered by a *later* pump as new messages, not
//!   by a nested call.

use std::collections::BTreeMap;

use grmpl_core::{
    Authority, DomainId, Edition, Entity, NoSchemas, RelId, Scope, TraceStore, Tuple,
    Value,
};
use grmpl_diff::{eval_snapshot, multiset, Query};
use grmpl_proc::{
    decode_activation, inbox_fact, CommitOutcome, OnWatch,
};

const BASE: RelId = RelId(30); // world relation `(a: Int, b: Int)`
const INBOX: RelId = RelId(31);
const CURSOR: RelId = RelId(32);
const SEQS: RelId = RelId(33);

const WATCH: Entity = Entity(9); // cursor key
const TARGET: Entity = Entity(7); // inbox addressee

fn pump_authority() -> Authority {
    Authority::new(
        DomainId(1),
        vec![Scope::whole(INBOX), Scope::whole(CURSOR), Scope::whole(SEQS)],
    )
}

/// The watched view: distinct `a` values that appear in `BASE` with net-positive
/// weight. `project([0])` then `distinct` — a non-linear boundary, so deltas
/// include the ±1 flips a value makes as its net weight crosses zero.
fn view() -> Query {
    Query::rel(BASE).project([0]).distinct()
}

fn on_watch() -> OnWatch {
    OnWatch {
        view: view(),
        watch: WATCH,
        target: TARGET,
        inbox: INBOX,
        cursor_rel: CURSOR,
        seqs: SEQS,
        authority: pump_authority(),
    }
}
/// Commit one world change to `BASE` (a raw store commit — the "world" moving,
/// outside the pump's authority).
fn churn(store: &dyn TraceStore, a: i64, b: i64, diff: i64) {
    store
        .commit(&[(BASE, Tuple::from([Value::Int(a), Value::Int(b)]), diff)])
        .unwrap();
}

/// The activation messages in `TARGET`'s inbox, sorted by seq: `(seq, diff, a)`.
/// Every row must have Z-weight exactly 1 (exactly-once witness).
fn activations(store: &dyn TraceStore) -> Vec<(i64, i64, i64)> {
    let at = store.current();
    let mut v: Vec<(i64, i64, i64)> = Vec::new();
    for (t, d) in store.read_at(INBOX, at).unwrap() {
        assert_eq!(d, 1, "inbox row not weight-1 (duplicate delivery): {t:?}");
        if let [Value::Ent(p), Value::Int(seq), Value::Tuple(body)] = t.as_slice() {
            assert_eq!(*p, TARGET);
            let (diff, row) = decode_activation(&Tuple(body.clone())).expect("activation body");
            let a = match row.as_slice() {
                [Value::Int(a)] => *a,
                other => panic!("unexpected activation row {other:?}"),
            };
            v.push((*seq, diff, a));
        }
    }
    v.sort();
    v
}

/// The cursor edition for `WATCH` (asserting there is exactly one live row).
fn cursor(store: &dyn TraceStore) -> Option<Edition> {
    let at = store.current();
    let rows: Vec<i64> = store
        .read_at(CURSOR, at)
        .unwrap()
        .into_iter()
        .filter(|(t, d)| *d > 0 && t.as_slice().first() == Some(&Value::Ent(WATCH)))
        .filter_map(|(t, _)| match t.as_slice().get(1) {
            Some(Value::Int(e)) => Some(*e),
            _ => None,
        })
        .collect();
    assert!(rows.len() <= 1, "watch cursor is not single-valued: {rows:?}");
    rows.first().map(|e| Edition(*e as u64))
}

/// The view result at `current` as a plain set of `a` values (the ground truth
/// `find(view, current)`), taken from the engine snapshot evaluator.
fn view_now(store: &dyn TraceStore) -> BTreeMap<i64, i64> {
    let snap = eval_snapshot(&view(), store, store.current()).unwrap();
    let mut m = BTreeMap::new();
    for (t, w) in multiset::to_sorted_vec(&snap) {
        if let [Value::Int(a)] = t.as_slice() {
            *m.entry(*a).or_insert(0) += w;
        }
    }
    m
}

/// Sum the delivered activation deltas into a set of `a` values: the net effect
/// of the streamed messages. By the snapshot–stream law this must equal the
/// pump's contribution to `find(view, current)`.
fn delivered_net(acts: &[(i64, i64, i64)]) -> BTreeMap<i64, i64> {
    let mut m = BTreeMap::new();
    for (_, diff, a) in acts {
        *m.entry(*a).or_insert(0) += *diff;
    }
    m.retain(|_, w| *w != 0);
    m
}

// ---------------------------------------------------------------------------
// Focused behavioural tests
// ---------------------------------------------------------------------------

/// Skip-initial (the default): a view with pre-existing content delivers
/// *nothing* on the first pump; only changes committed after install stream.
#[test]
fn skip_initial_default_skips_the_snapshot_then_streams() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        // Pre-existing world state (would be the "initial snapshot").
        churn(store, 1, 10, 1);
        churn(store, 2, 20, 1);

        let ow = on_watch();
        assert!(matches!(ow.install(store, &NoSchemas).unwrap(), CommitOutcome::Committed(_)));

        // First pump: skip-initial — the pre-existing snapshot is NOT delivered.
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 0);
        assert!(activations(store).is_empty());

        // A post-install change streams as one activation (+ on new value 3).
        churn(store, 3, 30, 1);
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 1);
        assert_eq!(activations(store), vec![(0, 1, 3)]);

        // Retracting value 3 to zero flips it out: a `-1` activation.
        churn(store, 3, 30, -1);
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 1);
        assert_eq!(activations(store), vec![(0, 1, 3), (1, -1, 3)]);

        // The cursor is single-valued and lags `current` only by the pump's own
        // non-view commit (it tracks the last edition whose view *change* was
        // delivered). Caught up: an idle pump is a no-op.
        let c = cursor(store).expect("cursor installed");
        assert!(c < store.current(), "cursor should lag current by the pump commit");
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 0);
    });
}

/// `including current`: the first pump delivers the whole current view as `+`
/// rows (the initial snapshot), then streams deltas.
#[test]
fn including_current_delivers_the_snapshot_first() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        churn(store, 1, 10, 1);
        churn(store, 2, 20, 1);
        churn(store, 2, 21, 1); // second row for a=2; distinct still one `2`

        let ow = on_watch();
        assert!(matches!(
            ow.install_including_current(store, &NoSchemas).unwrap(),
            CommitOutcome::Committed(_)
        ));

        // First pump: the current view {1, 2} as two `+1` activations.
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 2);
        let acts = activations(store);
        assert_eq!(acts, vec![(0, 1, 1), (1, 1, 2)]);
        // Snapshot–stream law: delivered net == find(view, current).
        assert_eq!(delivered_net(&acts), view_now(store));
    });
}

/// Activations are ordinary inbox messages a [`Process`] drains — and a change
/// the handler makes is delivered by a *later* pump, not by reentrancy.
#[test]
fn activations_are_messages_and_cascades_are_async() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let ow = on_watch();
        ow.install(store, &NoSchemas).unwrap();

        churn(store, 5, 50, 1);
        // The pump only *appends*; it never runs a behavior. The message is now a
        // durable inbox row (the P10-log-complete property).
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 1);
        let before = store.current();
        assert_eq!(activations(store), vec![(0, 1, 5)]);

        // "Handling" the activation is a *separate* commit (here, a world write the
        // handler would make). It happens strictly after the pump's commit.
        churn(store, 6, 60, 1);
        assert!(store.current() > before);

        // The cascade — the change the handler caused — is only delivered by the
        // NEXT pump, as a new message. No nested/reentrant firing occurred.
        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 1);
        assert_eq!(activations(store), vec![(0, 1, 5), (1, 1, 6)]);
    });
}

// ---------------------------------------------------------------------------
// Randomized-churn law oracle (TKT-91 idiom)
// ---------------------------------------------------------------------------

/// xorshift64* — deterministic, no new dependency.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Inbox activations as `(seq, diff, a)` rows.
type Activations = Vec<(i64, i64, i64)>;
/// A view result / net-delta as `a -> weight`.
type ViewSet = BTreeMap<i64, i64>;

/// Run one seeded scenario in a fresh store: install `including current`, then
/// interleave random `BASE` churn with pumps (sometimes several churns before a
/// pump, exercising multi-delta batches). Returns the final `(activations,
/// view_now)`.
fn run_scenario(case: &grmpl_conformance::Case, seed: u64) -> (Activations, ViewSet) {
    // A fresh store of the same substrate: the scenario's result must be a
    // function of the seed alone.
    let sib = case.sibling();
    let store = sib.store();
    let ow = on_watch();
    ow.install_including_current(store, &NoSchemas).unwrap();

    let mut st = seed ^ 0x9E37_79B9_7F4A_7C15;
    let rounds = 20 + (next_rand(&mut st) % 30) as i64; // 20..49 churns
    for _ in 0..rounds {
        // Small value space forces real cancellation (crossings of zero).
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(store, a, b, diff);
        // Pump most of the time, but not always — so some pumps see a batch of
        // several deltas at once.
        if !next_rand(&mut st).is_multiple_of(3) {
            ow.pump(store, &NoSchemas).unwrap();
        }
    }
    // Final drain so the whole net change has been streamed, then confirm we are
    // caught up (a second pump delivers nothing).
    ow.pump(store, &NoSchemas).unwrap();
    assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 0, "not caught up after drain");

    (activations(store), view_now(store))
}

#[test]
fn pump_delivery_holds_under_random_churn() {
    grmpl_conformance::for_each_store(|c| {
    for seed in 0..32u64 {
        let (acts, view_now) = run_scenario(c, seed);

        // Exactly-once + contiguous seqs: the inbox is 0,1,2,... with no gaps or
        // repeats (activations() already asserted every row is weight-1).
        let seqs: Vec<i64> = acts.iter().map(|(s, ..)| *s).collect();
        let expected_seqs: Vec<i64> = (0..acts.len() as i64).collect();
        assert_eq!(seqs, expected_seqs, "seed {seed}: inbox seqs not contiguous/unique");

        // Snapshot–stream law: including-current, so
        //   Σ(delivered deltas) == find(view, current).
        assert_eq!(
            delivered_net(&acts),
            view_now,
            "seed {seed}: streamed net != find(view, current)"
        );

        // Replay determinism: the same committed churn reproduces the identical
        // inbox (the pump reads no clock/random — a pure function of the trace).
        let (acts2, _) = run_scenario(c, seed);
        assert_eq!(acts, acts2, "seed {seed}: pump is not replay-deterministic");
    }
    });
}

// ---------------------------------------------------------------------------
// Concurrency: racing pumps deliver exactly once
// ---------------------------------------------------------------------------

/// Several pumps racing the same cursor (real threads on a shared store) resolve
/// to one winner per interval: the cursor precondition rejects the losers, which
/// retry. The union of delivered activations is a single clean stream — unique
/// contiguous seqs, each weight-1 — and its net equals `find(view, current)`.
#[test]
fn racing_pumps_deliver_exactly_once() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let ow = on_watch();
        ow.install_including_current(store, &NoSchemas).unwrap();

        // A body of world changes for the pumps to race over.
        let mut st = 0xDEAD_BEEFu64;
        for _ in 0..60 {
            let a = (next_rand(&mut st) % 5) as i64;
            let b = (next_rand(&mut st) % 3) as i64;
            let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
            churn(store, a, b, diff);
        }

        // Several threads hammer pump() concurrently on the shared store.
        std::thread::scope(|s| {
            for _ in 0..4 {
                let ow = on_watch();
                s.spawn(move || {
                    for _ in 0..20 {
                        ow.pump(store, &NoSchemas).unwrap();
                    }
                });
            }
        });
        // Final settle from the main thread.
        ow.pump(store, &NoSchemas).unwrap();

        let acts = activations(store); // asserts every row weight-1 (no duplicates)
        let seqs: Vec<i64> = acts.iter().map(|(s, ..)| *s).collect();
        let expected_seqs: Vec<i64> = (0..acts.len() as i64).collect();
        assert_eq!(seqs, expected_seqs, "racing pumps produced non-contiguous/duplicate seqs");

        assert_eq!(ow.pump(store, &NoSchemas).unwrap(), 0, "racing pumps left work undelivered");
        assert_eq!(
            delivered_net(&acts),
            view_now(store),
            "racing pumps: streamed net != find(view, current)"
        );
    });
}

/// A pump never appends outside `TARGET`'s inbox key, and an inbox message is a
/// well-formed activation the `Process` inbox layout can carry.
#[test]
fn activation_uses_the_shared_inbox_layout() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let ow = on_watch();
        ow.install(store, &NoSchemas).unwrap();
        churn(store, 1, 1, 1);
        ow.pump(store, &NoSchemas).unwrap();

        // The activation row is exactly what `inbox_fact` builds for `(TARGET, 0,
        // body)` — the same layout scheduled/sent messages use, so a Process drains
        // activations with its ordinary loop.
        let body = grmpl_proc::activation_body(1, &Tuple::from([Value::Int(1)]));
        let expected = inbox_fact(INBOX, TARGET, 0, body);
        let at = store.current();
        assert!(store
            .read_at(INBOX, at)
            .unwrap()
            .into_iter()
            .any(|(t, d)| d > 0 && t == expected.tuple));
    });
}
