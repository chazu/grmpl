//! # P12 law oracle — committing a behavior re-runs the effect/authority check
//!
//! Storing a behavior is installing live code, so the commit that installs it
//! must re-run the P8b effect/authority checker on the decoded behavior (ROADMAP
//! P12). This oracle proves the invariant under random churn (seeded xorshift64\*,
//! 24 seeds, seed printed on every assertion), not one example:
//!
//! For a random stored behavior (writing a random subset of relations) and a
//! random whole-relation authority:
//!
//! * **Boundary law.** [`commit_patch_checked`] with an [`EffectChecker`] admits
//!   the patch that installs the behavior **iff** every relation the behavior
//!   writes is owned by the authority — the same verdict
//!   [`check_stored_behavior_authority`] gives statically. Installing code that
//!   could write outside the installing domain is rejected before any edition is
//!   allocated.
//! * **Soundness.** When the boundary admits a behavior, *running* it and
//!   committing the patch it produces under the same authority never fails the
//!   Authority law — the static write-set over-approximates the runtime writes.
//!
//! A fixed witness (`unauthorized_behavior_is_rejected_at_commit`) is kept as a
//! readable case; the oracle above is the spec.

use std::collections::BTreeSet;

use grmpl_core::{
    Authority, BehaviorChecker, DomainId, EditionStore, Entity, Fact, NoSchemas, Patch, Scope,
    Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_ent::EntStore;
use grmpl_lang::{
    BehaviorIr, BehaviorOp, CompiledPackage, ExprIr, GrantSet, PredExpr, Program, ResolvedGrantSet,
    StoredBehavior, ValueExpr,
};
use grmpl_proc::{commit_patch, commit_patch_checked, CommitOutcome};
use grmpl_type::{check_stored_behavior_authority, EffectChecker};

/// xorshift64\* — the crate-wide seeded PRNG idiom, no external dependency.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

const WORLD: [&str; 5] = ["w0", "w1", "w2", "w3", "w4"];

/// A program declaring the world relations a behavior may write plus a `slot`
/// relation to hold the installed code cell.
fn program() -> Program {
    let mut src = String::new();
    for w in WORLD {
        src.push_str(&format!("rel {w}(a)\n"));
    }
    src.push_str("rel slot(code)\n");
    Program::compile(&src, 1).unwrap()
}

/// A stack-valid behavior that asserts into each relation in `writes`: for each,
/// push one literal then `assert`. Guard matches every message.
fn behavior_writing(writes: &BTreeSet<usize>) -> StoredBehavior {
    let body = writes
        .iter()
        .map(|&i| BehaviorOp::Assert {
            relation: WORLD[i].to_string(),
            arguments: vec![ExprIr::Value(ValueExpr::Literal(Value::Int(0)))],
        })
        .collect();
    StoredBehavior::new(PredExpr::And(vec![]), vec![], BehaviorIr::new(body))
}

/// A whole-relation authority owning `slot` (so the *carrier* fact is always
/// permitted) plus the world relations in `owns`.
fn authority(prog: &Program, owns: &BTreeSet<usize>) -> Authority {
    let mut scopes = vec![Scope::whole(prog.rel_id("slot").unwrap())];
    for &i in owns {
        scopes.push(Scope::whole(prog.rel_id(WORLD[i]).unwrap()));
    }
    Authority::new(DomainId(1), scopes)
}

#[test]
fn commit_boundary_rechecks_stored_behavior_under_random_churn() {
    let prog = program();
    let slot = prog.rel_id("slot").unwrap();

    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        let dir = tempfile::tempdir().unwrap();
        let store = EntStore::open(dir.path()).unwrap();
        let sound_dir = tempfile::tempdir().unwrap();
        let sound_store = EntStore::open(sound_dir.path()).unwrap();

        for _ in 0..30 {
            // Random write-set and random whole-relation authority.
            let writes: BTreeSet<usize> = (0..WORLD.len()).filter(|_| rng.bool()).collect();
            let owns: BTreeSet<usize> = (0..WORLD.len()).filter(|_| rng.bool()).collect();
            let behavior = behavior_writing(&writes);
            let auth = authority(&prog, &owns);

            let within = writes.is_subset(&owns);

            // Static verdict.
            let verdict = check_stored_behavior_authority(&prog, &behavior, &auth).is_ok();
            assert_eq!(
                verdict, within,
                "seed {seed}: static verdict ≠ (writes ⊆ owns) for writes {writes:?}, owns {owns:?}"
            );

            // The BehaviorChecker used at the commit boundary agrees.
            let carrier = Fact::new(slot, Tuple::from([behavior.to_value()]));
            let checker = EffectChecker::new(&prog);
            assert_eq!(
                checker.check_fact(&carrier, &auth).is_ok(),
                within,
                "seed {seed}: EffectChecker verdict ≠ (writes ⊆ owns)"
            );

            // Boundary law: commit_patch_checked admits the install iff within.
            let install = Patch::new().assert(carrier.clone());
            let outcome = commit_patch_checked(&store, &NoSchemas, &checker, &install, &auth);
            assert_eq!(
                outcome.is_ok(),
                within,
                "seed {seed}: commit_patch_checked outcome ≠ verdict for writes {writes:?}, owns {owns:?}"
            );
            if within {
                assert!(
                    matches!(outcome, Ok(CommitOutcome::Committed(_))),
                    "seed {seed}: an authorized behavior must commit, got {outcome:?}"
                );

                // Soundness: run the admitted behavior and commit its patch under
                // the same authority — it must never be Authority-rejected, since
                // the checked write-set over-approximates the runtime writes.
                let snap = Snapshot::at_current(&sound_store);
                let produced = behavior
                    .run(&prog, Entity(1), &snap, &Tuple::from([]))
                    .unwrap()
                    .expect("match-all body always yields a patch");
                let ran = commit_patch(&sound_store, &NoSchemas, &produced, &auth);
                assert!(
                    ran.is_ok(),
                    "seed {seed}: admitted behavior's patch was rejected at commit: {ran:?}"
                );
            } else {
                // A rejected install allocates no edition (patch–edition law).
                assert!(
                    matches!(outcome, Err(grmpl_core::Error::Authority(_))),
                    "seed {seed}: an unauthorized behavior must be rejected with Authority, got {outcome:?}"
                );
            }
        }
    }
}

