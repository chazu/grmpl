//! **The laws of item leaves**: rows, runs and holes, in both layouts.
//!
//! Random histories biased towards what forms runs (blocks of ids with one
//! tag, exits whose entity cells step together) mixed with scattered rows,
//! edits inside runs, value changes, holes reserved and filled, grafts, cuts
//! and joins. After every step the tree must hold its layout's invariants and
//! answer every read exactly as a model of rows and reserved keys does; runs
//! and holes must never change an answer, only how few items hold it.

use std::collections::{BTreeMap, BTreeSet};

use grmpl_core::{Entity, Tuple, Value};

use super::{Item, Kind, Layout, Span, Tree, B};
use crate::dsp::Displace;
use crate::granfilade::Granfilade;
use crate::measure::{Count, Extent, Measure};

type M = (Count, Extent);
type T = Tree<Tuple, i64, M>;

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn lead(n: u64) -> Tuple {
    Tuple::from([ent(n)])
}

/// Deterministic xorshift64*.
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

const WORLD: u64 = 3_000;

/// Rows and reserved keys.
#[derive(Clone, Default, Debug, PartialEq)]
struct Model {
    rows: BTreeMap<Tuple, i64>,
    holes: BTreeSet<Tuple>,
}

impl Model {
    fn occupied(&self, lo: &Tuple, last: &Tuple) -> bool {
        self.rows.range(lo.clone()..=last.clone()).next().is_some() || self.holes.range(lo.clone()..=last.clone()).next().is_some()
    }
    fn insert(&mut self, k: Tuple, v: i64) {
        self.holes.remove(&k);
        self.rows.insert(k, v);
    }
}

/// The tree in one layout, with its writes.
struct Lay(Layout);
impl Lay {
    fn insert(&self, t: &T, k: Tuple, v: i64) -> T {
        match self.0 {
            Layout::Ordered => t.insert(k, v),
            Layout::Kd => t.kd_insert(k, v),
        }
    }
    fn remove(&self, t: &T, k: &Tuple) -> T {
        match self.0 {
            Layout::Ordered => t.remove(k),
            Layout::Kd => t.kd_remove(k),
        }
    }
    fn reserve(&self, t: &T, s: Span<Tuple>) -> Option<T> {
        match self.0 {
            Layout::Ordered => t.reserve(s),
            Layout::Kd => t.kd_reserve(s),
        }
    }
    fn graft(&self, t: &T, lo: u64, hi: u64, by: i64) -> Option<T> {
        match self.0 {
            Layout::Ordered => t.graft(&lead(lo), &lead(hi), by),
            Layout::Kd => t.kd_graft(&lead(lo), &lead(hi), by),
        }
    }
    fn cut_and_rejoin(&self, t: &T, at: u64) -> T {
        let p = lead(at);
        match self.0 {
            Layout::Ordered => {
                let (a, b) = t.split(&p);
                T::join(&a, &b)
            }
            Layout::Kd => {
                let (a, b) = t.kd_split(&p);
                T::kd_join_at(&a, &p, &b)
            }
        }
    }
    fn check(&self, t: &T) {
        match self.0 {
            Layout::Ordered => t.check(),
            Layout::Kd => t.kd_check(),
        }
    }
}

/// How many items, and how many of them runs, the tree's leaves hold.
fn items_of(t: &T) -> (usize, usize) {
    let Some(n) = t.root.as_deref() else { return (0, 0) };
    match n.kind() {
        Kind::Leaf(items) => (items.len(), items.iter().filter(|it| matches!(it, Item::Run(..))).count()),
        Kind::Internal { children, .. } => children.iter().map(items_of).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        Kind::Split { children, .. } => children.iter().map(items_of).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
    }
}

fn contents(t: &T) -> BTreeMap<Tuple, i64> {
    t.iter().map(|(k, v)| (k, *v)).collect()
}

