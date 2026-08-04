//! # grmpl-proc
//!
//! The process layer (DESIGN.md §5): the optimistic commit protocol (M4) and,
//! from M5, `Process` and the actor loop. Depends on `grmpl-core` and
//! `grmpl-diff`; never on fjall.

pub mod alloc;
pub mod commit;
pub mod derived;
pub mod domain;
pub mod gc;
pub mod process;
pub mod replay;
pub mod retry;
pub mod schedule;
pub mod watch;

pub use alloc::Alloc;
pub use commit::{check_schema, commit_patch, commit_patch_checked, CommitOutcome};
pub use derived::Materialized;
pub use domain::{outbox_len, Domain};
pub use gc::{consolidate_to, min_watch_cursor};
pub use process::{enqueue, enqueue_seq, inbox_fact, seed_seq, Behavior, Prepared, Process};
pub use replay::{record_run, replay_from, Step};
pub use retry::{commit_retrying, Backoff};
pub use schedule::{timer_row, ClockDriver, Scheduler, SeqAlloc};
pub use watch::{activation_body, decode_activation, OnWatch};
