//! P8b: effect-row inference + the compile-time, relation-level Authority check.
//!
//! Two things are proven here:
//!
//! 1. **Inference is exact.** [`infer_handler_effects`] collects precisely the
//!    relations an `on` handler's `assert`/`retract` (writes) and `emit` (sends)
//!    statements name — no more, no less — validated against the patches the
//!    compiled behavior actually produces.
//!
//! 2. **The static check predicts the commit boundary.** For whole-relation
//!    authorities, `check_handler_authority` returns `Ok` *iff* no reachable
//!    message makes `grmpl_proc::commit_patch` fail the runtime Authority law —
//!    asserted as a randomized law oracle against a real store. The
//!    relation-level boundary (key-ranges stay a commit concern) is pinned by a
//!    dedicated test.

use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, Entity, Error, KeyRange, NoSchemas, Patch, RelId, Scope, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_lang::Program;
use grmpl_proc::{commit_patch, CommitOutcome};
use grmpl_store::FjallStore;
use grmpl_type::{
    check_authority, check_handler_authority, infer_handler_effects, EffectError, EffectRow,
};

/// The handler under test. Four arms write different relations; `wc` also emits.
/// Effect row: writes = {a, b, c}, sends = {d}.
const SRC: &str = r#"
    rel a(x)
    rel b(x)
    rel c(x)
    rel d(process, seq, body)

    form cmd {
        "wa"  -> Wa()
        "wb"  -> Wb()
        "wc"  -> Wc()
        "wab" -> Wab()
    }

    on inbox parse cmd {
        match Wa()  { assert a(self) }
        match Wb()  { assert b(self) }
        match Wc()  { retract c(self)  emit d(self) }
        match Wab() { assert a(self)  retract b(self) }
    }
"#;

const SELF: Entity = Entity(1);

/// The commands, each with the relations its arm actually writes (asserts or
/// retracts) — the ground truth the effect row must over-approximate exactly.
const COMMANDS: &[(&str, &[&str])] = &[
    ("wa", &["a"]),
    ("wb", &["b"]),
    ("wc", &["c"]),
    ("wab", &["a", "b"]),
];

fn program() -> Arc<Program> {
    Arc::new(Program::compile(SRC, 1).unwrap())
}

fn rid(prog: &Program, name: &str) -> RelId {
    prog.rel_id(name).unwrap()
}

fn store() -> (FjallStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    (store, dir)
}

/// Run one command through the compiled behavior, returning the patch it builds.
/// The arms use only `self`, so the snapshot content is irrelevant.
fn run_cmd(prog: &Arc<Program>, store: &FjallStore, word: &str) -> Patch {
    let behavior = Program::behavior(prog, "inbox", SELF).unwrap();
    let snap = Snapshot::at_current(store);
    behavior(&snap, &Tuple::from([Value::text(word)])).unwrap()
}

/// The relations a patch writes (assert/retract), for cross-checking the row.
fn patch_write_rels(patch: &Patch) -> Vec<RelId> {
    let mut rels: Vec<RelId> = patch
        .asserts
        .iter()
        .chain(patch.retracts.iter())
        .map(|f| f.rel)
        .collect();
    rels.sort();
    rels.dedup();
    rels
}

// ---- inference is exact ----------------------------------------------------

#[test]
fn effect_row_collects_writes_and_sends() {
    let prog = program();
    let row = infer_handler_effects(&prog, "inbox").unwrap();

    let want_writes: Vec<RelId> = ["a", "b", "c"].iter().map(|n| rid(&prog, n)).collect();
    let want_sends: Vec<RelId> = vec![rid(&prog, "d")];

    assert_eq!(row.writes().collect::<Vec<_>>(), want_writes);
    assert_eq!(row.sends().collect::<Vec<_>>(), want_sends);
    assert!(row.writes_relation(rid(&prog, "a")));
    assert!(!row.writes_relation(rid(&prog, "d"))); // emit is a send, not a write
    assert!(!row.is_empty());
}

/// The effect row must over-approximate every reachable patch's writes exactly:
/// each command's actual writes ⊆ the row, and the row's union == all commands'
/// writes. This is the soundness of the inference itself.
#[test]
fn effect_row_matches_the_behavior_it_summarizes() {
    let prog = program();
    let (store, _dir) = store();
    let row = infer_handler_effects(&prog, "inbox").unwrap();
    let row_writes: Vec<RelId> = row.writes().collect();

    let mut seen: Vec<RelId> = Vec::new();
    for (word, expected) in COMMANDS {
        let patch = run_cmd(&prog, &store, word);
        let actual = patch_write_rels(&patch);
        let expected_ids: Vec<RelId> = expected.iter().map(|n| rid(&prog, n)).collect();
        // The behavior really produced the writes we claim (guards against a
        // silent parse miss making the oracle vacuous).
        assert_eq!(actual, expected_ids, "command `{word}` writes");
        // Every actual write is in the inferred row.
        for r in &actual {
            assert!(row_writes.contains(r), "row missed {r:?} from `{word}`");
        }
        seen.extend(actual);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen, row_writes, "row is exactly the union of reachable writes");
}

