//! The commit-boundary hook for **stored code** (P12 — behaviors as relations).
//!
//! A behavior is ordinary data: a [`Value::Code`](crate::Value::Code) cell in a
//! relation, redefined by an ordinary [`Patch`](crate::Patch). But *installing*
//! code is special — the code it stores will later run and write to the world,
//! so the commit that installs it must re-run the P8b effect/authority checker
//! on the decoded behavior (ROADMAP P12; the P8b ticket's "runtime-committed
//! code re-checked at commit" clause).
//!
//! The core cannot do that check itself: it names no IR (the bright line), so it
//! cannot decode a `Value::Code` cell. So this module defines only the *hook* —
//! a trait the commit boundary calls — exactly as [`SchemaCatalog`] is the hook
//! for schema enforcement. The concrete checker lives above the line, in
//! `grmpl-type` (which knows the IR and holds the effect checker), and is passed
//! into the checked commit entry point. A caller with no code to police passes
//! [`NoBehaviorCheck`], the opt-out, mirroring [`NoSchemas`].
//!
//! [`SchemaCatalog`]: crate::SchemaCatalog
//! [`NoSchemas`]: crate::NoSchemas

use crate::authority::Authority;
use crate::error::Result;
use crate::fact::Fact;

/// Re-check the code carried by an asserted fact at the commit boundary.
///
/// The commit boundary calls [`check_fact`](Self::check_fact) on every asserted
/// world fact. An implementation inspects the fact's cells for
/// [`Value::Code`](crate::Value::Code), decodes each, infers its write-set, and
/// verifies that write-set lies within `authority` (relation-level, exactly as
/// P8b checks a handler). Returning `Err` **rejects the whole commit** — you
/// cannot install code that would write outside the domain that installs it — so
/// no edition is allocated (the patch–edition law holds: the check runs *before*
/// the atomic `commit_if`). A fact carrying no code cell must pass.
pub trait BehaviorChecker {
    /// Check every code-carrying cell of `fact` against `authority`. `Ok(())`
    /// when there is no code, or all stored behaviors are within authority.
    fn check_fact(&self, fact: &Fact, authority: &Authority) -> Result<()>;
}

/// A no-op [`BehaviorChecker`] for callers that store no code (or opt out of the
/// re-check). Every fact passes. Mirrors [`NoSchemas`](crate::NoSchemas): the
/// unchecked commit entry point (`commit_patch`) is exactly `commit_patch_checked`
/// with this checker.
pub struct NoBehaviorCheck;

impl BehaviorChecker for NoBehaviorCheck {
    fn check_fact(&self, _fact: &Fact, _authority: &Authority) -> Result<()> {
        Ok(())
    }
}
