//! # grmpl-core
//!
//! The semantic core of grmpl. This crate is *above the bright line* (see
//! `DESIGN.md` §1): it defines the pure value types, the differential substrate
//! (`Time`, `Edition`, `Diff`, `Update`), and the trait boundary to the
//! substrate (`TraceStore`, `EditionStore`). It names nothing below the line —
//! no LSM tree, no QUIC. Substrate crates depend on *these traits*, never the
//! reverse.

pub mod authority;
pub mod error;
pub mod fact;
pub mod patch;
pub mod schema;
pub mod store;
pub mod time;
pub mod transport;
pub mod value;
pub mod wire;

pub use authority::{Authority, KeyRange, Scope};
pub use error::{Error, Result};
pub use fact::Fact;
pub use patch::{CursorMove, Message, Patch, Scheduled};
pub use schema::{Column, Schema, Ty};
pub use store::{Catalog, EditionStore, NoSchemas, SchemaCatalog, TraceStore};
pub use transport::Transport;
pub use time::{Diff, Edition, Time, Update};
pub use value::{DomainId, Entity, RelId, Tuple, Value};