fn model_measure(rows: &BTreeMap<Tuple, i64>, holes: &BTreeSet<Tuple>, lo: &Tuple, hi: &Tuple) -> M {
    let mut m = <M as Measure<Tuple, i64>>::empty();
    for (k, v) in rows.range(lo.clone()..hi.clone()) {
        Measure::<Tuple, i64>::absorb_entry(&mut m, k, v);
    }
    // Holes count in the extent (where keys lie), not in the count.
    let _ = holes;
    m
}

/// Every read, through a handle displaced by `by`, against the model.
fn reads_agree(t: &T, m: &Model, rng: &mut Rng, by: i64) {
    let t = t.relocate(by);
    let rows: BTreeMap<Tuple, i64> = m.rows.iter().map(|(k, v)| (k.displace(by), *v)).collect();
    let holes: BTreeSet<Tuple> = m.holes.iter().map(|k| k.displace(by)).collect();
    assert_eq!(t.len(), rows.len(), "rows");
    assert_eq!(t.reserved(), holes.len() as u64, "reserved keys");
    assert_eq!(contents(&t), rows, "contents");
    let base = by as u64;
    for _ in 0..10 {
        let probe = match rng.below(3) {
            0 => rows.keys().nth(rng.below(rows.len() as u64 + 1) as usize).cloned(),
            1 => holes.iter().nth(rng.below(holes.len() as u64 + 1) as usize).cloned(),
            _ => None,
        }
        .unwrap_or_else(|| Tuple::from([ent(base + rng.below(WORLD)), Value::text("room")]));
        assert_eq!(t.get(&probe), rows.get(&probe), "get {probe:?}");
        assert_eq!(t.holds_key(&probe), rows.contains_key(&probe) || holes.contains(&probe), "holds {probe:?}");
        assert_eq!(
            t.last_le(&probe).map(|(k, v)| (k, *v)),
            rows.range(..=probe.clone()).next_back().map(|(k, v)| (k.clone(), *v)),
            "last_le {probe:?}"
        );
        let (a, b) = (rng.below(WORLD * 4), rng.below(WORLD * 4));
        let (lo, hi) = (lead(base + a.min(b)), lead(base + a.max(b) + 1));
        let want: Vec<(Tuple, i64)> = rows.range(lo.clone()..hi.clone()).map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(t.range_collect(&lo, &hi), want, "range");
        assert_eq!(t.count_range(&lo, &hi), want.len(), "count");
        let got = t.measure_range(&lo, &hi);
        let wantm = model_measure(&rows, &holes, &lo, &hi);
        assert_eq!(got.0, wantm.0, "measured count");
        // The extent over rows bounds what the tree reports for them.
        for col in 0..3 {
            if let Some((l, h)) = wantm.1.column(col) {
                let (gl, gh) = got.1.column(col).expect("the tree's extent covers its rows");
                assert!(gl <= l && h <= gh, "extent column {col}");
            }
        }
        let any_holes = holes.range(lo.clone()..hi.clone()).next().is_some();
        assert_eq!(t.any_in(&lo, &hi), !want.is_empty() || any_holes, "any_in counts rows and holes");
        let (c0, c1) = (base + a.min(b), base + a.min(b) + 1 + rng.below(300));
        let found = t.search(|(_, x)| x.meets(2, c0, c1), |k, _| matches!(k.as_slice().get(2), Some(Value::Ent(e)) if c0 <= e.0 && e.0 < c1));
        let want: Vec<(Tuple, i64)> = rows
            .iter()
            .filter(|(k, _)| matches!(k.as_slice().get(2), Some(Value::Ent(e)) if c0 <= e.0 && e.0 < c1))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        assert_eq!(found, want, "search");
    }
}

fn model_diff(a: &BTreeMap<Tuple, i64>, b: &BTreeMap<Tuple, i64>) -> Vec<(Tuple, Option<i64>, Option<i64>)> {
    let mut keys: Vec<&Tuple> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).map(|k| (k.clone(), a.get(k).copied(), b.get(k).copied())).collect()
}

fn room(e: u64) -> Tuple {
    Tuple::from([ent(e), Value::text("room")])
}

