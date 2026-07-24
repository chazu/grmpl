//! # grmpl-type — value & row types over the core IR (P8a)
//!
//! P8a gives the P7 core [`QueryIr`] a **static row type**, derived from the P1
//! relation [`Schema`]s (`grmpl-core::schema`). A *value type* is a
//! [`Ty`] (`Ent`/`Int`/`Text`/`Bool`/`Tuple`/`Any`); a **row type** ([`RowTy`])
//! is the ordered list of column value types a plan's rows carry — exactly the
//! `Ty` half of a [`Schema`], but synthesized *through* the dataflow operators
//! rather than declared.
//!
//! [`check_query`] walks a [`QueryIr`] bottom-up, looking each base relation's
//! schema up in a [`SchemaCatalog`] as-of an [`Edition`], and *synthesizes* the
//! row type of every operator:
//!
//! | operator      | row type out                                        |
//! |---------------|-----------------------------------------------------|
//! | `Rel(r)`      | the column types of `r`'s schema as-of the edition  |
//! | `Map`         | the type of each output [`RowExpr`]                 |
//! | `Filter`      | input unchanged (predicate is checked)              |
//! | `Project`     | the selected columns' types                         |
//! | `Join`        | left columns `++` right columns                     |
//! | `Union`       | column-wise least-upper-bound of the two inputs     |
//! | `Negate`      | input unchanged                                     |
//! | `Distinct`    | input unchanged                                     |
//! | `Reduce`      | key columns `++` `[aggregate type]`                 |
//! | `Iterate`     | LUB of `init` and `step` (with `Recur` bound)       |
//!
//! Along the way it rejects the type errors this layer exists to catch: a column
//! index out of range, an equality or join-key comparison between incompatible
//! concrete types (which can never hold / never match), a `Union` or `Iterate`
//! of mismatched arity, and a `Sum` over a non-numeric column. [`Ty::Any`] is
//! the lattice top: it is *compatible* with every type and it is the LUB widening
//! target, so untyped columns never cause a spurious error.
//!
//! ## Schemas are the source of types
//!
//! Typing is **relative to the schema registry**. A relation with no schema
//! as-of the edition is an [`TypeError::UnschemaedRelation`] — you get static
//! types for a plan by giving its relations schemas (P1). This mirrors the
//! commit-boundary rule (schemas are opt-in) but inverts the default: to *type*
//! a plan, its relations must be declared.
//!
//! ## Bright line
//!
//! This crate is **above the line** (`DESIGN.md` §1): it depends only on the
//! core value/schema types, the reified IR, and the aggregate tag — no storage,
//! no network. It reads schemas through the [`SchemaCatalog`] *trait*, never a
//! concrete registry.

use std::fmt;

use grmpl_core::{Edition, RelId, Schema, SchemaCatalog, Ty, Value};
use grmpl_diff::Agg;
use grmpl_lang::{Comp, PredExpr, QueryIr, RowExpr};

/// A **row type**: the ordered value types of a relation's columns. It is the
/// `Ty` projection of a [`Schema`] — the same layout, without the column names.
/// Column `i` types tuple position `i`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RowTy(Vec<Ty>);

impl RowTy {
    /// A row type from an explicit column-type list.
    pub fn new(cols: Vec<Ty>) -> RowTy {
        RowTy(cols)
    }

    /// The row type of a relation with this schema — its columns' types in order.
    pub fn from_schema(schema: &Schema) -> RowTy {
        RowTy(schema.columns.iter().map(|c| c.ty).collect())
    }

    /// The number of columns — the row's arity.
    pub fn arity(&self) -> usize {
        self.0.len()
    }

    /// The type of column `i`, or `None` if out of range.
    pub fn at(&self, i: usize) -> Option<Ty> {
        self.0.get(i).copied()
    }

    /// The column types in order.
    pub fn cols(&self) -> &[Ty] {
        &self.0
    }
}

