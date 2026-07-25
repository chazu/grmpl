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

    /// Relocate a whole relational tuple: shift **every** entity column together,
    /// leaving non-entity columns (names, directions, weights) untouched. This is
    /// the cluster relocation `located(thing, place)` / `exits(from, way, to)` need
    /// — both endpoints move by the same displacement so the sub-world stays
    /// internally connected, while its text and numbers are preserved. (For a
    /// single-entity-column key this is exactly [`apply`](Self::apply).)
    pub fn apply_all(&self, tuple: &Tuple) -> Tuple {
        let cols: Vec<Value> = tuple
            .as_slice()
            .iter()
            .map(|v| match v {
                Value::Ent(e) => Value::Ent(Entity(e.0.wrapping_add(self.shift as u64))),
                other => other.clone(),
            })
            .collect();
        Tuple::new(cols)
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

    /// **Displaced range read `[lo, hi)`, dsp threaded through the descent.**
    /// Rather than materialize the displaced view and filter, transform the *query*
    /// back into the shared tree's coordinates (the inverse dsp), walk its
    /// **WID-pruned** range there, and re-displace only the results — `O(result +
    /// log n)`, pruning whole out-of-range subtrees. This is the `DspLoaf`
    /// discipline: the displacement composes down the traversal onto the query, so
    /// the shared enfilade does the pruning in its own coordinate space.
    ///
    /// Precondition: entity-headed keys, and query bounds within the displaced
    /// block so the inverse displacement does not wrap the id space (the relocation
    /// regime) — then [`Dsp::apply`] is order-preserving and the transformed range
    /// denotes the same set. A bound that underflows the id space under the inverse
    /// dsp is outside the view's coordinate domain.
    pub fn range(&self, lo: &Tuple, hi: &Tuple) -> Vec<(Tuple, Diff)> {
        let inv = self.dsp.inverse();
        let (ilo, ihi) = (inv.apply(lo), inv.apply(hi));
        self.inner.range_collect(&ilo, &ihi).into_iter().map(|(k, v)| (self.dsp.apply(&k), v)).collect()
    }

    /// **Displaced range measure**, answered from the shared tree's cached subtree
    /// measures in `O(log n)` without materializing any row — the WID count over
    /// the displaced span, via the same query-coordinate transform as
    /// [`range`](Self::range).
    pub fn measure_range(&self, lo: &Tuple, hi: &Tuple) -> Count {
        let inv = self.dsp.inverse();
        self.inner.measure_range(&inv.apply(lo), &inv.apply(hi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(n: u64) -> Tuple {
        Tuple::from([Value::Ent(Entity(n)), Value::Int(1)])
    }

    #[test]
    fn apply_all_relocates_every_entity_column() {
        let d = Dsp::by(1000);
        // located(thing=2, place=10): both entity columns shift, together.
        let located = Tuple::from([Value::Ent(Entity(2)), Value::Ent(Entity(10))]);
        assert_eq!(
            d.apply_all(&located),
            Tuple::from([Value::Ent(Entity(1002)), Value::Ent(Entity(1010))])
        );
        // exits(from=10, way="north", to=11): endpoints shift, the direction text
        // and any non-entity column are preserved.
        let exit = Tuple::from([Value::Ent(Entity(10)), Value::text("north"), Value::Ent(Entity(11))]);
        assert_eq!(
            d.apply_all(&exit),
            Tuple::from([Value::Ent(Entity(1010)), Value::text("north"), Value::Ent(Entity(1011))])
        );
        // value(thing=20, coins=5): entity shifts, the Int weight does not.
        let value = Tuple::from([Value::Ent(Entity(20)), Value::Int(5)]);
        assert_eq!(d.apply_all(&value), Tuple::from([Value::Ent(Entity(1020)), Value::Int(5)]));
        // Relocation is invertible column-wise, and composes.
        assert_eq!(d.inverse().apply_all(&d.apply_all(&exit)), exit);
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

    #[test]
    fn displaced_range_and_measure_thread_the_dsp_through_the_walk() {
        // Entities 0..20 → value n; relocate into the 1000-block.
        let mut inner: Tree<Tuple, Diff, Count> = Tree::new();
        for n in 0..20u64 {
            inner = inner.insert(ent(n), n as i64);
        }
        let moved = DspEnf::relocate(inner, Dsp::by(1000));

        // A displaced range query is answered by transforming the query, walking
        // the shared tree's WID range, and re-displacing — it must equal the eager
        // "materialize then filter" over the displaced contents, for every span.
        // Spans stay within the relocated block (bounds ≥ the displaced origin),
        // the non-wrapping regime the transform is defined over.
        let all = moved.to_vec();
        for (a, b) in [(1000u64, 1000), (1003, 1010), (1015, 1025), (1000, 1019), (1000, 1020)] {
            let lo = ent(a);
            let hi = ent(b);
            let mut want: Vec<(Tuple, Diff)> =
                all.iter().filter(|(k, _)| lo <= *k && *k < hi).cloned().collect();
            want.sort();
            let mut got = moved.range(&lo, &hi);
            got.sort();
            assert_eq!(got, want, "displaced range [{a},{b})");
            // The WID measure matches the row count, without materializing.
            assert_eq!(moved.measure_range(&lo, &hi), Count(want.len() as u64), "measure [{a},{b})");
        }
    }
}
