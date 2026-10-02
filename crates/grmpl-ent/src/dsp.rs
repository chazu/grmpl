//! **DSP — coordinate transforms (E6).**
//!
//! A [`Dsp`] is a displacement: an **invertible** transform of the key
//! coordinate space (Gold's `Dsp` — "necessarily invertable and composable").
//! In Gold, dsps live in the nodes and compose down the descent, which is what
//! makes relocation and virtual copy `O(1)`. That is not built yet: here a
//! displacement is a single shift of the *entity coordinates* of a whole query,
//! and [`DspEnf`] is a displaced read view over a shared Fact enfilade.
//! `EntStore::instance_template` reads through it and commits the relocated
//! facts, so an instance still costs `O(template)`.

use grmpl_core::{Diff, Entity, Tuple, Value};

use crate::measure::{Count, Measure};
use crate::tree::Tree;

/// A coordinate displacement: shift the entity id in a key's column 0 by `shift`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Dsp {
    shift: i64,
}

impl Dsp {
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
}

/// A displaced view of a Fact enfilade: the underlying `inner` tree is **shared**
/// (an `Arc` clone), and the displacement is applied lazily on read — so a
/// relocation costs `O(1)` and shares every node with the original.
pub struct DspEnf<M = Count> {
    inner: Tree<Tuple, Diff, M>,
    dsp: Dsp,
}

impl<M: Measure<Tuple, Diff>> DspEnf<M> {
    /// Relocate `inner` by `dsp` — `O(1)`, sharing all of `inner`'s nodes.
    pub fn relocate(inner: Tree<Tuple, Diff, M>, dsp: Dsp) -> DspEnf<M> {
        DspEnf { inner, dsp }
    }

    /// **Displaced *cluster* range read `[lo, hi)`.** The query is transformed
    /// back into the shared tree's coordinates (the inverse dsp) and pruned there,
    /// so nothing is materialized that the span does not cover; each result is
    /// relocated with [`Dsp::apply_all`], moving **every** entity column together.
    ///
    /// That is what a self-contained sub-world needs: `located(thing, place)` and
    /// `exits(from, way, to)` must move both endpoints by the same displacement
    /// or the instance comes back internally disconnected, while its text and
    /// weights are preserved. The key order is set by the lead column alone, so
    /// transforming the *query* by the lead column stays exact.
    pub fn range_all(&self, lo: &Tuple, hi: &Tuple) -> Vec<(Tuple, Diff)> {
        let inv = self.dsp.inverse();
        self.inner
            .range_collect(&inv.apply(lo), &inv.apply(hi))
            .into_iter()
            .map(|(k, v)| (self.dsp.apply_all(&k), v))
            .collect()
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
        // Relocation is invertible column-wise.
        assert_eq!(d.inverse().apply_all(&d.apply_all(&exit)), exit);
    }

    #[test]
    fn dsp_is_invertible() {
        let d = Dsp::by(1000);
        let k = ent(7);
        assert_eq!(d.inverse().apply(&d.apply(&k)), k);
    }

    #[test]
    fn displaced_range_threads_the_dsp_through_the_walk() {
        // Entities 0..20 → value n; relocate into the 1000-block.
        let mut inner: Tree<Tuple, Diff, Count> = Tree::new();
        for n in 0..20u64 {
            inner = inner.insert(ent(n), n as i64);
        }
        let d = Dsp::by(1000);
        let all: Vec<(Tuple, Diff)> = inner.iter().map(|(k, v)| (d.apply_all(k), *v)).collect();
        let moved = DspEnf::relocate(inner, d);

        // A displaced range query is answered by transforming the query, walking
        // the shared tree's range, and re-displacing — it must equal the eager
        // "materialize then filter" over the displaced contents, for every span.
        // Spans stay within the relocated block (bounds ≥ the displaced origin),
        // the non-wrapping regime the transform is defined over.
        for (a, b) in [(1000u64, 1000), (1003, 1010), (1015, 1025), (1000, 1019), (1000, 1020)] {
            let lo = ent(a);
            let hi = ent(b);
            let want: Vec<(Tuple, Diff)> =
                all.iter().filter(|(k, _)| lo <= *k && *k < hi).cloned().collect();
            assert_eq!(moved.range_all(&lo, &hi), want, "displaced range [{a},{b})");
        }
    }
}
