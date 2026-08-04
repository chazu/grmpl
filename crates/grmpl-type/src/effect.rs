//! # Effect rows & the compile-time Authority check (P8b)
//!
//! P8a ([`crate::check_query`]) typed the *read* side — a [`QueryIr`] plan's row
//! type. P8b types the *write* side: it infers a handler's **effect row** — the
//! relations it may write — and checks that write set against a process's
//! [`Authority`] **before the handler ever runs**.
//!
//! ## Effect rows
//!
//! An `on` handler's body is a sequence of statements; the ones that produce
//! world effects are `assert` / `retract` (world writes) and `emit` (a message
//! to an inbox relation). The [`EffectRow`] is the union, over every arm, of the
//! relations those statements name — split by kind:
//!
//! * `writes` — the `assert`/`retract` targets. These are the world writes the
//!   commit boundary subjects to the [`Authority`] law
//!   (`grmpl-proc::commit_patch`).
//! * `sends` — the `emit` targets. A message crossing into another domain's
//!   inbox is *not* authority-checked at commit (cross-domain consequences are
//!   messages — `DESIGN.md` §2.3), so `sends` is tracked for inspection but is
//!   **not** consulted by [`check_authority`].
//!
//! The inference is a sound over-approximation: a single message drives exactly
//! one arm at runtime, but which arm is data-dependent, so the effect row unions
//! all arms. Every relation a handler *can* write at runtime is in `writes`, and
//! nothing else is — the write targets are exactly the `rel` names of its
//! `assert`/`retract` statements, the same names `exec_stmts` resolves when it
//! builds the patch.
//!
//! ## Relation-level, by design
//!
//! [`check_authority`] is **relation-level only**: a write to relation `r` is
//! permitted iff the authority owns *some* [`Scope`] naming `r`, ignoring that
//! scope's [`KeyRange`]. A handler that writes only within relations it owns
//! passes here; whether each concrete tuple falls inside an owned key-range is
//! left to the commit boundary, which re-checks every write against the full
//! [`Authority::permits`] (ranges included). This mirrors the P8b scope: ranges
//! stay checked at commit, and code committed at *runtime* is re-checked at
//! commit regardless (P12). What this pass buys is catching a handler that would
//! write a relation entirely outside its domain *statically*, rather than at the
//! first offending commit.
//!
//! [`QueryIr`]: grmpl_lang::QueryIr
//! [`Scope`]: grmpl_core::Scope
//! [`KeyRange`]: grmpl_core::KeyRange

use std::collections::BTreeSet;
use std::fmt;

use grmpl_core::{Authority, BehaviorChecker, Error as CoreError, Fact, RelId, Value};
use grmpl_lang::{BehaviorIr, BehaviorOp, Program, ResolvedGrantSet, StoredBehavior};

/// The inferred **effect row** of an `on` handler: the relations it may write,
/// at relation granularity, split by write kind. Both sets are deterministically
/// ordered (a [`BTreeSet`] over [`RelId`]).
///
/// * [`writes`](EffectRow::writes) — `assert`/`retract` targets, subject to the
///   relation-level [`Authority`] check.
/// * [`sends`](EffectRow::sends) — `emit` targets (inbox relations), tracked for
///   inspection but not authority-checked (messages cross domains freely).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct EffectRow {
    writes: BTreeSet<RelId>,
    sends: BTreeSet<RelId>,
    capabilities: BTreeSet<String>,
}