fn exit(e: u64) -> Tuple {
    Tuple::from([ent(e), Value::Int(0), ent(e + 1)])
}

fn placeholder(e: u64) -> Tuple {
    Tuple::from([ent(e), Value::Int(-1)])
}

/// One random step on tree and model.
fn step(lay: &Lay, t: &mut T, m: &mut Model, rng: &mut Rng) {
    match rng.below(20) {
        0..=5 => {
            // A block of rooms or exits, in order, as a world loads them.
            let (start, len) = (rng.below(WORLD), 1 + rng.below(150));
            let v = 1 + (rng.below(8) == 0) as i64;
            let shape = rng.below(2);
            for e in start..start + len {
                let k = if shape == 0 { room(e) } else { exit(e) };
                *t = lay.insert(t, k.clone(), v);
                m.insert(k, v);
            }
        }
        6..=8 => {
            let k = match rng.below(3) {
                0 => room(rng.below(WORLD)),
                1 => exit(rng.below(WORLD)),
                _ => Tuple::from([ent(rng.below(WORLD)), Value::Int(rng.below(5) as i64), ent(rng.below(WORLD))]),
            };
            let v = 1 + rng.below(3) as i64;
            *t = lay.insert(t, k.clone(), v);
            m.insert(k, v);
        }
        9..=11 => {
            // Remove a row, often from inside a run.
            let k = match m.rows.keys().nth(rng.below(m.rows.len() as u64 + 1) as usize) {
                Some(k) => k.clone(),
                None => room(rng.below(WORLD)),
            };
            *t = lay.remove(t, &k);
            m.rows.remove(&k);
        }
        12 | 13 => {
            // Reserve a block of placeholders.
            let (first, n) = (rng.below(WORLD), 1 + rng.below(60));
            let span = Span { first: placeholder(first), stride: Tuple::from([Value::Int(1), Value::Int(0)]), n };
            let last = span.last();
            let free = !m.occupied(&span.first, &last);
            let got = lay.reserve(t, span.clone());
            assert_eq!(got.is_some(), free, "reserving {:?}..={last:?}", span.first);
            if let Some(g) = got {
                *t = g;
                for i in 0..n {
                    m.holes.insert(span.row(i));
                }
            }
        }
        14 | 15 => {
            // Fill a reserved key.
            if let Some(k) = m.holes.iter().nth(rng.below(m.holes.len() as u64 + 1) as usize).cloned() {
                *t = lay.insert(t, k.clone(), 7);
                m.insert(k, 7);
            }
        }
        16 | 17 => {
            // Graft a block above the world: runs and holes travel with it.
            let (lo, span) = (rng.below(WORLD), 1 + rng.below(400));
            let by = (WORLD * (2 + rng.below(5))) as i64;
            let (src_lo, src_hi) = (lead(lo), lead(lo + span));
            let (tlo, thi) = (src_lo.displace(by), src_hi.displace(by));
            let copied_rows: Vec<(Tuple, i64)> = m.rows.range(src_lo.clone()..src_hi.clone()).map(|(k, v)| (k.clone(), *v)).collect();
            let copied_holes: Vec<Tuple> = m.holes.range(src_lo..src_hi).cloned().collect();
            let empty = copied_rows.is_empty() && copied_holes.is_empty();
            let occupied =
                m.rows.range(tlo.clone()..thi.clone()).next().is_some() || m.holes.range(tlo..thi).next().is_some();
            let got = lay.graft(t, lo, lo + span, by);
            assert_eq!(got.is_some(), empty || !occupied, "graft refusal");
            if let Some(g) = got {
                *t = g;
                for (k, v) in copied_rows {
                    m.rows.insert(k.displace(by), v);
                }
                for k in copied_holes {
                    m.holes.insert(k.displace(by));
                }
            }
        }
        _ => *t = lay.cut_and_rejoin(t, rng.below(WORLD * 7)),
    }
}

