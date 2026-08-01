//! **Parse-stream law oracle** (TKT-116; P9c design §6, §8.4) — the acceptance
//! test for incremental match maintenance.
//!
//! `ParseStream` maintains a `DeltaStream` *of parses*: it emits `M(W₀)` and
//! then signed parse deltas as the window advances. This file is the oracle for
//! the law that makes that meaningful, written in the shape the repo convention
//! requires (TKT-112) and mirroring the **M2 Snapshot–stream oracle**
//! (`tests/snapshot_stream.rs`) one level up the tower: a **seeded-xorshift
//! randomized-churn** test that re-checks its invariant on **every delta step**,
//! not example unit tests. Every assertion prints its `seed` and `round`, and
//! the PRNG is a hand-rolled xorshift64* so there is no `rand`/`proptest`
//! dependency.
//!
//! The laws, per round, under randomized insert/**retract** churn:
//!
//! 1. **The lifted Snapshot–stream law** (the central one, §6). For a
//!    monotonically advancing window sequence `W₀ ⊆ W₁ ⊆ …`,
//!
//!    ```text
//!      M(W₀)  +  Σ deltas  =  M(W_current)
//!    ```
//!
//!    where the right-hand side is the parser **re-run from scratch** over the
//!    current window. The oracle folds every emitted delta into an accumulator
//!    and compares it against that fresh recompute after *each* advance, in both
//!    matching modes — state mode (`Window::consolidate` + the `&[Value]` slice
//!    engine) and event mode (`Window::events` + `DeltaInput`) — since the law
//!    is stated "in a fixed mode" and must hold for either. It also asserts the
//!    maintained set itself equals the recompute, so the law cannot pass by two
//!    compensating errors in the deltas.
//!
//! 2. **The Delete–Rederive trap** (why this is the hard piece). A retraction
//!    removes a row from a state-mode window, and every parse resting on that
//!    row loses a derivation. The oracle uses a grammar whose support relation
//!    it knows analytically — parse `("key", k)` is derived by *every* present
//!    row with key `k` — computes the support from its **own world model**
//!    (never from the implementation), and asserts both halves:
//!
//!    * **(a) last support gone → the parse disappears.** The delta must carry
//!      `(parse, −1)` and the parse must leave the maintained set. No stale
//!      parse survives a retraction.
//!    * **(b) support remains → the parse survives.** The delta must *not*
//!      carry `(parse, −1)` and the parse must stay. This is the classic trap: a
//!      maintainer that deletes every parse touched by a retracted row
//!      over-removes, and DRed exists precisely to re-derive it.
//!
//!    Case (b) is not assumed to be live — a **naive-delete negative control**
//!    runs alongside (overdelete the retracted rows' parses, add the newly
//!    asserted rows' parses, never re-derive) and a per-seed counter asserts it
//!    really did over-delete somewhere. If the trap never fired, the law would
//!    be vacuous, so the oracle fails rather than passing quietly.
//!
//! 3. **Determinism and path independence.** Emitted deltas are strictly
//!    ascending by `Value` and zero-free, so no `HashMap` fold order leaks into
//!    a result; replaying the identical trace yields the identical delta
//!    sequence; and a stream advanced in one jump to the same far end reaches
//!    the same match-set as one advanced step by step — `M` is a function of the
//!    window, not of the path taken to it.
//!
//! `grmpl-pattern` is a **dev-dependency only**: the maintenance layer never
//! names the algebra (a `WindowParser` is supplied here, in the test), and
//! `grmpl-diff` reaches the trace solely through the `TraceStore` trait.

use std::collections::{HashMap, HashSet};

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Value};
use grmpl_diff::{window, ParseStream, Query, Window, WindowParser};
use grmpl_pattern::{Bindings, DeltaInput, Form, Pattern, Rule, VarId};
use grmpl_ent::EntStore;

const REL: RelId = RelId(1);
const SEEDS: std::ops::Range<u64> = 1..25; // 24 seeds
const ROUNDS: usize = 16;

/// Keys are drawn from a domain small enough that rows collide on a key — which
/// is what gives a parse *alternative support* and makes re-derivation
/// load-bearing — yet small enough that a key can also lose its **last**
/// support, which is the other half of the trap. Both halves are asserted to
/// have fired, per seed, so this tuning can never go quietly vacuous.
const KEYS: u64 = 3;
const PAYLOADS: u64 = 2;

