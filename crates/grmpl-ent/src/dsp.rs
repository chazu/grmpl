//! **DSP — coordinate transforms (E6).**
//!
//! A [`Dsp`] is a displacement: an **invertible, composable** transform of the key
//! coordinate space (Gold's `Dsp` — "necessarily invertable and composable").
//! Paired with structural sharing it is the mechanism of **O(1) relocation and
//! virtual copy**: a subtree is grafted into a new key range by *sharing its
//! nodes* and storing one displacement, rather than copying and re-keying every
//! node (which content-hashing alone cannot avoid — every relocated key changes).
//!
//! Here a displacement shifts the *entity coordinate* of a key (column 0), the
//! natural grmpl relocation — grafting a relation's entities into a new id range
//! (namespacing / clustering, `idea.md` §10). [`DspEnf`] is a displaced *view* of
//! a Fact enfilade that **shares** the underlying tree and applies the dsp lazily
//! on read — the composed-down-the-descent discipline of `DspLoaf`.

use grmpl_core::{Diff, Entity, Tuple, Value};

use crate::measure::Count;
use crate::tree::Tree;

/// A coordinate displacement: shift the entity id in a key's column 0 by `shift`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Dsp {
    shift: i64,
}

impl Dsp {
    /// The identity displacement (no-op).
    pub fn identity() -> Dsp {
        Dsp { shift: 0 }
    }

    /// Displace the entity coordinate by `shift`.
    pub fn by(shift: i64) -> Dsp {
        Dsp { shift }
    }

    /// Apply the displacement to a key: entity in column 0 is shifted; other
    /// shapes pass through unchanged.
    pub fn apply(&self, key: &Tuple) -> Tuple {
        match key.as_slice().first() {
            Some(Value::Ent(e)) => {
                let shifted = Value::Ent(Entity(e.0.wrapping_add(self.shift as u64)));
                let mut cols: Vec<Value> = key.as_slice().to_vec();
                cols[0] = shifted;
                Tuple::new(cols)
            }
            _ => key.clone(),
        }
    }

    /// The inverse displacement (`apply` then `inverse().apply` is the identity).
    pub fn inverse(&self) -> Dsp {
        Dsp { shift: self.shift.wrapping_neg() }
    }

    /// Composition: `self.compose(then)` displaces by `self` then by `then`
    /// (associative; identity is the unit).
    pub fn compose(&self, then: &Dsp) -> Dsp {
        Dsp { shift: self.shift.wrapping_add(then.shift) }
    }
}

/// A displaced view of a Fact enfilade: the underlying `inner` tree is **shared**
/// (an `Arc` clone), and the displacement is applied lazily on read — so a
/// relocation costs `O(1)` and shares every node with the original.
pub struct DspEnf {
    inner: Tree<Tuple, Diff, Count>,
    dsp: Dsp,
}

impl DspEnf {
    /// Relocate `inner` by `dsp` — `O(1)`, sharing all of `inner`'s nodes.
    pub fn relocate(inner: Tree<Tuple, Diff, Count>, dsp: Dsp) -> DspEnf {
        DspEnf { inner, dsp }
    }

    /// The value at a *displaced* key: invert the displacement and look it up in
    /// the shared tree — no copy.
    pub fn get(&self, key: &Tuple) -> Option<Diff> {
        self.inner.get(&self.dsp.inverse().apply(key)).copied()
    }

    /// The displaced contents `(key, value)`, in displaced-key order (the entity
    /// shift is order-preserving within column 0 for a fixed shape).
    pub fn to_vec(&self) -> Vec<(Tuple, Diff)> {
        self.inner.iter().map(|(k, v)| (self.dsp.apply(k), *v)).collect()
    }

    /// Compose a further displacement onto this view (still `O(1)`, still shared).
    pub fn then(&self, more: Dsp) -> DspEnf {
        DspEnf { inner: self.inner.clone(), dsp: self.dsp.compose(&more) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(n: u64) -> Tuple {
        Tuple::from([Value::Ent(Entity(n)), Value::Int(1)])
    }

    #[test]
    fn dsp_is_invertible_and_composable() {
        let d = Dsp::by(1000);
        let k = ent(7);
        // apply then inverse is the identity.
        assert_eq!(d.inverse().apply(&d.apply(&k)), k);
        // composition adds shifts, associatively, with identity as unit.
        assert_eq!(Dsp::by(5).compose(&Dsp::by(3)), Dsp::by(8));
        assert_eq!(d.compose(&Dsp::identity()), d);
        assert_eq!(Dsp::identity().apply(&k), k);
    }

    #[test]
    fn relocation_shares_nodes_and_reads_displaced() {
        // A Fact enfilade over entities 0..5.
        let mut inner: Tree<Tuple, Diff, Count> = Tree::new();
        for n in 0..5u64 {
            inner = inner.insert(ent(n), n as i64 * 10);
        }
        // Relocate the whole relation into the 1000-block — O(1), shares `inner`.
        let moved = DspEnf::relocate(inner.clone(), Dsp::by(1000));

        // Displaced reads: entity n now lives at n+1000, same value.
        for n in 0..5u64 {
            assert_eq!(moved.get(&ent(n + 1000)), Some(n as i64 * 10));
            assert_eq!(moved.get(&ent(n)), None); // the original coordinate is empty in the moved view
        }
        // The displaced contents are the originals shifted by 1000.
        let want: Vec<(Tuple, Diff)> =
            inner.iter().map(|(k, v)| (Dsp::by(1000).apply(k), *v)).collect();
        assert_eq!(moved.to_vec(), want);

        // Composing another shift is still O(1) and correct.
        let moved2 = moved.then(Dsp::by(7));
        assert_eq!(moved2.get(&ent(1007)), Some(0)); // entity 0 -> 1000 -> 1007
    }
}
