//! **Differential-parse law oracle** (TKT-117; P9c design §6 "honest tradeoff",
//! §8.5 #5) — the acceptance test for match maintenance by *shared arrangements*.
//!
//! TKT-116 satisfied the lifted Snapshot–stream law the cheap way: re-materialize
//! the window from its anchor and re-parse the whole thing per advance
//! (`ParseStream`). The design flagged that honestly as recompute-not-sharing and
//! deferred the real thing. `DiffParseStream` is the real thing, and this file is
//! its oracle, in the shape the repo convention requires (TKT-112): **seeded
//! xorshift randomized churn**, invariant re-checked on *every* delta step, seed
//! printed on every assertion, no `rand`/`proptest` dependency, examples kept only
//! as witnesses.
//!
//! The central law is an *equivalence*, which is a stronger acceptance test than
//! restating `M(W₀) + Σ deltas = M(W_current)` on its own would be:
//!
//! 1. **Differential ≡ recompute, step for step.** Running a `DiffParseStream`
//!    and a TKT-116 `ParseStream` in lockstep over one trace, the two must emit
//!    the **identical** delta at every advance and hold the identical match-set
//!    after it — and both must equal a from-scratch re-parse of the current
//!    window. Checked in **both** modes (state and event), because the law is
//!    stated "in a fixed mode" and must hold for either. The differential
//!    maintainer is therefore a drop-in acceleration of the same semantics, not a
//!    second one.
//!
//! 2. **Retraction / Delete–Rederive, hard.** The trap TKT-116 identified does
//!    not get easier when the recompute is replaced by an arrangement — it gets
//!    *harder*, because a stale arrangement entry would resurrect a parse whose
//!    support is gone. The oracle asserts both halves against a support relation
//!    it computes from its **own world model**: last support gone ⇒ the parse
//!    disappears; support remains ⇒ the parse survives. A **naive-delete negative
//!    control** runs alongside and a per-seed counter asserts it really did
//!    over-delete, so case (b) cannot pass vacuously.
//!
//! 3. **The sharing is real.** An accelerator that quietly re-parses everything
//!    would pass laws 1 and 2 perfectly. So the oracle also asserts the
//!    instrumentation: across a run, arrangement **hits strictly outnumber
//!    parses**, and after the opening window an advance re-parses only a
//!    grammar-bounded handful of anchors rather than all of them.
//!
//! 4. **The reach contract is sound.** The whole reuse argument rests on
//!    `SequenceParser::parses_at` reporting a reach beyond which it did not look
//!    (see `differential.rs`). That is *verified*, not trusted: for random
//!    sequences and anchors, when the parser reports `reach < len` the oracle
//!    mutates and extends everything from `reach` onward and asserts the parses
//!    are unchanged.
//!
//! 5. **Determinism.** Emitted deltas are strictly ascending by `Value`,
//!    zero-free, weighted ±1, and a replay of the identical trace yields the
//!    identical delta sequence — no `HashMap` fold order leaks.
//!
//! `grmpl-pattern` remains a **dev-dependency only**: the maintenance layer never
//! names the algebra (the `SequenceParser` is supplied here, in the test), and
//! `grmpl-diff` reaches the trace solely through the `TraceStore` trait.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value};
use grmpl_diff::{
    window, AnchoredParses, DiffParseStream, EventSequence, ParseStream, Query, SequenceParser,
    StateSequence, Window, WindowParser, WindowSequence,
};
use grmpl_pattern::{
    reify_delta, Bindings, DeltaInput, Form, MatchInput, Pattern, Rule, VarId,
};
use grmpl_store::FjallStore;

const REL: RelId = RelId(1);
const SEEDS: std::ops::Range<u64> = 1..25; // 24 seeds
const ROUNDS: usize = 12;

/// Keys collide often enough that a parse has *alternative* support (which is
/// what makes re-derivation load-bearing) yet rarely enough that a key can lose
/// its **last** support. Both halves are asserted to have fired, per seed.
const KEYS: u64 = 3;
const PAYLOADS: u64 = 2;

// ---------------------------------------------------------------------------
// PRNG
// ---------------------------------------------------------------------------

/// Deterministic, seedable PRNG (xorshift64*) — the repo idiom, reproducible with
/// no external dependency.
///
/// The seeding is **SplitMix64 finalisation with the nonzero guard applied after
/// mixing**. The older repo spelling, `Rng(seed ^ K | 1)`, parses as
/// `(seed ^ K) | 1`: the guard against xorshift's all-zero fixed point also
/// destroys bit 0 as a degree of freedom, so seeds `2n` and `2n+1` collapse to
/// the same state and produce byte-identical streams — an N-seed header claims
/// about N/2 distinct traces (TKT-141, which sweeps the existing oracles). New
/// oracles are written the fixed way so the sweep converges rather than grows.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Guard the fixed point *after* mixing, so no seed bit is spent on it.
        Rng(if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z })
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

