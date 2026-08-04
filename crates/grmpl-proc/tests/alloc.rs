//! `Alloc` — the replay-safe entity-id counter (P3).

use grmpl_core::{Entity, NoSchemas, Patch, RelId, Scope, Tuple, Value};
use grmpl_core::{Authority, DomainId, Fact};
use grmpl_proc::{commit_patch, Alloc, CommitOutcome};

const SEQ: RelId = RelId(30);
const THING: RelId = RelId(1); // (id, tag) — something to name with allocated ids
fn auth() -> Authority {
    Authority::new(DomainId(1), vec![Scope::whole(SEQ), Scope::whole(THING)])
}

#[test]
fn allocates_distinct_ids_and_persists_the_counter() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();

        // First allocation from an absent counter starts at the base.
        let mut a = Alloc::read(store, SEQ, 1000).unwrap();
        let x = a.fresh();
        let y = a.fresh();
        assert_eq!((x, y), (Entity(1000), Entity(1001)));
        assert_eq!(a.allocated(), 2);

        let patch = a.seal(
            Patch::new()
                .assert(Fact::new(THING, Tuple::from([Value::Ent(x), Value::text("x")])))
                .assert(Fact::new(THING, Tuple::from([Value::Ent(y), Value::text("y")]))),
        );
        assert!(matches!(
            commit_patch(store, &NoSchemas, &patch, &auth()).unwrap(),
            CommitOutcome::Committed(_)
        ));

        // The counter now reads 1002, held with weight exactly 1 (no leftover rows).
        let at = store.current();
        let counter: Vec<_> =
            store.read_at(SEQ, at).unwrap().into_iter().filter(|(_, d)| *d != 0).collect();
        assert_eq!(counter, vec![(Tuple::from([Value::Int(1002)]), 1)]);

        // A fresh allocator picks up where the last left off — no id reuse.
        let mut b = Alloc::read(store, SEQ, 1000).unwrap();
        assert_eq!(b.fresh(), Entity(1002));
    });
}

#[test]
fn seal_is_a_noop_when_nothing_was_allocated() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let a = Alloc::read(store, SEQ, 1000).unwrap();
        let patch = a.seal(Patch::new());
        assert_eq!(patch, Patch::new(), "no allocation -> no counter write");
    });
}

#[test]
fn allocation_is_replay_safe_from_a_fixed_edition() {
    grmpl_conformance::for_each_store(|c| {
        // Re-running from the same committed edition reproduces the same ids: the id
        // is a pure function of the counter it reads, nothing external.
        let store = c.store();
        store.commit(&[(SEQ, Tuple::from([Value::Int(1000)]), 1)]).unwrap();
        let pinned = store.current();

        let ids_a: Vec<Entity> = {
            let mut a = Alloc::read(store, SEQ, 1000).unwrap();
            vec![a.fresh(), a.fresh(), a.fresh()]
        };
        // The store is unchanged (we never committed), so a replay reads the same
        // counter and hands out the identical ids.
        assert_eq!(store.current(), pinned);
        let ids_b: Vec<Entity> = {
            let mut b = Alloc::read(store, SEQ, 1000).unwrap();
            vec![b.fresh(), b.fresh(), b.fresh()]
        };
        assert_eq!(ids_a, ids_b);
        assert_eq!(ids_a, vec![Entity(1000), Entity(1001), Entity(1002)]);
    });
}

// ---------------------------------------------------------------------------
// Contention: the guard, and the fairness the guard alone does not give.
// ---------------------------------------------------------------------------

