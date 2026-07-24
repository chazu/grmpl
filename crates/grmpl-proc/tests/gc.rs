//! GC policy safety (TKT-78, P6): the consolidation watermark is never advanced
//! past the **minimum durable watch cursor**, so every installed reactive
//! handler ([`OnWatch`]) stays pumpable after history is collapsed.
//!
//! The store's `consolidate` is a blunt physical instrument — it will happily
//! collapse history above a live cursor, because cursors are ordinary trace rows
//! it cannot interpret (they live above the bright line). The *policy* that
//! keeps GC safe therefore lives in `grmpl_proc::gc`:
//!
//!   * [`min_watch_cursor`] reads the least live cursor edition, and
//!   * [`consolidate_to`] clamps the requested horizon to it before consolidating.
//!
//! The laws under test:
//!
//!   * `consolidate_to` yields a watermark `≤` the minimum durable watch cursor,
//!     and `== min(target, min_cursor, current)` clamped monotonically
//!     (randomized oracle over arbitrary cursor configurations);
//!   * after a `consolidate_to`, every watch is still pumpable and delivers the
//!     correct deltas (the collapsed history was all *behind* the cursors);
//!   * consolidating *past* a cursor (bypassing the policy) trips the edition
//!     door: that watch's next pump **errors** rather than silently losing
//!     activations.

use grmpl_core::{
    Authority, DomainId, Edition, EditionStore, Entity, NoSchemas, RelId, Scope, TraceStore, Tuple,
    Value,
};
use grmpl_diff::Query;
use grmpl_proc::{consolidate_to, min_watch_cursor, OnWatch};
use grmpl_store::FjallStore;

const BASE: RelId = RelId(30);
const INBOX: RelId = RelId(31);
const CURSOR: RelId = RelId(32);
const SEQS: RelId = RelId(33);

/// A watch over `distinct(a)` of `BASE`, keyed by `watch`, delivering to `target`.
fn on_watch(watch: Entity, target: Entity) -> OnWatch {
    OnWatch {
        view: Query::rel(BASE).project([0]).distinct(),
        watch,
        target,
        inbox: INBOX,
        cursor_rel: CURSOR,
        seqs: SEQS,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(INBOX), Scope::whole(CURSOR), Scope::whole(SEQS)],
        ),
    }
}

fn open() -> (tempfile::TempDir, FjallStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    (dir, store)
}

/// Move the world: assert/retract `(a, b)` in `BASE`.
fn churn(store: &FjallStore, a: i64, b: i64, diff: i64) {
    store
        .commit(&[(BASE, Tuple::from([Value::Int(a), Value::Int(b)]), diff)])
        .unwrap();
}

#[test]
fn consolidate_to_never_passes_min_watch_cursor_and_keeps_watches_pumpable() {
    let (_dir, store) = open();
    let schemas = NoSchemas;

    // Two watches installed at different times → two cursors at different
    // editions. W1's cursor is the earlier (smaller) one.
    churn(&store, 1, 0, 1);
    churn(&store, 2, 0, 1);
    churn(&store, 3, 0, 1); // current == 3
    let w1 = on_watch(Entity(9), Entity(7));
    w1.install(&store, &schemas).unwrap(); // cursor C(w1) := 3

    churn(&store, 4, 0, 1);
    churn(&store, 5, 0, 1); // current == 5
    let w2 = on_watch(Entity(10), Entity(8));
    w2.install(&store, &schemas).unwrap(); // cursor C(w2) := 6

    // The min durable watch cursor is w1's, at edition 3 (the installs bumped
    // current past it — but the cursor rows still name the frontiers 3 and 6).
    let min = min_watch_cursor(&store, &[CURSOR]).unwrap();
    assert_eq!(min, Some(Edition(3)), "min cursor must be the earliest watch");

    // Ask to consolidate far into the future; the policy clamps to the min cursor.
    let wm = consolidate_to(&store, &[CURSOR], Edition(999)).unwrap();
    assert_eq!(wm, Edition(3), "consolidate_to must not pass the min cursor");
    assert_eq!(store.watermark(), Edition(3));

    // Both watches remain pumpable — their cursors (3 and 5) are at/above the
    // watermark (3), so `eval_delta` from them does not hit the door. New view
    // changes after installation are delivered.
    churn(&store, 6, 0, 1); // a fresh `a` value → a real view delta for both
    assert!(w1.pump(&store, &schemas).unwrap() >= 1, "w1 must deliver post-GC");
    assert!(w2.pump(&store, &schemas).unwrap() >= 1, "w2 must deliver post-GC");
}

