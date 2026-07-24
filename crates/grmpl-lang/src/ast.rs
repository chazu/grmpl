//! The surface AST (DESIGN.md §8 declaration forms). v1 covers `rel`, `view`,
//! and `form` — the declarative parts that lower directly to `Query`/`Pattern`.
//! `on` (which binds executable behavior) remains a programmatic construction.

/// A declared column of a `rel`: a name and an optional type annotation. The
/// type is kept as its surface spelling here (e.g. `"Ent"`); `compile` maps it
/// to a core `Ty`, defaulting an unannotated column to the permissive `Any`.
#[derive(Clone, PartialEq, Debug)]
pub struct ColDecl {
    pub name: String,
    pub ty: Option<String>,
}

/// An argument to a view atom: a variable, or a literal.
#[derive(Clone, PartialEq, Debug)]
pub enum Arg {
    Var(String),
    Str(String),
    Int(i64),
}

/// A body atom of a `view`: `rel(arg, arg, ...)`.
#[derive(Clone, PartialEq, Debug)]
pub struct Atom {
    pub rel: String,
    pub args: Vec<Arg>,
}

/// An aggregate function that may head a `yield` item: `count`, `sum`, `min`,
/// or `max`. `count` folds row *counts* (no column); the others fold a named
/// column. `compile` lowers this to a positional [`grmpl_diff::Agg`], so a
/// `view` carrying one of these lowers to a `Query::Reduce` (P2 aggregate
/// yield) rather than a plain projection.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
}

/// The single aggregate of a `view`'s `yield` clause: a function over an
/// optional column (`sum(points)`, `count()`). A view has at most one — the
/// engine's `Query::Reduce` folds one aggregate per group — and its grouping
/// columns are the *plain* identifiers of the same `yield` list.
#[derive(Clone, PartialEq, Debug)]
pub struct AggYield {
    pub func: AggFunc,
    pub col: Option<String>,
}

/// One element of a `form` rule pattern.
#[derive(Clone, PartialEq, Debug)]
pub enum PAtom {
    Lit(String),  // "take"
    Bind(String), // @name (written as a bare identifier)
}

/// A `form` rule: a pattern sequence and the value it constructs.
#[derive(Clone, PartialEq, Debug)]
pub struct FormRule {
    pub seq: Vec<PAtom>,
    pub tag: String,
    pub ctor_args: Vec<String>,
}

/// An argument to an action statement: a variable (incl. `self`) or a literal.
#[derive(Clone, PartialEq, Debug)]
pub enum SArg {
    Var(String),
    Str(String),
    Int(i64),
}

/// How a `resolve` matches a column: exact (`=`) or word-membership (`~`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MatchOp {
    Exact,
    Word,
}

/// A statement in an `on` handler arm.
#[derive(Clone, PartialEq, Debug)]
pub enum Stmt {
    /// Run a view, match a yielded column, bind all yielded columns as vars.
    Resolve { view: String, args: Vec<SArg>, col: String, op: MatchOp, rhs: SArg },
    /// Look a tuple up in a base relation; bind its unbound variable columns.
    Find { rel: String, args: Vec<SArg> },
    Expect { rel: String, args: Vec<SArg> },
    Assert { rel: String, args: Vec<SArg> },
    Retract { rel: String, args: Vec<SArg> },
    Emit { rel: String, args: Vec<SArg> },
}

/// One arm of an `on` handler: `match Tag(v..) { stmt* }`.
#[derive(Clone, PartialEq, Debug)]
pub struct Arm {
    pub tag: String,
    pub vars: Vec<String>,
    pub stmts: Vec<Stmt>,
}

/// A top-level declaration.
#[derive(Clone, PartialEq, Debug)]
pub enum Decl {
    Rel { name: String, cols: Vec<ColDecl> },
    /// A `view`. `yields` are the plain grouping/projection columns of the
    /// `yield` clause; `agg` is the optional single aggregate (`sum(pts)`,
    /// `count()`). With `agg == None` the view lowers to a projection (v1
    /// behavior); with `agg == Some(_)` it groups by `yields` and lowers to a
    /// `Query::Reduce` (P2 aggregate yield).
    View {
        name: String,
        params: Vec<String>,
        atoms: Vec<Atom>,
        yields: Vec<String>,
        agg: Option<AggYield>,
    },
    Form { name: String, rules: Vec<FormRule> },
    /// An `on` handler binding executable behavior to an inbox. Its arms may be
    /// written in either surface and freely mixed: `stmt_arms` are the v1
    /// statement bodies (`match Tag(v) { stmt* }`), `word_arms` are the
    /// concatenative point-free bodies (`match Tag(v) [ word* ]`). Both lower to
    /// the same effect plumbing (`compile.rs`).
    On {
        inbox: String,
        form: String,
        stmt_arms: Vec<Arm>,
        word_arms: Vec<crate::concat::ConcatArm>,
    },
}
