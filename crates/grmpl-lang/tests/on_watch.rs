//! TKT-108 acceptance: the `on watch <view> { … }` grammar surface lowers to the
//! **same** reactive handler behavior as the programmatic `grmpl_proc::OnWatch`
//! install path from TKT-77.
//!
//! The load-bearing test is a **seeded randomized-churn law oracle** (the
//! `grmpl-store/tests/determinism.rs` idiom): for a view over a random
//! pre-existing world plus a random sequence of subsequent commits, the
//! grammar-installed watcher must deliver the **identical activation stream**
//! (same seqs, same signed rows, same order) as a hand-built `OnWatch` installed
//! programmatically. The two example tests below are kept only as human-readable
//! witnesses of the two initial-delivery modes.
//!
//! Laws asserted:
//!
//! * **Grammar ≡ programmatic.** `install_watch` builds and installs an `OnWatch`
//!   wired from the source; over identical churn it produces the byte-identical
//!   inbox a hand-built `OnWatch::install()` does.
//! * **Mode mapping.** `including current` ≡ `install_including_current` (the
//!   pre-existing view is delivered once); the default ≡ `install` (skip-initial:
//!   no replay of pre-existing rows). Cross-checked by the snapshot–stream law
//!   `initial + Σ(delivered) = find(view, current)`.
//! * **Shared seq counter is monotone.** The inbox seqs are `0,1,2,…` —
//!   contiguous, unique, strictly ascending.
//! * **Watch-cursor is durable.** It survives a store close/reopen, and the
//!   reopened watcher is already caught up (delivers nothing more).
//! * **Bright line.** `grmpl-lang` names only the substrate traits and core value
//!   types to do all of this — no storage/transport technology, no wire/codec
//!   change (so no `FORMAT_VERSION` bump), and the handler observes opaque
//!   `Edition`s.

use std::collections::BTreeMap;

use grmpl_core::{
    Authority, DomainId, Edition, EditionStore, Entity, NoSchemas, RelId, Scope, TraceStore, Tuple,
    Value,
};
use grmpl_diff::{eval_snapshot, multiset, Query};
use grmpl_lang::Program;
use grmpl_proc::{decode_activation, CommitOutcome, OnWatch};
use grmpl_store::FjallStore;

const REL_BASE: u32 = 10;
const WATCH: Entity = Entity(9); // cursor key
const TARGET: Entity = Entity(7); // inbox addressee

/// The watched view, written in the surface grammar. `view v() { base(a,b) yield
/// a }` lowers to `distinct(project(base, [0]))` — the exact same `Query` the
/// programmatic comparator hand-builds below.
fn source(including_current: bool) -> String {
    let opt = if including_current { "including current " } else { "" };
    format!(
        "rel base(a, b)\n\
         rel mailbox(who, seq, body)\n\
         rel wcursor(watch, edition)\n\
         rel wseq(inbox, target, n)\n\
         view v() {{ base(a, b) yield a }}\n\
         on watch v {opt}{{ inbox mailbox cursor wcursor seqs wseq }}\n"
    )
}

fn compile(including_current: bool) -> Program {
    Program::compile(&source(including_current), REL_BASE).unwrap()
}

/// The relation ids the grammar assigned, in one bundle.
struct Wiring {
    base: RelId,
    inbox: RelId,
    cursor: RelId,
    seqs: RelId,
}

fn wiring(prog: &Program) -> Wiring {
    Wiring {
        base: prog.rel_id("base").unwrap(),
        inbox: prog.rel_id("mailbox").unwrap(),
        cursor: prog.rel_id("wcursor").unwrap(),
        seqs: prog.rel_id("wseq").unwrap(),
    }
}

fn authority(w: &Wiring) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(w.inbox),
            Scope::whole(w.cursor),
            Scope::whole(w.seqs),
        ],
    )
}

/// A hand-built `OnWatch` — the programmatic TKT-77 path — over the *same*
/// relation ids and identities the grammar wired, with the view constructed
/// independently from `grmpl_diff` constructors (not from the compiled program).
fn programmatic(w: &Wiring) -> OnWatch {
    OnWatch {
        view: Query::rel(w.base).project([0]).distinct(),
        watch: WATCH,
        target: TARGET,
        inbox: w.inbox,
        cursor_rel: w.cursor,
        seqs: w.seqs,
        authority: authority(w),
    }
}

