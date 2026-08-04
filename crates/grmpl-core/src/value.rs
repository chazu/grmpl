//! Values, tuples, entities, and interned identifiers (DESIGN.md §2.1).

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// A stable identity in the modeled world. An entity is a legitimate domain
/// value (Object law). It is **not** hidden tuple identity — tuples are
/// structural, identified by content.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Entity(pub u64);

/// A finite, canonical IEEE-754 binary64 value.
///
/// `Value` is structural and therefore must remain totally ordered, hashable,
/// and equality-comparable. Raw `f64` cannot satisfy those laws because NaN is
/// not equal to itself and IEEE distinguishes two signed zero encodings. This
/// wrapper rejects every non-finite value and canonicalizes `-0.0` to `0.0`, so
/// equality, hashing, ordering, and the durable wire representation agree.
#[derive(Copy, Clone)]
pub struct FiniteF64(u64);

impl FiniteF64 {
    /// Construct a finite value, normalizing either signed zero to `+0.0`.
    pub fn new(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let canonical = if value == 0.0 { 0.0 } else { value };
        Some(Self(canonical.to_bits()))
    }

    /// Decode canonical IEEE bits. Non-finite payloads are rejected and
    /// negative zero is normalized just as it is at every other boundary.
    pub fn from_bits(bits: u64) -> Option<Self> {
        Self::new(f64::from_bits(bits))
    }

    /// The finite Rust value.
    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Canonical IEEE bits, suitable for the durable big-endian wire form.
    pub fn to_bits(self) -> u64 {
        self.0
    }
}

impl TryFrom<f64> for FiniteF64 {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        FiniteF64::new(value).ok_or("float must be finite")
    }
}

impl From<FiniteF64> for f64 {
    fn from(value: FiniteF64) -> Self {
        value.get()
    }
}

impl PartialEq for FiniteF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for FiniteF64 {}

impl Hash for FiniteF64 {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl PartialOrd for FiniteF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FiniteF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.get().total_cmp(&other.get())
    }
}

impl fmt::Debug for FiniteF64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FiniteF64").field(&self.get()).finish()
    }
}

impl fmt::Display for FiniteF64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// A domain value.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Value {
    Ent(Entity),
    Int(i64),
    /// A finite, canonical IEEE-754 binary64 scalar.
    Float(FiniteF64),
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

    /// Construct a finite floating-point value. Returns `None` for NaN or
    /// either infinity; signed zero is canonicalized.
    pub fn float(value: f64) -> Option<Value> {
        FiniteF64::new(value).map(Value::Float)
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
impl From<FiniteF64> for Value {
    fn from(value: FiniteF64) -> Value {
        Value::Float(value)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    #[test]
    fn finite_float_rejects_non_finite_values() {
        assert!(FiniteF64::new(f64::NAN).is_none());
        assert!(FiniteF64::new(f64::INFINITY).is_none());
        assert!(FiniteF64::new(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn signed_zero_has_one_structural_identity() {
        let positive = FiniteF64::new(0.0).unwrap();
        let negative = FiniteF64::new(-0.0).unwrap();
        assert_eq!(positive, negative);
        assert_eq!(positive.to_bits(), 0);
        assert_eq!(negative.to_bits(), 0);

        let mut hash = HashSet::new();
        hash.insert(positive);
        hash.insert(negative);
        assert_eq!(hash.len(), 1);
    }

    #[test]
    fn finite_float_order_is_numeric_and_total() {
        let values = [-10.5, -0.0, 0.0, 0.25, f64::MAX];
        let set: BTreeSet<_> = values
            .into_iter()
            .map(|v| FiniteF64::new(v).unwrap())
            .collect();
        assert_eq!(
            set.into_iter().map(FiniteF64::get).collect::<Vec<_>>(),
            vec![-10.5, 0.0, 0.25, f64::MAX]
        );
    }
}
