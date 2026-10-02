//! **The context enfilade (G-5): the durable catalog and schema registry.**
//!
//! In the design (`idea.md` §1, §10) context — namespace, schema, authority,
//! placement, simulation parameters — flows **down** a scope cover as
//! DSP-inherited context. That inheritance is not built yet: today every binding
//! lives at the root scope, and the scope column is reserved for it.
//!
//! The bindings themselves are a real enfilade, not a map beside one: bindings live in a persistent,
//! measured [`Tree`] over the granfilade, versioned and structurally shared like
//! the Fact and Edition enfilades, and reachable from GC as a live root.
//!
//! **The catalog and the schema registry are bindings in it.** `CLAUDE.md` names
//! both load-bearing — the name→[`RelId`] map is append-only and durable, and the
//! schema registry is versioned by the edition each version took effect — and
//! `DESIGN.md` §1 puts the *contract* in the core while the durable map is a
//! store concern. That durable map is exactly what a context enfilade is for
//! (plan v4 §1.3: "namespace/schema/placement inherited down scope covers"), so
//! they are bindings at the root scope rather than a second mechanism bolted
//! alongside. `schema_at` then falls out as a **WID range walk** over the
//! `(rel, edition)` key span — an as-of query answered by the enfilade itself.
//!
//! Key layout, all under one ordered tree:
//!
//! ```text
//! (scope, NS_CATALOG, name)            -> Int(rel id)
//! (scope, NS_SCHEMA,  rel, edition)    -> Bytes(wire::encode_schema)
//! ```

use grmpl_core::{RelId, Tuple, Value};

use crate::measure::Count;
use crate::tree::Tree;

/// A scope path: the empty path is the root; `[1]` is enclosed by root; `[1, 2]`
/// by `[1]`; and so on.
pub type Scope = Vec<u64>;

/// The context enfilade: one persistent measured tree of scoped bindings.
pub type ContextEnf = Tree<Tuple, Value, Count>;

// Namespace 0 is reserved for scope-inherited context bindings.

/// The relation-name catalog.
pub const NS_CATALOG: i64 = 1;
/// The edition-versioned schema registry.
pub const NS_SCHEMA: i64 = 2;

/// The root scope — where the catalog and schema registry bind.
pub const ROOT_SCOPE: &[u64] = &[];

fn scope_value(scope: &[u64]) -> Value {
    Value::Tuple(scope.iter().map(|s| Value::Int(*s as i64)).collect())
}

/// `(root, NS_CATALOG, name)`.
pub fn catalog_key(name: &str) -> Tuple {
    Tuple::from([scope_value(ROOT_SCOPE), Value::Int(NS_CATALOG), Value::text(name)])
}

/// The half-open key span covering every catalog binding, for `entries`.
pub fn catalog_span() -> (Tuple, Tuple) {
    (
        Tuple::from([scope_value(ROOT_SCOPE), Value::Int(NS_CATALOG)]),
        Tuple::from([scope_value(ROOT_SCOPE), Value::Int(NS_CATALOG + 1)]),
    )
}

/// An edition as a key column. Editions are `u64` but a [`Value::Int`] is `i64`,
/// so the cast is **saturating**: a plain `as i64` would wrap a sentinel like
/// `u64::MAX` to `-1` and silently invert the span it was meant to bound.
fn edition_value(edition: u64) -> Value {
    Value::Int(edition.min(i64::MAX as u64) as i64)
}

/// `(root, NS_SCHEMA, rel, edition)`.
pub fn schema_key(rel: RelId, edition: u64) -> Tuple {
    Tuple::from([
        scope_value(ROOT_SCOPE),
        Value::Int(NS_SCHEMA),
        Value::Int(rel.0 as i64),
        edition_value(edition),
    ])
}

/// The half-open key span of `rel`'s schema versions with edition in
/// `[from, to)` — the span a WID range walk prunes to for an as-of query.
pub fn schema_span(rel: RelId, from: u64, to: u64) -> (Tuple, Tuple) {
    (schema_key(rel, from), schema_key(rel, to))
}

/// The span of **every** version of `rel`'s schema. The upper bound is the start
/// of the *next* relation — a three-column key, which sorts below every
/// four-column key of `rel` — so no real edition can fall on the boundary.
pub fn schema_all_span(rel: RelId) -> (Tuple, Tuple) {
    (
        schema_key(rel, 0),
        Tuple::from([
            scope_value(ROOT_SCOPE),
            Value::Int(NS_SCHEMA),
            Value::Int(rel.0 as i64 + 1),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespaces never collide, and each one's span is contiguous —
    /// which is what makes `entries` and `schema_at` range walks rather than
    /// scans.
    #[test]
    fn namespaces_are_disjoint_contiguous_spans() {
        let mut c = ContextEnf::new();
        c = c.insert(catalog_key("located"), Value::Int(1));
        c = c.insert(catalog_key("named"), Value::Int(2));
        c = c.insert(schema_key(RelId(1), 5), Value::Bytes(vec![9].into()));

        let (lo, hi) = catalog_span();
        let cat = c.range_collect(&lo, &hi);
        assert_eq!(cat.len(), 2, "the catalog span must hold exactly the catalog");
        assert_eq!(cat[0].1, Value::Int(1), "and stay sorted by name");

        let (lo, hi) = schema_all_span(RelId(1));
        assert_eq!(c.range_collect(&lo, &hi).len(), 1);
        // A different relation's span is empty — the span really is per-relation.
        let (lo, hi) = schema_all_span(RelId(2));
        assert!(c.range_collect(&lo, &hi).is_empty());
    }
}
