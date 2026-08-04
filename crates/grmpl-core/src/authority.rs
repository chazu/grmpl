//! `Authority` — the unit of atomic commit (DESIGN.md §2.3 #7).
//!
//! An authority domain owns a slice of the world it may write. One atomic
//! commit touches one authority domain (Authority law); cross-domain
//! consequences are messages.

use crate::fact::Fact;
use crate::value::{DomainId, RelId, Value};

/// An inclusive bound on one column of a relation's key, letting an authority
/// own a *slice* of a relation (e.g. `located[zone-17]`). v1 supports Entity /
/// Int columns; other column types are never in range.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyRange {
    pub col: usize,
    pub lo: i64,
    pub hi: i64,
}

impl KeyRange {
    fn contains(&self, tuple: &crate::value::Tuple) -> bool {
        match tuple.as_slice().get(self.col) {
            Some(Value::Ent(e)) => (e.0 as i64) >= self.lo && (e.0 as i64) <= self.hi,
            Some(Value::Int(i)) => *i >= self.lo && *i <= self.hi,
            _ => false,
        }
    }
}

/// A slice of the world: a whole relation (`range = None`) or a key-range of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Scope {
    pub rel: RelId,
    pub range: Option<KeyRange>,
}

impl Scope {
    pub fn whole(rel: RelId) -> Scope {
        Scope { rel, range: None }
    }
    pub fn slice(rel: RelId, range: KeyRange) -> Scope {
        Scope {
            rel,
            range: Some(range),
        }
    }
}

/// The relations (or key-ranges) a process may assert/retract into.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Authority {
    pub domain: DomainId,
    pub owns: Vec<Scope>,
}

impl Authority {
    pub fn new(domain: DomainId, owns: Vec<Scope>) -> Authority {
        Authority { domain, owns }
    }

    /// Does this authority permit writing `fact`?
    pub fn permits(&self, fact: &Fact) -> bool {
        self.owns
            .iter()
            .any(|s| s.rel == fact.rel && s.range.as_ref().is_none_or(|r| r.contains(&fact.tuple)))
    }
}
