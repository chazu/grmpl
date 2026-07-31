//! **G-5 acceptance: the catalog and schema registry are ent-native, and obey
//! the same laws on the Ent as on the LSM.**
//!
//! `CLAUDE.md` names both load-bearing — the name→`RelId` map is append-only and
//! durable, the schema registry is versioned by the edition each version took
//! effect and may only evolve additively. Until now `EntStore` implemented
//! neither, so a world running on the Ent had no durable names and committed
//! with `NoSchemas`: two of the substrate's four invariants were reachable only
//! from the LSM.
//!
//! Both now ride on the **context enfilade** — bindings at the root scope — so
//! they version, persist, GC-root and range-walk like everything else in the
//! plex. These tests drive the laws against `EntStore` (in memory and across a
//! reopen) and check them **differentially against `FjallStore`**, the same
//! oracle discipline the store contract uses.

use grmpl_core::{Catalog, Column, Edition, RelId, Schema, SchemaCatalog, Ty};
use grmpl_ent::EntStore;
use grmpl_store::FjallStore;

fn schema(cols: &[(&str, Ty)]) -> Schema {
    Schema::new(cols.iter().map(|(n, t)| Column::new(*n, *t)).collect())
}

// --- catalog laws ---------------------------------------------------------

#[test]
fn catalog_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let bound = {
        let store = EntStore::open(dir.path()).unwrap();
        store.register("located", RelId(1)).unwrap();
        store.register("named", RelId(2)).unwrap();
        store.register("held", RelId(5)).unwrap();
        assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)));
        assert_eq!(store.rel_id("absent").unwrap(), None);
        store.entries().unwrap()
    };
    // Reopened from the granfilade, the names still resolve to the same ids.
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.entries().unwrap(), bound, "catalog did not survive reopen");
    assert_eq!(store.rel_id("held").unwrap(), Some(RelId(5)));
    // `entries` is sorted by name — it is a range walk of one contiguous span.
    let names: Vec<String> = bound.iter().map(|(n, _)| n.clone()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "entries must come back in name order");
}

#[test]
fn catalog_is_append_only() {
    let store = EntStore::new();
    store.register("located", RelId(1)).unwrap();
    // Re-registering the same binding is idempotent.
    store.register("located", RelId(1)).unwrap();
    assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)));
    // Rebinding a name to a different id is a hard error, and has no effect.
    assert!(store.register("located", RelId(9)).is_err());
    assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)));
}

/// The ent and the LSM must agree on every catalog observable, driven by the
/// same sequence — the differential oracle, extended to naming.
#[test]
fn catalog_matches_fjall() {
    let dir = tempfile::tempdir().unwrap();
    let ent = EntStore::new();
    let fj = FjallStore::open(dir.path()).unwrap();
    let names = ["located", "named", "held", "exits", "value", "a", "zzz"];
    for (i, name) in names.iter().enumerate() {
        let id = RelId(i as u32 + 1);
        assert_eq!(ent.register(name, id).is_err(), fj.register(name, id).is_err());
    }
    // Conflicts agree.
    assert_eq!(
        ent.register("located", RelId(99)).is_err(),
        fj.register("located", RelId(99)).is_err()
    );
    for name in names.iter().chain(["missing"].iter()) {
        assert_eq!(ent.rel_id(name).unwrap(), fj.rel_id(name).unwrap(), "rel_id({name})");
    }
    assert_eq!(ent.entries().unwrap(), fj.entries().unwrap(), "entries");
}

// --- schema laws ----------------------------------------------------------

#[test]
fn schema_round_trips_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let s = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    {
        let store = EntStore::open(dir.path()).unwrap();
        store.put_schema(RelId(1), &s, Edition(1)).unwrap();
    }
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.schema(RelId(1)).unwrap(), Some(s));
    assert_eq!(store.schema(RelId(2)).unwrap(), None);
}

#[test]
fn additive_evolution_is_versioned_by_edition() {
    let store = EntStore::new();
    let v1 = schema(&[("thing", Ty::Ent)]);
    let v2 = schema(&[("thing", Ty::Ent), ("weight", Ty::Int)]);
    store.put_schema(RelId(1), &v1, Edition(1)).unwrap();
    store.put_schema(RelId(1), &v2, Edition(5)).unwrap();

    // `schema_at` answers as-of: each edition sees the typing of its own era.
    assert_eq!(store.schema_at(RelId(1), Edition(0)).unwrap(), None);
    assert_eq!(store.schema_at(RelId(1), Edition(1)).unwrap(), Some(v1.clone()));
    assert_eq!(store.schema_at(RelId(1), Edition(4)).unwrap(), Some(v1));
    assert_eq!(store.schema_at(RelId(1), Edition(5)).unwrap(), Some(v2.clone()));
    assert_eq!(store.schema_at(RelId(1), Edition(99)).unwrap(), Some(v2.clone()));
    assert_eq!(store.schema(RelId(1)).unwrap(), Some(v2));
}

