//! **Extents: Gold's wid, pruning on columns the tree is not ordered by.**
//!
//! Every Fact tree node carries, beside its count, the bounding box of its
//! entity cells per column. Three claims are pinned here:
//!
//! 1. **A search on any entity column is exact**, at any live edition —
//!    `search_at` and the as-of `read_range_on` agree with reading the relation
//!    and filtering it.
//! 2. **What it prunes depends on locality, and that is measurable.** A column
//!    that tracks the key order (an exit's destination is a nearby room) prunes
//!    to a handful of frames; a column scattered across the world prunes
//!    nothing. Both are pinned with the paging counter, because the second is
//!    the structure's real weakness, not a bug to hide.
//! 3. **Instancing checks its precondition.** A template whose facts name an
//!    entity outside its block is refused, proved from the block's extent.

use grmpl_core::{Diff, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, Layout};

const EXITS: RelId = RelId(1);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

/// `exits(from, way, to)`.
fn exit(from: u64, way: i64, to: u64) -> Tuple {
    Tuple::from([ent(from), Value::Int(way), ent(to)])
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

fn filtered(rows: Vec<(Tuple, Diff)>, col: usize, lo: u64, hi: u64) -> Vec<(Tuple, Diff)> {
    rows.into_iter()
        .filter(|(t, _)| matches!(t.as_slice().get(col), Some(Value::Ent(e)) if lo <= e.0 && e.0 < hi))
        .collect()
}

#[test]
fn an_entity_column_search_is_exact_at_every_edition() {
    let store = EntStore::new();
    let mut rng = Rng(0x5EED);
    let mut editions = Vec::new();
    for _ in 0..60 {
        let mut ups = Vec::new();
        for _ in 0..40 {
            let from = rng.below(2_000);
            let row = exit(from, rng.below(4) as i64, from + rng.below(30));
            let diff = if rng.below(4) == 0 { -1 } else { 1 };
            ups.push((EXITS, row, diff));
        }
        editions.push(store.commit(&ups).unwrap());
    }
    for &at in editions.iter().step_by(7) {
        let all = store.read_at(EXITS, at).unwrap();
        for _ in 0..10 {
            let (a, b) = (rng.below(2_100), rng.below(2_100));
            let (lo, hi) = (a.min(b), a.max(b));
            let want = filtered(all.clone(), 2, lo, hi);
            assert_eq!(store.search_at(EXITS, at, &[(2, lo, hi)]).unwrap(), want, "{at:?} [{lo},{hi})");
            // The as-of column read goes through the same search.
            let mut got = store.read_range_on(EXITS, at, 2, &ent(lo), &ent(hi)).unwrap();
            got.sort();
            assert_eq!(got, want);
            // A box on both entity columns is their intersection.
            let both = filtered(want.clone(), 0, lo.saturating_sub(15), hi);
            assert_eq!(store.search_at(EXITS, at, &[(2, lo, hi), (0, lo.saturating_sub(15), hi)]).unwrap(), both);
        }
    }
}

/// Build a durable world of `rooms` rooms with four exits each, where an
/// exit's destination is `to(room, way)`, then reopen it so every node starts
/// on disk.
fn reopened(dir: &std::path::Path, rooms: u64, to: impl Fn(u64, u64) -> u64) -> EntStore {
    {
        let store = EntStore::open_with(dir, grmpl_ent::Durability::Os).unwrap();
        // This measures the B+ layout's extents, so it pins that layout.
        store.set_default_layout(Layout::Ordered).unwrap();
        for chunk in (0..rooms).collect::<Vec<_>>().chunks(1_000) {
            let ups: Vec<_> = chunk
                .iter()
                .flat_map(|&r| (0..4).map(move |w| (r, w)))
                .map(|(r, w)| (EXITS, exit(r, w as i64, to(r, w)), 1))
                .collect();
            store.commit(&ups).unwrap();
        }
    }
    EntStore::open_with(dir, grmpl_ent::Durability::Os).unwrap()
}

#[test]
fn pruning_tracks_locality_strong_when_correlated_and_nothing_when_scattered() {
    const ROOMS: u64 = 20_000;
    let leaves = ROOMS * 4 / 64;

    // Exits lead to nearby rooms: the destination column tracks the key.
    let near = tempfile::tempdir().unwrap();
    let store = reopened(near.path(), ROOMS, |r, w| r + w + 1);
    let at = store.current();
    let before = store.frames_paged();
    let rows = store.search_at(EXITS, at, &[(2, 10_000, 10_010)]).unwrap();
    let near_frames = store.frames_paged() - before;
    assert_eq!(rows.len(), 40);
    assert!(near_frames <= 12, "a correlated column paged in {near_frames} frames");

    // Exits lead anywhere: every leaf's box spans the world.
    let far = tempfile::tempdir().unwrap();
    let store = reopened(far.path(), ROOMS, |r, w| (r * 7_919 + w * 104_729) % ROOMS);
    let at = store.current();
    let before = store.frames_paged();
    let rows = store.search_at(EXITS, at, &[(2, 10_000, 10_010)]).unwrap();
    let far_frames = store.frames_paged() - before;
    assert_eq!(rows.len(), 40);
    assert!(
        far_frames >= leaves / 2,
        "a scattered column paged in only {far_frames} of ~{leaves} leaves: the test proves nothing"
    );
}

#[test]
fn instancing_refuses_a_template_that_names_an_outside_entity() {
    let store = EntStore::new();
    // A room in the block whose exit leads out of it, to 5 000.
    store.commit(&[(EXITS, exit(1_000, 0, 1_001), 1), (EXITS, exit(1_001, 1, 5_000), 1)]).unwrap();
    let before = store.current();
    let err = store.instance_template(&[EXITS], 1_000, 1_100, 10_000).unwrap_err().to_string();
    assert!(err.contains("outside"), "{err}");
    assert_eq!(store.current(), before, "a refused instancing committed something");

    // Bring the exit home and the same template instances.
    store
        .commit(&[(EXITS, exit(1_001, 1, 5_000), -1), (EXITS, exit(1_001, 1, 1_000), 1)])
        .unwrap();
    let at = store.instance_template(&[EXITS], 1_000, 1_100, 10_000).unwrap();
    assert_eq!(
        store.search_at(EXITS, at, &[(2, 11_000, 11_100)]).unwrap(),
        vec![(exit(11_000, 0, 11_001), 1), (exit(11_001, 1, 11_000), 1)]
    );
}
