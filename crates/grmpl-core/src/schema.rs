//! Relation schemas (P1). A schema gives a relation an ordered list of
//! **named, typed columns**. Schemas are stored in the durable registry keyed
//! by [`RelId`] (see [`crate::store::SchemaCatalog`]), versioned by the edition
//! at which each version took effect, and they evolve **additively only** — a
//! new version may append columns but never remove, reorder, retype, or rename
//! an existing one.
//!
//! This module is *above the bright line*: it defines the pure schema value
//! types and the invariant logic (type admission, arity/type checking, and the
//! additive-evolution predicate). The durable map lives in `grmpl-store`; it
//! calls into these predicates rather than re-deriving them.

use crate::value::{Tuple, Value};

/// A column's value type. The concrete variants mirror [`Value`]'s scalar
/// cases; [`Ty::Any`] is the permissive top type an *untyped* surface column
/// lowers to (it admits every value, so it constrains arity but not type).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Ty {
    /// A stable domain identity ([`Value::Ent`]).
    Ent,
    /// A signed 64-bit integer ([`Value::Int`]).
    Int,
    /// Interned text ([`Value::Text`]).
    Text,
    /// A boolean ([`Value::Bool`]).
    Bool,
    /// A nested tuple ([`Value::Tuple`]).
    Tuple,
    /// An opaque byte string ([`Value::Bytes`]).
    Bytes,
    /// Serialized behavior IR ([`Value::Code`]) — a stored, live-redefinable
    /// behavior (P12). Distinct from [`Ty::Bytes`] so a relation column can
    /// declare it *holds code*, which is what the commit boundary re-checks.
    Code,
    /// Any value — an untyped column. Admits everything.
    Any,
}

impl Ty {
    /// Does a value inhabit this type?
    pub fn admits(&self, v: &Value) -> bool {
        match self {
            Ty::Any => true,
            Ty::Ent => matches!(v, Value::Ent(_)),
            Ty::Int => matches!(v, Value::Int(_)),
            Ty::Text => matches!(v, Value::Text(_)),
            Ty::Bool => matches!(v, Value::Bool(_)),
            Ty::Tuple => matches!(v, Value::Tuple(_)),
            Ty::Bytes => matches!(v, Value::Bytes(_)),
            Ty::Code => matches!(v, Value::Code(_)),
        }
    }

    /// A stable spelling for diagnostics and the surface grammar.
    pub fn name(&self) -> &'static str {
        match self {
            Ty::Ent => "Ent",
            Ty::Int => "Int",
            Ty::Text => "Text",
            Ty::Bool => "Bool",
            Ty::Tuple => "Tuple",
            Ty::Bytes => "Bytes",
            Ty::Code => "Code",
            Ty::Any => "Any",
        }
    }

    /// Parse a surface type name (the inverse of [`Ty::name`]). `None` for an
    /// unknown spelling — the caller reports the error with its own context.
    pub fn parse(name: &str) -> Option<Ty> {
        Some(match name {
            "Ent" => Ty::Ent,
            "Int" => Ty::Int,
            "Text" => Ty::Text,
            "Bool" => Ty::Bool,
            "Tuple" => Ty::Tuple,
            "Bytes" => Ty::Bytes,
            "Code" => Ty::Code,
            "Any" => Ty::Any,
            _ => return None,
        })
    }
}

/// One named, typed column of a relation.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Column {
    pub name: String,
    pub ty: Ty,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: Ty) -> Column {
        Column { name: name.into(), ty }
    }
}

/// A relation schema: an ordered list of named, typed columns. The column
/// order *is* the tuple layout — column `i` types tuple position `i`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Schema {
    pub columns: Vec<Column>,
}

impl Schema {
    pub fn new(columns: Vec<Column>) -> Schema {
        Schema { columns }
    }

    /// The number of columns — the relation's arity.
    pub fn arity(&self) -> usize {
        self.columns.len()
    }

