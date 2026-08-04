//! # Behaviors as relations — live code (P12)
//!
//! The defining MOO capability: a *behavior* is not privileged VM state, it is
//! ordinary data — a [`Value::Code`] cell in a relation. This module makes that
//! concrete over the canonical typed [`BehaviorIr`]:
//!
//! * A [`StoredBehavior`] is a message-pattern guard plus the same typed
//!   [`BehaviorIr`] executed by inline named and concatenative handlers.
//!
//! * [`encode_behavior`] / [`decode_behavior`] serialize that IR to bytes, which
//!   [`StoredBehavior::to_value`] wraps as [`Value::Code`]. The framing rides the
//!   one shared [`grmpl_core::wire::FORMAT_VERSION`] byte and reuses
//!   `wire::encode_value` for literals inside it — there is still exactly one
//!   value encoding. The IR tag set here is a *third* namespace under that
//!   version byte (beside value and schema tags). v4 replaces serialized word
//!   tags with stable symbolic intrinsic/capability envelopes.
//!
//! * **Dispatch** is a query, not a jump table. [`implements_ir`] is the
//!   recursive `implements(entity, behavior)` view from `idea.md` §3 —
//!   `direct_behavior ∪ (prototype ⋈ implements)` — as a [`QueryIr::Iterate`].
//!   [`select_behavior`] materializes it at an edition, keeps the behaviors the
//!   entity implements whose guard matches the message, and picks the **least**
//!   one deterministically (results are tuple-sorted). [`dispatch`] runs it.
//!
//! * **Live redefinition** needs no new mechanism: asserting/retracting a
//!   `direct_behavior` (or `prototype`) row is an ordinary [`Patch`], and the
//!   very next dispatch reads the new `implements` set. The behavior in force is
//!   a pure function of the world at that edition — the live-code law.
//!
//! The commit-boundary re-check of a stored behavior's effects against authority
//! (`grmpl_core::BehaviorChecker`) lives in `grmpl-type`, which owns the P8b
//! effect checker; this module only defines and runs the code.

use grmpl_core::wire;
use grmpl_core::{Entity, Error, Patch, RelId, Result, Tuple, Ty, Value};
use grmpl_diff::Snapshot;

use crate::ast::MatchOp;
use crate::behavior_ir::{BehaviorIr, BehaviorOp, BoolExpr, CompareOp, ExprIr, FindArg, ValueExpr};
use crate::compile::Program;
use crate::concat::Word;
use crate::ir::{PredExpr, RowExpr};

/// A behavior stored as data (P12): a message-pattern guard and a point-free
/// body. Round-trips through [`Value::Code`] via [`to_value`](Self::to_value) /
/// [`from_value`](Self::from_value); runs `snapshot × message ⇒ patch` via
/// [`run`](Self::run).
#[derive(Clone, PartialEq, Debug)]
pub struct StoredBehavior {
    /// The message-pattern guard: which messages this behavior handles. The
    /// reified P7 [`PredExpr`] language (only `Eq`/`And`), tested against the
    /// incoming message tuple. An empty `And` matches every message.
    pub guard: PredExpr,
    /// Local names receiving the incoming message columns, in column order.
    pub parameters: Vec<String>,
    /// The canonical typed executable plan. Stored and inline behaviors execute
    /// this same representation.
    pub body: BehaviorIr,
}

impl StoredBehavior {
    /// Build a stored behavior.
    pub fn new(guard: PredExpr, parameters: Vec<String>, body: BehaviorIr) -> StoredBehavior {
        StoredBehavior {
            guard,
            parameters,
            body,
        }
    }

