//! # P12 law oracles — behaviors as relations (live code)
//!
//! Two invariants of storing behaviors as data, each a **seeded randomized-churn
//! law oracle** (xorshift64\*, 24 seeds, seed printed on every assertion for
//! replay) rather than a fixed example — the fleet convention. A fixed witness
//! (`live_redefinition_through_prototype`) is kept below as a readable
//! demonstration, but the oracles are the spec.
//!
//! 1. **Codec round-trip.** A [`StoredBehavior`] survives `decode ∘ encode`, and
//!    the [`Value::Code`] cell it wraps survives the one shared value codec —
//!    over random guards and bodies covering every IR tag.
//!
//! 2. **Dispatch = a function of the world.** Selecting the behavior an entity
//!    dispatches for a message is exactly the recursive `implements` view
//!    (`direct_behavior ∪ prototype ⋈ implements`) followed by least-match
//!    selection — re-derived from an independent model after every commit of
//!    random churn (asserts/retracts of `direct_behavior`/`prototype` rows).
//!    Because the world is churned by ordinary commits, this *is* the
//!    live-redefinition law: change the rows, and the very next dispatch follows.

use std::collections::{BTreeSet, HashSet};

use grmpl_core::{Authority, DomainId, Entity, NoSchemas, RelId, Scope, Tuple, Value};
use grmpl_diff::Snapshot;
use grmpl_lang::ast::MatchOp;
use grmpl_lang::behavior::{decode_behavior, encode_behavior};
use grmpl_lang::{
    dispatch, implemented_behaviors, select_behavior, BehaviorIr, BehaviorOp, BoolExpr, CompareOp,
    ExprIr, FindArg, PredExpr, Program, RowExpr, StoredBehavior, ValueExpr,
};
use grmpl_proc::commit_patch;

/// Deterministic, seedable xorshift64\* — the crate-wide test idiom (no external
/// `rand`/`proptest`). Identical math to `grmpl-ent/tests/store_laws.rs`.
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
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    fn bool(&mut self) -> bool {
        self.below(2) == 0
    }
}

// ---- random IR generators (codec coverage) ------------------------------

fn rand_value(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0 => Value::Int(rng.below(7) as i64 - 3),
        1 => Value::text(["", "a", "take", "look north"][rng.below(4) as usize]),
        2 => Value::Bool(rng.bool()),
        3 => Value::Ent(Entity(rng.below(5))),
        4 => Value::bytes([rng.below(256) as u8, rng.below(256) as u8]),
        // A nested code cell — the codec must handle a Value::Code literal too.
        _ => Value::code([rng.below(256) as u8]),
    }
}

fn rand_row(rng: &mut Rng, ncols: usize) -> RowExpr {
    if ncols > 0 && rng.bool() {
        RowExpr::Col(rng.below(ncols as u64) as usize)
    } else {
        RowExpr::Lit(rand_value(rng))
    }
}

fn rand_pred(rng: &mut Rng, ncols: usize, depth: u32) -> PredExpr {
    if depth == 0 || rng.bool() {
        PredExpr::Eq(rand_row(rng, ncols), rand_row(rng, ncols))
    } else {
        let n = rng.below(3);
        PredExpr::And((0..n).map(|_| rand_pred(rng, ncols, depth - 1)).collect())
    }
}

fn rand_value_expr(rng: &mut Rng, depth: u32) -> ValueExpr {
    if depth == 0 || rng.bool() {
        if rng.bool() {
            ValueExpr::Local(format!("v{}", rng.below(4)))
        } else {
            ValueExpr::Literal(rand_value(rng))
        }
    } else {
        ValueExpr::Intrinsic {
            name: ["add", "min", "float"][rng.below(3) as usize].into(),
            arguments: (0..rng.below(3))
                .map(|_| rand_value_expr(rng, depth - 1))
                .collect(),
        }
    }
}

fn rand_bool(rng: &mut Rng, depth: u32) -> BoolExpr {
    if depth == 0 {
        BoolExpr::Value(rand_value_expr(rng, 1))
    } else {
        match rng.below(5) {
            0 => BoolExpr::Value(rand_value_expr(rng, 1)),
            1 => BoolExpr::Compare {
                op: [
                    CompareOp::Eq,
                    CompareOp::Ne,
                    CompareOp::Lt,
                    CompareOp::Le,
                    CompareOp::Gt,
                    CompareOp::Ge,
                ][rng.below(6) as usize],
                left: rand_value_expr(rng, 1),
                right: rand_value_expr(rng, 1),
            },
            2 => BoolExpr::Not(Box::new(rand_bool(rng, depth - 1))),
            3 => BoolExpr::And(
                Box::new(rand_bool(rng, depth - 1)),
                Box::new(rand_bool(rng, depth - 1)),
            ),
            _ => BoolExpr::Or(
                Box::new(rand_bool(rng, depth - 1)),
                Box::new(rand_bool(rng, depth - 1)),
            ),
        }
    }
}

