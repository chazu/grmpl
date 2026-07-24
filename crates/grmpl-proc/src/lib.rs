//! # grmpl-proc
//!
//! The process layer (DESIGN.md §5): the optimistic commit protocol (M4) and,
//! from M5, `Process` and the actor loop. Depends on `grmpl-core` and
//! `grmpl-diff`; never on fjall.

pub mod commit;
pub mod domain;
pub mod process;

pub use commit::{check_schema, commit_patch, CommitOutcome};
pub use domain::{outbox_len, Domain};
pub use process::{enqueue, inbox_fact, Behavior, Prepared, Process};
