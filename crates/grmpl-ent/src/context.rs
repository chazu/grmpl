//! **The context enfilade: DSPative inherited context (E5).**
//!
//! Context — namespace, schema, authority, placement, simulation parameters —
//! flows **down** a scope cover: a child scope inherits its ancestors' context
//! unless it overrides a key (Xanadu's *dsp*-inherited context, `idea.md` §3/§10).
//! A scope is a path of ids (an enclosing namespace/zone/actor); resolving a key
//! at a scope returns the value set at the **nearest enclosing** scope.
//!
//! This is the inheritance semantics over a scope map. (Authority proper lives in
//! the canopy endorsement lattice, per Gold; this carries the ambient
//! namespace/schema/placement context.)

use std::collections::{BTreeMap, HashMap};

use grmpl_core::Value;

/// A scope path: the empty path is the root; `[1]` is enclosed by root; `[1, 2]`
/// by `[1]`; and so on.
pub type Scope = Vec<u64>;

/// A tree of scopes, each carrying overrides; lookups inherit down the cover.
#[derive(Default)]
pub struct Context {
    by_scope: BTreeMap<Scope, HashMap<String, Value>>,
}

impl Context {
    pub fn new() -> Context {
        Context::default()
    }

    /// Set `key = val` at `scope` (overrides any inherited value for descendants
    /// of `scope`).
    pub fn set(&mut self, scope: &[u64], key: &str, val: Value) {
        self.by_scope.entry(scope.to_vec()).or_default().insert(key.to_string(), val);
    }

    /// Resolve `key` as-of `scope`: the value set at the **nearest enclosing**
    /// scope (the longest prefix of `scope`, including `scope` itself, that binds
    /// `key`), or `None`.
    pub fn get(&self, scope: &[u64], key: &str) -> Option<&Value> {
        // Walk from the scope itself up to the root, returning the first binding.
        for len in (0..=scope.len()).rev() {
            if let Some(map) = self.by_scope.get(&scope[..len]) {
                if let Some(v) = map.get(key) {
                    return Some(v);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_inherits_down_scopes_and_overrides() {
        let mut c = Context::new();
        c.set(&[], "namespace", Value::text("root"));
        c.set(&[], "schema", Value::Int(1));
        c.set(&[1], "namespace", Value::text("zone-1")); // override in zone 1

        // A deep scope inherits the nearest enclosing binding.
        assert_eq!(c.get(&[1, 2, 3], "namespace"), Some(&Value::text("zone-1")));
        // …and still inherits root for keys zone-1 didn't override.
        assert_eq!(c.get(&[1, 2, 3], "schema"), Some(&Value::Int(1)));
        // A sibling scope not under zone 1 sees the root namespace.
        assert_eq!(c.get(&[9], "namespace"), Some(&Value::text("root")));
        // The overriding scope itself sees its own value.
        assert_eq!(c.get(&[1], "namespace"), Some(&Value::text("zone-1")));
        // An unbound key is None everywhere.
        assert_eq!(c.get(&[1, 2], "placement"), None);
    }
}
