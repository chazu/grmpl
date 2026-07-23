//! # grmpl-lang
//!
//! A minimal text surface for grmpl (DESIGN.md §7, §8). It parses the
//! declarative forms — `rel`, `view`, `form` — and lowers them to the core
//! `Query` and `Pattern` constructors. This demonstrates the design's central
//! claim that a MOO-like facade (`lamp.location`, a command grammar) is just
//! specialized syntax for relational views and structural patterns, not a
//! privileged engine.
//!
//! `on` (which binds executable behavior) stays a programmatic construction in
//! v1, since it needs an action/expression sublanguage.
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
pub mod lexer;
pub mod parser;

pub use compile::Program;
pub use parser::parse;