impl EffectRow {
    /// The relations this handler asserts into or retracts from, in ascending
    /// [`RelId`] order. These are the authority-relevant world writes.
    pub fn writes(&self) -> impl Iterator<Item = RelId> + '_ {
        self.writes.iter().copied()
    }

    /// The inbox relations this handler emits messages to, in ascending order.
    /// Not consulted by [`check_authority`] — emits are not authority-checked.
    pub fn sends(&self) -> impl Iterator<Item = RelId> + '_ {
        self.sends.iter().copied()
    }

    /// Semantic capability names this behavior may invoke.
    pub fn capabilities(&self) -> impl Iterator<Item = &str> + '_ {
        self.capabilities.iter().map(String::as_str)
    }

    /// Does this handler write (assert/retract) relation `rel`?
    pub fn writes_relation(&self, rel: RelId) -> bool {
        self.writes.contains(&rel)
    }

    /// True when the handler produces no world writes and no messages.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.sends.is_empty() && self.capabilities.is_empty()
    }
}

/// A failure inferring or authorizing a handler's effect row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EffectError {
    /// No `on` handler is declared for the named inbox.
    NoHandler(String),
    /// The source could not be type-checked and lowered to executable IR.
    Lowering(String),
    /// A handler statement names a relation the program never declared, so it
    /// has no [`RelId`] to place in the effect row. (This is also a runtime
    /// error; catching it here makes it a compile-time one.)
    UndeclaredRelation(String),
    /// Durable code invokes a capability not declared by the package.
    UndeclaredCapability(String),
    /// The package declared a capability but this runtime did not resolve a host grant.
    UngrantedCapability(String),
    /// A relation the handler writes is not owned by the authority at the
    /// relation level — no owned [`Scope`](grmpl_core::Scope) names it, so the
    /// write can never be permitted regardless of key-range. Committing such a
    /// write would fail the Authority law at the commit boundary.
    Unauthorized { rel: RelId },
}

impl fmt::Display for EffectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EffectError::NoHandler(inbox) => write!(f, "no on-handler for inbox `{inbox}`"),
            EffectError::Lowering(error) => write!(f, "behavior lowering failed: {error}"),
            EffectError::UndeclaredRelation(rel) => {
                write!(f, "handler writes undeclared relation `{rel}`")
            }
            EffectError::UndeclaredCapability(name) => {
                write!(f, "behavior invokes undeclared capability `{name}`")
            }
            EffectError::UngrantedCapability(name) => {
                write!(f, "behavior invokes ungranted capability `{name}`")
            }
            EffectError::Unauthorized { rel } => write!(
                f,
                "handler writes relation {} outside its authority domain",
                rel.0
            ),
        }
    }
}

impl std::error::Error for EffectError {}

/// Infer the [`EffectRow`] of the `on` handler bound to `inbox`.
///
/// Walks every arm — both statement arms and P11 concatenative (point-free)
/// arms, which coexist in one handler — resolving each write's relation name to
/// its [`RelId`] against `prog`. In either surface, `assert`/`retract` are world
/// writes and `emit` is a send; `resolve`/`find`/`expect` (and every stack
/// shuffler) are reads or preconditions and contribute nothing. Because both
/// surfaces name their write relations the same way, the effect row — and the
/// Authority guarantee it feeds — is identical whichever surface the handler is
/// written in. Errors with [`EffectError::NoHandler`] if `inbox` has no handler,
/// or [`EffectError::UndeclaredRelation`] if a write names an undeclared
/// relation.
pub fn infer_handler_effects(prog: &Program, inbox: &str) -> Result<EffectRow, EffectError> {
    let arms = prog.handler_irs(inbox).map_err(|error| {
        if error.starts_with("no on-handler") {
            EffectError::NoHandler(inbox.to_string())
        } else if let Some(relation) = error
            .strip_prefix("operation names undeclared relation `")
            .and_then(|tail| tail.strip_suffix('`'))
        {
            EffectError::UndeclaredRelation(relation.to_owned())
        } else {
            EffectError::Lowering(error)
        }
    })?;
    let mut row = EffectRow::default();
    for arm in arms {
        add_ir_effects(prog, &arm, &mut row)?;
    }
    Ok(row)
}

