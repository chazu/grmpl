//! # grmpl-bench — the P13 benchmark harness
//!
//! Five axes, each a submodule of [`scenarios`], measured against a real
//! [`grmpl_store::FjallStore`]:
//!
//! * [`scenarios::churn`] — commit/patch throughput (the write floor).
//! * [`scenarios::watch`] — watch fan-out over independent v1 delta streams,
//!   plus the reactive `OnWatch::pump` path.
//! * [`scenarios::precond`] — optimistic `commit_if` precondition cost (the
//!   O(relation-history) `holds_at` scan).
//! * [`scenarios::contention`] — retry throughput and fairness when many threads
//!   race one precondition.
//! * [`scenarios::arrangement`] — the shared-arrangement memo: compute saved,
//!   memory pinned, and the transient-pointer (ABA) hazard the P13
//!   engine-statefulness work must design around.
//!
//! This is an **edge / tooling crate**, above the bright line: it names fjall
//! (via `grmpl-store`) exactly as an application would. It measures the semantic
//! core; it is not part of it. The [`harness`] module holds the dependency-free
//! timing scaffolding shared by every scenario.

pub mod harness;
pub mod rng;
pub mod scenarios;

pub use harness::{Report, Sample};

/// Run the whole suite at default sizes and return every report. The binary
/// prints these; a caller may inspect the [`Sample`] notes programmatically.
pub fn run_all() -> Vec<Report> {
    vec![
        scenarios::churn::report(2_000),
        scenarios::watch::report(200, 500),
        scenarios::precond::report(2_000),
        scenarios::contention::report(2_000),
        scenarios::arrangement::report(500, 200),
    ]
}