// ---------------------------------------------------------------------------
// PRNG, world model, churn generator
// ---------------------------------------------------------------------------

/// Deterministic, seedable PRNG (xorshift64*) — the repo idiom; reproducible
/// churn with no external dependency.
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

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn tup(k: u64, p: u64) -> Tuple {
    Tuple::from([Value::Int(k as i64), Value::Ent(Entity(p))])
}

/// The key cell of a row — the column the trap grammar derives its parse from.
fn key_of(t: &Tuple) -> i64 {
    match &t.0[0] {
        Value::Int(k) => *k,
        v => panic!("row key cell is not an Int: {v:?}"),
    }
}

/// The world the churn maintains, so the generator keeps every weight
/// non-negative (a realistic trace) while still producing multiplicities > 1.
type World = HashMap<Tuple, Diff>;

/// What the generator committed, per edition, in submit order — the oracle's
/// own record of the trace, from which it recomputes truth without ever
/// consulting `grmpl-diff`.
type Log = Vec<(Edition, Vec<(Tuple, Diff)>)>;

fn apply(world: &mut World, t: &Tuple, d: Diff) {
    *world.entry(t.clone()).or_insert(0) += d;
}

fn weight(world: &World, t: &Tuple) -> Diff {
    world.get(t).copied().unwrap_or(0)
}

/// Commit one edition of 1–3 random churn updates over a small overlapping
/// domain. `retract_bias` in `0..=4` is the chance in four of retracting a row
/// the world actually holds — the case the whole ticket is about — so a high
/// bias keeps the world thin and lets a key lose its *last* support, while rows
/// still collide on keys often enough to leave alternative support. Magnitudes
/// reach 2 sometimes, so a second derivation from the *same* row (multiplicity
/// two) is exercised too. Retractions never drive a weight below zero.
fn churn_edition(
    store: &EntStore,
    world: &mut World,
    rng: &mut Rng,
    log: &mut Log,
    retract_bias: u64,
) {
    let n = 1 + rng.below(3);
    let mut batch: Vec<(RelId, Tuple, Diff)> = Vec::new();
    let mut pending = World::new();
    for _ in 0..n {
        let t = tup(rng.below(KEYS), rng.below(PAYLOADS));
        let mag = if rng.below(4) == 0 { 2 } else { 1 };
        let held = weight(world, &t) + weight(&pending, &t);
        let d = if held >= mag && rng.below(4) < retract_bias { -mag } else { mag };
        apply(&mut pending, &t, d);
        batch.push((REL, t, d));
    }
    let at = store.commit(&batch).unwrap();
    for (_, t, d) in &batch {
        apply(world, t, *d);
    }
    log.push((at, batch.into_iter().map(|(_, t, d)| (t, d)).collect()));
}

/// Commit one edition, keeping the world model and log in step.
fn commit_batch(store: &EntStore, world: &mut World, log: &mut Log, batch: Vec<(Tuple, Diff)>) {
    if batch.is_empty() {
        return;
    }
    let rows: Vec<(RelId, Tuple, Diff)> =
        batch.iter().map(|(t, d)| (REL, t.clone(), *d)).collect();
    let at = store.commit(&rows).unwrap();
    for (t, d) in &batch {
        apply(world, t, *d);
    }
    log.push((at, batch));
}

/// Retract **every** present occurrence of one key in a single edition — the
/// targeted way to drive trap case (a): the key's last derivation is destroyed,
/// so its parse must genuinely disappear. Left to chance this is rare (a key
/// usually keeps some support), and a law that never exercises its hard case is
/// worthless, so the generator aims at it deliberately. Returns whether the key
/// had anything to retract.
fn retract_whole_key(store: &EntStore, world: &mut World, log: &mut Log, k: u64) -> bool {
    let batch: Vec<(Tuple, Diff)> = world
        .iter()
        .filter(|(t, d)| **d > 0 && key_of(t) == k as i64)
        .map(|(t, d)| (t.clone(), -*d))
        .collect();
    let hit = !batch.is_empty();
    commit_batch(store, world, log, batch);
    hit
}

