//! **G-5 acceptance: the catalog and schema registry are ent-native.**
//!
//! `CLAUDE.md` names both load-bearing — the name→`RelId` map is append-only and
//! durable, the schema registry is versioned by the edition each version took
//! effect and may only evolve additively. Until now `EntStore` implemented
//! neither, so a world running on the Ent had no durable names and committed
//! with `NoSchemas`: two of the substrate's four invariants had no ent-native
//! implementation at all.
//!
//! Both now ride on the **context enfilade** — bindings at the root scope — so
//! they version, persist, GC-root and range-walk like everything else in the
//! plex. These tests state the laws absolutely — in memory, across a reopen, and
//! under randomized evolution — rather than as agreement with another store.

use grmpl_core::{Catalog, Column, Edition, RelId, Schema, SchemaCatalog, Ty};
use grmpl_ent::EntStore;

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

/// The evolution law under randomized puts, stated directly: a put is accepted
/// exactly when it is the first schema, an identical re-put, or a strict
/// additive extension at a strictly later edition — and a rejected put leaves
/// the registry untouched.
#[test]
fn schema_evolution_laws_hold_under_random_evolution() {
    for seed in 1..=16u64 {
        let store = EntStore::new();
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

            // What the law says should happen, decided before the call.
            let before = store.schema(rel).unwrap();
            let before_at: Vec<Option<Schema>> =
                (0..12).map(|e| store.schema_at(rel, Edition(e)).unwrap()).collect();
            let expected_ok = match &before {
                None => true,
                Some(cur) if cur == &s => true,
                Some(cur) => s.is_additive_over(cur) && at.0 > current_edition(&store, rel),
            };

            let got = store.put_schema(rel, &s, at);
            assert_eq!(got.is_ok(), expected_ok, "seed {seed}: verdict for width {width} at {at:?}");

            if got.is_err() {
                // A rejected put changes nothing, at any edition.
                assert_eq!(store.schema(rel).unwrap(), before, "seed {seed}: rejected put mutated");
                for (e, want) in before_at.iter().enumerate() {
                    assert_eq!(
                        &store.schema_at(rel, Edition(e as u64)).unwrap(),
                        want,
                        "seed {seed}: rejected put moved schema_at({e})"
                    );
                }
            } else {
                // `schema_at` is monotone in the edition: once a version is in
                // force it stays in force until a later one supersedes it.
                let mut last: Option<Schema> = None;
                for e in 0..12u64 {
                    let here = store.schema_at(rel, Edition(e)).unwrap();
                    if let (Some(p), Some(h)) = (&last, &here) {
                        assert!(
                            h == p || h.is_additive_over(p),
                            "seed {seed}: schema_at went backwards between {} and {e}",
                            e - 1
                        );
                    }
                    last = here;
                }
            }
        }
    }
}

/// The edition the current version of `rel` took effect — the floor a later
/// version must strictly exceed.
fn current_edition(store: &EntStore, rel: RelId) -> u64 {
    let cur = match store.schema(rel).unwrap() {
        None => return 0,
        Some(c) => c,
    };
    (0..12u64)
        .find(|e| store.schema_at(rel, Edition(*e)).unwrap().as_ref() == Some(&cur))
        .unwrap_or(0)
}
