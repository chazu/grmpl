//! # grmpl
//!
//! The public runtime facade for compiling, opening, and driving a durable
//! language-defined world.
//!
//! This is an **edge** crate: it sits above the semantic core and wires it to
//! clients. It may name a concrete transport (std TCP here) exactly because it
//! is *not* one of the semantic-core crates — the bright line (`DESIGN.md` §1)
//! constrains the core, not the application built on it.
//!
//! [`Runtime`] owns compilation, durable relation resolution, schema
//! registration, queries, processes, inbox sequencing, and watches.
//! [`MooRuntime`] packages the built-in language-defined MOO, while [`Server`]
//! adds transport-independent player provisioning and sessions. [`net::serve`]
//! is only a TCP adapter over that same runtime.

pub mod moo;
pub mod net;
pub mod runtime;
pub mod session;
pub mod watch;

pub use moo::{MooRelations, MooRuntime};
pub use net::serve;
pub use runtime::{
    tokenize, DriveReport, DriveStatus, NamedAuthority, NamedScope, Runtime, RuntimePolicy,
};
pub use session::{Server, Session};
pub use watch::{Delivered, Subscription, WatchRelations};