// ---------------------------------------------------------------------------
// World model and churn generator (the oracle's independent record of truth)
// ---------------------------------------------------------------------------

fn tup(k: u64, p: u64) -> Tuple {
    Tuple::from([Value::Int(k as i64), Value::Ent(Entity(p))])
}

fn key_of(t: &Tuple) -> i64 {
    match &t.0[0] {
        Value::Int(k) => *k,
        v => panic!("row key cell is not an Int: {v:?}"),
    }
}

type World = HashMap<Tuple, Diff>;
type Log = Vec<(Edition, Vec<(Tuple, Diff)>)>;

fn apply(world: &mut World, t: &Tuple, d: Diff) {
    *world.entry(t.clone()).or_insert(0) += d;
}

fn weight(world: &World, t: &Tuple) -> Diff {
    world.get(t).copied().unwrap_or(0)
}

/// Commit one edition of 1–3 random churn updates over a small overlapping
/// domain. `retract_bias` in `0..=4` is the chance in four of retracting a row
/// the world actually holds. Magnitudes reach 2 sometimes, so a second derivation
/// from the *same* row is exercised. Retractions never drive a weight below zero.
fn churn_edition(
    store: &FjallStore,
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

fn commit_batch(store: &FjallStore, world: &mut World, log: &mut Log, batch: Vec<(Tuple, Diff)>) {
    if batch.is_empty() {
        return;
    }
    let rows: Vec<(RelId, Tuple, Diff)> = batch.iter().map(|(t, d)| (REL, t.clone(), *d)).collect();
    let at = store.commit(&rows).unwrap();
    for (t, d) in &batch {
        apply(world, t, *d);
    }
    log.push((at, batch));
}

/// Retract **every** present occurrence of one key in one edition — the targeted
/// way to drive the trap's delete half: the key's last derivation is destroyed,
/// so its parse must genuinely disappear.
fn retract_whole_key(store: &FjallStore, world: &mut World, log: &mut Log, k: u64) {
    let batch: Vec<(Tuple, Diff)> = world
        .iter()
        .filter(|(t, d)| **d > 0 && key_of(t) == k as i64)
        .map(|(t, d)| (t.clone(), -*d))
        .collect();
    commit_batch(store, world, log, batch);
}

/// Retract **one** occurrence of a key that has at least two — the targeted way
/// to drive the trap's re-derive half: a derivation is destroyed but the parse
/// keeps alternative support, so it must be re-derived rather than deleted.
fn retract_one_of_many(store: &FjallStore, world: &mut World, log: &mut Log, rng: &mut Rng) {
    let mut support: HashMap<i64, Diff> = HashMap::new();
    for (t, d) in world.iter().filter(|(_, d)| **d > 0) {
        *support.entry(key_of(t)).or_insert(0) += *d;
    }
    let mut keys: Vec<i64> = support.iter().filter(|(_, n)| **n >= 2).map(|(k, _)| *k).collect();
    keys.sort();
    if keys.is_empty() {
        return;
    }
    let k = keys[rng.below(keys.len() as u64) as usize];
    let mut rows: Vec<Tuple> =
        world.iter().filter(|(t, d)| **d > 0 && key_of(t) == k).map(|(t, _)| t.clone()).collect();
    rows.sort();
    let t = rows[rng.below(rows.len() as u64) as usize].clone();
    commit_batch(store, world, log, vec![(t, -1)]);
}

/// The tuples the window `(from, to]` touched, at their weight **at `to`** — the
/// snapshot-anchored net contents of §4a, recomputed from the generator's own log
/// rather than from any `grmpl-diff` code.
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

/// How many derivations each key has in the window — one per *occurrence*, so a
/// row at multiplicity two supports its parse twice. The support relation the
/// trap law is stated over.
fn expected_support(log: &Log, w: Window) -> HashMap<i64, usize> {
    let mut sup: HashMap<i64, usize> = HashMap::new();
    for (t, d) in expected_contents(log, w) {
        *sup.entry(key_of(&t)).or_insert(0) += d as usize;
    }
    sup
}

// ---------------------------------------------------------------------------
// The reach-tracking cursor and the `SequenceParser` built on it
// ---------------------------------------------------------------------------

/// A `MatchInput` that records how deep the engine actually looked.
///
/// The reach is the deepest position the **cursor** was driven to: `next()` at
/// position `p` reaches `p + 1` when an item is there and `p` when it runs off
/// the end (observing that the input stops there). `measure()` observes only its
/// own position, which is sound for this engine because `Pattern::Repeat` — the
/// engine's only measure consumer — *compares* two measures rather than reading
/// either magnitude (`rest2.measure() >= rest.measure()`), and the difference of
/// two measures is a property of the span between them, not of the tail.
///
/// That reasoning is the soundness argument for the whole match arrangement, so
/// it is not left as reasoning: `parses_at_never_depends_on_anything_past_reach`
/// verifies it empirically against random sequences.
#[derive(Clone)]
struct Reach<'a, T> {
    seq: &'a [T],
    pos: usize,
    lift: fn(&T) -> Value,
    deepest: Rc<Cell<usize>>,
}