/// Retract **one** occurrence of a key that has at least two — the targeted way
/// to drive trap case (b): a derivation is destroyed but the parse keeps
/// alternative support, so it must be re-derived rather than deleted. Returns
/// whether such a key existed.
fn retract_one_of_many(store: &EntStore, world: &mut World, log: &mut Log, rng: &mut Rng) -> bool {
    let mut support: HashMap<i64, Diff> = HashMap::new();
    for (t, d) in world.iter().filter(|(_, d)| **d > 0) {
        *support.entry(key_of(t)).or_insert(0) += *d;
    }
    let mut keys: Vec<i64> = support.iter().filter(|(_, n)| **n >= 2).map(|(k, _)| *k).collect();
    keys.sort();
    if keys.is_empty() {
        return false;
    }
    let k = keys[rng.below(keys.len() as u64) as usize];
    let mut rows: Vec<Tuple> =
        world.iter().filter(|(t, d)| **d > 0 && key_of(t) == k).map(|(t, _)| t.clone()).collect();
    rows.sort();
    let t = rows[rng.below(rows.len() as u64) as usize].clone();
    commit_batch(store, world, log, vec![(t, -1)]);
    true
}

// ---------------------------------------------------------------------------
// The oracle's independent model of a window's state-mode contents
// ---------------------------------------------------------------------------

/// The tuples the window `(from, to]` touched, with their weight **at `to`** —
/// the snapshot-anchored net contents of §4a, recomputed from the generator's
/// own log rather than from `consolidate_window`. Rows with a non-positive
/// weight at `to` are absent (retracted), which is exactly the anchoring TKT-114
/// pinned.
fn expected_contents(log: &Log, w: Window) -> Vec<(Tuple, Diff)> {
    let mut touched: HashSet<Tuple> = HashSet::new();
    for (at, batch) in log {
        if w.contains(*at) {
            touched.extend(batch.iter().map(|(t, _)| t.clone()));
        }
    }
    let mut at_to: World = World::new();
    for (at, batch) in log {
        if *at <= w.to() {
            for (t, d) in batch {
                apply(&mut at_to, t, *d);
            }
        }
    }
    let mut rows: Vec<(Tuple, Diff)> = touched
        .into_iter()
        .filter_map(|t| {
            let d = weight(&at_to, &t);
            (d > 0).then_some((t, d))
        })
        .collect();
    rows.sort();
    rows
}

/// How many derivations each key has in the window: one per *occurrence* of a
/// row with that key in the window's contents, so a row at multiplicity two
/// supports its parse twice. This is the support relation the trap law is
/// stated over.
fn expected_support(log: &Log, w: Window) -> HashMap<i64, usize> {
    let mut sup: HashMap<i64, usize> = HashMap::new();
    for (t, d) in expected_contents(log, w) {
        *sup.entry(key_of(&t)).or_insert(0) += d as usize;
    }
    sup
}

// ---------------------------------------------------------------------------
// Grammars and the two matching modes (the `WindowParser` instances)
// ---------------------------------------------------------------------------

fn key_parse(k: i64) -> Value {
    Value::Tuple(std::sync::Arc::from([Value::text("key"), Value::Int(k)]))
}

/// The **trap grammar**: one row derives the parse `("key", k)`. Its support
/// relation is analytically known — *every* present row with key `k` derives it
/// — which is what lets the oracle classify each retraction as "last support
/// gone" or "support remains" and assert the right outcome.
fn key_form() -> Form {
    Form::new(vec![Rule::new(Pattern::Bind(VarId(0)), |b: &Bindings| match b.get(&VarId(0)) {
        Some(Value::Tuple(cells)) => key_parse(match cells[0] {
            Value::Int(k) => k,
            _ => -1,
        }),
        _ => key_parse(-1),
    })])
}

/// A richer state-mode grammar: the trap rule plus a position-sensitive
/// adjacent-pair rule, so the match-set is ambiguous (several rules match at one
/// position) and churn in the middle of the window reshuffles parses rather than
/// only adding and removing at the ends.
fn state_form() -> Form {
    let mut rules = key_form().rules;
    rules.push(Rule::new(
        Pattern::Seq(vec![Pattern::Bind(VarId(0)), Pattern::Bind(VarId(1))]),
        |b: &Bindings| {
            let cell = |var: VarId| match b.get(&var) {
                Some(Value::Tuple(cells)) => cells[0].clone(),
                _ => Value::Int(-1),
            };
            Value::Tuple(std::sync::Arc::from([
                Value::text("pair"),
                cell(VarId(0)),
                cell(VarId(1)),
            ]))
        },
    ));
    Form::new(rules)
}