#[test]
fn consolidating_past_a_cursor_trips_the_edition_door() {
    let (_dir, store) = open();
    let schemas = NoSchemas;

    churn(&store, 1, 0, 1);
    churn(&store, 2, 0, 1); // current == 2
    let w = on_watch(Entity(9), Entity(7));
    w.install(&store, &schemas).unwrap(); // cursor := 2
    churn(&store, 3, 0, 1); // a real view change (a new `a`) awaits the pump

    // Bypass the policy: consolidate PAST the cursor (2) to edition 3. The store
    // obliges — it cannot see the cursor — and now the cursor (2) is stranded
    // below the watermark (3).
    assert_eq!(store.consolidate(Edition(3)).unwrap(), Edition(3));

    // The pump reads from cursor 2 and scans `(2, 3]`, but the raw updates in
    // `(2, 3]`... 3 is present, yet the scan starts *from* 2 which is below the
    // watermark → the door errors loudly rather than losing the delta.
    let err = w.pump(&store, &schemas);
    assert!(err.is_err(), "pump from a stranded cursor must error at the door");

    // And the policy would have prevented it: consolidate_to clamps to the cursor.
    // (Fresh store to show the safe path in isolation.)
    let (_dir2, store2) = open();
    churn(&store2, 1, 0, 1);
    churn(&store2, 2, 0, 1);
    let w2 = on_watch(Entity(9), Entity(7));
    w2.install(&store2, &schemas).unwrap(); // cursor := 2
    churn(&store2, 3, 0, 1);
    assert_eq!(consolidate_to(&store2, &[CURSOR], Edition(999)).unwrap(), Edition(2));
    assert!(w2.pump(&store2, &schemas).is_ok(), "policy keeps the pump alive");
}

// --- Randomized policy oracle -----------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
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

/// Over arbitrary cursor configurations and requested horizons, `consolidate_to`
/// always yields a watermark that (a) never exceeds the minimum durable watch
/// cursor, (b) never exceeds `current`, and (c) is monotonic — exactly
/// `min(target, min_cursor, current)` floored at the prior watermark.
#[test]
fn consolidate_to_clamps_to_min_cursor_under_random_churn() {
    for seed in 1..=24u64 {
        let (_dir, store) = open();
        let mut rng = Rng::new(seed);

        // Advance the edition clock with plain world commits.
        let editions = 5 + rng.below(10);
        for _ in 0..editions {
            churn(&store, rng.below(6) as i64, 0, 1);
        }

        // Plant a random set of cursor rows at arbitrary editions ≤ current.
        let current = store.current().0;
        let n_cursors = 1 + rng.below(4);
        let mut min_cursor: Option<u64> = None;
        for k in 0..n_cursors {
            let e = rng.below(current + 1); // 0..=current
            store
                .commit(&[(CURSOR, Tuple::from([Value::Ent(Entity(k + 1)), Value::Int(e as i64)]), 1)])
                .unwrap();
            min_cursor = Some(min_cursor.map_or(e, |m| m.min(e)));
        }
        let current = store.current().0; // the cursor commits bumped it

        // A random requested horizon, possibly in the future.
        let target = rng.below(current + 4);
        let got = consolidate_to(&store, &[CURSOR], Edition(target)).unwrap();

        let floor = min_cursor.unwrap();
        // Fresh store each round, so the prior watermark is 0.
        let expected = target.min(floor).min(current);
        assert_eq!(
            got.0, expected,
            "seed {seed}: consolidate_to(target {target}) with min cursor {floor}, current {current}"
        );
        assert!(got.0 <= floor, "seed {seed}: watermark {} passed min cursor {floor}", got.0);
        assert_eq!(min_watch_cursor(&store, &[CURSOR]).unwrap(), Some(Edition(floor)));
    }
}
