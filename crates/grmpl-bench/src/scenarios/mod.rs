//! The five benchmark axes the P13 ticket names. Each submodule builds a
//! [`Report`](crate::harness::Report) at a range of sizes; the binary runs all
//! five and the smoke tests run each at a tiny size and assert its invariants.
//!
//! Every scenario runs against a real [`FjallStore`] in a fresh temp directory —
//! the honest substrate. The engine costs we are here to measure (the O(relation)
//! precondition scan, the linear watch fan-out, the transient arrangement memo)
//! sit *above* the store, so a real store keeps the numbers trustworthy without
//! an in-memory mock that would flatter them.

use grmpl_store::FjallStore;
use tempfile::TempDir;

pub mod arrangement;
pub mod churn;
pub mod contention;
pub mod precond;
pub mod watch;

/// A fresh, empty store in its own temp directory. The `TempDir` must be held
/// for as long as the store is used — dropping it deletes the backing files.
pub fn open_store() -> (TempDir, FjallStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FjallStore::open(dir.path()).expect("open store");
    (dir, store)
}

/// Facts-per-second implied by a run: `facts` total rows over `elapsed`.
pub fn per_sec(count: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        f64::INFINITY
    } else {
        count as f64 / secs
    }
}
