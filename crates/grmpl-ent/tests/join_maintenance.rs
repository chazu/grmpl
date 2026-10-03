//! **Join maintenance through the Ent's indexes: a derived enfilade at work.**
//!
//! `eval_delta` maintains a join as `ΔA⋈B_to + A_to⋈ΔB − ΔA⋈ΔB`, and reads the
//! unchanged side only where it matches the change's keys, through
//! `TraceStore::lookup`. On the Ent each key is an index probe — the primary
//! order on column 0, an Arrangement (persisted, maintained derived state) on
//! any other column at the current edition, an extent search below it — so a
//! one-row change to a large join costs a few descents, not two snapshots.
//!
//! Pinned here:
//!
//! 1. **The delta law**, `eval_delta = snapshot(to) − snapshot(from)`, over
//!    random histories in which either side or both change in one interval,
//!    for joins on lead and trailing columns, on two columns at once, through a
//!    range-restricted side and through a compound side — at the current
//!    edition and in the past, on the Ent and on a store with only the trait's
//!    default `lookup`.
//! 2. **The cost**: maintaining a join over 20 000 rows after a one-row commit
//!    pages in a handful of frames from a reopened store.

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value,
};
use grmpl_diff::{eval_delta, eval_snapshot, multiset, Query};
use grmpl_ent::EntStore;

const R: RelId = RelId(1);
const S: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

/// Deterministic xorshift64*.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A store that forwards only the required reads, so every keyed lookup takes
/// the trait's default path.
struct Plain(EntStore);

impl EditionStore for Plain {
    fn current(&self) -> Edition {
        self.0.current()
    }
}

impl TraceStore for Plain {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.0.commit(updates)
    }
    fn commit_if(&self, pre: &[(RelId, Tuple)], updates: &[(RelId, Tuple, Diff)]) -> Result<Option<Edition>> {
        self.0.commit_if(pre, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.0.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.0.scan_updates(rel, from, to)
    }
    fn watermark(&self) -> Edition {
        self.0.watermark()
    }
    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        self.0.consolidate(up_to)
    }
}

/// `r(a, b, n)` and `s(x, y)`, with entity ids drawn from a small space so
/// joins actually match, and retractions of rows that exist.
fn churn(store: &dyn TraceStore, rng: &mut Rng, rounds: usize) -> Vec<Edition> {
    let mut held_r: Vec<Tuple> = Vec::new();
    let mut held_s: Vec<Tuple> = Vec::new();
    let mut eds = vec![store.current()];
    for _ in 0..rounds {
        let mut ups = Vec::new();
        // Each commit touches r, s, or both.
        let which = rng.below(3);
        for _ in 0..1 + rng.below(4) {
            if which != 1 {
                if !held_r.is_empty() && rng.below(3) == 0 {
                    let t = held_r.swap_remove(rng.below(held_r.len() as u64) as usize);
                    ups.push((R, t, -1));
                } else {
                    let t = Tuple::from([ent(rng.below(12)), ent(rng.below(12)), Value::Int(rng.below(3) as i64)]);
                    if !held_r.contains(&t) {
                        held_r.push(t.clone());
                        ups.push((R, t, 1));
                    }
                }
            }
            if which != 0 {
                if !held_s.is_empty() && rng.below(3) == 0 {
                    let t = held_s.swap_remove(rng.below(held_s.len() as u64) as usize);
                    ups.push((S, t, -1));
                } else {
                    let t = Tuple::from([ent(rng.below(12)), ent(rng.below(12))]);
                    if !held_s.contains(&t) {
                        held_s.push(t.clone());
                        ups.push((S, t, 1));
                    }
                }
            }
        }
        if !ups.is_empty() {
            eds.push(store.commit(&ups).unwrap());
        }
    }
    eds
}

fn queries() -> Vec<(&'static str, Query)> {
    let lo = Tuple::from([ent(3)]);
    let hi = Tuple::from([ent(9)]);
    vec![
        ("r.b = s.x (lead on s)", Query::rel(R).join(Query::rel(S), [1], [0])),
        ("r.a = s.y (trailing on s)", Query::rel(R).join(Query::rel(S), [0], [1])),
        ("s.y = r.b (trailing on both)", Query::rel(S).join(Query::rel(R), [1], [1])),
        ("two key columns", Query::rel(R).join(Query::rel(S), [0, 1], [0, 1])),
        ("range-restricted side", Query::rel(R).join(Query::range(S, lo, hi), [1], [0])),
        (
            "compound side",
            Query::rel(R).join(Query::rel(S), [1], [0]).join(Query::rel(S), [4], [0]),
        ),
        ("self join", Query::rel(R).join(Query::rel(R), [1], [0])),
    ]
}

