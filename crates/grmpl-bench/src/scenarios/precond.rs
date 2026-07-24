//! **Precondition cost.** What does an optimistic `commit_if` pay to re-check a
//! precondition atomically?
//!
//! The store checks each precondition with `holds_at`, which **sums the tuple's
//! weight across the relation's whole checkpoint + tail** — there is no point
//! index. Below the consolidation watermark that tail is the relation's entire
//! *history*, so a precondition costs O(updates-ever-written-to-that-relation)
//! per precondition, *not* O(live-rows). This scenario pins that number: it
//! freezes a relation `P` at a range of history sizes and times `commit_if`
//! calls whose preconditions read `P` while their writes land in a *different*
//! relation `Q` (so `P`'s history stays fixed during the timed loop).
//!
//! The baseline is the same loop with **no** preconditions — so the delta
//! between the two rows is the pure precondition-scan cost. This is the headline
//! motivation for the P13 arranged/indexed precondition check.

use grmpl_core::{RelId, TraceStore, Tuple, Value};

use crate::harness::{measure, Report, Sample};
use crate::scenarios::open_store;

const P: RelId = RelId(20); // the precondition relation (history frozen while timing)
const Q: RelId = RelId(21); // the write relation (kept out of the precondition set)

fn p_row(k: i64) -> Tuple {
    Tuple::from([Value::Int(k)])
}

/// Time `probes` `commit_if`s with `k_preconds` preconditions over a `P` grown
/// to `hist` distinct rows (so `P`'s tail holds `hist` updates).
pub fn commit_if_cost(hist: u64, k_preconds: u64, probes: u64) -> Sample {
    let (_d, store) = open_store();
    // Grow P to `hist` history (one update per commit → `hist`-long tail).
    for k in 0..hist as i64 {
        store.commit(&[(P, p_row(k), 1)]).unwrap();
    }
    // Preconditions: the first `k_preconds` rows, all present (they hold).
    let preconds: Vec<(RelId, Tuple)> =
        (0..k_preconds as i64).map(|k| (P, p_row(k))).collect();

    let mut q: i64 = 0;
    let s = measure(format!("commit_if  hist={hist} k={k_preconds}"), probes, || {
        for _ in 0..probes {
            q += 1;
            let ok = store
                .commit_if(&preconds, &[(Q, Tuple::from([Value::Int(q)]), 1)])
                .unwrap();
            assert!(ok.is_some(), "held precondition rejected");
        }
    });
    let ns_probe = s.ns_per_op();
    let ns_per_precond_row = if hist * k_preconds > 0 {
        s.elapsed.as_nanos() as f64 / (probes as f64 * (hist * k_preconds) as f64)
    } else {
        f64::NAN
    };
    s.note("hist", hist as f64)
        .note("k", k_preconds as f64)
        .note("ns/probe", ns_probe)
        .note("ns/precond-row", ns_per_precond_row)
}

/// Baseline: the same write loop with **no** preconditions — the plain
/// `commit` floor the precondition check sits on top of.
pub fn baseline(hist: u64, probes: u64) -> Sample {
    let (_d, store) = open_store();
    for k in 0..hist as i64 {
        store.commit(&[(P, p_row(k), 1)]).unwrap();
    }
    let mut q: i64 = 0;
    let s = measure(format!("commit     hist={hist} k=0"), probes, || {
        for _ in 0..probes {
            q += 1;
            store.commit(&[(Q, Tuple::from([Value::Int(q)]), 1)]).unwrap();
        }
    });
    let ns_probe = s.ns_per_op();
    s.note("hist", hist as f64).note("k", 0.0).note("ns/probe", ns_probe)
}

/// Precondition cost as relation history grows, with the no-precondition floor.
pub fn report(probes: u64) -> Report {
    let mut rep = Report::new("Precondition cost (holds_at scans the whole relation tail)");
    for &hist in &[100u64, 1_000, 10_000] {
        rep.push(baseline(hist, probes));
        rep.push(commit_if_cost(hist, 1, probes));
        rep.push(commit_if_cost(hist, 4, probes));
    }
    rep
}