fn rand_expr(rng: &mut Rng) -> ExprIr {
    if rng.bool() {
        ExprIr::Value(rand_value_expr(rng, 2))
    } else {
        ExprIr::Bool(rand_bool(rng, 2))
    }
}

fn rand_ops(rng: &mut Rng, depth: u32) -> Vec<BehaviorOp> {
    let names = ["located", "held", "named", "greeted"];
    let mut operations = Vec::new();
    for _ in 0..rng.below(5) {
        let relation = names[rng.below(names.len() as u64) as usize].to_string();
        operations.push(match rng.below(if depth == 0 { 8 } else { 9 }) {
            0 => BehaviorOp::Resolve {
                view: relation,
                arguments: vec![rand_expr(rng)],
                column: "value".into(),
                op: if rng.bool() {
                    MatchOp::Exact
                } else {
                    MatchOp::Word
                },
                rhs: rand_expr(rng),
                destinations: vec![format!("v{}", rng.below(4))],
            },
            1 => BehaviorOp::Find {
                relation,
                arguments: vec![
                    if rng.bool() {
                        FindArg::Match(rand_expr(rng))
                    } else {
                        FindArg::MatchBind {
                            value: rand_expr(rng),
                            local: "bound".into(),
                        }
                    },
                    FindArg::Bind("other".into()),
                ],
            },
            2 => BehaviorOp::Let {
                local: "x".into(),
                value: rand_expr(rng),
            },
            3 if depth > 0 => BehaviorOp::If {
                condition: rand_bool(rng, 2),
                then_ops: rand_ops(rng, depth - 1),
                else_ops: rand_ops(rng, depth - 1),
            },
            3 => BehaviorOp::Expect {
                relation,
                arguments: vec![rand_expr(rng)],
            },
            4 => BehaviorOp::Assert {
                relation,
                arguments: vec![rand_expr(rng)],
            },
            5 => BehaviorOp::Retract {
                relation,
                arguments: vec![rand_expr(rng)],
            },
            6 => BehaviorOp::Emit {
                relation,
                arguments: vec![rand_expr(rng)],
            },
            _ => BehaviorOp::InvokeCapability {
                capability: relation,
                arguments: vec![rand_expr(rng)],
                destinations: vec!["result".into()],
            },
        });
    }
    operations
}

fn rand_behavior(rng: &mut Rng) -> StoredBehavior {
    let ncols = rng.below(4) as usize;
    StoredBehavior::new(
        rand_pred(rng, ncols, 2),
        (0..ncols).map(|index| format!("v{index}")).collect(),
        BehaviorIr::new(rand_ops(rng, 2)),
    )
}

// ---- Oracle 1: codec round-trip -----------------------------------------

#[test]
fn behavior_codec_roundtrips_under_random_churn() {
    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        for _ in 0..80 {
            let b = rand_behavior(&mut rng);

            // decode ∘ encode is the identity.
            let bytes = encode_behavior(&b);
            let back = decode_behavior(&bytes)
                .unwrap_or_else(|e| panic!("seed {seed}: decode failed: {e:?} for {b:?}"));
            assert_eq!(back, b, "seed {seed}: behavior codec not round-trip");

            // The wrapped Value::Code survives the one shared value/tuple codec.
            assert_eq!(
                StoredBehavior::from_value(&b.to_value()).unwrap(),
                b,
                "seed {seed}: from_value ∘ to_value not round-trip"
            );
            let mut wire_bytes = Vec::new();
            let t = Tuple::from([b.to_value()]);
            grmpl_core::wire::encode_tuple(&t, &mut wire_bytes);
            let (back_t, pos) = grmpl_core::wire::decode_tuple(&wire_bytes, 0).unwrap();
            assert_eq!(pos, wire_bytes.len(), "seed {seed}: trailing wire bytes");
            assert_eq!(back_t, t, "seed {seed}: Value::Code not wire round-trip");

            // The version byte leads the framing (P0 policy).
            assert_eq!(
                bytes[0],
                grmpl_core::wire::FORMAT_VERSION,
                "seed {seed}: behavior must lead with the format version"
            );
        }
    }
}

