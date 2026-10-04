//! **DSP — displacements of the key coordinate space.**
//!
//! In Xanadu Gold every pointer to a subtree carries a *dsp*: the subtree's
//! position relative to its parent. A node's keys are stored in its own local
//! frame, and a descent accumulates the dsps on the way down to recover absolute
//! positions. That is what makes relocation and virtual copy cheap: moving a
//! subtree, or pointing a second parent at it, changes one number on one edge
//! and shares every node beneath.
//!
//! [`Tree`](crate::tree::Tree) carries a dsp on every handle. This module says
//! what a displacement *does* to a key: [`Displace`]. grmpl's displacements are
//! entity shifts — a sub-world relocated into a fresh id block moves every
//! entity it mentions by the same amount, so its rooms, exits and items stay
//! connected while its text and numbers are preserved.

use std::cmp::Ordering;

use grmpl_core::{Entity, Tuple, Value};

/// A key coordinate space that displacements act on.
///
/// Laws, which the tree relies on:
///
/// * **Identity:** `k.displace(0) == k`.
/// * **Additive:** `k.displace(a).displace(b) == k.displace(a + b)` (wrapping).
/// * **Order-preserving** over the keys of any subtree that carries a non-zero
///   dsp: `a < b` implies `a.displace(d) < b.displace(d)`. The tree keeps its
///   separators in local frames and compares there, so a displacement that
///   reordered keys would corrupt the search.
///
/// A key type with no meaningful displacement implements it as the identity.
/// Grafting such a tree by a non-zero amount is then refused, because the
/// target span is the source span and is already occupied.
///
/// The tree only ever moves *stored* keys up into a query's frame, never a
/// query down into a subtree's: the order law holds over the keys a subtree
/// holds, not over arbitrary keys, and an entity id below a block's offset would
/// wrap if moved down.
pub trait Displace: Ord + Clone {
    /// This key moved by `by`.
    fn displace(&self, by: i64) -> Self;

    /// `self.displace(by).cmp(other)`. Types that can compare without building
    /// the displaced key override it; the tree calls it on every comparison
    /// below a displaced subtree.
    fn cmp_displaced(&self, by: i64, other: &Self) -> Ordering {
        if by == 0 {
            self.cmp(other)
        } else {
            self.displace(by).cmp(other)
        }
    }
}

/// A tuple moves by shifting **every** entity cell together; other cells (names,
/// directions, weights, nested tuples) are untouched.
///
/// Order-preserving as long as no entity cell wraps the id space: lexicographic
/// order compares cell by cell, an entity shifted by `d` stays below another
/// shifted by `d`, and the variant order between an entity and a non-entity cell
/// does not change.
impl Displace for Tuple {
    fn displace(&self, by: i64) -> Self {
        if by == 0 {
            return self.clone();
        }
        Tuple::new(
            self.as_slice()
                .iter()
                .map(|v| match v {
                    Value::Ent(e) => Value::Ent(Entity(e.0.wrapping_add(by as u64))),
                    other => other.clone(),
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Cell by cell, exactly as the derived lexicographic order compares: two
    /// entity cells compare by shifted id, and any other pair by the ordinary
    /// order — a displacement cannot change the variant order between them.
    fn cmp_displaced(&self, by: i64, other: &Self) -> Ordering {
        if by == 0 {
            return self.cmp(other);
        }
        let (a, b) = (self.as_slice(), other.as_slice());
        for (x, y) in a.iter().zip(b) {
            let o = match (x, y) {
                (Value::Ent(x), Value::Ent(y)) => x.0.wrapping_add(by as u64).cmp(&y.0),
                _ => x.cmp(y),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        a.len().cmp(&b.len())
    }
}

/// Coordinates with no displacement: edition numbers, relation ids, interest and
/// branch ids, and content keys.
macro_rules! fixed_coordinate {
    ($($t:ty),*) => {
        $(impl Displace for $t {
            fn displace(&self, _by: i64) -> Self {
                *self
            }
        })*
    };
}

fixed_coordinate!(u64, u32, i64, [u8; 32]);

impl<A: Displace, B: Displace> Displace for (A, B) {
    fn displace(&self, by: i64) -> Self {
        (self.0.displace(by), self.1.displace(by))
    }
    fn cmp_displaced(&self, by: i64, other: &Self) -> Ordering {
        self.0
            .cmp_displaced(by, &other.0)
            .then_with(|| self.1.cmp_displaced(by, &other.1))
    }
}

impl<A: Displace, B: Displace, C: Displace> Displace for (A, B, C) {
    fn displace(&self, by: i64) -> Self {
        (self.0.displace(by), self.1.displace(by), self.2.displace(by))
    }
    fn cmp_displaced(&self, by: i64, other: &Self) -> Ordering {
        self.0
            .cmp_displaced(by, &other.0)
            .then_with(|| self.1.cmp_displaced(by, &other.1))
            .then_with(|| self.2.cmp_displaced(by, &other.2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(n: u64) -> Value {
        Value::Ent(Entity(n))
    }

    #[test]
    fn a_tuple_moves_every_entity_column_together() {
        // exits(from=10, way="north", to=11): endpoints shift, the direction text
        // is preserved.
        let exit = Tuple::from([ent(10), Value::text("north"), ent(11)]);
        assert_eq!(
            exit.displace(1000),
            Tuple::from([ent(1010), Value::text("north"), ent(1011)])
        );
        // value(thing=20, coins=5): the Int weight does not move.
        let value = Tuple::from([ent(20), Value::Int(5)]);
        assert_eq!(value.displace(1000), Tuple::from([ent(1020), Value::Int(5)]));
    }

    #[test]
    fn displacement_is_additive_with_identity_zero() {
        let k = Tuple::from([ent(7), Value::text("x"), ent(9)]);
        assert_eq!(k.displace(0), k);
        assert_eq!(k.displace(5).displace(-2), k.displace(3));
        assert_eq!(k.displace(1000).displace(-1000), k);
    }

    #[test]
    fn cmp_displaced_agrees_with_displacing_first() {
        let keys = [
            Tuple::from([ent(1)]),
            Tuple::from([ent(1), Value::Int(3)]),
            Tuple::from([ent(1), ent(4)]),
            Tuple::from([ent(900), Value::text("a")]),
            Tuple::from([ent(1001), ent(2)]),
            Tuple::from([Value::Int(0), ent(1)]),
            Tuple::from([ent(1)]).displace(-5),
        ];
        for a in &keys {
            for b in &keys {
                for by in [0, 3, 1000, -1000] {
                    assert_eq!(a.cmp_displaced(by, b), a.displace(by).cmp(b), "{a:?} +{by} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn displacement_preserves_order() {
        let keys = [
            Tuple::from([ent(1)]),
            Tuple::from([ent(1), Value::Int(3)]),
            Tuple::from([ent(1), ent(4)]),
            Tuple::from([ent(2), Value::text("a")]),
            Tuple::from([ent(2), Value::text("b")]),
            Tuple::from([Value::Int(0), ent(1)]),
        ];
        let mut sorted = keys.to_vec();
        sorted.sort();
        let moved: Vec<Tuple> = sorted.iter().map(|k| k.displace(500)).collect();
        let mut resorted = moved.clone();
        resorted.sort();
        assert_eq!(moved, resorted);
    }
}
