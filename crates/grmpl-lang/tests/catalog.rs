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

// --- Randomized-churn law oracle (TKT-90) -----------------------------------
//
// The example tests above pin single interleavings. The steward bar for the
// catalog-stability law is that it holds under *arbitrary* compile order and
// growth, not just the hand-picked cases above. So this test churns many random
// compiles of random name subsets — in random declaration order — against ONE
// shared catalog, and checks every round:
//
//   (a) every previously-seen name keeps its first-assigned id (stability, the
//       whole point of TKT-90: physical ids never depend on source layout),
//   (b) all ids handed out in a program are distinct (no two names collide), and
//   (c) each brand-new name's id is strictly above the prior high-water mark.
//
// A tiny xorshift64* PRNG keeps the churn reproducible with no external `rand`
// dependency (mirrors grmpl-store's determinism.rs); every assertion prints its
// `seed` so a failure replays directly.

/// Deterministic, seedable PRNG (xorshift64*). Reproducible churn without a
/// `rand`/`proptest` dependency in the language's test harness.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point of xorshift.
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

    /// Uniform-ish value in `0..n` (`n > 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// The universe of relation names. A round compiles a random subset of these;
/// a name selected for the first time is "brand-new" and its fresh id must land
/// above the high-water mark. Not every name appears early, so new names keep
/// entering the catalog across rounds.
const NAMES: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    "india", "juliet", "kilo", "lima",
];

#[test]
fn catalog_ids_are_stable_and_fresh_under_random_churn() {
    for seed in 1..=32u64 {
        churn_round(seed);
    }
}

fn churn_round(seed: u64) {
    let cat = MemCatalog::default();
    let mut rng = Rng::new(seed);

    // Oracle: the first id each name was ever assigned, and the highest id
    // handed out so far. `rel_base` is 1, matching a fresh catalog's first id.
    let mut first_id: HashMap<String, RelId> = HashMap::new();
    let mut high_water = 0u32;

    let rounds = 8 + rng.below(12); // 8..=19 compiles against the shared catalog
    for _ in 0..rounds {
        // A random, randomly-ordered subset of the universe (at least one name).
        let mut subset: Vec<&str> = NAMES.iter().copied().filter(|_| rng.below(2) == 0).collect();
        if subset.is_empty() {
            subset.push(NAMES[rng.below(NAMES.len() as u64) as usize]);
        }
        // Fisher-Yates shuffle so declaration order is random each round — the
        // property must not depend on it.
        for i in (1..subset.len()).rev() {
            let j = rng.below((i + 1) as u64) as usize;
            subset.swap(i, j);
        }

        let src: String = subset.iter().map(|n| format!("rel {n}(x)\n")).collect();
        let prog = Program::compile_with_catalog(&src, &cat, 1).unwrap();

        // Snapshot the high-water mark *before* this round: every fresh id must
        // clear it, whichever order the names were declared in.
        let prior_high_water = high_water;
        let mut seen_this_round: HashMap<RelId, &str> = HashMap::new();
        for name in &subset {
            let id = prog.rel_id(name).unwrap();

            // (b) distinct: no two names in this program share an id.
            if let Some(other) = seen_this_round.insert(id, name) {
                panic!("seed {seed}: `{name}` and `{other}` both got {id:?}");
            }

            match first_id.get(*name) {
                // (a) stability: a previously-seen name keeps its first id.
                Some(&want) => assert_eq!(
                    id, want,
                    "seed {seed}: `{name}` drifted from {want:?} to {id:?}"
                ),
                // (c) freshness: a brand-new name lands strictly above the mark.
                None => {
                    assert!(
                        id.0 > prior_high_water,
                        "seed {seed}: fresh `{name}` got {id:?}, not above high-water {prior_high_water}"
                    );
                    first_id.insert(name.to_string(), id);
                    high_water = high_water.max(id.0);
                }
            }
        }
    }
}
