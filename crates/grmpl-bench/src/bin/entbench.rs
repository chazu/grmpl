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
use grmpl_ent::{EntStore, Granfilade, Layout, Version};

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

/// `exits(from, way, to)`: two entity columns, so the Fact trees' extents have
/// something to bound.
fn exit(from: u64, way: i64, to: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(from)), Value::Int(way), Value::Ent(Entity(to))])
}

/// A world of `rows` exits, four per room, whose destinations are `to(room,
/// way)`, loaded in 1 000-row commits.
fn exits_world(dir: &std::path::Path, rows: u64, to: impl Fn(u64, u64) -> u64) -> EntStore {
    let store = EntStore::open(dir).expect("open");
    let all: Vec<(RelId, Tuple, Diff)> =
        (0..rows).map(|i| (REL, exit(i / 4, (i % 4) as i64, to(i / 4, i % 4)), 1)).collect();
    for chunk in all.chunks(1_000) {
        store.commit(chunk).unwrap();
    }
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
    // `entbench <section>` runs one section: `layouts` or `identity`.
    match std::env::args().nth(1).as_deref() {
        Some("layouts") => return kd_layout(),
        Some("identity") => return identity_compare(),
        Some("runs") => return runs(),
        _ => {}
    }
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

    // ------------------------------------------------------------ extents ---
    header(
        "Extents — pruning on a column the tree is not ordered by",
        "Exits whose destination lies in a 10-room span. `near`: exits lead to adjacent rooms; `far`: anywhere.",
    );
    for &n in &sizes {
        let rooms = n as u64 / 4;
        let span = (rooms / 2, rooms / 2 + 10);
        let near = |r: u64, w: u64| (r + w + 1) % rooms;
        let far = |r: u64, w: u64| (r * 7_919 + w * 104_729) % rooms;
        for (label, to) in [("near", &near as &dyn Fn(u64, u64) -> u64), ("far", &far)] {
            let dir = tempfile::tempdir().unwrap();
            drop(exits_world(dir.path(), n as u64, to));
            let store = EntStore::open(dir.path()).unwrap();
            let at = store.current();

            let before = store.frames_paged();
            let start = Instant::now();
            let got = store.search_at(REL, at, &[(2, span.0, span.1)]).unwrap().len();
            let cold = start.elapsed().as_nanos() as f64;
            let paged = store.frames_paged() - before;
            row(&format!("search_at, {label} (cold)"), n, cold, &format!("{got} rows, {paged} frames paged"));

            const R: u32 = 50;
            let start = Instant::now();
            for _ in 0..R {
                std::hint::black_box(store.search_at(REL, at, &[(2, span.0, span.1)]).unwrap());
            }
            let warm = start.elapsed().as_nanos() as f64 / R as f64;
            row(&format!("search_at, {label} (warm)"), n, warm, "");

            let (lo, hi) = (Value::Ent(Entity(span.0)), Value::Ent(Entity(span.1)));
            let start = Instant::now();
            store.read_range_on(REL, at, 2, &lo, &hi).unwrap();
            let build = start.elapsed().as_nanos() as f64;
            let start = Instant::now();
            for _ in 0..R {
                std::hint::black_box(store.read_range_on(REL, at, 2, &lo, &hi).unwrap());
            }
            let arr = start.elapsed().as_nanos() as f64 / R as f64;
            row(&format!("Arrangement, {label}"), n, arr, &format!("first call built it in {:.1} ms", build / 1e6));

            let start = Instant::now();
            let all = store.read_at(REL, at).unwrap();
            let hits = all
                .iter()
                .filter(|(t, _)| matches!(t.as_slice()[2], Value::Ent(e) if span.0 <= e.0 && e.0 < span.1))
                .count();
            let scan = start.elapsed().as_nanos() as f64;
            row(&format!("read_at + filter, {label}"), n, scan, &format!("{hits} rows"));
        }
    }

    // --------------------------------------------------------------- size ---
    header(
        "Size — what the frames cost",
        "An exits world (two entity columns per row), then 200 single-row commits. Frame bytes before compression.",
    );
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let store = exits_world(dir.path(), n as u64, |r, w| r + w + 1);
        let loaded = store.bytes_encoded();
        let (before, bytes_before) = (store.frames_encoded(), store.bytes_encoded());
        const C: u64 = 200;
        let start = Instant::now();
        for k in 0..C {
            store.commit(&[(REL, exit(n as u64 + k, 0, n as u64 + k + 1), 1)]).unwrap();
        }
        let ns = start.elapsed().as_nanos() as f64 / C as f64;
        let frames = (store.frames_encoded() - before) as f64 / C as f64;
        let bytes = (store.bytes_encoded() - bytes_before) as f64 / C as f64;
        row(
            "exits commit (fsync'd)",
            n,
            ns,
            &format!(
                "{frames:.1} frames, {:.1} KB/commit; load wrote {:.0} B/row",
                bytes / 1e3,
                loaded as f64 / n as f64
            ),
        );
    }

    for &n in &sizes {
        let store = EntStore::new();
        let all: Vec<(RelId, Tuple, Diff)> = (0..n as u64)
            .map(|i| (REL, exit(i / 4, (i % 4) as i64, i / 4 + i % 4 + 1), 1))
            .collect();
        for chunk in all.chunks(1_000) {
            store.commit(chunk).unwrap();
        }
        const C: u64 = 20_000;
        let start = Instant::now();
        for k in 0..C {
            store.commit(&[(REL, exit(n as u64 + k, 0, n as u64 + k + 1), 1)]).unwrap();
        }
        let ns = start.elapsed().as_nanos() as f64 / C as f64;
        row("exits commit (in memory)", n, ns, "no granfilade: the tree work alone");
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
        row("count_at (sizes, no rows)", n, m_ns, "");
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

    kd_layout();
    identity_compare();
    runs();
    println!();
}