/// The **event-mode** grammar over reified deltas `[sign, tuple, edition]`: a
/// retraction event, and the delete-after-insert CEP pair. Both have alternative
/// support (a tuple may be retracted more than once in a window), so the set
/// collapse is exercised on this side too.
fn event_form() -> Form {
    let sign_is = |var: VarId, want: i64| -> grmpl_pattern::Guard {
        std::sync::Arc::new(move |b: &Bindings| {
            matches!(b.get(&var), Some(Value::Tuple(cells)) if cells[0] == Value::Int(want))
        })
    };
    let payload = |b: &Bindings, var: VarId| match b.get(&var) {
        Some(Value::Tuple(cells)) => cells[1].clone(),
        _ => Value::Int(-1),
    };
    Form::new(vec![
        Rule::new(
            Pattern::Guard(Box::new(Pattern::Bind(VarId(0))), sign_is(VarId(0), -1)),
            move |b: &Bindings| {
                Value::Tuple(std::sync::Arc::from([Value::text("retract"), payload(b, VarId(0))]))
            },
        ),
        Rule::new(
            Pattern::Seq(vec![
                Pattern::Guard(Box::new(Pattern::Bind(VarId(0))), sign_is(VarId(0), 1)),
                Pattern::Guard(Box::new(Pattern::Bind(VarId(1))), sign_is(VarId(1), -1)),
            ]),
            move |b: &Bindings| {
                Value::Tuple(std::sync::Arc::from([
                    Value::text("churn"),
                    payload(b, VarId(0)),
                    payload(b, VarId(1)),
                ]))
            },
        ),
    ])
}

/// Run a form at **every suffix** of the window: "every parse the window
/// admits", not just the one anchored at position zero. Partial parses count —
/// `parse_in` returns `(value, rest)` and the rest is free — which is what makes
/// a match-set that grows and shrinks with the window's middle, not only its
/// ends.
fn scan<I: grmpl_pattern::MatchInput>(form: &Form, at: impl Fn(usize) -> I, len: usize) -> Vec<Value> {
    (0..len).flat_map(|i| form.parse_in(at(i)).into_iter().map(|(v, _)| v)).collect()
}

/// **State mode** (§4a): the window's snapshot-anchored net contents, matched by
/// the existing `&[Value]` slice engine.
struct StateMode {
    q: Query,
    form: Form,
}

impl WindowParser for StateMode {
    fn parses(&self, w: Window, store: &dyn TraceStore) -> Result<Vec<Value>> {
        let values = w.consolidate(&self.q, store)?.values();
        Ok(scan(&self.form, |i| &values[i..], values.len()))
    }
}

/// **Event mode** (§4b): the window's raw signed updates in commit order,
/// matched through `DeltaInput`, which reifies each sign into the matched value.
struct EventMode {
    rel: RelId,
    form: Form,
}

impl WindowParser for EventMode {
    fn parses(&self, w: Window, store: &dyn TraceStore) -> Result<Vec<Value>> {
        let ups = w.events(store, self.rel)?;
        Ok(scan(&self.form, |i| DeltaInput::over(&ups[i..]), ups.len()))
    }
}

