//! # grmpl-lang
//!
//! A minimal text surface for grmpl (DESIGN.md §7, §8). It parses the
//! declarative forms — `rel`, `view`, `form` — and lowers them to the core
//! `Query` and `Pattern` constructors. This demonstrates the design's central
//! claim that a MOO-like facade (`lamp.location`, a command grammar) is just
//! specialized syntax for relational views and structural patterns, not a
//! privileged engine.
//!
//! `on` binds executable behavior, in either of two coexisting surfaces over the
//! same effect plumbing: a statement body (`match Tag(v) { resolve … / assert …
//! }`) or a P11 **concatenative** point-free body (`match Tag(v) [ … ]`, see
//! [`concat`]). View bodies keep their named logic variables; the handler words
//! carry declared stack effects.
//!
//! ```text
//! rel located(thing, place)
//! view visible(viewer) {
//!     located(viewer, room)
//!     located(thing, room)
//!     named(thing, name)
//!     permits(viewer, "see", thing)
//!     yield thing, name
//! }
//! form command {
//!     "take" name -> Take(name)
//!     "look"      -> Look()
//! }
//! ```

pub mod ast;
pub mod compile;
pub mod concat;
pub mod ir;
pub mod lexer;
pub mod parser;

pub use compile::{NamedAgg, Program};
pub use concat::{ConcatArm, Schemas, StackEffect, Word};
pub use ir::{Comp, CtorSpec, FormIr, MapExpr, PredExpr, QueryIr, RowExpr, RuleIr};
pub use parser::parse;