fn add_ir_effects(
    prog: &Program,
    behavior: &BehaviorIr,
    row: &mut EffectRow,
) -> Result<(), EffectError> {
    fn walk(
        prog: &Program,
        operations: &[BehaviorOp],
        row: &mut EffectRow,
    ) -> Result<(), EffectError> {
        for operation in operations {
            match operation {
                BehaviorOp::Assert { relation, .. } | BehaviorOp::Retract { relation, .. } => {
                    row.writes.insert(rel_id(prog, relation)?);
                }
                BehaviorOp::Emit { relation, .. } => {
                    row.sends.insert(rel_id(prog, relation)?);
                }
                BehaviorOp::If {
                    then_ops, else_ops, ..
                } => {
                    walk(prog, then_ops, row)?;
                    walk(prog, else_ops, row)?;
                }
                BehaviorOp::InvokeCapability { capability, .. } => {
                    row.capabilities.insert(capability.clone());
                    let (_, state_relation) = prog
                        .capability(capability)
                        .ok_or_else(|| EffectError::UndeclaredCapability(capability.clone()))?;
                    row.writes.insert(state_relation);
                }
                BehaviorOp::Resolve { .. }
                | BehaviorOp::Find { .. }
                | BehaviorOp::Let { .. }
                | BehaviorOp::Expect { .. } => {}
            }
        }
        Ok(())
    }
    walk(prog, &behavior.operations, row)
}

fn rel_id(prog: &Program, rel: &str) -> Result<RelId, EffectError> {
    prog.rel_id(rel)
        .ok_or_else(|| EffectError::UndeclaredRelation(rel.to_string()))
}

/// Relation-level compile-time Authority check: every relation in
/// `effects.writes` must be owned by `authority` — some [`Scope`] names it,
/// key-range ignored. Returns the first write relation the authority does not
/// own as [`EffectError::Unauthorized`], mirroring the commit boundary's
/// first-violation behavior. `sends` (emits) are not checked.
///
/// [`Scope`]: grmpl_core::Scope
pub fn check_authority(effects: &EffectRow, authority: &Authority) -> Result<(), EffectError> {
    for rel in effects.writes() {
        if !owns_relation(authority, rel) {
            return Err(EffectError::Unauthorized { rel });
        }
    }
    Ok(())
}

/// Does `authority` own *any* slice of relation `rel`? Relation-level: a single
/// matching [`Scope`](grmpl_core::Scope) suffices, its key-range notwithstanding.
fn owns_relation(authority: &Authority, rel: RelId) -> bool {
    authority.owns.iter().any(|s| s.rel == rel)
}

/// Infer a handler's effect row and check its writes against `authority` at
/// relation granularity, in one step. Returns the inferred [`EffectRow`] on
/// success (useful for further inspection), or the first [`EffectError`].
///
/// A success is the P8b guarantee: no patch this handler produces will be
/// rejected by the commit-boundary Authority check *on the grounds of a
/// relation the authority does not own*. (A per-tuple key-range violation can
/// still be rejected at commit — that is deliberately out of scope here.)
pub fn check_handler_authority(
    prog: &Program,
    inbox: &str,
    authority: &Authority,
) -> Result<EffectRow, EffectError> {
    let effects = infer_handler_effects(prog, inbox)?;
    check_authority(&effects, authority)?;
    Ok(effects)
}

// ---- P12: stored behaviors (behaviors as relations) ---------------------

/// Infer the [`EffectRow`] of a **stored** behavior (P12): its point-free body
/// is walked exactly as a concatenative handler arm's is (shared
/// [`add_word_effect`]), so a behavior installed as data and the same words
/// written inline in an `on` handler infer the identical write set. The guard
/// (message-pattern) reads only; it contributes no effect.
pub fn infer_stored_behavior_effects(
    prog: &Program,
    behavior: &StoredBehavior,
) -> Result<EffectRow, EffectError> {
    let mut row = EffectRow::default();
    add_ir_effects(prog, &behavior.body, &mut row)?;
    Ok(row)
}

