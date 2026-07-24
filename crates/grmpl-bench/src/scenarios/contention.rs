//! **Contention & retry fairness.** How does the optimistic commit protocol
//! behave when many threads race the *same* precondition?
//!
//! `T` threads run a compare-and-commit loop on one shared single-row counter:
//! read `v`, `commit_if` preconditioned on `(v)` still holding, retract `(v)` /
//! assert `(v+1)`. Exactly one racer wins each round; the losers get `Rejected`
//! and retry against the new value (`commit_if`'s exactly-one-winner guarantee,
//! DESIGN.md §5.2). We drive the counter to a fixed target and measure:
//!
//! * **throughput** — successful increments per second as `T` grows (does
//!   contention collapse it?),
//! * **retry rate** — `rejections / (successes + rejections)` (the wasted work),
//! * **fairness** — `min_wins / max_wins` across threads (1.0 = every thread won
//!   an equal share; a low ratio means the protocol starves someone).

use std::thread;

use grmpl_core::{RelId, TraceStore, Tuple, Value};

use crate::harness::{Report, Sample};
use crate::scenarios::{open_store, per_sec};

const C: RelId = RelId(40); // the contended single-row counter

fn counter_row(v: i64) -> Tuple {
    Tuple::from([Value::Int(v)])
}

/// Read the counter's current value (0 if unset).
fn read_counter(store: &dyn TraceStore) -> i64 {
    let at = store.current();
    for (t, d) in store.read_at(C, at).unwrap() {
        if d > 0 {
            if let Some(Value::Int(v)) = t.as_slice().first() {
                return *v;
            }
        }
    }
    0
}

/// `threads` racers drive the counter from 0 to `target` via `commit_if`.
pub fn race(threads: u64, target: i64) -> Sample {
    let (_d, store) = open_store();
    store.commit(&[(C, counter_row(0), 1)]).unwrap();

    let store_ref: &(dyn TraceStore + Sync) = &store;
    let start = std::time::Instant::now();
    // Each thread returns (wins, rejections).
    let per_thread: Vec<(u64, u64)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let (mut wins, mut rejects) = (0u64, 0u64);
                    loop {
                        let v = read_counter(store_ref);
                        if v >= target {
                            break;
                        }
                        let ok = store_ref
                            .commit_if(
                                &[(C, counter_row(v))],
                                &[(C, counter_row(v), -1), (C, counter_row(v + 1), 1)],
                            )
                            .unwrap();
                        if ok.is_some() {
                            wins += 1;
                        } else {
                            rejects += 1;
                        }
                    }
                    (wins, rejects)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let elapsed = start.elapsed();

    let total_wins: u64 = per_thread.iter().map(|(w, _)| *w).sum();
    let total_rejects: u64 = per_thread.iter().map(|(_, r)| *r).sum();
    let min_wins = per_thread.iter().map(|(w, _)| *w).min().unwrap_or(0);
    let max_wins = per_thread.iter().map(|(w, _)| *w).max().unwrap_or(0);
    let fairness = if max_wins > 0 { min_wins as f64 / max_wins as f64 } else { 1.0 };
    let retry_rate = if total_wins + total_rejects > 0 {
        total_rejects as f64 / (total_wins + total_rejects) as f64
    } else {
        0.0
    };

    // The counter must have landed exactly on target — one winner per round.
    assert_eq!(read_counter(&store), target, "lost or double-counted an increment");
    assert_eq!(total_wins as i64, target, "wins != increments");

    Sample::new(format!("race x{threads} threads"), total_wins, elapsed)
        .note("commits/s", per_sec(total_wins, elapsed.as_secs_f64()))
        .note("rejects", total_rejects as f64)
        .note("retry_rate", retry_rate)
        .note("fair(min/max)", fairness)
}

/// Contention across a range of thread counts.
pub fn report(target: i64) -> Report {
    let mut rep = Report::new("Contention & retry fairness (racing one counter's precondition)");
    for &t in &[1u64, 2, 4, 8] {
        rep.push(race(t, target));
    }
    rep
}