/// The law the P3 interim scheme could not state: **two patches allocating from
/// the same counter value never both commit.** `seal` preconditions the present
/// counter row, so racing allocations resolve to exactly one winner and the
/// loser retries against the winner's value.
///
/// Checked over the committed trace rather than over what the threads believed:
/// every id that reached the world is distinct, the ids are exactly the
/// contiguous block `base..base+n`, and the counter rests on a single row at
/// `base + n`. A duplicate id — the failure the unguarded `seal` permitted —
/// would show up as a short block *and* a `THING` row missing.
#[test]
fn racing_allocations_never_hand_out_the_same_id() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        // Seed on the un-raced path: the very first allocation has no counter row
        // to precondition on. This is the one sanctioned unguarded write.
        let seed = Alloc::read(store, SEQ, 1000).unwrap();
        assert!(!seed.is_seeded(), "counter starts absent");
        let patch = seed.seed(Patch::new());
        commit_patch(store, &NoSchemas, &patch, &auth()).unwrap();
        assert!(Alloc::read(store, SEQ, 1000).unwrap().is_seeded());

        const THREADS: u64 = 4;
        const PER_THREAD: u64 = 12;

        std::thread::scope(|scope| {
            for tid in 0..THREADS {
                scope.spawn(move || {
                    for k in 0..PER_THREAD {
                        // Every attempt re-reads the counter — the optimistic
                        // protocol's whole point.
                        grmpl_proc::commit_retrying(grmpl_proc::Backoff::default(), || {
                            let mut a = Alloc::read(store, SEQ, 1000).unwrap();
                            let id = a.fresh();
                            let patch = a.seal(Patch::new().assert(Fact::new(
                                THING,
                                Tuple::from([
                                    Value::Ent(id),
                                    Value::text(format!("t{tid}-{k}")),
                                ]),
                            )));
                            commit_patch(store, &NoSchemas, &patch, &auth())
                        })
                        .unwrap();
                    }
                });
            }
        });

        let total = (THREADS * PER_THREAD) as i64;
        let at = store.current();

        // Every id that landed is distinct, and together they are exactly the
        // contiguous block the counter should have handed out.
        let mut ids: Vec<u64> = store
            .read_at(THING, at)
            .unwrap()
            .into_iter()
            .map(|(t, d)| {
                assert_eq!(d, 1, "every allocated thing has weight 1");
                match t.as_slice().first() {
                    Some(Value::Ent(e)) => e.0,
                    other => panic!("malformed thing row: {other:?}"),
                }
            })
            .collect();
        ids.sort_unstable();
        let want: Vec<u64> = (1000..1000 + total as u64).collect();
        assert_eq!(ids, want, "ids must be distinct and contiguous — no id handed out twice");

        // The counter rests on exactly one row, at the end of the block.
        let counter: Vec<_> =
            store.read_at(SEQ, at).unwrap().into_iter().filter(|(_, d)| *d != 0).collect();
        assert_eq!(counter, vec![(Tuple::from([Value::Int(1000 + total)]), 1)]);
    });
}

/// Backoff is about **fairness**, which the guard alone does not provide: the
/// measured baseline was `fair(min/max) = 0.000` at 8 racing threads — at least
/// one thread committing nothing while the protocol reported no errors.
///
/// Under a retry policy every contender makes progress: each thread's full
/// quota of allocations lands. That is the property `fair(min/max) > 0` was
/// measuring, stated as a law rather than a benchmark number.
#[test]
fn every_contender_makes_progress_under_backoff() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        commit_patch(
            store,
            &NoSchemas,
            &Alloc::read(store, SEQ, 1000).unwrap().seed(Patch::new()),
            &auth(),
        )
        .unwrap();

        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 4;

        let landed: Vec<u64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|tid| {
                    scope.spawn(move || {
                        let mut n = 0;
                        for k in 0..PER_THREAD {
                            grmpl_proc::commit_retrying(grmpl_proc::Backoff::default(), || {
                                let mut a = Alloc::read(store, SEQ, 1000).unwrap();
                                let id = a.fresh();
                                let patch = a.seal(Patch::new().assert(Fact::new(
                                    THING,
                                    Tuple::from([
                                        Value::Ent(id),
                                        Value::text(format!("f{tid}-{k}")),
                                    ]),
                                )));
                                commit_patch(store, &NoSchemas, &patch, &auth())
                            })
                            .unwrap();
                            n += 1;
                        }
                        n
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(
            landed,
            vec![PER_THREAD; THREADS as usize],
            "no contender starves: every thread commits its whole quota"
        );
        assert_eq!(landed.iter().min(), landed.iter().max(), "fair(min/max) == 1");
    });
}