fn delta_law(store: &dyn TraceStore, seed: u64) {
    let mut rng = Rng(seed);
    let eds = churn(store, &mut rng, 80);
    let now = *eds.last().unwrap();
    for (name, q) in queries() {
        for _ in 0..25 {
            let a = eds[rng.below(eds.len() as u64) as usize];
            // Half the intervals end at the present, where the Arrangements are.
            let b = if rng.below(2) == 0 { now } else { eds[rng.below(eds.len() as u64) as usize] };
            let (from, to) = (a.min(b), a.max(b));
            let mut want = eval_snapshot(&q, store, to).unwrap();
            let before = eval_snapshot(&q, store, from).unwrap();
            for (t, d) in before {
                multiset::add(&mut want, t, -d);
            }
            multiset::strip_zeros(&mut want);
            let got = eval_delta(&q, store, from, to).unwrap();
            assert_eq!(
                multiset::to_sorted_vec(&got),
                multiset::to_sorted_vec(&want),
                "seed {seed}, {name}, ({from:?}, {to:?}]"
            );
        }
    }
}

#[test]
fn the_join_delta_law_holds_through_the_ents_indexes() {
    for seed in 1..=12 {
        delta_law(&EntStore::new(), seed);
    }
}

#[test]
fn the_join_delta_law_holds_through_the_default_lookup() {
    for seed in 1..=6 {
        delta_law(&Plain(EntStore::new()), seed);
    }
}

#[test]
fn maintaining_a_large_join_after_a_small_commit_reads_a_few_frames() {
    // `world`: things and their names, joined on the thing — 10 000 of each.
    const N: u64 = 10_000;
    let dir = tempfile::tempdir().unwrap();
    {
        let store = EntStore::open(dir.path()).unwrap();
        for chunk in (0..N).collect::<Vec<_>>().chunks(1_000) {
            let mut ups = Vec::new();
            for &i in chunk {
                ups.push((R, Tuple::from([ent(i), ent(i % 50), Value::Int(0)]), 1));
                ups.push((S, Tuple::from([ent(i), ent(i + 1)]), 1));
            }
            store.commit(&ups).unwrap();
        }
    }
    let store = EntStore::open(dir.path()).unwrap();
    let from = store.current();
    // One thing moves: a retraction and an assertion in r.
    store
        .commit(&[
            (R, Tuple::from([ent(4_321), ent(4_321 % 50), Value::Int(0)]), -1),
            (R, Tuple::from([ent(4_321), ent(7), Value::Int(0)]), 1),
        ])
        .unwrap();
    let to = store.current();
    let q = Query::rel(R).join(Query::rel(S), [0], [0]);

    let before = store.frames_paged();
    let delta = eval_delta(&q, &store, from, to).unwrap();
    let paged = store.frames_paged() - before;
    assert_eq!(multiset::to_sorted_vec(&delta).len(), 2, "one row out, one row in");
    assert!(paged <= 16, "a one-row join delta paged in {paged} frames");

    // The snapshot difference it replaces reads both relations at both ends.
    let before = store.frames_paged();
    eval_snapshot(&q, &store, to).unwrap();
    assert!(store.frames_paged() - before > 10 * paged, "the comparison proves nothing");
}

#[test]
fn lookup_returns_exactly_the_rows_with_a_key() {
    let store = EntStore::new();
    let mut rng = Rng(77);
    let eds = churn(&store, &mut rng, 60);
    for &at in eds.iter().rev().step_by(9) {
        let rows = store.read_at(R, at).unwrap();
        for col in 0..3 {
            for _ in 0..8 {
                let mut keys: Vec<Value> = (0..1 + rng.below(4)).map(|_| ent(rng.below(14))).collect();
                if col == 2 {
                    keys = vec![Value::Int(rng.below(3) as i64)];
                }
                let mut want: Vec<(Tuple, Diff)> =
                    rows.iter().filter(|(t, _)| keys.contains(&t.as_slice()[col])).cloned().collect();
                let mut got = store.lookup(R, at, col, &keys).unwrap();
                want.sort();
                got.sort();
                assert_eq!(got, want, "{at:?} col {col} keys {keys:?}");
            }
        }
    }
}
