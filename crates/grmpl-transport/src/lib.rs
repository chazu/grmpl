//! # grmpl-transport
//!
//! Implementations of `grmpl_core::Transport` (DESIGN.md §4.2): an in-process net
//! between authority domains. A networked transport (iroh is the designed-for
//! one) is deferred with distribution; nothing in the language depends on this
//! crate.

pub mod inproc;

pub use inproc::{InProcessNet, InProcessTransport};