impl<'a, T> Reach<'a, T> {
    /// A cursor at `anchor`, plus the cell that will hold its reach.
    fn at(seq: &'a [T], anchor: usize, lift: fn(&T) -> Value) -> (Reach<'a, T>, Rc<Cell<usize>>) {
        let deepest = Rc::new(Cell::new(anchor));
        (Reach { seq, pos: anchor, lift, deepest: Rc::clone(&deepest) }, deepest)
    }

    fn observe(&self, p: usize) {
        if p > self.deepest.get() {
            self.deepest.set(p);
        }
    }
}

impl<T: Clone> MatchInput for Reach<'_, T> {
    fn next(&self) -> Option<(Value, Self)> {
        match self.seq.get(self.pos) {
            Some(item) => {
                self.observe(self.pos + 1);
                Some(((self.lift)(item), Reach { pos: self.pos + 1, ..self.clone() }))
            }
            None => {
                self.observe(self.pos);
                None
            }
        }
    }

    fn measure(&self) -> usize {
        self.observe(self.pos);
        self.seq.len() - self.pos
    }
}

/// A `Form` as a `SequenceParser`: every parse anchored at one position, plus the
/// reach the cursor recorded. `lift` is the mode's reification — identity for
/// state mode's `Value`s, `reify_delta` for event mode's `Update`s, exactly as
/// `DeltaInput` does it.
struct FormParser<T> {
    form: Form,
    lift: fn(&T) -> Value,
}

impl<T: Clone> SequenceParser<T> for FormParser<T> {
    fn parses_at(&self, seq: &[T], anchor: usize) -> Result<AnchoredParses> {
        let (cursor, deepest) = Reach::at(seq, anchor, self.lift);
        let parses = self.form.parse_in(cursor).into_iter().map(|(v, _)| v).collect();
        Ok(AnchoredParses { parses, reach: deepest.get() })
    }
}

// ---------------------------------------------------------------------------
// Grammars, and the TKT-116 recompute parsers to compare against
// ---------------------------------------------------------------------------

fn key_parse(k: i64) -> Value {
    Value::Tuple(std::sync::Arc::from([Value::text("key"), Value::Int(k)]))
}

/// The **trap grammar**: one row derives the parse `("key", k)`, so its support
/// relation is analytically known — *every* present row with key `k` derives it.
fn key_form() -> Form {
    Form::new(vec![Rule::new(Pattern::Bind(VarId(0)), |b: &Bindings| match b.get(&VarId(0)) {
        Some(Value::Tuple(cells)) => key_parse(match cells[0] {
            Value::Int(k) => k,
            _ => -1,
        }),
        _ => key_parse(-1),
    })])
}

/// The trap rule plus a position-sensitive adjacent-pair rule, so the match-set
/// is ambiguous and churn in the middle of the window reshuffles parses rather
/// than only adding and removing at the ends.
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
/// retraction event, and the delete-after-insert CEP pair.
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

/// Run a form at **every suffix** — the same scan the TKT-116 oracle uses, so the
/// two maintainers are compared over identically-defined match-sets.
fn scan<I: MatchInput>(form: &Form, at: impl Fn(usize) -> I, len: usize) -> Vec<Value> {
    (0..len).flat_map(|i| form.parse_in(at(i)).into_iter().map(|(v, _)| v)).collect()
}

/// The TKT-116 **recompute** parser, state mode: re-materialize the window from
/// its anchor, re-parse the whole thing.
struct StateRecompute {
    q: Query,
    form: Form,
}

impl WindowParser for StateRecompute {
    fn parses(&self, w: Window, store: &dyn TraceStore) -> Result<Vec<Value>> {
        let values = w.consolidate(&self.q, store)?.values();
        Ok(scan(&self.form, |i| &values[i..], values.len()))
    }
}

/// The TKT-116 **recompute** parser, event mode.
struct EventRecompute {
    rel: RelId,
    form: Form,
}

impl WindowParser for EventRecompute {
    fn parses(&self, w: Window, store: &dyn TraceStore) -> Result<Vec<Value>> {
        let ups = w.events(store, self.rel)?;
        Ok(scan(&self.form, |i| DeltaInput::over(&ups[i..]), ups.len()))
    }
}

fn state_diff(form: Form) -> (StateSequence, FormParser<Value>) {
    (StateSequence::new(Query::rel(REL)), FormParser { form, lift: Value::clone })
}

