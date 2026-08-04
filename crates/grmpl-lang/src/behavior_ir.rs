//! The sole executable behavior plan. Named statements, concatenative arms,
//! and stored behaviors all lower here before execution.

use std::collections::{BTreeMap, HashMap};

use grmpl_core::{Entity, Error, Fact, FiniteF64, Message, Patch, Result, Scheduled, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_proc::Alloc;

use crate::ast::MatchOp;
use crate::package::{ResolvedCapabilityGrant, ResolvedGrantSet};
use crate::Program;

/// A pure scalar expression. Intrinsic names come from a closed registry and
/// are serialized symbolically so the framing does not change when a later
/// version grows that registry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueExpr {
    Local(String),
    Literal(Value),
    Intrinsic {
        name: String,
        arguments: Vec<ValueExpr>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Boolean expressions are distinct so `and` and `or` retain deterministic
/// short-circuit semantics.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BoolExpr {
    Value(ValueExpr),
    Compare {
        op: CompareOp,
        left: ValueExpr,
        right: ValueExpr,
    },
    Not(Box<BoolExpr>),
    And(Box<BoolExpr>, Box<BoolExpr>),
    Or(Box<BoolExpr>, Box<BoolExpr>),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExprIr {
    Value(ValueExpr),
    Bool(BoolExpr),
}

/// A relation lookup argument: match a checked expression or bind a new local
/// from that column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FindArg {
    Match(ExprIr),
    Bind(String),
    MatchBind { value: ExprIr, local: String },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BehaviorOp {
    Resolve {
        view: String,
        arguments: Vec<ExprIr>,
        column: String,
        op: MatchOp,
        rhs: ExprIr,
        destinations: Vec<String>,
    },
    Find {
        relation: String,
        arguments: Vec<FindArg>,
    },
    Let {
        local: String,
        value: ExprIr,
    },
    If {
        condition: BoolExpr,
        then_ops: Vec<BehaviorOp>,
        else_ops: Vec<BehaviorOp>,
    },
    Expect {
        relation: String,
        arguments: Vec<ExprIr>,
    },
    Assert {
        relation: String,
        arguments: Vec<ExprIr>,
    },
    Retract {
        relation: String,
        arguments: Vec<ExprIr>,
    },
    Emit {
        relation: String,
        arguments: Vec<ExprIr>,
    },
    /// Stable capability envelope. The execution-frame implementation is
    /// supplied by the loaded runtime; capability-free execution faults.
    InvokeCapability {
        capability: String,
        arguments: Vec<ExprIr>,
        destinations: Vec<String>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BehaviorIr {
    pub operations: Vec<BehaviorOp>,
}

impl BehaviorIr {
    pub fn new(operations: Vec<BehaviorOp>) -> Self {
        Self { operations }
    }

    /// Execute without semantic capabilities. Loaded package behaviors use the
    /// execution-frame entry point added by the capability layer; legacy callers
    /// fail deterministically if an invocation appears.
    pub fn execute(
        &self,
        program: &Program,
        self_entity: Entity,
        snapshot: &Snapshot,
        initial: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Option<Patch>> {
        self.execute_inner(program, self_entity, snapshot, initial, None)
    }

    /// Execute with the immutable grants resolved when a package was loaded.
    pub fn execute_with_grants(
        &self,
        program: &Program,
        grants: &ResolvedGrantSet,
        self_entity: Entity,
        snapshot: &Snapshot,
        initial: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Option<Patch>> {
        self.execute_inner(program, self_entity, snapshot, initial, Some(grants))
    }

    fn execute_inner(
        &self,
        program: &Program,
        self_entity: Entity,
        snapshot: &Snapshot,
        initial: impl IntoIterator<Item = (String, Value)>,
        grants: Option<&ResolvedGrantSet>,
    ) -> Result<Option<Patch>> {
        let mut locals: HashMap<String, Value> = initial.into_iter().collect();
        locals
            .entry("self".into())
            .or_insert(Value::Ent(self_entity));
        let mut patch = Patch::new();
        let mut frame = CapabilityFrame::new(grants, snapshot);
        match execute_ops(
            program,
            snapshot,
            &self.operations,
            &mut locals,
            &mut patch,
            &mut frame,
        )? {
            Flow::Continue => Ok(Some(frame.seal(patch))),
            Flow::Abort => Ok(None),
        }
    }
}

struct RngStream {
    relation: grmpl_core::RelId,
    owner: Entity,
    original: i64,
    state: u64,
    used: bool,
}

struct CapabilityFrame<'a> {
    grants: Option<&'a ResolvedGrantSet>,
    snapshot: &'a Snapshot<'a>,
    allocators: BTreeMap<String, (Alloc, i64)>,
    random: BTreeMap<String, RngStream>,
    scheduled: Vec<Scheduled>,
}

impl<'a> CapabilityFrame<'a> {
    fn new(grants: Option<&'a ResolvedGrantSet>, snapshot: &'a Snapshot<'a>) -> Self {
        Self {
            grants,
            snapshot,
            allocators: BTreeMap::new(),
            random: BTreeMap::new(),
            scheduled: Vec::new(),
        }
    }

    fn invoke(&mut self, capability: &str, arguments: Vec<Value>) -> Result<Vec<Value>> {
        let grant = self
            .grants
            .and_then(|grants| grants.get(capability))
            .cloned()
            .ok_or_else(|| {
                behavior_error(format!("capability `{capability}` has no resolved grant"))
            })?;
        match grant {
            ResolvedCapabilityGrant::Allocate {
                counter,
                first,
                last,
            } => {
                if !arguments.is_empty() {
                    return Err(behavior_error(format!(
                        "allocate capability `{capability}` takes no arguments"
                    )));
                }
                if !self.allocators.contains_key(capability) {
                    validate_counter(self.snapshot, counter, first, last)?;
                    let allocator = Alloc::from_snapshot(self.snapshot, counter, first)?;
                    if !allocator.is_seeded() {
                        return Err(behavior_error(format!(
                            "allocator `{capability}` is not seeded"
                        )));
                    }
                    self.allocators
                        .insert(capability.to_owned(), (allocator, last));
                }
                let (allocator, last) = self.allocators.get_mut(capability).unwrap();
                let entity = allocator.fresh();
                if entity.0 > *last as u64 {
                    return Err(behavior_error(format!(
                        "allocator `{capability}` exhausted inclusive range at {last}"
                    )));
                }
                Ok(vec![Value::Ent(entity)])
            }
            ResolvedCapabilityGrant::Random {
                state,
                owner,
                algorithm,
            } => {
                if algorithm != "xorshift64star_v1" {
                    return Err(behavior_error(format!(
                        "unsupported random algorithm `{algorithm}`"
                    )));
                }
                let [Value::Int(bound)] = arguments.as_slice() else {
                    return Err(behavior_error(format!(
                        "random capability `{capability}` requires one Int bound"
                    )));
                };
                if *bound <= 0 {
                    return Err(behavior_error(format!(
                        "random bound must be in 1..=i64::MAX, found {bound}"
                    )));
                }
                if !self.random.contains_key(capability) {
                    let original = read_rng_state(self.snapshot, state, owner, capability)?;
                    self.random.insert(
                        capability.to_owned(),
                        RngStream {
                            relation: state,
                            owner,
                            original,
                            state: original as u64,
                            used: false,
                        },
                    );
                }
                let stream = self.random.get_mut(capability).unwrap();
                let bound = *bound as u64;
                let threshold = 0u64.wrapping_sub(bound) % bound;
                let output = loop {
                    let output = xorshift64star(&mut stream.state);
                    stream.used = true;
                    if output >= threshold {
                        break output;
                    }
                };
                Ok(vec![Value::Int((output % bound) as i64)])
            }
            ResolvedCapabilityGrant::Schedule {
                timers, targets, ..
            } => {
                let [Value::Text(target), Value::Int(due), body @ ..] = arguments.as_slice() else {
                    return Err(behavior_error(format!(
                        "schedule capability `{capability}` requires target Text, due Int, and Text body tokens"
                    )));
                };
                if body.iter().any(|value| !matches!(value, Value::Text(_))) {
                    return Err(behavior_error(format!(
                        "schedule capability `{capability}` body must contain only Text tokens"
                    )));
                }
                let (entity, inbox) = targets.get(target.as_ref()).copied().ok_or_else(|| {
                    behavior_error(format!(
                        "schedule capability `{capability}` disallows target actor `{target}`"
                    ))
                })?;
                self.scheduled.push(Scheduled {
                    timers,
                    due: *due,
                    inbox,
                    target: entity,
                    body: Tuple::new(body.to_vec()),
                });
                Ok(Vec::new())
            }
        }
    }

    fn seal(self, mut patch: Patch) -> Patch {
        for (_, (allocator, _)) in self.allocators {
            patch = allocator.seal(patch);
        }
        for (_, stream) in self.random {
            if !stream.used {
                continue;
            }
            let old = Fact::new(
                stream.relation,
                Tuple::from([Value::Ent(stream.owner), Value::Int(stream.original)]),
            );
            let new = Fact::new(
                stream.relation,
                Tuple::from([Value::Ent(stream.owner), Value::Int(stream.state as i64)]),
            );
            patch = patch.expect(old.clone()).retract(old).assert(new);
        }
        for scheduled in self.scheduled {
            patch = patch.schedule(scheduled);
        }
        patch
    }
}

fn validate_counter(
    snapshot: &Snapshot,
    relation: grmpl_core::RelId,
    first: i64,
    last: i64,
) -> Result<()> {
    let rows: Vec<_> = snapshot
        .read(relation)?
        .into_iter()
        .filter(|(_, weight)| *weight != 0)
        .collect();
    let [(tuple, 1)] = rows.as_slice() else {
        return Err(behavior_error(
            "allocator counter must contain exactly one weight-1 row",
        ));
    };
    let [Value::Int(next)] = tuple.as_slice() else {
        return Err(behavior_error(
            "allocator counter row must be `(next: Int)`",
        ));
    };
    let exhausted = last.checked_add(1);
    if *next < first || exhausted.map_or(*next > last, |end| *next > end) {
        return Err(behavior_error(format!(
            "allocator counter {next} is outside configured range {first}..={last}"
        )));
    }
    Ok(())
}

fn read_rng_state(
    snapshot: &Snapshot,
    relation: grmpl_core::RelId,
    owner: Entity,
    capability: &str,
) -> Result<i64> {
    let rows: Vec<_> = snapshot
        .read(relation)?
        .into_iter()
        .filter(|(tuple, weight)| {
            *weight != 0 && tuple.as_slice().first() == Some(&Value::Ent(owner))
        })
        .collect();
    let [(tuple, 1)] = rows.as_slice() else {
        return Err(behavior_error(format!(
            "random capability `{capability}` requires exactly one weight-1 state row"
        )));
    };
    let [Value::Ent(_), Value::Int(state)] = tuple.as_slice() else {
        return Err(behavior_error(format!(
            "random capability `{capability}` has malformed state row"
        )));
    };
    if *state == 0 {
        return Err(behavior_error(format!(
            "random capability `{capability}` has absorbing zero state"
        )));
    }
    Ok(*state)
}

fn xorshift64star(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

enum Flow {
    Continue,
    Abort,
}

fn execute_ops(
    program: &Program,
    snapshot: &Snapshot,
    operations: &[BehaviorOp],
    locals: &mut HashMap<String, Value>,
    patch: &mut Patch,
    frame: &mut CapabilityFrame<'_>,
) -> Result<Flow> {
    for operation in operations {
        match operation {
            BehaviorOp::Resolve {
                view,
                arguments,
                column,
                op,
                rhs,
                destinations,
            } => {
                let arguments = eval_exprs(arguments, locals)?;
                let query = program.view(view, &arguments).map_err(behavior_error)?;
                let yields = program
                    .view_yields(view)
                    .ok_or_else(|| behavior_error(format!("no view `{view}`")))?;
                let column_index =
                    yields
                        .iter()
                        .position(|name| name == column)
                        .ok_or_else(|| {
                            behavior_error(format!("view `{view}` has no column `{column}`"))
                        })?;
                let wanted = eval_expr(rhs, locals)?;
                let picked = query
                    .find(snapshot)?
                    .into_iter()
                    .filter(|(tuple, _)| {
                        column_match(*op, &tuple.as_slice()[column_index], &wanted)
                    })
                    .min_by(|(left, _), (right, _)| left.cmp(right));
                let Some((tuple, _)) = picked else {
                    return Ok(Flow::Abort);
                };
                for (destination, value) in destinations.iter().zip(tuple.as_slice()) {
                    bind_local(locals, destination, value.clone())?;
                }
            }
            BehaviorOp::Find {
                relation,
                arguments,
            } => {
                let relation_id = program
                    .rel_id(relation)
                    .ok_or_else(|| behavior_error(format!("no relation `{relation}`")))?;
                let matches: Vec<(usize, Value)> = arguments
                    .iter()
                    .enumerate()
                    .filter_map(|(index, argument)| match argument {
                        FindArg::Match(expression)
                        | FindArg::MatchBind {
                            value: expression, ..
                        } => Some(eval_expr(expression, locals).map(|value| (index, value))),
                        FindArg::Bind(_) => None,
                    })
                    .collect::<Result<_>>()?;
                let picked = snapshot
                    .read(relation_id)?
                    .into_iter()
                    .filter(|(tuple, _)| {
                        matches
                            .iter()
                            .all(|(index, value)| tuple.as_slice().get(*index) == Some(value))
                    })
                    .min_by(|(left, _), (right, _)| left.cmp(right));
                let Some((tuple, _)) = picked else {
                    return Ok(Flow::Abort);
                };
                for (index, argument) in arguments.iter().enumerate() {
                    if let FindArg::Bind(local) | FindArg::MatchBind { local, .. } = argument {
                        bind_local(locals, local, tuple.as_slice()[index].clone())?;
                    }
                }
            }
            BehaviorOp::Let { local, value } => {
                let value = eval_expr(value, locals)?;
                bind_local(locals, local, value)?;
            }
            BehaviorOp::If {
                condition,
                then_ops,
                else_ops,
            } => {
                let chosen = if eval_bool(condition, locals)? {
                    then_ops
                } else {
                    else_ops
                };
                let mut branch_locals = locals.clone();
                if let Flow::Abort =
                    execute_ops(program, snapshot, chosen, &mut branch_locals, patch, frame)?
                {
                    return Ok(Flow::Abort);
                }
            }
            BehaviorOp::Expect {
                relation,
                arguments,
            } => {
                *patch = std::mem::take(patch).expect(fact(program, relation, arguments, locals)?);
            }
            BehaviorOp::Assert {
                relation,
                arguments,
            } => {
                *patch = std::mem::take(patch).assert(fact(program, relation, arguments, locals)?);
            }
            BehaviorOp::Retract {
                relation,
                arguments,
            } => {
                *patch = std::mem::take(patch).retract(fact(program, relation, arguments, locals)?);
            }
            BehaviorOp::Emit {
                relation,
                arguments,
            } => {
                let inbox = program
                    .rel_id(relation)
                    .ok_or_else(|| behavior_error(format!("no relation `{relation}`")))?;
                *patch = std::mem::take(patch).emit(Message {
                    inbox,
                    body: Tuple::new(eval_exprs(arguments, locals)?),
                });
            }
            BehaviorOp::InvokeCapability {
                capability,
                arguments,
                destinations,
            } => {
                let arguments = eval_exprs(arguments, locals)?;
                let values = frame.invoke(capability, arguments)?;
                if values.len() != destinations.len() {
                    return Err(behavior_error(format!(
                        "capability `{capability}` returned {} values for {} destinations",
                        values.len(),
                        destinations.len()
                    )));
                }
                for (destination, value) in destinations.iter().zip(values) {
                    bind_local(locals, destination, value)?;
                }
            }
        }
    }
    Ok(Flow::Continue)
}

fn bind_local(locals: &mut HashMap<String, Value>, name: &str, value: Value) -> Result<()> {
    if locals.insert(name.to_owned(), value).is_some() {
        return Err(behavior_error(format!(
            "local `{name}` was bound more than once"
        )));
    }
    Ok(())
}

fn fact(
    program: &Program,
    relation: &str,
    arguments: &[ExprIr],
    locals: &HashMap<String, Value>,
) -> Result<Fact> {
    let relation_id = program
        .rel_id(relation)
        .ok_or_else(|| behavior_error(format!("no relation `{relation}`")))?;
    Ok(Fact::new(
        relation_id,
        Tuple::new(eval_exprs(arguments, locals)?),
    ))
}

fn eval_exprs(expressions: &[ExprIr], locals: &HashMap<String, Value>) -> Result<Vec<Value>> {
    expressions
        .iter()
        .map(|expression| eval_expr(expression, locals))
        .collect()
}

fn eval_expr(expression: &ExprIr, locals: &HashMap<String, Value>) -> Result<Value> {
    match expression {
        ExprIr::Value(expression) => eval_value(expression, locals),
        ExprIr::Bool(expression) => Ok(Value::Bool(eval_bool(expression, locals)?)),
    }
}

fn eval_value(expression: &ValueExpr, locals: &HashMap<String, Value>) -> Result<Value> {
    match expression {
        ValueExpr::Local(name) => locals
            .get(name)
            .cloned()
            .ok_or_else(|| behavior_error(format!("unbound local `{name}`"))),
        ValueExpr::Literal(value) => Ok(value.clone()),
        ValueExpr::Intrinsic { name, arguments } => {
            let values = arguments
                .iter()
                .map(|argument| eval_value(argument, locals))
                .collect::<Result<Vec<_>>>()?;
            eval_intrinsic(name, &values)
        }
    }
}

fn eval_bool(expression: &BoolExpr, locals: &HashMap<String, Value>) -> Result<bool> {
    match expression {
        BoolExpr::Value(expression) => match eval_value(expression, locals)? {
            Value::Bool(value) => Ok(value),
            other => Err(behavior_error(format!("expected Bool, found {other:?}"))),
        },
        BoolExpr::Compare { op, left, right } => {
            let left = eval_value(left, locals)?;
            let right = eval_value(right, locals)?;
            compare(*op, &left, &right)
        }
        BoolExpr::Not(value) => Ok(!eval_bool(value, locals)?),
        BoolExpr::And(left, right) => Ok(eval_bool(left, locals)? && eval_bool(right, locals)?),
        BoolExpr::Or(left, right) => Ok(eval_bool(left, locals)? || eval_bool(right, locals)?),
    }
}

fn eval_intrinsic(name: &str, values: &[Value]) -> Result<Value> {
    match (name, values) {
        ("neg", [Value::Int(value)]) => value
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("neg", [Value::Float(value)]) => finite(name, -value.get()),
        ("float", [Value::Int(value)]) => finite(name, *value as f64),
        ("add", [Value::Int(left), Value::Int(right)]) => left
            .checked_add(*right)
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("sub", [Value::Int(left), Value::Int(right)]) => left
            .checked_sub(*right)
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("mul", [Value::Int(left), Value::Int(right)]) => left
            .checked_mul(*right)
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("div", [Value::Int(left), Value::Int(right)]) => left
            .checked_div(*right)
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("rem", [Value::Int(left), Value::Int(right)]) => left
            .checked_rem(*right)
            .map(Value::Int)
            .ok_or_else(|| arithmetic_fault(name)),
        ("min", [Value::Int(left), Value::Int(right)]) => Ok(Value::Int((*left).min(*right))),
        ("max", [Value::Int(left), Value::Int(right)]) => Ok(Value::Int((*left).max(*right))),
        (
            op @ ("add" | "sub" | "mul" | "div" | "rem" | "min" | "max"),
            [Value::Float(left), Value::Float(right)],
        ) => {
            let left = left.get();
            let right = right.get();
            if matches!(op, "div" | "rem") && right == 0.0 {
                return Err(arithmetic_fault(op));
            }
            let result = match op {
                "add" => left + right,
                "sub" => left - right,
                "mul" => left * right,
                "div" => left / right,
                "rem" => left % right,
                "min" => left.min(right),
                "max" => left.max(right),
                _ => unreachable!(),
            };
            finite(op, result)
        }
        _ => Err(behavior_error(format!(
            "unknown or ill-typed intrinsic `{name}` with arguments {values:?}"
        ))),
    }
}

fn finite(operation: &str, value: f64) -> Result<Value> {
    FiniteF64::new(value)
        .map(Value::Float)
        .ok_or_else(|| arithmetic_fault(operation))
}

fn compare(op: CompareOp, left: &Value, right: &Value) -> Result<bool> {
    if std::mem::discriminant(left) != std::mem::discriminant(right) {
        return Err(behavior_error("comparison operands have different types"));
    }
    Ok(match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Lt => ordered(left, right, |order| order.is_lt())?,
        CompareOp::Le => ordered(left, right, |order| order.is_le())?,
        CompareOp::Gt => ordered(left, right, |order| order.is_gt())?,
        CompareOp::Ge => ordered(left, right, |order| order.is_ge())?,
    })
}

fn ordered(
    left: &Value,
    right: &Value,
    predicate: impl FnOnce(std::cmp::Ordering) -> bool,
) -> Result<bool> {
    match (left, right) {
        (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Float(_)) => {
            Ok(predicate(left.cmp(right)))
        }
        _ => Err(behavior_error("ordered comparison requires Int or Float")),
    }
}

fn column_match(op: MatchOp, have: &Value, want: &Value) -> bool {
    match (op, have, want) {
        (MatchOp::Word, Value::Text(have), Value::Text(want)) => {
            have.as_ref() == want.as_ref()
                || have.split_whitespace().any(|word| word == want.as_ref())
        }
        _ => have == want,
    }
}

fn arithmetic_fault(operation: &str) -> Error {
    behavior_error(format!("arithmetic fault in `{operation}`"))
}

fn behavior_error(message: impl Into<String>) -> Error {
    Error::Behavior(message.into())
}

#[cfg(test)]
mod tests {
    use super::xorshift64star;

    #[test]
    fn xorshift64star_v1_fixed_vectors() {
        let vectors = [
            (
                0x0000_0000_0000_0001,
                0x0000_0000_0200_0001,
                0x47e4_ce4b_896c_dd1d,
            ),
            (
                0x0000_0001_2345_6789,
                0x0246_aea6_d582_870c,
                0xeaef_17c1_8b6e_a85c,
            ),
            (
                0x8000_0000_0000_0001,
                0x8008_0010_0300_0001,
                0x38ea_0cf8_a66c_dd1d,
            ),
            (
                0xffff_ffff_ffff_ffff,
                0xfff0_001f_fe00_0000,
                0xf92c_c9e5_c600_0000,
            ),
        ];
        for (initial, successor, output) in vectors {
            let mut state = initial;
            assert_eq!(xorshift64star(&mut state), output);
            assert_eq!(state, successor);
        }
    }

    #[test]
    fn bounded_vectors_match_the_language_contract() {
        for (initial, bound, wanted) in [(1, 100, 65), (0x0000_0001_2345_6789, 52, 36)] {
            let mut state = initial;
            let threshold = 0u64.wrapping_sub(bound) % bound;
            let result = loop {
                let output = xorshift64star(&mut state);
                if output >= threshold {
                    break output % bound;
                }
            };
            assert_eq!(result, wanted);
        }
    }
}