#[test]
fn decode_rejects_wrong_version_and_garbage() {
    let b = StoredBehavior::new(PredExpr::And(vec![]), vec![], BehaviorIr::default());
    let mut bytes = encode_behavior(&b);
    bytes[0] = grmpl_core::wire::FORMAT_VERSION.wrapping_add(1);
    assert!(
        decode_behavior(&bytes).is_err(),
        "wrong version must be rejected"
    );
    assert!(
        decode_behavior(&[]).is_err(),
        "empty buffer must be rejected"
    );
}

// ---- Oracle 2: dispatch = the implements view over the live world -------

const DIRECT: RelId = RelId(1); // direct_behavior(entity, code)
const PROTO: RelId = RelId(2); //  prototype(entity, parent)

/// The independent model of dispatch: the transitive `implements` closure over
/// the current rows, then least-code selection among matches.
fn model_select(
    direct: &HashSet<(u64, usize)>,
    proto: &HashSet<(u64, u64)>,
    pool: &[(StoredBehavior, Value)],
    entity: u64,
    message: &Tuple,
) -> Option<StoredBehavior> {
    // Transitive closure of entity's reachable prototypes (including itself).
    let mut reach: HashSet<u64> = HashSet::new();
    let mut frontier = vec![entity];
    while let Some(e) = frontier.pop() {
        if reach.insert(e) {
            for &(c, p) in proto {
                if c == e {
                    frontier.push(p);
                }
            }
        }
    }
    // Behaviors implemented = direct behaviors of any reachable entity, deduped
    // by their stored Value and taken in ascending Value order (what the
    // tuple-sorted `implements` view yields).
    let mut codes: BTreeSet<Value> = BTreeSet::new();
    for &(e, bidx) in direct {
        if reach.contains(&e) {
            codes.insert(pool[bidx].1.clone());
        }
    }
    for code in codes {
        let b = StoredBehavior::from_value(&code).unwrap();
        if b.matches(message) {
            return Some(b);
        }
    }
    None
}

fn stored_assert(guard: PredExpr, parameters: usize, relation: &str) -> StoredBehavior {
    StoredBehavior::new(
        guard,
        (0..parameters)
            .map(|index| format!("message{index}"))
            .collect(),
        BehaviorIr::new(vec![BehaviorOp::Assert {
            relation: relation.into(),
            arguments: vec![ExprIr::Value(ValueExpr::Local("self".into()))],
        }]),
    )
}

#[test]
fn dispatch_matches_model_under_random_churn() {
    grmpl_conformance::for_each_store(|c| {
        // A fixed pool of distinct behaviors, each guarding a message tag (plus one
        // match-all). Their stored bytes give the deterministic selection order.
        let tags = ["a", "b", "c"];
        let pool: Vec<(StoredBehavior, Value)> = {
            let mut v: Vec<StoredBehavior> = tags
                .iter()
                .map(|t| {
                    stored_assert(
                        PredExpr::Eq(RowExpr::Col(0), RowExpr::Lit(Value::text(*t))),
                        1,
                        "greeted",
                    )
                })
                .collect();
            v.push(StoredBehavior::new(
                PredExpr::And(vec![]),
                vec!["message0".into()],
                BehaviorIr::default(),
            ));
            v.into_iter().map(|b| (b.clone(), b.to_value())).collect()
        };
        let messages: Vec<Tuple> = tags
            .iter()
            .map(|t| Tuple::from([Value::text(*t)]))
            .chain([Tuple::from([Value::text("z")])])
            .collect();

        const NE: u64 = 5;
        // Declares the relation a selected body names (`greeted`); compiled once.
        // `direct_behavior`/`prototype` are read by RelId, not name.
        let prog = Program::compile("rel greeted(who)", 100).unwrap();

        for seed in 1..=24u64 {
            // A fresh store of the same substrate each round: behaviors committed
            // by an earlier seed would otherwise still be dispatchable.
            let sib = c.sibling();
            let store = sib.store();
            let mut rng = Rng::new(seed);
            let mut direct: HashSet<(u64, usize)> = HashSet::new();
            let mut proto: HashSet<(u64, u64)> = HashSet::new();

            let rounds = 20 + rng.below(25);
            for _ in 0..rounds {
                // A batch of random toggles keeps row weights in {0,1} so the store's
                // consolidated read equals the model set exactly.
                let mut batch: Vec<(RelId, Tuple, i64)> = Vec::new();
                let edits = 1 + rng.below(4);
                for _ in 0..edits {
                    if rng.bool() {
                        let e = rng.below(NE);
                        let bidx = rng.below(pool.len() as u64) as usize;
                        let row = (e, bidx);
                        let diff = if direct.remove(&row) {
                            -1
                        } else {
                            direct.insert(row);
                            1
                        };
                        batch.push((
                            DIRECT,
                            Tuple::from([Value::Ent(Entity(e)), pool[bidx].1.clone()]),
                            diff,
                        ));
                    } else {
                        let e = rng.below(NE);
                        let p = rng.below(NE);
                        let row = (e, p);
                        let diff = if proto.remove(&row) {
                            -1
                        } else {
                            proto.insert(row);
                            1
                        };
                        batch.push((
                            PROTO,
                            Tuple::from([Value::Ent(Entity(e)), Value::Ent(Entity(p))]),
                            diff,
                        ));
                    }
                }
                let ed = store.commit(&batch).unwrap();
                let snap = Snapshot::new(store, ed);

                for e in 0..NE {
                    for msg in &messages {
                        let want = model_select(&direct, &proto, &pool, e, msg);
                        let got = select_behavior(DIRECT, PROTO, Entity(e), &snap, msg).unwrap();
                        assert_eq!(
                            got, want,
                            "seed {seed}: dispatch selection disagrees for entity {e}, msg {msg:?}"
                        );

                        // dispatch() runs iff a behavior was selected; the two agree
                        // on *whether* something fires.
                        let ran = dispatch(&prog, DIRECT, PROTO, Entity(e), &snap, msg).unwrap();
                        assert_eq!(
                            ran.is_some(),
                            want.is_some(),
                            "seed {seed}: dispatch fired ≠ selected for entity {e}, msg {msg:?}"
                        );
                    }
                }
            }
        }
    });
}