#[test]
fn unauthorized_behavior_is_rejected_at_commit() {
    let prog = program();
    let slot = prog.rel_id("slot").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    // A behavior that writes w0 and w1, but the authority owns only slot + w0.
    let behavior = behavior_writing(&[0usize, 1].into_iter().collect());
    let auth = authority(&prog, &[0usize].into_iter().collect());
    let checker = EffectChecker::new(&prog);

    let carrier = Fact::new(slot, Tuple::from([behavior.to_value()]));
    let before = store.current();
    let install = Patch::new().assert(carrier);
    let outcome = commit_patch_checked(&store, &NoSchemas, &checker, &install, &auth);
    assert!(
        matches!(outcome, Err(grmpl_core::Error::Authority(_))),
        "installing code that writes an unowned relation must be rejected, got {outcome:?}"
    );
    assert_eq!(
        store.current(),
        before,
        "a rejected install must allocate no edition"
    );

    // Grant w1 and the same install now commits.
    let auth = authority(&prog, &[0usize, 1].into_iter().collect());
    let carrier = Fact::new(slot, Tuple::from([behavior.to_value()]));
    let install = Patch::new().assert(carrier);
    assert!(
        matches!(
            commit_patch_checked(&store, &NoSchemas, &checker, &install, &auth),
            Ok(CommitOutcome::Committed(_))
        ),
        "once authorized, the behavior installs"
    );
}

