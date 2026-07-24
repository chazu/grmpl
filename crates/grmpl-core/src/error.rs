//! Core error type. Kept dependency-free (no `thiserror`) so the semantic core
//! stays above the line and light.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A substrate (store/transport) failure, surfaced opaquely so the core
    /// never learns which substrate produced it.
    Store(String),
    /// A patch tried to write outside its authority domain (Authority law).
    Authority(String),
    /// A write violated its relation's schema — wrong arity or column type — or
    /// a schema evolution was not additive (P1 relation schemas).
    Schema(String),
    /// An optimistic commit's precondition no longer held (DESIGN.md §5.2).
    PreconditionFailed,
    /// A dataflow plan is ill-formed — e.g. a non-linear operator such as
    /// `Reduce` (an aggregate) placed inside a recursive `Iterate`, whose
    /// fixpoint requires a monotone, linear step (grmpl-diff P2).
    Query(String),
    /// A physical (de)serialization failure below the line.
    Codec(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store(s) => write!(f, "store error: {s}"),
            Error::Authority(s) => write!(f, "authority violation: {s}"),
            Error::Schema(s) => write!(f, "schema violation: {s}"),
            Error::PreconditionFailed => write!(f, "precondition failed"),
            Error::Query(s) => write!(f, "invalid query: {s}"),
            Error::Codec(s) => write!(f, "codec error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