// ---- Fixed witness: live redefinition through a prototype ----------------

#[test]
fn live_redefinition_through_prototype() {
    grmpl_conformance::for_each_store(|c| {
        let store = c.store();
        let prog = Program::compile(
            "rel direct_behavior(entity, code)\nrel prototype(entity, parent)\nrel greeted(who)\nrel waved(who)",
            1,
        )
        .unwrap();
        let direct = prog.rel_id("direct_behavior").unwrap();
        let proto = prog.rel_id("prototype").unwrap();
        let greeted = prog.rel_id("greeted").unwrap();
        let waved = prog.rel_id("waved").unwrap();

        let auth = Authority::new(
            DomainId(1),
            vec![
                Scope::whole(direct),
                Scope::whole(proto),
                Scope::whole(greeted),
                Scope::whole(waved),
            ],
        );
        let player = Entity(10);
        let avatar = Entity(20);
        let msg = Tuple::from([]);

        // A behavior that greets, and one that waves — both stored as data.
        let greet = stored_assert(PredExpr::And(vec![]), 0, "greeted");
        let wave = stored_assert(PredExpr::And(vec![]), 0, "waved");

        // Install: player inherits from avatar, avatar directly has `greet`.
        let install = grmpl_core::Patch::new()
            .assert(grmpl_core::Fact::new(
                proto,
                Tuple::from([Value::Ent(player), Value::Ent(avatar)]),
            ))
            .assert(grmpl_core::Fact::new(
                direct,
                Tuple::from([Value::Ent(avatar), greet.to_value()]),
            ));
        commit_patch(store, &NoSchemas, &install, &auth).unwrap();

        // The player dispatches the *inherited* behavior via the implements view.
        let snap = Snapshot::at_current(store);
        let behaviors = implemented_behaviors(direct, proto, player, &snap).unwrap();
        assert_eq!(
            behaviors,
            vec![greet.clone()],
            "player must inherit avatar's behavior"
        );
        let patch = dispatch(&prog, direct, proto, player, &snap, &msg)
            .unwrap()
            .unwrap();
        assert_eq!(patch.asserts.len(), 1);
        assert_eq!(patch.asserts[0].rel, greeted, "inherited dispatch greets");

        // Redefine the prototype's behavior with an *ordinary Patch* — no new
        // mechanism. The next dispatch through the same player follows immediately.
        let redefine = grmpl_core::Patch::new()
            .retract(grmpl_core::Fact::new(
                direct,
                Tuple::from([Value::Ent(avatar), greet.to_value()]),
            ))
            .assert(grmpl_core::Fact::new(
                direct,
                Tuple::from([Value::Ent(avatar), wave.to_value()]),
            ));
        commit_patch(store, &NoSchemas, &redefine, &auth).unwrap();

        let snap = Snapshot::at_current(store);
        let patch = dispatch(&prog, direct, proto, player, &snap, &msg)
            .unwrap()
            .unwrap();
        assert_eq!(patch.asserts.len(), 1);
        assert_eq!(
            patch.asserts[0].rel, waved,
            "after redefinition, dispatch waves — live code"
        );
    });
}
