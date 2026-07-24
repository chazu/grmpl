//! `grmpl-bench` — run the P13 benchmark suite and print a table per axis.
//!
//! ```text
//! cargo run -p grmpl-bench --release            # the whole suite
//! cargo run -p grmpl-bench --release -- churn   # one axis
//! ```
//!
//! Axes: `churn`, `watch`, `precond`, `contention`, `arrangement`. With no
//! argument (or `all`) every axis runs. Build `--release`: a debug build's
//! numbers reflect the compiler, not the engine.

use grmpl_bench::scenarios;
use grmpl_bench::Report;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    if !cfg!(debug_assertions) {
        // release: nothing to say
    } else {
        eprintln!(
            "warning: debug build — numbers reflect the compiler, not the engine. \
             Re-run with `--release` for real figures."
        );
    }

    let reports: Vec<Report> = match arg.as_str() {
        "churn" => vec![scenarios::churn::report(2_000)],
        "watch" => vec![scenarios::watch::report(200, 500)],
        "precond" => vec![scenarios::precond::report(2_000)],
        "contention" => vec![scenarios::contention::report(2_000)],
        "arrangement" => vec![scenarios::arrangement::report(500, 200)],
        "all" => grmpl_bench::run_all(),
        other => {
            eprintln!("unknown axis {other:?}; choose one of: churn watch precond contention arrangement all");
            std::process::exit(2);
        }
    };

    for r in &reports {
        r.print();
    }
    println!();
}
