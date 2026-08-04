//! # Concatenative surface — the point-free patch/effect-plumbing seam (P11)
//!
//! `idea.md` §7/§9 sketch a MOO handler written *concatenatively*: instead of
//! the v1 statement body (`resolve … / find … / retract … / assert … / emit …`,
//! each naming its logic variables), the arm is a **point-free word sequence**
//! that threads values through an implicit stack. The two surfaces are just two
//! spellings of the same effect plumbing over the P7 CBPV core — they **coexist**
//! (an `on` handler may mix statement arms and word arms) and lower to the *same*
//! primitives (view-instantiate + least-tuple select + `Patch` build), so a
//! trace written either way produces byte-identical editions.
//!
//! What stays named and what goes point-free follows the bright line the ticket
//! draws: **view bodies keep named logic variables** (a `view`/`form` is still
//! the relational/structural surface of §8), while the **handler words** — the
//! effect seam — carry [stack effects](StackEffect), not variable names.
//!
//! ## Words carry declared stack effects first
//!
//! Every [`Word`] declares how many stack cells it consumes and produces
//! ([`Word::effect`]). [`check`] walks an arm composing those declared effects
//! over an abstract stack *depth*, catching underflow and a non-empty leftover
//! stack before the handler ever runs. Behavior compilation then symbolically
//! executes those words over typed expressions, rejects mismatched scalar and
//! relation operands, and emits canonical [`crate::BehaviorIr`].
//!
//! ## The reified word IR is the CBPV thunk layer
//!
//! A [`ConcatArm`]'s `words` are inspectable data (an enum, not an `Fn`), exactly
//! like [`crate::ir`]'s reified value/computation layer. A word list is a
//! *suspended computation held as a value* — the CBPV thunk — until
//! [`crate::Program::behavior`] runs it against a snapshot and message.

use grmpl_core::Value;

use crate::ast::MatchOp;

/// One concatenative word: a point-free operation over the handler's value
/// stack. Words fall into three families — stack shufflers (pure plumbing),
/// value pushers (`self` / literals), and the effect seam (`resolve` / `find`
/// read the world and bind; `expect` / `retract` / `assert` / `emit` build the
/// patch). The seam words name a relation/view but **no logic variables**: their
/// operands come from the stack, in column order (deepest cell = column 0).
#[derive(Clone, PartialEq, Debug)]
pub enum Word {
    /// Push the handler's own process entity. `( -- ent )`
    SelfEntity,
    /// Push a literal value. `( -- v )`
    Lit(Value),

    /// `( a -- a a )`
    Dup,
    /// `( a -- )`
    Drop,
    /// `( a b -- b a )`
    Swap,
    /// `( a b -- a b a )`
    Over,
    /// `( a b c -- b c a )`
    Rot,
    /// `( a b -- b )`
    Nip,
    /// `( a b -- b a b )`
    Tuck,
    /// `( a b -- a b a b )`
    TwoDup,
    /// `( a b -- )`
    TwoDrop,

    /// Closed scalar intrinsic registry. Binary words consume `a b` and push
    /// one result; unary words consume `a`. The compiler checks concrete types
    /// and lowers these words to the same [`crate::BehaviorIr`] expressions as
    /// the named surface.
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Min,
    Max,
    ToFloat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Not,
    And,
    Or,

    /// Resolve a noun against a `view`. Pops the match key (top) and the view's
    /// parameters beneath it (deepest = first parameter), instantiates the view,
    /// selects the **least** row whose column `col` matches the key under `op`
    /// (exactly the v1 `resolve` rule), and pushes the view's yielded columns in
    /// yield order. A miss aborts the arm with no change.
    /// `( param.. key -- yield.. )`
    Resolve {
        view: String,
        col: String,
        op: MatchOp,
    },
    /// Look a tuple up in a base relation. Pops `keyn` key values (the relation's
    /// first `keyn` columns, deepest = column 0), selects the **least** matching
    /// row, and pushes that row's *full* column list. A miss aborts the arm.
    /// `( k{keyn} -- c{arity} )`
    Find {
        rel: String,
        keyn: usize,
    },

    /// Pop the relation's `arity` columns (deepest = column 0) into a
    /// precondition fact. `( c{arity} -- )`
    Expect(String),
    /// Pop the relation's `arity` columns into a world assertion. `( c{arity} -- )`
    Assert(String),
    /// Pop the relation's `arity` columns into a world retraction. `( c{arity} -- )`
    Retract(String),
    /// Pop the relation's `arity` columns into an emitted message whose inbox is
    /// the named relation. `( c{arity} -- )`
    Emit(String),
}

/// The declared stack effect of a word: cells consumed and cells produced.
///
/// A word's effect is *data* — for shufflers and pushers it is fixed, and for
/// the seam words it is derived from the schema the word names (a `view`'s
/// parameter/yield counts, a relation's arity). [`check`] composes these over an
/// abstract depth; it never inspects a value, so the arm is validated without a
/// snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StackEffect {
    /// Cells popped from the stack.
    pub pops: usize,
    /// Cells pushed onto the stack.
    pub pushes: usize,
}