    /// Type-check a legacy point-free body and immediately lower it to the
    /// canonical IR. The serialized representation contains no words.
    pub fn from_words(
        prog: &Program,
        guard: PredExpr,
        parameter_types: Vec<Ty>,
        words: Vec<Word>,
    ) -> std::result::Result<StoredBehavior, String> {
        let parameters: Vec<String> = (0..parameter_types.len())
            .map(|index| format!("#message{index}"))
            .collect();
        let body = prog.lower_words(
            &words,
            parameters.iter().cloned().zip(parameter_types).collect(),
        )?;
        Ok(StoredBehavior::new(guard, parameters, body))
    }

    /// Does this behavior's guard accept `message`? Safe against arity
    /// mismatches: a guard that references a column the message lacks does not
    /// match (rather than panicking), so dispatch over arbitrary messages is
    /// total.
    pub fn matches(&self, message: &Tuple) -> bool {
        pred_max_col(&self.guard)
            .map(|c| c < message.arity())
            .unwrap_or(true)
            && self.guard.test(message)
    }

    /// Run the body against a snapshot and message, seeding the value stack with
    /// the message's columns. `self_entity` is the receiver (`self` inside the
    /// body). `Ok(None)` when a `resolve`/`find` in the body matched nothing.
    /// Does **not** re-test the guard — dispatch already selected on it.
    pub fn run(
        &self,
        prog: &Program,
        self_entity: Entity,
        snap: &Snapshot,
        message: &Tuple,
    ) -> Result<Option<Patch>> {
        if message.arity() != self.parameters.len() {
            return Err(Error::Behavior(format!(
                "stored behavior expects {} message columns, got {}",
                self.parameters.len(),
                message.arity()
            )));
        }
        self.body.execute(
            prog,
            self_entity,
            snap,
            self.parameters
                .iter()
                .cloned()
                .zip(message.as_slice().iter().cloned()),
        )
    }

    /// Wrap this behavior as a code-carrying value (P12), ready to assert into a
    /// relation.
    pub fn to_value(&self) -> Value {
        Value::Code(std::sync::Arc::from(encode_behavior(self)))
    }

    /// Recover a behavior from a [`Value::Code`] cell. `Err` on a non-code value
    /// or malformed bytes.
    pub fn from_value(v: &Value) -> Result<StoredBehavior> {
        match v {
            Value::Code(bytes) => decode_behavior(bytes),
            other => Err(Error::Codec(format!(
                "expected Value::Code, found {other:?}"
            ))),
        }
    }
}

// ---- dispatch: the `implements` view ------------------------------------

/// The recursive `implements(entity, behavior)` view (`idea.md` §3) as a plan:
/// `direct_behavior ∪ { (e, b) : prototype(e, parent) ∧ implements(parent, b) }`,
/// the least fixpoint DESIGN.md §3.1 names as the canonical recursive view. Both
/// relations are arity-2 — `direct_behavior(entity, behavior)` and
/// `prototype(entity, parent)`.
pub fn implements_ir(direct_behavior: RelId, prototype: RelId) -> crate::ir::QueryIr {
    use crate::ir::QueryIr;
    // step: prototype(e, parent) ⋈[parent = recur.entity] implements(parent, b)
    // join output columns = [proto.entity, proto.parent, recur.entity, recur.behavior];
    // keep (proto.entity, recur.behavior).
    let step = QueryIr::Project {
        input: Box::new(QueryIr::Join {
            left: Box::new(QueryIr::Rel(prototype)),
            right: Box::new(QueryIr::Recur),
            left_key: vec![1],
            right_key: vec![0],
        }),
        cols: vec![0, 3],
    };
    QueryIr::Iterate {
        init: Box::new(QueryIr::Rel(direct_behavior)),
        step: Box::new(step),
    }
}

