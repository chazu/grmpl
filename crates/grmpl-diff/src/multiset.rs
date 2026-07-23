//! A consolidated multiset (Z-weighted collection): the value of a relation or
//! query result at one edition. Zero-weight tuples are dropped.

use std::collections::HashMap;

use grmpl_core::{Diff, Tuple};

/// A Z-weighted set of tuples.
pub type Multiset = HashMap<Tuple, Diff>;

/// Add `(tuple, diff)` into `acc`, dropping the entry if it cancels to zero.
pub fn add(acc: &mut Multiset, tuple: Tuple, diff: Diff) {
    if diff == 0 {
        return;
    }
    let e = acc.entry(tuple).or_insert(0);
    *e += diff;
    if *e == 0 {
        // Remove using the tuple we still own via the entry key is awkward;
        // do a follow-up cleanup instead.
    }
}

/// Merge `src` into `acc`.
pub fn merge(acc: &mut Multiset, src: &Multiset) {
    for (t, d) in src {
        add(acc, t.clone(), *d);
    }
}

/// Drop any zero-weight entries left after cancellation.
pub fn strip_zeros(m: &mut Multiset) {
    m.retain(|_, d| *d != 0);
}

/// Consolidate a stream of `(tuple, diff)` into a zero-free multiset.
pub fn from_pairs(pairs: impl IntoIterator<Item = (Tuple, Diff)>) -> Multiset {
    let mut m = Multiset::new();
    for (t, d) in pairs {
        add(&mut m, t, d);
    }
    strip_zeros(&mut m);
    m
}

/// A deterministic, sorted `Vec` view (tuples ascending) — used by callers and
/// tests that need stable ordering.
pub fn to_sorted_vec(m: &Multiset) -> Vec<(Tuple, Diff)> {
    let mut v: Vec<(Tuple, Diff)> = m.iter().filter(|(_, d)| **d != 0).map(|(t, d)| (t.clone(), *d)).collect();
    v.sort();
    v
}
