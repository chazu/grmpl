//! P2 acceptance (DESIGN.md §9, Snapshot–stream law) for `Reduce`:
//! `initial + Σ deltas = find(current)` under a randomized sequence of asserts
//! and retractions, for each aggregate (`Count`/`Sum`/`Min`/`Max`). The churn
//! drives group *creation*, *update*, and *emptying* (a retraction removing a
//! group's last member) — the cases that separate a correct aggregate delta
//! from a broken one. Every round is also checked against an **independent
//! model** that recomputes each aggregate from the set of present base tuples
//! directly, so the test is a genuine law oracle rather than a self-consistency
//! check.
//!
//! P13: a `Reduce` over a base relation (seen through `Distinct`, `Filter` and
//! `Project`) is maintained **per key**: the version compare names the groups
//! that changed and a keyed lookup reads just their members. The laws below run
//! that path on every substrate shape (B+, k-d, runs), over keys on any column,
//! multi-column keys, range-restricted inputs and the shapes that must fall
//! back (`Map`, an empty key, a join); the cost law shows it reads the touched
//! group rather than the relation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, Result, TraceStore, Tuple, Update, Value,
};
use grmpl_diff::{eval_delta, eval_snapshot, multiset, Agg, Query, Snapshot};
use grmpl_ent::EntStore;

const MEASURED: RelId = RelId(1); // (group: Ent, value: Int)

fn row(g: u64, v: i64) -> Tuple {
    Tuple::from([Value::Ent(Entity(g)), Value::Int(v)])
}

