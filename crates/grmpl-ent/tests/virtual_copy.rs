//! **DSP virtual copy at the store: `instance_template` is a graft.**
//!
//! A template instance shares the template's nodes instead of re-inserting its
//! facts, and the edition log records it as one entry. That must not change
//! what anyone observes, so the laws here are the store's ordinary ones, checked
//! against a model that performs every instance as an explicit row-by-row copy:
//!
//! * `read_at` and `scan_updates` agree with the model at every edition, before
//!   and after a reopen — the graft's log entry expands to exactly the copied
//!   rows;
//! * the instance is independent: edits on either side never reach the other;
//! * routing sees it: relation-wide and key-range interests over the copied
//!   block fire, disjoint ones do not;
//! * forks and consolidation carry it;
//! * an occupied target block is refused without allocating an edition;
//! * and it is cheap: the nodes a large instance adds are `O(log n)`.

use std::collections::BTreeMap;

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Time, TraceStore, Tuple, Update, Value,
};
use grmpl_ent::EntStore;

const LOCATED: RelId = RelId(1);
const NAMED: RelId = RelId(2);
const EXITS: RelId = RelId(3);
const RELS: [RelId; 3] = [LOCATED, NAMED, EXITS];

const BLOCK: u64 = 1_000;
const SPAN: u64 = 100;

