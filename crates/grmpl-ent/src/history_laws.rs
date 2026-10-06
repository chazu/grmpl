//! **The history laws**: backfollow and identity compare against brute force.
//!
//! The model enumerates every retained version on every branch, walks each one
//! whole, and records where every node sits. Backfollow must then report
//! exactly the versions holding each queried node, at the right shift; and the
//! identity compare must report exactly what one version's nodes are found in
//! another. Histories mix edits, grafts (which share nodes at a displacement),
//! forks at the present and into the past, consolidation, and indexing in
//! partial steps, on an in-memory store and a durable one reopened midway.

use std::collections::{BTreeMap, HashMap};

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};

use super::{content_key, pieces, ContentKey, EntStore, FactTree, Holding, Version};

const R1: RelId = RelId(1);
const R2: RelId = RelId(2);

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

/// Every retained version in the world, with its Fact root.
fn all_versions(store: &EntStore) -> Vec<(Version, FactTree)> {
    let root = store.family.root.lock().unwrap();
    let mut out = Vec::new();
    for (b, state) in root.branches.iter() {
        for (rel, roots) in state.rels.iter() {
            for (e, t) in roots.versions.iter() {
                out.push((Version { branch: b, rel: RelId(rel), edition: Edition(e) }, t.clone()));
            }
        }
    }
    out
}

/// A node as `(content key, offset of its frame, rows)`.
type Placed = (ContentKey, i64, usize);

/// Every node of a version.
fn nodes_of(t: &FactTree) -> Vec<Placed> {
    let t = t.normalized();
    let mut out = Vec::new();
    if content_key(&t).is_none() {
        return out;
    }
    let mut stack = vec![(t, 0i64)];
    while let Some((n, parent_off)) = stack.pop() {
        let off = parent_off.wrapping_add(n.dsp());
        out.push((*n.ck_cell().unwrap().get().unwrap(), off, n.len()));
        if let Some(children) = n.node().map(|n| n.children()) {
            stack.extend(children.iter().map(|c| (c.clone(), off)));
        }
    }
    out
}

