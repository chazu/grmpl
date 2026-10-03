//! **Scope-inherited context: the Context enfilade, stabbed through extents.**
//!
//! A scope relation holds bindings `(first, last, key, value)` over inclusive
//! entity spans; `Query::Inherit` gives each entity the value of the most
//! specific span containing it. The Ent answers "which spans contain these
//! entities" (`read_containing`) by searching the scope relation's extents —
//! the least start and greatest end under each subtree, which is an interval
//! tree's pruning — rather than reading the relation.
//!
//! Pinned here, on the Ent and on a store with only the trait defaults:
//!
//! 1. `read_containing` returns exactly the containing rows.
//! 2. `Inherit` matches a brute-force "innermost span" over random nested
//!    scopes, including scopes added, overridden and removed.
//! 3. Its delta law holds across changes to entities, to scopes, and to both.
//! 4. A graft carries a block's scopes with it, displaced: an instance's rooms
//!    inherit from the instance's copy of the template's scopes.

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value,
};
use grmpl_diff::{eval_delta, eval_snapshot, multiset, Query};
use grmpl_ent::EntStore;

const THINGS: RelId = RelId(1);
const SCOPES: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn scope(first: u64, last: u64, key: &str, value: &str) -> Tuple {
    Tuple::from([ent(first), ent(last), Value::text(key), Value::text(value)])
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

/// Forwards only the required reads, so every stab takes the trait default.
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

/// Nested scopes: a span is a node of a 4-ary tree over `[0, 256)`, so any two
/// spans are disjoint or nested, as entity blocks are.
fn random_span(rng: &mut Rng) -> (u64, u64) {
    let depth = rng.below(4);
    let width = 256 >> (2 * depth);
    let start = rng.below(256 / width) * width;
    (start, start + width - 1)
}

/// The brute-force answer: for each thing `(e)`, the value of the binding of
/// `key` from the containing span that starts latest, ends earliest, and then
/// has the least value.
fn truth(store: &dyn TraceStore, at: Edition, key: &str) -> Vec<(Tuple, Diff)> {
    let scopes = store.read_at(SCOPES, at).unwrap();
    let mut out = Vec::new();
    for (thing, w) in store.read_at(THINGS, at).unwrap() {
        let e = &thing.as_slice()[0];
        let best = scopes
            .iter()
            .filter(|(t, d)| *d > 0 && t.as_slice()[2] == Value::text(key))
            .filter(|(t, _)| &t.as_slice()[0] <= e && e <= &t.as_slice()[1])
            .map(|(t, _)| (t.as_slice()[0].clone(), t.as_slice()[1].clone(), t.as_slice()[3].clone()))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)).then_with(|| b.2.cmp(&a.2)));
        if let Some((_, _, v)) = best {
            out.push((Tuple::from([e.clone(), v]), w));
        }
    }
    out.sort();
    out
}

fn mood() -> Query {
    Query::Inherit { input: Box::new(Query::rel(THINGS)), col: 0, key: Value::text("mood"), ctx: SCOPES }
}

fn churn(store: &dyn TraceStore, seed: u64) -> Vec<Edition> {
    let mut rng = Rng(seed);
    let (mut things, mut scopes): (Vec<Tuple>, Vec<Tuple>) = (Vec::new(), Vec::new());
    let mut eds = vec![store.current()];
    for _ in 0..70 {
        let mut ups = Vec::new();
        match rng.below(3) {
            0 if !scopes.is_empty() => {
                let t = scopes.swap_remove(rng.below(scopes.len() as u64) as usize);
                ups.push((SCOPES, t, -1));
            }
            0 | 1 => {
                let (a, b) = random_span(&mut rng);
                let key = if rng.below(4) == 0 { "light" } else { "mood" };
                let t = scope(a, b, key, &format!("v{}", rng.below(5)));
                if !scopes.contains(&t) {
                    scopes.push(t.clone());
                    ups.push((SCOPES, t, 1));
                }
            }
            _ => {
                let t = Tuple::from([ent(rng.below(256))]);
                if let Some(i) = things.iter().position(|x| *x == t) {
                    things.swap_remove(i);
                    ups.push((THINGS, t, -1));
                } else {
                    things.push(t.clone());
                    ups.push((THINGS, t, 1));
                }
            }
        }
        if !ups.is_empty() {
            eds.push(store.commit(&ups).unwrap());
        }
    }
    eds
}