#[test]
fn no_handler_is_an_error() {
    let prog = program();
    assert_eq!(
        infer_handler_effects(&prog, "nope"),
        Err(EffectError::NoHandler("nope".into()))
    );
}

#[test]
fn writing_an_undeclared_relation_is_an_error() {
    // `ghost` is never declared, so the write has no RelId.
    let src = r#"
        rel a(x)
        form cmd { "g" -> G() }
        on inbox parse cmd { match G() { assert ghost(self) } }
    "#;
    let prog = Program::compile(src, 1).unwrap();
    assert_eq!(
        infer_handler_effects(&prog, "inbox"),
        Err(EffectError::UndeclaredRelation("ghost".into()))
    );
}

// ---- the relation-level Authority check ------------------------------------

fn domain(scopes: Vec<Scope>) -> Authority {
    Authority::new(DomainId(1), scopes)
}

#[test]
fn owning_every_written_relation_passes() {
    let prog = program();
    let auth = domain(vec![
        Scope::whole(rid(&prog, "a")),
        Scope::whole(rid(&prog, "b")),
        Scope::whole(rid(&prog, "c")),
    ]);
    let row = check_handler_authority(&prog, "inbox", &auth).unwrap();
    // The emit target `d` need not be owned — sends are not authority-checked.
    assert!(!auth.owns.iter().any(|s| s.rel == rid(&prog, "d")));
    assert_eq!(row.writes().count(), 3);
}

#[test]
fn a_written_relation_outside_the_domain_is_rejected() {
    let prog = program();
    // Owns a and b but not c — the handler's `retract c(self)` is unauthorized.
    let auth = domain(vec![
        Scope::whole(rid(&prog, "a")),
        Scope::whole(rid(&prog, "b")),
    ]);
    assert_eq!(
        check_handler_authority(&prog, "inbox", &auth),
        Err(EffectError::Unauthorized { rel: rid(&prog, "c") })
    );
}

/// The headline P8b boundary: the static check is **relation-level**. Owning a
/// key-*range* of a relation counts as owning the relation here, even a range
/// that excludes the tuple actually written — that per-tuple violation is left
/// to the commit boundary, which still rejects it.
#[test]
fn relation_level_owns_a_slice_but_commit_still_checks_the_range() {
    let prog = program();
    let (store, _dir) = store();
    let a = rid(&prog, "a");

    // A slice of `a` on col 0 covering ids 1000..=2000 — SELF (id 1) is outside.
    let auth = domain(vec![Scope::slice(a, KeyRange { col: 0, lo: 1000, hi: 2000 })]);

    // Relation-level: the write to `a` is authorized statically (we own a slice
    // of `a`), so inference + check pass.
    let row = infer_handler_effects(&prog, "inbox").unwrap();
    // Restrict to just the `wa` arm's effect to isolate the `a` write:
    // check_authority over the full row would also flag b, c — so assert the
    // relation-level ownership predicate for `a` directly.
    assert!(row.writes_relation(a));
    let a_only = single_write_row(&prog, "a");
    assert_eq!(check_authority(&a_only, &auth), Ok(()));

    // But committing `wa` (which writes a[SELF], id 1 ∉ [1000,2000]) is rejected
    // at the boundary by the *range* check — exactly what P8b defers to commit.
    let patch = run_cmd(&prog, &store, "wa");
    let outcome = commit_patch(&store, &NoSchemas, &patch, &auth);
    assert!(matches!(outcome, Err(Error::Authority(_))), "range still checked at commit");
}

/// An effect row containing exactly one write relation, for isolating a single
/// arm's authorization in the relation-level range test.
fn single_write_row(prog: &Program, rel: &str) -> EffectRow {
    // Build via a one-arm program so we don't depend on EffectRow internals.
    let src = format!(
        r#"
        rel {rel}(x)
        form cmd {{ "w" -> W() }}
        on inbox parse cmd {{ match W() {{ assert {rel}(self) }} }}
        "#
    );
    let p = Program::compile(&src, prog.rel_id(rel).unwrap().0).unwrap();
    infer_handler_effects(&p, "inbox").unwrap()
}

// ---- randomized law oracle: static verdict ⇔ runtime commit outcome --------

/// xorshift64* — a seeded PRNG, no new dependency (the crate-wide test idiom).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

