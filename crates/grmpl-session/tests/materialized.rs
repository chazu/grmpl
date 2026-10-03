//! **`materialized view`: the Derived enfilade in the language.**
//!
//! A materialized view is stored in the world — its open form, parameters as
//! leading columns, in a backing relation — and maintained by
//! `Runtime::refresh_views`. Pinned here:
//!
//! 1. **It never changes an answer.** Over random churn, read with and without
//!    refreshes, a materialized view returns exactly what the same view
//!    declared plainly returns.
//! 2. **A current copy is read, not evaluated**, and a stale one is not read.
//!    Shown by planting a row in the backing relation that the view could
//!    never produce: it is visible while the cursor is current, and gone the
//!    moment a base relation moves.
//! 3. **It survives a reopen** as stored rows, with nothing recomputed.
//! 4. A parameter the body does not bind is a compile error.

use std::sync::Arc;

use grmpl::Runtime;
use grmpl_core::{Entity, Tuple, Value, WorldStore};
use grmpl_ent::EntStore;

const BODY: &str = r#"
rel located(thing: Ent, place: Ent)
rel named(thing: Ent, name: Text)
rel value(thing: Ent, coins: Int)

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

VIEW wealth() {
    located(thing, room)
    value(thing, coins)
    yield room, sum(coins)
}
"#;

fn source(materialized: bool) -> String {
    BODY.replace("VIEW", if materialized { "materialized view" } else { "view" })
}

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
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

fn sorted(mut rows: Vec<(Tuple, i64)>) -> Vec<(Tuple, i64)> {
    rows.sort();
    rows
}

fn reads(rt: &Runtime) -> Vec<Vec<(Tuple, i64)>> {
    let mut out = vec![sorted(rt.view("world", &[]).unwrap()), sorted(rt.view("wealth", &[]).unwrap())];
    for viewer in 0..6 {
        out.push(sorted(rt.view("here", &[ent(viewer)]).unwrap()));
    }
    out
}

#[test]
fn a_materialized_view_answers_exactly_as_the_plain_view() {
    for seed in 1..=6u64 {
        let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
        let mat = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
        // The plain twin reads the same store through the same relation ids.
        let plain = Runtime::compile(Arc::clone(&store), &source(false), 100).unwrap();
        let (located, named, value) =
            (mat.relation("located").unwrap(), mat.relation("named").unwrap(), mat.relation("value").unwrap());
        assert_eq!(plain.relation("located").unwrap(), located);

        let mut rng = Rng(seed);
        let mut where_: Vec<Option<u64>> = vec![None; 6];
        for round in 0..60 {
            let thing = rng.below(6);
            let mut ups = Vec::new();
            if let Some(old) = where_[thing as usize] {
                ups.push((located, Tuple::from([ent(thing), ent(old)]), -1));
            }
            let place = 100 + rng.below(3);
            ups.push((located, Tuple::from([ent(thing), ent(place)]), 1));
            where_[thing as usize] = Some(place);
            if round < 6 {
                ups.push((named, Tuple::from([ent(thing), Value::text(format!("t{thing}"))]), 1));
                ups.push((value, Tuple::from([ent(thing), Value::Int(thing as i64 + 1)]), 1));
            }
            store.commit(&ups).unwrap();
            // Refresh only sometimes, so reads see both current and stale copies.
            if rng.below(2) == 0 {
                mat.refresh_views().unwrap();
            }
            assert_eq!(reads(&mat), reads(&plain), "seed {seed} round {round}");
        }
    }
}

#[test]
fn a_current_copy_is_read_and_a_stale_one_is_not() {
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
    let rt = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
    let located = rt.relation("located").unwrap();
    let named = rt.relation("named").unwrap();
    let backing = rt.relation("view:here").unwrap();
    store
        .commit(&[
            (located, Tuple::from([ent(1), ent(100)]), 1),
            (named, Tuple::from([ent(1), Value::text("lamp")]), 1),
        ])
        .unwrap();
    assert!(rt.refresh_views().unwrap() > 0);

    // A row no evaluation of `here` could produce: viewer 1 sees a ghost.
    let ghost = Tuple::from([ent(1), ent(999), Value::text("ghost")]);
    store.commit(&[(backing, ghost, 1)]).unwrap();
    let names = |rt: &Runtime| -> Vec<Value> {
        sorted(rt.view("here", &[ent(1)]).unwrap()).into_iter().map(|(t, _)| t.as_slice()[1].clone()).collect()
    };
    assert_eq!(names(&rt), vec![Value::text("lamp"), Value::text("ghost")], "the stored copy was not read");

    // Something moves: the copy is no longer provably current, so the view
    // is evaluated and the ghost is gone.
    store.commit(&[(named, Tuple::from([ent(2), Value::text("key")]), 1)]).unwrap();
    assert_eq!(names(&rt), vec![Value::text("lamp")], "a stale copy was read");
}

