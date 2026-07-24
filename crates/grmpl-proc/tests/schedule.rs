//! P4 acceptance (DESIGN.md §2.2, §5): scheduling & simulation time.
//!
//! The laws under test:
//!
//! * **Same-commit durability.** A `Patch.scheduled` entry lands as a durable
//!   timer row in the *same atomic commit* as the patch's effects.
//! * **Exactly-once fire.** Each due timer is delivered exactly once — the fire
//!   is a `commit_if` preconditioned on the timer row, so a re-fire (or a
//!   racing driver, or a post-crash retry) cannot duplicate it. The witness is
//!   the M5 idiom: an inbox row with weight exactly 1.
//! * **Race-safe seq allocation.** The durable per-key `SeqAlloc` hands out
//!   contiguous, unique inbox seqs even when allocations race (guarded by a
//!   precondition on the counter row).
//! * **Replay determinism.** Firing reads simulation time from *committed clock
//!   samples* (data), never a live clock, so replaying the same samples
//!   reproduces the identical inbox — the Replay law.

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, NoSchemas, Patch, RelId, Scheduled, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_proc::{commit_patch, ClockDriver, CommitOutcome, Scheduler, SeqAlloc};
use grmpl_store::FjallStore;

const TIMERS: RelId = RelId(20);
const SEQS: RelId = RelId(21);
const INBOX: RelId = RelId(22);
const CLOCK: RelId = RelId(24);

const PROC: Entity = Entity(7);

fn driver_authority() -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(TIMERS),
            Scope::whole(SEQS),
            Scope::whole(INBOX),
            Scope::whole(CLOCK),
        ],
    )
}

fn scheduler() -> Scheduler {
    Scheduler::new(TIMERS, SEQS, driver_authority())
}

fn clock() -> ClockDriver {
    ClockDriver::new(CLOCK, driver_authority())
}

/// Seq-counter key for `(INBOX, PROC)`, seeded so allocation is fully race-safe.
fn seq_key() -> Vec<Value> {
    vec![Value::Int(INBOX.0 as i64), Value::Ent(PROC)]
}

fn seed_seqs(store: &FjallStore) {
    let alloc = SeqAlloc::read(store, SEQS, seq_key()).unwrap();
    let patch = alloc.seed(Patch::new());
    let out = commit_patch(store, &NoSchemas, &patch, &driver_authority()).unwrap();
    assert!(matches!(out, CommitOutcome::Committed(_)));
}

/// Commit a patch that schedules a timer (body `[Int(tag)]`) due at `due`.
fn schedule_timer(store: &FjallStore, due: i64, tag: i64) {
    let patch = Patch::new().schedule(Scheduled {
        timers: TIMERS,
        due,
        inbox: INBOX,
        target: PROC,
        body: Tuple::from([Value::Int(tag)]),
    });
    let out = commit_patch(store, &NoSchemas, &patch, &driver_authority()).unwrap();
    assert!(matches!(out, CommitOutcome::Committed(_)), "schedule commit");
}

