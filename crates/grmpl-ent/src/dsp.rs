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

    /// How this key's column `col` compares with the lone coordinate of a
    /// stored **pivot** moved up by `by`: the test a k-d split node makes
    /// (`tree.rs`, `Kind::Split`). A key that has no column `col` sorts below
    /// every pivot. As with [`cmp_displaced`](Self::cmp_displaced), the stored
    /// pivot moves up to the key, never the key down.
    ///
    /// A key type with one column (every scalar key) has only column `0`, and
    /// its pivot is a whole key.
    fn cmp_column(&self, col: usize, pivot: &Self, by: i64) -> Ordering {
        assert_eq!(col, 0, "a scalar key has one column");
        pivot.cmp_displaced(by, self).reverse()
    }

    /// **Runs** (Gold's `RegionLoaf`; fidelity gap G9). The stride that takes
    /// this key to `next` in one step, if `next` is the next row of some run
    /// starting here: a key greater than this one, differing only in columns
    /// a stride can step. `None` for a key type that never forms runs, which
    /// is the default.
    fn stride_to(&self, _next: &Self) -> Option<Self> {
        None
    }

    /// The key `i` steps along a run from this one (`i` may be negative).
    /// Only called with a stride [`stride_to`](Self::stride_to) produced.
    fn step(&self, _stride: &Self, _i: i64) -> Self {
        unreachable!("a key type without runs is never stepped")
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

    /// Entity and integer cells step, by an integer stride per column; every
    /// other cell stays fixed. The first column that steps steps up, so a
    /// run's rows ascend.
    fn stride_to(&self, next: &Self) -> Option<Self> {
        let (a, b) = (self.as_slice(), next.as_slice());
        if a.len() != b.len() || self >= next {
            return None;
        }
        let mut stride = Vec::with_capacity(a.len());
        for (x, y) in a.iter().zip(b) {
            let s = match (x, y) {
                _ if x == y => 0,
                (Value::Ent(x), Value::Ent(y)) => i64::try_from(y.0 as i128 - x.0 as i128).ok()?,
                (Value::Int(x), Value::Int(y)) => y.checked_sub(*x)?,
                _ => return None,
            };
            stride.push(Value::Int(s));
        }
        Some(Tuple::new(stride))
    }

    fn step(&self, stride: &Self, i: i64) -> Self {
        Tuple::new(
            self.as_slice()
                .iter()
                .zip(stride.as_slice())
                .map(|(cell, s)| match (cell, s) {
                    (_, Value::Int(0)) => cell.clone(),
                    (Value::Ent(e), Value::Int(s)) => Value::Ent(Entity(e.0.wrapping_add(s.wrapping_mul(i) as u64))),
                    (Value::Int(x), Value::Int(s)) => Value::Int(x.wrapping_add(s.wrapping_mul(i))),
                    _ => cell.clone(),
                })
                .collect::<Vec<_>>(),
        )
    }

    /// A pivot is a one-column tuple; a tuple too short to have `col` sorts
    /// below it.
    fn cmp_column(&self, col: usize, pivot: &Self, by: i64) -> Ordering {
        let (Some(x), Some(p)) = (self.as_slice().get(col), pivot.as_slice().first()) else {
            return Ordering::Less;
        };
        match (x, p) {
            (Value::Ent(x), Value::Ent(p)) => x.0.cmp(&p.0.wrapping_add(by as u64)),
            _ => x.cmp(p),
        }
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
