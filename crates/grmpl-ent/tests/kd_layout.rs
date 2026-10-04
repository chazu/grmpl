//! **The k-d layout at the store** (fidelity gap G8).
//!
//! A relation laid out as k-d trees must answer exactly as one laid out as B+
//! trees: the layout changes every tree's shape and nothing anyone observes.
//! So the first law runs one random history, of commits, grafts, forks and
//! reopens, into a store of each layout and asks both every question the store
//! answers. The B+ store is the reference: its own laws are stated absolutely
//! in `store_laws.rs`, and the whole conformance suite runs on both.
//!
//! The rest pin what the layout changes, on cold stores:
//!
//! * a box on a scattered column prunes, where the B+ layout reads every leaf;
//! * a range on any column needs no Arrangement;
//! * a read on the lead column costs a binary tree's depth when the other
//!   columns track it, and about the square root of the leaves when they are
//!   scattered: the price of pruning on every column;
//! * a compare across a graft reads the copy and the seams;
//! * grafts share the template, and the layout is fixed once written.

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, Layout, MergeOutcome};

const EXITS: RelId = RelId(1);
const NAMES: RelId = RelId(2);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn exit(from: u64, way: i64, to: u64) -> Tuple {
    Tuple::from([ent(from), Value::Int(way), ent(to)])
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

/// Rooms live in blocks of `BLOCK` ids; a template's exits stay inside its
/// block, so it can be instanced.
const BLOCK: u64 = 100;
const BLOCKS: u64 = 20;

fn store(dir: &std::path::Path, layout: Layout) -> EntStore {
    let s = EntStore::open(dir).unwrap();
    s.set_default_layout(layout).unwrap();
    s
}

/// Every read the store answers about `rel` at `at`, as one comparable value.
fn answers(s: &EntStore, at: Edition, rng: &mut Rng) -> Vec<String> {
    let mut out = Vec::new();
    for rel in [EXITS, NAMES] {
        out.push(format!("{:?}", s.read_at(rel, at).unwrap()));
        out.push(format!("{:?}", s.scan_updates(rel, s.watermark(), at).unwrap()));
    }
    for _ in 0..4 {
        let (a, b) = (rng.below(BLOCK * BLOCKS * 4), rng.below(BLOCK * BLOCKS * 4));
        let (lo, hi) = (a.min(b), a.max(b) + 1);
        let (tlo, thi) = (Tuple::from([ent(lo)]), Tuple::from([ent(hi)]));
        out.push(format!("{:?}", s.range_at(EXITS, at, &tlo, &thi).unwrap()));
        out.push(format!("{:?}", s.count_at(EXITS, at, &tlo, &thi).unwrap()));
        out.push(format!("{:?}", s.search_at(EXITS, at, &[(2, lo, hi)]).unwrap()));
        out.push(format!("{:?}", s.search_at(EXITS, at, &[(0, lo, hi), (2, lo / 2, hi)]).unwrap()));
        let mut on = s.read_range_on(EXITS, at, 2, &ent(lo), &ent(hi)).unwrap();
        on.sort();
        out.push(format!("{on:?}"));
        let mut on = s.read_range_on(EXITS, at, 1, &Value::Int(1), &Value::Int(3)).unwrap();
        on.sort();
        out.push(format!("{on:?}"));
        let mut found = s.lookup(EXITS, at, 2, &[ent(lo), ent(hi)]).unwrap();
        found.sort();
        out.push(format!("{found:?}"));
    }
    out
}

#[test]
fn a_kd_world_answers_as_the_ordered_one_does() {
    for seed in 0..6u64 {
        let mut rng = Rng::new(seed);
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let layouts = [Layout::Ordered, Layout::Kd];
        let mut stores: Vec<EntStore> = dirs.iter().zip(layouts).map(|(d, l)| store(d.path(), l)).collect();
        let mut editions = Vec::new();
        for round in 0..60 {
            match rng.below(10) {
                0..=6 => {
                    // A commit of exits and names, adding and retracting.
                    let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
                    for _ in 0..1 + rng.below(80) {
                        let b = rng.below(BLOCKS);
                        let from = b * BLOCK + rng.below(BLOCK);
                        let to = b * BLOCK + rng.below(BLOCK);
                        let diff = if rng.below(5) == 0 { -1 } else { 1 };
                        ups.push((EXITS, exit(from, rng.below(4) as i64, to), diff));
                        if rng.below(3) == 0 {
                            ups.push((NAMES, Tuple::from([ent(from), Value::text(format!("r{from}"))]), 1));
                        }
                    }
                    for s in &stores {
                        s.commit(&ups).unwrap();
                    }
                }
                7 | 8 => {
                    // An instance of one block into a fresh one above the world,
                    // or onto an occupied one, refused alike.
                    let b = rng.below(BLOCKS);
                    let to = if rng.below(4) == 0 { rng.below(BLOCKS) } else { BLOCKS + rng.below(3 * BLOCKS) };
                    let shift = (to * BLOCK) as i64 - (b * BLOCK) as i64;
                    let got: Vec<bool> = stores
                        .iter()
                        .map(|s| s.instance_template(&[EXITS, NAMES], b * BLOCK, (b + 1) * BLOCK, shift).is_ok())
                        .collect();
                    assert_eq!(got[0], got[1], "seed {seed} round {round}: instancing agrees");
                }
                _ => {
                    // Reopen both: every node now starts on disk.
                    drop(std::mem::take(&mut stores));
                    stores = dirs.iter().map(|d| EntStore::open(d.path()).unwrap()).collect();
                    assert_eq!(stores[1].layout(EXITS), Layout::Kd, "the layout survives a reopen");
                }
            }
            assert_eq!(stores[0].current(), stores[1].current());
            editions.push(stores[0].current());
            let at = stores[0].current();
            let probe = rng.next();
            let (a, b) = (answers(&stores[0], at, &mut Rng::new(probe)), answers(&stores[1], at, &mut Rng::new(probe)));
            assert_eq!(a, b, "seed {seed} round {round}: the layouts disagree");
        }
        // Compares between any two editions, by row and by span.
        for _ in 0..12 {
            let (x, y) = (editions[rng.below(editions.len() as u64) as usize], editions[rng.below(editions.len() as u64) as usize]);
            let (a, b) = (x.min(y), x.max(y));
            for rel in [EXITS, NAMES] {
                assert_eq!(stores[0].compare(rel, a, b).unwrap(), stores[1].compare(rel, a, b).unwrap());
                let (sa, sb) = (stores[0].compare_spans(rel, a, b).unwrap(), stores[1].compare_spans(rel, a, b).unwrap());
                assert_eq!(sa.rows, sb.rows, "span compare rows {a:?}..{b:?}");
                assert_eq!(sa.copies, sb.copies, "span compare copies {a:?}..{b:?}");
            }
        }
        // A fork into the past, and a merge of a fork back.
        let at = editions[editions.len() / 2];
        let forks: Vec<EntStore> = stores.iter().map(|s| s.fork_at(at).unwrap()).collect();
        assert_eq!(forks[1].layout(EXITS), Layout::Kd, "a fork keeps the layout");
        for f in &forks {
            f.commit(&[(EXITS, exit(1, 0, 2), 1), (NAMES, Tuple::from([ent(1), Value::text("forked")]), 1)]).unwrap();
        }
        let merged: Vec<EntStore> = stores
            .iter()
            .zip(&forks)
            .map(|(s, f)| match s.merge(f).unwrap() {
                MergeOutcome::Merged(m) => m,
                MergeOutcome::Conflict(c) => panic!("seed {seed}: merge conflict {c:?}"),
            })
            .collect();
        assert_eq!(merged[1].layout(EXITS), Layout::Kd, "a merge keeps the layout");
        let probe = rng.next();
        assert_eq!(
            answers(&merged[0], merged[0].current(), &mut Rng::new(probe)),
            answers(&merged[1], merged[1].current(), &mut Rng::new(probe)),
            "seed {seed}: merged worlds disagree"
        );
    }
}

#[test]
fn the_layout_is_chosen_before_the_first_write_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = EntStore::open(dir.path()).unwrap();
        assert_eq!(s.layout(EXITS), Layout::default());
        s.set_layout(EXITS, Layout::Kd).unwrap();
        s.set_layout(NAMES, Layout::Ordered).unwrap();
        s.commit(&[(EXITS, exit(1, 0, 2), 1), (NAMES, Tuple::from([ent(1), Value::text("a")]), 1)]).unwrap();
        // Written relations keep their layout: the same choice is a no-op, any
        // other is refused, and so is a default that would move them.
        s.set_layout(EXITS, Layout::Kd).unwrap();
        assert!(s.set_layout(EXITS, Layout::Ordered).is_err());
        assert!(s.set_layout(NAMES, Layout::Kd).is_err());
        // Every written relation is laid out by name, so the default may move.
        s.set_default_layout(Layout::Kd).unwrap();
        // A relation laid out but never written is still free.
        s.set_layout(RelId(9), Layout::Kd).unwrap();
        s.set_layout(RelId(9), Layout::Ordered).unwrap();
    }
    // Durable: a reopen sees the choices without any further commit.
    let s = EntStore::open(dir.path()).unwrap();
    assert_eq!(s.layout(EXITS), Layout::Kd);
    assert_eq!(s.layout(NAMES), Layout::Ordered);
    assert_eq!(s.layout(RelId(9)), Layout::Ordered);
    // A fork carries the layouts, and a merge unites them.
    let fork = s.fork_at(s.current()).unwrap();
    fork.set_layout(RelId(10), Layout::Kd).unwrap();
    fork.commit(&[(RelId(10), exit(5, 0, 6), 1)]).unwrap();
    assert_eq!(fork.layout(EXITS), Layout::Kd);
    let MergeOutcome::Merged(m) = s.merge(&fork).unwrap() else { panic!("merge conflict") };
    assert_eq!(m.layout(RelId(10)), Layout::Kd);
    assert_eq!(m.read_at(RelId(10), m.current()).unwrap().len(), 1);
}