#[test]
fn runs_and_holes_never_change_an_answer() {
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        for seed in 0..16u64 {
            let mut rng = Rng::new(seed);
            let (mut t, mut m) = (T::new(), Model::default());
            for round in 0..120 {
                let before = (t.clone(), m.clone());
                step(&lay, &mut t, &mut m, &mut rng);
                lay.check(&t);
                assert_eq!(contents(&t), m.rows, "{layout:?} seed {seed} round {round}: contents");
                assert_eq!(t.reserved(), m.holes.len() as u64, "{layout:?} seed {seed} round {round}: holes");
                assert_eq!(before.0.diff(&t), model_diff(&before.1.rows, &m.rows), "{layout:?} seed {seed} round {round}: diff");
                if round % 15 == 0 {
                    let by = if rng.below(2) == 0 { 0 } else { 1 + rng.below(1 << 20) as i64 };
                    reads_agree(&t, &m, &mut rng, by);
                }
            }
            let (items, runs) = items_of(&t);
            assert!(runs > 0, "{layout:?} seed {seed}: no run formed from {} rows in {items} items", t.len());
        }
    }
}

#[test]
fn a_block_loaded_row_by_row_is_one_item() {
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        let mut t = T::new();
        for e in 0..100_000u64 {
            t = lay.insert(&t, room(e), 1);
        }
        lay.check(&t);
        assert_eq!(t.len(), 100_000);
        assert_eq!(items_of(&t), (1, 1), "{layout:?}: 100k rooms in order");
        // Its measures are computed, not folded: count and extent of a span.
        let m = t.measure_range(&lead(10), &lead(90_010));
        assert_eq!(m.0 .0, 90_000);
        assert_eq!(m.1.column(0), Some((10, 90_009)));
        // An edit inside it splits it; the next row back rejoins it.
        let cut = lay.remove(&t, &room(50_000));
        assert_eq!(items_of(&cut), (2, 2));
        assert_eq!(cut.diff(&t), vec![(room(50_000), None, Some(1))]);
        let back = lay.insert(&cut, room(50_000), 1);
        assert_eq!(items_of(&back), (1, 1), "{layout:?}: the row rejoins its run");
        // A different value there splits it into three.
        let other = lay.insert(&t, room(50_000), 2);
        assert_eq!(items_of(&other), (3, 2));
    }
}

#[test]
fn runs_and_holes_round_trip_through_the_granfilade() {
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        let dir = tempfile::tempdir().unwrap();
        let gran = Granfilade::open(dir.path()).unwrap();
        let mut rng = Rng::new(9);
        let (mut t, mut m) = (T::new(), Model::default());
        for _ in 0..200 {
            step(&lay, &mut t, &mut m, &mut rng);
        }
        let ck = gran.persist(&t).unwrap();
        let back: T = gran.load(ck).unwrap();
        lay.check(&back);
        assert_eq!(contents(&back), m.rows);
        assert_eq!(back.reserved(), m.holes.len() as u64);
        reads_agree(&back, &m, &mut rng, 0);
        assert!(back.diff(&t).is_empty());
    }
}

#[test]
fn a_hole_is_filled_key_by_key_and_refuses_what_it_overlaps() {
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        let mut t = T::new();
        for e in 0..2 * B as u64 {
            t = lay.insert(&t, Tuple::from([ent(e * 10_000), Value::text("anchor")]), 1);
        }
        let span = Span { first: placeholder(500), stride: Tuple::from([Value::Int(1), Value::Int(0)]), n: 1_000 };
        t = lay.reserve(&t, span.clone()).expect("free keys");
        lay.check(&t);
        assert_eq!(t.reserved(), 1_000);
        assert_eq!(t.len(), 2 * B, "a hole holds no rows");
        // Its keys are held, but not as rows.
        assert!(t.holds_key(&placeholder(700)) && t.get(&placeholder(700)).is_none());
        // Overlapping keys are refused: another hole, or one over a row.
        assert!(lay.reserve(&t, Span { first: placeholder(1_400), ..span.clone() }).is_none());
        assert!(lay.reserve(&t, Span { first: Tuple::from([ent(9_999), Value::text("x")]), stride: Tuple::from([Value::Int(1), Value::Int(0)]), n: 3 }).is_none());
        // A fill takes the key's place.
        t = lay.insert(&t, placeholder(700), 5);
        lay.check(&t);
        assert_eq!((t.reserved(), t.get(&placeholder(700))), (999, Some(&5)));
    }
}