fn store_at(path: &std::path::Path) -> FjallStore {
    FjallStore::open(path).unwrap()
}

/// One world change to `base` — the "world" moving, outside the pump's authority.
fn churn(store: &FjallStore, base: RelId, a: i64, b: i64, diff: i64) {
    store
        .commit(&[(base, Tuple::from([Value::Int(a), Value::Int(b)]), diff)])
        .unwrap();
}

/// `TARGET`'s inbox activations sorted by seq: `(seq, diff, a)`. Every row must be
/// weight-1 (exactly-once witness).
fn activations(store: &FjallStore, inbox: RelId) -> Vec<(i64, i64, i64)> {
    let at = store.current();
    let mut v: Vec<(i64, i64, i64)> = Vec::new();
    for (t, d) in store.read_at(inbox, at).unwrap() {
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

/// The single live cursor edition for `WATCH`.
fn cursor(store: &FjallStore, cursor_rel: RelId) -> Option<Edition> {
    let at = store.current();
    let rows: Vec<i64> = store
        .read_at(cursor_rel, at)
        .unwrap()
        .into_iter()
        .filter(|(t, d)| *d > 0 && t.as_slice().first() == Some(&Value::Ent(WATCH)))
        .filter_map(|(t, _)| match t.as_slice().get(1) {
            Some(Value::Int(e)) => Some(*e),
            _ => None,
        })
        .collect();
    assert!(rows.len() <= 1, "watch cursor not single-valued: {rows:?}");
    rows.first().map(|e| Edition(*e as u64))
}

/// The view result at `current` as `a -> weight` — the ground-truth
/// `find(view, current)`.
fn view_now(store: &FjallStore, base: RelId) -> BTreeMap<i64, i64> {
    let q = Query::rel(base).project([0]).distinct();
    let snap = eval_snapshot(&q, store, store.current()).unwrap();
    let mut m = BTreeMap::new();
    for (t, wgt) in multiset::to_sorted_vec(&snap) {
        if let [Value::Int(a)] = t.as_slice() {
            *m.entry(*a).or_insert(0) += wgt;
        }
    }
    m
}

/// The net effect of the streamed activation deltas: `a -> Σ diff`, zeros dropped.
fn delivered_net(acts: &[(i64, i64, i64)]) -> BTreeMap<i64, i64> {
    let mut m = BTreeMap::new();
    for (_, diff, a) in acts {
        *m.entry(*a).or_insert(0) += *diff;
    }
    m.retain(|_, wgt| *wgt != 0);
    m
}

// ---------------------------------------------------------------------------
// Seeded randomized-churn law oracle
// ---------------------------------------------------------------------------

/// xorshift64* — deterministic, no external `rand` dependency.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// How the watcher is installed for a run — the two paths under comparison.
#[derive(Clone, Copy)]
enum Path {
    /// The text surface: `Program::install_watch` (lowering + mode-selected install).
    Grammar,
    /// The hand-built `OnWatch` installed with the matching TKT-77 method.
    Programmatic,
}

struct Run {
    acts: Vec<(i64, i64, i64)>,
    view_now: BTreeMap<i64, i64>,
    /// The view over the pre-existing world, captured just before install.
    initial: BTreeMap<i64, i64>,
    cursor: Option<Edition>,
}

/// Run one seeded scenario for one path: fresh store, seed a random pre-existing
/// world, install the watcher (grammar or programmatic) in `including_current`
/// mode, then interleave random churn with pumps. The rand stream is a pure
/// function of `seed`, and install issues the *same* one commit on both paths, so
/// the two paths' commit sequences stay in lockstep and are directly comparable.
fn run_path(seed: u64, including_current: bool, path: Path) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path());
    let prog = compile(including_current);
    let w = wiring(&prog);

    let mut st = seed ^ 0x9E37_79B9_7F4A_7C15;

    // Pre-existing world: a handful of random rows before the watcher exists.
    let pre = 3 + (next_rand(&mut st) % 6) as i64;
    for _ in 0..pre {
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        churn(&store, w.base, a, b, 1);
    }
    let initial = view_now(&store, w.base);

    // Install the watcher via the path under test, then confirm it committed.
    let ow = match path {
        Path::Grammar => {
            let (ow, outcome) = prog
                .install_watch(
                    "v",
                    &[],
                    WATCH,
                    TARGET,
                    authority(&w),
                    &store,
                    &NoSchemas,
                )
                .unwrap();
            assert!(matches!(outcome, CommitOutcome::Committed(_)));
            ow
        }
        Path::Programmatic => {
            let ow = programmatic(&w);
            let outcome = if including_current {
                ow.install_including_current(&store, &NoSchemas).unwrap()
            } else {
                ow.install(&store, &NoSchemas).unwrap()
            };
            assert!(matches!(outcome, CommitOutcome::Committed(_)));
            ow
        }
    };

    // Interleave random churn with pumps (sometimes several churns per pump, so
    // some pumps deliver a multi-delta batch).
    let rounds = 20 + (next_rand(&mut st) % 30) as i64;
    for _ in 0..rounds {
        let a = (next_rand(&mut st) % 4) as i64;
        let b = (next_rand(&mut st) % 3) as i64;
        let diff = if next_rand(&mut st).is_multiple_of(2) { 1 } else { -1 };
        churn(&store, w.base, a, b, diff);
        if !next_rand(&mut st).is_multiple_of(3) {
            ow.pump(&store, &NoSchemas).unwrap();
        }
    }
    // Final drain, then confirm caught up.
    ow.pump(&store, &NoSchemas).unwrap();
    assert_eq!(ow.pump(&store, &NoSchemas).unwrap(), 0, "seed {seed}: not caught up");

    Run {
        acts: activations(&store, w.inbox),
        view_now: view_now(&store, w.base),
        initial,
        cursor: cursor(&store, w.cursor),
    }
}