/// For a random **whole-relation** authority over subsets of {a, b, c}, the
/// static `check_handler_authority` verdict must match reality at the commit
/// boundary exactly:
///
/// * `Ok` ⇔ no reachable command's patch fails `commit_patch`'s Authority law;
/// * `Unauthorized` ⇔ some reachable command's patch does fail it.
///
/// The left side is pure static analysis of the handler AST; the right side is
/// real `commit_patch` calls against a fresh `FjallStore`. Their equivalence,
/// asserted every round over random authorities, is the P8b soundness law.
#[test]
fn static_authority_matches_commit_boundary_under_random_domains() {
    let prog = program();
    let rels = ["a", "b", "c"];

    for seed in 1u64..=200 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

        // Random subset of {a, b, c} owned as whole relations.
        let mut scopes = Vec::new();
        let mut owned: Vec<RelId> = Vec::new();
        for r in rels {
            if rng.bool() {
                scopes.push(Scope::whole(rid(&prog, r)));
                owned.push(rid(&prog, r));
            }
        }
        let auth = domain(scopes);

        let verdict = check_handler_authority(&prog, "inbox", &auth);

        // Independently drive every command through the real commit boundary.
        let (store, _dir) = store();
        let mut any_authority_failure = false;
        for (word, _) in COMMANDS {
            let patch = run_cmd(&prog, &store, word);
            match commit_patch(&store, &NoSchemas, &patch, &auth) {
                Ok(CommitOutcome::Committed(_)) => {}
                Ok(CommitOutcome::Rejected) => {
                    panic!("seed {seed}: `{word}` unexpectedly rejected (no preconditions)")
                }
                Err(Error::Authority(_)) => any_authority_failure = true,
                Err(e) => panic!("seed {seed}: `{word}` unexpected error {e:?}"),
            }
        }

        match verdict {
            Ok(row) => {
                // Static pass ⇒ the boundary never trips on Authority grounds.
                assert!(
                    !any_authority_failure,
                    "seed {seed}: static OK but a commit hit the Authority law (owned={owned:?})"
                );
                // And a static pass means every written relation is owned.
                for r in row.writes() {
                    assert!(owned.contains(&r), "seed {seed}: passed but {r:?} not owned");
                }
            }
            Err(EffectError::Unauthorized { rel }) => {
                // Static reject ⇒ some commit really does hit the Authority law.
                assert!(
                    any_authority_failure,
                    "seed {seed}: static rejected {rel:?} but no commit tripped Authority \
                     (owned={owned:?})"
                );
                assert!(!owned.contains(&rel), "seed {seed}: rejected an owned {rel:?}");
            }
            Err(other) => panic!("seed {seed}: unexpected effect error {other:?}"),
        }
    }
}

/// P11 coexistence: effect inference is **surface-agnostic**. A concatenative
/// (point-free) handler and the equivalent statement handler infer the same
/// effect row — the same `writes` and `sends` — because both name their write
/// relations the same way. A handler that mixes the two surfaces infers their
/// union.
#[test]
fn concatenative_and_statement_arms_infer_the_same_effects() {
    const STMT: &str = r#"
        rel a(x)
        rel b(x)
        rel d(process)
        form cmd { "wa" -> Wa()  "wc" -> Wc() }
        on inbox parse cmd {
            match Wa() { assert a(self) }
            match Wc() { retract b(self)  emit d(self) }
        }
    "#;
    // Same handler, point-free: `self assert a`, `self retract b`, `self emit d`.
    // (The concatenative `emit` pops the inbox relation's full arity, so `d` is
    // one-column here; the inferred `sends` set is identical either way.)
    const CONCAT: &str = r#"
        rel a(x)
        rel b(x)
        rel d(process)
        form cmd { "wa" -> Wa()  "wc" -> Wc() }
        on inbox parse cmd {
            match Wa() [ self assert a ]
            match Wc() [ self retract b  self emit d ]
        }
    "#;

    let stmt = Program::compile(STMT, 1).unwrap();
    let concat = Program::compile(CONCAT, 1).unwrap();

    let row_stmt = infer_handler_effects(&stmt, "inbox").unwrap();
    let row_concat = infer_handler_effects(&concat, "inbox").unwrap();

    // Relation ids line up (same declaration order), so the rows are equal.
    assert_eq!(row_stmt.writes().collect::<Vec<_>>(), row_concat.writes().collect::<Vec<_>>());
    assert_eq!(row_stmt.sends().collect::<Vec<_>>(), row_concat.sends().collect::<Vec<_>>());
    // Concretely: writes = {a, b}, sends = {d}.
    assert_eq!(
        row_concat.writes().collect::<Vec<_>>(),
        vec![rid(&concat, "a"), rid(&concat, "b")]
    );
    assert_eq!(row_concat.sends().collect::<Vec<_>>(), vec![rid(&concat, "d")]);
}