fn event_diff(form: Form) -> (EventSequence, FormParser<Update>) {
    (EventSequence::new(REL), FormParser { form, lift: reify_delta })
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

fn as_set(parses: Vec<Value>) -> Vec<Value> {
    let mut v: Vec<Value> = parses.into_iter().collect::<HashSet<_>>().into_iter().collect();
    v.sort();
    v
}

/// The shape every emitted delta must have regardless of `HashMap` fold order:
/// strictly ascending by `Value`, zero-free, weights only ±1.
fn assert_delta_shape(delta: &[(Value, Diff)], what: &str) {
    assert!(
        delta.windows(2).all(|p| p[0].0 < p[1].0),
        "{what}: parse delta is not strictly ascending — a HashMap order leaked: {delta:?}"
    );
    for (p, d) in delta {
        assert!(
            *d == 1 || *d == -1,
            "{what}: parse delta weight {d} for {p:?} is not ±1 (parses are a set)"
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

/// Sharing instrumentation accumulated over a run.
#[derive(Default)]
struct Sharing {
    anchors: usize,
    calls: usize,
}

// ---------------------------------------------------------------------------
// Law 1 — differential ≡ recompute, step for step, in both modes
// ---------------------------------------------------------------------------

/// The central acceptance law. A `DiffParseStream` (arrangements) and a
/// `ParseStream` (window-recompute, TKT-116) run in lockstep over one randomized
/// insert/**retract** trace must emit the *identical* delta at every advance,
/// hold the *identical* match-set after it, and both agree with a from-scratch
/// re-parse of the current window — so `M(W₀) + Σ deltas = M(W_current)` holds of
/// the differential maintainer too. Checked in state mode and event mode.
///
/// Also asserts that the sharing is real (law 3): if the arrangement never hit,
/// the "differential" maintainer would merely be the recompute one wearing a
/// different type, and this equivalence would be trivial.
#[test]
fn differential_matching_equals_window_recompute_at_every_step() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed);
        let mut world = World::new();
        let mut log = Log::new();

        // A prefix of history, so `from` can sit strictly above ⊥ and the
        // window's anchor is genuinely load-bearing.
        for _ in 0..3 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 3);
        }
        let from = if rng.below(2) == 0 { Edition::ZERO } else { log[rng.below(3) as usize].0 };
        let w0 = window(from, store.current()).unwrap();

        let (sseq, sparser) = state_diff(state_form());
        let mut state_diff_stream = DiffParseStream::open(sseq, sparser, &store, w0);
        let mut state_recompute =
            ParseStream::open(StateRecompute { q: Query::rel(REL), form: state_form() }, &store, w0);

        let (eseq, eparser) = event_diff(event_form());
        let mut event_diff_stream = DiffParseStream::open(eseq, eparser, &store, w0);
        let mut event_recompute =
            ParseStream::open(EventRecompute { rel: REL, form: event_form() }, &store, w0);

        let mut state_acc = HashMap::new();
        let mut event_acc = HashMap::new();
        let mut state_share = Sharing::default();
        let mut event_share = Sharing::default();
        let mut removals = 0usize;
        let mut additions = 0usize;

        // Opening delivery: M(W₀) as positive rows, without moving the cursor.
        for (what, d, r) in [
            ("state", state_diff_stream.poll().unwrap(), state_recompute.poll().unwrap()),
            ("event", event_diff_stream.poll().unwrap(), event_recompute.poll().unwrap()),
        ] {
            assert_delta_shape(&d, &format!("seed {seed} {what} initial"));
            assert!(
                d.iter().all(|(_, w)| *w == 1),
                "seed {seed} {what}: the initial batch must be M(W₀) as positive rows"
            );
            assert_eq!(
                d, r,
                "seed {seed} {what}: the differential opening batch differs from the recompute one"
            );
        }
        assert_eq!(state_diff_stream.window(), w0, "seed {seed}: the first poll moved the cursor");
        fold(&mut state_acc, &state_diff_stream.matches().iter().map(|p| (p.clone(), 1)).collect::<Vec<_>>());
        fold(&mut event_acc, &event_diff_stream.matches().iter().map(|p| (p.clone(), 1)).collect::<Vec<_>>());
        state_share.anchors += state_diff_stream.last_anchors;
        state_share.calls += state_diff_stream.last_parse_calls;
        event_share.anchors += event_diff_stream.last_anchors;
        event_share.calls += event_diff_stream.last_parse_calls;

        for round in 0..ROUNDS {
            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 3);
            }
            // Aim at the hard case deliberately: leaving retraction of a whole
            // key to chance makes the delete half of the trap rare.
            match rng.below(4) {
                0 => retract_whole_key(&store, &mut world, &mut log, rng.below(KEYS)),
                1 => retract_one_of_many(&store, &mut world, &mut log, &mut rng),
                _ => {}
            }
            let now = window(from, store.current()).unwrap();

            // --- state mode -------------------------------------------------
            let d = state_diff_stream.poll().unwrap();
            let r = state_recompute.poll().unwrap();
            assert_delta_shape(&d, &format!("seed {seed} round {round} state"));
            assert_eq!(
                d, r,
                "seed {seed} round {round} state: the differential delta differs from the \
                 TKT-116 window-recompute delta — the accelerator changed the semantics"
            );
            fold(&mut state_acc, &d);
            let fresh = as_set(state_recompute.parser().parses(now, &store).unwrap());
            assert_eq!(
                acc_set(&state_acc),
                fresh,
                "seed {seed} round {round} state: M(W₀) + Σ deltas != M(W_current) recomputed \
                 from scratch"
            );
            assert_eq!(
                state_diff_stream.matches(),
                fresh,
                "seed {seed} round {round} state: the maintained match-set != a fresh recompute"
            );
            assert_eq!(
                state_diff_stream.matches(),
                state_recompute.matches(),
                "seed {seed} round {round} state: the two maintainers hold different match-sets"
            );
            assert_eq!(state_diff_stream.window(), now);
            removals += d.iter().filter(|(_, w)| *w < 0).count();
            additions += d.iter().filter(|(_, w)| *w > 0).count();
            state_share.anchors += state_diff_stream.last_anchors;
            state_share.calls += state_diff_stream.last_parse_calls;

            // --- event mode -------------------------------------------------
            let d = event_diff_stream.poll().unwrap();
            let r = event_recompute.poll().unwrap();
            assert_delta_shape(&d, &format!("seed {seed} round {round} event"));
            assert_eq!(
                d, r,
                "seed {seed} round {round} event: the differential delta differs from the \
                 TKT-116 window-recompute delta"
            );
            fold(&mut event_acc, &d);
            let fresh = as_set(event_recompute.parser().parses(now, &store).unwrap());
            assert_eq!(
                acc_set(&event_acc),
                fresh,
                "seed {seed} round {round} event: M(W₀) + Σ deltas != M(W_current)"
            );
            assert_eq!(
                event_diff_stream.matches(),
                fresh,
                "seed {seed} round {round} event: the maintained match-set != a fresh recompute"
            );
            event_share.anchors += event_diff_stream.last_anchors;
            event_share.calls += event_diff_stream.last_parse_calls;
        }

        assert!(
            removals > 0,
            "seed {seed}: no parse was ever retracted — the retraction path went untested"
        );
        assert!(additions > 0, "seed {seed}: no parse was ever added");

        // LAW 3 — the sharing is real. A maintainer that re-parsed every anchor
        // would satisfy everything above and be no accelerator at all. This is
        // the anti-vacuity floor; the *asymptotic* claim (per-advance work is
        // decoupled from window size) is
        // `reparses_per_advance_do_not_grow_with_the_window` below, which needs a
        // window big enough for an asymptote to be visible.
        for (what, s) in [("state", &state_share), ("event", &event_share)] {
            assert!(
                s.calls < s.anchors,
                "seed {seed} {what}: the match arrangement never shared — {} parses over {} \
                 anchors. Differential matching means an anchor is re-parsed only where its \
                 consumed span changed.",
                s.calls,
                s.anchors
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Law 3 — the sharing is real, asymptotically
// ---------------------------------------------------------------------------

/// The claim that makes this module worth having, stated as the thing it
/// actually asserts: **an advance re-parses a number of anchors bounded by what
/// changed, not by how big the window is.**
///
/// The equivalence law above is satisfied just as well by a maintainer that
/// re-parses everything, so the accelerator has to be measured separately — and
/// measured on a window large enough for an asymptote to exist. This runs a wide
/// domain (so the state-mode sequence holds ~a hundred rows and the event log
/// several hundred) under small per-advance churn, and asserts that
/// `last_parse_calls` stays flat while `last_anchors` grows. A window-recompute
/// maintainer has `parse_calls == anchors` by definition and fails immediately.
#[test]
fn reparses_per_advance_do_not_grow_with_the_window() {
    const WIDE_KEYS: u64 = 60;
    const WIDE_PAYLOADS: u64 = 3;
    // Whatever an advance costs, it is bounded by the churn (1–3 rows), the
    // grammar's two-item lookahead, and the two saturated anchors at the tail —
    // never by the window. Generous, and still an order below the anchor count.
    const BOUND: usize = 24;

    for seed in 1..17u64 {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x5A7E_1117);

        // A wide, mostly-monotone history so the window is genuinely large.
        for _ in 0..60 {
            let n = 1 + rng.below(3);
            let batch: Vec<(RelId, Tuple, Diff)> = (0..n)
                .map(|_| (REL, tup(rng.below(WIDE_KEYS), rng.below(WIDE_PAYLOADS)), 1))
                .collect();
            store.commit(&batch).unwrap();
        }
        let w0 = window(Edition::ZERO, store.current()).unwrap();

        let (sseq, sparser) = state_diff(state_form());
        let mut state = DiffParseStream::open(sseq, sparser, &store, w0);
        let (eseq, eparser) = event_diff(event_form());
        let mut event = DiffParseStream::open(eseq, eparser, &store, w0);
        state.poll().unwrap();
        event.poll().unwrap();

        let mut widest = 0usize;
        for round in 0..10 {
            // Small churn, retraction included: a handful of rows move.
            let n = 1 + rng.below(3);
            let batch: Vec<(RelId, Tuple, Diff)> = (0..n)
                .map(|_| {
                    let t = tup(rng.below(WIDE_KEYS), rng.below(WIDE_PAYLOADS));
                    (REL, t, if rng.below(3) == 0 { -1 } else { 1 })
                })
                .collect();
            store.commit(&batch).unwrap();

            // The two modes are different `WindowSequence` types, so the check
            // is a closure over the instrumentation rather than a loop.
            let check = |what: &str, calls: usize, anchors: usize| {
                assert!(
                    calls <= BOUND,
                    "seed {seed} round {round} {what}: an advance re-parsed {calls} of {anchors} \
                     anchors. Differential matching means the cost of an advance tracks what \
                     changed, not the size of the window — this is window-recompute wearing an \
                     arrangement."
                );
                anchors
            };
            state.poll().unwrap();
            widest = widest.max(check("state", state.last_parse_calls, state.last_anchors));
            event.poll().unwrap();
            widest = widest.max(check("event", event.last_parse_calls, event.last_anchors));
        }
        assert!(
            widest > 4 * BOUND,
            "seed {seed}: the window only ever reached {widest} anchors, which is not big enough \
             for the bound of {BOUND} to mean anything — this law would pass vacuously"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 2 — retraction / Delete–Rederive, stressed
// ---------------------------------------------------------------------------

/// Retraction destroys *a* derivation, not necessarily *the* parse — and an
/// arrangement makes that harder, not easier, because a stale entry would
/// resurrect a parse whose support is gone. Asserts both halves against a support
/// relation computed from the oracle's own world model, and proves the trap is
/// live with a naive over-deleting maintainer as a negative control.
#[test]
fn retraction_drops_unsupported_parses_and_rederives_supported_ones() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0xA5A5_A5A5);
        let mut world = World::new();
        let mut log = Log::new();

        for _ in 0..4 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 0);
        }
        let from = Edition::ZERO;
        let mut w = window(from, store.current()).unwrap();

        let (seq, parser) = state_diff(key_form());
        let mut stream = DiffParseStream::open(seq, parser, &store, w);
        stream.poll().unwrap();

        let mut case_a = 0usize; // last support gone → the parse must disappear
        let mut case_b = 0usize; // a derivation lost, support remains → must survive
        let mut overdeleted = 0usize; // parses the naive control wrongly dropped

        for round in 0..ROUNDS {
            let support_before = expected_support(&log, w);
            let truth_before: HashSet<Value> =
                support_before.keys().map(|k| key_parse(*k)).collect();
            let first_new = log.len();

            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 4);
            }
            match rng.below(3) {
                0 => retract_whole_key(&store, &mut world, &mut log, rng.below(KEYS)),
                1 => retract_one_of_many(&store, &mut world, &mut log, &mut rng),
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
                    (true, false) => {
                        assert!(
                            dropped.contains(&parse),
                            "seed {seed} round {round}: key {k} lost its last support but the \
                             delta did not retract its parse — a stale arrangement entry \
                             resurrected it"
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
                    (true, true) => {
                        assert!(
                            !dropped.contains(&parse),
                            "seed {seed} round {round}: key {k} still has {} supporting \
                             occurrence(s) but its parse was retracted — over-deletion (the DRed \
                             trap)",
                            support_after[&k]
                        );
                        assert!(
                            live.contains(&parse),
                            "seed {seed} round {round}: key {k} still has support but its parse \
                             is missing — a live parse was wrongly dropped"
                        );
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

            assert_eq!(
                stream.matches(),
                {
                    let mut v: Vec<Value> = truth_after.iter().cloned().collect();
                    v.sort();
                    v
                },
                "seed {seed} round {round}: maintained parses != the analytic support model"
            );

            // NEGATIVE CONTROL — the naive maintainer: overdelete every parse
            // whose row was retracted, add the parses of newly asserted rows,
            // never re-derive from surviving support.
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
            "seed {seed}: no retraction ever removed a key's last support — the delete half of \
             the trap went untested"
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
// Law 4 — the reach contract is sound
// ---------------------------------------------------------------------------

/// The entire arrangement-reuse argument rests on one claim: when the parser
/// reports `reach`, nothing at or beyond `reach` can change its answer. That
/// claim is *verified* here rather than assumed — for random sequences and random
/// anchors, everything from `reach` on is replaced with fresh random content of a
/// different length, and the parses must be identical.
///
/// A parser that under-reported its reach would pass every other law on the
/// traces those laws happen to generate and then silently serve a stale
/// arrangement entry on some other trace. This is the law that rules that out.
#[test]
fn parses_at_never_depends_on_anything_past_reach() {
    let forms: [(&str, Form); 2] = [("state", state_form()), ("key", key_form())];
    let mut checked = 0usize;
    for seed in 1..33u64 {
        let mut rng = Rng::new(seed ^ 0x5A17_C0DE);
        for (what, form) in &forms {
            let parser = FormParser { form: form.clone(), lift: Value::clone };
            for round in 0..8 {
                let len = 1 + rng.below(9) as usize;
                let seq: Vec<Value> = (0..len)
                    .map(|_| Value::Tuple(std::sync::Arc::from([Value::Int(rng.below(4) as i64)])))
                    .collect();
                let anchor = rng.below(len as u64) as usize;

                let got = parser.parses_at(&seq, anchor).unwrap();
                assert!(
                    anchor <= got.reach && got.reach <= len,
                    "seed {seed} round {round} {what}: reach {} out of bounds for anchor {anchor} \
                     of {len}",
                    got.reach
                );
                if got.reach == len {
                    continue; // saturated: the tail *is* observed, nothing to check
                }

                // Replace everything from `reach` on with different content of a
                // different length. If the reach is honest, nothing changes.
                let mut mutated: Vec<Value> = seq[..got.reach].to_vec();
                for _ in 0..(1 + rng.below(5)) {
                    mutated.push(Value::Tuple(std::sync::Arc::from([Value::Int(
                        100 + rng.below(4) as i64,
                    )])));
                }
                let after = parser.parses_at(&mutated, anchor).unwrap();
                assert_eq!(
                    as_set(got.parses.clone()),
                    as_set(after.parses),
                    "seed {seed} round {round} {what}: the parser reported reach {} but its \
                     parses at anchor {anchor} changed when the sequence beyond {} was replaced \
                     — the match arrangement's reuse would be unsound",
                    got.reach,
                    got.reach
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "every parse saturated its input — the reach contract was never actually exercised"
    );
}

// ---------------------------------------------------------------------------
// Law 5 — determinism
// ---------------------------------------------------------------------------

/// The emitted delta sequence is a deterministic function of the trace, and the
/// maintained match-set is a function of the *window* rather than of the path
/// taken to it — including when a shared arrangement is carried between streams.
#[test]
fn differential_deltas_are_deterministic_and_path_independent() {
    for seed in SEEDS {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x5EED_5EED);
        let mut world = World::new();
        let mut log = Log::new();

        for _ in 0..3 {
            churn_edition(&store, &mut world, &mut rng, &mut log, 3);
        }
        let from = log[0].0;
        let w0 = window(from, store.current()).unwrap();

        let mut stops = Vec::new();
        for _ in 0..ROUNDS {
            for _ in 0..(1 + rng.below(2)) {
                churn_edition(&store, &mut world, &mut rng, &mut log, 3);
            }
            stops.push(store.current());
        }

        let replay = || -> Vec<Vec<(Value, Diff)>> {
            let (seq, parser) = state_diff(state_form());
            let mut s = DiffParseStream::open(seq, parser, &store, w0);
            let mut out = vec![s.poll().unwrap()];
            for e in &stops {
                out.push(s.advance_to(*e).unwrap());
            }
            out
        };

        let once = replay();
        let twice = replay();
        assert_eq!(
            once, twice,
            "seed {seed}: replaying the same trace produced a different delta sequence — a \
             HashMap fold order or other nondeterminism leaked"
        );
        assert!(
            once.iter().any(|d| d.iter().any(|(_, w)| *w < 0)),
            "seed {seed}: no parse was ever retracted — determinism was checked on a monotone \
             run only"
        );

        // Path independence, and a **warm** arrangement carried over from the
        // first stream: reuse must not change the answer, only the cost.
        let (seq, parser) = state_diff(state_form());
        let mut warm = DiffParseStream::open(seq, parser, &store, w0);
        warm.poll().unwrap();
        for e in &stops {
            warm.advance_to(*e).unwrap();
        }
        let stepwise = warm.matches();
        let arrangement = warm.into_arrangement();
        assert!(!arrangement.is_empty(), "seed {seed}: the arrangement stayed empty");

        let (seq, parser) = state_diff(state_form());
        let mut jumped = DiffParseStream::open_with(seq, parser, &store, w0, arrangement);
        jumped.poll().unwrap();
        jumped.advance_to(*stops.last().unwrap()).unwrap();
        assert_eq!(
            stepwise,
            jumped.matches(),
            "seed {seed}: M depends on the path taken to the window, or a shared arrangement \
             changed the answer rather than only the cost"
        );
        assert_eq!(
            jumped.last_parse_calls, 0,
            "seed {seed}: a stream seeded with a warm arrangement over the same grammar and the \
             same sequence content still re-parsed {} of {} anchors",
            jumped.last_parse_calls, jumped.last_anchors
        );
    }
}

// ---------------------------------------------------------------------------
// Witnesses — readable statements of the two hard cases
// ---------------------------------------------------------------------------

/// The trap, hand-built: two rows share key `7`; retracting one keeps the parse
/// (re-derived from the other), retracting the second finally removes it.
#[test]
fn witness_alternative_support_keeps_a_parse_alive_across_a_retraction() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let a = tup(7, 1);
    let b = tup(7, 2);

    store.commit(&[(REL, a.clone(), 1), (REL, b.clone(), 1)]).unwrap();
    let w0 = window(Edition::ZERO, store.current()).unwrap();
    let (seq, parser) = state_diff(key_form());
    let mut stream = DiffParseStream::open(seq, parser, &store, w0);
    assert_eq!(stream.poll().unwrap(), vec![(key_parse(7), 1)], "one parse, two derivations");

    store.commit(&[(REL, a, -1)]).unwrap();
    assert_eq!(
        stream.poll().unwrap(),
        vec![],
        "the parse still has alternative support — it must not be retracted"
    );
    assert_eq!(stream.matches(), vec![key_parse(7)]);

    store.commit(&[(REL, b, -1)]).unwrap();
    assert_eq!(
        stream.poll().unwrap(),
        vec![(key_parse(7), -1)],
        "the last derivation is gone — the parse must disappear"
    );
    assert!(stream.matches().is_empty(), "no stale parse survives");
}

/// Appending to an event log re-parses only the anchors that ran to the old end:
/// the incremental-lexing invariant, stated as a number. A window-recompute
/// maintainer would re-parse all of them.
#[test]
fn witness_appending_an_event_reparses_only_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    for k in 0..12 {
        store.commit(&[(REL, tup(k, 1), 1)]).unwrap();
    }
    let w0 = window(Edition::ZERO, store.current()).unwrap();
    let (seq, parser) = event_diff(event_form());
    let mut stream = DiffParseStream::open(seq, parser, &store, w0);
    stream.poll().unwrap();
    assert_eq!(stream.last_anchors, 12, "twelve events, twelve anchors");
    assert_eq!(stream.last_parse_calls, 12, "a cold arrangement parses every anchor");

    store.commit(&[(REL, tup(99, 1), 1)]).unwrap();
    stream.poll().unwrap();
    assert_eq!(stream.last_anchors, 13, "the log grew by one");
    assert!(
        stream.last_parse_calls <= 3,
        "an append must re-parse only the anchors that ran to the old end (a grammar-bounded \
         handful), not the whole log — got {} of {}",
        stream.last_parse_calls,
        stream.last_anchors
    );
}

/// The law is stated over a monotonically advancing window sequence, so a
/// retreating far end is an ill-formed plan, not an empty delta.
#[test]
fn witness_the_window_may_not_retreat() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(REL, tup(1, 1), 1)]).unwrap();
    let early = store.current();
    store.commit(&[(REL, tup(2, 1), 1)]).unwrap();

    let (seq, parser) = state_diff(key_form());
    let mut stream = DiffParseStream::open(seq, parser, &store, window(Edition::ZERO, early).unwrap());
    stream.poll().unwrap();
    stream.advance_to(store.current()).unwrap();

    assert!(
        matches!(stream.advance_to(early), Err(grmpl_core::Error::Query(_))),
        "advancing backwards must be rejected: W₀ ⊆ W₁ ⊆ … is what the law is stated over"
    );
    assert_eq!(stream.advance_to(store.current()).unwrap(), vec![]);
}

/// The maintained state-mode sequence is exactly the snapshot-anchored contents
/// TKT-114 pinned — including the subtle case that makes incremental maintenance
/// different from accumulation: a tuple whose window changes **cancel** is not
/// touched by the window and must be absent, even though it is present in the
/// world.
#[test]
fn witness_a_cancelled_row_is_not_window_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let t = tup(4, 1);
    store.commit(&[(REL, t.clone(), 1)]).unwrap();
    let from = store.current(); // the row is in the world at the anchor

    let mut seq = StateSequence::new(Query::rel(REL));
    // Assert then retract inside the window: the net window delta is zero.
    store.commit(&[(REL, t.clone(), 1)]).unwrap();
    let mid = store.current();
    store.commit(&[(REL, t.clone(), -1)]).unwrap();
    let to = store.current();

    seq.advance(&store, from, from, mid).unwrap();
    assert_eq!(seq.rows(), vec![(t.clone(), 2)], "mid-window the row is touched, at weight 2");
    seq.advance(&store, from, mid, to).unwrap();
    assert!(
        seq.rows().is_empty(),
        "the window's changes cancelled, so the row is world content, not window content — \
         accumulating the anchor weight instead would leak it in"
    );
    assert_eq!(
        seq.rows(),
        window(from, to).unwrap().consolidate(&Query::rel(REL), &store).unwrap().rows(),
        "the maintained rows must equal the TKT-114 recompute"
    );
}
