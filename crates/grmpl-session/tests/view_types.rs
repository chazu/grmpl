//! Load-time view typing (P8a): the runtime type-checks every declared view
//! against the world's schemas, so a view that can never match fails the load
//! with its name rather than answering empty at every read. Unannotated
//! columns are `Any`, which compares with everything, so an untyped world
//! loads.

use std::sync::Arc;

use grmpl::Runtime;
use grmpl_core::{Edition, Value, WorldStore};
use grmpl_ent::EntStore;
use grmpl_lang::GrantSet;

fn fresh_store() -> (tempfile::TempDir, Arc<dyn WorldStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open_with(dir.path(), grmpl_ent::Durability::Os).unwrap();
    (dir, Arc::new(store))
}

fn load_error(source: &str) -> String {
    let (_dir, store) = fresh_store();
    match Runtime::compile(store, source, 100) {
        Ok(_) => panic!("an ill-typed view must fail the load"),
        Err(error) => error,
    }
}

#[test]
fn a_view_joining_an_int_to_an_ent_fails_the_load() {
    let error = load_error(
        "rel score(who: Ent, points: Int)\n\
         view ok(who) { score(who, points) yield points }\n\
         view crossed() { score(who, points) score(points, more) yield who }",
    );
    assert!(error.contains("view `crossed`"), "{error}");
    assert!(error.contains("Int and Ent"), "{error}");
}

#[test]
fn a_view_comparing_text_to_an_int_fails_the_load() {
    let error = load_error(
        "rel score(who: Ent, points: Int)\n\
         view high() { score(who, \"lots\") yield who }",
    );
    assert!(error.contains("view `high`"), "{error}");
    assert!(error.contains("Int and Text"), "{error}");
}

#[test]
fn a_sum_over_text_fails_the_load() {
    let error = load_error(
        "rel tag(who: Ent, label: Text)\n\
         view total() { tag(who, label) yield who, sum(label) }",
    );
    assert!(error.contains("view `total`"), "{error}");
}

#[test]
fn an_ill_typed_package_view_installs_nothing() {
    let (_dir, store) = fresh_store();
    let source = "package typed bootstrap 1\n\
                  rel score(who: Ent, points: Int)\n\
                  view crossed() { score(who, points) score(points, more) yield who }\n\
                  bootstrap { }";
    let error = match Runtime::load_package(Arc::clone(&store), source, 100, &GrantSet::new()) {
        Ok(_) => panic!("an ill-typed view must fail the load"),
        Err(error) => error,
    };
    assert!(error.contains("view `crossed`"), "{error}");
    assert_eq!(store.current(), Edition::ZERO, "the bootstrap never commits");
    // The views are checked before the schemas are registered, so a refused
    // world leaves none behind to collide with its corrected source.
    for (name, rel) in store.entries().unwrap() {
        assert_eq!(store.schema(rel).unwrap(), None, "`{name}` kept a schema");
    }
}

#[test]
fn an_untyped_world_loads() {
    let (_dir, store) = fresh_store();
    // Every column is `Any`: the shared variables compare with anything.
    let runtime = Runtime::compile(
        store,
        "rel located(thing, room)\nrel named(thing, name)\n\
         view here(viewer) { located(viewer, room) located(thing, room) named(thing, name) \
         yield name }\n\
         view count() { named(thing, name) yield name, sum(thing) }",
        100,
    )
    .unwrap();
    assert!(runtime.view("here", &[Value::text("anyone")]).unwrap().is_empty());
}