/// The inbox rows for `PROC`, sorted by seq: `(seq, tag)`.
fn inbox_rows(store: &FjallStore) -> Vec<(i64, i64)> {
    let at = store.current();
    let mut v: Vec<(i64, i64)> = store
        .read_at(INBOX, at)
        .unwrap()
        .into_iter()
        .filter(|(_, d)| *d > 0)
        .filter_map(|(t, _)| match t.as_slice() {
            [Value::Ent(p), Value::Int(seq), Value::Tuple(body)] if *p == PROC => {
                match body.first() {
                    Some(Value::Int(tag)) => Some((*seq, *tag)),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn timer_weight(store: &FjallStore, due: i64, tag: i64) -> i64 {
    let at = store.current();
    let row = Tuple::from([
        Value::Int(due),
        Value::Int(INBOX.0 as i64),
        Value::Ent(PROC),
        Value::Tuple([Value::Int(tag)].into()),
    ]);
    store
        .read_at(TIMERS, at)
        .unwrap()
        .into_iter()
        .filter(|(t, _)| *t == row)
        .map(|(_, d)| d)
        .sum()
}

/// End-to-end: schedule → the timer is durable → fire delivers it exactly once
/// at the seeded seq, retracting the timer. Re-firing is a no-op (idempotent).
#[test]
fn schedule_then_fire_delivers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_seqs(&store);

    schedule_timer(&store, 5, 100);
    // Durable, but not yet delivered.
    assert_eq!(timer_weight(&store, 5, 100), 1);
    assert_eq!(inbox_rows(&store), vec![]);

    // Not due yet at now=4.
    assert_eq!(scheduler().fire_due(&store, &NoSchemas, 4).unwrap(), 0);
    assert_eq!(inbox_rows(&store), vec![]);

    // Due at now=5: delivered once.
    assert_eq!(scheduler().fire_due(&store, &NoSchemas, 5).unwrap(), 1);
    assert_eq!(inbox_rows(&store), vec![(0, 100)]);
    // Timer consumed (weight exactly 0), inbox row weight exactly 1 (M5 witness).
    assert_eq!(timer_weight(&store, 5, 100), 0);

    // Firing again delivers nothing and adds no duplicate.
    assert_eq!(scheduler().fire_due(&store, &NoSchemas, 100).unwrap(), 0);
    assert_eq!(inbox_rows(&store), vec![(0, 100)]);
}

/// A racing second fire of the *same* timer (two patches preconditioned on the
/// one timer row) resolves to exactly one winner — the exactly-once guard.
#[test]
fn racing_fire_of_same_timer_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_seqs(&store);
    schedule_timer(&store, 0, 42);

    // Build two independent fire patches against the same timer row, both
    // valid at the current edition (simulating two drivers racing).
    let timer = Fact::new(
        TIMERS,
        Tuple::from([
            Value::Int(0),
            Value::Int(INBOX.0 as i64),
            Value::Ent(PROC),
            Value::Tuple([Value::Int(42)].into()),
        ]),
    );
    let inbox_row = |seq: i64| {
        Fact::new(
            INBOX,
            Tuple::from([Value::Ent(PROC), Value::Int(seq), Value::Tuple([Value::Int(42)].into())]),
        )
    };
    let a = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    let b = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    let patch_a = a.seal(Patch::new().expect(timer.clone()).retract(timer.clone()).assert(inbox_row(a.peek())));
    let patch_b = b.seal(Patch::new().expect(timer.clone()).retract(timer.clone()).assert(inbox_row(b.peek())));

    let out_a = commit_patch(&store, &NoSchemas, &patch_a, &driver_authority()).unwrap();
    let out_b = commit_patch(&store, &NoSchemas, &patch_b, &driver_authority()).unwrap();

    // Exactly one committed; the loser was rejected (its precondition — the now
    // retracted timer row — no longer held).
    let committed = [&out_a, &out_b]
        .iter()
        .filter(|o| matches!(o, CommitOutcome::Committed(_)))
        .count();
    assert_eq!(committed, 1, "exactly one fire wins");
    assert_eq!(inbox_rows(&store), vec![(0, 42)]);
    assert_eq!(timer_weight(&store, 0, 42), 0);
}

/// The guarded `SeqAlloc`: two allocations racing the same seeded key resolve to
/// one winner per value; the loser retries and gets the next seq. Contiguous,
/// unique seqs with no collision — the durable, concurrency-safe allocator.
#[test]
fn seq_alloc_is_race_safe() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_seqs(&store);

    // Two allocators read the same start=0.
    let mut a = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    let mut b = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    assert!(a.is_seeded() && b.is_seeded());
    let sa = a.fresh();
    let sb = b.fresh();
    assert_eq!((sa, sb), (0, 0), "both optimistically read seq 0");

    let mark = |seq: i64| Fact::new(INBOX, Tuple::from([Value::Ent(PROC), Value::Int(seq), Value::Tuple([Value::Int(seq)].into())]));

    let out_a = commit_patch(&store, &NoSchemas, &a.seal(Patch::new().assert(mark(sa))), &driver_authority()).unwrap();
    let out_b = commit_patch(&store, &NoSchemas, &b.seal(Patch::new().assert(mark(sb))), &driver_authority()).unwrap();
    assert!(matches!(out_a, CommitOutcome::Committed(_)));
    assert!(matches!(out_b, CommitOutcome::Rejected), "the stale seq-0 bump is rejected");

    // The loser retries against the winner's committed value and gets seq 1.
    let mut b2 = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    let sb2 = b2.fresh();
    assert_eq!(sb2, 1, "retry allocates the next seq");
    let out = commit_patch(&store, &NoSchemas, &b2.seal(Patch::new().assert(mark(sb2))), &driver_authority()).unwrap();
    assert!(matches!(out, CommitOutcome::Committed(_)));

    assert_eq!(inbox_rows(&store), vec![(0, 0), (1, 1)]);
}

/// The first allocation of an *unseeded* key still works (no counter row to
/// precondition on): it asserts the counter at 1 and hands out seq 0.
#[test]
fn seq_alloc_first_allocation_on_unseeded_key_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();

    let mut a = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    assert!(!a.is_seeded());
    let s = a.fresh();
    assert_eq!(s, 0);
    let mark = Fact::new(INBOX, Tuple::from([Value::Ent(PROC), Value::Int(s), Value::Tuple([Value::Int(0)].into())]));
    commit_patch(&store, &NoSchemas, &a.seal(Patch::new().assert(mark)), &driver_authority()).unwrap();

    // The counter now exists at 1, so the next allocation is race-safe.
    let b = SeqAlloc::read(&store, SEQS, seq_key()).unwrap();
    assert!(b.is_seeded());
    assert_eq!(b.peek(), 1);
}

/// The clock driver commits wall-clock/randomness samples as data; nothing else
/// reads a clock. `latest` returns the newest committed sample.
#[test]
fn clock_driver_records_samples_as_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    let c = clock();
    assert_eq!(c.latest(&store).unwrap(), None);

    assert_eq!(c.sample(&store, &NoSchemas, 1000, 7).unwrap(), (0, 1000, 7));
    assert_eq!(c.sample(&store, &NoSchemas, 1001, 9).unwrap(), (1, 1001, 9));
    assert_eq!(c.latest(&store).unwrap(), Some((1, 1001, 9)));
}

