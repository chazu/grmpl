//! **Session-window law oracle** (TKT-117; P9c design §5, where session windows
//! are one of the two deferred shapes).
//!
//! A session window is **gap-based**: it stays open while events keep arriving
//! and closes once the trace goes idle for longer than a timeout. Unlike
//! `tumbling`/`sliding`, whose boundaries are fixed in advance by arithmetic, a
//! session's boundaries are *discovered from the data* — which is exactly why it
//! can be got subtly wrong in two opposite directions, and why the law is stated
//! in both:
//!
//! * **Under-splitting** — a gap longer than the timeout left inside a session.
//! * **Over-splitting** — a boundary cut where the gap was within the timeout.
//!
//! Together they pin the partition uniquely: it is the *coarsest* split of the
//! active editions with no over-long internal gap, and any other split violates
//! one of the two. Everything below is that statement, plus the structural
//! consequences (partition, tightness, ordering) and the monotonicity in
//! `timeout` that a gap grammar must have.
//!
//! Written in the shape the repo convention requires (TKT-112): **seeded
//! xorshift randomized** streams, invariant re-checked every round, seed printed
//! on every assertion, no `rand`/`proptest` dependency, examples kept only as
//! witnesses.

use std::collections::HashSet;

use grmpl_core::{Edition, EditionStore, RelId, TraceStore, Tuple, Value};
use grmpl_diff::{sessions, window, Window};
use grmpl_store::FjallStore;

const REL: RelId = RelId(1);
const OTHER: RelId = RelId(2);
const SEEDS: std::ops::Range<u64> = 1..33; // 32 seeds
const ROUNDS: usize = 12;

// ---------------------------------------------------------------------------
// PRNG — SplitMix64 finalisation with the nonzero guard applied *after* mixing,
// so no seed bit is spent on the guard (cf. TKT-141, which sweeps the older
// `Rng(seed ^ K | 1)` spelling out of the existing oracles; that form collapses
// seeds 2n and 2n+1 to one stream and halves declared coverage).
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z })
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

/// A random strictly-ascending run of active editions with a deliberate mix of
/// gaps: mostly small (within any plausible timeout, so sessions grow) and
/// sometimes large (so sessions actually close). Both regimes must occur or the
/// laws below are checked on a degenerate shape, which the anti-vacuity counters
/// in each test enforce.
fn random_actives(rng: &mut Rng, n: usize, max_gap: u64) -> Vec<Edition> {
    let mut out = Vec::with_capacity(n);
    let mut e = 1 + rng.below(3);
    for _ in 0..n {
        out.push(Edition(e));
        // Three quarters small gaps, one quarter potentially large.
        let gap = if rng.below(4) == 0 { 1 + rng.below(max_gap) } else { 1 + rng.below(3) };
        e += gap;
    }
    out
}

