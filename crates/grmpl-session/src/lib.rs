//! # grmpl-session
//!
//! Client sessions & world construction (DESIGN.md §5–6, roadmap **P3**).
//!
//! This is an **edge** crate: it sits above the semantic core and wires it to
//! clients. It may name a concrete transport (std TCP here) exactly because it
//! is *not* one of the semantic-core crates — the bright line (`DESIGN.md` §1)
//! constrains the core, not the application built on it.
//!
//! What P3 adds:
//!
//! * **Replay-safe entity allocation** — [`grmpl_proc::Alloc`] over the
//!   [`world::ENTITY_SEQ`] counter relation; every new room/object/player id is
//!   read from the committed counter and bumped in the same patch.
//! * **Provisioning** — [`Server::login`] turns a connection into a player:
//!   a name is the identity (minimal auth, durable across reconnects), and a
//!   fresh player is *spawned as a commit* (allocated, named, placed).
//! * **Verbs as ordinary patches** — `dig` / `create` / `take` / `go` / `look`
//!   in [`world`] are plain guarded [`grmpl_core::Patch`]es, no privileged
//!   engine path.
//! * **A line-based TCP session layer** — [`net::serve`], with the interim
//!   single-writer inbox-seq scheme (the durable allocator is P4).
//!
//! The acceptance scenario — build a room and race for a lamp, entirely through
//! client commands — lives in `tests/`.

pub mod net;
pub mod session;
pub mod watch;
pub mod world;

pub use net::serve;
pub use session::{Server, Session};
pub use watch::{Delivered, Subscription};