/// The static value type inhabited by a concrete [`Value`] — the inverse of
/// [`Ty::admits`] for the ground cases (`Ty::Any` is never *synthesized*, only
/// read from a schema, so it does not appear here).
pub fn value_ty(v: &Value) -> Ty {
    match v {
        Value::Ent(_) => Ty::Ent,
        Value::Int(_) => Ty::Int,
        Value::Text(_) => Ty::Text,
        Value::Bool(_) => Ty::Bool,
        Value::Tuple(_) => Ty::Tuple,
    }
}

/// Can values of these two types ever be equal / match as a join key? Two
/// concrete types compare only when identical; [`Ty::Any`] (the top type) is
/// compatible with everything. Comparing two *distinct concrete* types can never
/// hold, so it is a type error rather than a filter that silently drops all rows.
fn compatible(a: Ty, b: Ty) -> bool {
    a == b || a == Ty::Any || b == Ty::Any
}

/// The least upper bound of two column types in the `{concrete…} < Any` lattice:
/// equal types stay themselves, anything else widens to [`Ty::Any`]. Used where a
/// column can hold rows of two provenances (`Union`, the `Iterate` fixpoint).
fn lub(a: Ty, b: Ty) -> Ty {
    if a == b {
        a
    } else {
        Ty::Any
    }
}

/// A static typing failure in a [`QueryIr`] plan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeError {
    /// A base relation has no schema as-of the checking edition, so its rows
    /// have no known type. Register a schema (P1) to type plans over it.
    UnschemaedRelation(RelId),
    /// A column reference (in a `Map`/`Project`/`Filter`/`Join`/`Reduce`) points
    /// past the end of its input row.
    ColumnOutOfRange { index: usize, arity: usize },
    /// An equality filter compares two distinct concrete types — it can never
    /// hold. (`Ty::Any` on either side is fine.)
    Incomparable { left: Ty, right: Ty },
    /// A `Join`'s two key lists have different lengths.
    KeyArityMismatch { left: usize, right: usize },
    /// A `Join` pairs two key columns of incompatible concrete types — they can
    /// never match, so the join is empty by construction.
    JoinKeyMismatch { pos: usize, left: Ty, right: Ty },
    /// A `Union` combines two inputs of different arity.
    UnionArityMismatch { left: usize, right: usize },
    /// An `Iterate`'s `step` produces a different arity than its `init` seed, so
    /// the fixpoint union is ill-formed.
    IterateArityMismatch { init: usize, step: usize },
    /// A `Reduce`'s `Sum` folds a column that is neither `Int` nor `Any` — it
    /// would sum to zero. (`Count` needs no column; `Min`/`Max` accept any type.)
    SumNonNumeric { col: usize, ty: Ty },
    /// A `Recur` appears outside any enclosing `Iterate`, so it has no type.
    RecurOutsideIterate,
    /// The schema registry itself failed the lookup (propagated verbatim).
    Registry(String),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::UnschemaedRelation(r) => {
                write!(f, "relation {} has no schema at this edition", r.0)
            }
            TypeError::ColumnOutOfRange { index, arity } => {
                write!(f, "column {index} out of range for a row of arity {arity}")
            }
            TypeError::Incomparable { left, right } => write!(
                f,
                "equality compares incompatible types {} and {} (never equal)",
                left.name(),
                right.name()
            ),
            TypeError::KeyArityMismatch { left, right } => {
                write!(
                    f,
                    "join key arity mismatch: {left} left column(s) vs {right} right"
                )
            }
            TypeError::JoinKeyMismatch { pos, left, right } => write!(
                f,
                "join key {pos} pairs incompatible types {} and {} (never matches)",
                left.name(),
                right.name()
            ),
            TypeError::UnionArityMismatch { left, right } => {
                write!(f, "union arity mismatch: {left} vs {right} columns")
            }
            TypeError::IterateArityMismatch { init, step } => {
                write!(
                    f,
                    "iterate step arity {step} differs from init arity {init}"
                )
            }
            TypeError::SumNonNumeric { col, ty } => {
                write!(
                    f,
                    "sum over column {col} of type {} (expected Int)",
                    ty.name()
                )
            }
            TypeError::RecurOutsideIterate => write!(f, "recur used outside an iterate"),
            TypeError::Registry(msg) => write!(f, "schema registry error: {msg}"),
        }
    }
}