#[test]
fn stored_capability_code_requires_both_a_grant_and_state_authority() {
    let source = r#"
package stored_capability bootstrap 1
entity WORLD = 1
rel rng_state(owner: Ent, state: Int)
rel slot(code: Code)
requires random rolls(state: rng_state, owner: WORLD, algorithm: xorshift64star_v1)
bootstrap { rng_state(WORLD, 1) }
"#;
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let package = CompiledPackage::compile_with_catalog(source, &store, 100).unwrap();
    let slot = package.program.rel_id("slot").unwrap();
    let rng_state = package.program.rel_id("rng_state").unwrap();
    let behavior = StoredBehavior::new(
        PredExpr::And(vec![]),
        vec![],
        BehaviorIr::new(vec![BehaviorOp::InvokeCapability {
            capability: "rolls".into(),
            arguments: vec![ExprIr::Value(ValueExpr::Literal(Value::Int(100)))],
            destinations: vec!["roll".into()],
        }]),
    );
    let carrier = Fact::new(slot, Tuple::from([behavior.to_value()]));
    let install = Patch::new().assert(carrier.clone());
    let full_authority = Authority::new(
        DomainId(1),
        vec![Scope::whole(slot), Scope::whole(rng_state)],
    );

    let no_grants = ResolvedGrantSet::default();
    let before = store.current();
    let outcome = commit_patch_checked(
        &store,
        &NoSchemas,
        &EffectChecker::with_grants(&package.program, &no_grants),
        &install,
        &full_authority,
    );
    assert!(
        matches!(outcome, Err(grmpl_core::Error::Authority(_))),
        "{outcome:?}"
    );
    assert_eq!(
        store.current(),
        before,
        "ungranted code must allocate no edition"
    );

    let grants = GrantSet::new()
        .grant_random("rolls", "rng_state", Entity(1), "xorshift64star_v1")
        .unwrap();
    let resolved = package.resolve_grants(&grants).unwrap();
    let carrier_only = Authority::new(DomainId(1), vec![Scope::whole(slot)]);
    let outcome = commit_patch_checked(
        &store,
        &NoSchemas,
        &EffectChecker::with_grants(&package.program, &resolved),
        &install,
        &carrier_only,
    );
    assert!(
        matches!(outcome, Err(grmpl_core::Error::Authority(_))),
        "{outcome:?}"
    );
    assert_eq!(
        store.current(),
        before,
        "unauthorized state access must allocate no edition"
    );

    assert!(matches!(
        commit_patch_checked(
            &store,
            &NoSchemas,
            &EffectChecker::with_grants(&package.program, &resolved),
            &Patch::new().assert(carrier),
            &full_authority,
        ),
        Ok(CommitOutcome::Committed(_))
    ));
}

#[test]
fn stored_schedule_code_cannot_bypass_the_granted_target_allowlist() {
    let source = r#"
        package stored_schedule bootstrap 1
        entity ACTOR = 7
        rel clock(seq: Int, wall_ms: Int, random: Int)
        rel timers(due: Int, inbox: Int, target: Ent, body: Tuple)
        rel inbox(process: Ent, seq: Int, body: Tuple)
        rel inbox_seq(process: Ent, next: Int)
        rel cursor(process: Ent, pos: Int)
        rel slot(code: Code)
        requires schedule world_clock(clock: clock, timers: timers, sequences: inbox_seq)
        authority writes { write cursor }
        actor ACTOR { inbox inbox cursor cursor authority writes }
        form command { "noop" -> Noop() }
        on inbox parse command { match Noop() { } }
        bootstrap { inbox_seq(ACTOR, 0) }
    "#;
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();
    let package = CompiledPackage::compile_with_catalog(source, &store, 100).unwrap();
    let grants = GrantSet::new()
        .grant_schedule("world_clock", "clock", "timers", "inbox_seq", ["ACTOR"])
        .unwrap();
    let resolved = package.resolve_grants(&grants).unwrap();
    let slot = package.program.rel_id("slot").unwrap();
    let timers = package.program.rel_id("timers").unwrap();
    let authority = Authority::new(DomainId(1), vec![Scope::whole(slot), Scope::whole(timers)]);
    let code = |target: &str| {
        StoredBehavior::new(
            PredExpr::And(vec![]),
            vec![],
            BehaviorIr::new(vec![BehaviorOp::InvokeCapability {
                capability: "world_clock".into(),
                arguments: vec![
                    ExprIr::Value(ValueExpr::Literal(Value::text(target))),
                    ExprIr::Value(ValueExpr::Literal(Value::Int(0))),
                    ExprIr::Value(ValueExpr::Literal(Value::text("noop"))),
                ],
                destinations: vec![],
            }]),
        )
        .to_value()
    };

    let before = store.current();
    let rejected = commit_patch_checked(
        &store,
        &NoSchemas,
        &EffectChecker::with_grants(&package.program, &resolved),
        &Patch::new().assert(Fact::new(slot, Tuple::from([code("OTHER")]))),
        &authority,
    );
    assert!(matches!(rejected, Err(grmpl_core::Error::Authority(_))));
    assert_eq!(store.current(), before);

    assert!(matches!(
        commit_patch_checked(
            &store,
            &NoSchemas,
            &EffectChecker::with_grants(&package.program, &resolved),
            &Patch::new().assert(Fact::new(slot, Tuple::from([code("ACTOR")]))),
            &authority,
        ),
        Ok(CommitOutcome::Committed(_))
    ));
}
