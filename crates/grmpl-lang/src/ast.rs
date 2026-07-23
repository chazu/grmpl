//! The surface AST (DESIGN.md §8 declaration forms). v1 covers `rel`, `view`,
//! and `form` — the declarative parts that lower directly to `Query`/`Pattern`.
//! `on` (which binds executable behavior) remains a programmatic construction.

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
    Rel { name: String, cols: Vec<String> },
    View { name: String, params: Vec<String>, atoms: Vec<Atom>, yields: Vec<String> },
    Form { name: String, rules: Vec<FormRule> },
    On { inbox: String, form: String, arms: Vec<Arm> },
}