fn state_mode(form: Form) -> StateMode {
    StateMode { q: Query::rel(REL), form }
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// A parse set as the oracle compares it: ascending, deduplicated.
fn as_set(parses: Vec<Value>) -> Vec<Value> {
    let mut v: Vec<Value> = parses.into_iter().collect::<HashSet<_>>().into_iter().collect();
    v.sort();
    v
}

/// The shape every emitted delta must have regardless of `HashMap` fold order:
/// strictly ascending by `Value`, zero-free, weights only ±1 (parses are a set).
fn assert_delta_shape(delta: &[(Value, Diff)], what: &str) {
    assert!(
        delta.windows(2).all(|p| p[0].0 < p[1].0),
        "{what}: parse delta is not strictly ascending — a HashMap order leaked: {delta:?}"
    );
    for (p, d) in delta {
        assert!(
            *d == 1 || *d == -1,
            "{what}: parse delta weight {d} for {p:?} is not ±1 (parses are a set, not a multiset)"
        );
    }
}

fn fold(acc: &mut HashMap<Value, Diff>, delta: &[(Value, Diff)]) {
    for (p, d) in delta {
        *acc.entry(p.clone()).or_insert(0) += d;
    }
    acc.retain(|_, d| *d != 0);
}

fn acc_set(acc: &HashMap<Value, Diff>) -> Vec<Value> {
    let mut v: Vec<Value> = acc
        .iter()
        .map(|(p, d)| {
            assert_eq!(*d, 1, "accumulated parse {p:?} has weight {d}, not 1 — parses are a set");
            p.clone()
        })
        .collect();
    v.sort();
    v
}

/// The first delivery: `M(W₀)` as positive rows, **without** moving the cursor —
/// the `DeltaStream` idiom, so the registered window really is `W₀` and no
/// interval is skipped. Returns the accumulator seeded with it.
fn open_batch<P: WindowParser>(
    stream: &mut ParseStream<P>,
    w0: Window,
    seed: u64,
    what: &str,
) -> HashMap<Value, Diff> {
    let initial = stream.poll().unwrap();
    assert_delta_shape(&initial, &format!("seed {seed} {what} initial"));
    assert!(
        initial.iter().all(|(_, d)| *d == 1),
        "seed {seed} {what}: the initial batch must be M(W₀) as positive rows"
    );
    assert_eq!(stream.window(), w0, "seed {seed} {what}: the first poll moved the cursor off W₀");
    let mut acc = HashMap::new();
    fold(&mut acc, &initial);
    acc
}

/// One advance, fully checked: fold the emitted delta into `acc` and assert the
/// **lifted Snapshot–stream law** against a from-scratch recompute of the same
/// parser over the current window. Generic over the mode, because state mode and
/// event mode are different `WindowParser` types and the law must hold for
/// either. Returns `(removals, additions)` in this delta, for anti-vacuity.
fn check_advance<P: WindowParser>(
    stream: &mut ParseStream<P>,
    acc: &mut HashMap<Value, Diff>,
    now: Window,
    store: &dyn TraceStore,
    seed: u64,
    round: usize,
    what: &str,
) -> (usize, usize) {
    let delta = stream.poll().unwrap();
    assert_delta_shape(&delta, &format!("seed {seed} round {round} {what}"));
    fold(acc, &delta);

    // THE LAW: the accumulation equals the parser re-run from scratch over the
    // current window.
    let fresh = as_set(stream.parser().parses(now, store).unwrap());
    assert_eq!(
        acc_set(acc),
        fresh,
        "seed {seed} round {round} {what}: parse-stream law violated — \
         M(W₀) + Σ deltas != M(W_current) recomputed from scratch"
    );
    // And the maintained set itself is right, so the law cannot pass by two
    // compensating errors in the emitted deltas.
    assert_eq!(
        stream.matches(),
        fresh,
        "seed {seed} round {round} {what}: the maintained match-set != a fresh recompute"
    );
    assert_eq!(
        stream.window(),
        now,
        "seed {seed} round {round} {what}: the stream's window is not (from, current]"
    );
    (
        delta.iter().filter(|(_, d)| *d < 0).count(),
        delta.iter().filter(|(_, d)| *d > 0).count(),
    )
}

// ---------------------------------------------------------------------------
// Law 1 — the lifted Snapshot–stream law
// ---------------------------------------------------------------------------

/// `M(W₀) + Σ deltas = M(W_current)`, re-checked after **every** advance, in
/// both matching modes, under random insert/retract churn — the M2
/// Snapshot–stream oracle lifted from tuples to parses (design §6).
#[test]
fn parse_stream_law_maintained_parses_equal_a_from_scratch_recompute() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed);
        let mut world = World::new();
        let mut log = Log::new();

        // A prefix of history, so `from` can sit strictly above ⊥ and the
        // window's anchor is genuinely load-bearing (a retraction inside the
        // window of a row asserted before it).
        for _ in 0..3 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 3);
        }
        let from = if rng.below(2) == 0 { Edition::ZERO } else { log[rng.below(3) as usize].0 };
        let w0 = window(from, store.current()).unwrap();

        let mut state_stream = ParseStream::open(state_mode(state_form()), &store, w0);
        let mut event_stream =
            ParseStream::open(EventMode { rel: REL, form: event_form() }, &store, w0);

        let mut state_acc = open_batch(&mut state_stream, w0, seed, "state");
        let mut event_acc = open_batch(&mut event_stream, w0, seed, "event");

        // Anti-vacuity: the churn must actually retract parses out of the
        // state-mode match-set, and must actually add some.
        let mut removals = 0usize;
        let mut additions = 0usize;

        for round in 0..ROUNDS {
            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 3);
            }
            let now = window(from, store.current()).unwrap();

            let (down, up) =
                check_advance(&mut state_stream, &mut state_acc, now, &store, seed, round, "state");
            removals += down;
            additions += up;
            check_advance(&mut event_stream, &mut event_acc, now, &store, seed, round, "event");
        }

        assert!(
            removals > 0,
            "seed {seed}: no parse was ever retracted — the retraction path went untested"
        );
        assert!(additions > 0, "seed {seed}: no parse was ever added");
    }
}

