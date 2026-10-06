//! **Version compare across a graft: the copy is recognized, not re-read.**
//!
//! A graft's join rebuilds the spines above the copy, so two versions either
//! side of it share every subtree but few separators. `Tree::diff` walks each
//! version as a frontier of whole subtrees and drops a subtree both sides
//! share at the same position, whatever was rebuilt above it.
//! `EntStore::compare_spans` goes further and names each copy by its span,
//! from the spanfilade, instead of listing its rows. Pinned here:
//!
//! 1. **`diff` is exact for any two shapes**: trees built by different
//!    histories, with stale separators, relocated, and grafted.
//! 2. **The span law**: replaying `compare_spans(a, b)` onto `a` rebuilds `b`,
//!    over random histories of edits, grafts, chains of grafts and blocks
//!    cleared and refilled; without a graft in `(a, b]` its rows are `compare`.
//! 3. **The cost**: on a reopened store, a compare across a graft pages a
//!    number of frames that does not grow with the relation, and
//!    `compare_spans` one that does not grow with the copy either. An edit
//!    that splits nodes costs its leaves, and a one-row edit one path.

use std::collections::BTreeMap;

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{Count, EntStore, Layout, Tree};

const R: RelId = RelId(1);

type T = Tree<Tuple, Diff, Count>;

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn row(e: u64, tag: i64) -> Tuple {
    Tuple::from([ent(e), Value::Int(tag)])
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

fn model_diff(a: &BTreeMap<Tuple, Diff>, b: &BTreeMap<Tuple, Diff>) -> Vec<(Tuple, Option<Diff>, Option<Diff>)> {
    let keys: std::collections::BTreeSet<&Tuple> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter_map(|k| {
            let (x, y) = (a.get(k).copied(), b.get(k).copied());
            (x != y).then(|| (k.clone(), x, y))
        })
        .collect()
}

fn contents(t: &T) -> BTreeMap<Tuple, Diff> {
    t.iter().map(|(k, v)| (k, *v)).collect()
}

#[test]
fn diff_is_exact_between_any_two_shapes() {
    // Keys sit mid-space, so relocations and grafts move them both ways.
    const BASE: u64 = 100_000;
    for seed in 1..=40u64 {
        let mut rng = Rng(seed);
        // Two lineages over overlapping content, built in different orders and
        // thinned by removals, so their separators go stale differently.
        let mut versions: Vec<T> = Vec::new();
        for lineage in 0..2 {
            let mut t = T::new();
            let n = 200 + rng.below(1_500);
            for _ in 0..n {
                t = t.insert(row(BASE + rng.below(3_000), lineage), 1 + rng.below(3) as i64);
            }
            for _ in 0..rng.below(n) {
                t = t.remove(&row(BASE + rng.below(3_000), lineage));
            }
            versions.push(t.clone());
            // Later versions of the lineage: edits, relocations and grafts,
            // up and down.
            for _ in 0..5 {
                let sign = if rng.below(2) == 0 { 1 } else { -1 };
                t = match rng.below(4) {
                    0 => t.insert(row(BASE + rng.below(3_000), lineage), 9),
                    1 => t.remove(&row(BASE + rng.below(3_000), lineage)),
                    2 => {
                        let lo = BASE + rng.below(3_000);
                        let span = (Tuple::from([ent(lo)]), Tuple::from([ent(lo + 1 + rng.below(400))]));
                        let by = sign * (5_000 + rng.below(15_000) as i64);
                        t.graft(&span.0, &span.1, by).unwrap_or(t)
                    }
                    _ => t.relocate(sign * [7, 14, 1_500][rng.below(3) as usize]),
                };
                versions.push(t.clone());
            }
        }
        for a in &versions {
            for b in &versions {
                assert_eq!(a.diff(b), model_diff(&contents(a), &contents(b)), "seed {seed}");
            }
        }
    }
}

#[test]
fn one_node_at_two_positions_is_two_sets_of_rows() {
    let t: T = (0..500u64).fold(T::new(), |t, e| t.insert(row(e, 0), 1));
    // Every node of `moved` is a node of `t`, at another position.
    let moved = t.relocate(1_000);
    let d = t.diff(&moved);
    assert_eq!(d.len(), 1_000);
    assert_eq!(d, model_diff(&contents(&t), &contents(&moved)));
}

/// A history of edits and grafts over blocks of 100 entities, with a model of
/// the relation at every edition.
fn history(store: &EntStore, seed: u64) -> Vec<(Edition, BTreeMap<Tuple, Diff>)> {
    let mut rng = Rng(seed);
    let mut now: BTreeMap<Tuple, Diff> = BTreeMap::new();
    let mut eds = vec![(store.current(), now.clone())];
    let block_of = |t: &Tuple| match t.as_slice()[0] {
        Value::Ent(Entity(e)) => e / 100,
        _ => unreachable!(),
    };
    for _ in 0..60 {
        match rng.below(5) {
            // Graft a block into an empty one, so chains of copies arise.
            0 | 1 => {
                let (src, dst) = (rng.below(8), rng.below(8));
                let occupied = |b: u64| now.keys().any(|t| block_of(t) == b);
                if src == dst || !occupied(src) || occupied(dst) {
                    continue;
                }
                let shift = (dst as i64 - src as i64) * 100;
                let at = store.instance_template(&[R], src * 100, src * 100 + 100, shift).unwrap();
                let copied: Vec<(Tuple, Diff)> = now
                    .iter()
                    .filter(|(t, _)| block_of(t) == src)
                    .map(|(t, d)| {
                        let Value::Ent(Entity(e)) = t.as_slice()[0] else { unreachable!() };
                        (Tuple::from([ent((e as i64 + shift) as u64), t.as_slice()[1].clone()]), *d)
                    })
                    .collect();
                now.extend(copied);
                eds.push((at, now.clone()));
            }
            // Clear a whole block, so a later copy may land where rows were.
            2 => {
                let b = rng.below(8);
                let gone: Vec<Tuple> = now.keys().filter(|t| block_of(t) == b).cloned().collect();
                if gone.is_empty() {
                    continue;
                }
                let ups: Vec<_> = gone.iter().map(|t| (R, t.clone(), -now[t])).collect();
                for t in &gone {
                    now.remove(t);
                }
                eds.push((store.commit(&ups).unwrap(), now.clone()));
            }
            // Ordinary edits, some inside copies.
            _ => {
                let mut ups = Vec::new();
                for _ in 0..1 + rng.below(5) {
                    let t = row(rng.below(800), rng.below(3) as i64);
                    if ups.iter().any(|(_, u, _)| *u == t) {
                        continue;
                    }
                    match now.get(&t).copied() {
                        Some(d) => {
                            now.remove(&t);
                            ups.push((R, t, -d));
                        }
                        None => {
                            now.insert(t.clone(), 1);
                            ups.push((R, t, 1));
                        }
                    }
                }
                eds.push((store.commit(&ups).unwrap(), now.clone()));
            }
        }
    }
    eds
}

/// Rebuild the relation at `b` from the one at `a` and `compare_spans(a, b)`.
fn replay(store: &EntStore, a: &BTreeMap<Tuple, Diff>, b: Edition, from: Edition) -> BTreeMap<Tuple, Diff> {
    let cmp = store.compare_spans(R, from, b).unwrap();
    let mut out = a.clone();
    for g in &cmp.copies {
        let (tlo, thi) = (Tuple::from([ent(g.target.0)]), Tuple::from([ent(g.target.1)]));
        out.retain(|t, _| !(tlo <= *t && *t < thi));
        let source = store
            .range_at(R, Edition(g.edition.0 - 1), &Tuple::from([ent(g.source.0)]), &Tuple::from([ent(g.source.1)]))
            .unwrap();
        for (t, d) in source {
            let Value::Ent(Entity(e)) = t.as_slice()[0] else { unreachable!() };
            out.insert(Tuple::from([ent((e as i64 + g.shift()) as u64), t.as_slice()[1].clone()]), d);
        }
    }
    for (t, before, after) in cmp.rows {
        assert_eq!(out.get(&t).copied(), before, "a row's weight before disagrees with the spliced base");
        match after {
            Some(d) => out.insert(t, d),
            None => out.remove(&t),
        };
    }
    out
}

#[test]
fn replaying_a_span_compare_rebuilds_the_later_edition() {
    for seed in 1..=12u64 {
        let store = EntStore::new();
        let eds = history(&store, seed);
        let mut rng = Rng(seed ^ 0x5EED);
        for _ in 0..80 {
            let (i, j) = (rng.below(eds.len() as u64) as usize, rng.below(eds.len() as u64) as usize);
            let ((a, ma), (b, mb)) = (&eds[i.min(j)], &eds[i.max(j)]);
            assert_eq!(&replay(&store, ma, *b, *a), mb, "seed {seed} ({a:?}, {b:?}]");
            // The row form is the plain diff, copies included.
            let rows = store.compare(R, *a, *b).unwrap();
            let want: Vec<_> = model_diff(ma, mb)
                .into_iter()
                .map(|(t, x, y)| (t, x.unwrap_or(0), y.unwrap_or(0)))
                .collect();
            assert_eq!(rows, want, "seed {seed} ({a:?}, {b:?}]");
            let cmp = store.compare_spans(R, *a, *b).unwrap();
            if cmp.copies.is_empty() {
                assert_eq!(cmp.rows, model_diff(ma, mb), "seed {seed}: no copy, so the rows are the diff");
            }
        }
    }
}

#[test]
fn a_copy_edited_afterwards_reports_only_the_edit() {
    let store = EntStore::new();
    let rows: Vec<_> = (0..1_000u64).map(|e| (R, row(e, 0), 1)).collect();
    store.commit(&rows).unwrap();
    let a = store.current();
    store.instance_template(&[R], 0, 1_000, 50_000).unwrap();
    let b = store.commit(&[(R, row(50_007, 0), -1), (R, row(50_007, 1), 1)]).unwrap();
    let cmp = store.compare_spans(R, a, b).unwrap();
    assert_eq!(cmp.copies.len(), 1);
    assert_eq!((cmp.copies[0].source, cmp.copies[0].target), ((0, 1_000), (50_000, 51_000)));
    assert_eq!(cmp.rows, vec![(row(50_007, 0), Some(1), None), (row(50_007, 1), None, Some(1))]);
}

/// Frames paged by a row compare and by a span compare across one graft of
/// `copy` rows into a relation of `n` other rows, on a reopened store.
fn frames(n: u64, copy: u64) -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = {
        let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        // These bounds are the B+ layout's, so they pin it; `kd_layout.rs`
        // has the k-d layout's.
        store.set_default_layout(Layout::Ordered).unwrap();
        let ids: Vec<u64> = (0..n).chain(1_000_000..1_000_000 + copy).collect();
        // Irregular tags, so no run folds the rows: these bounds are about
        // copies the compare must read row by row.
        for chunk in ids.chunks(5_000) {
            store.commit(&chunk.iter().map(|&e| (R, row(e, (e * e % 97) as i64), 1)).collect::<Vec<_>>()).unwrap();
        }
        let a = store.current();
        (a, store.instance_template(&[R], 1_000_000, 1_000_000 + copy, 4_000_000).unwrap())
    };
    let measure = |f: &dyn Fn(&EntStore)| {
        let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        let before = store.frames_paged();
        f(&store);
        store.frames_paged() - before
    };
    let rows = measure(&|s| assert_eq!(s.compare(R, a, b).unwrap().len() as u64, copy));
    let spans = measure(&|s| assert!(s.compare_spans(R, a, b).unwrap().rows.is_empty()));
    (rows, spans)
}