/// A small deterministic PRNG so the test is reproducible without a dep.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Independent model: recompute the expected `reduce(key=[group], agg)` result
/// directly from the present base tuples, as a tuple-sorted `(group, agg)` vec.
fn model(present: &HashSet<Tuple>, agg: Agg) -> Vec<(Tuple, Diff)> {
    let mut groups: HashMap<Value, Vec<i64>> = HashMap::new();
    for t in present {
        let g = t.as_slice()[0].clone();
        let v = match t.as_slice()[1] {
            Value::Int(n) => n,
            _ => unreachable!(),
        };
        groups.entry(g).or_default().push(v);
    }
    let mut out: Vec<(Tuple, Diff)> = groups
        .into_iter()
        .map(|(g, vs)| {
            let a = match agg {
                Agg::Count => Value::Int(vs.len() as i64),
                Agg::Sum(_) => Value::Int(vs.iter().sum()),
                Agg::Min(_) => Value::Int(*vs.iter().min().unwrap()),
                Agg::Max(_) => Value::Int(*vs.iter().max().unwrap()),
            };
            (Tuple::from([g, a]), 1)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn reduce_tracks_find_under_random_churn() {
    for case in grmpl_conformance::each_store() {
        let store = case.store();

        // One maintained stream per aggregate, all over the same base relation,
        // grouping by column 0 and folding column 1.
        let aggs = [
            ("count", Agg::Count),
            ("sum", Agg::Sum(1)),
            ("min", Agg::Min(1)),
            ("max", Agg::Max(1)),
        ];
        let queries: Vec<Query> =
            aggs.iter().map(|(_, a)| Query::rel(MEASURED).reduce([0], *a)).collect();

        let mut streams: Vec<_> = queries.iter().map(|q| q.watch(store, store.current())).collect();
        let mut accs: Vec<multiset::Multiset> = vec![multiset::Multiset::new(); aggs.len()];
        for (i, s) in streams.iter_mut().enumerate() {
            for (t, d) in s.poll().unwrap() {
                multiset::add(&mut accs[i], t, d); // initial (empty world)
            }
        }

        // Track present base facts so we only assert absent ones / retract
        // present ones, keeping base weights in {0,1} (a realistic world).
        let mut present: HashSet<Tuple> = HashSet::new();
        let mut rng = Lcg(0x0d1a_5eed_face_b00c);

        for round in 0..300 {
            // A small overlapping domain: group 0..3 forces multi-member groups
            // and repeated values so Min/Max ties and Sum cancellation are
            // exercised.
            let tuple = row(rng.below(3), rng.below(4) as i64);
            let diff: Diff = if present.contains(&tuple) {
                present.remove(&tuple);
                -1
            } else {
                present.insert(tuple.clone());
                1
            };
            store.commit(&[(MEASURED, tuple, diff)]).unwrap();

            for (i, (name, agg)) in aggs.iter().enumerate() {
                // Fold the maintained deltas in.
                for (t, d) in streams[i].poll().unwrap() {
                    multiset::add(&mut accs[i], t, d);
                }
                multiset::strip_zeros(&mut accs[i]);

                // Law 1: maintained result equals a fresh evaluation at current.
                let want = queries[i].find(&Snapshot::at_current(store)).unwrap();
                let got = multiset::to_sorted_vec(&accs[i]);
                assert_eq!(
                    got, want,
                    "[{} {name}] snapshot-stream law violated at round {round}",
                    case.name
                );

                // Law 2: that evaluation matches the independent model.
                let expected = model(&present, *agg);
                assert_eq!(
                    want, expected,
                    "[{} {name}] find disagrees with model at round {round}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn reduce_inside_iterate_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();

    // An `Iterate` whose step contains a `Reduce` is an ill-formed plan.
    let step = Query::recur().reduce([0], Agg::Count);
    let q = Query::iterate(Query::rel(MEASURED), step);

    let err = q.find(&Snapshot::at_current(&store)).unwrap_err();
    assert!(matches!(err, grmpl_core::Error::Query(_)), "expected Error::Query, got {err:?}");
}

// ---------------------------------------------------------------------------
// Per-key maintenance: every shape, every interval, every substrate
// ---------------------------------------------------------------------------

const WIDE: RelId = RelId(2); // (a: Ent, b: Int, v: Int, s: Text)

fn wide(a: u64, b: i64, v: i64, s: &str) -> Tuple {
    Tuple::from([Value::Ent(Entity(a)), Value::Int(b), Value::Int(v), Value::text(s)])
}

fn int(t: &Tuple, c: usize) -> i64 {
    match t.as_slice()[c] {
        Value::Int(n) => n,
        _ => unreachable!(),
    }
}

/// One maintained shape and its independent meaning: `image` is what the input
/// makes of a base row (`None` if it drops it), and the model groups the
/// images by `key` and folds them with `agg` itself.
struct Shape {
    name: &'static str,
    query: Query,
    image: fn(&Tuple) -> Option<Tuple>,
    key: Vec<usize>,
    agg: Agg,
}

impl Shape {
    /// Does the per-key path take this shape? The ones it does not are named so.
    fn per_key(&self) -> bool {
        !self.name.ends_with("(falls back)")
    }
}

fn shapes() -> Vec<Shape> {
    let whole: fn(&Tuple) -> Option<Tuple> = |t| Some(t.clone());
    let base = || Query::rel(WIDE);
    let plain = |name, key: Vec<usize>, agg| Shape {
        name,
        query: base().reduce(key.clone(), agg),
        image: whole,
        key,
        agg,
    };
    vec![
        plain("count by col 0", vec![0], Agg::Count),
        plain("sum by col 0", vec![0], Agg::Sum(2)),
        plain("min by col 0", vec![0], Agg::Min(2)),
        plain("max by col 0", vec![0], Agg::Max(2)),
        plain("sum by col 1", vec![1], Agg::Sum(2)),
        // A text lead column has no successor: the lookup reads and filters.
        plain("max by (text, int)", vec![3, 1], Agg::Max(2)),
        plain("min by (col 1, col 0)", vec![1, 0], Agg::Min(2)),
        Shape {
            name: "count over a tuple range, by col 1",
            query: Query::RangeRel {
                rel: WIDE,
                lo: Tuple::from([Value::Ent(Entity(1))]),
                hi: Tuple::from([Value::Ent(Entity(3))]),
            }
            .reduce([1], Agg::Count),
            image: |t| matches!(t.as_slice()[0], Value::Ent(Entity(1 | 2))).then(|| t.clone()),
            key: vec![1],
            agg: Agg::Count,
        },
        Shape {
            name: "min over a column range, by col 0",
            query: Query::RangeRelOn { rel: WIDE, col: 2, lo: Value::Int(1), hi: Value::Int(4) }
                .reduce([0], Agg::Min(2)),
            image: |t| (1..4).contains(&int(t, 2)).then(|| t.clone()),
            key: vec![0],
            agg: Agg::Min(2),
        },
        // The shape the language lowers a one-atom aggregate view to.
        Shape {
            name: "reduce(distinct(project))",
            query: base().project([1, 2]).distinct().reduce([0], Agg::Sum(1)),
            image: |t| Some(Tuple::from([t.as_slice()[1].clone(), t.as_slice()[2].clone()])),
            key: vec![0],
            agg: Agg::Sum(1),
        },
        Shape {
            name: "max over a filter, by col 1",
            query: base().filter(|t| int(t, 2) % 2 == 0).reduce([1], Agg::Max(2)),
            image: |t| (int(t, 2) % 2 == 0).then(|| t.clone()),
            key: vec![1],
            agg: Agg::Max(2),
        },
        Shape {
            name: "sum over shared(project).filter",
            query: base()
                .into_shared()
                .project([3, 0, 2])
                .filter(|t| t.as_slice()[0] == Value::text("x"))
                .reduce([1], Agg::Sum(2)),
            image: |t| {
                (t.as_slice()[3] == Value::text("x")).then(|| {
                    Tuple::from([
                        t.as_slice()[3].clone(),
                        t.as_slice()[0].clone(),
                        t.as_slice()[2].clone(),
                    ])
                })
            },
            key: vec![1],
            agg: Agg::Sum(2),
        },
        // Shapes the per-key path does not take; they must still be right.
        Shape {
            name: "count over a map (falls back)",
            query: base()
                .map(|t| Tuple::from([t.as_slice()[2].clone(), t.as_slice()[0].clone()]))
                .reduce([0], Agg::Count),
            image: |t| Some(Tuple::from([t.as_slice()[2].clone(), t.as_slice()[0].clone()])),
            key: vec![0],
            agg: Agg::Count,
        },
        plain("global sum (falls back)", vec![], Agg::Sum(2)),
    ]
}

/// The independent model: weigh each image, keep the present ones, group and
/// fold.
fn shape_model(present: &BTreeMap<Tuple, Diff>, shape: &Shape) -> Vec<(Tuple, Diff)> {
    let mut images: BTreeMap<Tuple, Diff> = BTreeMap::new();
    for (t, w) in present {
        if let Some(i) = (shape.image)(t) {
            *images.entry(i).or_default() += w;
        }
    }
    let mut groups: BTreeMap<Vec<Value>, Vec<Tuple>> = BTreeMap::new();
    for (i, w) in images {
        if w > 0 {
            let g = shape.key.iter().map(|&c| i.as_slice()[c].clone()).collect();
            groups.entry(g).or_default().push(i);
        }
    }
    groups
        .into_iter()
        .map(|(mut g, members)| {
            g.push(match shape.agg {
                Agg::Count => Value::Int(members.len() as i64),
                Agg::Sum(c) => Value::Int(members.iter().map(|t| int(t, c)).sum()),
                Agg::Min(c) => members.iter().map(|t| t.as_slice()[c].clone()).min().unwrap(),
                Agg::Max(c) => members.iter().map(|t| t.as_slice()[c].clone()).max().unwrap(),
            });
            (Tuple::new(g), 1)
        })
        .collect()
}

fn boundary_difference(q: &Query, store: &dyn TraceStore, from: Edition, to: Edition) -> Vec<(Tuple, Diff)> {
    let mut out = eval_snapshot(q, store, to).unwrap();
    for (t, d) in &eval_snapshot(q, store, from).unwrap() {
        multiset::add(&mut out, t.clone(), -d);
    }
    multiset::strip_zeros(&mut out);
    multiset::to_sorted_vec(&out)
}

fn delta(q: &Query, store: &dyn TraceStore, from: Edition, to: Edition) -> Vec<(Tuple, Diff)> {
    multiset::to_sorted_vec(&eval_delta(q, store, from, to).unwrap())
}

/// **The law, per key.** Over randomized churn — weights in `[-2, 2]`, so they
/// cross zero both ways and change without crossing it — every shape's delta
/// over every interval equals its boundary difference, and every boundary
/// equals the model. On the B+, k-d and runs Ents alike.
#[test]
fn per_key_reduce_deltas_match_the_boundary_on_every_substrate() {
    let shapes = shapes();
    for seed in 1..7u64 {
        for case in grmpl_conformance::each_store() {
            let store = case.store();
            let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut present: BTreeMap<Tuple, Diff> = BTreeMap::new();
            let mut marks = vec![(store.current(), present.clone())];
            for _ in 0..8 + rng.below(8) {
                let mut ups = Vec::new();
                for _ in 0..1 + rng.below(4) {
                    let t = wide(
                        rng.below(4),
                        rng.below(3) as i64,
                        rng.below(5) as i64,
                        ["x", "y"][rng.below(2) as usize],
                    );
                    let d = [-2i64, -1, 1, 2][rng.below(4) as usize];
                    *present.entry(t.clone()).or_default() += d;
                    ups.push((WIDE, t, d));
                }
                present.retain(|_, w| *w != 0);
                store.commit(&ups).unwrap();
                marks.push((store.current(), present.clone()));
            }

            for shape in &shapes {
                let ctx = format!("[{} seed={seed}] {}", case.name, shape.name);
                for (i, (from, at_from)) in marks.iter().enumerate() {
                    assert_eq!(
                        multiset::to_sorted_vec(&eval_snapshot(&shape.query, store, *from).unwrap()),
                        shape_model(at_from, shape),
                        "{ctx}: snapshot at {} disagrees with the model",
                        from.0
                    );
                    for (to, _) in &marks[i..] {
                        assert_eq!(
                            delta(&shape.query, store, *from, *to),
                            boundary_difference(&shape.query, store, *from, *to),
                            "{ctx}: delta over ({}, {}]",
                            from.0,
                            to.0
                        );
                    }
                }
            }
        }
    }
}

/// The cases that break an aggregate maintained by its changes alone: the
/// current extreme retracted (`Min`/`Max` must refold the survivors), a group
/// emptied, and a group whose changed row lies outside the input's range.
#[test]
fn retracting_an_extreme_and_emptying_a_group() {
    let g = |n| Value::Ent(Entity(n));
    for case in grmpl_conformance::each_store() {
        let store = case.store();
        store
            .commit(&[
                (MEASURED, row(1, 1), 1),
                (MEASURED, row(1, 5), 1),
                (MEASURED, row(1, 9), 1),
                (MEASURED, row(2, 4), 1),
            ])
            .unwrap();
        let max = Query::rel(MEASURED).reduce([0], Agg::Max(1));
        let min = Query::rel(MEASURED).reduce([0], Agg::Min(1));
        let t = |k, v: i64| Tuple::from([g(k), Value::Int(v)]);

        let from = store.current();
        store.commit(&[(MEASURED, row(1, 9), -1)]).unwrap();
        assert_eq!(
            delta(&max, store, from, store.current()),
            vec![(t(1, 5), 1), (t(1, 9), -1)],
            "[{}] retracting the max refolds the group",
            case.name
        );

        let from = store.current();
        store.commit(&[(MEASURED, row(1, 1), -1)]).unwrap();
        assert_eq!(
            delta(&min, store, from, store.current()),
            vec![(t(1, 1), -1), (t(1, 5), 1)],
            "[{}] retracting the min refolds the group",
            case.name
        );

        let from = store.current();
        store.commit(&[(MEASURED, row(1, 5), -1), (MEASURED, row(2, 3), 1)]).unwrap();
        assert_eq!(
            delta(&max, store, from, store.current()),
            vec![(t(1, 5), -1)],
            "[{}] an emptied group leaves; a row below the max changes nothing",
            case.name
        );

        // A range that admits only group 2: a change to group 1 is outside it.
        let ranged = Query::RangeRel { rel: MEASURED, lo: Tuple::from([g(2)]), hi: Tuple::from([g(3)]) }
            .reduce([0], Agg::Count);
        let from = store.current();
        store.commit(&[(MEASURED, row(1, 7), 1)]).unwrap();
        assert!(
            delta(&ranged, store, from, store.current()).is_empty(),
            "[{}] a change outside the range touches no group",
            case.name
        );
        let from = store.current();
        store.commit(&[(MEASURED, row(2, 3), -1), (MEASURED, row(2, 4), -1)]).unwrap();
        assert_eq!(
            delta(&ranged, store, from, store.current()),
            vec![(Tuple::from([g(2), Value::Int(2)]), -1)],
            "[{}] emptying the admitted group retracts it",
            case.name
        );
    }
}

// ---------------------------------------------------------------------------
// Cost: a one-row change reads its group, not the relation
// ---------------------------------------------------------------------------

/// An Ent that counts the rows its reads return. Every read primitive is
/// delegated, so the Ent's probes are the ones taken; the reader is left at its
/// default, so a boundary evaluation's reads come back through here too.
struct Counting {
    inner: EntStore,
    rows: AtomicUsize,
    /// Whole-relation reads: what a boundary evaluation does.
    boundaries: AtomicUsize,
    _dir: tempfile::TempDir,
}

impl Counting {
    fn new() -> Counting {
        let dir = tempfile::tempdir().unwrap();
        let inner = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
        Counting { inner, rows: AtomicUsize::new(0), boundaries: AtomicUsize::new(0), _dir: dir }
    }
    fn count<T>(&self, rows: Result<Vec<T>>) -> Result<Vec<T>> {
        if let Ok(r) = &rows {
            self.rows.fetch_add(r.len(), Relaxed);
        }
        rows
    }
    fn take(&self) -> usize {
        self.rows.swap(0, Relaxed)
    }
}

impl EditionStore for Counting {
    fn current(&self) -> Edition {
        self.inner.current()
    }
}

impl TraceStore for Counting {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.inner.commit(updates)
    }
    fn commit_if(&self, pre: &[(RelId, Tuple)], updates: &[(RelId, Tuple, Diff)]) -> Result<Option<Edition>> {
        self.inner.commit_if(pre, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.boundaries.fetch_add(1, Relaxed);
        self.count(self.inner.read_at(rel, at))
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        self.count(self.inner.scan_updates(rel, from, to))
    }
    fn read_range(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.count(self.inner.read_range(rel, at, lo, hi))
    }
    fn read_range_on(
        &self,
        rel: RelId,
        at: Edition,
        col: usize,
        lo: &Value,
        hi: &Value,
    ) -> Result<Vec<(Tuple, Diff)>> {
        self.count(self.inner.read_range_on(rel, at, col, lo, hi))
    }
    fn lookup(&self, rel: RelId, at: Edition, col: usize, keys: &[Value]) -> Result<Vec<(Tuple, Diff)>> {
        self.count(self.inner.lookup(rel, at, col, keys))
    }
    fn read_containing(
        &self,
        rel: RelId,
        at: Edition,
        first: usize,
        last: usize,
        points: &[Value],
    ) -> Result<Vec<(Tuple, Diff)>> {
        self.count(self.inner.read_containing(rel, at, first, last, points))
    }
    fn touched_since(&self, from: Edition, to: Edition, rels: &[RelId]) -> Result<bool> {
        self.inner.touched_since(from, to, rels)
    }
    fn compare(&self, rel: RelId, a: Edition, b: Edition) -> Result<Vec<(Tuple, Diff, Diff)>> {
        self.count(self.inner.compare(rel, a, b))
    }
}

/// **The cost law.** 5,000 rows in 500 groups of 10; one row is added to one
/// group. Maintaining a per-group aggregate over that commit reads the changed
/// row and its group's members — not the relation at both ends, which is what
/// the boundary recompute read. Checked for a key on the lead column (a probe
/// of the primary order) and for the language's `reduce(distinct(project))`
/// keyed by a trailing column (a probe of its Arrangement).
#[test]
fn a_one_row_change_reads_its_group_not_the_relation() {
    const PLAYS: RelId = RelId(3); // (player: Ent, team: Ent, points: Int)
    let store = Counting::new();
    let mut seed = Vec::new();
    for i in 0..5_000u64 {
        seed.push((MEASURED, row(i % 500, i as i64), 1));
        seed.push((
            PLAYS,
            Tuple::from([Value::Ent(Entity(i)), Value::Ent(Entity(i % 500)), Value::Int(i as i64)]),
            1,
        ));
    }
    store.commit(&seed).unwrap();
    let from = store.current();
    store
        .commit(&[
            (MEASURED, row(7, -1), 1),
            (PLAYS, Tuple::from([Value::Ent(Entity(9_999)), Value::Ent(Entity(7)), Value::Int(3)]), 1),
        ])
        .unwrap();
    let to = store.current();

    let by_lead = Query::rel(MEASURED).reduce([0], Agg::Sum(1));
    let by_team = Query::rel(PLAYS).project([1, 2]).distinct().reduce([0], Agg::Sum(1));
    // Build the team column's Arrangement first: it is persisted derived state,
    // made once and maintained by every commit after, not a cost of this delta.
    store.inner.lookup(PLAYS, to, 1, &[Value::Ent(Entity(0))]).unwrap();

    for (name, q) in [("lead key", &by_lead), ("team key", &by_team)] {
        store.take();
        let got = delta(q, &store, from, to);
        let rows = store.take();
        assert_eq!(got, boundary_difference(q, &store, from, to), "{name}: the delta is right");
        store.take();
        eprintln!("{name}: {rows} rows read for a one-row change to a 5,000-row relation");
        // The changed row (from the compare) and the group's 11 members.
        assert!(rows <= 12, "{name}: read {rows} rows; a group holds 11");
    }
}

/// **The route.** The randomized law cannot tell the per-key path from the
/// recompute it replaced — both are right — so this checks which one ran: every
/// shape the per-key path claims is answered with no whole-relation read, and
/// every shape it declines reads both boundaries.
#[test]
fn every_per_key_shape_reads_no_boundary() {
    let store = Counting::new();
    let mut rows = Vec::new();
    for a in 0..4u64 {
        for b in 0..3i64 {
            rows.push((WIDE, wide(a, b, a as i64 + b, ["x", "y"][(a % 2) as usize]), 1));
        }
    }
    store.commit(&rows).unwrap();
    let from = store.current();
    store
        .commit(&[
            (WIDE, wide(3, 2, 5, "y"), -1),
            (WIDE, wide(1, 0, 1, "y"), -1),
            (WIDE, wide(2, 1, 0, "x"), 1),
            (WIDE, wide(3, 1, 8, "y"), 1),
            (WIDE, wide(0, 2, 6, "x"), 1),
        ])
        .unwrap();
    let to = store.current();

    for shape in shapes() {
        store.boundaries.swap(0, Relaxed);
        let got = delta(&shape.query, &store, from, to);
        let boundaries = store.boundaries.swap(0, Relaxed);
        assert!(!got.is_empty(), "{}: the commit moves every shape", shape.name);
        assert_eq!(got, boundary_difference(&shape.query, &store, from, to), "{}", shape.name);
        if shape.per_key() {
            assert_eq!(boundaries, 0, "{}: the per-key path reads no boundary", shape.name);
        } else {
            assert_eq!(boundaries, 2, "{}: a declined shape reads both boundaries", shape.name);
        }
    }
}
