//! The measurement scaffolding shared by every scenario.
//!
//! Deliberately dependency-free: a `Sample` is one timed run of `ops` logical
//! operations plus a bag of scalar `notes` (retries, memo entries, fairness
//! spread, …). Scenarios build `Report`s of `Sample`s; the binary prints them
//! as a table and the smoke tests assert the notes are sane. There is no
//! statistical machinery (criterion-style bootstrapping) on purpose — these
//! benchmarks measure *engine* costs whose signal (O(relation) precondition
//! scans, linear watch fan-out) is orders of magnitude, not percent, so a
//! single warmed wall-clock run per size is the honest, reproducible unit.

use std::time::{Duration, Instant};

/// One measured line: `ops` logical operations took `elapsed`, with extra
/// scalar facts in `notes`.
pub struct Sample {
    pub label: String,
    /// The count of logical operations this run performed (commits, watcher
    /// polls, precondition checks, …) — the denominator for throughput.
    pub ops: u64,
    pub elapsed: Duration,
    /// Named scalar observations beyond raw timing.
    pub notes: Vec<(String, f64)>,
}

impl Sample {
    pub fn new(label: impl Into<String>, ops: u64, elapsed: Duration) -> Sample {
        Sample { label: label.into(), ops, elapsed, notes: Vec::new() }
    }

    /// Attach a scalar note (chainable).
    pub fn note(mut self, key: impl Into<String>, value: f64) -> Sample {
        self.notes.push((key.into(), value));
        self
    }

    /// Look a note up by key (used by the smoke tests to assert invariants).
    pub fn get(&self, key: &str) -> Option<f64> {
        self.notes.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    pub fn ops_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            f64::INFINITY
        } else {
            self.ops as f64 / secs
        }
    }

    pub fn ns_per_op(&self) -> f64 {
        if self.ops == 0 {
            f64::NAN
        } else {
            self.elapsed.as_nanos() as f64 / self.ops as f64
        }
    }
}

/// Time `body`, which is expected to perform `ops` logical operations, and
/// label the result. The caller is responsible for any warmup (`repeat` /
/// scenario-local priming) — `measure` times exactly one pass so the reported
/// `ops` and `elapsed` correspond to the same work.
pub fn measure(label: impl Into<String>, ops: u64, body: impl FnOnce()) -> Sample {
    let t = Instant::now();
    body();
    Sample::new(label, ops, t.elapsed())
}

/// Run `body` `n` times purely to warm caches / the allocator; the result is
/// discarded. Used before a `measure` when the first run would pay one-time
/// costs (keyspace handle creation, page-cache fill) that would swamp the signal.
pub fn warmup(n: u32, mut body: impl FnMut()) {
    for _ in 0..n {
        body();
    }
}

/// A titled group of samples — one axis of the benchmark suite.
pub struct Report {
    pub title: String,
    pub samples: Vec<Sample>,
}

impl Report {
    pub fn new(title: impl Into<String>) -> Report {
        Report { title: title.into(), samples: Vec::new() }
    }

    pub fn push(&mut self, s: Sample) {
        self.samples.push(s);
    }

    /// Render as an aligned table on stdout.
    pub fn print(&self) {
        println!("\n== {} ==", self.title);
        // Column widths.
        let lw = self.samples.iter().map(|s| s.label.len()).max().unwrap_or(0).max(5);
        println!(
            "{:<lw$}  {:>10}  {:>12}  {:>14}  notes",
            "scenario",
            "ops",
            "ms",
            "ops/s",
            lw = lw
        );
        for s in &self.samples {
            let notes: Vec<String> =
                s.notes.iter().map(|(k, v)| format!("{k}={}", fmt_num(*v))).collect();
            println!(
                "{:<lw$}  {:>10}  {:>12.3}  {:>14}  {}",
                s.label,
                s.ops,
                s.elapsed.as_secs_f64() * 1e3,
                fmt_num(s.ops_per_sec()),
                notes.join(" "),
                lw = lw
            );
        }
    }
}

/// Compact human number: thousands separators for large integers, else 3 sig
/// figs. Keeps the table readable without pulling in a formatting crate.
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "inf".to_string();
    }
    if v >= 1000.0 {
        let n = v.round() as i64;
        let s = n.abs().to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        if n < 0 {
            format!("-{out}")
        } else {
            out
        }
    } else {
        format!("{v:.3}")
    }
}