#[test]
fn interleaving_runs_come_apart_in_one_leaf() {
    // Two lines of exits whose keys alternate row by row, and rows that fall
    // between a run's keys: a k-d build may bring them into one leaf, where
    // items must not overlap.
    let mut rows: BTreeMap<Tuple, i64> = BTreeMap::new();
    for e in 0..300u64 {
        rows.insert(Tuple::from([ent(e), Value::Int(0), ent(e + 1)]), 1);
        rows.insert(Tuple::from([ent(e), Value::Int(1), ent(e + 5)]), 1);
        if e % 7 == 0 {
            rows.insert(Tuple::from([ent(e), Value::Int(0), ent(9_000 + e)]), 2);
        }
    }
    let t = T::kd_build(rows.iter().map(|(k, v)| (k.clone(), *v)).collect());
    t.kd_check();
    assert_eq!(contents(&t), rows);
    // And through edits that rebuild subtrees.
    let mut t = t;
    let mut m = rows.clone();
    let mut rng = Rng::new(4);
    for _ in 0..2_000 {
        let e = rng.below(300);
        let k = Tuple::from([ent(e), Value::Int(rng.below(2) as i64), ent(e + 1 + 4 * rng.below(2))]);
        if rng.below(3) == 0 {
            t = t.kd_remove(&k);
            m.remove(&k);
        } else {
            t = t.kd_insert(k.clone(), 1);
            m.insert(k, 1);
        }
    }
    t.kd_check();
    assert_eq!(contents(&t), m);
}

#[test]
fn a_cut_at_a_runs_own_rows_splits_it_there() {
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        let mut t = T::new();
        for e in 0..1_000u64 {
            t = lay.insert(&t, room(e), 1);
        }
        assert_eq!(items_of(&t).1, 1);
        // At a row inside the run, at its first and at its last.
        for at in [room(500), room(0), room(999), room(998)] {
            let (a, b) = match layout {
                Layout::Ordered => t.split(&at),
                Layout::Kd => t.kd_split(&at),
            };
            lay.check(&a);
            lay.check(&b);
            let all = contents(&t);
            assert_eq!(contents(&a), all.range(..at.clone()).map(|(k, v)| (k.clone(), *v)).collect(), "{layout:?} below {at:?}");
            assert_eq!(contents(&b), all.range(at.clone()..).map(|(k, v)| (k.clone(), *v)).collect(), "{layout:?} from {at:?}");
        }
    }
}

#[test]
fn a_rebuild_brings_interleaving_runs_apart() {
    // Two lines of exits split onto two leaves by their second column: each
    // leaf is one run, and their keys alternate. A rebuild that gathers them
    // into one leaf must cut them into rows rather than overlap them.
    let line = |w: i64, to: u64| Span {
        first: Tuple::from([ent(0), Value::Int(w), ent(to)]),
        stride: Tuple::from([Value::Int(1), Value::Int(0), Value::Int(1)]),
        n: 20,
    };
    let (a, b) = (T::leaf(vec![Item::Run(line(0, 1), 1)]), T::leaf(vec![Item::Run(line(1, 5), 1)]));
    let t = T::split_node(1, Tuple::from([Value::Int(1)]), a, b);
    t.kd_check();
    let rebuilt = T::build(T::items_of(&t));
    rebuilt.kd_check();
    assert_eq!(contents(&rebuilt), contents(&t));
}

