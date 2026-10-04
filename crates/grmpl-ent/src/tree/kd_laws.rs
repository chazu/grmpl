//! **The k-d tree's laws**, against a `BTreeMap` model.
//!
//! Random histories of inserts, removes, grafts, cuts and joins, read through
//! displaced handles and after a round trip through the granfilade. After
//! every step the tree must hold the k-d invariants (`kd_check`) and answer
//! every read exactly as the model does: point, key range, count, measure,
//! existence, as-of, box search, any-column range, iteration and diff. Keys
//! mix arities and value kinds, so splits meet missing cells and text.

use std::collections::BTreeMap;

use grmpl_core::{Entity, Tuple, Value};

use super::{Layout, Tree, B};
use crate::dsp::Displace;
use crate::granfilade::Granfilade;
use crate::measure::{Count, Extent};

type M = (Count, Extent);
type T = Tree<Tuple, i64, M>;
type Model = BTreeMap<Tuple, i64>;

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
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

/// The entity space rows live in, below the target blocks grafts copy into.
const WORLD: u64 = 4_000;

/// A random row: mostly `(from, to)` exits with a scattered second column,
/// some with a number or text between, and a few of arity one.
fn row(rng: &mut Rng) -> Tuple {
    let a = rng.below(WORLD);
    match rng.below(10) {
        0 => Tuple::from([ent(a)]),
        1 => Tuple::from([ent(a), Value::text(["n", "s", "e", "w"][rng.below(4) as usize]), ent(rng.below(WORLD))]),
        2 => Tuple::from([ent(a), Value::Int(rng.below(50) as i64), ent(rng.below(WORLD))]),
        _ => Tuple::from([ent(a), ent(rng.below(WORLD))]),
    }
}

fn lead(n: u64) -> Tuple {
    Tuple::from([ent(n)])
}

fn contents(t: &T) -> Model {
    t.iter().map(|(k, v)| (k, *v)).collect()
}

fn model_range(m: &Model, lo: &Tuple, hi: &Tuple) -> Vec<(Tuple, i64)> {
    if lo >= hi {
        return Vec::new();
    }
    m.range(lo.clone()..hi.clone()).map(|(k, v)| (k.clone(), *v)).collect()
}

fn ent_in(k: &Tuple, col: usize, lo: u64, hi: u64) -> bool {
    matches!(k.as_slice().get(col), Some(Value::Ent(e)) if lo <= e.0 && e.0 < hi)
}

/// Every read the tree answers, against the model, through a handle at `by`.
fn reads_agree(t: &T, m: &Model, rng: &mut Rng, by: i64) {
    let t = t.relocate(by);
    let m: Model = m.iter().map(|(k, v)| (k.displace(by), *v)).collect();
    assert_eq!(t.len(), m.len(), "size");
    assert_eq!(contents(&t), m, "contents");
    let base = (by as u64).wrapping_add(0);
    for _ in 0..8 {
        let probe = Tuple::from([ent(base + rng.below(WORLD)), ent(base + rng.below(WORLD))]);
        let hit = m.keys().nth(rng.below(m.len() as u64 + 1) as usize).cloned().unwrap_or(probe.clone());
        for key in [&probe, &hit] {
            assert_eq!(t.get(key), m.get(key), "get {key:?}");
            assert_eq!(
                t.last_le(key).map(|(k, v)| (k, *v)),
                m.range(..=key.clone()).next_back().map(|(k, v)| (k.clone(), *v)),
                "last_le {key:?}"
            );
        }
        let (a, b) = (rng.below(WORLD), rng.below(WORLD));
        let (lo, hi) = (lead(base + a.min(b)), lead(base + a.max(b)));
        let want = model_range(&m, &lo, &hi);
        assert_eq!(t.range_collect(&lo, &hi), want, "range");
        assert_eq!(t.count_range(&lo, &hi), want.len(), "count");
        assert_eq!(t.measure_range(&lo, &hi).0 .0 as usize, want.len(), "measure");
        assert_eq!(t.any_in(&lo, &hi), !want.is_empty(), "any_in");
        // A box on the scattered column: the k-d layout's reason to exist.
        let (c0, c1) = (base + a.min(b), base + a.min(b) + 1 + rng.below(400));
        let found = t.search(|(_, x)| x.meets(1, c0, c1), |k, _| ent_in(k, 1, c0, c1));
        let want: Vec<(Tuple, i64)> = m.iter().filter(|(k, _)| ent_in(k, 1, c0, c1)).map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(found, want, "search");
        // A range on any column, pruned by the splits on it.
        for col in [1usize, 2] {
            let found = t.kd_range_on(col, &ent(c0), &ent(c1));
            let want: Vec<(Tuple, i64)> =
                m.iter().filter(|(k, _)| ent_in(k, col, c0, c1)).map(|(k, v)| (k.clone(), *v)).collect();
            assert_eq!(found, want, "range on column {col}");
        }
    }
}

