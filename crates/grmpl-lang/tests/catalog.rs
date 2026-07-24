//! TKT-90: `Program::compile_with_catalog` recovers stable `RelId`s from the
//! durable [`Catalog`] instead of assigning them from declaration order.
//!
//! The property under test is that a relation's id, once bound, survives a
//! reopen and a source reshuffle — the whole point of TKT-72's durable catalog.
//! Contrast with plain [`Program::compile`], whose ids follow declaration order
//! and so drift when the source is reordered.

use std::collections::HashMap;
use std::sync::Mutex;

use grmpl_core::{Catalog, RelId, Result};
use grmpl_lang::Program;
use grmpl_store::FjallStore;

/// A minimal in-memory [`Catalog`] — enough to exercise resolve/allocate
/// without touching disk. Mirrors the store contract: append-only, rebinding to
/// a different id is an error.
#[derive(Default)]
struct MemCatalog {
    map: Mutex<HashMap<String, RelId>>,
}

impl Catalog for MemCatalog {
    fn rel_id(&self, name: &str) -> Result<Option<RelId>> {
        Ok(self.map.lock().unwrap().get(name).copied())
    }

    fn register(&self, name: &str, id: RelId) -> Result<()> {
        let mut m = self.map.lock().unwrap();
        match m.get(name) {
            Some(existing) if *existing != id => Err(grmpl_core::Error::Store(format!(
                "`{name}` already bound to {existing:?}, not {id:?}"
            ))),
            _ => {
                m.insert(name.to_string(), id);
                Ok(())
            }
        }
    }

    fn entries(&self) -> Result<Vec<(String, RelId)>> {
        let mut v: Vec<_> = self
            .map
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.sort();
        Ok(v)
    }
}

#[test]
fn compile_registers_fresh_names_into_the_catalog() {
    let cat = MemCatalog::default();
    let prog = Program::compile_with_catalog("rel a(x)\nrel b(y)\nrel c(z)", &cat, 1).unwrap();

    // Ids were handed out sequentially from the base for a fresh catalog…
    assert_eq!(prog.rel_id("a"), Some(RelId(1)));
    assert_eq!(prog.rel_id("b"), Some(RelId(2)));
    assert_eq!(prog.rel_id("c"), Some(RelId(3)));

    // …and every one was durably registered.
    assert_eq!(
        cat.entries().unwrap(),
        vec![
            ("a".to_string(), RelId(1)),
            ("b".to_string(), RelId(2)),
            ("c".to_string(), RelId(3)),
        ]
    );
}

#[test]
fn reordering_declarations_does_not_change_ids() {
    let cat = MemCatalog::default();
    let first = Program::compile_with_catalog("rel a(x)\nrel b(y)\nrel c(z)", &cat, 1).unwrap();
    let a = first.rel_id("a").unwrap();
    let b = first.rel_id("b").unwrap();
    let c = first.rel_id("c").unwrap();

    // Recompile the *same* relations in a different order against the same
    // catalog: plain `compile` would give `c` id 1 here, but the catalog pins
    // each name to the id it was first bound to.
    let second = Program::compile_with_catalog("rel c(z)\nrel a(x)\nrel b(y)", &cat, 1).unwrap();
    assert_eq!(second.rel_id("a"), Some(a));
    assert_eq!(second.rel_id("b"), Some(b));
    assert_eq!(second.rel_id("c"), Some(c));
}

#[test]
fn a_new_relation_gets_a_fresh_id_above_every_bound_id() {
    let cat = MemCatalog::default();
    let first = Program::compile_with_catalog("rel a(x)\nrel b(y)", &cat, 1).unwrap();
    assert_eq!(first.rel_id("a"), Some(RelId(1)));
    assert_eq!(first.rel_id("b"), Some(RelId(2)));

    // Add a relation *before* the existing ones in the source. It must not
    // reuse id 1 (which belongs to `a`); it lands above the high-water mark.
    let second =
        Program::compile_with_catalog("rel d(w)\nrel a(x)\nrel b(y)", &cat, 1).unwrap();
    assert_eq!(second.rel_id("a"), Some(RelId(1)));
    assert_eq!(second.rel_id("b"), Some(RelId(2)));
    assert_eq!(second.rel_id("d"), Some(RelId(3)));
}

#[test]
fn fresh_ids_never_collide_with_ids_below_the_base() {
    // Pre-bind a name to an id *at* the base, then compile with a base that
    // would otherwise start there. The new relation must skip past it.
    let cat = MemCatalog::default();
    cat.register("legacy", RelId(5)).unwrap();

    let prog = Program::compile_with_catalog("rel fresh(x)", &cat, 5).unwrap();
    assert_eq!(prog.rel_id("fresh"), Some(RelId(6)));
    assert_eq!(cat.rel_id("legacy").unwrap(), Some(RelId(5)));
}

#[test]
fn ids_are_stable_across_a_store_reopen() {
    let dir = tempfile::tempdir().unwrap();

    // First open: compile against the durable catalog, capturing the ids.
    let (a, b, c) = {
        let store = FjallStore::open(dir.path()).unwrap();
        let prog =
            Program::compile_with_catalog("rel a(x)\nrel b(y)\nrel c(z)", &store, 1).unwrap();
        (
            prog.rel_id("a").unwrap(),
            prog.rel_id("b").unwrap(),
            prog.rel_id("c").unwrap(),
        )
    };

    // Reopen the same directory and recompile with the declarations reordered.
    // The persisted catalog recovers each id regardless of source layout.
    let store = FjallStore::open(dir.path()).unwrap();
    let prog =
        Program::compile_with_catalog("rel b(y)\nrel c(z)\nrel a(x)", &store, 1).unwrap();
    assert_eq!(prog.rel_id("a"), Some(a));
    assert_eq!(prog.rel_id("b"), Some(b));
    assert_eq!(prog.rel_id("c"), Some(c));
}

#[test]
fn register_schemas_is_idempotent_after_compile_with_catalog() {
    // Compiling against the catalog binds ids; a subsequent `register_schemas`
    // against the same catalog re-binds the *same* ids (a no-op) and only adds
    // the schemas — the two paths compose without conflict.
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    let prog =
        Program::compile_with_catalog("rel located(thing: Ent, place: Ent)", &store, 1).unwrap();
    let located = prog.rel_id("located").unwrap();

    prog.register_schemas(&store, &store, grmpl_core::Edition(1))
        .unwrap();

    assert_eq!(Catalog::rel_id(&store, "located").unwrap(), Some(located));
    assert_eq!(
        prog.schema("located"),
        grmpl_core::SchemaCatalog::schema(&store, located).unwrap()
    );
}