    /// The position of the column named `name`, if any (named-column
    /// resolution — the same predicate the surface `view`/`on` layer uses to
    /// turn a column *name* into a tuple index).
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The column named `name`, if any.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Check a tuple against this schema: the arity must match and every value
    /// must inhabit its column's type. `Ok(())` when well-typed; otherwise a
    /// human-readable description of the first violation.
    pub fn check(&self, tuple: &Tuple) -> Result<(), String> {
        let vals = tuple.as_slice();
        if vals.len() != self.columns.len() {
            return Err(format!(
                "arity mismatch: schema has {} column(s), tuple has {}",
                self.columns.len(),
                vals.len()
            ));
        }
        for (i, (col, v)) in self.columns.iter().zip(vals).enumerate() {
            if !col.ty.admits(v) {
                return Err(format!(
                    "column {i} `{}` expects {}, got {v:?}",
                    col.name,
                    col.ty.name()
                ));
            }
        }
        Ok(())
    }

    /// Is `self` an **additive** evolution of `prev`? That is, `prev`'s columns
    /// are an exact prefix of `self`'s (same names *and* types, in order), and
    /// `self` only *appends*. Equal schemas are trivially additive. This is the
    /// only legal shape of schema change in P1: no removal, reorder, retype, or
    /// rename of an existing column.
    pub fn is_additive_over(&self, prev: &Schema) -> bool {
        self.columns.len() >= prev.columns.len()
            && self.columns[..prev.columns.len()] == prev.columns[..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Entity;

    fn schema(cols: &[(&str, Ty)]) -> Schema {
        Schema::new(cols.iter().map(|(n, t)| Column::new(*n, *t)).collect())
    }

    #[test]
    fn admits_matches_value_shape() {
        assert!(Ty::Ent.admits(&Value::Ent(Entity(1))));
        assert!(!Ty::Ent.admits(&Value::Int(1)));
        assert!(Ty::Int.admits(&Value::Int(-4)));
        assert!(Ty::Text.admits(&Value::text("x")));
        assert!(Ty::Bool.admits(&Value::Bool(true)));
        // Any is the top type.
        for v in [Value::Ent(Entity(9)), Value::Int(0), Value::text(""), Value::Bool(false)] {
            assert!(Ty::Any.admits(&v));
        }
    }

    #[test]
    fn check_enforces_arity_then_types() {
        let s = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
        assert!(s.check(&Tuple::from([Value::Ent(Entity(1)), Value::Ent(Entity(2))])).is_ok());
        // wrong arity
        assert!(s.check(&Tuple::from([Value::Ent(Entity(1))])).is_err());
        // right arity, wrong type
        assert!(s.check(&Tuple::from([Value::Ent(Entity(1)), Value::Int(2)])).is_err());
    }

    #[test]
    fn any_column_admits_anything_of_right_arity() {
        let s = schema(&[("a", Ty::Any), ("b", Ty::Any)]);
        assert!(s.check(&Tuple::from([Value::Int(1), Value::text("z")])).is_ok());
        assert!(s.check(&Tuple::from([Value::Int(1)])).is_err()); // arity still enforced
    }

    #[test]
    fn additive_evolution_is_prefix_superset() {
        let base = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
        let added = schema(&[("thing", Ty::Ent), ("place", Ty::Ent), ("since", Ty::Int)]);
        assert!(added.is_additive_over(&base));
        assert!(base.is_additive_over(&base)); // equal is additive

        // Not additive: shrink, retype, rename, reorder.
        assert!(!base.is_additive_over(&added)); // removal
        let retyped = schema(&[("thing", Ty::Ent), ("place", Ty::Int)]);
        assert!(!retyped.is_additive_over(&base));
        let renamed = schema(&[("thing", Ty::Ent), ("room", Ty::Ent)]);
        assert!(!renamed.is_additive_over(&base));
        let reordered = schema(&[("place", Ty::Ent), ("thing", Ty::Ent)]);
        assert!(!reordered.is_additive_over(&base));
    }

    #[test]
    fn column_index_resolves_by_name() {
        let s = schema(&[("thing", Ty::Ent), ("place", Ty::Ent)]);
        assert_eq!(s.column_index("place"), Some(1));
        assert_eq!(s.column_index("nope"), None);
    }
}
