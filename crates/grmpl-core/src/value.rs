//! Values, tuples, entities, and interned identifiers (DESIGN.md §2.1).

use std::sync::Arc;

/// A stable identity in the modeled world. An entity is a legitimate domain
/// value (Object law). It is **not** hidden tuple identity — tuples are
/// structural, identified by content.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Entity(pub u64);

/// A domain value.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Value {
    Ent(Entity),
    Int(i64),
    Text(Arc<str>),
    Bool(bool),
    Tuple(Arc<[Value]>),
    /// An opaque, ordered byte string. The input for `form` over byte
    /// sequences (P9): a `Pattern` runs over its bytes via
    /// `grmpl_pattern::BytesInput`. Structural — identity is by content.
    Bytes(Arc<[u8]>),
    /// A **code-carrying** value: the serialized bytes of a P7 IR *behavior*
    /// (P12 — behaviors as relations). The bytes are opaque to the core (which
    /// names no IR — the bright line): only `grmpl-lang` encodes/decodes them
    /// (`grmpl_lang::behavior`). It is a *distinct* variant from [`Value::Bytes`]
    /// precisely so it can carry a distinct schema type ([`crate::Ty::Code`]) and
    /// so the commit boundary knows which cells hold live code to re-check
    /// (`grmpl_core::BehaviorChecker`). Structural — identity is by content
    /// (byte order), so it is deterministically `Ord`/`Hash`/`Eq` like any value.
    Code(Arc<[u8]>),
}

impl Value {
    pub fn text(s: impl AsRef<str>) -> Value {
        Value::Text(Arc::from(s.as_ref()))
    }

    pub fn bytes(b: impl AsRef<[u8]>) -> Value {
        Value::Bytes(Arc::from(b.as_ref()))
    }

    /// Wrap serialized behavior IR bytes as a code-carrying value (P12). The
    /// bytes are produced by `grmpl_lang::behavior::encode_behavior`.
    pub fn code(b: impl AsRef<[u8]>) -> Value {
        Value::Code(Arc::from(b.as_ref()))
    }
}

impl From<Entity> for Value {
    fn from(e: Entity) -> Value {
        Value::Ent(e)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::text(s)
    }
}

/// A row in a relation. Structural: identity is by content, never hidden.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Tuple(pub Arc<[Value]>);

impl Tuple {
    pub fn new(vals: impl Into<Arc<[Value]>>) -> Self {
        Tuple(vals.into())
    }

    pub fn as_slice(&self) -> &[Value] {
        &self.0
    }

    pub fn arity(&self) -> usize {
        self.0.len()
    }
}

impl<const N: usize> From<[Value; N]> for Tuple {
    fn from(vals: [Value; N]) -> Tuple {
        Tuple(Arc::from(vals))
    }
}

/// Interned relation name. The schema (later) maps it to an arity + column
/// types; the store maps it to a physical keyspace.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RelId(pub u32);

/// Identity of an authority domain. In v1 this is a local id; in the
/// distributed future it maps to an iroh `EndpointId` (Ed25519 pubkey),
/// *below the line* only.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DomainId(pub u64);
