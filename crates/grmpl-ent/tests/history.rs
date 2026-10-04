//! **The history index: what it costs and what it finds.**
//!
//! `src/history_laws.rs` pins backfollow and identity compare against brute
//! force. This pins their costs and the shape of their answers on a world of
//! realistic size:
//!
//! 1. **Indexing is deferred and new-node-sized.** A commit leaves the index
//!    alone; indexing a one-row commit visits only the nodes it wrote, and adds
//!    about one edge per child of each.
//! 2. **Backfollow finds copies.** From a template block it finds every
//!    instance a graft made, at the graft's shift, in every version since, on
//!    every branch a fork carried it to.
//! 3. **Identity compare finds where content moved**: comparing the template's
//!    version with a later one finds the template at shift 0 and each instance
//!    at its shift.

use grmpl_core::{Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, Layout, Version};

const R: RelId = RelId(1);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn row(e: u64) -> Tuple {
    Tuple::from([ent(e), Value::Int(0)])
}

const N: u64 = 20_000;
const BLOCK: (u64, u64) = (1_000_000, 1_001_000);

/// `N` rows plus a 1,000-row template block, in the B+ layout: the costs and
/// shares pinned here are its, and `kd_layout.rs` has the k-d layout's.
fn world(s: &EntStore) -> Edition {
    s.set_default_layout(Layout::Ordered).unwrap();
    let ids: Vec<u64> = (0..N).chain(BLOCK.0..BLOCK.1).collect();
    for c in ids.chunks(5_000) {
        s.commit(&c.iter().map(|&e| (R, row(e), 1)).collect::<Vec<_>>()).unwrap();
    }
    s.current()
}

#[test]
fn indexing_is_deferred_and_costs_the_new_nodes() {
    let s = EntStore::new();
    world(&s);
    s.step_history(usize::MAX).unwrap();
    let before = s.history_size();
    s.commit(&[(R, row(N + 7), 1)]).unwrap();
    assert_eq!(s.history_size(), before, "a commit touched the history index");
    assert_eq!(s.history_backlog(), 1);
    s.step_history(usize::MAX).unwrap();
    let after = s.history_size();
    let (edges, versions, nodes) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
    // One version; its new nodes are one path (height 3 at 21k rows); each
    // new internal node gains an edge per child.
    assert_eq!(versions, 1, "one new version");
    assert!((1..=4).contains(&nodes), "a one-row commit indexed {nodes} new nodes");
    assert!((1..=2 * 64 + 2).contains(&edges), "a one-row commit added {edges} edges");
}

#[test]
fn indexing_steps_honour_their_budget() {
    let s = EntStore::new();
    world(&s);
    let pending = s.history_backlog();
    assert!(pending >= 3);
    assert_eq!(s.step_history(1).unwrap(), 1);
    assert_eq!(s.history_backlog(), pending - 1);
    assert_eq!(s.step_history(usize::MAX).unwrap(), pending - 1);
    assert_eq!(s.history_backlog(), 0);
}

#[test]
fn indexing_a_fork_costs_only_what_it_wrote() {
    // The same world, indexed alone.
    let alone = EntStore::new();
    world(&alone);
    alone.step_history(usize::MAX).unwrap();
    let (_, versions, nodes) = alone.history_size();
    // Forked before any indexing, with one commit on the fork.
    let s = EntStore::new();
    world(&s);
    let fork = s.fork_at(s.current()).unwrap();
    fork.commit(&[(R, row(N + 9), 1)]).unwrap();
    s.step_history(usize::MAX).unwrap();
    let (_, v, n) = s.history_size();
    assert_eq!(v, versions + 1, "the fork indexed versions it inherited");
    assert!(n <= nodes + 4, "the fork re-indexed nodes it inherited: {n} against {nodes}");
}

#[test]
fn backfollow_finds_every_instance_on_every_branch() {
    let s = EntStore::new();
    let template_at = world(&s);
    let shifts: Vec<i64> = (1..=4).map(|k| k * 1_000_000).collect();
    for &d in &shifts {
        s.instance_template(&[R], BLOCK.0, BLOCK.1, d).unwrap();
    }
    // A fork carries everything; the parent then edits one instance.
    let fork = s.fork_at(s.current()).unwrap();
    s.commit(&[(R, row(BLOCK.0 + 1_000_000 + 5), -1)]).unwrap();
    let (lo, hi) = (Tuple::from([ent(BLOCK.0)]), Tuple::from([ent(BLOCK.1)]));
    let found = s.backfollow(R, template_at, &lo, &hi).unwrap();
    for &d in &shifts {
        for branch in [s.branch_id(), fork.branch_id()] {
            assert!(
                found.iter().any(|h| h.shift == d && h.version.branch == branch && h.rows > 900),
                "instance at {d} not found on branch {branch}"
            );
        }
    }
    // The template itself, at shift 0, in its own version.
    assert!(found.iter().any(|h| h.shift == 0 && h.version.edition == template_at && h.rows == 1_000));
    // The edited instance shares less with the template after the edit.
    let latest = |b: u64, d: i64| {
        found.iter().filter(|h| h.version.branch == b && h.shift == d).max_by_key(|h| h.version.edition).unwrap().rows
    };
    assert!(latest(s.branch_id(), 1_000_000) < latest(fork.branch_id(), 1_000_000));
}

#[test]
fn identity_compare_finds_the_template_and_its_instances() {
    let s = EntStore::new();
    let template_at = world(&s);
    for k in 1..=3 {
        s.instance_template(&[R], BLOCK.0, BLOCK.1, k * 1_000_000).unwrap();
    }
    let v = |e: Edition| Version { branch: s.branch_id(), rel: R, edition: e };
    let shared = s.shared_region(v(template_at), v(s.current())).unwrap();
    // Everything at shift 0; the template block again at each instance's shift
    // (less the leaves its seams rebuilt).
    let at = |d: i64| shared.iter().find(|(s, _)| *s == d).map_or(0, |(_, r)| *r);
    assert!(at(0) > N as usize, "the unchanged relation is shared in place");
    for k in 1..=3 {
        let rows = at(k * 1_000_000);
        assert!((900..=1_000).contains(&rows), "instance {k} shares {rows} rows with the template");
    }
}