fn e(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn lead(n: u64) -> Tuple {
    Tuple::from([e(n)])
}

/// Shift every entity cell, as an instance does.
fn moved(t: &Tuple, by: i64) -> Tuple {
    Tuple::new(
        t.as_slice()
            .iter()
            .map(|v| match v {
                Value::Ent(x) => e(x.0.wrapping_add(by as u64)),
                other => other.clone(),
            })
            .collect::<Vec<_>>(),
    )
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Full history: every update in commit order, an instance written out row by
/// row in the copied block's key order.
#[derive(Default, Clone)]
struct Model {
    log: Vec<(u64, RelId, Tuple, Diff)>,
}

impl Model {
    fn read_at(&self, rel: RelId, at: u64) -> Vec<(Tuple, Diff)> {
        let mut net: BTreeMap<Tuple, Diff> = BTreeMap::new();
        for (ed, r, t, d) in &self.log {
            if *r == rel && *ed <= at {
                *net.entry(t.clone()).or_insert(0) += d;
            }
        }
        net.into_iter().filter(|(_, w)| *w != 0).collect()
    }
    fn scan(&self, rel: RelId, from: u64, to: u64) -> Vec<Update> {
        self.log
            .iter()
            .filter(|(ed, r, _, _)| *r == rel && *ed > from && *ed <= to)
            .map(|(ed, _, t, d)| Update { tuple: t.clone(), time: Time::input(*ed), diff: *d })
            .collect()
    }
    fn commit(&mut self, ed: u64, updates: &[(RelId, Tuple, Diff)]) {
        for (r, t, d) in updates {
            self.log.push((ed, *r, t.clone(), *d));
        }
    }
    /// The instance as explicit copies. `None` if the target block is occupied.
    fn instance(&mut self, ed: u64, cur: u64, shift: i64) -> Option<()> {
        let (lo, hi) = (lead(BLOCK), lead(BLOCK + SPAN));
        let (tlo, thi) = (moved(&lo, shift), moved(&hi, shift));
        let mut copies = Vec::new();
        for rel in RELS {
            let rows = self.read_at(rel, cur);
            let src: Vec<_> = rows.iter().filter(|(t, _)| *t >= lo && *t < hi).collect();
            if src.is_empty() {
                continue;
            }
            if rows.iter().any(|(t, _)| *t >= tlo && *t < thi) {
                return None;
            }
            copies.extend(src.into_iter().map(|(t, d)| (rel, moved(t, shift), *d)));
        }
        self.commit(ed, &copies);
        Some(())
    }
}

/// A self-contained template in `[BLOCK, BLOCK + SPAN)`: rooms in a chain with
/// exits both ways, an item in each room.
fn template() -> Vec<(RelId, Tuple, Diff)> {
    let mut out = Vec::new();
    for i in 0..20 {
        let room = BLOCK + i;
        out.push((NAMED, Tuple::from([e(room), Value::text(format!("room {i}"))]), 1));
        out.push((LOCATED, Tuple::from([e(BLOCK + 50 + i), e(room)]), 1));
        if i > 0 {
            out.push((EXITS, Tuple::from([e(room - 1), Value::text("east"), e(room)]), 1));
            out.push((EXITS, Tuple::from([e(room), Value::text("west"), e(room - 1)]), 1));
        }
    }
    out
}

fn agree(store: &EntStore, model: &Model, ctx: &str) {
    let cur = store.current().0;
    for rel in RELS {
        for at in [cur.saturating_sub(3), cur.saturating_sub(1), cur] {
            assert_eq!(
                store.read_at(rel, Edition(at)).unwrap(),
                model.read_at(rel, at),
                "{ctx}: read_at {rel:?} @ {at}"
            );
        }
        assert_eq!(
            store.scan_updates(rel, Edition::ZERO, Edition(cur)).unwrap(),
            model.scan(rel, 0, cur),
            "{ctx}: scan_updates {rel:?}"
        );
    }
}

/// Random edits — inside the template, inside instances, and outside both —
/// interleaved with instances into a few candidate blocks, some of which
/// collide and must be refused. Checked against the model after every step and
/// again after a reopen.
#[test]
fn instances_match_an_explicit_copy_model_under_churn() {
    let (mut accepted, mut refused) = (0, 0);
    for seed in 1..=12u64 {
        let mut rng = Rng::new(seed);
        let dir = tempfile::tempdir().unwrap();
        let mut model = Model::default();
        {
            let store = EntStore::open(dir.path()).unwrap();
            let seed_rows = template();
            let ed = store.commit(&seed_rows).unwrap().0;
            model.commit(ed, &seed_rows);
            for round in 0..40 {
                let ctx = format!("seed {seed} round {round}");
                let cur = store.current().0;
                if rng.below(3) == 0 {
                    let shift = 1_000 * (1 + rng.below(4)) as i64;
                    let got = store.instance_template(&RELS, BLOCK, BLOCK + SPAN, shift);
                    match model.instance(cur + 1, cur, shift) {
                        Some(()) => {
                            assert_eq!(got.unwrap().0, cur + 1, "{ctx}: instance edition");
                            accepted += 1;
                        }
                        None => {
                            refused += 1;
                            assert!(got.is_err(), "{ctx}: instance over an occupied block accepted");
                            assert_eq!(store.current().0, cur, "{ctx}: a refused instance allocated an edition");
                        }
                    }
                } else {
                    // Edit a random entity in the template or one of the
                    // candidate instance blocks (or outside all of them).
                    let base = [BLOCK, 2_000, 3_000, 4_000, 5_000, 7_000][rng.below(6) as usize];
                    let thing = base + 50 + rng.below(20);
                    let room = base + rng.below(20);
                    let row = Tuple::from([e(thing), e(room)]);
                    let have = model.read_at(LOCATED, cur).iter().any(|(t, _)| *t == row);
                    let d = if have { -1 } else { 1 };
                    let updates = [(LOCATED, row, d)];
                    let ed = store.commit(&updates).unwrap().0;
                    model.commit(ed, &updates);
                }
                agree(&store, &model, &ctx);
            }
        }
        let store = EntStore::open(dir.path()).unwrap();
        agree(&store, &model, &format!("seed {seed} after reopen"));
    }
    // Both outcomes must actually have been exercised.
    assert!(accepted > 10 && refused > 10, "accepted {accepted}, refused {refused}");
}

#[test]
fn routing_sees_an_instance_by_its_block() {
    let store = EntStore::new();
    store.commit(&template()).unwrap();
    let before = store.current();
    // Declare interests first: one over the target block, one elsewhere.
    let (hit_lo, hit_hi) = (lead(2_000), lead(2_100));
    let (miss_lo, miss_hi) = (lead(9_000), lead(9_100));
    assert!(!store.touched_range_since(before, before, NAMED, &hit_lo, &hit_hi).unwrap());
    assert!(!store.touched_range_since(before, before, NAMED, &miss_lo, &miss_hi).unwrap());

    let at = store.instance_template(&RELS, BLOCK, BLOCK + SPAN, 1_000).unwrap();
    assert!(store.touched_since(before, at, &[NAMED]).unwrap(), "relation-wide routing missed it");
    assert!(
        store.touched_range_since(before, at, NAMED, &hit_lo, &hit_hi).unwrap(),
        "an interest over the instance block was not woken"
    );
    assert!(
        !store.touched_range_since(before, at, NAMED, &miss_lo, &miss_hi).unwrap(),
        "a disjoint interest was woken"
    );
}

#[test]
fn forks_and_consolidation_carry_an_instance() {
    let store = EntStore::new();
    store.commit(&template()).unwrap();
    let at = store.instance_template(&RELS, BLOCK, BLOCK + SPAN, 1_000).unwrap();
    let rows = |s: &EntStore, ed: Edition| s.read_at(NAMED, ed).unwrap();
    let want = rows(&store, at);
    assert_eq!(want.len(), 40, "template and instance each name 20 rooms");

    // A fork at the instance edition sees it, and evolves independently.
    let fork = store.fork_at(at).unwrap();
    assert_eq!(rows(&fork, at), want);
    assert_eq!(
        fork.scan_updates(NAMED, Edition(at.0 - 1), at).unwrap(),
        store.scan_updates(NAMED, Edition(at.0 - 1), at).unwrap(),
    );
    fork.commit(&[(NAMED, Tuple::from([e(2_000), Value::text("room 0")]), -1)]).unwrap();
    assert_eq!(rows(&store, store.current()), want, "a fork's edit reached its parent");

    // Consolidating up to the instance keeps it; a later read is unchanged and
    // the log above the watermark still expands.
    let after = store.commit(&[(NAMED, Tuple::from([e(2_001), Value::text("room 1")]), -1)]).unwrap();
    store.consolidate(at).unwrap();
    assert_eq!(rows(&store, at), want);
    assert_eq!(store.scan_updates(NAMED, at, after).unwrap().len(), 1);
    assert!(store.scan_updates(NAMED, Edition::ZERO, after).is_err(), "below the watermark");
}

/// The point of a graft: a large template instances in `O(log n)` new nodes.
/// The same rows re-inserted one by one would add a leaf per run of `B`.
#[test]
fn a_large_instance_adds_log_n_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    // 20 000 facts in the template block, plus unrelated facts around it.
    let mut rows = Vec::new();
    for i in 0..20_000u64 {
        rows.push((LOCATED, Tuple::from([e(100_000 + i), e(100_000 + (i % 97))]), 1));
        rows.push((LOCATED, Tuple::from([e(i), e(i % 13)]), 1));
    }
    store.commit(&rows).unwrap();
    let before = store.stored_nodes().unwrap();
    store.instance_template(&[LOCATED], 100_000, 120_000, 1_000_000).unwrap();
    let added = store.stored_nodes().unwrap() - before;
    assert!(added <= 24, "instancing 20 000 facts added {added} nodes");
    assert_eq!(store.count_at(LOCATED, store.current(), &lead(1_100_000), &lead(1_120_000)).unwrap(), 20_000);
}
