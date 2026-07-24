//! P1 (TKT-73): the durable schema registry. Schemas round-trip across a store
//! reopen, evolve **additively** and are **versioned by edition** (so
//! `schema_at` answers as-of queries), and reject any non-additive change.
//!
//! The final test is a randomized-churn **law oracle** (matching the fleet's
//! determinism-test idiom): over many seeds it grows a relation's schema by
//! random additive extensions at increasing editions and, after every step,
//! checks the registry against an independent model — `schema` is the latest
//! version, `schema_at(e)` is the newest version introduced at edition ≤ e, and
//! a random *non-additive* mutation is always rejected.

use grmpl_core::{Column, Edition, RelId, Schema, SchemaCatalog, Ty};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn schema(cols: &[(&str, Ty)]) -> Schema {
    Schema::new(cols.iter().map(|(n, t)| Column::new(*n, *t)).collect())
}

#[test]
fn schema_round_trips_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let s = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    {
        let store = FjallStore::open(dir.path()).unwrap();
        store.put_schema(R, &s, Edition(1)).unwrap();
        assert_eq!(store.schema(R).unwrap(), Some(s.clone()));
    }
    // Reopen: the durable registry survives.
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.schema(R).unwrap(), Some(s));
    assert_eq!(store.schema(RelId(99)).unwrap(), None, "unregistered rel has no schema");
}

#[test]
fn additive_evolution_is_versioned_by_edition() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    let v1 = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    let v2 = schema(&[("thing", Ty::Ent), ("place", Ty::Ent), ("since", Ty::Int)]);
    store.put_schema(R, &v1, Edition(2)).unwrap();
    store.put_schema(R, &v2, Edition(5)).unwrap();

    // Latest is v2.
    assert_eq!(store.schema(R).unwrap(), Some(v2.clone()));
    // As-of reads see the version in effect at each edition.
    assert_eq!(store.schema_at(R, Edition(1)).unwrap(), None, "before any schema");
    assert_eq!(store.schema_at(R, Edition(2)).unwrap(), Some(v1.clone()));
    assert_eq!(store.schema_at(R, Edition(4)).unwrap(), Some(v1));
    assert_eq!(store.schema_at(R, Edition(5)).unwrap(), Some(v2.clone()));
    assert_eq!(store.schema_at(R, Edition(9)).unwrap(), Some(v2));
}

#[test]
fn re_putting_the_same_schema_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let s = schema(&[("a", Ty::Int)]);
    store.put_schema(R, &s, Edition(1)).unwrap();
    // Same schema at a later edition: no-op, no new version, still one version.
    store.put_schema(R, &s, Edition(3)).unwrap();
    assert_eq!(store.schema_at(R, Edition(1)).unwrap(), Some(s.clone()));
    // The version's introducing edition is unchanged (still 1, not 3).
    assert_eq!(store.schema_at(R, Edition(2)).unwrap(), Some(s));
}

#[test]
fn non_additive_change_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let base = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
    store.put_schema(R, &base, Edition(1)).unwrap();

    // Removal, retype, rename, reorder — all rejected.
    for bad in [
        schema(&[("thing", Ty::Ent)]),                          // removal
        schema(&[("thing", Ty::Ent), ("place", Ty::Int)]),      // retype
        schema(&[("thing", Ty::Ent), ("room", Ty::Ent)]),       // rename
        schema(&[("place", Ty::Ent), ("thing", Ty::Ent)]),      // reorder
    ] {
        let err = store.put_schema(R, &bad, Edition(2)).unwrap_err();
        assert!(matches!(err, grmpl_core::Error::Schema(_)), "expected Schema error, got {err:?}");
    }
    // The registry is unchanged after the rejected writes.
    assert_eq!(store.schema(R).unwrap(), Some(base));
}

#[test]
fn additive_at_a_non_increasing_edition_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let v1 = schema(&[("a", Ty::Int)]);
    let v2 = schema(&[("a", Ty::Int), ("b", Ty::Int)]);
    store.put_schema(R, &v1, Edition(5)).unwrap();
    // A genuine extension, but at an edition not after the current version's.
    let err = store.put_schema(R, &v2, Edition(5)).unwrap_err();
    assert!(matches!(err, grmpl_core::Error::Schema(_)), "got {err:?}");
}

// ---- randomized law oracle ------------------------------------------------

/// Deterministic, seedable PRNG (xorshift64*) — reproducible without a `rand`
/// dependency; every assertion prints its `seed` for direct replay. Matches the
/// idiom in `determinism.rs`.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

const TYS: [Ty; 5] = [Ty::Ent, Ty::Int, Ty::Text, Ty::Bool, Ty::Any];

#[test]
fn schema_registry_laws_hold_under_random_evolution() {
    for seed in 1..=32u64 {
        evolution_round(seed);
    }
}

fn evolution_round(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let mut rng = Rng::new(seed);

    // Independent model: each recorded version as (edition, schema), editions
    // strictly increasing.
    let mut model: Vec<(u64, Schema)> = Vec::new();
    let mut columns: Vec<Column> = Vec::new();
    let mut edition = 0u64;

    let steps = 3 + rng.below(6); // 3..=8 additive steps
    for step in 0..steps {
        // Append 1..=3 fresh columns → a strict additive extension.
        let add = 1 + rng.below(3);
        for _ in 0..add {
            let ty = TYS[rng.below(TYS.len() as u64) as usize];
            columns.push(Column::new(format!("c{}", columns.len()), ty));
        }
        edition += 1 + rng.below(4); // strictly increasing editions
        let s = Schema::new(columns.clone());
        store
            .put_schema(R, &s, Edition(edition))
            .unwrap_or_else(|e| panic!("seed {seed} step {step}: additive put rejected: {e:?}"));
        model.push((edition, s));

        // Law 1: `schema` is the latest recorded version.
        assert_eq!(
            store.schema(R).unwrap().as_ref(),
            model.last().map(|(_, s)| s),
            "seed {seed} step {step}: schema() must be the latest version"
        );

        // Law 2: `schema_at(e)` is the newest version with introducing edition ≤ e.
        for probe in 0..=(edition + 2) {
            let want = model.iter().rfind(|(e, _)| *e <= probe).map(|(_, s)| s);
            assert_eq!(
                store.schema_at(R, Edition(probe)).unwrap().as_ref(),
                want,
                "seed {seed} step {step}: schema_at({probe}) disagrees with the model"
            );
        }

        // Law 3: an idempotent re-put of the current version records no new
        // version and does not move its introducing edition.
        store.put_schema(R, &model.last().unwrap().1, Edition(edition + 1)).unwrap();
        assert_eq!(
            store.schema_at(R, Edition(edition)).unwrap().as_ref(),
            model.last().map(|(_, s)| s),
            "seed {seed} step {step}: idempotent re-put must not add a version"
        );

        // Law 4: a non-additive mutation (drop the last column, when there is
        // more than one) is always rejected, leaving the registry unchanged.
        if columns.len() > 1 {
            let shorter = Schema::new(columns[..columns.len() - 1].to_vec());
            let err = store.put_schema(R, &shorter, Edition(edition + 5)).unwrap_err();
            assert!(
                matches!(err, grmpl_core::Error::Schema(_)),
                "seed {seed} step {step}: non-additive change must be a Schema error, got {err:?}"
            );
            assert_eq!(
                store.schema(R).unwrap().as_ref(),
                model.last().map(|(_, s)| s),
                "seed {seed} step {step}: rejected write must not mutate the registry"
            );
        }
    }
}