// ---------------------------------------------------------------------------
// Law 2 — the Delete–Rederive trap
// ---------------------------------------------------------------------------

/// The reason this is the hard piece: a retraction destroys *a* derivation, not
/// necessarily *the* parse. Asserts both halves of the trap against a support
/// relation the oracle computes from its own world model — a parse that lost its
/// last support disappears, a parse that kept alternative support survives — and
/// proves the trap is live by running a naive over-deleting maintainer as a
/// negative control (design §6; the DRed reading in `parse_stream.rs`).
#[test]
fn retraction_drops_unsupported_parses_and_rederives_supported_ones() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0xA5A5_A5A5);
        let mut world = World::new();
        let mut log = Log::new();

        // Seed a world dense enough that keys have several supporting rows.
        for _ in 0..4 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 0);
        }
        let from = Edition::ZERO;
        let mut w = window(from, store.current()).unwrap();

        let mut stream = ParseStream::open(state_mode(key_form()), &store, w);
        stream.poll().unwrap();

        // Counters proving neither half of the trap went untested, and that the
        // naive maintainer really would have got it wrong.
        let mut case_a = 0usize; // last support gone → parse must disappear
        let mut case_b = 0usize; // a derivation lost, support remains → must survive
        let mut overdeleted = 0usize; // parses the naive control wrongly dropped

        for round in 0..ROUNDS {
            let before = w;
            let support_before = expected_support(&log, before);
            let truth_before: HashSet<Value> =
                support_before.keys().map(|k| key_parse(*k)).collect();
            let first_new = log.len();

            // Random churn, plus a targeted move at one of the two trap cases so
            // neither half is left to luck. Which move (and whether it lands at
            // all) is still random, and the counters below prove both fired.
            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 4);
            }
            match rng.below(3) {
                0 => {
                    retract_whole_key(&store, &mut world, &mut log, rng.below(KEYS));
                }
                1 => {
                    retract_one_of_many(&store, &mut world, &mut log, &mut rng);
                }
                _ => {}
            }
            w = window(from, store.current()).unwrap();
            let support_after = expected_support(&log, w);
            let truth_after: HashSet<Value> = support_after.keys().map(|k| key_parse(*k)).collect();

            let delta = stream.poll().unwrap();
            assert_delta_shape(&delta, &format!("seed {seed} round {round}"));
            let dropped: HashSet<Value> =
                delta.iter().filter(|(_, d)| *d < 0).map(|(p, _)| p.clone()).collect();
            let added: HashSet<Value> =
                delta.iter().filter(|(_, d)| *d > 0).map(|(p, _)| p.clone()).collect();
            let live: HashSet<Value> = stream.matches().into_iter().collect();

            // The rows this advance retracted, and the keys they supported.
            let mut keys_hit_by_retraction: HashSet<i64> = HashSet::new();
            let mut asserted_keys: HashSet<i64> = HashSet::new();
            for (_, batch) in &log[first_new..] {
                for (t, d) in batch {
                    if *d < 0 {
                        keys_hit_by_retraction.insert(key_of(t));
                    } else {
                        asserted_keys.insert(key_of(t));
                    }
                }
            }

            for k in 0..KEYS as i64 {
                let had = support_before.get(&k).copied().unwrap_or(0) > 0;
                let has = support_after.get(&k).copied().unwrap_or(0) > 0;
                let parse = key_parse(k);
                let hit = keys_hit_by_retraction.contains(&k);

                match (had, has) {
                    // (a) The trap's first half: the last derivation is gone, so
                    // the parse must genuinely disappear. No stale parse.
                    (true, false) => {
                        assert!(
                            dropped.contains(&parse),
                            "seed {seed} round {round}: key {k} lost its last support but the \
                             delta did not retract its parse — a stale parse survived"
                        );
                        assert!(
                            !live.contains(&parse),
                            "seed {seed} round {round}: key {k} lost its last support but its \
                             parse is still in the maintained set"
                        );
                        if hit {
                            case_a += 1;
                        }
                    }
                    // (b) The trap's second half: a derivation was destroyed but
                    // another remains, so the parse must be re-derived and stay.
                    // A naive delete would have removed it here.
                    (true, true) => {
                        assert!(
                            !dropped.contains(&parse),
                            "seed {seed} round {round}: key {k} still has \
                             {} supporting occurrence(s) but its parse was retracted — \
                             over-deletion (the DRed trap)",
                            support_after[&k]
                        );
                        assert!(
                            live.contains(&parse),
                            "seed {seed} round {round}: key {k} still has support but its parse \
                             is missing from the maintained set — a live parse was wrongly dropped"
                        );
                        // Count it only when the key genuinely *lost* support
                        // and kept some — a retraction cancelled by a re-assert
                        // in the same advance is not the trap.
                        if hit && support_after[&k] < support_before[&k] {
                            case_b += 1;
                        }
                    }
                    (false, true) => assert!(
                        added.contains(&parse),
                        "seed {seed} round {round}: key {k} gained support but its parse was \
                         never emitted"
                    ),
                    (false, false) => {
                        assert!(
                            !dropped.contains(&parse) && !added.contains(&parse),
                            "seed {seed} round {round}: key {k} was never supported yet its \
                             parse appears in the delta"
                        );
                        assert!(!live.contains(&parse));
                    }
                }
            }

            // The maintained set is exactly what the support model says.
            assert_eq!(
                stream.matches(),
                {
                    let mut v: Vec<Value> = truth_after.iter().cloned().collect();
                    v.sort();
                    v
                },
                "seed {seed} round {round}: maintained parses != the analytic support model"
            );

            // NEGATIVE CONTROL — the naive maintainer this ticket exists to
            // rule out: overdelete every parse whose row was retracted, add the
            // parses of newly asserted rows, never re-derive from surviving
            // support. Recomputed from `truth_before` each round so it measures
            // exactly this advance's over-deletion.
            let mut naive = truth_before.clone();
            for k in &keys_hit_by_retraction {
                naive.remove(&key_parse(*k));
            }
            for k in &asserted_keys {
                naive.insert(key_parse(*k));
            }
            overdeleted += truth_after.difference(&naive).count();
        }

        assert!(
            case_a > 0,
            "seed {seed}: no retraction ever removed a key's last support — the \
             delete half of the trap went untested"
        );
        assert!(
            case_b > 0,
            "seed {seed}: no retraction ever left alternative support — the re-derive half of \
             the trap went untested"
        );
        assert!(
            overdeleted > 0,
            "seed {seed}: the naive over-deleting maintainer never diverged from truth, so the \
             DRed trap never fired and this law is vacuous"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 3 — determinism and path independence
// ---------------------------------------------------------------------------

/// The emitted delta sequence is a deterministic function of the trace, and the
/// maintained match-set is a function of the *window*, not of the path taken to
/// it: a stream advanced in one jump lands exactly where one advanced step by
/// step does.
#[test]
fn parse_deltas_are_deterministic_and_path_independent() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x5EED_5EED);
        let mut world = World::new();
        let mut log = Log::new();

        for _ in 0..3 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 3);
        }
        let from = log[0].0;
        let w0 = window(from, store.current()).unwrap();

        // Build the whole trace first, recording the editions to advance
        // through, so all three streams see one fixed history.
        let mut stops = Vec::new();
        for _ in 0..ROUNDS {
            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 3);
            }
            stops.push(store.current());
        }

        let replay = |stops: &[Edition]| -> Vec<Vec<(Value, Diff)>> {
            let mut s = ParseStream::open(state_mode(state_form()), &store, w0);
            let mut out = vec![s.poll().unwrap()];
            for e in stops {
                out.push(s.advance_to(*e).unwrap());
            }
            out
        };

        let once = replay(&stops);
        let twice = replay(&stops);
        assert_eq!(
            once, twice,
            "seed {seed}: replaying the same trace produced a different delta sequence — \
             a HashMap fold order or other nondeterminism leaked"
        );
        assert!(
            once.iter().any(|d| d.iter().any(|(_, w)| *w < 0)),
            "seed {seed}: no parse was ever retracted — determinism was checked on a \
             monotone run only"
        );

        // Path independence: one jump straight to the final edition reaches the
        // same match-set as every intermediate step.
        let stepwise = {
            let mut s = ParseStream::open(state_mode(state_form()), &store, w0);
            s.poll().unwrap();
            for e in &stops {
                s.advance_to(*e).unwrap();
            }
            s.matches()
        };
        let jumped = {
            let mut s = ParseStream::open(state_mode(state_form()), &store, w0);
            s.poll().unwrap();
            s.advance_to(*stops.last().unwrap()).unwrap();
            s.matches()
        };
        assert_eq!(
            stepwise, jumped,
            "seed {seed}: M depends on the path taken to the window, not just the window"
        );
    }
}

