//! `viewbench` — what derived state in the Ent buys and costs.
//!
//! Two mechanisms from Ent-fidelity step 3, measured on a moo-shaped world of
//! N things in rooms of four:
//!
//! * **Join maintenance through the Ent's indexes**: a view's delta after a
//!   one-row commit, against the snapshot difference it replaced.
//! * **`materialized view`**: reading `here(viewer)` from its stored copy,
//!   against evaluating it, and what keeping the copy current costs.
//!
//! One warmed wall-clock run per line; build `--release`.

use std::sync::Arc;
use std::time::Instant;

use grmpl::Runtime;
use grmpl_core::{Entity, Tuple, Value, WorldStore};
use grmpl_diff::{eval_delta, eval_snapshot, multiset};
use grmpl_ent::EntStore;

const BODY: &str = r#"
rel located(thing: Ent, place: Ent)
rel named(thing: Ent, name: Text)

VIEW here(viewer) {
    located(viewer, room)
    located(thing, room)
    named(thing, name)
    yield thing, name
}

VIEW world() {
    located(thing, room)
    named(thing, name)
    yield thing, name
}
"#;

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn row(label: &str, size: u64, ns: f64, extra: &str) {
    println!("  {label:<38} {size:>8}  {ns:>12.0} ns  {extra}");
}

/// A durable world of `n` things, four to a room, all named; returns the store
/// and a runtime compiled with or without `materialized`.
fn world(dir: &std::path::Path, n: u64, materialized: bool) -> (Arc<dyn WorldStore>, Arc<Runtime>) {
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir).unwrap());
    let src = BODY.replace("VIEW", if materialized { "materialized view" } else { "view" });
    let rt = Runtime::compile(Arc::clone(&store), &src, 100).unwrap();
    let (located, named) = (rt.relation("located").unwrap(), rt.relation("named").unwrap());
    let all: Vec<u64> = (0..n).collect();
    for chunk in all.chunks(1_000) {
        let mut ups = Vec::new();
        for &i in chunk {
            ups.push((located, Tuple::from([ent(i), ent(1_000_000 + i / 4)]), 1));
            ups.push((named, Tuple::from([ent(i), Value::text(format!("thing {i}"))]), 1));
        }
        store.commit(&ups).unwrap();
    }
    (store, rt)
}

/// Move thing `i` to the next room over.
fn nudge(store: &dyn WorldStore, rt: &Runtime, i: u64, k: u64) {
    let located = rt.relation("located").unwrap();
    let (from, to) = (1_000_000 + (i / 4 + k) % 1_000_000, 1_000_000 + (i / 4 + k + 1) % 1_000_000);
    store
        .commit(&[
            (located, Tuple::from([ent(i), ent(if k == 0 { 1_000_000 + i / 4 } else { from })]), -1),
            (located, Tuple::from([ent(i), ent(to)]), 1),
        ])
        .unwrap();
}