/// The core law across a spread of seeds and *both* initial-delivery modes.
fn oracle(including_current: bool) {
    for seed in 0..24u64 {
        let g = run_path(seed, including_current, Path::Grammar);
        let p = run_path(seed, including_current, Path::Programmatic);

        // THE LAW: the grammar surface delivers the identical activation stream
        // (same seqs, same signed rows, same order) as the programmatic install.
        assert_eq!(
            g.acts, p.acts,
            "seed {seed} (including_current={including_current}): \
             grammar-installed watcher diverged from programmatic install"
        );
        // …down to the same final cursor edition (opaque Edition, never a
        // physical seq number).
        assert_eq!(g.cursor, p.cursor, "seed {seed}: cursor editions diverged");

        // Shared seq counter is monotone: inbox seqs are 0,1,2,… (contiguous,
        // unique, strictly ascending).
        let seqs: Vec<i64> = g.acts.iter().map(|(s, ..)| *s).collect();
        let expected: Vec<i64> = (0..g.acts.len() as i64).collect();
        assert_eq!(seqs, expected, "seed {seed}: inbox seqs not contiguous/monotone");

        // Snapshot–stream law, per mode. `including current` replays the
        // pre-existing view, so the whole current view is Σ(delivered);
        // skip-initial omits it, so Σ(delivered) is exactly the post-install net.
        if including_current {
            assert_eq!(
                delivered_net(&g.acts),
                g.view_now,
                "seed {seed}: including-current Σ(delivered) != find(view, current)"
            );
        } else {
            let mut reconstructed = g.initial.clone();
            for (a, d) in delivered_net(&g.acts) {
                *reconstructed.entry(a).or_insert(0) += d;
            }
            reconstructed.retain(|_, wgt| *wgt != 0);
            assert_eq!(
                reconstructed, g.view_now,
                "seed {seed}: skip-initial initial + Σ(delivered) != find(view, current)"
            );
        }
    }
}

#[test]
fn grammar_watch_equals_programmatic_including_current() {
    oracle(true);
}

#[test]
fn grammar_watch_equals_programmatic_skip_initial() {
    oracle(false);
}