/// The two structural facts that hold of *any* well-formed session split,
/// asserted for every partition this file produces.
fn assert_partitions(actives: &[Edition], out: &[Window], what: &str) {
    // Every active edition is in exactly one session window.
    for e in actives {
        let hits: Vec<&Window> = out.iter().filter(|w| w.contains(*e)).collect();
        assert_eq!(
            hits.len(),
            1,
            "{what}: active edition {} lies in {} session windows, not exactly one — sessions \
             must partition the activity ({out:?})",
            e.0,
            hits.len()
        );
    }
    // Windows are ascending and pairwise disjoint.
    for pair in out.windows(2) {
        assert!(
            pair[0].to() <= pair[1].from(),
            "{what}: session windows overlap or are out of order: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    // Each window is *tight*: it starts one below its first active edition and
    // ends exactly at its last, so it contains no edition it did not have to.
    for w in out {
        let mine: Vec<Edition> = actives.iter().copied().filter(|e| w.contains(*e)).collect();
        assert!(!mine.is_empty(), "{what}: session window {w:?} contains no activity at all");
        assert_eq!(
            w.from(),
            Edition(mine[0].0 - 1),
            "{what}: session window {w:?} is not anchored one below its first active edition {}",
            mine[0].0
        );
        assert_eq!(
            w.to(),
            *mine.last().unwrap(),
            "{what}: session window {w:?} extends past its last active edition"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 1 — no under-splitting, no over-splitting
// ---------------------------------------------------------------------------

/// The central law, both directions. Within a session every consecutive gap is
/// `<= timeout` (nothing was left un-split that should have closed); at every
/// boundary between consecutive sessions the gap is `> timeout` (nothing was
/// split that should have stayed open). Anti-vacuity counters assert that both
/// regimes actually occurred — a stream that never went idle, or never didn't,
/// would satisfy one half trivially.
#[test]
fn sessions_split_exactly_at_gaps_over_the_timeout() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        let mut multi_event_sessions = 0usize; // proves "no over-split" is live
        let mut boundaries = 0usize; // proves "no under-split" is live

        for round in 0..ROUNDS {
            let n = 2 + rng.below(24) as usize;
            let max_gap = 4 + rng.below(12);
            let actives = random_actives(&mut rng, n, max_gap);
            let timeout = 1 + rng.below(8);
            let what = format!("seed {seed} round {round} timeout {timeout}");

            let out = sessions(&actives, timeout).unwrap();
            assert!(!out.is_empty(), "{what}: activity but no sessions");
            assert_partitions(&actives, &out, &what);

            // Per session: the members, in order, with no over-long internal gap.
            let mut covered = 0usize;
            for w in &out {
                let mine: Vec<Edition> =
                    actives.iter().copied().filter(|e| w.contains(*e)).collect();
                covered += mine.len();
                if mine.len() > 1 {
                    multi_event_sessions += 1;
                }
                for pair in mine.windows(2) {
                    assert!(
                        pair[1].0 - pair[0].0 <= timeout,
                        "{what}: session {w:?} holds a gap of {} between editions {} and {}, \
                         which exceeds the timeout — the session should have closed there \
                         (under-splitting)",
                        pair[1].0 - pair[0].0,
                        pair[0].0,
                        pair[1].0
                    );
                }
            }
            assert_eq!(
                covered,
                actives.len(),
                "{what}: the sessions cover {covered} of {} active editions",
                actives.len()
            );

            // Per boundary: the gap really did exceed the timeout.
            for pair in out.windows(2) {
                let last = pair[0].to();
                let first = Edition(pair[1].from().0 + 1);
                boundaries += 1;
                assert!(
                    first.0 - last.0 > timeout,
                    "{what}: a session boundary was cut between editions {} and {}, but the gap \
                     of {} is within the timeout — those events belong to one session \
                     (over-splitting)",
                    last.0,
                    first.0,
                    first.0 - last.0
                );
            }
        }

        assert!(
            multi_event_sessions > 0,
            "seed {seed}: every session held a single event, so the no-over-split half of the \
             law was never exercised"
        );
        assert!(
            boundaries > 0,
            "seed {seed}: the stream never went idle past the timeout, so the no-under-split \
             half of the law was never exercised"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 2 — the split is the coarsest one, and monotone in the timeout
// ---------------------------------------------------------------------------

/// Two consequences of the law above, checked independently of it so a shared
/// mistake cannot hide in both.
///
/// * **Coarsest.** Merging any two adjacent sessions would introduce a gap over
///   the timeout, so no coarser split satisfies law 1 — the partition is unique.
/// * **Monotone.** Raising the timeout can only merge sessions, never split
///   them: the partition at a larger timeout is *coarser*, and every session at
///   the smaller timeout sits inside exactly one session at the larger. A
///   timeout at or above the widest gap yields a single session; a timeout of 1
///   over strictly-consecutive editions likewise.
#[test]
fn sessions_are_the_coarsest_split_and_monotone_in_the_timeout() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0xC0A5_5E1D);
        let mut merges = 0usize;

        for round in 0..ROUNDS {
            let n = 2 + rng.below(20) as usize;
            let max_gap = 4 + rng.below(12);
            let actives = random_actives(&mut rng, n, max_gap);
            let small = 1 + rng.below(6);
            let large = small + 1 + rng.below(6);
            let what = format!("seed {seed} round {round}");

            let fine = sessions(&actives, small).unwrap();
            let coarse = sessions(&actives, large).unwrap();
            assert_partitions(&actives, &fine, &format!("{what} fine"));
            assert_partitions(&actives, &coarse, &format!("{what} coarse"));

            assert!(
                coarse.len() <= fine.len(),
                "{what}: raising the timeout from {small} to {large} produced *more* sessions \
                 ({} vs {}) — a longer idle tolerance can only merge",
                coarse.len(),
                fine.len()
            );
            if coarse.len() < fine.len() {
                merges += 1;
            }

            // Refinement: every fine session's activity sits inside exactly one
            // coarse session.
            for f in &fine {
                let mine: Vec<Edition> =
                    actives.iter().copied().filter(|e| f.contains(*e)).collect();
                let hosts: HashSet<usize> = mine
                    .iter()
                    .map(|e| {
                        coarse
                            .iter()
                            .position(|c| c.contains(*e))
                            .unwrap_or_else(|| panic!("{what}: {} escaped the coarse split", e.0))
                    })
                    .collect();
                assert_eq!(
                    hosts.len(),
                    1,
                    "{what}: a session at timeout {small} was *split* across {} sessions at \
                     timeout {large} — the coarser partition must refine, never subdivide",
                    hosts.len()
                );
            }

            // Coarsest: no two adjacent sessions could have been merged.
            for pair in fine.windows(2) {
                assert!(
                    Edition(pair[1].from().0 + 1).0 - pair[0].to().0 > small,
                    "{what}: two adjacent sessions could be merged without introducing an \
                     over-long gap, so the split is not the coarsest one"
                );
            }

            // A timeout at or above the widest gap collapses to one session.
            let widest = actives.windows(2).map(|p| p[1].0 - p[0].0).max().unwrap_or(0);
            let one = sessions(&actives, widest.max(1)).unwrap();
            assert_eq!(
                one.len(),
                1,
                "{what}: a timeout of {} (the widest gap) must leave exactly one session, got {}",
                widest.max(1),
                one.len()
            );
            assert_eq!(one[0].from(), Edition(actives[0].0 - 1));
            assert_eq!(one[0].to(), *actives.last().unwrap());
        }

        assert!(
            merges > 0,
            "seed {seed}: raising the timeout never merged anything, so monotonicity was checked \
             on a fixed partition only"
        );
    }
}

// ---------------------------------------------------------------------------
// Law 3 — sessions over a real trace
// ---------------------------------------------------------------------------

/// `Window::sessions` cuts sessions from *observed activity*, so it must agree
/// with the pure `sessions` grammar over exactly the editions the relation
/// actually committed in — deduplicated (an edition with several updates is one
/// active edition), scoped to the enclosing window, and **per base relation**
/// (idleness is a property of one relation's own history; a peer relation
/// committing in the gap must not keep the session open).
#[test]
fn window_sessions_agree_with_the_grammar_over_observed_activity() {
    for seed in SEEDS.take(16) {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        let mut rng = Rng::new(seed ^ 0x7ACE_0FF5);

        // Build a trace whose *active* editions for REL are separated by random
        // idle stretches, filled with commits to a peer relation so the two
        // relations' session structures genuinely differ.
        let mut active: Vec<Edition> = Vec::new();
        for _ in 0..(6 + rng.below(10)) {
            let n = 1 + rng.below(3);
            let batch: Vec<(RelId, Tuple, i64)> = (0..n)
                .map(|i| (REL, Tuple::from([Value::Int(rng.below(9) as i64 + i as i64)]), 1))
                .collect();
            active.push(store.commit(&batch).unwrap());
            for _ in 0..rng.below(6) {
                store.commit(&[(OTHER, Tuple::from([Value::Int(0)]), 1)]).unwrap();
            }
        }
        let w = window(Edition::ZERO, store.current()).unwrap();

        let mut split_differed = false;
        for timeout in 1..7u64 {
            let what = format!("seed {seed} timeout {timeout}");
            let got = w.sessions(&store, REL, timeout).unwrap();
            let want = sessions(&active, timeout).unwrap();
            assert_eq!(
                got, want,
                "{what}: sessions over the trace disagree with the grammar over the relation's \
                 own active editions"
            );
            assert_partitions(&active, &got, &what);

            // Each session window must materialize exactly its own events.
            let mut seen = 0usize;
            for s in &got {
                let ups = s.events(&store, REL).unwrap();
                assert!(!ups.is_empty(), "{what}: session {s:?} materializes no events");
                seen += ups.len();
                for u in &ups {
                    assert!(
                        s.contains(Edition(u.time.edition)),
                        "{what}: session {s:?} materialized an event from edition {}",
                        u.time.edition
                    );
                }
            }
            assert_eq!(
                seen,
                w.events(&store, REL).unwrap().len(),
                "{what}: the sessions together lost or duplicated events"
            );

            // Per relation: the peer's activity has its own session structure.
            let peer = w.sessions(&store, OTHER, timeout).unwrap();
            if peer != got {
                split_differed = true;
            }
        }
        assert!(
            split_differed,
            "seed {seed}: the two relations' session splits never differed, so the per-relation \
             reading went untested"
        );
    }
}

// ---------------------------------------------------------------------------
// Witnesses and ill-formed input
// ---------------------------------------------------------------------------

/// A readable statement of the whole shape: three bursts separated by long idle
/// stretches, at a timeout that closes on the stretches and not inside a burst.
#[test]
fn witness_three_bursts_become_three_sessions() {
    let actives: Vec<Edition> =
        [3, 4, 5, /* gap 6 */ 11, 12, /* gap 9 */ 21, 22, 23].iter().map(|e| Edition(*e)).collect();

    let out = sessions(&actives, 3).unwrap();
    assert_eq!(
        out,
        vec![
            window(Edition(2), Edition(5)).unwrap(),
            window(Edition(10), Edition(12)).unwrap(),
            window(Edition(20), Edition(23)).unwrap(),
        ],
        "each burst is one tight (first-1, last] window"
    );

    // Raise the timeout past the first gap and the first two bursts merge.
    assert_eq!(
        sessions(&actives, 6).unwrap(),
        vec![
            window(Edition(2), Edition(12)).unwrap(),
            window(Edition(20), Edition(23)).unwrap(),
        ],
        "a timeout of 6 tolerates the gap of 6 but not the gap of 9"
    );

    // Past the widest gap, everything is one session.
    assert_eq!(sessions(&actives, 9).unwrap(), vec![window(Edition(2), Edition(23)).unwrap()]);

    // A timeout of 1 keeps only strictly-consecutive editions together.
    assert_eq!(
        sessions(&actives, 1).unwrap(),
        vec![
            window(Edition(2), Edition(5)).unwrap(),
            window(Edition(10), Edition(12)).unwrap(),
            window(Edition(20), Edition(23)).unwrap(),
        ],
        "these bursts are internally consecutive, so timeout 1 already separates them"
    );
}

/// No activity is no sessions — not one empty session.
#[test]
fn witness_no_activity_yields_no_sessions() {
    assert_eq!(sessions(&[], 3).unwrap(), vec![]);
    assert_eq!(sessions(&[Edition(7)], 3).unwrap(), vec![window(Edition(6), Edition(7)).unwrap()]);
}

/// Ill-formed input is an error, not a silently degenerate answer — the reading
/// `sliding` already takes of a gap-producing step.
#[test]
fn witness_ill_formed_session_input_is_rejected() {
    let ok = [Edition(2), Edition(4)];
    assert!(
        matches!(sessions(&ok, 0), Err(grmpl_core::Error::Query(_))),
        "a zero timeout closes at every event: that is tumbling(1), not a gap grammar"
    );
    assert!(
        matches!(sessions(&[Edition(4), Edition(2)], 3), Err(grmpl_core::Error::Query(_))),
        "active editions must be strictly ascending"
    );
    assert!(
        matches!(sessions(&[Edition(2), Edition(2)], 3), Err(grmpl_core::Error::Query(_))),
        "duplicates are not strictly ascending — dedup before calling"
    );
    assert!(
        matches!(sessions(&[Edition::ZERO], 3), Err(grmpl_core::Error::Query(_))),
        "edition 0 is the empty world and carries no update"
    );
    assert!(sessions(&ok, 1).is_ok());
}