/// **G9: run leaves.** One relation of 100k rows, loaded in 1 000-row commits,
/// three ways: regular rows (`(room, 0)`) with runs on, the same with runs off
/// (the default), and irregular rows (a tag no step repeats) with runs on.
/// Then reads, an edit inside the block, a compare across it, and what
/// backfollow finds of a template after a graft.
fn runs() {
    header(
        "Runs — rows that fold against rows that cannot",
        "100k rows, B+ layout; cold reads on a reopened store.",
    );
    let n = 100_000u64;
    for (label, regular, runs) in [("regular, runs", true, true), ("regular, no runs", true, false), ("irregular, runs", false, true)] {
        let fact = move |e: u64| {
            let tag = if regular { 0 } else { (e * e % 97) as i64 };
            Tuple::from([Value::Ent(Entity(e)), Value::Int(tag)])
        };
        let dir = tempfile::tempdir().unwrap();
        let (load, nodes, bytes) = {
            let store = EntStore::open(dir.path()).unwrap();
            store.set_runs(REL, runs).unwrap();
            let start = Instant::now();
            let all: Vec<u64> = (0..n).collect();
            for chunk in all.chunks(1_000) {
                store.commit(&chunk.iter().map(|&e| (REL, fact(e), 1)).collect::<Vec<_>>()).unwrap();
            }
            let load = start.elapsed().as_nanos() as f64 / n as f64;
            store.consolidate(store.current()).unwrap();
            store.gc().unwrap();
            (load, store.stored_nodes().unwrap(), store.bytes_encoded())
        };
        row(&format!("{label}: load, per row"), n as i64, load, &format!("{nodes} nodes stored after consolidation, {bytes} bytes encoded"));

        let store = EntStore::open(dir.path()).unwrap();
        let at = store.current();
        let (lo, hi) = (Tuple::from([Value::Ent(Entity(40_000))]), Tuple::from([Value::Ent(Entity(41_000))]));
        let (got, frames, ns) = cold(&store, |s| s.range_at(REL, at, &lo, &hi).unwrap());
        row(&format!("{label}: 1 000-row range (cold)"), n as i64, ns, &format!("{} rows, {frames} frames", got.len()));
        let ns = warm(5, || {
            std::hint::black_box(store.read_at(REL, at).unwrap());
        });
        row(&format!("{label}: read_at whole relation"), n as i64, ns, "");

        // An edit inside the block, then the compare across it, cold.
        let before = store.frames_encoded();
        let start = Instant::now();
        let edited = store.commit(&[(REL, fact(50_000), -1), (REL, fact(50_000), 1), (REL, fact(60_000), 1)]).unwrap();
        let ns = start.elapsed().as_nanos() as f64;
        row(&format!("{label}: commit inside the block"), n as i64, ns, &format!("{} frames written", store.frames_encoded() - before));
        drop(store);
        let store = EntStore::open(dir.path()).unwrap();
        let (diff, frames, ns) = cold(&store, |s| s.compare(REL, at, edited).unwrap());
        row(&format!("{label}: compare across it (cold)"), n as i64, ns, &format!("{} rows, {frames} frames", diff.len()));

        // Instance a 1 000-row template above the world; what does
        // backfollow find of it, by shared nodes?
        let template = store.current();
        store.instance_template(&[REL], 10_000, 11_000, 10_000_000).unwrap();
        let found = store
            .backfollow(REL, template, &Tuple::from([Value::Ent(Entity(10_000))]), &Tuple::from([Value::Ent(Entity(11_000))]))
            .unwrap();
        let copy: usize = found.iter().filter(|h| h.shift == 10_000_000).map(|h| h.rows).max().unwrap_or(0);
        row(&format!("{label}: backfollow finds of the copy"), n as i64, 0.0, &format!("{copy} of 1 000 rows by shared nodes"));
    }
}