#[test]
fn a_changed_text_cell_never_steps() {
    // `(e, "a")`, `(e+1, "b")`, `(e+2, "b")`: a stride that ignored the text
    // would fold them into a run computing `(e+1, "a")`.
    for layout in [Layout::Ordered, Layout::Kd] {
        let lay = Lay(layout);
        let rows = [
            Tuple::from([ent(10), Value::text("a")]),
            Tuple::from([ent(11), Value::text("b")]),
            Tuple::from([ent(12), Value::text("b")]),
        ];
        let mut t = T::new();
        for k in &rows {
            t = lay.insert(&t, k.clone(), 1);
        }
        assert_eq!(contents(&t).into_keys().collect::<Vec<_>>(), rows.to_vec());
        assert_eq!(rows[0].stride_to(&rows[1]), None);
    }
}

#[test]
fn runs_from_one_key_on_different_steps_differ() {
    // Two versions holding runs from the same first row, stepping by 1 and by
    // 2: their rows agree only at the start.
    let run = |step: i64| {
        T::leaf(vec![Item::Run(
            Span { first: Tuple::from([ent(0), Value::Int(0)]), stride: Tuple::from([Value::Int(step), Value::Int(0)]), n: 10 },
            1,
        )])
    };
    let (a, b) = (run(1), run(2));
    assert_eq!(a.diff(&b), model_diff(&contents(&a), &contents(&b)));
}

#[test]
fn cutting_every_run_of_a_copy_keeps_the_tree_sound() {
    // A block of 96 three-row runs, copied under a displacement into a taller
    // world; cutting a row out of each run in turn doubles the copy's items,
    // overflowing its leaves one remove at a time.
    let mut t = T::new();
    let mut m: BTreeMap<Tuple, i64> = BTreeMap::new();
    // A world taller than the copy, so the graft hangs the copy's own root,
    // displacement and all, rather than fusing it into the spine.
    for e in 2_000..20_000u64 {
        let k = Tuple::from([ent(e), Value::Int((e * e % 97) as i64)]);
        t = t.insert(k.clone(), 1);
        m.insert(k, 1);
    }
    for g in 0..96u64 {
        for j in 0..3 {
            let k = Tuple::from([ent(g * 10 + j), Value::Int(0)]);
            t = t.insert(k.clone(), 1);
            m.insert(k, 1);
        }
    }
    t.check();
    let by = 1_000_000i64;
    t = t.graft(&lead(0), &lead(1_000), by).unwrap();
    for (k, v) in m.clone().range(lead(0)..lead(1_000)) {
        m.insert(k.displace(by), *v);
    }
    for g in (0..96u64).rev() {
        let k = Tuple::from([ent(1_000_000 + g * 10 + 1), Value::Int(0)]);
        t = t.remove(&k);
        m.remove(&k);
        t.check();
        assert!(contents(&t) == m, "after cutting run {g} of the copy");
    }
}

#[test]
fn a_remove_that_splits_a_leaf_under_a_displaced_node() {
    // Built by hand: a root over two subtrees, the second displaced by a
    // graft's shift, each holding two full leaves of three-row runs. The first
    // remove inside it overflows a leaf whose parent still carries the shift,
    // so the split's separator must move into the parent's frame.
    let runs = |from: u64| -> Vec<Item<Tuple, i64>> {
        (0..B as u64)
            .map(|g| {
                let first = Tuple::from([ent(from + g * 10), Value::Int(0)]);
                Item::Run(Span { first, stride: Tuple::from([Value::Int(1), Value::Int(0)]), n: 3 }, 1)
            })
            .collect()
    };
    // Thirty-two full leaves under each half: the floor for an inner node.
    let half = |from: u64| {
        let leaves = (0..32u64).map(|i| T::leaf(runs(from + i * 640))).collect();
        T::internal_of((1..32u64).map(|i| lead(from + i * 640)).collect(), leaves)
    };
    let by = 1_000_000i64;
    let t = T::internal_of(vec![lead(by as u64)], vec![half(0), half(0).relocate(by)]);
    t.check();
    let mut m = contents(&t);
    let k = Tuple::from([ent(by as u64 + 5 * 640 + 51), Value::Int(0)]);
    let t = t.remove(&k);
    m.remove(&k);
    t.check();
    assert_eq!(contents(&t), m);
}