/// A durable world of `rooms` rooms with four exits each to `to(room, way)`,
/// in `layout`, reopened so every node starts on disk.
fn reopened(dir: &std::path::Path, layout: Layout, rooms: u64, to: impl Fn(u64, u64) -> u64) -> EntStore {
    {
        let s = store(dir, layout);
        for chunk in (0..rooms).collect::<Vec<_>>().chunks(1_000) {
            let ups: Vec<_> =
                chunk.iter().flat_map(|&r| (0..4).map(move |w| (r, w))).map(|(r, w)| (EXITS, exit(r, w as i64, to(r, w)), 1)).collect();
            s.commit(&ups).unwrap();
        }
    }
    EntStore::open(dir).unwrap()
}

/// Frames a read pages in on a cold store.
fn paged<T>(s: &EntStore, read: impl FnOnce(&EntStore) -> T) -> (T, u64) {
    let before = s.frames_paged();
    let out = read(s);
    (out, s.frames_paged() - before)
}

const ROOMS: u64 = 20_000;

fn scattered(r: u64, w: u64) -> u64 {
    (r * 7_919 + w * 104_729) % ROOMS
}

#[test]
fn a_scattered_column_prunes_in_the_kd_layout() {
    let (od, kd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (o, k) = (reopened(od.path(), Layout::Ordered, ROOMS, scattered), reopened(kd.path(), Layout::Kd, ROOMS, scattered));
    let at = o.current();
    let (rows_o, frames_o) = paged(&o, |s| s.search_at(EXITS, at, &[(2, 10_000, 10_010)]).unwrap());
    let (rows_k, frames_k) = paged(&k, |s| s.search_at(EXITS, at, &[(2, 10_000, 10_010)]).unwrap());
    assert_eq!(rows_o, rows_k);
    assert_eq!(rows_k.len(), 40);
    // The B+ tree reads most of its leaves; the k-d tree splits on the
    // scattered column too, so its extents are tight and it reads a few paths.
    let leaves = ROOMS * 4 / 64;
    assert!(frames_o >= leaves / 2, "the B+ layout paged only {frames_o} frames");
    assert!(frames_k <= frames_o / 10, "the k-d layout paged {frames_k} frames against {frames_o}");
}

#[test]
fn a_range_on_any_column_needs_no_arrangement() {
    let (od, kd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (o, k) = (reopened(od.path(), Layout::Ordered, ROOMS, scattered), reopened(kd.path(), Layout::Kd, ROOMS, scattered));
    let at = o.current();
    // Below the present a B+ relation has no Arrangement to use and filters;
    // at the present it builds one, reading the relation. The k-d tree answers
    // both from its own splits on the column.
    let past = Edition(at.0 - 1);
    let read = |s: &EntStore, at: Edition| {
        let mut rows = s.read_range_on(EXITS, at, 2, &ent(5_000), &ent(5_020)).unwrap();
        rows.sort();
        rows
    };
    for at in [past, at] {
        let (want, frames_o) = paged(&o, |s| read(s, at));
        let (got, frames_k) = paged(&k, |s| read(s, at));
        assert_eq!(got, want);
        assert!(frames_k * 10 <= frames_o, "{at:?}: k-d paged {frames_k} frames, B+ {frames_o}");
    }
}

#[test]
fn a_lead_column_read_pays_for_the_other_columns() {
    // One room's four exits, read by the lead column.
    let read = |s: &EntStore| {
        let at = s.current();
        let (lo, hi) = (Tuple::from([ent(12_345)]), Tuple::from([ent(12_346)]));
        paged(s, |s| s.range_at(EXITS, at, &lo, &hi).unwrap())
    };
    let near = |r: u64, w: u64| r + w + 1;
    let mut frames = Vec::new();
    for to in [&near as &dyn Fn(u64, u64) -> u64, &scattered] {
        let (od, kd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (o, k) = (reopened(od.path(), Layout::Ordered, ROOMS, to), reopened(kd.path(), Layout::Kd, ROOMS, to));
        let ((want, fo), (got, fk)) = (read(&o), read(&k));
        assert_eq!(got, want);
        assert_eq!(got.len(), 4);
        frames.push((fo, fk));
    }
    let [(near_o, near_k), (far_o, far_k)] = frames[..] else { unreachable!() };
    // A B+ tree of 80k rows is three levels deep, whatever the other columns.
    assert!(near_o <= 12 && far_o <= 12, "B+ lead reads paged {near_o} and {far_o}");
    // When the destination tracks the room, the k-d tree splits almost only on
    // the lead column, and a read costs a binary tree's depth.
    assert!(near_k <= near_o + 40, "k-d lead read, correlated, paged {near_k}");
    // When it is scattered, half the splits are on the destination, and a read
    // on the lead column enters both sides of each: about the square root of
    // the leaves, the k-d tree's price for pruning on every column.
    assert!(far_k >= 10 * far_o, "k-d lead read, scattered, paged only {far_k}");
    assert!(far_k <= 1_000, "k-d lead read, scattered, paged {far_k}");
}

/// Frames a row compare pages across one graft of `copy` rows into `n` rows.
fn graft_compare_frames(n: u64, copy: u64) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = {
        let s = store(dir.path(), Layout::Kd);
        let ids: Vec<u64> = (0..n).chain(1_000_000..1_000_000 + copy).collect();
        for chunk in ids.chunks(5_000) {
            s.commit(&chunk.iter().map(|&e| (EXITS, Tuple::from([ent(e), Value::Int(0)]), 1)).collect::<Vec<_>>()).unwrap();
        }
        let a = s.current();
        (a, s.instance_template(&[EXITS], 1_000_000, 1_000_000 + copy, 4_000_000).unwrap())
    };
    let s = EntStore::open(dir.path()).unwrap();
    let (rows, frames) = paged(&s, |s| s.compare(EXITS, a, b).unwrap());
    assert_eq!(rows.len() as u64, copy);
    frames
}

#[test]
fn a_compare_across_a_graft_reads_the_copy_and_its_seams() {
    let (small, large) = (graft_compare_frames(2_000, 1_000), graft_compare_frames(100_000, 1_000));
    // Fifty times the relation adds the extra depth, not the relation.
    assert!(large <= small + 60, "k-d row compare paged {small} frames at 2k rows, {large} at 100k");
    assert!(large <= 160, "k-d row compare across a 1000-row graft paged {large} frames");
}

#[test]
fn grafts_share_the_template_in_the_kd_layout() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Layout::Kd);
    // Twenty rooms per block, each with exits inside it, in a world of blocks.
    let mut ups = Vec::new();
    for b in 0..50u64 {
        for r in 0..20 {
            for w in 0..3 {
                ups.push((EXITS, exit(b * BLOCK + r, w, b * BLOCK + (r + w as u64 + 1) % 20), 1));
            }
        }
    }
    s.commit(&ups).unwrap();
    let template_at = s.current();
    let before = s.stored_nodes().unwrap();
    for k in 1..=10u64 {
        s.instance_template(&[EXITS], 0, BLOCK, (100 * BLOCK * k) as i64).unwrap();
    }
    // Ten 60-row copies add their seams, not 600 rows of nodes.
    let added = s.stored_nodes().unwrap() - before;
    assert!(added <= 10 * 40, "ten instances added {added} nodes");
    let (lo, hi) = (Tuple::from([ent(0)]), Tuple::from([ent(BLOCK)]));
    let found = s.backfollow(EXITS, template_at, &lo, &hi).unwrap();
    for k in 1..=10i64 {
        let rows: usize = found.iter().filter(|h| h.shift == 100 * BLOCK as i64 * k).map(|h| h.rows).max().unwrap_or(0);
        assert!(rows > 0, "instance {k} shares nothing with its template");
    }
}