#[test]
fn a_compare_across_a_graft_reads_the_copy_not_the_relation() {
    let (small, _) = frames(2_000, 1_000);
    let (large, _) = frames(100_000, 1_000);
    // The relation grew fifty-fold; the compare reads the copy and the seams.
    assert!(large <= small + 12, "row compare paged {small} frames at 2k rows, {large} at 100k");
    assert!(large <= 60, "row compare across a 1000-row graft paged {large} frames");
}

#[test]
fn a_span_compare_reads_neither_the_copy_nor_the_relation() {
    let (rows_small, small) = frames(100_000, 1_000);
    let (rows_large, large) = frames(100_000, 20_000);
    // The copy grew twenty-fold: the row compare reads it, the span compare
    // reads the seams.
    assert!(rows_large > 10 * rows_small / 2, "row compare: {rows_small} then {rows_large} frames");
    assert!(large <= small + 12, "span compare paged {small} frames for a 1k copy, {large} for 20k");
    assert!(large <= 60, "span compare across a 20k-row graft paged {large} frames");
}

#[test]
fn a_compare_reads_the_edit_however_the_spines_were_rebuilt() {
    // A relation thinned by removals, so its separators are stale, then three
    // edits: one row, forty scattered rows, and 3000 rows dense enough to
    // split nodes and rebuild spines.
    const N: u64 = 30_000;
    let dir = tempfile::tempdir().unwrap();
    let eds = {
        let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        store.set_default_layout(Layout::Ordered).unwrap();
        let ids: Vec<u64> = (0..N).map(|i| i * 3).collect();
        for chunk in ids.chunks(5_000) {
            store.commit(&chunk.iter().map(|&e| (R, row(e, 0), 1)).collect::<Vec<_>>()).unwrap();
        }
        let thin: Vec<_> = (0..N).step_by(7).map(|i| (R, row(i * 3, 0), -1)).collect();
        let e0 = store.commit(&thin).unwrap();
        let e1 = store.commit(&[(R, row(45_001, 0), 1)]).unwrap();
        let e2 = store.commit(&(0..40).map(|i| (R, row(i * 2_011 + 1, 0), 1)).collect::<Vec<_>>()).unwrap();
        let e3 = store.commit(&(0..3_000).map(|i| (R, row(i * 3 + 1, 0), 1)).collect::<Vec<_>>()).unwrap();
        [e0, e1, e2, e3]
    };
    let cold = |a: Edition, b: Edition, rows: usize| {
        let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        let before = store.frames_paged();
        assert_eq!(store.compare(R, a, b).unwrap().len(), rows);
        store.frames_paged() - before
    };
    let (one, forty, dense) = (cold(eds[0], eds[1], 1), cold(eds[1], eds[2], 40), cold(eds[2], eds[3], 3_000));
    // Compared the other way, the row is a removal.
    let back = cold(eds[1], eds[0], 1);
    // One path in each version: the root, two internal nodes and the leaf.
    assert!(one <= 8 && back <= 8, "a one-row compare paged {one} frames, {back} backwards");
    // The forty rows' paths, sharing their upper levels.
    assert!(forty <= 120, "a forty-row compare paged {forty} frames");
    // The leaves the rows land in on each side, not the relation's ~600 a
    // side, which is what a descent that pairs only equal separators reads.
    assert!(dense <= 300, "a 3000-row compare paged {dense} frames");
}