// ---------------------------------------------------------------------------
// Randomized-churn law oracle (TKT-91 idiom): schedule K timers at random due
// times, advance a committed clock monotonically, fire after every sample, and
// check the exact delivery against an independent model each round.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64* — deterministic, no new dependency.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: i64) -> i64 {
        (self.next() % n as u64) as i64
    }
}

/// `(seq, tag)` inbox rows.
type Rows = Vec<(i64, i64)>;

/// Run one seeded scenario in a fresh store; return `(delivered, model)` inbox
/// rows. Deterministic in `(seed, k)`: the only nondeterminism (the clock) is
/// committed as data, so this doubles as the replay driver.
fn run_scenario(seed: u64, k: i64) -> (Rows, Rows) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    seed_seqs(&store);
    let c = clock();
    let sch = scheduler();

    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1));
    const HORIZON: i64 = 28;

    // Schedule K timers up front with distinct tags and random due times.
    let mut scheduled: Vec<(i64, i64)> = Vec::new(); // (due, tag)
    for tag in 0..k {
        let due = rng.below(HORIZON + 10); // some may be beyond the final now
        schedule_timer(&store, due, tag);
        scheduled.push((due, tag));
    }

    // Advance a committed wall clock monotonically in random steps, firing after
    // each sample. `now` is read back from the committed sample, never live.
    let mut now = 0;
    while now <= HORIZON {
        c.sample(&store, &NoSchemas, now, rng.below(1_000_000)).unwrap();
        let latest = c.latest(&store).unwrap().unwrap();
        sch.fire_due(&store, &NoSchemas, latest.1).unwrap();
        now += 1 + rng.below(5);
    }
    let final_now = c.latest(&store).unwrap().unwrap().1;

    // Independent model: every timer with due <= final_now is delivered exactly
    // once, in (due, tag) order, at contiguous seqs from the seed (0).
    let mut model_src: Vec<(i64, i64)> =
        scheduled.iter().copied().filter(|(due, _)| *due <= final_now).collect();
    model_src.sort();
    let model: Vec<(i64, i64)> =
        model_src.iter().enumerate().map(|(i, (_, tag))| (i as i64, *tag)).collect();

    (inbox_rows(&store), model)
}

#[test]
fn scheduler_delivery_matches_model_under_random_churn() {
    for seed in 0..24u64 {
        let k = 3 + (seed as i64 % 10); // 3..=12 timers
        let (delivered, model) = run_scenario(seed, k);
        assert_eq!(
            delivered, model,
            "seed={seed} k={k}: delivered inbox must match the independent model \
             (exactly-once, due-ordered, contiguous seqs)"
        );

        // Replay law: re-running the identical scenario reproduces the identical
        // inbox — the only nondeterminism (the clock) is committed data.
        let (replay, _) = run_scenario(seed, k);
        assert_eq!(delivered, replay, "seed={seed}: replay is deterministic");
    }
}