/// **Identity compare** (step 4's world, now reproducible): a 100k-row
/// relation with a 1 000-row template block, ten instances of it, then one-row
/// commits; `shared_region` between the template's edition and the latest,
/// both ways, each on a freshly reopened store.
fn identity_compare() {
    header(
        "Identity compare — Gold's upward climb against a downward walk",
        "100k rows, a 1 000-row template, ten instances, then N one-row commits; cold.",
    );
    let fact = |e: u64| Tuple::from([Value::Ent(Entity(e)), Value::Int(0)]);
    for commits in [200u64, 2_000] {
        let dir = tempfile::tempdir().unwrap();
        let (branch, template_at, latest) = {
            let store = EntStore::open(dir.path()).unwrap();
            let ids: Vec<u64> = (0..100_000).chain(1_000_000..1_001_000).collect();
            for chunk in ids.chunks(5_000) {
                store.commit(&chunk.iter().map(|&e| (REL, fact(e), 1)).collect::<Vec<_>>()).unwrap();
            }
            let template_at = store.current();
            for k in 1..=10u64 {
                store.instance_template(&[REL], 1_000_000, 1_001_000, (k * 1_000_000) as i64).unwrap();
            }
            for k in 0..commits {
                store.commit(&[(REL, fact(200_000 + k), 1)]).unwrap();
            }
            store.step_history(usize::MAX).unwrap();
            (store.branch_id(), template_at, store.current())
        };
        let v = |e: Edition| Version { branch, rel: REL, edition: e };
        for (label, a, b) in [("early → late", template_at, latest), ("late → early", latest, template_at)] {
            for (method, descent) in [("upward", false), ("descent", true)] {
                let store = EntStore::open(dir.path()).unwrap();
                let before = store.frames_paged();
                let start = Instant::now();
                let shared = if descent {
                    store.shared_region_by_descent(v(a), v(b)).unwrap()
                } else {
                    store.shared_region(v(a), v(b)).unwrap()
                };
                let ns = start.elapsed().as_nanos() as f64;
                let frames = store.frames_paged() - before;
                row(&format!("{method}, {label}"), commits as i64, ns, &format!("{frames} frames, {} shifts", shared.len()));
            }
        }
    }
}

/// `exits_world` laid out as `layout`, reopened cold.
fn exits_world_in(dir: &std::path::Path, layout: Layout, rows: u64, to: impl Fn(u64, u64) -> u64) -> (EntStore, f64) {
    let start = Instant::now();
    {
        let store = EntStore::open(dir).expect("open");
        store.set_default_layout(layout).unwrap();
        let all: Vec<(RelId, Tuple, Diff)> =
            (0..rows).map(|i| (REL, exit(i / 4, (i % 4) as i64, to(i / 4, i % 4)), 1)).collect();
        for chunk in all.chunks(1_000) {
            store.commit(chunk).unwrap();
        }
    }
    let load = start.elapsed().as_nanos() as f64 / rows as f64;
    (EntStore::open(dir).unwrap(), load)
}

/// Frames a cold read pages in, and its wall-clock time.
fn cold<T>(store: &EntStore, read: impl FnOnce(&EntStore) -> T) -> (T, u64, f64) {
    let before = store.frames_paged();
    let start = Instant::now();
    let out = read(store);
    let ns = start.elapsed().as_nanos() as f64;
    (out, store.frames_paged() - before, ns)
}

/// Mean wall-clock time of `r` warm runs.
fn warm(r: u32, mut f: impl FnMut()) -> f64 {
    let start = Instant::now();
    for _ in 0..r {
        f();
    }
    start.elapsed().as_nanos() as f64 / r as f64
}