// ---------------------------------------------------------------------------
// Witnesses — the two trap cases spelled out, and the advancing-window rule
// ---------------------------------------------------------------------------

/// A hand-built witness of the trap, readable without running the generator:
/// two rows share key `7`; retracting one keeps the parse (re-derived from the
/// other), retracting the second finally removes it.
#[test]
fn witness_alternative_support_keeps_a_parse_alive_across_a_retraction() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let a = tup(7, 1);
    let b = tup(7, 2);

    store.commit(&[(REL, a.clone(), 1), (REL, b.clone(), 1)]).unwrap();
    let w0 = window(Edition::ZERO, store.current()).unwrap();
    let mut stream = ParseStream::open(state_mode(key_form()), &store, w0);
    assert_eq!(stream.poll().unwrap(), vec![(key_parse(7), 1)], "one parse, two derivations");

    // Retract one of the two supporting rows: the parse keeps a derivation, so
    // it must be re-derived — the delta is empty. A naive maintainer that
    // deleted the parses of the retracted row would emit (key 7, -1) here.
    store.commit(&[(REL, a, -1)]).unwrap();
    assert_eq!(
        stream.poll().unwrap(),
        vec![],
        "the parse still has alternative support — it must not be retracted"
    );
    assert_eq!(stream.matches(), vec![key_parse(7)]);

    // Retract the last one: now the parse genuinely has no support left.
    store.commit(&[(REL, b, -1)]).unwrap();
    assert_eq!(
        stream.poll().unwrap(),
        vec![(key_parse(7), -1)],
        "the last derivation is gone — the parse must disappear"
    );
    assert!(stream.matches().is_empty(), "no stale parse survives");
}

/// The law is stated over a monotonically advancing window sequence, so a
/// retreating far end is an ill-formed plan, not an empty delta.
#[test]
fn witness_the_window_may_not_retreat() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    store.commit(&[(REL, tup(1, 1), 1)]).unwrap();
    let early = store.current();
    store.commit(&[(REL, tup(2, 1), 1)]).unwrap();

    let mut stream = ParseStream::open(state_mode(key_form()), &store, window(Edition::ZERO, early).unwrap());
    stream.poll().unwrap();
    stream.advance_to(store.current()).unwrap();

    assert!(
        matches!(stream.advance_to(early), Err(grmpl_core::Error::Query(_))),
        "advancing backwards must be rejected: W₀ ⊆ W₁ ⊆ … is what the law is stated over"
    );
    // Re-advancing to the same edition is a no-op, not an error.
    assert_eq!(stream.advance_to(store.current()).unwrap(), vec![]);
}