fn main() {
    let sizes = [1_000u64, 10_000, 100_000];

    println!("\n── Maintenance — the delta of `world` (located ⋈ named) after a one-row move");
    println!("  {:<38} {:>8}  {:>15}", "case", "things", "per op");
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let (store, rt) = world(dir.path(), n, true);
        rt.refresh_views().unwrap();
        let from = store.current();
        nudge(store.as_ref(), &rt, n / 2, 0);
        rt.refresh_views().unwrap();
        let to = store.current();
        let (located, named) = (rt.relation("located").unwrap(), rt.relation("named").unwrap());
        let time = |q: &grmpl_diff::Query| {
            let start = Instant::now();
            let d = eval_delta(q, store.as_ref(), from, to).unwrap();
            (start.elapsed().as_nanos() as f64, d.len())
        };
        let join = grmpl_diff::Query::rel(located).join(grmpl_diff::Query::rel(named), [0], [0]);
        let (ns, rows) = time(&join);
        row("bare join, keyed lookup", n, ns, &format!("{rows} rows"));
        let start = Instant::now();
        let mut diff = eval_snapshot(&join, store.as_ref(), to).unwrap();
        for (t, w) in eval_snapshot(&join, store.as_ref(), from).unwrap() {
            multiset::add(&mut diff, t, -w);
        }
        multiset::strip_zeros(&mut diff);
        row("bare join, snapshot difference", n, start.elapsed().as_nanos() as f64, "");
        let plain = grmpl_diff::Query::Distinct(Box::new(join.project([0, 3])));
        let (ns, rows) = time(&plain);
        row("view (distinct over the join)", n, ns, &format!("{rows} rows; distinct recomputes both ends"));
        let (ns, rows) = time(&rt.query("world", &[]).unwrap());
        row("materialized view (compare its copy)", n, ns, &format!("{rows} rows"));
    }

    println!("\n── Materialized view — reading `here(viewer)`");
    println!("  {:<38} {:>8}  {:>15}", "case", "things", "per op");
    for &n in &sizes {
        let plain_dir = tempfile::tempdir().unwrap();
        let (_, plain) = world(plain_dir.path(), n, false);
        let dir = tempfile::tempdir().unwrap();
        let (store, mat) = world(dir.path(), n, true);

        let start = Instant::now();
        let written = mat.refresh_views().unwrap();
        let first = start.elapsed().as_nanos() as f64;
        row("first refresh (materialize whole)", n, first, &format!("{written} stored rows"));

        const R: u64 = 200;
        let viewer = |k: u64| [ent((k * 7_919) % n)];
        let time = |rt: &Runtime| {
            let start = Instant::now();
            for k in 0..R {
                std::hint::black_box(rt.view("here", &viewer(k)).unwrap());
            }
            start.elapsed().as_nanos() as f64 / R as f64
        };
        row("here(viewer), evaluated", n, time(&plain), "");
        row("here(viewer), materialized and current", n, time(&mat), "");

        // A move the copy has not folded in: reads fall back to evaluating.
        nudge(store.as_ref(), &mat, n / 3, 0);
        row("here(viewer), materialized but stale", n, time(&mat), "falls back to the plan");
        // The first refresh after loading is the stale one above.

        let start = Instant::now();
        mat.refresh_views().unwrap();
        row(
            "first incremental refresh",
            n,
            start.elapsed().as_nanos() as f64,
            "builds the Arrangement `here` probes (located by room)",
        );
        const C: u64 = 20;
        let mut refresh = 0.0;
        for k in 1..=C {
            nudge(store.as_ref(), &mat, n / 3, k);
            let start = Instant::now();
            mat.refresh_views().unwrap();
            refresh += start.elapsed().as_nanos() as f64;
        }
        row("refresh after a one-row move", n, refresh / C as f64, "two views, fsync'd commits");
    }
    println!("\n── Scopes — which spans contain an entity, on a reopened store");
    println!("  {:<38} {:>8}  {:>15}", "case", "scopes", "per op");
    for &n in &sizes {
        let dir = tempfile::tempdir().unwrap();
        let rel = grmpl_core::RelId(1);
        let scope = |a: u64, b: u64| Tuple::from([ent(a), ent(b), Value::text("mood"), Value::text("x")]);
        {
            let store = EntStore::open(dir.path()).unwrap();
            let rows: Vec<_> = (0..n).map(|i| (rel, scope(i * 10, i * 10 + 9), 1)).collect();
            for chunk in rows.chunks(1_000) {
                grmpl_core::TraceStore::commit(&store, chunk).unwrap();
            }
        }
        let store = EntStore::open(dir.path()).unwrap();
        let at = grmpl_core::EditionStore::current(&store);
        let point = [ent(n * 5 + 3)];
        let before = store.frames_paged();
        let start = Instant::now();
        let hits = grmpl_core::TraceStore::read_containing(&store, rel, at, 0, 1, &point).unwrap();
        let cold = start.elapsed().as_nanos() as f64;
        let paged = store.frames_paged() - before;
        row("read_containing, cold", n, cold, &format!("{} hit, {paged} frames paged", hits.len()));
        const R: u32 = 1_000;
        let start = Instant::now();
        for _ in 0..R {
            std::hint::black_box(grmpl_core::TraceStore::read_containing(&store, rel, at, 0, 1, &point).unwrap());
        }
        row("read_containing, warm", n, start.elapsed().as_nanos() as f64 / R as f64, "");
    }
    println!();
}
