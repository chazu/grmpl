//! # grmpl-diff
//!
//! The differential engine (DESIGN.md §3). A `Query` is a dataflow plan held as
//! an immutable value; `find` evaluates it against one edition and `watch`
//! maintains it across editions with genuinely incremental deltas. This crate
//! depends only on `grmpl-core`'s `TraceStore` trait — never on fjall.

pub mod multiset;
pub mod query;
pub mod recursive;
pub mod snapshot;
pub mod watch;
pub mod window;

pub use multiset::Multiset;
pub use query::{
    eval_delta, eval_snapshot, eval_snapshot_with, eval_with, eval_with_recur, Agg, Arrangements,
    MapFn, Pred, Query,
};
pub use recursive::{IncrementalFixpoint, Maintenance};
pub use snapshot::Snapshot;
pub use watch::DeltaStream;
pub use window::{
    consolidate_events, consolidate_window, sliding, tumbling, window, ConsolidatedWindow, Window,
};