/// **G8: the k-d layout against the B+ layout**, on one exits world with
/// scattered destinations, every read on a cold store first.
fn kd_layout() {
    header(
        "Layouts — B+ (ordered) against k-d (splits on any column)",
        "Exits with scattered destinations; each read first cold (frames paged), then warm.",
    );
    for n in [10_000u64, 100_000] {
        let rooms = n / 4;
        let far = move |r: u64, w: u64| (r * 7_919 + w * 104_729) % rooms;
        let span = (rooms / 2, rooms / 2 + 10);
        for layout in [Layout::Ordered, Layout::Kd] {
            let tag = match layout {
                Layout::Ordered => "B+",
                Layout::Kd => "k-d",
            };
            let dir = tempfile::tempdir().unwrap();
            let (store, load) = exits_world_in(dir.path(), layout, n, far);
            let at = store.current();
            let size = n as i64;
            row(&format!("{tag}: load, per row"), size, load, "1 000-row commits");

            let (lo, hi) = (Tuple::from([Value::Ent(Entity(rooms / 3))]), Tuple::from([Value::Ent(Entity(rooms / 3 + 1))]));
            let (got, frames, ns) = cold(&store, |s| s.range_at(REL, at, &lo, &hi).unwrap());
            row(&format!("{tag}: one room's exits (cold)"), size, ns, &format!("{} rows, {frames} frames", got.len()));
            let ns = warm(1_000, || {
                std::hint::black_box(store.range_at(REL, at, &lo, &hi).unwrap());
            });
            row(&format!("{tag}: one room's exits (warm)"), size, ns, "");

            let (got, frames, ns) = cold(&store, |s| s.search_at(REL, at, &[(2, span.0, span.1)]).unwrap());
            row(&format!("{tag}: box on scattered column (cold)"), size, ns, &format!("{} rows, {frames} frames", got.len()));
            let ns = warm(50, || {
                std::hint::black_box(store.search_at(REL, at, &[(2, span.0, span.1)]).unwrap());
            });
            row(&format!("{tag}: box on scattered column (warm)"), size, ns, "");

            let (vlo, vhi) = (Value::Ent(Entity(span.0)), Value::Ent(Entity(span.1)));
            let (_, frames, ns) = cold(&store, |s| s.read_range_on(REL, Edition(at.0 - 1), 2, &vlo, &vhi).unwrap());
            row(&format!("{tag}: column range, past (cold)"), size, ns, &format!("{frames} frames"));
            let (_, _, first) = cold(&store, |s| s.read_range_on(REL, at, 2, &vlo, &vhi).unwrap());
            let ns = warm(50, || {
                std::hint::black_box(store.read_range_on(REL, at, 2, &vlo, &vhi).unwrap());
            });
            row(&format!("{tag}: column range, present (warm)"), size, ns, &format!("first call {:.1} ms", first / 1e6));

            let ns = warm(3, || {
                std::hint::black_box(store.read_at(REL, at).unwrap());
            });
            row(&format!("{tag}: read_at, whole relation"), size, ns, "sorted output");

            let mut k = 0u64;
            let before = store.frames_encoded();
            let ns = warm(200, || {
                store.commit(&[(REL, exit(k % rooms, 9, (k * 31) % rooms), 1)]).unwrap();
                k += 1;
            });
            let frames = (store.frames_encoded() - before) / 200;
            row(&format!("{tag}: single-row commit"), size, ns, &format!("{frames} frames/commit"));

            // A 1 000-row block instanced above the world, then compared.
            let (block_lo, block_hi) = (rooms / 4, rooms / 4 + 250);
            let local = move |r: u64, w: u64| block_lo + (r + w) % 250;
            let ups: Vec<(RelId, Tuple, Diff)> =
                (block_lo..block_hi).flat_map(|r| (0..4).map(move |w| (REL, exit(r, 20 + w as i64, local(r, w)), 1))).collect();
            store.commit(&ups).unwrap();
            let template = store.current();
            // Only the template rows are inside the block's span with ways ≥ 20,
            // but a graft moves every fact of the block; drop the scattered ones.
            let stray: Vec<(RelId, Tuple, Diff)> = store
                .range_at(REL, template, &Tuple::from([Value::Ent(Entity(block_lo))]), &Tuple::from([Value::Ent(Entity(block_hi))]))
                .unwrap()
                .into_iter()
                .filter(|(t, _)| !matches!(t.as_slice()[1], Value::Int(w) if w >= 20))
                .map(|(t, d)| (REL, t, -d))
                .collect();
            store.commit(&stray).unwrap();
            let template = store.current();
            let before = store.frames_encoded();
            let start = Instant::now();
            let grafted = store.instance_template(&[REL], block_lo, block_hi, 10_000_000).unwrap();
            let ns = start.elapsed().as_nanos() as f64;
            row(&format!("{tag}: graft 1 000 rows"), size, ns, &format!("{} frames", store.frames_encoded() - before));
            drop(store);
            let store = EntStore::open(dir.path()).unwrap();
            let (rows, frames, ns) = cold(&store, |s| s.compare(REL, template, grafted).unwrap());
            row(&format!("{tag}: compare across graft (cold)"), size, ns, &format!("{} rows, {frames} frames", rows.len()));
        }
    }
}