impl std::error::Error for TypeError {}

/// Synthesize the row type of a [`QueryIr`] plan against the schema registry
/// as-of `at`, or report the first type error found.
///
/// This is the P8a entry point: it succeeds with the plan's [`RowTy`] iff every
/// column reference is in range, every comparison and join key pairs compatible
/// types, every `Union`/`Iterate` combines equal arities, and every base relation
/// has a schema as-of `at`.
pub fn check_query(
    q: &QueryIr,
    schemas: &dyn SchemaCatalog,
    at: Edition,
) -> Result<RowTy, TypeError> {
    synth(q, schemas, at, None)
}

/// Type a reified [`Comp`]: [`Comp::Find`]/[`Comp::Watch`] carry a relational
/// plan and yield its [`RowTy`]; [`Comp::Parse`] produces a value stream, not a
/// relation, so it is not row-typed here (its pattern typing is P9) and returns
/// `Ok(None)`.
pub fn check_comp(
    c: &Comp,
    schemas: &dyn SchemaCatalog,
    at: Edition,
) -> Result<Option<RowTy>, TypeError> {
    match c {
        Comp::Find(q) | Comp::Watch(q) => Ok(Some(check_query(q, schemas, at)?)),
        Comp::Parse(_) => Ok(None),
    }
}

/// The type of a scalar [`RowExpr`] against a known input row type.
fn expr_ty(e: &RowExpr, input: &RowTy) -> Result<Ty, TypeError> {
    match e {
        RowExpr::Col(i) => col(input, *i),
        RowExpr::Lit(v) => Ok(value_ty(v)),
    }
}

/// The type of column `i`, or a range error naming the row's arity.
fn col(row: &RowTy, i: usize) -> Result<Ty, TypeError> {
    row.at(i).ok_or(TypeError::ColumnOutOfRange {
        index: i,
        arity: row.arity(),
    })
}

/// Check a reified predicate against an input row type. Structure only — a
/// well-typed predicate leaves the row type unchanged (`Filter` is type-neutral).
fn check_pred(p: &PredExpr, input: &RowTy) -> Result<(), TypeError> {
    match p {
        PredExpr::Eq(a, b) => {
            let ta = expr_ty(a, input)?;
            let tb = expr_ty(b, input)?;
            if compatible(ta, tb) {
                Ok(())
            } else {
                Err(TypeError::Incomparable {
                    left: ta,
                    right: tb,
                })
            }
        }
        PredExpr::And(ps) => {
            for p in ps {
                check_pred(p, input)?;
            }
            Ok(())
        }
    }
}

/// The type of a [`Reduce`]'s appended aggregate column.
fn agg_ty(agg: &Agg, input: &RowTy) -> Result<Ty, TypeError> {
    match *agg {
        // A count is always an Int, independent of the input columns.
        Agg::Count => Ok(Ty::Int),
        // Sum folds an i64 column; a non-numeric concrete column always sums to 0
        // (`fold_agg` treats non-`Int` cells as 0), which is a typing bug.
        Agg::Sum(c) => {
            let t = col(input, c)?;
            if t == Ty::Int || t == Ty::Any {
                Ok(Ty::Int)
            } else {
                Err(TypeError::SumNonNumeric { col: c, ty: t })
            }
        }
        // Min/Max of a column of type T is a T (any type has a total order).
        Agg::Min(c) | Agg::Max(c) => col(input, c),
    }
}

/// Column-wise least-upper-bound of two row types of equal arity — the row type
/// of their multiset union / an `Iterate` fixpoint.
fn union_ty(a: &RowTy, b: &RowTy) -> Result<RowTy, TypeError> {
    if a.arity() != b.arity() {
        return Err(TypeError::UnionArityMismatch {
            left: a.arity(),
            right: b.arity(),
        });
    }
    Ok(RowTy(
        a.0.iter().zip(&b.0).map(|(&x, &y)| lub(x, y)).collect(),
    ))
}

