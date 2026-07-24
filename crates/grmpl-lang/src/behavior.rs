//! # Behaviors as relations — live code (P12)
//!
//! The defining MOO capability: a *behavior* is not privileged VM state, it is
//! ordinary data — a [`Value::Code`] cell in a relation. This module makes that
//! concrete over the P7 CBPV IR the earlier phases reified:
//!
//! * A [`StoredBehavior`] is a **message-pattern guard** plus a **point-free
//!   body**. The guard is the reified P7 predicate language ([`PredExpr`] —
//!   only `Eq`/`And`, tested against the incoming message tuple), exactly as the
//!   ticket restricts stored-pattern guards; the body is a [`Word`] list, the
//!   CBPV thunk (`concat`), run by the *same* interpreter a `on`-handler arm
//!   uses ([`crate::compile::exec_words`]). So a stored behavior and an inline
//!   handler with the same words build byte-identical patches.
//!
//! * [`encode_behavior`] / [`decode_behavior`] serialize that IR to bytes, which
//!   [`StoredBehavior::to_value`] wraps as [`Value::Code`]. The framing rides the
//!   one shared [`grmpl_core::wire::FORMAT_VERSION`] byte and reuses
//!   `wire::encode_value` for the literals inside it — there is still exactly one
//!   value encoding. The IR tag set here is a *third* namespace under that
//!   version byte (beside the value tags and the schema `Ty` tags); changing it
//!   bumps `FORMAT_VERSION`, same policy (P12 already bumped it 2→3 to add
//!   `Value::Code`).
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
use grmpl_core::{Entity, Error, Patch, RelId, Result, Tuple, Value};
use grmpl_diff::Snapshot;

use crate::ast::MatchOp;
use crate::compile::{exec_words, Program};
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
    /// The point-free effect body — the CBPV thunk. Run over a stack seeded with
    /// the message's columns, exactly as a concatenative handler arm's body runs
    /// over its constructor arguments.
    pub body: Vec<Word>,
}

impl StoredBehavior {
    /// Build a stored behavior.
    pub fn new(guard: PredExpr, body: Vec<Word>) -> StoredBehavior {
        StoredBehavior { guard, body }
    }