// ---------------------------------------------------------------------------
// Durability: the watch-cursor survives a store reopen
// ---------------------------------------------------------------------------

/// The grammar-installed watcher's durable cursor survives a close/reopen: the
/// reopened `OnWatch` reads back the same cursor edition and is already caught up.
#[test]
fn watch_cursor_is_durable_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let prog = compile(false);
    let w = wiring(&prog);

    let (ow, cursor_before, acts_before) = {
        let store = store_at(dir.path());
        for i in 0..5 {
            churn(&store, w.base, i, 0, 1);
        }
        let (ow, _) = prog
            .install_watch("v", &[], WATCH, TARGET, authority(&w), &store, &NoSchemas)
            .unwrap();
        for i in 5..10 {
            churn(&store, w.base, i, 0, 1);
            ow.pump(&store, &NoSchemas).unwrap();
        }
        ow.pump(&store, &NoSchemas).unwrap();
        (ow, cursor(&store, w.cursor), activations(&store, w.inbox))
    }; // store dropped — fjall persisted.

    assert!(cursor_before.is_some(), "cursor should be installed");
    assert!(!acts_before.is_empty(), "should have delivered activations");

    // Reopen at the same path: the cursor persisted, and the same handler picks
    // up exactly where it left off — no re-delivery, nothing new.
    let reopened = store_at(dir.path());
    assert_eq!(
        cursor(&reopened, w.cursor),
        cursor_before,
        "watch cursor did not survive reopen"
    );
    assert_eq!(
        activations(&reopened, w.inbox),
        acts_before,
        "inbox changed across reopen"
    );
    assert_eq!(
        ow.pump(&reopened, &NoSchemas).unwrap(),
        0,
        "reopened watcher re-delivered or was not caught up"
    );
    assert_eq!(cursor(&reopened, w.cursor), cursor_before, "cursor moved on reopen");
}

// ---------------------------------------------------------------------------
// Witnesses: the two initial-delivery modes, through the grammar
// ---------------------------------------------------------------------------

/// Skip-initial (the default `on watch v { … }`): a pre-existing world is *not*
/// replayed; only post-install changes stream.
#[test]
fn witness_default_skips_initial_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path());
    let prog = compile(false);
    let w = wiring(&prog);

    churn(&store, w.base, 1, 10, 1);
    churn(&store, w.base, 2, 20, 1);

    let (ow, _) = prog
        .install_watch("v", &[], WATCH, TARGET, authority(&w), &store, &NoSchemas)
        .unwrap();

    // First pump delivers nothing: the pre-existing snapshot is skipped.
    assert_eq!(ow.pump(&store, &NoSchemas).unwrap(), 0);
    assert!(activations(&store, w.inbox).is_empty());

    // A post-install value streams as one `+` activation.
    churn(&store, w.base, 3, 30, 1);
    assert_eq!(ow.pump(&store, &NoSchemas).unwrap(), 1);
    assert_eq!(activations(&store, w.inbox), vec![(0, 1, 3)]);

    assert_eq!(prog.watch_including_current("v"), Some(false));
}

/// `on watch v including current { … }`: the whole pre-existing view is delivered
/// once, as `+` rows, on the first pump.
#[test]
fn witness_including_current_delivers_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path());
    let prog = compile(true);
    let w = wiring(&prog);

    churn(&store, w.base, 1, 10, 1);
    churn(&store, w.base, 2, 20, 1);
    churn(&store, w.base, 2, 21, 1); // a second row for a=2; distinct still one `2`

    let (ow, _) = prog
        .install_watch("v", &[], WATCH, TARGET, authority(&w), &store, &NoSchemas)
        .unwrap();

    // First pump delivers the current view {1, 2} as two `+1` activations.
    assert_eq!(ow.pump(&store, &NoSchemas).unwrap(), 2);
    let acts = activations(&store, w.inbox);
    assert_eq!(acts, vec![(0, 1, 1), (1, 1, 2)]);
    assert_eq!(delivered_net(&acts), view_now(&store, w.base));

    assert_eq!(prog.watch_including_current("v"), Some(true));
}