fn model_diff(a: &Model, b: &Model) -> Vec<(Tuple, Option<i64>, Option<i64>)> {
    let mut keys: Vec<&Tuple> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| (k.clone(), a.get(k).copied(), b.get(k).copied()))
        .collect()
}

/// Graft `[lo, lo + span)` of the lead column by `by` in the model, or `None`
/// if the target is occupied.
fn model_graft(m: &Model, lo: u64, span: u64, by: i64) -> Option<Model> {
    let src = model_range(m, &lead(lo), &lead(lo + span));
    let (tlo, thi) = (lead(lo).displace(by), lead(lo + span).displace(by));
    if src.is_empty() {
        return Some(m.clone());
    }
    if !model_range(m, &tlo, &thi).is_empty() {
        return None;
    }
    let mut out = m.clone();
    for (k, v) in src {
        out.insert(k.displace(by), v);
    }
    Some(out)
}

#[test]
fn kd_reads_and_writes_agree_with_the_model() {
    for seed in 0..24u64 {
        let mut rng = Rng::new(seed);
        let mut t = T::new();
        let mut m = Model::new();
        let mut versions: Vec<(T, Model)> = Vec::new();
        for round in 0..400 {
            let before = (t.clone(), m.clone());
            match rng.below(20) {
                0..=11 => {
                    let k = row(&mut rng);
                    let v = 1 + rng.below(3) as i64;
                    t = t.kd_insert(k.clone(), v);
                    m.insert(k, v);
                }
                12..=15 => {
                    let k = match m.keys().nth(rng.below(m.len() as u64 + 1) as usize) {
                        Some(k) if rng.below(4) > 0 => k.clone(),
                        _ => row(&mut rng),
                    };
                    t = t.kd_remove(&k);
                    m.remove(&k);
                }
                16 | 17 => {
                    // A graft, often into a fresh block above the world, now and
                    // then onto occupied ground. Shifts are upward: a row's other
                    // entity cells may lie below the block, and the tree, like
                    // the store, relies on no cell wrapping the id space.
                    let (lo, span) = (rng.below(WORLD), 1 + rng.below(300));
                    let by = if rng.below(4) == 0 {
                        rng.below(WORLD - lo) as i64
                    } else {
                        (WORLD * (1 + rng.below(6))) as i64
                    };
                    let got = t.kd_graft(&lead(lo), &lead(lo + span), by);
                    let want = model_graft(&m, lo, span, by);
                    assert_eq!(got.is_some(), want.is_some(), "seed {seed} round {round}: graft refusal");
                    if let (Some(g), Some(w)) = (got, want) {
                        t = g;
                        m = w;
                    }
                }
                18 => {
                    // Cut at an arbitrary key, and join back at a lead pivot.
                    let k = row(&mut rng);
                    let (a, b) = t.kd_split(&k);
                    a.kd_check();
                    b.kd_check();
                    assert_eq!(contents(&a), m.range(..k.clone()).map(|(k, v)| (k.clone(), *v)).collect(), "cut below");
                    assert_eq!(contents(&b), m.range(k.clone()..).map(|(k, v)| (k.clone(), *v)).collect(), "cut above");
                    let p = lead(rng.below(WORLD * 7));
                    let (a, b) = t.kd_split(&p);
                    t = T::kd_join_at(&a, &p, &b);
                }
                _ => {
                    // Rebuild whole, as a scapegoat at the root would.
                    t = T::kd_build(m.iter().map(|(k, v)| (k.clone(), *v)).collect());
                }
            }
            t.kd_check();
            assert_eq!(contents(&t), m, "seed {seed} round {round}: contents");
            let diff = before.0.diff(&t);
            assert_eq!(diff, model_diff(&before.1, &m), "seed {seed} round {round}: diff");
            if round % 25 == 0 {
                let by = if rng.below(2) == 0 { 0 } else { 1 + rng.below(1 << 20) as i64 };
                reads_agree(&t, &m, &mut rng, by);
                versions.push((t.clone(), m.clone()));
            }
        }
        // Any two versions of one history compare exactly.
        for _ in 0..10 {
            let i = rng.below(versions.len() as u64) as usize;
            let j = rng.below(versions.len() as u64) as usize;
            assert_eq!(versions[i].0.diff(&versions[j].0), model_diff(&versions[i].1, &versions[j].1));
        }
    }
}

