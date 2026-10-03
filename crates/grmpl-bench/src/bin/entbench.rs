//! `entbench` — measure what the *Ent* specifically is good and bad at.
//!
//! The P13 axes (`churn`, `watch`, `precond`, `contention`, `arrangement`)
//! measure the engine's semantic costs. This measures the **substrate's shape**:
//! the places where being a persistent, measured, content-addressed tree family
//! wins by an order of magnitude, and the places where it loses to a flat array
//! for exactly the same reason.
//!
//! Each line is one warmed wall-clock run — the signal here is orders of
//! magnitude, not percent. Build `--release`.

use std::time::Instant;

use grmpl_core::{Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value};
use grmpl_ent::{EntStore, Granfilade};

const REL: RelId = RelId(1);
const OTHER: RelId = RelId(2);

fn t(n: i64) -> Tuple {
    Tuple::from([Value::Int(n)])
}

/// Bulk-load `rows` in one commit (the load itself is not what we measure).
fn seeded(dir: &std::path::Path, rows: i64) -> EntStore {
    let store = EntStore::open(dir).expect("open");
    let updates: Vec<(RelId, Tuple, Diff)> = (0..rows).map(|k| (REL, t(k), 1)).collect();
    store.commit(&updates).unwrap();
    store
}

fn row(label: &str, size: i64, ns: f64, extra: &str) {
    println!("  {label:<34} {size:>8}  {ns:>12.0} ns  {extra}");
}

fn header(title: &str, what: &str) {
    println!("\n── {title}");
    println!("   {what}");
    println!("  {:<34} {:>8}  {:>15}", "case", "size", "per op");
}

