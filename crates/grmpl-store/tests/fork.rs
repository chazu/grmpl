//! Forks v1 (P10): copy the whole domain, bit-identical, then evolve
//! independently. Cost is O(domain) — the physical store, not O(history).
//!
//! Fork identity / clock semantics are deferred to P15; v1 is the flat copy.

use grmpl_core::{
    Catalog, Column, Diff, Edition, EditionStore, Entity, RelId, Schema, SchemaCatalog, TraceStore,
    Tuple, Ty, Value,
};
use grmpl_store::FjallStore;

const R: RelId = RelId(1);

fn ent(a: u64) -> Tuple {
    Tuple::from([Value::Ent(Entity(a))])
}

fn thing_schema() -> Schema {
    Schema::new(vec![Column::new("thing", Ty::Ent)])
}

/// A store with a few editions of churn, a catalog binding and a schema.
fn seeded() -> (FjallStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.commit(&[(R, ent(1), 1), (R, ent(2), 1)]).unwrap(); // e1
    store.commit(&[(R, ent(3), 1), (R, ent(1), -1)]).unwrap(); // e2 → {2,3}
    store.register("things", R).unwrap();
    store.put_schema(R, &thing_schema(), Edition(1)).unwrap();
    (store, dir)
}

#[test]
fn fork_is_a_bit_identical_copy() {
    let (store, _dir) = seeded();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();

    // The whole physical store copied byte for byte.
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "a fork is bit-identical to its source at the moment of the copy"
    );

    // …and every observable surface agrees: clock, watermark, data, catalog,
    // schema.
    assert_eq!(fork.current(), store.current());
    assert_eq!(TraceStore::watermark(&fork), TraceStore::watermark(&store));
    assert_eq!(
        fork.read_at(R, Edition(2)).unwrap(),
        vec![(ent(2), 1), (ent(3), 1)]
    );
    assert_eq!(
        fork.read_at(R, Edition(1)).unwrap(),
        store.read_at(R, Edition(1)).unwrap()
    );
    assert_eq!(Catalog::rel_id(&fork, "things").unwrap(), Some(R));
    assert_eq!(
        SchemaCatalog::schema(&fork, R).unwrap(),
        Some(thing_schema())
    );
}

#[test]
fn fork_evolves_independently_then_replay_reconverges() {
    let (store, _dir) = seeded();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();
    let base_edition = store.current();

    // Commit only to the source: the fork does not move.
    store.commit(&[(R, ent(9), 1)]).unwrap();
    assert_eq!(fork.current(), base_edition, "the fork's clock is its own");
    assert_ne!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "independent evolution diverges the two stores"
    );

    // Replaying the *same* commit onto the fork reconverges it bit-identically —
    // the checkpoint is a fork point, and identical inputs reproduce the world.
    fork.commit(&[(R, ent(9), 1)]).unwrap();
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "identical commits on a fork reproduce the source bit for bit"
    );
    assert_eq!(fork.current(), store.current());
}

#[test]
fn fork_does_not_disturb_the_source() {
    let (store, _dir) = seeded();
    let before: Vec<(String, Vec<u8>, Vec<u8>)> = store.canonical_dump().unwrap();
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();

    // A write to the fork must not touch the source.
    fork.commit(&[(R, ent(42), 1)]).unwrap();
    assert_eq!(
        store.canonical_dump().unwrap(),
        before,
        "forking then writing the fork leaves the source untouched"
    );
}

// --- Randomized-churn law oracle (fork bit-identity under arbitrary history) --
//
// The example tests above pin *one* fixed trace. The steward bar for a
// determinism/replay law (CLAUDE.md §Determinism) is that it holds under
// *arbitrary* committed history, so this oracle churns a random history, forks
// at a random cut, and re-asserts the fork law every seed:
//
//   * bit-identity — the fork's `canonical_dump` equals the source's at the cut
//     (clock and watermark too), whether or not the source was consolidated;
//   * isolation    — committing to the fork never disturbs the source's copy;
//   * reconvergence — replaying the *same* remaining commits onto both stores
//     re-converges them bit-for-bit after every step.
//
// A seedable xorshift64* PRNG (mirroring grmpl-store/tests/determinism.rs) keeps
// each round reproducible and every assertion prints its `seed`.

/// Deterministic, seedable PRNG (xorshift64*) — reproducible churn with no
/// external `rand` dependency.
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

/// A random batch of asserts/retracts over `R` — small key/diff spaces so
/// retractions cancel real state.
fn random_batch(rng: &mut Rng) -> Vec<(RelId, Tuple, Diff)> {
    let len = 1 + rng.below(5); // 1..=5 updates
    (0..len)
        .map(|_| {
            let t = ent(rng.below(8));
            let d: Diff = if rng.below(2) == 0 { 1 } else { -1 };
            (R, t, d)
        })
        .collect()
}

#[test]
fn fork_bit_identity_and_reconvergence_under_random_churn() {
    for seed in 1..=24u64 {
        fork_churn_round(seed);
    }
}

fn fork_churn_round(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let mut rng = Rng::new(seed);

    store.register("things", R).unwrap();
    store.put_schema(R, &thing_schema(), Edition(1)).unwrap();

    // A random committed history before the fork cut.
    let pre = 1 + rng.below(12); // 1..=12 commits
    for _ in 0..pre {
        let batch = random_batch(&mut rng);
        store.commit(&batch).unwrap();
    }
    // Sometimes consolidate first, so the fork copies a checkpoint (O(state))
    // rather than raw history — the copy must be bit-identical either way.
    if rng.below(2) == 0 {
        store.consolidate(store.current()).unwrap();
    }

    let fork_dir = tempfile::tempdir().unwrap();
    let fork = store.fork(fork_dir.path()).unwrap();

    // Bit-identical at the cut, down to clock and watermark.
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "seed {seed}: fork is not bit-identical to its source at the cut"
    );
    assert_eq!(fork.current(), store.current(), "seed {seed}: fork clock differs");
    assert_eq!(
        TraceStore::watermark(&fork),
        TraceStore::watermark(&store),
        "seed {seed}: fork watermark differs"
    );

    // Drive both with the *same* remaining commits. Each round: write the fork
    // first and prove the source is untouched (isolation), then apply the
    // identical commit to the source and prove they reconverge bit-for-bit.
    let post = 1 + rng.below(12);
    for _ in 0..post {
        let batch = random_batch(&mut rng);
        let source_before = store.canonical_dump().unwrap();

        fork.commit(&batch).unwrap();
        assert_eq!(
            store.canonical_dump().unwrap(),
            source_before,
            "seed {seed}: writing the fork disturbed the source"
        );

        store.commit(&batch).unwrap();
        assert_eq!(
            store.canonical_dump().unwrap(),
            fork.canonical_dump().unwrap(),
            "seed {seed}: identical commits failed to reconverge source and fork"
        );
    }
}
