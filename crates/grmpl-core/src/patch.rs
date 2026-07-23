//! `Patch` — a guarded proposal for the next edition (DESIGN.md §2.3 #4).
//!
//! A patch is a *value*: preconditions that must hold in the edition it commits
//! against, tuples to assert/retract, messages to emit, and an optional inbox
//! cursor advance (the actor exactly-once bookkeeping — DESIGN.md §5.3).
//! Committing it is a computation (see `grmpl-proc`).

use crate::fact::Fact;
use crate::value::{RelId, Tuple};

/// An outbound message: a body appended to a target inbox relation. In v1 the
/// target is named by its inbox `RelId` directly (single authority domain); in
/// the distributed future it is routed to a `DomainId` below the line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Message {
    pub inbox: RelId,
    pub body: Tuple,
}

/// An inbox cursor advance, realized as a retract-then-assert in a cursor
/// relation so it lands atomically in the same commit as the effects it guards
/// (DESIGN.md §5.3). This is the concrete form of the design's `cursor_advance`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CursorMove {
    pub rel: RelId,
    /// The prior cursor tuple to retract (None on the very first advance).
    pub retract: Option<Tuple>,
    /// The new cursor tuple to assert.
    pub assert: Tuple,
}

/// The semantic center: a candidate next edition.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Patch {
    pub preconditions: Vec<Fact>,
    pub asserts: Vec<Fact>,
    pub retracts: Vec<Fact>,
    pub emits: Vec<Message>,
    pub cursor_advance: Option<CursorMove>,
}

impl Patch {
    pub fn new() -> Patch {
        Patch::default()
    }

    /// Require `fact` to hold in the edition this patch commits against.
    pub fn expect(mut self, fact: Fact) -> Patch {
        self.preconditions.push(fact);
        self
    }
    pub fn assert(mut self, fact: Fact) -> Patch {
        self.asserts.push(fact);
        self
    }
    pub fn retract(mut self, fact: Fact) -> Patch {
        self.retracts.push(fact);
        self
    }
    pub fn emit(mut self, msg: Message) -> Patch {
        self.emits.push(msg);
        self
    }
    pub fn advance_cursor(mut self, mv: CursorMove) -> Patch {
        self.cursor_advance = Some(mv);
        self
    }
}