#[test]
fn a_compare_into_fresh_space_is_placed_by_the_extents() {
    // A template of local exits instanced above a world of scattered ones.
    // The new copy hangs beside the old tree under a lead-column split, and
    // the old tree's root splits on the destination, so nothing above it
    // bounds its lead column: only its extent places it below the copy.
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = {
        let s = reopened(dir.path(), Layout::Kd, ROOMS, scattered);
        let (lo, hi) = (ROOMS, ROOMS + 250);
        let ups: Vec<_> =
            (lo..hi).flat_map(|r| (0..4).map(move |w| (EXITS, exit(r, w, lo + (r + w as u64) % 250), 1))).collect();
        s.commit(&ups).unwrap();
        let a = s.current();
        (a, s.instance_template(&[EXITS], lo, hi, 10_000_000).unwrap())
    };
    let s = EntStore::open(dir.path()).unwrap();
    let (rows, frames) = paged(&s, |s| s.compare(EXITS, a, b).unwrap());
    assert_eq!(rows.len(), 1_000);
    assert!(frames <= 150, "a compare across a graft into fresh space paged {frames} frames");
}

#[test]
fn a_compare_without_extents_is_placed_by_the_splits_above() {
    // Numbers have no extent, so only the bounds each piece inherits from its
    // ancestors' splits keep a one-row compare from reading the relation.
    const NUMS: RelId = RelId(3);
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = {
        let s = store(dir.path(), Layout::Kd);
        let mut rng = Rng::new(5);
        let rows: Vec<_> =
            (0..50_000i64).map(|i| (NUMS, Tuple::from([Value::Int(i), Value::Int(rng.below(1 << 30) as i64)]), 1)).collect();
        for chunk in rows.chunks(5_000) {
            s.commit(chunk).unwrap();
        }
        let a = s.current();
        (a, s.commit(&[(NUMS, Tuple::from([Value::Int(25_000), Value::Int(7)]), 1)]).unwrap())
    };
    let s = EntStore::open(dir.path()).unwrap();
    let (rows, frames) = paged(&s, |s| s.compare(NUMS, a, b).unwrap());
    assert_eq!(rows.len(), 1);
    assert!(frames <= 80, "a one-row compare without extents paged {frames} frames");
}