    /// Does this behavior's guard accept `message`? Safe against arity
    /// mismatches: a guard that references a column the message lacks does not
    /// match (rather than panicking), so dispatch over arbitrary messages is
    /// total.
    pub fn matches(&self, message: &Tuple) -> bool {
        pred_max_col(&self.guard).map(|c| c < message.arity()).unwrap_or(true)
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
        exec_words(prog, &self.body, self_entity, snap, message.as_slice().to_vec())
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

// ---- codec: serialized P7 IR --------------------------------------------
//
// A *third* tag namespace under the shared `wire::FORMAT_VERSION` byte, beside
// the value tags and the schema `Ty` tags. Changing any tag below bumps
// `FORMAT_VERSION` (same P0 policy). Embedded literals reuse `wire::encode_value`
// so there is still exactly one value encoding.

// RowExpr
const ROW_COL: u8 = 1;
const ROW_LIT: u8 = 2;
// PredExpr
const PRED_EQ: u8 = 1;
const PRED_AND: u8 = 2;
// Word
const W_SELF: u8 = 1;
const W_LIT: u8 = 2;
const W_DUP: u8 = 3;
const W_DROP: u8 = 4;
const W_SWAP: u8 = 5;
const W_OVER: u8 = 6;
const W_ROT: u8 = 7;
const W_NIP: u8 = 8;
const W_TUCK: u8 = 9;
const W_2DUP: u8 = 10;
const W_2DROP: u8 = 11;
const W_RESOLVE: u8 = 12;
const W_FIND: u8 = 13;
const W_EXPECT: u8 = 14;
const W_ASSERT: u8 = 15;
const W_RETRACT: u8 = 16;
const W_EMIT: u8 = 17;
// MatchOp
const OP_EXACT: u8 = 0;
const OP_WORD: u8 = 1;

/// Serialize a [`StoredBehavior`] to bytes: `version(1) || predexpr || u32(nwords) || word*`.
/// The bytes go straight into a [`Value::Code`] cell (see [`StoredBehavior::to_value`]).
pub fn encode_behavior(b: &StoredBehavior) -> Vec<u8> {
    let mut out = Vec::new();
    wire::push_version(&mut out);
    put_pred(&b.guard, &mut out);
    put_u32(b.body.len() as u32, &mut out);
    for w in &b.body {
        put_word(w, &mut out);
    }
    out
}

/// Inverse of [`encode_behavior`]. Rejects a wrong version byte, an unknown IR
/// tag, or trailing bytes (all [`Error::Codec`]).
pub fn decode_behavior(bytes: &[u8]) -> Result<StoredBehavior> {
    let mut pos = wire::read_version(bytes)?;
    let guard = get_pred(bytes, &mut pos)?;
    let n = get_u32(bytes, &mut pos)? as usize;
    let mut body = Vec::with_capacity(n);
    for _ in 0..n {
        body.push(get_word(bytes, &mut pos)?);
    }
    if pos != bytes.len() {
        return Err(Error::Codec("trailing bytes after behavior".into()));
    }
    Ok(StoredBehavior { guard, body })
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

fn put_word(w: &Word, out: &mut Vec<u8>) {
    // Exhaustive over `Word` — a new word variant must gain a tag here (and in
    // `get_word`) rather than silently failing to round-trip.
    match w {
        Word::SelfEntity => out.push(W_SELF),
        Word::Lit(v) => {
            out.push(W_LIT);
            wire::encode_value(v, out);
        }
        Word::Dup => out.push(W_DUP),
        Word::Drop => out.push(W_DROP),
        Word::Swap => out.push(W_SWAP),
        Word::Over => out.push(W_OVER),
        Word::Rot => out.push(W_ROT),
        Word::Nip => out.push(W_NIP),
        Word::Tuck => out.push(W_TUCK),
        Word::TwoDup => out.push(W_2DUP),
        Word::TwoDrop => out.push(W_2DROP),
        Word::Resolve { view, col, op } => {
            out.push(W_RESOLVE);
            put_str(view, out);
            put_str(col, out);
            out.push(match op {
                MatchOp::Exact => OP_EXACT,
                MatchOp::Word => OP_WORD,
            });
        }
        Word::Find { rel, keyn } => {
            out.push(W_FIND);
            put_str(rel, out);
            put_u32(*keyn as u32, out);
        }
        Word::Expect(rel) => {
            out.push(W_EXPECT);
            put_str(rel, out);
        }
        Word::Assert(rel) => {
            out.push(W_ASSERT);
            put_str(rel, out);
        }
        Word::Retract(rel) => {
            out.push(W_RETRACT);
            put_str(rel, out);
        }
        Word::Emit(rel) => {
            out.push(W_EMIT);
            put_str(rel, out);
        }
    }
}

fn get_word(bytes: &[u8], pos: &mut usize) -> Result<Word> {
    Ok(match get_tag(bytes, pos)? {
        W_SELF => Word::SelfEntity,
        W_LIT => {
            let (v, next) = wire::decode_value(bytes, *pos)?;
            *pos = next;
            Word::Lit(v)
        }
        W_DUP => Word::Dup,
        W_DROP => Word::Drop,
        W_SWAP => Word::Swap,
        W_OVER => Word::Over,
        W_ROT => Word::Rot,
        W_NIP => Word::Nip,
        W_TUCK => Word::Tuck,
        W_2DUP => Word::TwoDup,
        W_2DROP => Word::TwoDrop,
        W_RESOLVE => {
            let view = get_str(bytes, pos)?;
            let col = get_str(bytes, pos)?;
            let op = match get_tag(bytes, pos)? {
                OP_EXACT => MatchOp::Exact,
                OP_WORD => MatchOp::Word,
                other => return Err(Error::Codec(format!("unknown matchop tag {other}"))),
            };
            Word::Resolve { view, col, op }
        }
        W_FIND => {
            let rel = get_str(bytes, pos)?;
            let keyn = get_u32(bytes, pos)? as usize;
            Word::Find { rel, keyn }
        }
        W_EXPECT => Word::Expect(get_str(bytes, pos)?),
        W_ASSERT => Word::Assert(get_str(bytes, pos)?),
        W_RETRACT => Word::Retract(get_str(bytes, pos)?),
        W_EMIT => Word::Emit(get_str(bytes, pos)?),
        other => return Err(Error::Codec(format!("unknown word tag {other}"))),
    })
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
    let t = *bytes.get(*pos).ok_or_else(|| Error::Codec("unexpected end (ir tag)".into()))?;
    *pos += 1;
    Ok(t)
}

fn get_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let end = *pos + 4;
    let slice = bytes.get(*pos..end).ok_or_else(|| Error::Codec("unexpected end (u32)".into()))?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(slice);
    *pos = end;
    Ok(u32::from_be_bytes(buf))
}

fn get_str(bytes: &[u8], pos: &mut usize) -> Result<String> {
    let len = get_u32(bytes, pos)? as usize;
    let end = *pos + len;
    let slice = bytes.get(*pos..end).ok_or_else(|| Error::Codec("unexpected end (str)".into()))?;
    let s = std::str::from_utf8(slice).map_err(|e| Error::Codec(e.to_string()))?.to_string();
    *pos = end;
    Ok(s)
}