/// Bottom-up row-type synthesis. `recur` is the row type bound to
/// [`QueryIr::Recur`] by the nearest enclosing [`QueryIr::Iterate`], if any.
fn synth(
    q: &QueryIr,
    schemas: &dyn SchemaCatalog,
    at: Edition,
    recur: Option<&RowTy>,
) -> Result<RowTy, TypeError> {
    match q {
        QueryIr::Rel(r) => {
            let schema = schemas
                .schema_at(*r, at)
                .map_err(|e| TypeError::Registry(e.to_string()))?
                .ok_or(TypeError::UnschemaedRelation(*r))?;
            Ok(RowTy::from_schema(&schema))
        }
        QueryIr::Map { input, map } => {
            let ti = synth(input, schemas, at, recur)?;
            let mut cols = Vec::with_capacity(map.out.len());
            for e in &map.out {
                cols.push(expr_ty(e, &ti)?);
            }
            Ok(RowTy(cols))
        }
        QueryIr::Filter { input, pred } => {
            let ti = synth(input, schemas, at, recur)?;
            check_pred(pred, &ti)?;
            Ok(ti)
        }
        QueryIr::Project { input, cols } => {
            let ti = synth(input, schemas, at, recur)?;
            let mut out = Vec::with_capacity(cols.len());
            for &i in cols {
                out.push(col(&ti, i)?);
            }
            Ok(RowTy(out))
        }
        QueryIr::Join {
            left,
            right,
            left_key,
            right_key,
        } => {
            let tl = synth(left, schemas, at, recur)?;
            let tr = synth(right, schemas, at, recur)?;
            if left_key.len() != right_key.len() {
                return Err(TypeError::KeyArityMismatch {
                    left: left_key.len(),
                    right: right_key.len(),
                });
            }
            for (pos, (&li, &ri)) in left_key.iter().zip(right_key).enumerate() {
                let lt = col(&tl, li)?;
                let rt = col(&tr, ri)?;
                if !compatible(lt, rt) {
                    return Err(TypeError::JoinKeyMismatch {
                        pos,
                        left: lt,
                        right: rt,
                    });
                }
            }
            // Join concatenates the matched rows (see `grmpl_diff::query::concat`).
            let mut cols = tl.0;
            cols.extend_from_slice(&tr.0);
            Ok(RowTy(cols))
        }
        QueryIr::Union(a, b) => {
            let ta = synth(a, schemas, at, recur)?;
            let tb = synth(b, schemas, at, recur)?;
            union_ty(&ta, &tb)
        }
        QueryIr::Negate(q) => synth(q, schemas, at, recur),
        QueryIr::Distinct(q) => synth(q, schemas, at, recur),
        QueryIr::Reduce { input, key, agg } => {
            let ti = synth(input, schemas, at, recur)?;
            let mut cols = Vec::with_capacity(key.len() + 1);
            for &i in key {
                cols.push(col(&ti, i)?);
            }
            cols.push(agg_ty(agg, &ti)?);
            Ok(RowTy(cols))
        }
        QueryIr::Recur => recur.cloned().ok_or(TypeError::RecurOutsideIterate),
        QueryIr::Iterate { init, step } => {
            // The fixpoint is `distinct(init ∪ step(Recur))`. Seed `Recur` with
            // the init row type, type the step under it, then take the union type.
            // v1 iterate is single-level and monotone, so one pass is sound: the
            // step produces same-arity rows and the LUB is the fixpoint type.
            let t_init = synth(init, schemas, at, recur)?;
            let t_step = synth(step, schemas, at, Some(&t_init))?;
            if t_init.arity() != t_step.arity() {
                return Err(TypeError::IterateArityMismatch {
                    init: t_init.arity(),
                    step: t_step.arity(),
                });
            }
            union_ty(&t_init, &t_step)
        }
    }
}

#[cfg(test)]
mod tests;