/// The behaviors `entity` implements at this snapshot, decoded, in the
/// deterministic (tuple-sorted) order the `implements` view yields them —
/// i.e. ascending by their stored `Value::Code` bytes. A row whose behavior
/// column is not a `Value::Code` is skipped (the column may be untyped).
pub fn implemented_behaviors(
    direct_behavior: RelId,
    prototype: RelId,
    entity: Entity,
    snap: &Snapshot,
) -> Result<Vec<StoredBehavior>> {
    let q = implements_ir(direct_behavior, prototype).lower();
    let rows = q.find(snap)?;
    let mut out = Vec::new();
    for (t, d) in rows {
        if d <= 0 {
            continue;
        }
        let cols = t.as_slice();
        if cols.first() == Some(&Value::Ent(entity)) {
            if let Some(v @ Value::Code(_)) = cols.get(1) {
                out.push(StoredBehavior::from_value(v)?);
            }
        }
    }
    Ok(out)
}

/// The behavior `entity` dispatches for `message`: the **least** behavior it
/// implements (by stored bytes) whose guard matches. `Ok(None)` if none match.
/// Deterministic and world-determined — the live-code law's observable.
pub fn select_behavior(
    direct_behavior: RelId,
    prototype: RelId,
    entity: Entity,
    snap: &Snapshot,
    message: &Tuple,
) -> Result<Option<StoredBehavior>> {
    for b in implemented_behaviors(direct_behavior, prototype, entity, snap)? {
        if b.matches(message) {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// Dispatch `message` to `entity`: select the behavior (see [`select_behavior`])
/// and run it, with `self` bound to the receiver `entity`. `Ok(None)` if no
/// behavior matched, or the matched body's `resolve`/`find` found nothing.
pub fn dispatch(
    prog: &Program,
    direct_behavior: RelId,
    prototype: RelId,
    entity: Entity,
    snap: &Snapshot,
    message: &Tuple,
) -> Result<Option<Patch>> {
    match select_behavior(direct_behavior, prototype, entity, snap, message)? {
        Some(b) => b.run(prog, entity, snap, message),
        None => Ok(None),
    }
}

/// The greatest column index any `RowExpr::Col` in `guard` references, or `None`
/// if the guard references no column (constants only, or an empty `And`).
fn pred_max_col(guard: &PredExpr) -> Option<usize> {
    fn row(e: &RowExpr) -> Option<usize> {
        match e {
            RowExpr::Col(i) => Some(*i),
            RowExpr::Lit(_) => None,
        }
    }
    match guard {
        PredExpr::Eq(a, b) => row(a).into_iter().chain(row(b)).max(),
        PredExpr::And(ps) => ps.iter().filter_map(pred_max_col).max(),
    }
}

// ---- codec: serialized BehaviorIr ---------------------------------------

const ROW_COL: u8 = 1;
const ROW_LIT: u8 = 2;
const PRED_EQ: u8 = 1;
const PRED_AND: u8 = 2;
const EXPR_VALUE: u8 = 1;
const EXPR_BOOL: u8 = 2;
const VALUE_LOCAL: u8 = 1;
const VALUE_LITERAL: u8 = 2;
const VALUE_INTRINSIC: u8 = 3;
const BOOL_VALUE: u8 = 1;
const BOOL_COMPARE: u8 = 2;
const BOOL_NOT: u8 = 3;
const BOOL_AND: u8 = 4;
const BOOL_OR: u8 = 5;
const FIND_MATCH: u8 = 1;
const FIND_BIND: u8 = 2;
const FIND_MATCH_BIND: u8 = 3;
const OP_RESOLVE: u8 = 1;
const OP_FIND: u8 = 2;
const OP_LET: u8 = 3;
const OP_IF: u8 = 4;
const OP_EXPECT: u8 = 5;
const OP_ASSERT: u8 = 6;
const OP_RETRACT: u8 = 7;
const OP_EMIT: u8 = 8;
const OP_CAPABILITY: u8 = 9;

/// Serialize the guard, message bindings, and canonical executable IR under
/// the one shared v4 format byte. Literal cells reuse the core value codec.
pub fn encode_behavior(behavior: &StoredBehavior) -> Vec<u8> {
    let mut out = Vec::new();
    wire::push_version(&mut out);
    put_pred(&behavior.guard, &mut out);
    put_strings(&behavior.parameters, &mut out);
    put_ops(&behavior.body.operations, &mut out);
    out
}

pub fn decode_behavior(bytes: &[u8]) -> Result<StoredBehavior> {
    let mut pos = wire::read_version(bytes)?;
    let guard = get_pred(bytes, &mut pos)?;
    let parameters = get_strings(bytes, &mut pos)?;
    let operations = get_ops(bytes, &mut pos)?;
    if pos != bytes.len() {
        return Err(Error::Codec("trailing bytes after behavior".into()));
    }
    Ok(StoredBehavior::new(
        guard,
        parameters,
        BehaviorIr::new(operations),
    ))
}

fn put_pred(p: &PredExpr, out: &mut Vec<u8>) {
    match p {
        PredExpr::Eq(a, b) => {
            out.push(PRED_EQ);
            put_row(a, out);
            put_row(b, out);
        }
        PredExpr::And(ps) => {
            out.push(PRED_AND);
            put_u32(ps.len() as u32, out);
            for q in ps {
                put_pred(q, out);
            }
        }
    }
}

fn get_pred(bytes: &[u8], pos: &mut usize) -> Result<PredExpr> {
    match get_tag(bytes, pos)? {
        PRED_EQ => {
            let a = get_row(bytes, pos)?;
            let b = get_row(bytes, pos)?;
            Ok(PredExpr::Eq(a, b))
        }
        PRED_AND => {
            let n = get_u32(bytes, pos)? as usize;
            let mut ps = Vec::with_capacity(n);
            for _ in 0..n {
                ps.push(get_pred(bytes, pos)?);
            }
            Ok(PredExpr::And(ps))
        }
        other => Err(Error::Codec(format!("unknown predexpr tag {other}"))),
    }
}

fn put_row(e: &RowExpr, out: &mut Vec<u8>) {
    match e {
        RowExpr::Col(i) => {
            out.push(ROW_COL);
            put_u32(*i as u32, out);
        }
        RowExpr::Lit(v) => {
            out.push(ROW_LIT);
            wire::encode_value(v, out);
        }
    }
}

fn get_row(bytes: &[u8], pos: &mut usize) -> Result<RowExpr> {
    match get_tag(bytes, pos)? {
        ROW_COL => Ok(RowExpr::Col(get_u32(bytes, pos)? as usize)),
        ROW_LIT => {
            let (v, next) = wire::decode_value(bytes, *pos)?;
            *pos = next;
            Ok(RowExpr::Lit(v))
        }
        other => Err(Error::Codec(format!("unknown rowexpr tag {other}"))),
    }
}

fn put_ops(operations: &[BehaviorOp], out: &mut Vec<u8>) {
    put_u32(operations.len() as u32, out);
    for operation in operations {
        put_op(operation, out);
    }
}

fn get_ops(bytes: &[u8], pos: &mut usize) -> Result<Vec<BehaviorOp>> {
    let count = get_u32(bytes, pos)? as usize;
    (0..count).map(|_| get_op(bytes, pos)).collect()
}

fn put_op(operation: &BehaviorOp, out: &mut Vec<u8>) {
    match operation {
        BehaviorOp::Resolve {
            view,
            arguments,
            column,
            op,
            rhs,
            destinations,
        } => {
            out.push(OP_RESOLVE);
            put_str(view, out);
            put_exprs(arguments, out);
            put_str(column, out);
            out.push(match op {
                MatchOp::Exact => 0,
                MatchOp::Word => 1,
            });
            put_expr(rhs, out);
            put_strings(destinations, out);
        }
        BehaviorOp::Find {
            relation,
            arguments,
        } => {
            out.push(OP_FIND);
            put_str(relation, out);
            put_u32(arguments.len() as u32, out);
            for argument in arguments {
                put_find_arg(argument, out);
            }
        }
        BehaviorOp::Let { local, value } => {
            out.push(OP_LET);
            put_str(local, out);
            put_expr(value, out);
        }
        BehaviorOp::If {
            condition,
            then_ops,
            else_ops,
        } => {
            out.push(OP_IF);
            put_bool(condition, out);
            put_ops(then_ops, out);
            put_ops(else_ops, out);
        }
        BehaviorOp::Expect {
            relation,
            arguments,
        } => put_relation_op(OP_EXPECT, relation, arguments, out),
        BehaviorOp::Assert {
            relation,
            arguments,
        } => put_relation_op(OP_ASSERT, relation, arguments, out),
        BehaviorOp::Retract {
            relation,
            arguments,
        } => put_relation_op(OP_RETRACT, relation, arguments, out),
        BehaviorOp::Emit {
            relation,
            arguments,
        } => put_relation_op(OP_EMIT, relation, arguments, out),
        BehaviorOp::InvokeCapability {
            capability,
            arguments,
            destinations,
        } => {
            out.push(OP_CAPABILITY);
            put_str(capability, out);
            put_exprs(arguments, out);
            put_strings(destinations, out);
        }
    }
}

fn get_op(bytes: &[u8], pos: &mut usize) -> Result<BehaviorOp> {
    Ok(match get_tag(bytes, pos)? {
        OP_RESOLVE => BehaviorOp::Resolve {
            view: get_str(bytes, pos)?,
            arguments: get_exprs(bytes, pos)?,
            column: get_str(bytes, pos)?,
            op: match get_tag(bytes, pos)? {
                0 => MatchOp::Exact,
                1 => MatchOp::Word,
                tag => return Err(Error::Codec(format!("unknown match op tag {tag}"))),
            },
            rhs: get_expr(bytes, pos)?,
            destinations: get_strings(bytes, pos)?,
        },
        OP_FIND => {
            let relation = get_str(bytes, pos)?;
            let count = get_u32(bytes, pos)? as usize;
            let arguments = (0..count)
                .map(|_| get_find_arg(bytes, pos))
                .collect::<Result<_>>()?;
            BehaviorOp::Find {
                relation,
                arguments,
            }
        }
        OP_LET => BehaviorOp::Let {
            local: get_str(bytes, pos)?,
            value: get_expr(bytes, pos)?,
        },
        OP_IF => BehaviorOp::If {
            condition: get_bool(bytes, pos)?,
            then_ops: get_ops(bytes, pos)?,
            else_ops: get_ops(bytes, pos)?,
        },
        OP_EXPECT => get_relation_op(bytes, pos, |relation, arguments| BehaviorOp::Expect {
            relation,
            arguments,
        })?,
        OP_ASSERT => get_relation_op(bytes, pos, |relation, arguments| BehaviorOp::Assert {
            relation,
            arguments,
        })?,
        OP_RETRACT => get_relation_op(bytes, pos, |relation, arguments| BehaviorOp::Retract {
            relation,
            arguments,
        })?,
        OP_EMIT => get_relation_op(bytes, pos, |relation, arguments| BehaviorOp::Emit {
            relation,
            arguments,
        })?,
        OP_CAPABILITY => BehaviorOp::InvokeCapability {
            capability: get_str(bytes, pos)?,
            arguments: get_exprs(bytes, pos)?,
            destinations: get_strings(bytes, pos)?,
        },
        tag => return Err(Error::Codec(format!("unknown behavior op tag {tag}"))),
    })
}

fn put_relation_op(tag: u8, relation: &str, arguments: &[ExprIr], out: &mut Vec<u8>) {
    out.push(tag);
    put_str(relation, out);
    put_exprs(arguments, out);
}

fn get_relation_op<F>(bytes: &[u8], pos: &mut usize, make: F) -> Result<BehaviorOp>
where
    F: FnOnce(String, Vec<ExprIr>) -> BehaviorOp,
{
    Ok(make(get_str(bytes, pos)?, get_exprs(bytes, pos)?))
}

fn put_find_arg(argument: &FindArg, out: &mut Vec<u8>) {
    match argument {
        FindArg::Match(value) => {
            out.push(FIND_MATCH);
            put_expr(value, out);
        }
        FindArg::Bind(local) => {
            out.push(FIND_BIND);
            put_str(local, out);
        }
        FindArg::MatchBind { value, local } => {
            out.push(FIND_MATCH_BIND);
            put_expr(value, out);
            put_str(local, out);
        }
    }
}

fn get_find_arg(bytes: &[u8], pos: &mut usize) -> Result<FindArg> {
    Ok(match get_tag(bytes, pos)? {
        FIND_MATCH => FindArg::Match(get_expr(bytes, pos)?),
        FIND_BIND => FindArg::Bind(get_str(bytes, pos)?),
        FIND_MATCH_BIND => FindArg::MatchBind {
            value: get_expr(bytes, pos)?,
            local: get_str(bytes, pos)?,
        },
        tag => return Err(Error::Codec(format!("unknown find argument tag {tag}"))),
    })
}

fn put_exprs(expressions: &[ExprIr], out: &mut Vec<u8>) {
    put_u32(expressions.len() as u32, out);
    for expression in expressions {
        put_expr(expression, out);
    }
}

fn get_exprs(bytes: &[u8], pos: &mut usize) -> Result<Vec<ExprIr>> {
    let count = get_u32(bytes, pos)? as usize;
    (0..count).map(|_| get_expr(bytes, pos)).collect()
}

fn put_expr(expression: &ExprIr, out: &mut Vec<u8>) {
    match expression {
        ExprIr::Value(value) => {
            out.push(EXPR_VALUE);
            put_value_expr(value, out);
        }
        ExprIr::Bool(value) => {
            out.push(EXPR_BOOL);
            put_bool(value, out);
        }
    }
}

fn get_expr(bytes: &[u8], pos: &mut usize) -> Result<ExprIr> {
    match get_tag(bytes, pos)? {
        EXPR_VALUE => Ok(ExprIr::Value(get_value_expr(bytes, pos)?)),
        EXPR_BOOL => Ok(ExprIr::Bool(get_bool(bytes, pos)?)),
        tag => Err(Error::Codec(format!("unknown expression tag {tag}"))),
    }
}

fn put_value_expr(expression: &ValueExpr, out: &mut Vec<u8>) {
    match expression {
        ValueExpr::Local(local) => {
            out.push(VALUE_LOCAL);
            put_str(local, out);
        }
        ValueExpr::Literal(value) => {
            out.push(VALUE_LITERAL);
            wire::encode_value(value, out);
        }
        ValueExpr::Intrinsic { name, arguments } => {
            out.push(VALUE_INTRINSIC);
            put_str(name, out);
            put_u32(arguments.len() as u32, out);
            for argument in arguments {
                put_value_expr(argument, out);
            }
        }
    }
}

fn get_value_expr(bytes: &[u8], pos: &mut usize) -> Result<ValueExpr> {
    Ok(match get_tag(bytes, pos)? {
        VALUE_LOCAL => ValueExpr::Local(get_str(bytes, pos)?),
        VALUE_LITERAL => {
            let (value, next) = wire::decode_value(bytes, *pos)?;
            *pos = next;
            ValueExpr::Literal(value)
        }
        VALUE_INTRINSIC => {
            let name = get_str(bytes, pos)?;
            let count = get_u32(bytes, pos)? as usize;
            let arguments = (0..count)
                .map(|_| get_value_expr(bytes, pos))
                .collect::<Result<_>>()?;
            ValueExpr::Intrinsic { name, arguments }
        }
        tag => return Err(Error::Codec(format!("unknown value expression tag {tag}"))),
    })
}

fn put_bool(expression: &BoolExpr, out: &mut Vec<u8>) {
    match expression {
        BoolExpr::Value(value) => {
            out.push(BOOL_VALUE);
            put_value_expr(value, out);
        }
        BoolExpr::Compare { op, left, right } => {
            out.push(BOOL_COMPARE);
            out.push(compare_tag(*op));
            put_value_expr(left, out);
            put_value_expr(right, out);
        }
        BoolExpr::Not(value) => {
            out.push(BOOL_NOT);
            put_bool(value, out);
        }
        BoolExpr::And(left, right) => {
            out.push(BOOL_AND);
            put_bool(left, out);
            put_bool(right, out);
        }
        BoolExpr::Or(left, right) => {
            out.push(BOOL_OR);
            put_bool(left, out);
            put_bool(right, out);
        }
    }
}

fn get_bool(bytes: &[u8], pos: &mut usize) -> Result<BoolExpr> {
    Ok(match get_tag(bytes, pos)? {
        BOOL_VALUE => BoolExpr::Value(get_value_expr(bytes, pos)?),
        BOOL_COMPARE => BoolExpr::Compare {
            op: get_compare(get_tag(bytes, pos)?)?,
            left: get_value_expr(bytes, pos)?,
            right: get_value_expr(bytes, pos)?,
        },
        BOOL_NOT => BoolExpr::Not(Box::new(get_bool(bytes, pos)?)),
        BOOL_AND => BoolExpr::And(
            Box::new(get_bool(bytes, pos)?),
            Box::new(get_bool(bytes, pos)?),
        ),
        BOOL_OR => BoolExpr::Or(
            Box::new(get_bool(bytes, pos)?),
            Box::new(get_bool(bytes, pos)?),
        ),
        tag => {
            return Err(Error::Codec(format!(
                "unknown boolean expression tag {tag}"
            )))
        }
    })
}

fn compare_tag(op: CompareOp) -> u8 {
    match op {
        CompareOp::Eq => 0,
        CompareOp::Ne => 1,
        CompareOp::Lt => 2,
        CompareOp::Le => 3,
        CompareOp::Gt => 4,
        CompareOp::Ge => 5,
    }
}
fn get_compare(tag: u8) -> Result<CompareOp> {
    match tag {
        0 => Ok(CompareOp::Eq),
        1 => Ok(CompareOp::Ne),
        2 => Ok(CompareOp::Lt),
        3 => Ok(CompareOp::Le),
        4 => Ok(CompareOp::Gt),
        5 => Ok(CompareOp::Ge),
        _ => Err(Error::Codec(format!("unknown compare tag {tag}"))),
    }
}

fn put_strings(strings: &[String], out: &mut Vec<u8>) {
    put_u32(strings.len() as u32, out);
    for value in strings {
        put_str(value, out);
    }
}
fn get_strings(bytes: &[u8], pos: &mut usize) -> Result<Vec<String>> {
    let count = get_u32(bytes, pos)? as usize;
    (0..count).map(|_| get_str(bytes, pos)).collect()
}

// ---- low-level codec helpers --------------------------------------------

fn put_u32(n: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    put_u32(s.len() as u32, out);
    out.extend_from_slice(s.as_bytes());
}

fn get_tag(bytes: &[u8], pos: &mut usize) -> Result<u8> {
    let t = *bytes
        .get(*pos)
        .ok_or_else(|| Error::Codec("unexpected end (ir tag)".into()))?;
    *pos += 1;
    Ok(t)
}

fn get_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let end = *pos + 4;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| Error::Codec("unexpected end (u32)".into()))?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u32::from_be_bytes(buf))
}

fn get_str(bytes: &[u8], pos: &mut usize) -> Result<String> {
    let len = get_u32(bytes, pos)? as usize;
    let end = *pos + len;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| Error::Codec("unexpected end (str)".into()))?;
    let s = std::str::from_utf8(slice)
        .map_err(|e| Error::Codec(e.to_string()))?
        .to_string();
    *pos = end;
    Ok(s)
}