fn laws(store: &dyn TraceStore, seed: u64) {
    let eds = churn(store, seed);
    let mut rng = Rng(seed ^ 0xABCD);
    for &at in eds.iter().step_by(5) {
        // 1. read_containing is exact.
        let points: Vec<Value> = (0..1 + rng.below(4)).map(|_| ent(rng.below(256))).collect();
        let mut got = store.read_containing(SCOPES, at, 0, 1, &points).unwrap();
        let mut want: Vec<(Tuple, Diff)> = store
            .read_at(SCOPES, at)
            .unwrap()
            .into_iter()
            .filter(|(t, _)| points.iter().any(|p| &t.as_slice()[0] <= p && p <= &t.as_slice()[1]))
            .collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "seed {seed} {at:?} points {points:?}");
        // 2. Inherit is the innermost binding.
        let got = multiset::to_sorted_vec(&eval_snapshot(&mood(), store, at).unwrap());
        assert_eq!(got, truth(store, at, "mood"), "seed {seed} {at:?}");
    }
    // 3. The delta law.
    for _ in 0..30 {
        let (a, b) = (eds[rng.below(eds.len() as u64) as usize], eds[rng.below(eds.len() as u64) as usize]);
        let (from, to) = (a.min(b), a.max(b));
        let mut want = eval_snapshot(&mood(), store, to).unwrap();
        for (t, d) in eval_snapshot(&mood(), store, from).unwrap() {
            multiset::add(&mut want, t, -d);
        }
        multiset::strip_zeros(&mut want);
        let got = eval_delta(&mood(), store, from, to).unwrap();
        assert_eq!(multiset::to_sorted_vec(&got), multiset::to_sorted_vec(&want), "seed {seed} ({from:?}, {to:?}]");
    }
}

#[test]
fn inheritance_is_the_innermost_scope_on_the_ent() {
    for seed in 1..=10 {
        laws(&EntStore::new(), seed);
    }
}

#[test]
fn inheritance_is_the_innermost_scope_through_the_defaults() {
    for seed in 1..=5 {
        laws(&Plain(EntStore::new()), seed);
    }
}

#[test]
fn a_graft_carries_a_blocks_scopes_displaced() {
    let store = EntStore::new();
    // A template block [1000, 1099]: the whole block is "hushed", and its
    // inner sanctum [1050, 1059] is "holy".
    store
        .commit(&[
            (SCOPES, scope(1_000, 1_099, "mood", "hushed"), 1),
            (SCOPES, scope(1_050, 1_059, "mood", "holy"), 1),
            (SCOPES, scope(0, 1_000_000, "mood", "mundane"), 1),
            (THINGS, Tuple::from([ent(1_003)]), 1),
            (THINGS, Tuple::from([ent(1_055)]), 1),
        ])
        .unwrap();
    // The world-wide scope starts outside the block, so only the block's own
    // scopes travel with it.
    let at = store.instance_template(&[THINGS, SCOPES], 1_000, 1_100, 5_000).unwrap();
    let moods: Vec<(Tuple, Diff)> = multiset::to_sorted_vec(&eval_snapshot(&mood(), &store, at).unwrap());
    let of = |e: u64| moods.iter().find(|(t, _)| t.as_slice()[0] == ent(e)).map(|(t, _)| t.as_slice()[1].clone());
    assert_eq!(of(6_003), Some(Value::text("hushed")), "the instance's room inherits the copied block scope");
    assert_eq!(of(6_055), Some(Value::text("holy")), "and its sanctum the copied inner scope");
    assert_eq!(of(1_055), Some(Value::text("holy")), "the template is unchanged");
    // Retract the instance's inner scope: its outer copy shows through.
    let at = store.commit(&[(SCOPES, scope(6_050, 6_059, "mood", "holy"), -1)]).unwrap();
    let moods = multiset::to_sorted_vec(&eval_snapshot(&mood(), &store, at).unwrap());
    assert!(moods.contains(&(Tuple::from([ent(6_055), Value::text("hushed")]), 1)));
}

#[test]
fn stabbing_a_large_scope_relation_reads_a_few_frames() {
    // 20 000 disjoint room-sized scopes, plus a few enclosing regions.
    let dir = tempfile::tempdir().unwrap();
    {
        let store = EntStore::open(dir.path()).unwrap();
        let rows: Vec<(RelId, Tuple, Diff)> = (0..20_000u64)
            .map(|i| (SCOPES, scope(i * 10, i * 10 + 9, "mood", "room"), 1))
            .chain((0..20u64).map(|r| (SCOPES, scope(r * 10_000, r * 10_000 + 9_999, "mood", "region"), 1)))
            .collect();
        for chunk in rows.chunks(1_000) {
            store.commit(chunk).unwrap();
        }
    }
    let store = EntStore::open(dir.path()).unwrap();
    let before = store.frames_paged();
    let hits = store.read_containing(SCOPES, store.current(), 0, 1, &[ent(123_456)]).unwrap();
    let paged = store.frames_paged() - before;
    assert_eq!(hits.len(), 2, "the room and its region");
    assert!(paged <= 12, "a stab paged in {paged} frames");
}