#[test]
fn a_materialized_view_survives_a_reopen_as_stored_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir.path()).unwrap());
        let rt = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
        let (located, named) = (rt.relation("located").unwrap(), rt.relation("named").unwrap());
        store
            .commit(&[
                (located, Tuple::from([ent(1), ent(100)]), 1),
                (named, Tuple::from([ent(1), Value::text("lamp")]), 1),
            ])
            .unwrap();
        rt.refresh_views().unwrap();
        let ghost = Tuple::from([ent(7), Value::text("ghost")]);
        store.commit(&[(rt.relation("view:world").unwrap(), ghost, 1)]).unwrap();
    }
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::open(dir.path()).unwrap());
    let rt = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
    // The ghost proves the answer came from the stored rows, not a rebuild.
    assert_eq!(sorted(rt.view("world", &[]).unwrap()).len(), 2);
    assert_eq!(rt.refresh_views().unwrap(), 0, "nothing changed, so nothing to fold in");
}

#[test]
fn a_parameter_the_body_does_not_bind_is_refused() {
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
    let src = "rel named(thing: Ent, name: Text)\nmaterialized view odd(who) {\n named(thing, name)\n yield name\n}\n";
    let err = Runtime::compile(store, src, 100).err().expect("compiled");
    assert!(err.contains("parameter `who` must appear in the body"), "{err}");
}

#[test]
fn a_package_installs_its_views_in_the_bootstrap_edition() {
    use grmpl_core::Edition;
    let src = r#"
package mat_world bootstrap 1
entity WORLD = 1
rel named(thing: Ent, name: Text)
materialized view names() {
    named(thing, name)
    yield thing, name
}
bootstrap {
    named(WORLD, "Test World")
}
"#;
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
    let rt = Runtime::load_package(Arc::clone(&store), src, 100, &grmpl_lang::GrantSet::new()).unwrap();
    assert_eq!(store.current(), Edition(1), "the views did not install with the bootstrap");
    assert_eq!(rt.refresh_views().unwrap(), 1, "the bootstrap's row was not materialized");
    assert_eq!(rt.view("names", &[]).unwrap(), vec![(Tuple::from([ent(1), Value::text("Test World")]), 1)]);
}

#[test]
fn a_materialized_views_delta_is_the_plain_views_delta() {
    use grmpl_core::TraceStore;
    use grmpl_diff::{eval_delta, multiset};
    for seed in 1..=4u64 {
        let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
        let mat = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
        let plain = Runtime::compile(Arc::clone(&store), &source(false), 100).unwrap();
        let (located, named) = (mat.relation("located").unwrap(), mat.relation("named").unwrap());
        let mut rng = Rng(seed);
        let mut eds = vec![store.current()];
        let mut where_: Vec<Option<u64>> = vec![None; 6];
        for round in 0..40 {
            let thing = rng.below(6);
            let mut ups = Vec::new();
            if let Some(old) = where_[thing as usize] {
                ups.push((located, Tuple::from([ent(thing), ent(old)]), -1));
            }
            let place = 100 + rng.below(3);
            ups.push((located, Tuple::from([ent(thing), ent(place)]), 1));
            where_[thing as usize] = Some(place);
            if round < 6 {
                ups.push((named, Tuple::from([ent(thing), Value::text(format!("t{thing}"))]), 1));
            }
            eds.push(store.commit(&ups).unwrap());
            if rng.below(2) == 0 {
                mat.refresh_views().unwrap();
                eds.push(store.current());
            }
        }
        let trace: &dyn TraceStore = store.as_ref();
        for _ in 0..60 {
            let (a, b) = (eds[rng.below(eds.len() as u64) as usize], eds[rng.below(eds.len() as u64) as usize]);
            let (from, to) = (a.min(b), a.max(b));
            let mut queries = vec![("world".to_string(), vec![])];
            for viewer in 0..6 {
                queries.push(("here".to_string(), vec![ent(viewer)]));
            }
            for (name, args) in queries {
                let got = eval_delta(&mat.query(&name, &args).unwrap(), trace, from, to).unwrap();
                let want = eval_delta(&plain.query(&name, &args).unwrap(), trace, from, to).unwrap();
                assert_eq!(
                    multiset::to_sorted_vec(&got),
                    multiset::to_sorted_vec(&want),
                    "seed {seed} {name}{args:?} ({from:?}, {to:?}]"
                );
            }
        }
    }
}