fn main() {
    let sizes: [i64; 3] = [1_000, 10_000, 100_000];

    // ---------------------------------------------------------------- fork ---
    header(
        "Fork — O(edit) virtual copy",
        "A fork adds a branch to the DAG and branch enfilade; every relation's nodes are shared.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(dir.path(), n);
        let before = store.frames_encoded();
        let start = Instant::now();
        let fork = store.fork_at(store.current()).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        let frames = store.frames_encoded() - before;
        assert_eq!(fork.read_at(REL, fork.current()).unwrap().len(), n as usize);
        row("fork whole world", n, ns, &format!("{frames} node frames written"));
    }

    // ------------------------------------------------------- virtual copy ---
    header(
        "Instance — DSP virtual copy of a template",
        "An N-fact template block copied to a fresh block: graft vs. committing the copies.",
    );
    for &n in &sizes {
        // The template sits between unrelated facts, so the graft really has to
        // cut it out of the middle of the relation.
        let ent = |e: u64, room: u64| Tuple::from([Value::Ent(Entity(e)), Value::Ent(Entity(room))]);
        let base = 1_000_000u64;
        let mut rows: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for i in 0..n as u64 {
            rows.push((REL, ent(i, i % 13), 1));
            rows.push((REL, ent(base + i, base + i % 97), 1));
            rows.push((REL, ent(10 * base + i, i % 7), 1));
        }
        let (lo, hi) = (base, base + n as u64);
        let shift = 4 * base as i64;

        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        store.commit(&rows).unwrap();
        let before = store.frames_encoded();
        let start = Instant::now();
        store.instance_template(&[REL], lo, hi, shift).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        let frames = store.frames_encoded() - before;
        row("instance_template (graft)", n, ns, &format!("{frames} node frames written"));

        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        store.commit(&rows).unwrap();
        let lo_t = Tuple::from([Value::Ent(Entity(lo))]);
        let hi_t = Tuple::from([Value::Ent(Entity(hi))]);
        let before = store.frames_encoded();
        let start = Instant::now();
        let copies: Vec<(RelId, Tuple, Diff)> = store
            .range_at(REL, store.current(), &lo_t, &hi_t)
            .unwrap()
            .into_iter()
            .map(|(t, d)| (REL, grmpl_ent::Displace::displace(&t, shift), d))
            .collect();
        store.commit(&copies).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        let frames = store.frames_encoded() - before;
        row("copy by commit (row by row)", n, ns, &format!("{frames} node frames written"));
    }

    // ------------------------------------------------------------- commits ---
    header(
        "Commit — path-only work",
        "One row into a relation of N. Cost should be depth, not N.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(dir.path(), n);
        let before = store.frames_encoded();
        const C: i64 = 200;
        let start = Instant::now();
        for k in 0..C {
            store.commit(&[(REL, t(n + k), 1)]).unwrap();
        }
        let ns = start.elapsed().as_nanos() as f64 / C as f64;
        let frames = (store.frames_encoded() - before) as f64 / C as f64;
        row("single-row commit (fsync'd)", n, ns, &format!("{frames:.1} frames/commit"));
    }

    // ---------------------------------------------------------------- reads ---
    header(
        "Read — WID range vs. full scan",
        "1% key span out of N, against reading the whole relation.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(dir.path(), n);
        let at = store.current();
        let (lo, hi) = (t(0), t(n / 100));

        const R: u32 = 200;
        let start = Instant::now();
        let mut got = 0;
        for _ in 0..R {
            got = store.read_range(REL, at, &lo, &hi).unwrap().len();
        }
        let range_ns = start.elapsed().as_nanos() as f64 / R as f64;
        row("read_range, 1% span", n, range_ns, &format!("{got} rows"));

        const S: u32 = 20;
        let start = Instant::now();
        let mut all = 0;
        for _ in 0..S {
            all = store.read_at(REL, at).unwrap().len();
        }
        let scan_ns = start.elapsed().as_nanos() as f64 / S as f64;
        row("read_at, whole relation", n, scan_ns, &format!("{all} rows"));

        // The measure answers "how many" without building a row.
        const M: u32 = 2_000;
        let start = Instant::now();
        for _ in 0..M {
            std::hint::black_box(store.count_at(REL, at, &lo, &hi).unwrap());
        }
        let m_ns = start.elapsed().as_nanos() as f64 / M as f64;
        row("count_at (measure, no rows)", n, m_ns, "");
    }

    // ------------------------------------------------- scan vs flat array ---
    header(
        "Full scan — what the tree costs when you want every row",
        "read_at against cloning a flat Vec holding the same tuples.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(dir.path(), n);
        let at = store.current();
        let flat: Vec<(Tuple, Diff)> = (0..n).map(|k| (t(k), 1i64)).collect();

        const S: u32 = 20;
        let start = Instant::now();
        for _ in 0..S {
            std::hint::black_box(store.read_at(REL, at).unwrap());
        }
        let tree_ns = start.elapsed().as_nanos() as f64 / S as f64;

        let start = Instant::now();
        for _ in 0..S {
            std::hint::black_box(flat.clone());
        }
        let flat_ns = start.elapsed().as_nanos() as f64 / S as f64;

        row("read_at (enfilade)", n, tree_ns, &format!("{:.0} ns/row", tree_ns / n as f64));
        row(
            "clone a flat Vec",
            n,
            flat_ns,
            &format!("{:.0} ns/row — tree is {:.1}x", flat_ns / n as f64, tree_ns / flat_ns),
        );
    }

    // --------------------------------------------------------------- as-of ---
    header(
        "As-of — reading the past",
        "N editions of history; read at the oldest live edition vs the newest.",
    );
    for &n in &[1_000i64, 10_000] {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        for k in 0..n {
            store.commit(&[(REL, t(k), 1)]).unwrap();
        }
        let cur = store.current();
        const R: u32 = 500;
        for (label, at) in [("newest edition", cur), ("oldest edition", Edition(1))] {
            let start = Instant::now();
            let mut rows = 0;
            for _ in 0..R {
                rows = store.read_at(REL, at).unwrap().len();
            }
            let ns = start.elapsed().as_nanos() as f64 / R as f64;
            row(&format!("read_at, {label}"), n, ns, &format!("{rows} rows"));
        }
    }

    // -------------------------------------------------------------- reopen ---
    header(
        "Reopen — recovery is a root lookup, and nodes page in on demand",
        "Open a store holding N rows, then read 10 of them. fjall's own open, alone, for comparison.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        {
            let _ = seeded(dir.path(), n);
        }
        let start = Instant::now();
        drop(Granfilade::open(dir.path()).unwrap());
        let fjall = start.elapsed().as_nanos() as f64;
        row("open fjall alone", n, fjall, "");

        let start = Instant::now();
        let store = EntStore::open(dir.path()).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        let opened = store.frames_paged();
        row("open EntStore", n, ns, &format!("{opened} frames paged"));

        let start = Instant::now();
        let rows = store.read_range(REL, store.current(), &t(n / 2), &t(n / 2 + 10)).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        assert_eq!(rows.len(), 10);
        row("first 10-row read", n, ns, &format!("{} frames paged", store.frames_paged() - opened));
        assert_eq!(store.read_at(REL, store.current()).unwrap().len(), n as usize);
    }

    // ------------------------------------------------------------- routing ---
    header(
        "Routing — a watcher the commit cannot concern",
        "Does an unrelated commit cost anything to a watcher of another relation?",
    );
    {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(dir.path(), 100_000);
        let from = store.current();
        store.commit(&[(OTHER, t(1), 1)]).unwrap();
        let to = store.current();
        const R: u32 = 20_000;
        let start = Instant::now();
        for _ in 0..R {
            std::hint::black_box(store.touched_since(from, to, &[REL]).unwrap());
        }
        let ns = start.elapsed().as_nanos() as f64 / R as f64;
        row("touched_since (proves quiet)", 100_000, ns, "vs a full re-evaluation");
    }

    // ------------------------------------------- commit cost vs history ---
    header(
        "Commit vs. accumulated history",
        "Cost of the Nth single-row commit into an unconsolidated world.",
    );
    {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let mut k = 0i64;
        for depth in [0i64, 500, 1_000, 2_000, 4_000] {
            while k < depth {
                store.commit(&[(REL, t(k), 1)]).unwrap();
                k += 1;
            }
            const C: i64 = 50;
            let start = Instant::now();
            for _ in 0..C {
                store.commit(&[(REL, t(k), 1)]).unwrap();
                k += 1;
            }
            let ns = start.elapsed().as_nanos() as f64 / C as f64;
            row("commit at history depth", depth, ns, "");
        }
    }

    // ----------------------------------------------------- history growth ---
    header(
        "History — what unconsolidated editions cost",
        "N single-row commits, then the stored-node count and a consolidate.",
    );
    for &n in &[1_000i64, 5_000] {
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        for k in 0..n {
            store.commit(&[(REL, t(k), 1)]).unwrap();
        }
        let grown = store.stored_nodes().unwrap();
        let start = Instant::now();
        store.consolidate(store.current()).unwrap();
        let collected = store.gc().unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        let after = store.stored_nodes().unwrap();
        row(
            "consolidate + gc",
            n,
            ns,
            &format!("{grown} nodes -> {after} ({collected} collected)"),
        );
    }

    println!();
}