impl StackEffect {
    fn new(pops: usize, pushes: usize) -> StackEffect {
        StackEffect { pops, pushes }
    }
}

/// Resolves the schema facts a word's effect depends on: a view's parameter and
/// yield counts, and a relation's arity. Implemented by [`crate::Program`] so
/// [`Word::effect`] and [`check`] can run at compile time. Returns `None` when a
/// name is undeclared, which surfaces as a check error.
pub trait Schemas {
    /// The number of parameters and yielded columns of `view`.
    fn view_shape(&self, view: &str) -> Option<(usize, usize)>;
    /// The arity (column count) of base relation `rel`.
    fn rel_arity(&self, rel: &str) -> Option<usize>;
}

impl Word {
    /// This word's declared [`StackEffect`], given the schemas it names. `Err`
    /// names the undeclared view/relation.
    pub fn effect(&self, schemas: &dyn Schemas) -> Result<StackEffect, String> {
        Ok(match self {
            Word::SelfEntity | Word::Lit(_) => StackEffect::new(0, 1),
            Word::Dup => StackEffect::new(1, 2),
            Word::Drop => StackEffect::new(1, 0),
            Word::Swap => StackEffect::new(2, 2),
            Word::Over => StackEffect::new(2, 3),
            Word::Rot => StackEffect::new(3, 3),
            Word::Nip => StackEffect::new(2, 1),
            Word::Tuck => StackEffect::new(2, 3),
            Word::TwoDup => StackEffect::new(2, 4),
            Word::TwoDrop => StackEffect::new(2, 0),
            Word::Neg | Word::ToFloat | Word::Not => StackEffect::new(1, 1),
            Word::Add
            | Word::Sub
            | Word::Mul
            | Word::Div
            | Word::Rem
            | Word::Min
            | Word::Max
            | Word::Eq
            | Word::Ne
            | Word::Lt
            | Word::Le
            | Word::Gt
            | Word::Ge
            | Word::And
            | Word::Or => StackEffect::new(2, 1),
            Word::Resolve { view, .. } => {
                let (params, yields) = schemas
                    .view_shape(view)
                    .ok_or_else(|| format!("word `resolve` names undeclared view `{view}`"))?;
                // params + the match key in, the yielded columns out.
                StackEffect::new(params + 1, yields)
            }
            Word::Find { rel, keyn } => {
                let arity = rel_arity(schemas, rel)?;
                StackEffect::new(*keyn, arity)
            }
            Word::Expect(rel) | Word::Assert(rel) | Word::Retract(rel) | Word::Emit(rel) => {
                StackEffect::new(rel_arity(schemas, rel)?, 0)
            }
        })
    }
}

fn rel_arity(schemas: &dyn Schemas, rel: &str) -> Result<usize, String> {
    schemas
        .rel_arity(rel)
        .ok_or_else(|| format!("word names undeclared relation `{rel}`"))
}

/// One arm of a concatenative `on` handler: a constructor tag, the constructor
/// arguments it binds (pushed onto the stack, in order, when the arm fires), and
/// the point-free word body.
///
/// `vars` names the constructor slots for arity matching and readability; the
/// body is point-free, so the names are not referenced by the words.
#[derive(Clone, PartialEq, Debug)]
pub struct ConcatArm {
    /// The constructor tag this arm handles (e.g. `Take`).
    pub tag: String,
    /// The constructor arguments, pushed as the arm's initial stack.
    pub vars: Vec<String>,
    /// The point-free word body.
    pub words: Vec<Word>,
}

impl ConcatArm {
    /// Statically check the arm's declared stack effects: starting from a stack
    /// holding the constructor arguments, apply each word's [`StackEffect`],
    /// rejecting an underflow (a word popping more than is present) and a body
    /// that leaves the stack non-empty (a handler must fully consume its inputs
    /// into the patch — its declared overall effect is `( args.. -- )`).
    ///
    /// This is the "declared stack effects first" gate: cell *counts* compose;
    /// cell *types* are a later inference upgrade.
    pub fn check(&self, schemas: &dyn Schemas) -> Result<(), String> {
        let mut depth = self.vars.len();
        for (i, word) in self.words.iter().enumerate() {
            let eff = word.effect(schemas)?;
            if depth < eff.pops {
                return Err(format!(
                    "arm `{}`: stack underflow at word {i} ({word:?}): needs {} cell(s), \
                     stack has {depth}",
                    self.tag, eff.pops
                ));
            }
            depth = depth - eff.pops + eff.pushes;
        }
        if depth != 0 {
            return Err(format!(
                "arm `{}`: body leaves {depth} value(s) on the stack; a handler must \
                 consume its stack into the patch (declared effect `( … -- )`)",
                self.tag
            ));
        }
        Ok(())
    }
}
