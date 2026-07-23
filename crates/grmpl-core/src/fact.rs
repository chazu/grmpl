//! Ground facts (DESIGN.md §2.3). A `Fact` is a ground tuple in a specific
//! relation — the unit that patches assert, retract, and precondition on.

use crate::value::{RelId, Tuple};

/// A ground tuple in a specific relation.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Fact {
    pub rel: RelId,
    pub tuple: Tuple,
}

impl Fact {
    pub fn new(rel: RelId, tuple: Tuple) -> Fact {
        Fact { rel, tuple }
    }
}