#[test]
fn re_putting_the_same_schema_is_idempotent() {
    let store = EntStore::new();
    let s = schema(&[("thing", Ty::Ent)]);
    store.put_schema(RelId(1), &s, Edition(1)).unwrap();
    // Same schema at an *earlier* edition is still accepted as a no-op — no new
    // version, so the effective-edition of the binding does not move.
    store.put_schema(RelId(1), &s, Edition(1)).unwrap();
    assert_eq!(store.schema_at(RelId(1), Edition(1)).unwrap(), Some(s));
}

#[test]
fn non_additive_and_non_increasing_changes_are_rejected() {
    let store = EntStore::new();
    let v1 = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    store.put_schema(RelId(1), &v1, Edition(3)).unwrap();

    // Renaming/retyping an existing column is not additive.
    let renamed = schema(&[("thing", Ty::Ent), ("room", Ty::Ent)]);
    assert!(store.put_schema(RelId(1), &renamed, Edition(4)).is_err());
    // Dropping a column is not additive.
    let shrunk = schema(&[("thing", Ty::Ent)]);
    assert!(store.put_schema(RelId(1), &shrunk, Edition(4)).is_err());
    // A genuine extension, but at or before the current version's edition.
    let grown = schema(&[("thing", Ty::Ent), ("place", Ty::Ent), ("weight", Ty::Int)]);
    assert!(store.put_schema(RelId(1), &grown, Edition(3)).is_err());
    assert!(store.put_schema(RelId(1), &grown, Edition(2)).is_err());
    // …and the same extension strictly after it is accepted.
    store.put_schema(RelId(1), &grown, Edition(4)).unwrap();
    assert_eq!(store.schema(RelId(1)).unwrap(), Some(grown));
    // A rejected put left nothing behind.
    assert_eq!(store.schema_at(RelId(1), Edition(3)).unwrap(), Some(v1));
}

/// Per-relation spans really are independent: one relation's evolution is
/// invisible to another's `schema_at`.
#[test]
fn relations_do_not_share_schema_versions() {
    let store = EntStore::new();
    let a = schema(&[("x", Ty::Int)]);
    let b = schema(&[("y", Ty::Text), ("z", Ty::Bool)]);
    store.put_schema(RelId(1), &a, Edition(1)).unwrap();
    store.put_schema(RelId(2), &b, Edition(2)).unwrap();
    assert_eq!(store.schema_at(RelId(1), Edition(99)).unwrap(), Some(a));
    assert_eq!(store.schema_at(RelId(2), Edition(1)).unwrap(), None);
    assert_eq!(store.schema_at(RelId(2), Edition(2)).unwrap(), Some(b));
    assert_eq!(store.schema_at(RelId(3), Edition(99)).unwrap(), None);
}

/// The registry laws under random evolution, checked against the LSM: the ent
/// must accept exactly the same puts and answer exactly the same `schema_at`.
#[test]
fn schema_registry_matches_fjall_under_random_evolution() {
    for seed in 1..=16u64 {
        let dir = tempfile::tempdir().unwrap();
        let ent = EntStore::new();
        let fj = FjallStore::open(dir.path()).unwrap();
        let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15 | 1;
        let mut next = || {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let cols = [("a", Ty::Int), ("b", Ty::Text), ("c", Ty::Bool), ("d", Ty::Ent)];

        for _ in 0..40 {
            let rel = RelId((next() % 3) as u32 + 1);
            let width = (next() % 4) as usize + 1;
            let at = Edition(next() % 12);
            let s = schema(&cols[..width]);
            let ea = ent.put_schema(rel, &s, at);
            let fa = fj.put_schema(rel, &s, at);
            assert_eq!(ea.is_err(), fa.is_err(), "seed {seed}: put_schema verdict diverged");

            for r in 1..=3u32 {
                assert_eq!(
                    ent.schema(RelId(r)).unwrap(),
                    fj.schema(RelId(r)).unwrap(),
                    "seed {seed}: schema({r})"
                );
                for e in 0..12 {
                    assert_eq!(
                        ent.schema_at(RelId(r), Edition(e)).unwrap(),
                        fj.schema_at(RelId(r), Edition(e)).unwrap(),
                        "seed {seed}: schema_at({r}, {e})"
                    );
                }
            }
        }
    }
}

/// The catalog and schema registry are GC-exempt: they are live roots, so
/// collecting orphaned world nodes must never take the world's names with it.
#[test]
fn gc_does_not_collect_the_catalog_or_schemas() {
    use grmpl_core::{EditionStore, TraceStore, Tuple, Value};

    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    store.register("located", RelId(1)).unwrap();
    let s = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    store.put_schema(RelId(1), &s, Edition(1)).unwrap();

    // Churn enough to orphan plenty of nodes, then consolidate and collect.
    for i in 0..200i64 {
        store.commit(&[(RelId(1), Tuple::from([Value::Int(i)]), 1)]).unwrap();
    }
    store.consolidate(store.current()).unwrap();
    store.gc().unwrap();

    assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)), "GC collected the catalog");
    assert_eq!(store.schema(RelId(1)).unwrap(), Some(s.clone()), "GC collected the schemas");

    // …and they are still there after a reopen, i.e. the surviving nodes really
    // are the persisted ones, not in-memory leftovers.
    drop(store);
    let store = EntStore::open(dir.path()).unwrap();
    assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)));
    assert_eq!(store.schema(RelId(1)).unwrap(), Some(s));
}