#[test]
fn kd_trees_round_trip_through_the_granfilade() {
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open(dir.path()).unwrap();
    let mut rng = Rng::new(7);
    let mut t = T::new();
    let mut m = Model::new();
    for _ in 0..3_000 {
        let k = row(&mut rng);
        t = t.kd_insert(k.clone(), 1);
        m.insert(k, 1);
    }
    // A graft, so a stored child carries a dsp.
    t = t.kd_graft(&lead(0), &lead(500), (WORLD * 3) as i64).unwrap();
    m = model_graft(&m, 0, 500, (WORLD * 3) as i64).unwrap();
    let ck = gran.persist(&t).unwrap();
    let back: T = gran.load(ck).unwrap();
    assert!(back.is_kd());
    back.kd_check();
    assert_eq!(contents(&back), m);
    reads_agree(&back, &m, &mut rng, 0);
    // A paged copy edits like a resident one and shares what it did not touch.
    let edited = back.kd_insert(row(&mut rng), 9);
    edited.kd_check();
    assert_eq!(edited.diff(&back).len(), 1);
    let before = gran.frames_encoded();
    gran.persist(&edited).unwrap();
    assert!(gran.frames_encoded() - before < 40, "an insert rewrites its path, not the tree");
}

/// Split levels above the deepest leaf.
fn height(t: &T) -> usize {
    super::kd::kd_height_of(t)
}

#[test]
fn kd_depth_stays_logarithmic_under_sorted_and_random_inserts() {
    for sorted in [true, false] {
        let mut rng = Rng::new(3);
        let mut t = T::new();
        let n = 20_000u64;
        for i in 0..n {
            let a = if sorted { i } else { rng.below(1 << 40) };
            t = t.kd_insert(Tuple::from([ent(a), ent(rng.below(1 << 30))]), 1);
        }
        t.kd_check();
        let leaves = (2 * n as usize / B).max(1);
        let balanced = usize::BITS as usize - leaves.leading_zeros() as usize;
        assert!(height(&t) <= 3 * balanced, "sorted={sorted}: height {} for {} leaves", height(&t), leaves);
    }
}

#[test]
fn grafts_keep_the_lead_levels_balanced_and_share_the_copy() {
    // Many instances of one template, each into the next block up: the joins
    // must rotate, or the oldest instances would sink one level per graft.
    let mut t = T::new();
    for i in 0..2_000u64 {
        t = t.kd_insert(Tuple::from([ent(i % 200), ent(i)]), 1);
    }
    let template = t.clone();
    for k in 1..=200u64 {
        t = t.kd_graft(&lead(0), &lead(200), (200 * k) as i64).unwrap();
    }
    t.kd_check();
    assert_eq!(t.len(), 2_000 * 201);
    assert!(height(&t) <= height(&template) + 24, "height {} after 200 grafts", height(&t));
    // The last copy still shares the template's nodes: persisting it after the
    // template writes only the new spine.
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open(dir.path()).unwrap();
    gran.persist(&template).unwrap();
    let before = gran.frames_encoded();
    let one = template.kd_graft(&lead(0), &lead(200), 1_000_000).unwrap();
    gran.persist(&one).unwrap();
    assert!(gran.frames_encoded() - before < 20, "a graft writes {} frames", gran.frames_encoded() - before);
}

#[test]
fn the_layout_tag_round_trips() {
    for l in [Layout::Ordered, Layout::Kd] {
        assert_eq!(Layout::from_tag(l.tag()), Some(l));
    }
    assert_eq!(Layout::from_tag(9), None);
}

#[test]
fn a_large_copy_edits_through_its_displacement() {
    // A copy big enough that its root is a split, hung under a dsp: every
    // remove and insert inside it must move its keys through that dsp.
    let mut rng = Rng::new(11);
    let mut t = T::new();
    let mut m = Model::new();
    for i in 0..3_000u64 {
        let k = Tuple::from([ent(i % 1_000), ent(rng.below(1_000))]);
        t = t.kd_insert(k.clone(), 1);
        m.insert(k, 1);
    }
    let by = (WORLD * 2) as i64;
    t = t.kd_graft(&lead(0), &lead(1_000), by).unwrap();
    m = model_graft(&m, 0, 1_000, by).unwrap();
    let copied: Vec<Tuple> = m.keys().filter(|k| *k >= &lead(WORLD * 2)).cloned().collect();
    assert!(copied.len() > 4 * B, "the copy spans several splits");
    for (i, k) in copied.iter().enumerate() {
        if i % 3 == 0 {
            t = t.kd_remove(k);
            m.remove(k);
        } else if i % 3 == 1 {
            let fresh = Tuple::from([ent(WORLD * 2 + rng.below(1_000)), ent(WORLD * 2 + rng.below(1_000))]);
            t = t.kd_insert(fresh.clone(), 2);
            m.insert(fresh, 2);
        }
        if i % 50 == 0 {
            t.kd_check();
            assert_eq!(contents(&t), m, "after {i} edits in the copy");
        }
    }
    t.kd_check();
    assert_eq!(contents(&t), m);
}