fn model_backfollow(store: &EntStore, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Vec<Holding> {
    let tree = store.inner.lock().unwrap().fact_at(rel, at.0).cloned().unwrap_or_default().normalized();
    let mut found: BTreeMap<(Version, i64), usize> = BTreeMap::new();
    let versions: Vec<(Version, Vec<Placed>)> =
        all_versions(store).into_iter().map(|(v, t)| (v, nodes_of(&t))).collect();
    for (ck, at_off, rows) in pieces(&tree, lo, hi) {
        for (v, nodes) in &versions {
            for (n, off, _) in nodes {
                if *n == ck {
                    *found.entry((*v, off - at_off)).or_insert(0) += rows;
                }
            }
        }
    }
    found.into_iter().map(|((version, shift), rows)| Holding { version, shift, rows }).collect()
}

fn tree_of(store: &EntStore, v: Version) -> FactTree {
    let root = store.family.root.lock().unwrap();
    let state = root.branches.get(&v.branch).unwrap();
    state.fact_at(v.rel, v.edition.0).cloned().unwrap_or_default().normalized()
}

fn model_shared(store: &EntStore, a: Version, b: Version) -> Vec<(i64, usize)> {
    let mut in_b: HashMap<ContentKey, Vec<i64>> = HashMap::new();
    for (ck, off, _) in nodes_of(&tree_of(store, b)) {
        in_b.entry(ck).or_default().push(off);
    }
    // Every leaf of `a`, at every position `b` holds that leaf: content held
    // at several shifts is counted at each, as Gold's `mapSharedTo` maps each
    // key to all its appearances.
    let ta = tree_of(store, a);
    let mut out: BTreeMap<i64, usize> = BTreeMap::new();
    if content_key(&ta).is_none() {
        return Vec::new();
    }
    let mut stack = vec![(ta, 0i64)];
    while let Some((n, parent_off)) = stack.pop() {
        let off = parent_off.wrapping_add(n.dsp());
        match n.node() {
            Some(node) if node.children().is_empty() => {
                let ck = *n.ck_cell().unwrap().get().unwrap();
                let mut offs = in_b.get(&ck).cloned().unwrap_or_default();
                offs.sort_unstable();
                offs.dedup();
                for o in offs {
                    *out.entry(o - off).or_insert(0) += n.len();
                }
            }
            Some(node) => stack.extend(node.children().iter().map(|c| (c.clone(), off))),
            None => {}
        }
    }
    out.into_iter().collect()
}

/// Fill both relations deep enough that versions share interior nodes: every
/// entity of eight blocks of 100, in three tags for `R1`.
fn seed_world(s: &EntStore) {
    let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
    for e in 0..800 {
        for tag in 0..3 {
            ups.push((R1, row(e, tag), 1));
        }
        ups.push((R2, row(e, 0), 1));
    }
    s.commit(&ups).unwrap();
}

/// A random history over `branches[0]`'s world, growing `branches` with forks.
fn churn(branches: &mut Vec<EntStore>, rng: &mut Rng, steps: usize, check: &mut dyn FnMut(&[EntStore], &mut Rng)) {
    for step in 0..steps {
        let i = rng.below(branches.len() as u64) as usize;
        let s = &branches[i];
        match rng.below(10) {
            // Edits: rows across eight blocks of 100 entities, two relations.
            // Blocks 6 and 7 start empty, so grafts have somewhere to land.
            0..=3 => {
                let rel = if rng.below(3) == 0 { R2 } else { R1 };
                let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
                for _ in 0..1 + rng.below(40) {
                    let t = row(rng.below(800), rng.below(3) as i64);
                    if ups.iter().any(|(_, u, _)| *u == t) {
                        continue;
                    }
                    let held = s.read_at(rel, s.current()).unwrap().iter().any(|(x, _)| *x == t);
                    ups.push((rel, t, if held { -1 } else { 1 }));
                }
                s.commit(&ups).unwrap();
            }
            // A graft: copy one block into an empty one, sharing its nodes.
            4 | 5 => {
                let (src, dst) = (rng.below(8), rng.below(8));
                let rows = s.read_at(R1, s.current()).unwrap();
                let occupied = |b: u64| rows.iter().any(|(t, _)| t.as_slice()[0] >= ent(b * 100) && t.as_slice()[0] < ent(b * 100 + 100));
                if src != dst && occupied(src) && !occupied(dst) {
                    let _ = s.instance_template(&[R1, R2], src * 100, src * 100 + 100, (dst as i64 - src as i64) * 100);
                }
            }
            // A fork, at the present or into the past.
            6 if branches.len() < 5 => {
                let lo = s.inner.lock().unwrap().watermark;
                let hi = s.current().0;
                let at = if rng.below(2) == 0 { hi } else { lo + rng.below(hi - lo + 1) };
                let child = s.fork_at(Edition(at)).unwrap();
                branches.push(child);
            }
            // A merge: a new branch with two parents, by replay.
            9 if branches.len() < 6 && rng.below(2) == 0 => {
                let j = rng.below(branches.len() as u64) as usize;
                if let Ok(super::MergeOutcome::Merged(m)) = branches[i].merge(&branches[j]) {
                    branches.push(m);
                }
            }
            // Clear a block, so later grafts can land in it again.
            7 if rng.below(3) == 1 => {
                clear_blocks(s, &[rng.below(8)]);
            }
            // Consolidation retires old versions.
            7 if rng.below(3) == 0 => {
                let cur = s.current().0;
                if cur > 4 {
                    s.consolidate(Edition(cur - 1 - rng.below(4))).unwrap();
                }
            }
            // Index a little, so queries meet a partly indexed history.
            8 => {
                s.step_history(rng.below(6) as usize).unwrap();
            }
            _ => {}
        }
        if step % 6 == 5 {
            check(branches, rng);
        }
    }
}

/// Retract every row in the given blocks, leaving room for grafts.
fn clear_blocks(s: &EntStore, blocks: &[u64]) {
    let mut ups: Vec<(RelId, Tuple, Diff)> = Vec::new();
    for rel in [R1, R2] {
        for (t, d) in s.read_at(rel, s.current()).unwrap() {
            if blocks.iter().any(|b| t.as_slice()[0] >= ent(b * 100) && t.as_slice()[0] < ent(b * 100 + 100)) {
                ups.push((rel, t, -d));
            }
        }
    }
    s.commit(&ups).unwrap();
}

fn check_laws(branches: &[EntStore], rng: &mut Rng) {
    let s = &branches[rng.below(branches.len() as u64) as usize];
    let wm = s.inner.lock().unwrap().watermark;
    let cur = s.current().0;
    let at = Edition(wm + rng.below(cur - wm + 1));
    let rel = if rng.below(3) == 0 { R2 } else { R1 };
    let lo = rng.below(800);
    let hi = lo + 1 + rng.below(300);
    let (lo, hi) = (Tuple::from([ent(lo)]), Tuple::from([ent(hi)]));
    // The content queried is exactly the rows in the span, each once.
    let tree = s.inner.lock().unwrap().fact_at(rel, at.0).cloned().unwrap_or_default().normalized();
    let in_span = s.read_at(rel, at).unwrap().iter().filter(|(t, _)| *t >= lo && *t < hi).count();
    assert_eq!(pieces(&tree, &lo, &hi).iter().map(|p| p.2).sum::<usize>(), in_span, "pieces miscount the span");
    let want = model_backfollow(s, rel, at, &lo, &hi);
    let got = s.backfollow(rel, at, &lo, &hi).unwrap();
    assert_eq!(got, want, "backfollow {rel:?} at {at:?} on branch {}", s.branch_id());

    // Identity compare between two random retained versions.
    let versions: Vec<Version> = all_versions(s).into_iter().map(|(v, _)| v).collect();
    if versions.is_empty() {
        return;
    }
    let a = versions[rng.below(versions.len() as u64) as usize];
    let b = versions[rng.below(versions.len() as u64) as usize];
    let want = model_shared(s, a, b);
    assert_eq!(s.shared_region(a, b).unwrap(), want, "shared_region {a:?} → {b:?}");
    assert_eq!(s.shared_region_by_descent(a, b).unwrap(), want, "shared_region_by_descent {a:?} → {b:?}");
}

#[test]
fn backfollow_and_compare_match_brute_force_in_memory() {
    for seed in 1..=12u64 {
        let mut rng = Rng(seed);
        let mut branches = vec![EntStore::new()];
        seed_world(&branches[0]);
        clear_blocks(&branches[0], &[6, 7]);
        churn(&mut branches, &mut rng, 90, &mut check_laws);
        // Fully indexed, nothing is left behind.
        branches[0].step_history(usize::MAX).unwrap();
        assert_eq!(branches[0].history_backlog(), 0, "seed {seed}");
    }
}

#[test]
fn backfollow_and_compare_survive_a_reopen() {
    for seed in 1..=4u64 {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(seed ^ 0xD15C);
        {
            let mut branches = vec![EntStore::open_with(dir.path(), crate::Durability::Os).unwrap()];
            seed_world(&branches[0]);
            clear_blocks(&branches[0], &[6, 7]);
            churn(&mut branches, &mut rng, 50, &mut check_laws);
            branches[0].step_history(usize::MAX).unwrap();
        }
        // Reopen: the index comes back as far as it was made durable, and
        // catches up the rest.
        let root = EntStore::open_with(dir.path(), crate::Durability::Os).unwrap();
        let ids: Vec<u64> = root.dag().tree().iter().map(|(b, _)| b).collect();
        // The index was made durable: nothing is left to redo.
        assert_eq!(root.history_backlog(), 0, "seed {seed}: the history index did not persist");
        let mut branches: Vec<EntStore> = ids.iter().map(|b| root.branch(*b).unwrap()).collect();
        churn(&mut branches, &mut rng, 30, &mut check_laws);
    }
}