/// Infer a stored behavior's effect row and check its writes against `authority`
/// at relation granularity — the P8b check, re-run on **committed** code. This
/// is what makes installing a behavior safe: the same guarantee an inline
/// handler gets statically, a stored behavior gets at the commit that installs
/// it (see [`EffectChecker`]).
pub fn check_stored_behavior_authority(
    prog: &Program,
    behavior: &StoredBehavior,
    authority: &Authority,
) -> Result<EffectRow, EffectError> {
    let effects = infer_stored_behavior_effects(prog, behavior)?;
    check_authority(&effects, authority)?;
    Ok(effects)
}

/// The commit-boundary [`BehaviorChecker`] (P12): re-runs the P8b
/// effect/authority check on every stored behavior a patch installs.
///
/// The commit boundary (`grmpl_proc::commit_patch_checked`) calls
/// [`check_fact`](BehaviorChecker::check_fact) on each asserted fact; this
/// implementation decodes every [`Value::Code`] cell to a [`StoredBehavior`] and
/// requires its inferred write-set to lie within `authority`. Installing code
/// that could write outside the installing domain's authority is rejected —
/// before any edition is allocated, so the patch–edition law holds. Holds a
/// `&Program` to resolve the relation *names* the stored words carry to
/// [`RelId`]s.
///
/// [`BehaviorChecker`]: grmpl_core::BehaviorChecker
/// [`Value::Code`]: grmpl_core::Value::Code
pub struct EffectChecker<'a> {
    prog: &'a Program,
    grants: Option<&'a ResolvedGrantSet>,
}

impl<'a> EffectChecker<'a> {
    /// A checker resolving stored relation names against `prog`.
    pub fn new(prog: &'a Program) -> EffectChecker<'a> {
        EffectChecker { prog, grants: None }
    }

    /// A checker for loaded package code. Capability symbols must resolve in
    /// this exact immutable host-grant environment.
    pub fn with_grants(prog: &'a Program, grants: &'a ResolvedGrantSet) -> EffectChecker<'a> {
        EffectChecker {
            prog,
            grants: Some(grants),
        }
    }
}

impl BehaviorChecker for EffectChecker<'_> {
    fn check_fact(&self, fact: &Fact, authority: &Authority) -> Result<(), CoreError> {
        for cell in fact.tuple.as_slice() {
            if let Value::Code(_) = cell {
                let behavior = StoredBehavior::from_value(cell)
                    .map_err(|e| CoreError::Codec(format!("stored behavior decode: {e}")))?;
                let effects = check_stored_behavior_authority(self.prog, &behavior, authority)
                    .map_err(|e| {
                        CoreError::Authority(format!(
                            "stored behavior in relation {}: {e}",
                            fact.rel.0
                        ))
                    })?;
                if let Some(grants) = self.grants {
                    for capability in effects.capabilities() {
                        if grants.get(capability).is_none() {
                            return Err(CoreError::Authority(format!(
                                "stored behavior in relation {}: {}",
                                fact.rel.0,
                                EffectError::UngrantedCapability(capability.to_owned())
                            )));
                        }
                    }
                    validate_granted_invocations(&behavior.body.operations, grants).map_err(
                        |error| {
                            CoreError::Authority(format!(
                                "stored behavior in relation {}: {error}",
                                fact.rel.0
                            ))
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn validate_granted_invocations(
    operations: &[BehaviorOp],
    grants: &ResolvedGrantSet,
) -> Result<(), String> {
    for operation in operations {
        match operation {
            BehaviorOp::InvokeCapability {
                capability,
                arguments,
                ..
            } => grants.validate_invocation(capability, arguments)?,
            BehaviorOp::If {
                then_ops, else_ops, ..
            } => {
                validate_granted_invocations(then_ops, grants)?;
                validate_granted_invocations(else_ops, grants)?;
            }
            _ => {}
        }
    }
    Ok(())
}
