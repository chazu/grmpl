//! # grmpl-transport
//!
//! Implementations of `grmpl_core::Transport` (DESIGN.md §4.2). The in-process
//! net is always available; the iroh-backed QUIC transport is behind the `iroh`
//! feature. This is the only crate that names iroh — it stays below the bright
//! line, and nothing in the language depends on it.

pub mod inproc;
pub mod wire;

pub use inproc::{InProcessNet, InProcessTransport};
pub use wire::{decode_message, encode_message};

#[cfg(feature = "iroh")]
pub mod iroh_transport;
#[cfg(feature = "iroh")]
pub use iroh_transport::IrohTransport;
