//! **`context` and `inherit`: scope-inherited context in the language.**
//!
//! `context scopes` declares a scope relation `scopes(first, last, key, value)`;
//! a view atom `inherit scopes(e, "key", v)` binds `v` to what entity `e`
//! inherits for `key` from the most specific span containing it. Pinned here:
//!
//! 1. An inner scope overrides an outer one; removing it lets the outer show.
//! 2. **Instancing carries scopes, displaced**: a template's rooms and its
//!    scopes are grafted together, and the instance's rooms inherit from the
//!    instance's copy.
//! 3. A materialized view that inherits answers exactly as the plain one,
//!    through scope changes.
//! 4. Misuse is a compile error.

use std::sync::Arc;

use grmpl::Runtime;
use grmpl_core::{Entity, Tuple, Value, WorldStore};
use grmpl_ent::EntStore;

const BODY: &str = r#"
rel located(thing: Ent, place: Ent)
context scopes

view ambience(viewer) {
    located(viewer, room)
    inherit scopes(room, "ambient", mood)
    yield mood
}

VIEW moods() {
    located(thing, room)
    inherit scopes(room, "ambient", mood)
    yield thing, mood
}
"#;

fn source(materialized: bool) -> String {
    BODY.replace("VIEW", if materialized { "materialized view" } else { "view" })
}

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn scope(first: u64, last: u64, value: &str) -> Tuple {
    Tuple::from([ent(first), ent(last), Value::text("ambient"), Value::text(value)])
}

fn ambience(rt: &Runtime, viewer: u64) -> Vec<Value> {
    let mut rows: Vec<Value> =
        rt.view("ambience", &[ent(viewer)]).unwrap().into_iter().map(|(t, _)| t.as_slice()[0].clone()).collect();
    rows.sort();
    rows
}

#[test]
fn an_inner_scope_overrides_and_instancing_carries_scopes() {
    let ent_store = Arc::new(EntStore::new());
    let store: Arc<dyn WorldStore> = ent_store.clone();
    let rt = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
    let (located, scopes) = (rt.relation("located").unwrap(), rt.relation("scopes").unwrap());

    // A dungeon block [1000, 1099], dim throughout, with a lit shrine room.
    // The player (entity 7) lives outside every block.
    store
        .commit(&[
            (scopes, scope(1_000, 1_099, "dim"), 1),
            (scopes, scope(1_050, 1_050, "candlelit"), 1),
            (located, Tuple::from([ent(7), ent(1_003)]), 1),
            (located, Tuple::from([ent(1_090), ent(1_050)]), 1),
        ])
        .unwrap();
    assert_eq!(ambience(&rt, 7), vec![Value::text("dim")]);
    assert_eq!(ambience(&rt, 1_090), vec![Value::text("candlelit")], "the inner scope overrides");

    // Instance the dungeon: its scopes travel with its rooms, shifted.
    let shift = 5_000;
    ent_store.instance_template(&[located, scopes], 1_000, 1_100, shift).unwrap();
    store
        .commit(&[
            (located, Tuple::from([ent(7), ent(1_003)]), -1),
            (located, Tuple::from([ent(7), ent(6_050)]), 1),
        ])
        .unwrap();
    assert_eq!(ambience(&rt, 7), vec![Value::text("candlelit")], "the instance's shrine is lit too");

    // Douse the instance's shrine; the template's is untouched.
    store.commit(&[(scopes, scope(6_050, 6_050, "candlelit"), -1)]).unwrap();
    assert_eq!(ambience(&rt, 7), vec![Value::text("dim")], "the outer scope shows through");
    assert_eq!(ambience(&rt, 1_090), vec![Value::text("candlelit")]);
}

#[test]
fn a_materialized_view_that_inherits_answers_as_the_plain_one() {
    let store: Arc<dyn WorldStore> = Arc::new(EntStore::new());
    let mat = Runtime::compile(Arc::clone(&store), &source(true), 100).unwrap();
    let plain = Runtime::compile(Arc::clone(&store), &source(false), 100).unwrap();
    let (located, scopes) = (mat.relation("located").unwrap(), mat.relation("scopes").unwrap());
    let mut x = 0x9E37_79B9u64;
    let mut next = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let mut held: Vec<(grmpl_core::RelId, Tuple)> = Vec::new();
    for round in 0..80 {
        let t = if next(2) == 0 {
            let start = next(4) * 25;
            let width = [25, 5, 1][next(3) as usize];
            let start = start + next(25 / width) * width;
            (scopes, scope(start, start + width - 1, &format!("m{}", next(4))))
        } else {
            (located, Tuple::from([ent(next(8)), ent(next(100))]))
        };
        let diff = match held.iter().position(|h| *h == t) {
            Some(i) => {
                held.swap_remove(i);
                -1
            }
            None => {
                held.push(t.clone());
                1
            }
        };
        store.commit(&[(t.0, t.1, diff)]).unwrap();
        if next(2) == 0 {
            mat.refresh_views().unwrap();
        }
        let mut a = mat.view("moods", &[]).unwrap();
        let mut b = plain.view("moods", &[]).unwrap();
        a.sort();
        b.sort();
        assert_eq!(a, b, "round {round}");
    }
}

#[test]
fn misuse_is_refused_when_the_view_is_instantiated() {
    // As for every view, semantic errors surface at instantiation.
    let err = |src: &str, args: &[Value]| {
        let rt = Runtime::compile(Arc::new(EntStore::new()), src, 100).unwrap();
        rt.view("v", args).expect_err("instantiated")
    };
    let e = err(
        "rel located(thing: Ent, place: Ent)\nrel plain(a: Ent, b: Ent, k: Text, v: Text)\nview v(x) {\n located(x, r)\n inherit plain(r, \"k\", m)\n yield m\n}\n",
        &[ent(1)],
    );
    assert!(e.contains("not declared with `context`"), "{e}");
    let e = err("context scopes\nview v() {\n inherit scopes(r, \"k\", m)\n yield m\n}\n", &[]);
    assert!(e.contains("needs an earlier atom"), "{e}");
    let e = err(
        "rel located(thing: Ent, place: Ent)\ncontext scopes\nview v(x) {\n located(x, r)\n inherit scopes(r, k, m)\n yield m\n}\n",
        &[ent(1)],
    );
    assert!(e.contains("key must be a string literal"), "{e}");
}
