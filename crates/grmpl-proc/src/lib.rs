//! # grmpl-proc
//!
//! The process layer (DESIGN.md §5): the optimistic commit protocol (M4) and,
//! from M5, `Process` and the actor loop. Depends on `grmpl-core` and
//! `grmpl-diff`; never on fjall.

pub mod alloc;
pub mod commit;
pub mod domain;
pub mod gc;
pub mod process;
pub mod replay;
pub mod schedule;
pub mod watch;

pub use alloc::Alloc;
pub use commit::{check_schema, commit_patch, commit_patch_checked, CommitOutcome};
pub use domain::{outbox_len, Domain};
pub use gc::{consolidate_to, min_watch_cursor};
pub use process::{enqueue, inbox_fact, Behavior, Prepared, Process};
pub use replay::{record_run, replay_from, Step};
pub use schedule::{timer_row, ClockDriver, Scheduler, SeqAlloc};
pub use watch::{activation_body, decode_activation, OnWatch};
