//! `grmpl shotengai [STORE_DIR]` — the durable Kasumi Shotengai game.
//!
//! The world laws live in `worlds/shotengai.grmpl`. This edge host owns only
//! terminal I/O, initial conditions, finite rule tables, DSP instancing, combat
//! coordination, and whole-store branch routing.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grmpl_core::{
    Authority, Diff, DomainId, Edition, EditionStore, Entity, Fact, Patch, RelId, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_ent::{BranchId, Dag, EntStore};
use grmpl_lang::Program;
use grmpl_proc::{
    commit_patch, decode_activation, enqueue_seq, seed_seq, CommitOutcome, OnWatch, Process,
};

const WORLD_SOURCE: &str = include_str!("../../../worlds/shotengai.grmpl");

const PLAYER: Entity = Entity(1);
const WORLD: Entity = Entity(2);

// Surface rooms.
const EAST_GATE: Entity = Entity(10);
const ARCADE: Entity = Entity(11);
const KISSATEN: Entity = Entity(12);
const REPAIR_SHOP: Entity = Entity(13);
const SENTO: Entity = Entity(14);
const COOPERATIVE: Entity = Entity(15);
const SHRINE_ALLEY: Entity = Entity(16);
const CINEMA: Entity = Entity(17);
const ROOFTOP: Entity = Entity(18);

// Surface things and residents.
const RED_UMBRELLA: Entity = Entity(20);
const BRASS_TOKEN: Entity = Entity(21);
const OLD_RADIO: Entity = Entity(22);
const TEA_TIN: Entity = Entity(23);
const CAT: Entity = Entity(30);
const PROPRIETOR: Entity = Entity(31);
const REPAIRER: Entity = Entity(32);
const CARETAKER: Entity = Entity(33);

// Jobs and their signature abilities.
const COURIER: Entity = Entity(100);
const COOK: Entity = Entity(101);
const REPAIRER_JOB: Entity = Entity(102);
const SHRINE_ATTENDANT: Entity = Entity(103);
const NIGHT_WATCH: Entity = Entity(104);
const QUICK_STEP: Entity = Entity(200);
const SEARING_PAN: Entity = Entity(201);
const LIVE_WIRE: Entity = Entity(202);
const PAPER_WARD: Entity = Entity(203);
const LANTERN_WALL: Entity = Entity(204);

// The self-contained dungeon template. Every template fact is keyed by an
// entity in this block, allowing `instance_template` to relocate all entity
// coordinates together.
const DUNGEON_BASE: u64 = 1_000;
const DUNGEON_SPAN: u64 = 100;
const DUNGEON_ENTRY: Entity = Entity(1_000);
const FLOODED_STORE: Entity = Entity(1_001);
const FORGOTTEN_ARCADE: Entity = Entity(1_002);
const LEDGER_VAULT: Entity = Entity(1_003);
const MIRROR_CHAMBER: Entity = Entity(1_004);
const MANNEQUIN_STOCKROOM: Entity = Entity(1_005);
const RECEIPT_MOTH: Entity = Entity(1_010);
const SHUTTER_MAW: Entity = Entity(1_011);
const LAST_CUSTOMER: Entity = Entity(1_012);
const MANNEQUIN_SHELL: Entity = Entity(1_013);
const DUNGEON_COIN: Entity = Entity(1_020);
const RED_THREAD: Entity = Entity(1_021);
const COPYING_MIRROR: Entity = Entity(1_022);
const INSTANCE_BASE: u64 = 100_000;
const INSTANCE_STRIDE: u64 = 1_000;

const PATROL: [(Entity, Entity); 5] = [
    (EAST_GATE, ARCADE),
    (ARCADE, KISSATEN),
    (KISSATEN, SHRINE_ALLEY),
    (SHRINE_ALLEY, COOPERATIVE),
    (COOPERATIVE, EAST_GATE),
];

#[derive(Clone, Copy)]
struct Rels {
    located: RelId,
    named: RelId,
    described: RelId,
    held: RelId,
    exits: RelId,
    players: RelId,
    value: RelId,
    person: RelId,
    label: RelId,
    knows: RelId,
    patrol: RelId,
    job_clock: RelId,
    job: RelId,
    active_job: RelId,
    job_rank: RelId,
    job_points: RelId,
    job_stats: RelId,
    job_progression: RelId,
    ability: RelId,
    ability_rank: RelId,
    ability_power: RelId,
    monster: RelId,
    hp: RelId,
    defense: RelId,
    monster_power: RelId,
    monster_reward: RelId,
    defeated: RelId,
    attack_result: RelId,
    combat_event: RelId,
    session_branch: RelId,
    echo_depth: RelId,
    instance_state: RelId,
    instance_counter: RelId,
    rng_state: RelId,
    card: RelId,
    cardval: RelId,
    cardrank: RelId,
    slotpair: RelId,
    slottrio: RelId,
    sum15two: RelId,
    sum15three: RelId,
    tell: RelId,
    inbox: RelId,
    inbox_seq: RelId,
    cursor: RelId,
    wmail: RelId,
    wcursor: RelId,
    wseq: RelId,
}

impl Rels {
    fn resolve(program: &Program) -> Result<Rels, String> {
        let get = |name: &str| {
            program
                .rel_id(name)
                .ok_or_else(|| format!("shotengai world has no `rel {name}`"))
        };
        Ok(Rels {
            located: get("located")?,
            named: get("named")?,
            described: get("described")?,
            held: get("held")?,
            exits: get("exits")?,
            players: get("players")?,
            value: get("value")?,
            person: get("person")?,
            label: get("label")?,
            knows: get("knows")?,
            patrol: get("patrol")?,
            job_clock: get("job_clock")?,
            job: get("job")?,
            active_job: get("active_job")?,
            job_rank: get("job_rank")?,
            job_points: get("job_points")?,
            job_stats: get("job_stats")?,
            job_progression: get("job_progression")?,
            ability: get("ability")?,
            ability_rank: get("ability_rank")?,
            ability_power: get("ability_power")?,
            monster: get("monster")?,
            hp: get("hp")?,
            defense: get("defense")?,
            monster_power: get("monster_power")?,
            monster_reward: get("monster_reward")?,
            defeated: get("defeated")?,
            attack_result: get("attack_result")?,
            combat_event: get("combat_event")?,
            session_branch: get("session_branch")?,
            echo_depth: get("echo_depth")?,
            instance_state: get("instance_state")?,
            instance_counter: get("instance_counter")?,
            rng_state: get("rng_state")?,
            card: get("card")?,
            cardval: get("cardval")?,
            cardrank: get("cardrank")?,
            slotpair: get("slotpair")?,
            slottrio: get("slottrio")?,
            sum15two: get("sum15two")?,
            sum15three: get("sum15three")?,
            tell: get("tell")?,
            inbox: get("inbox")?,
            inbox_seq: get("inbox_seq")?,
            cursor: get("cursor")?,
            wmail: get("wmail")?,
            wcursor: get("wcursor")?,
            wseq: get("wseq")?,
        })
    }

    fn all(self) -> Vec<RelId> {
        vec![
            self.located,
            self.named,
            self.described,
            self.held,
            self.exits,
            self.players,
            self.value,
            self.person,
            self.label,
            self.knows,
            self.patrol,
            self.job_clock,
            self.job,
            self.active_job,
            self.job_rank,
            self.job_points,
            self.job_stats,
            self.job_progression,
            self.ability,
            self.ability_rank,
            self.ability_power,
            self.monster,
            self.hp,
            self.defense,
            self.monster_power,
            self.monster_reward,
            self.defeated,
            self.attack_result,
            self.combat_event,
            self.session_branch,
            self.echo_depth,
            self.instance_state,
            self.instance_counter,
            self.rng_state,
            self.card,
            self.cardval,
            self.cardrank,
            self.slotpair,
            self.slottrio,
            self.sum15two,
            self.sum15three,
            self.tell,
            self.inbox,
            self.inbox_seq,
            self.cursor,
            self.wmail,
            self.wcursor,
            self.wseq,
        ]
    }

    fn template(self) -> [RelId; 11] {
        [
            self.located,
            self.named,
            self.described,
            self.exits,
            self.value,
            self.monster,
            self.hp,
            self.defense,
            self.monster_power,
            self.monster_reward,
            self.defeated,
        ]
    }
}

fn player_authority(r: Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(r.located),
            Scope::whole(r.held),
            Scope::whole(r.knows),
            Scope::whole(r.active_job),
            Scope::whole(r.hp),
            Scope::whole(r.combat_event),
            Scope::whole(r.tell),
            Scope::whole(r.cursor),
        ],
    )
}

fn patrol_authority(r: Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![Scope::whole(r.located), Scope::whole(r.cursor)],
    )
}

fn coordinator_authority(r: Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(r.hp),
            Scope::whole(r.job_rank),
            Scope::whole(r.job_points),
            Scope::whole(r.ability_rank),
            Scope::whole(r.defeated),
            Scope::whole(r.monster_reward),
            Scope::whole(r.exits),
            Scope::whole(r.combat_event),
            Scope::whole(r.located),
            Scope::whole(r.instance_state),
            Scope::whole(r.instance_counter),
            Scope::whole(r.held),
            Scope::whole(r.card),
            Scope::whole(r.rng_state),
        ],
    )
}

/// One granfilade family with the root control branch kept open and at most one
/// non-root branch selected for this single-player terminal session.
struct WorldFamily {
    root: EntStore,
    active: Option<EntStore>,
}

impl WorldFamily {
    fn open(root: EntStore, r: Rels) -> Result<WorldFamily, String> {
        let branch = live_int_for(&root, r.session_branch, PLAYER, 1)
            .unwrap_or(Dag::ROOT as i64)
            .max(0) as BranchId;
        let active = if branch == Dag::ROOT {
            None
        } else {
            Some(root.branch(branch).map_err(err)?)
        };
        Ok(WorldFamily { root, active })
    }

    fn active(&self) -> &EntStore {
        self.active.as_ref().unwrap_or(&self.root)
    }

    fn active_branch(&self) -> BranchId {
        self.active().branch_id()
    }

    fn route_to(&mut self, child: EntStore, r: Rels) -> Result<(), String> {
        let old = self.active_branch();
        let new = child.branch_id();
        let old_row = Tuple::from([Value::Ent(PLAYER), Value::Int(old as i64)]);
        let new_row = Tuple::from([Value::Ent(PLAYER), Value::Int(new as i64)]);
        self.root
            .commit_if(
                &[(r.session_branch, old_row.clone())],
                &[
                    (r.session_branch, old_row, -1),
                    (r.session_branch, new_row, 1),
                ],
            )
            .map_err(err)?
            .ok_or_else(|| "the durable branch route changed concurrently".to_string())?;
        self.active = Some(child);
        Ok(())
    }
}

/// The headless game. The interactive terminal and the test suite both drive
/// this API, so the tested command path is the player-facing one.
struct Game {
    program: Arc<Program>,
    family: WorldFamily,
    r: Rels,
    player: Process,
    patrol: Process,
    watch: Option<OnWatch>,
    delivered: usize,
}

impl Game {
    fn open(path: &Path) -> Result<Game, String> {
        let root = EntStore::open(path)
            .map_err(|e| format!("cannot open shotengai store at {}: {e:?}", path.display()))?;
        let program = Arc::new(Program::compile_with_catalog(WORLD_SOURCE, &root, 1)?);
        let r = Rels::resolve(&program)?;
        program
            .register_schemas(&root, &root, Edition(root.current().0 + 1))
            .map_err(err)?;
        seed_world(&root, r)?;
        seed_rule_tables(&root, r)?;
        seed_seq(&root, r.inbox_seq, PLAYER).map_err(err)?;
        seed_seq(&root, r.inbox_seq, CAT).map_err(err)?;

        let family = WorldFamily::open(root, r)?;
        if family.active_branch() != Dag::ROOT {
            let store = family.active();
            program
                .register_schemas(store, store, Edition(store.current().0 + 1))
                .map_err(err)?;
            seed_seq(store, r.inbox_seq, PLAYER).map_err(err)?;
            seed_seq(store, r.inbox_seq, CAT).map_err(err)?;
        }

        let player = Process {
            entity: PLAYER,
            authority: player_authority(r),
            inbox: r.inbox,
            cursor_rel: r.cursor,
            behavior: Program::behavior(&program, "inbox", PLAYER)?,
        };
        let patrol = Process {
            entity: CAT,
            authority: patrol_authority(r),
            inbox: r.inbox,
            cursor_rel: r.cursor,
            behavior: Program::behavior(&program, "inbox", CAT)?,
        };
        Ok(Game {
            program,
            family,
            r,
            player,
            patrol,
            watch: None,
            delivered: 0,
        })
    }

    fn store(&self) -> &EntStore {
        self.family.active()
    }
}

pub fn run(store_dir: Option<String>) -> Result<(), String> {
    let path = store_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".grmpl/shotengai"));
    let mut game = Game::open(&path)?;
    println!("grmpl — Kasumi Shotengai");
    println!("An old covered arcade, a basement that should not fit beneath it, and a world-copying mirror.");
    println!("Persistent store: {}", path.display());
    println!("Type `help` for commands, `quit` to leave.\n");
    print_lines(game.look());

    let stdin = io::stdin();
    loop {
        print!("\n> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!("\nThe arcade lights click off behind you.");
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => return Err(format!("stdin: {e}")),
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "quit" | "q" | "exit") {
            println!("The arcade lights click off behind you.");
            return Ok(());
        }
        print_lines(game.execute(line)?);
    }
}

fn print_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

// ===========================================================================
// Initial conditions and finite rules
// ===========================================================================

fn seed_world(store: &EntStore, r: Rels) -> Result<(), String> {
    if live_entity_text(store, r.named, PLAYER).is_some() {
        return Ok(());
    }

    let e = Value::Ent;
    let text = Value::text;
    let mut facts: Vec<(RelId, Tuple, Diff)> = Vec::new();
    let mut put = |rel: RelId, tuple: Tuple| facts.push((rel, tuple, 1));

    for (entity, name, description) in [
        (PLAYER, "you", "Your reflection seems half a beat late."),
        (
            EAST_GATE,
            "East Gate",
            "A rusted arch announces Kasumi Shotengai beneath a roof of cloudy panels.",
        ),
        (
            ARCADE,
            "Central Arcade",
            "Most shutters are down, but warm rectangles of light survive between them.",
        ),
        (
            KISSATEN,
            "Tsukikage Kissaten",
            "Dark wood, cooling coffee, and a card table polished by decades of hands.",
        ),
        (
            REPAIR_SHOP,
            "Hoshino Repair",
            "Radios, fans, and clocks wait in careful rows for one more life.",
        ),
        (
            SENTO,
            "Kasumi Sento",
            "Steam rolls through the tiled entryway. The bath restores exhausted wanderers.",
        ),
        (
            COOPERATIVE,
            "Cooperative Office",
            "Ledgers fill the walls. An old time clock offers work that feels like fate.",
        ),
        (
            SHRINE_ALLEY,
            "Shrine Alley",
            "A narrow lane bends around a pocket shrine and disappears under red lanterns.",
        ),
        (
            CINEMA,
            "Shuttered Cinema",
            "Film posters bleach behind glass. A freight lift waits behind the ticket booth.",
        ),
        (
            ROOFTOP,
            "Rooftop PA Room",
            "Dusty speakers and a switchboard listen to every change below.",
        ),
        (
            RED_UMBRELLA,
            "red umbrella",
            "Its paper is dry although rain ticks against the roof.",
        ),
        (
            BRASS_TOKEN,
            "brass token",
            "A shopping-street token stamped with a shop that no longer exists.",
        ),
        (
            OLD_RADIO,
            "old radio",
            "The dial glows at a station between stations.",
        ),
        (
            TEA_TIN,
            "tea tin",
            "A dented tin fragrant with roasted tea.",
        ),
        (
            CAT,
            "Mugi",
            "A patched calico cat who knows every unlocked door.",
        ),
        (
            PROPRIETOR,
            "Sachiko",
            "The kissaten proprietor watches the arcade through rising steam.",
        ),
        (
            REPAIRER,
            "Daichi",
            "A repairer with a pencil behind one ear and burn marks on both hands.",
        ),
        (
            CARETAKER,
            "Reiko",
            "The cooperative caretaker keeps keys whose locks have been forgotten.",
        ),
    ] {
        put(r.named, Tuple::from([e(entity), text(name)]));
        put(r.described, Tuple::from([e(entity), text(description)]));
    }

    for (thing, room) in [
        (PLAYER, EAST_GATE),
        (RED_UMBRELLA, EAST_GATE),
        (BRASS_TOKEN, ARCADE),
        (OLD_RADIO, REPAIR_SHOP),
        (TEA_TIN, KISSATEN),
        (CAT, EAST_GATE),
        (PROPRIETOR, KISSATEN),
        (REPAIRER, REPAIR_SHOP),
        (CARETAKER, COOPERATIVE),
    ] {
        put(r.located, Tuple::from([e(thing), e(room)]));
    }

    for (who, public) in [
        (CAT, "a calico cat"),
        (PROPRIETOR, "the kissaten proprietor"),
        (REPAIRER, "a radio repairer"),
        (CARETAKER, "the cooperative caretaker"),
    ] {
        put(r.person, Tuple::from([e(who)]));
        put(r.label, Tuple::from([e(who), text(public)]));
    }
    put(r.knows, Tuple::from([e(PLAYER), e(PLAYER)]));
    put(r.players, Tuple::from([e(PLAYER), text("you")]));

    for (thing, coins) in [
        (RED_UMBRELLA, 6),
        (BRASS_TOKEN, 10),
        (OLD_RADIO, 18),
        (TEA_TIN, 4),
    ] {
        put(r.value, Tuple::from([e(thing), Value::Int(coins)]));
    }

    for (from, way, to) in [
        (EAST_GATE, "north", ARCADE),
        (ARCADE, "south", EAST_GATE),
        (ARCADE, "east", KISSATEN),
        (KISSATEN, "west", ARCADE),
        (ARCADE, "west", REPAIR_SHOP),
        (REPAIR_SHOP, "east", ARCADE),
        (REPAIR_SHOP, "west", SENTO),
        (SENTO, "east", REPAIR_SHOP),
        (ARCADE, "north", COOPERATIVE),
        (COOPERATIVE, "south", ARCADE),
        (COOPERATIVE, "east", SHRINE_ALLEY),
        (SHRINE_ALLEY, "west", COOPERATIVE),
        (COOPERATIVE, "west", CINEMA),
        (CINEMA, "east", COOPERATIVE),
        (KISSATEN, "up", ROOFTOP),
        (ROOFTOP, "down", KISSATEN),
    ] {
        put(r.exits, Tuple::from([e(from), text(way), e(to)]));
    }
    for (from, to) in PATROL {
        put(r.patrol, Tuple::from([e(from), e(to)]));
    }
    put(r.job_clock, Tuple::from([e(COOPERATIVE)]));

    let jobs = [
        (COURIER, "courier", "Courier", QUICK_STEP, "quick step"),
        (COOK, "cook", "Cook", SEARING_PAN, "searing pan"),
        (REPAIRER_JOB, "repairer", "Repairer", LIVE_WIRE, "live wire"),
        (
            SHRINE_ATTENDANT,
            "shrine attendant",
            "Shrine Attendant",
            PAPER_WARD,
            "paper ward",
        ),
        (
            NIGHT_WATCH,
            "night watch",
            "Night Watch",
            LANTERN_WALL,
            "lantern wall",
        ),
    ];
    for (job, job_key, job_name, ability, ability_name) in jobs {
        put(r.job, Tuple::from([e(job), text(job_key)]));
        put(r.ability, Tuple::from([e(ability), text(ability_name)]));
        put(r.named, Tuple::from([e(job), text(job_name)]));
        put(r.named, Tuple::from([e(ability), text(ability_name)]));
        put(r.job_rank, Tuple::from([e(PLAYER), e(job), Value::Int(1)]));
        put(
            r.job_points,
            Tuple::from([e(PLAYER), e(job), Value::Int(0)]),
        );
        put(
            r.ability_rank,
            Tuple::from([e(PLAYER), e(ability), Value::Int(1)]),
        );
    }
    put(r.active_job, Tuple::from([e(PLAYER), e(COURIER)]));
    put(r.hp, Tuple::from([e(PLAYER), Value::Int(20)]));

    put(
        r.session_branch,
        Tuple::from([e(PLAYER), Value::Int(Dag::ROOT as i64)]),
    );
    put(r.echo_depth, Tuple::from([e(WORLD), Value::Int(0)]));
    put(r.instance_counter, Tuple::from([e(PLAYER), Value::Int(0)]));
    put(
        r.rng_state,
        Tuple::from([e(WORLD), Value::Int(0x1_2345_6789)]),
    );

    seed_dungeon_template(r, &mut put);

    store
        .commit(&facts)
        .map_err(|e| format!("seed shotengai world: {e:?}"))?;
    Ok(())
}

fn seed_dungeon_template(r: Rels, put: &mut impl FnMut(RelId, Tuple)) {
    let e = Value::Ent;
    let text = Value::text;
    for (entity, name, description) in [
        (
            DUNGEON_ENTRY,
            "Basement Landing",
            "The lift doors close on a corridor much longer than the cinema above.",
        ),
        (
            FLOODED_STORE,
            "Flooded Storage",
            "Black water reflects price cards hanging from a ceiling that cannot be seen.",
        ),
        (
            FORGOTTEN_ARCADE,
            "Forgotten Arcade",
            "Shop fronts repeat into darkness, stocked with memories instead of goods.",
        ),
        (
            MANNEQUIN_STOCKROOM,
            "Mannequin Stockroom",
            "Headless figures turn a fraction toward you whenever the lights flicker.",
        ),
        (
            LEDGER_VAULT,
            "Ledger Vault",
            "Every purchase the shotengai forgot is written in books chained to the floor.",
        ),
        (
            MIRROR_CHAMBER,
            "Mirror Chamber",
            "A standing mirror contains the arcade in impossible, perfect detail.",
        ),
        (
            RECEIPT_MOTH,
            "receipt moth",
            "A moth folded from thermal paper, trailing itemized dust.",
        ),
        (
            SHUTTER_MAW,
            "shutter maw",
            "A corrugated shop shutter that opens only to reveal teeth.",
        ),
        (
            LAST_CUSTOMER,
            "last customer",
            "A mannequin carrying every bag and wearing no face.",
        ),
        (
            MANNEQUIN_SHELL,
            "mannequin shell",
            "A hollow display figure animated by the scrape of coat hangers.",
        ),
        (
            DUNGEON_COIN,
            "square coin",
            "A coin with four edges and five shadows.",
        ),
        (
            RED_THREAD,
            "red thread",
            "It pulls gently toward another version of your hand.",
        ),
        (
            COPYING_MIRROR,
            "copying mirror",
            "Its reflection continues even when you stand still.",
        ),
    ] {
        put(r.named, Tuple::from([e(entity), text(name)]));
        put(r.described, Tuple::from([e(entity), text(description)]));
    }
    for (thing, room) in [
        (RECEIPT_MOTH, FLOODED_STORE),
        (SHUTTER_MAW, FORGOTTEN_ARCADE),
        (MANNEQUIN_SHELL, MANNEQUIN_STOCKROOM),
        (LAST_CUSTOMER, LEDGER_VAULT),
        (DUNGEON_COIN, FLOODED_STORE),
        (RED_THREAD, FORGOTTEN_ARCADE),
        (COPYING_MIRROR, MIRROR_CHAMBER),
    ] {
        put(r.located, Tuple::from([e(thing), e(room)]));
    }
    for (from, way, to) in [
        (DUNGEON_ENTRY, "north", FLOODED_STORE),
        (FLOODED_STORE, "south", DUNGEON_ENTRY),
        (FLOODED_STORE, "north", FORGOTTEN_ARCADE),
        (FORGOTTEN_ARCADE, "south", FLOODED_STORE),
        (FORGOTTEN_ARCADE, "north", MANNEQUIN_STOCKROOM),
        (MANNEQUIN_STOCKROOM, "south", FORGOTTEN_ARCADE),
        (MANNEQUIN_STOCKROOM, "north", LEDGER_VAULT),
        (LEDGER_VAULT, "south", MANNEQUIN_STOCKROOM),
    ] {
        put(r.exits, Tuple::from([e(from), text(way), e(to)]));
    }
    for (monster, hp, guard, power, reward) in [
        (RECEIPT_MOTH, 8, 1, 3, 1),
        (SHUTTER_MAW, 13, 3, 5, 1),
        (MANNEQUIN_SHELL, 16, 4, 6, 1),
        (LAST_CUSTOMER, 22, 5, 7, 1),
    ] {
        put(r.monster, Tuple::from([e(monster)]));
        put(r.hp, Tuple::from([e(monster), Value::Int(hp)]));
        put(r.defense, Tuple::from([e(monster), Value::Int(guard)]));
        put(
            r.monster_power,
            Tuple::from([e(monster), Value::Int(power)]),
        );
        put(
            r.monster_reward,
            Tuple::from([e(monster), Value::Int(reward)]),
        );
    }
    put(r.value, Tuple::from([e(DUNGEON_COIN), Value::Int(25)]));
    put(r.value, Tuple::from([e(RED_THREAD), Value::Int(12)]));
}

fn seed_rule_tables(store: &EntStore, r: Rels) -> Result<(), String> {
    if !store
        .read_at(r.attack_result, store.current())
        .map_err(err)?
        .is_empty()
    {
        return Ok(());
    }
    let i = Value::Int;
    let e = Value::Ent;
    let mut facts: Vec<(RelId, Tuple, Diff)> = Vec::new();

    // Every attack used by the initial content is a lookup in this finite table.
    for power in 0..=14i64 {
        for guard in 0..=8i64 {
            for old_hp in 0..=40i64 {
                let damage = (power - guard / 2).max(1);
                let new_hp = (old_hp - damage).max(0);
                let outcome = if new_hp == 0 { "defeated" } else { "standing" };
                facts.push((
                    r.attack_result,
                    Tuple::from([
                        i(power),
                        i(guard),
                        i(old_hp),
                        i(new_hp),
                        Value::text(outcome),
                    ]),
                    1,
                ));
            }
        }
    }

    let job_defs = [
        (COURIER, QUICK_STEP, (4, 2, 20)),
        (COOK, SEARING_PAN, (3, 3, 24)),
        (REPAIRER_JOB, LIVE_WIRE, (3, 5, 28)),
        (SHRINE_ATTENDANT, PAPER_WARD, (5, 1, 18)),
        (NIGHT_WATCH, LANTERN_WALL, (2, 6, 30)),
    ];
    for (job, ability, (base_power, base_guard, base_hp)) in job_defs {
        for rank in 1..=3i64 {
            facts.push((
                r.job_stats,
                Tuple::from([
                    e(job),
                    i(rank),
                    i(base_power + rank - 1),
                    i(base_guard + rank - 1),
                    i(base_hp + 2 * (rank - 1)),
                ]),
                1,
            ));
        }
        for (old_rank, old_points, new_rank, new_points, old_ar, new_ar) in [
            (1, 0, 1, 1, 1, 1),
            (1, 1, 2, 0, 1, 2),
            (2, 0, 2, 1, 2, 2),
            (2, 1, 3, 0, 2, 3),
            (3, 0, 3, 0, 3, 3),
        ] {
            facts.push((
                r.job_progression,
                Tuple::from([
                    e(job),
                    i(old_rank),
                    i(old_points),
                    i(1),
                    i(new_rank),
                    i(new_points),
                    e(ability),
                    i(old_ar),
                    i(new_ar),
                ]),
                1,
            ));
        }
    }

    for (ability, powers) in [
        (QUICK_STEP, [6, 8, 10]),
        (SEARING_PAN, [5, 9, 12]),
        (LIVE_WIRE, [7, 10, 13]),
        (PAPER_WARD, [6, 10, 14]),
        (LANTERN_WALL, [5, 8, 11]),
    ] {
        for (offset, power) in powers.into_iter().enumerate() {
            facts.push((
                r.ability_power,
                Tuple::from([e(ability), i(offset as i64 + 1), i(power)]),
                1,
            ));
        }
    }

    // Cribbage remains as the relational minigame in the kissaten.
    for code in 0..52i64 {
        let rank = code % 13;
        facts.push((r.cardval, Tuple::from([i(code), i((rank + 1).min(10))]), 1));
        facts.push((r.cardrank, Tuple::from([i(code), i(rank)]), 1));
    }
    for a in 0..5i64 {
        for b in (a + 1)..5 {
            facts.push((r.slotpair, Tuple::from([i(a), i(b)]), 1));
        }
    }
    for a in 0..5i64 {
        for b in (a + 1)..5 {
            for c in (b + 1)..5 {
                facts.push((r.slottrio, Tuple::from([i(a), i(b), i(c)]), 1));
            }
        }
    }
    for a in 1..=10i64 {
        for b in 1..=10i64 {
            if a + b == 15 {
                facts.push((r.sum15two, Tuple::from([i(a), i(b)]), 1));
            }
            for c in 1..=10i64 {
                if a + b + c == 15 {
                    facts.push((r.sum15three, Tuple::from([i(a), i(b), i(c)]), 1));
                }
            }
        }
    }
    store
        .commit(&facts)
        .map_err(|e| format!("seed shotengai rules: {e:?}"))?;
    Ok(())
}

// ===========================================================================
// Player-facing command surface
// ===========================================================================

impl Game {
    fn execute(&mut self, line: &str) -> Result<Vec<String>, String> {
        let verb = line.split_whitespace().next().unwrap_or("");
        match verb {
            "help" | "?" => Ok(self.help()),
            "look" | "l" => Ok(self.look()),
            "inventory" | "inv" | "i" => Ok(self.inventory()),
            "who" => Ok(self.who()),
            "treasure" | "commerce" => Ok(self.treasure()),
            "status" => Ok(self.status()),
            "jobs" => Ok(self.jobs()),
            "deal" => self.deal(),
            "cards" | "hand" => Ok(self.cards()),
            "score" => Ok(self.score()),
            "watch" => self.start_watch(),
            "enter" => self.enter_basement(line),
            "leave" => self.leave_dungeon(false),
            "activate" => self.activate_mirror(line),
            "branches" | "lineage" => Ok(self.lineage()),
            "take" | "drop" | "go" | "say" | "greet" | "change" | "attack" | "use" => {
                self.turn(line)
            }
            other => Ok(vec![format!("I don't understand `{other}`. Try `help`.")]),
        }
    }

    fn help(&self) -> Vec<String> {
        vec![
            "Moving and meeting: look | go <way> | take <thing> | drop <thing> | inventory | greet <person> | say <word>".into(),
            "Jobs and combat: jobs | status | change <job> (at the cooperative office) | attack <monster> | use <ability>".into(),
            "Basement: enter basement (at the cinema) | leave | activate mirror | branches".into(),
            "Kissaten cards: deal | cards | score".into(),
            "World: who | treasure | watch | help | quit".into(),
        ]
    }

    fn look(&self) -> Vec<String> {
        let room = match self.room_of(PLAYER) {
            Some(room) => room,
            None => return vec!["You are nowhere in particular.".into()],
        };
        if room == ROOFTOP {
            return self.rooftop_report();
        }
        let mut out = vec![format!("{}", self.name_of(room))];
        if let Some(description) = self.description_of(room) {
            out.push(description);
        }
        let depth = self.echo_depth();
        if depth > 0 {
            out.push(format!(
                "The fluorescent lights have an extra shadow. This is echo world {depth}."
            ));
        }

        let names = self.read_entity_text(self.r.named);
        let labels = self.read_entity_text(self.r.label);
        let people = self.read_entity_set(self.r.person);
        let known = self.known_by(PLAYER);
        let defeated = self.read_entity_set(self.r.defeated);
        let mut visible = Vec::new();
        for thing in self.contents(PLAYER) {
            if thing == PLAYER {
                continue;
            }
            let mut shown = render_identity(thing, &names, &labels, &people, &known);
            if defeated.contains(&thing) {
                shown = format!("defeated {shown}");
            }
            visible.push(shown);
        }
        visible.sort();
        if visible.is_empty() {
            out.push("You see nothing of note.".into());
        } else {
            out.push(format!("You see: {}.", visible.join(", ")));
        }
        let mut ways: Vec<String> = self
            .view_rows("ways", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .iter()
            .map(|(tuple, _)| text_at(tuple, 0))
            .collect();
        ways.sort();
        out.push(format!(
            "Exits: {}.",
            if ways.is_empty() {
                "none".into()
            } else {
                ways.join(", ")
            }
        ));
        out
    }

    fn rooftop_report(&self) -> Vec<String> {
        let total: i64 = self
            .view_rows("treasure", &[])
            .unwrap_or_default()
            .iter()
            .map(|(tuple, _)| int_at(tuple, 1))
            .sum();
        let people = self.read_entity_set(self.r.person).len();
        let monsters = self.read_entity_set(self.r.monster).len();
        let passages = self.count(self.r.exits);
        vec![
            "Rooftop PA Room".into(),
            "The switchboard renders the arcade as a live report:".into(),
            format!("  · {total} coins of merchandise remain on counters and floors."),
            format!("  · {people} known residents move beneath the roof."),
            format!("  · {monsters} things wait below the cinema."),
            format!("  · {passages} passages connect this version of the world."),
            format!(
                "  · branch {}, echo depth {}.",
                self.family.active_branch(),
                self.echo_depth()
            ),
            "Every line is a query over the current branch.".into(),
        ]
    }

    fn inventory(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .view_rows("inventory", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .iter()
            .map(|(tuple, _)| text_at(tuple, 1))
            .collect();
        names.sort();
        if names.is_empty() {
            vec!["You are carrying nothing.".into()]
        } else {
            vec![format!("You are carrying: {}.", names.join(", "))]
        }
    }

    fn who(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .view_rows("roster", &[])
            .unwrap_or_default()
            .iter()
            .map(|(tuple, _)| text_at(tuple, 1))
            .collect();
        names.sort();
        vec![format!(
            "{} online in branch {}: {}",
            names.len(),
            self.family.active_branch(),
            names.join(", ")
        )]
    }

    fn treasure(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .view_rows("treasure", &[])
            .unwrap_or_default()
            .iter()
            .map(|(tuple, _)| format!("  {}: {} coins", text_at(tuple, 0), int_at(tuple, 1)))
            .collect();
        lines.sort();
        let mut out = vec!["Merchandise value per room (a sum aggregate):".into()];
        out.extend(lines);
        out
    }

    fn status(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((job, name, rank, points, power, guard, max_hp)) = self.current_job() {
            let hp = self.entity_int(self.r.hp, PLAYER).unwrap_or(0);
            out.push(format!(
                "{name} rank {rank} ({points}/2 JP) — HP {hp}/{max_hp}, power {power}, guard {guard}."
            ));
            let mut skills = Vec::new();
            for (ability, skill_name, skill_rank, skill_power) in self.abilities() {
                let owner = self
                    .job_for_ability(ability)
                    .map(|owner| if owner == job { "active" } else { "mastered" })
                    .unwrap_or("learned");
                skills.push(format!(
                    "{skill_name} r{skill_rank}/p{skill_power} ({owner})"
                ));
            }
            skills.sort();
            out.push(format!("Abilities: {}.", skills.join(", ")));
        } else {
            out.push("You have no job.".into());
        }
        out
    }

    fn jobs(&self) -> Vec<String> {
        let active = self.entity_entity(self.r.active_job, PLAYER);
        let mut rows = Vec::new();
        for (tuple, diff) in self
            .store()
            .read_at(self.r.job, self.store().current())
            .unwrap_or_default()
        {
            if diff <= 0 {
                continue;
            }
            let Some(job) = entity_at(&tuple, 0) else {
                continue;
            };
            let name = text_at(&tuple, 1);
            let rank = self.keyed_int(self.r.job_rank, PLAYER, job, 2).unwrap_or(0);
            let points = self
                .keyed_int(self.r.job_points, PLAYER, job, 2)
                .unwrap_or(0);
            rows.push(format!(
                "  {}{name}: rank {rank}, {points}/2 JP",
                if active == Some(job) { "* " } else { "  " }
            ));
        }
        rows.sort();
        let mut out = vec!["Jobs (* active; change at the cooperative time clock):".into()];
        out.extend(rows);
        out
    }

    fn lineage(&self) -> Vec<String> {
        let store = self.store();
        let branch = store.branch_id();
        let chain = store.dag().lineage(branch, store.current().0);
        let rendered = chain
            .iter()
            .map(|(b, e)| format!("branch {b}@{e}"))
            .collect::<Vec<_>>()
            .join(" <- ");
        vec![format!(
            "Echo depth {}: {rendered}",
            chain.len().saturating_sub(1)
        )]
    }

    fn view_rows(&self, name: &str, args: &[Value]) -> Result<Vec<(Tuple, Diff)>, String> {
        let query = self.program.view(name, args)?;
        query.find(&Snapshot::at_current(self.store())).map_err(err)
    }

    fn contents(&self, viewer: Entity) -> Vec<Entity> {
        self.view_rows("contents", &[Value::Ent(viewer)])
            .unwrap_or_default()
            .iter()
            .filter_map(|(tuple, _)| entity_at(tuple, 0))
            .collect()
    }

    fn known_by(&self, viewer: Entity) -> HashSet<Entity> {
        self.view_rows("acquainted", &[Value::Ent(viewer)])
            .unwrap_or_default()
            .iter()
            .filter_map(|(tuple, _)| entity_at(tuple, 0))
            .collect()
    }

    fn room_of(&self, who: Entity) -> Option<Entity> {
        self.entity_entity(self.r.located, who)
    }

    fn name_of(&self, who: Entity) -> String {
        self.read_entity_text(self.r.named)
            .remove(&who)
            .unwrap_or_else(|| "something".into())
    }

    fn description_of(&self, who: Entity) -> Option<String> {
        self.read_entity_text(self.r.described).remove(&who)
    }

    fn read_entity_text(&self, rel: RelId) -> HashMap<Entity, String> {
        let mut result = HashMap::new();
        for (tuple, diff) in self
            .store()
            .read_at(rel, self.store().current())
            .unwrap_or_default()
        {
            if diff > 0 {
                if let Some(entity) = entity_at(&tuple, 0) {
                    result.insert(entity, text_at(&tuple, 1));
                }
            }
        }
        result
    }

    fn read_entity_set(&self, rel: RelId) -> HashSet<Entity> {
        self.store()
            .read_at(rel, self.store().current())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, diff)| *diff > 0)
            .filter_map(|(tuple, _)| entity_at(&tuple, 0))
            .collect()
    }

    fn count(&self, rel: RelId) -> usize {
        self.store()
            .read_at(rel, self.store().current())
            .unwrap_or_default()
            .iter()
            .filter(|(_, diff)| *diff > 0)
            .count()
    }

    fn entity_entity(&self, rel: RelId, key: Entity) -> Option<Entity> {
        live_row(self.store(), rel, |tuple| {
            tuple.as_slice().first() == Some(&Value::Ent(key))
        })
        .and_then(|tuple| entity_at(&tuple, 1))
    }

    fn entity_int(&self, rel: RelId, key: Entity) -> Option<i64> {
        live_int_for(self.store(), rel, key, 1)
    }

    fn keyed_int(&self, rel: RelId, a: Entity, b: Entity, column: usize) -> Option<i64> {
        live_row(self.store(), rel, |tuple| {
            tuple.as_slice().first() == Some(&Value::Ent(a))
                && tuple.as_slice().get(1) == Some(&Value::Ent(b))
        })
        .map(|tuple| int_at(&tuple, column))
    }

    fn echo_depth(&self) -> i64 {
        self.entity_int(self.r.echo_depth, WORLD).unwrap_or(0)
    }

    fn current_job(&self) -> Option<(Entity, String, i64, i64, i64, i64, i64)> {
        self.view_rows("current_job", &[Value::Ent(PLAYER)])
            .ok()?
            .into_iter()
            .find(|(_, diff)| *diff > 0)
            .and_then(|(tuple, _)| {
                Some((
                    entity_at(&tuple, 0)?,
                    text_at(&tuple, 1),
                    int_at(&tuple, 2),
                    int_at(&tuple, 3),
                    int_at(&tuple, 4),
                    int_at(&tuple, 5),
                    int_at(&tuple, 6),
                ))
            })
    }

    fn abilities(&self) -> Vec<(Entity, String, i64, i64)> {
        self.view_rows("abilities", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, diff)| *diff > 0)
            .filter_map(|(tuple, _)| {
                Some((
                    entity_at(&tuple, 0)?,
                    text_at(&tuple, 1),
                    int_at(&tuple, 2),
                    int_at(&tuple, 3),
                ))
            })
            .collect()
    }

    fn job_for_ability(&self, ability: Entity) -> Option<Entity> {
        live_row(self.store(), self.r.job_progression, |tuple| {
            tuple.as_slice().get(6) == Some(&Value::Ent(ability))
        })
        .and_then(|tuple| entity_at(&tuple, 0))
    }
}

// ===========================================================================
// Small value/query helpers
// ===========================================================================

fn live_row(store: &EntStore, rel: RelId, predicate: impl Fn(&Tuple) -> bool) -> Option<Tuple> {
    store
        .read_at(rel, store.current())
        .ok()?
        .into_iter()
        .find(|(tuple, diff)| *diff > 0 && predicate(tuple))
        .map(|(tuple, _)| tuple)
}

fn live_int_for(store: &EntStore, rel: RelId, key: Entity, column: usize) -> Option<i64> {
    live_row(store, rel, |tuple| {
        tuple.as_slice().first() == Some(&Value::Ent(key))
    })
    .map(|tuple| int_at(&tuple, column))
}

fn live_entity_text(store: &EntStore, rel: RelId, key: Entity) -> Option<String> {
    live_row(store, rel, |tuple| {
        tuple.as_slice().first() == Some(&Value::Ent(key))
    })
    .map(|tuple| text_at(&tuple, 1))
}

fn tokens(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}

fn entity_at(tuple: &Tuple, column: usize) -> Option<Entity> {
    match tuple.as_slice().get(column) {
        Some(Value::Ent(entity)) => Some(*entity),
        _ => None,
    }
}

fn text_at(tuple: &Tuple, column: usize) -> String {
    match tuple.as_slice().get(column) {
        Some(Value::Text(text)) => text.to_string(),
        Some(other) => format!("{other:?}"),
        None => "?".into(),
    }
}

fn int_at(tuple: &Tuple, column: usize) -> i64 {
    match tuple.as_slice().get(column) {
        Some(Value::Int(value)) => *value,
        _ => 0,
    }
}

fn render_identity(
    thing: Entity,
    names: &HashMap<Entity, String>,
    labels: &HashMap<Entity, String>,
    people: &HashSet<Entity>,
    known: &HashSet<Entity>,
) -> String {
    if people.contains(&thing) && !known.contains(&thing) {
        labels
            .get(&thing)
            .cloned()
            .unwrap_or_else(|| "someone".into())
    } else {
        names
            .get(&thing)
            .cloned()
            .unwrap_or_else(|| "something".into())
    }
}

fn shifted(entity: Entity, shift: i64) -> Entity {
    Entity(entity.0.wrapping_add(shift as u64))
}

fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    value.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

const RANKS: [&str; 13] = [
    "A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K",
];
const SUITS: [&str; 4] = ["♠", "♥", "♦", "♣"];

fn card_name(code: i64) -> String {
    let card = code.rem_euclid(52);
    format!(
        "{}{}",
        RANKS[(card % 13) as usize],
        SUITS[(card / 13) as usize]
    )
}

fn err(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

impl Game {
    fn turn(&mut self, line: &str) -> Result<Vec<String>, String> {
        let before = self.store().current();
        enqueue_seq(
            self.store(),
            self.r.inbox,
            self.r.inbox_seq,
            PLAYER,
            tokens(line),
        )
        .map_err(err)?;
        self.player
            .run_to_idle(self.store(), self.store())
            .map_err(err)?;

        let verb = line.split_whitespace().next().unwrap_or("");
        let told = self.drain_tell(before)?;
        let mut out = Vec::new();
        match verb {
            "greet" => match told.last() {
                Some(name) => out.push(format!("You exchange names. This is {name}.")),
                None => out.push("There is no one like that here to greet.".into()),
            },
            "change" => {
                if told.is_empty() {
                    out.push("The time clock is only available in the cooperative office, or that job is unknown.".into());
                } else {
                    self.heal_to_job_max()?;
                    out.push(format!("The time card stamps itself. {}", told.join(" ")));
                    out.extend(self.status());
                }
            }
            "attack" | "use" => {
                if told.is_empty() {
                    out.push(if verb == "attack" {
                        "There is no such monster here.".into()
                    } else {
                        "That ability is unknown, or there is no monster here.".into()
                    });
                } else {
                    out.extend(told);
                    self.resolve_combat(before, &mut out)?;
                }
            }
            _ if told.is_empty() => out.push("Nothing happens.".into()),
            _ => out.extend(told),
        }

        self.tick_patrol(&mut out)?;
        self.pump_watch(&mut out)?;
        Ok(out)
    }

    fn drain_tell(&self, since: Edition) -> Result<Vec<String>, String> {
        let to = self.store().current();
        let mut out = Vec::new();
        for update in self
            .store()
            .scan_updates(self.r.tell, since, to)
            .map_err(err)?
        {
            let cols = update.tuple.as_slice();
            if update.diff > 0 && cols.first() == Some(&Value::Ent(PLAYER)) {
                if let Some(Value::Text(text)) = cols.get(1) {
                    out.push(text.to_string());
                }
            }
        }
        Ok(out)
    }

    fn tick_patrol(&self, out: &mut Vec<String>) -> Result<(), String> {
        let player_room = self.room_of(PLAYER);
        let before = self.room_of(CAT);
        enqueue_seq(
            self.store(),
            self.r.inbox,
            self.r.inbox_seq,
            CAT,
            tokens("tick"),
        )
        .map_err(err)?;
        self.patrol
            .run_to_idle(self.store(), self.store())
            .map_err(err)?;
        let after = self.room_of(CAT);
        let known = self.known_by(PLAYER);
        let name = if known.contains(&CAT) {
            self.name_of(CAT)
        } else {
            "The cat".into()
        };
        if player_room.is_some() && before == player_room && after != player_room {
            out.push(format!("{name} slips beneath a shutter."));
        } else if player_room.is_some() && after == player_room && before != player_room {
            out.push(format!("{name} appears between your feet."));
        }
        Ok(())
    }

    fn heal_to_job_max(&self) -> Result<(), String> {
        let Some((_, _, _, _, _, _, max_hp)) = self.current_job() else {
            return Ok(());
        };
        let old_hp = self.entity_int(self.r.hp, PLAYER).unwrap_or(0);
        if old_hp == max_hp {
            return Ok(());
        }
        let old = Fact::new(
            self.r.hp,
            Tuple::from([Value::Ent(PLAYER), Value::Int(old_hp)]),
        );
        let patch = Patch::new()
            .expect(old.clone())
            .retract(old)
            .assert(Fact::new(
                self.r.hp,
                Tuple::from([Value::Ent(PLAYER), Value::Int(max_hp)]),
            ));
        match commit_patch(
            self.store(),
            self.store(),
            &patch,
            &coordinator_authority(self.r),
        )
        .map_err(err)?
        {
            CommitOutcome::Committed(_) => Ok(()),
            CommitOutcome::Rejected => Err("HP changed while changing jobs".into()),
        }
    }

    fn resolve_combat(&mut self, since: Edition, out: &mut Vec<String>) -> Result<(), String> {
        let to = self.store().current();
        let mut events = Vec::new();
        for update in self
            .store()
            .scan_updates(self.r.combat_event, since, to)
            .map_err(err)?
        {
            if update.diff <= 0 || update.tuple.as_slice().first() != Some(&Value::Ent(PLAYER)) {
                continue;
            }
            if let (Some(monster), Some(Value::Text(outcome))) =
                (entity_at(&update.tuple, 1), update.tuple.as_slice().get(2))
            {
                events.push((update.tuple.clone(), monster, outcome.to_string()));
            }
        }
        for (event, monster, outcome) in events {
            // The event is an edge handoff, not durable combat state. Consume
            // exactly the positive weight the behavior asserted.
            self.store()
                .commit(&[(self.r.combat_event, event, -1)])
                .map_err(err)?;
            if outcome == "defeated" {
                self.award_victory(monster, out)?;
            } else {
                let old_hp = self.entity_int(self.r.hp, monster).unwrap_or(0);
                out.push(format!("{} remains at {old_hp} HP.", self.name_of(monster)));
                self.counterattack(monster, out)?;
            }
        }
        Ok(())
    }

    fn award_victory(&mut self, monster: Entity, out: &mut Vec<String>) -> Result<(), String> {
        if self.read_entity_set(self.r.defeated).contains(&monster) {
            out.push(format!("{} is already defeated.", self.name_of(monster)));
            return Ok(());
        }
        let reward_row = live_row(self.store(), self.r.monster_reward, |tuple| {
            tuple.as_slice().first() == Some(&Value::Ent(monster))
        });
        let Some(reward_row) = reward_row else {
            out.push(format!(
                "{} has no unclaimed reward.",
                self.name_of(monster)
            ));
            return Ok(());
        };
        let reward = int_at(&reward_row, 1);
        let job = self
            .entity_entity(self.r.active_job, PLAYER)
            .ok_or_else(|| "player has no active job".to_string())?;
        let rank = self
            .keyed_int(self.r.job_rank, PLAYER, job, 2)
            .ok_or_else(|| "active job has no rank".to_string())?;
        let points = self
            .keyed_int(self.r.job_points, PLAYER, job, 2)
            .ok_or_else(|| "active job has no point row".to_string())?;
        let progression = live_row(self.store(), self.r.job_progression, |tuple| {
            tuple.as_slice().first() == Some(&Value::Ent(job))
                && tuple.as_slice().get(1) == Some(&Value::Int(rank))
                && tuple.as_slice().get(2) == Some(&Value::Int(points))
                && tuple.as_slice().get(3) == Some(&Value::Int(reward))
        })
        .ok_or_else(|| {
            format!("no progression rule for rank {rank}, points {points}, reward {reward}")
        })?;
        let new_rank = int_at(&progression, 4);
        let new_points = int_at(&progression, 5);
        let ability =
            entity_at(&progression, 6).ok_or_else(|| "progression has no ability".to_string())?;
        let old_ability_rank = int_at(&progression, 7);
        let new_ability_rank = int_at(&progression, 8);

        let job_rank_old = Fact::new(
            self.r.job_rank,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(job), Value::Int(rank)]),
        );
        let job_points_old = Fact::new(
            self.r.job_points,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(job), Value::Int(points)]),
        );
        let ability_old = Fact::new(
            self.r.ability_rank,
            Tuple::from([
                Value::Ent(PLAYER),
                Value::Ent(ability),
                Value::Int(old_ability_rank),
            ]),
        );
        let monster_zero = Fact::new(self.r.hp, Tuple::from([Value::Ent(monster), Value::Int(0)]));
        let reward_claim = Fact::new(self.r.monster_reward, reward_row);
        let mut patch = Patch::new()
            .expect(monster_zero)
            .expect(reward_claim.clone())
            .expect(job_rank_old.clone())
            .expect(job_points_old.clone())
            .expect(ability_old.clone())
            .retract(job_rank_old)
            .retract(job_points_old)
            .retract(ability_old)
            .retract(reward_claim)
            .assert(Fact::new(
                self.r.job_rank,
                Tuple::from([Value::Ent(PLAYER), Value::Ent(job), Value::Int(new_rank)]),
            ))
            .assert(Fact::new(
                self.r.job_points,
                Tuple::from([Value::Ent(PLAYER), Value::Ent(job), Value::Int(new_points)]),
            ))
            .assert(Fact::new(
                self.r.ability_rank,
                Tuple::from([
                    Value::Ent(PLAYER),
                    Value::Ent(ability),
                    Value::Int(new_ability_rank),
                ]),
            ))
            .assert(Fact::new(
                self.r.defeated,
                Tuple::from([Value::Ent(monster)]),
            ));

        if let Some((shift, _)) = self.instance_info() {
            if monster == shifted(LAST_CUSTOMER, shift) {
                let vault = shifted(LEDGER_VAULT, shift);
                let chamber = shifted(MIRROR_CHAMBER, shift);
                patch = patch
                    .assert(Fact::new(
                        self.r.exits,
                        Tuple::from([Value::Ent(vault), Value::text("east"), Value::Ent(chamber)]),
                    ))
                    .assert(Fact::new(
                        self.r.exits,
                        Tuple::from([Value::Ent(chamber), Value::text("west"), Value::Ent(vault)]),
                    ));
            }
        }

        match commit_patch(
            self.store(),
            self.store(),
            &patch,
            &coordinator_authority(self.r),
        )
        .map_err(err)?
        {
            CommitOutcome::Rejected => {
                out.push("The victory was claimed by another turn first.".into());
                return Ok(());
            }
            CommitOutcome::Committed(_) => {}
        }
        out.push(format!(
            "{} collapses. +{reward} job point.",
            self.name_of(monster)
        ));
        if new_rank > rank {
            out.push(format!(
                "Your {} job reaches rank {new_rank}; {} rises to rank {new_ability_rank}.",
                self.name_of(job),
                self.name_of(ability)
            ));
            self.heal_to_job_max()?;
        }
        if monster == self.shifted_boss() {
            out.push(
                "The chained ledgers open. An east passage appears where the wall was.".into(),
            );
        }
        Ok(())
    }

    fn counterattack(&mut self, monster: Entity, out: &mut Vec<String>) -> Result<(), String> {
        let power = self.entity_int(self.r.monster_power, monster).unwrap_or(1);
        let (_, _, _, _, _, guard, _) = self
            .current_job()
            .ok_or_else(|| "player has no combat stats".to_string())?;
        let old_hp = self.entity_int(self.r.hp, PLAYER).unwrap_or(0);
        let result = self
            .attack_result(power, guard, old_hp)
            .ok_or_else(|| format!("no counterattack rule for {power}/{guard}/{old_hp}"))?;
        let (new_hp, outcome) = result;
        let old = Fact::new(
            self.r.hp,
            Tuple::from([Value::Ent(PLAYER), Value::Int(old_hp)]),
        );
        let patch = Patch::new()
            .expect(old.clone())
            .retract(old)
            .assert(Fact::new(
                self.r.hp,
                Tuple::from([Value::Ent(PLAYER), Value::Int(new_hp)]),
            ));
        if matches!(
            commit_patch(
                self.store(),
                self.store(),
                &patch,
                &coordinator_authority(self.r)
            )
            .map_err(err)?,
            CommitOutcome::Rejected
        ) {
            return Err("player HP changed during counterattack".into());
        }
        out.push(format!(
            "{} strikes back. Your HP falls to {new_hp}.",
            self.name_of(monster)
        ));
        if outcome == "defeated" {
            out.push("The basement folds shut around you.".into());
            out.extend(self.leave_dungeon(true)?);
        }
        Ok(())
    }

    fn attack_result(&self, power: i64, guard: i64, old_hp: i64) -> Option<(i64, String)> {
        live_row(self.store(), self.r.attack_result, |tuple| {
            tuple.as_slice().first() == Some(&Value::Int(power))
                && tuple.as_slice().get(1) == Some(&Value::Int(guard))
                && tuple.as_slice().get(2) == Some(&Value::Int(old_hp))
        })
        .map(|tuple| (int_at(&tuple, 3), text_at(&tuple, 4)))
    }

    fn shifted_boss(&self) -> Entity {
        self.instance_info()
            .map(|(shift, _)| shifted(LAST_CUSTOMER, shift))
            .unwrap_or(LAST_CUSTOMER)
    }

    fn deal(&mut self) -> Result<Vec<String>, String> {
        if self.room_of(PLAYER) != Some(KISSATEN) {
            return Ok(vec!["The cribbage table is in Tsukikage Kissaten.".into()]);
        }
        let old_state = self.entity_int(self.r.rng_state, WORLD).unwrap_or(1);
        let mut state = old_state as u64;
        let mut deck: Vec<i64> = (0..52).collect();
        for index in (1..deck.len()).rev() {
            let chosen = (next_random(&mut state) % (index as u64 + 1)) as usize;
            deck.swap(index, chosen);
        }
        let rng_old = Fact::new(
            self.r.rng_state,
            Tuple::from([Value::Ent(WORLD), Value::Int(old_state)]),
        );
        let mut patch = Patch::new()
            .expect(rng_old.clone())
            .retract(rng_old)
            .assert(Fact::new(
                self.r.rng_state,
                Tuple::from([Value::Ent(WORLD), Value::Int(state as i64)]),
            ));
        for (tuple, diff) in self
            .store()
            .read_at(self.r.card, self.store().current())
            .map_err(err)?
        {
            if diff > 0 && tuple.as_slice().first() == Some(&Value::Ent(PLAYER)) {
                patch = patch.retract(Fact::new(self.r.card, tuple));
            }
        }
        for (slot, code) in deck.iter().take(5).enumerate() {
            patch = patch.assert(Fact::new(
                self.r.card,
                Tuple::from([
                    Value::Ent(PLAYER),
                    Value::Int(slot as i64),
                    Value::Int(*code),
                ]),
            ));
        }
        match commit_patch(
            self.store(),
            self.store(),
            &patch,
            &coordinator_authority(self.r),
        )
        .map_err(err)?
        {
            CommitOutcome::Committed(_) => Ok(vec![
                "Sachiko deals five cards. `cards` to see them; `score` to count the views.".into(),
            ]),
            CommitOutcome::Rejected => Ok(vec![
                "The deck moved under Sachiko's hand; deal again.".into()
            ]),
        }
    }

    fn hand(&self) -> HashMap<i64, i64> {
        let mut hand = HashMap::new();
        for (tuple, diff) in self
            .store()
            .read_at(self.r.card, self.store().current())
            .unwrap_or_default()
        {
            if diff > 0 && tuple.as_slice().first() == Some(&Value::Ent(PLAYER)) {
                hand.insert(int_at(&tuple, 1), int_at(&tuple, 2));
            }
        }
        hand
    }

    fn cards(&self) -> Vec<String> {
        let hand = self.hand();
        if hand.is_empty() {
            return vec!["You have no cards. Type `deal` in the kissaten.".into()];
        }
        let cards = (0..4)
            .filter_map(|slot| hand.get(&slot).map(|card| card_name(*card)))
            .collect::<Vec<_>>()
            .join(" ");
        let starter = hand
            .get(&4)
            .map(|card| card_name(*card))
            .unwrap_or_else(|| "?".into());
        vec![format!("Hand: {cards}   |   starter: {starter}")]
    }

    fn score(&self) -> Vec<String> {
        if self.hand().len() < 5 {
            return vec!["Deal a full hand first.".into()];
        }
        let pairs = self
            .view_rows("crib_pairs", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .len();
        let fifteens = self
            .view_rows("crib_fif2", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .len()
            + self
                .view_rows("crib_fif3", &[Value::Ent(PLAYER)])
                .unwrap_or_default()
                .len();
        let mut out = self.cards();
        out.push(format!(
            "Fifteens: {fifteens} × 2; pairs: {pairs} × 2; total: {}.",
            2 * (fifteens + pairs)
        ));
        out
    }
}

impl Game {
    fn start_watch(&mut self) -> Result<Vec<String>, String> {
        if self.watch.is_some() {
            return Ok(vec!["Already watching this branch.".into()]);
        }
        let authority = Authority::new(
            DomainId(1),
            vec![
                Scope::whole(self.r.wmail),
                Scope::whole(self.r.wcursor),
                Scope::whole(self.r.wseq),
            ],
        );
        let (watch, _) = self
            .program
            .install_watch(
                "world",
                &[],
                PLAYER,
                PLAYER,
                authority,
                self.store(),
                self.store(),
            )
            .map_err(err)?;
        watch.pump(self.store(), self.store()).map_err(err)?;
        self.delivered = self.count_for(self.r.wmail, PLAYER);
        self.watch = Some(watch);
        Ok(vec![format!(
            "Watching branch {} — world changes will stream as signed deltas.",
            self.family.active_branch()
        )])
    }

    fn pump_watch(&mut self, out: &mut Vec<String>) -> Result<(), String> {
        let Some(watch) = &self.watch else {
            return Ok(());
        };
        watch.pump(self.store(), self.store()).map_err(err)?;
        let mut rows = Vec::new();
        for (tuple, diff) in self
            .store()
            .read_at(self.r.wmail, self.store().current())
            .map_err(err)?
        {
            if diff <= 0 || tuple.as_slice().first() != Some(&Value::Ent(PLAYER)) {
                continue;
            }
            let seq = int_at(&tuple, 1);
            if seq < self.delivered as i64 {
                continue;
            }
            if let Some(Value::Tuple(body)) = tuple.as_slice().get(2) {
                if let Some((delta, row)) = decode_activation(&Tuple(body.clone())) {
                    rows.push((seq, delta, text_at(&row, 1)));
                }
            }
        }
        rows.sort();
        for (_, delta, name) in rows {
            out.push(format!(
                "  [world] {} {name}",
                if delta > 0 { "+" } else { "-" }
            ));
        }
        self.delivered = self.count_for(self.r.wmail, PLAYER);
        Ok(())
    }

    fn count_for(&self, rel: RelId, key: Entity) -> usize {
        self.store()
            .read_at(rel, self.store().current())
            .unwrap_or_default()
            .iter()
            .filter(|(tuple, diff)| *diff > 0 && tuple.as_slice().first() == Some(&Value::Ent(key)))
            .count()
    }

    fn enter_basement(&mut self, line: &str) -> Result<Vec<String>, String> {
        if line.split_whitespace().nth(1) != Some("basement") {
            return Ok(vec!["The only special entrance is `enter basement`.".into()]);
        }
        if self.instance_info().is_some() {
            return Ok(vec![
                "You are already below the arcade. `leave` first.".into()
            ]);
        }
        if self.room_of(PLAYER) != Some(CINEMA) {
            return Ok(vec![
                "The freight lift is inside the shuttered cinema.".into()
            ]);
        }
        let old_index = self
            .entity_int(self.r.instance_counter, PLAYER)
            .unwrap_or(0);
        let counter_old = Fact::new(
            self.r.instance_counter,
            Tuple::from([Value::Ent(PLAYER), Value::Int(old_index)]),
        );
        let counter_patch = Patch::new()
            .expect(counter_old.clone())
            .retract(counter_old)
            .assert(Fact::new(
                self.r.instance_counter,
                Tuple::from([Value::Ent(PLAYER), Value::Int(old_index + 1)]),
            ));
        if matches!(
            commit_patch(
                self.store(),
                self.store(),
                &counter_patch,
                &coordinator_authority(self.r),
            )
            .map_err(err)?,
            CommitOutcome::Rejected
        ) {
            return Err("dungeon instance counter changed concurrently".into());
        }

        let target_base = INSTANCE_BASE + old_index as u64 * INSTANCE_STRIDE;
        let shift = target_base as i64 - DUNGEON_BASE as i64;
        self.store()
            .instance_template(
                &self.r.template(),
                DUNGEON_BASE,
                DUNGEON_BASE + DUNGEON_SPAN,
                shift,
            )
            .map_err(err)?;
        let entry = shifted(DUNGEON_ENTRY, shift);
        let old_location = Fact::new(
            self.r.located,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(CINEMA)]),
        );
        let patch = Patch::new()
            .expect(old_location.clone())
            .retract(old_location)
            .assert(Fact::new(
                self.r.located,
                Tuple::from([Value::Ent(PLAYER), Value::Ent(entry)]),
            ))
            .assert(Fact::new(
                self.r.instance_state,
                Tuple::from([Value::Ent(PLAYER), Value::Int(shift), Value::Ent(CINEMA)]),
            ));
        if matches!(
            commit_patch(
                self.store(),
                self.store(),
                &patch,
                &coordinator_authority(self.r)
            )
            .map_err(err)?,
            CommitOutcome::Rejected
        ) {
            return Err("the player moved while the dungeon was being instanced".into());
        }
        let mut out = vec![
            "The freight lift descends much farther than the building is tall.".into(),
            "A private DSP-relocated dungeon resolves around you.".into(),
        ];
        out.extend(self.look());
        self.pump_watch(&mut out)?;
        Ok(out)
    }

    fn instance_info(&self) -> Option<(i64, Entity)> {
        live_row(self.store(), self.r.instance_state, |tuple| {
            tuple.as_slice().first() == Some(&Value::Ent(PLAYER))
        })
        .and_then(|tuple| Some((int_at(&tuple, 1), entity_at(&tuple, 2)?)))
    }

    fn leave_dungeon(&mut self, defeated_player: bool) -> Result<Vec<String>, String> {
        let Some((shift, home)) = self.instance_info() else {
            if defeated_player {
                self.recover_on_surface()?;
                let mut out = vec!["You wake in Kasumi Sento.".into()];
                out.extend(self.look());
                return Ok(out);
            }
            return Ok(vec!["You are not inside a basement instance.".into()]);
        };
        let low = DUNGEON_BASE.wrapping_add(shift as u64);
        let high = low + DUNGEON_SPAN;
        let in_block = |entity: Entity| entity.0 >= low && entity.0 < high;
        let at = self.store().current();
        let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for rel in self.r.template() {
            for (tuple, diff) in self.store().read_at(rel, at).map_err(err)? {
                if diff > 0
                    && matches!(tuple.as_slice().first(), Some(Value::Ent(entity)) if in_block(*entity))
                {
                    updates.push((rel, tuple, -diff));
                }
            }
        }
        for (tuple, diff) in self.store().read_at(self.r.held, at).map_err(err)? {
            if diff > 0
                && matches!(tuple.as_slice().get(1), Some(Value::Ent(entity)) if in_block(*entity))
            {
                updates.push((self.r.held, tuple, -diff));
            }
        }
        let current = self
            .room_of(PLAYER)
            .unwrap_or(shifted(DUNGEON_ENTRY, shift));
        updates.push((
            self.r.located,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(current)]),
            -1,
        ));
        let destination = if defeated_player { SENTO } else { home };
        updates.push((
            self.r.located,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(destination)]),
            1,
        ));
        updates.push((
            self.r.instance_state,
            Tuple::from([Value::Ent(PLAYER), Value::Int(shift), Value::Ent(home)]),
            -1,
        ));
        if defeated_player {
            let old_hp = self.entity_int(self.r.hp, PLAYER).unwrap_or(0);
            let max_hp = self.current_job().map(|row| row.6).unwrap_or(20);
            updates.push((
                self.r.hp,
                Tuple::from([Value::Ent(PLAYER), Value::Int(old_hp)]),
                -1,
            ));
            updates.push((
                self.r.hp,
                Tuple::from([Value::Ent(PLAYER), Value::Int(max_hp)]),
                1,
            ));
        }
        self.store().commit(&updates).map_err(err)?;
        let mut out = if defeated_player {
            vec![
                "Steam and white tile replace the impossible basement. Your job progress remains."
                    .into(),
            ]
        } else {
            vec!["The lift returns you to the cinema. The private basement folds away.".into()]
        };
        out.extend(self.look());
        self.pump_watch(&mut out)?;
        Ok(out)
    }

    fn recover_on_surface(&self) -> Result<(), String> {
        let current = self.room_of(PLAYER).unwrap_or(EAST_GATE);
        let old_hp = self.entity_int(self.r.hp, PLAYER).unwrap_or(0);
        let max_hp = self.current_job().map(|row| row.6).unwrap_or(20);
        self.store()
            .commit(&[
                (
                    self.r.located,
                    Tuple::from([Value::Ent(PLAYER), Value::Ent(current)]),
                    -1,
                ),
                (
                    self.r.located,
                    Tuple::from([Value::Ent(PLAYER), Value::Ent(SENTO)]),
                    1,
                ),
                (
                    self.r.hp,
                    Tuple::from([Value::Ent(PLAYER), Value::Int(old_hp)]),
                    -1,
                ),
                (
                    self.r.hp,
                    Tuple::from([Value::Ent(PLAYER), Value::Int(max_hp)]),
                    1,
                ),
            ])
            .map_err(err)?;
        Ok(())
    }

    fn activate_mirror(&mut self, line: &str) -> Result<Vec<String>, String> {
        if line.split_whitespace().nth(1) != Some("mirror") {
            return Ok(vec!["The artifact command is `activate mirror`.".into()]);
        }
        let Some((shift, _)) = self.instance_info() else {
            return Ok(vec!["There is no copying mirror here.".into()]);
        };
        let chamber = shifted(MIRROR_CHAMBER, shift);
        let boss = shifted(LAST_CUSTOMER, shift);
        if self.room_of(PLAYER) != Some(chamber) {
            return Ok(vec!["There is no copying mirror here.".into()]);
        }
        if !self.read_entity_set(self.r.defeated).contains(&boss) {
            return Ok(vec![
                "The mirror remains blank while the Last Customer stands.".into(),
            ]);
        }

        let old_depth = self.echo_depth();
        let parent_branch = self.family.active_branch();
        let cut = self.store().current();
        let child = self.store().fork_at(cut).map_err(err)?;
        for rel in self.r.all() {
            let parent_rows = self.store().read_at(rel, cut).map_err(err)?;
            let child_rows = child.read_at(rel, child.current()).map_err(err)?;
            if parent_rows != child_rows {
                return Err(format!("fork identity failed for relation {}", rel.0));
            }
        }

        let old_echo = Tuple::from([Value::Ent(WORLD), Value::Int(old_depth)]);
        let new_echo = Tuple::from([Value::Ent(WORLD), Value::Int(old_depth + 1)]);
        child
            .commit_if(
                &[(self.r.echo_depth, old_echo.clone())],
                &[
                    (self.r.echo_depth, old_echo, -1),
                    (self.r.echo_depth, new_echo, 1),
                ],
            )
            .map_err(err)?
            .ok_or_else(|| "the echo marker changed during the fork".to_string())?;
        let child_branch = child.branch_id();
        self.family.route_to(child, self.r)?;
        // The canopy registration is branch-local. The durable cursor was
        // copied, but the player opts back into the new branch's live feed.
        self.watch = None;
        self.delivered = 0;

        Ok(vec![
            "You touch the mirror. The complete world copies without moving a grain of dust.".into(),
            format!(
                "Your session leaves branch {parent_branch} at edition {} and wakes in child branch {child_branch}.",
                cut.0
            ),
            format!("This is echo world {}. The parent will no longer hear your steps.", old_depth + 1),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_game() -> (tempfile::TempDir, Game) {
        let dir = tempfile::tempdir().unwrap();
        let game = Game::open(dir.path()).unwrap();
        (dir, game)
    }

    fn run(game: &mut Game, command: &str) -> Vec<String> {
        game.execute(command).unwrap()
    }

    fn go_to_cooperative(game: &mut Game) {
        run(game, "go north");
        run(game, "go north");
        assert_eq!(game.room_of(PLAYER), Some(COOPERATIVE));
    }

    fn go_to_cinema(game: &mut Game) {
        go_to_cooperative(game);
        run(game, "go west");
        assert_eq!(game.room_of(PLAYER), Some(CINEMA));
    }

    fn reach_mirror(game: &mut Game) {
        go_to_cooperative(game);
        run(game, "change repairer");
        run(game, "go west");
        run(game, "enter basement");
        run(game, "go north");
        run(game, "use live");
        run(game, "use live");
        run(game, "go north");
        run(game, "use live");
        run(game, "use live");
        run(game, "use live");
        run(game, "go north");
        let (shift, _) = game.instance_info().unwrap();
        assert_eq!(
            game.room_of(PLAYER),
            Some(shifted(MANNEQUIN_STOCKROOM, shift))
        );
        assert!(game
            .contents(PLAYER)
            .contains(&shifted(MANNEQUIN_SHELL, shift)));
        run(game, "use live");
        run(game, "use live");
        assert!(game
            .read_entity_set(game.r.defeated)
            .contains(&shifted(MANNEQUIN_SHELL, shift)));
        run(game, "go north");
        run(game, "use live");
        run(game, "use live");
        run(game, "use live");
        run(game, "go east");
        let (shift, _) = game.instance_info().unwrap();
        assert_eq!(game.room_of(PLAYER), Some(shifted(MIRROR_CHAMBER, shift)));
        assert!(game
            .read_entity_set(game.r.defeated)
            .contains(&shifted(LAST_CUSTOMER, shift)));
    }

    #[test]
    fn language_surface_compiles_and_resolves() {
        let program = Program::compile(WORLD_SOURCE, 1).unwrap();
        let rels = Rels::resolve(&program).unwrap();
        assert!(rels.all().len() >= 40);
        for (view, args) in [
            ("here", &[Value::Ent(PLAYER)][..]),
            ("inventory", &[Value::Ent(PLAYER)]),
            ("ways", &[Value::Ent(PLAYER)]),
            ("folks", &[Value::Ent(PLAYER)]),
            ("acquainted", &[Value::Ent(PLAYER)]),
            ("current_job", &[Value::Ent(PLAYER)]),
            ("abilities", &[Value::Ent(PLAYER)]),
            ("ability_targets", &[Value::Ent(PLAYER)]),
            ("enemies", &[Value::Ent(PLAYER)]),
            ("treasure", &[]),
            ("world", &[]),
            ("crib_pairs", &[Value::Ent(PLAYER)]),
            ("crib_fif2", &[Value::Ent(PLAYER)]),
            ("crib_fif3", &[Value::Ent(PLAYER)]),
        ] {
            assert!(
                program.view(view, args).is_ok(),
                "view `{view}` did not resolve"
            );
        }
        assert!(program.form("command").is_ok());
        assert!(Program::behavior(&Arc::new(program), "inbox", PLAYER).is_ok());
    }

    #[test]
    fn surface_world_retains_the_manor_feature_set() {
        let (_dir, mut game) = fresh_game();
        assert!(game.look().join("\n").contains("East Gate"));
        assert!(game.look().join("\n").contains("a calico cat"));
        let greeting = run(&mut game, "greet cat").join("\n");
        assert!(greeting.contains("Mugi"));
        assert!(greeting.contains("slips beneath a shutter"), "{greeting}");
        assert!(run(&mut game, "take umbrella").join("\n").contains("Taken"));
        assert!(game.inventory().join("\n").contains("red umbrella"));
        assert!(run(&mut game, "say hello").join("\n").contains("hello"));
        assert!(run(&mut game, "drop umbrella")
            .join("\n")
            .contains("Dropped"));
        assert!(!game.inventory().join("\n").contains("red umbrella"));

        // Reactive stream: taking an object changes the language-defined world view.
        let (_watch_dir, mut watched) = fresh_game();
        run(&mut watched, "watch");
        let deltas = run(&mut watched, "take umbrella").join("\n");
        assert!(deltas.contains("[world] - red umbrella"), "{deltas}");

        // Aggregate, live-report room, and relation-scored card game.
        let (_card_dir, mut cards) = fresh_game();
        run(&mut cards, "go north");
        run(&mut cards, "go east");
        assert!(run(&mut cards, "deal")
            .join("\n")
            .contains("deals five cards"));
        assert!(run(&mut cards, "score").join("\n").contains("total:"));
        run(&mut cards, "go up");
        assert!(cards.look().join("\n").contains("live report"));
        assert!(cards.treasure().join("\n").contains("sum aggregate"));
        assert!(cards.who().join("\n").contains("branch 0"));
    }

    #[test]
    fn jobs_retain_progress_and_upgrade_abilities() {
        let (_dir, mut game) = fresh_game();
        go_to_cooperative(&mut game);
        run(&mut game, "change repairer");
        assert_eq!(
            game.entity_entity(game.r.active_job, PLAYER),
            Some(REPAIRER_JOB)
        );
        run(&mut game, "go west");
        run(&mut game, "enter basement");
        run(&mut game, "go north");
        run(&mut game, "use live");
        run(&mut game, "use live");
        assert_eq!(
            game.keyed_int(game.r.job_points, PLAYER, REPAIRER_JOB, 2),
            Some(1)
        );
        let (shift, _) = game.instance_info().unwrap();
        assert_eq!(
            game.entity_int(game.r.monster_reward, shifted(RECEIPT_MOTH, shift)),
            None,
            "victory must consume the monster's one-shot reward claim"
        );
        // A zero-HP monster can still be named by the positive-only view, but
        // the durable `defeated` fact makes its reward idempotent.
        run(&mut game, "use live");
        assert_eq!(
            game.keyed_int(game.r.job_points, PLAYER, REPAIRER_JOB, 2),
            Some(1)
        );
        run(&mut game, "go north");
        run(&mut game, "use live");
        run(&mut game, "use live");
        run(&mut game, "use live");
        assert_eq!(
            game.keyed_int(game.r.job_rank, PLAYER, REPAIRER_JOB, 2),
            Some(2)
        );
        assert_eq!(
            game.keyed_int(game.r.ability_rank, PLAYER, LIVE_WIRE, 2),
            Some(2)
        );

        run(&mut game, "leave");
        run(&mut game, "go east");
        run(&mut game, "change cook");
        assert_eq!(game.entity_entity(game.r.active_job, PLAYER), Some(COOK));
        assert_eq!(
            game.keyed_int(game.r.job_rank, PLAYER, REPAIRER_JOB, 2),
            Some(2)
        );
        assert_eq!(
            game.keyed_int(game.r.ability_rank, PLAYER, LIVE_WIRE, 2),
            Some(2)
        );
    }

    #[test]
    fn dungeon_instances_are_disjoint_and_leaving_cleans_them() {
        let (_dir, mut game) = fresh_game();
        go_to_cinema(&mut game);
        run(&mut game, "enter basement");
        let (first_shift, _) = game.instance_info().unwrap();
        let first_entry = shifted(DUNGEON_ENTRY, first_shift);
        assert_eq!(game.room_of(PLAYER), Some(first_entry));
        run(&mut game, "leave");
        assert!(live_entity_text(game.store(), game.r.named, first_entry).is_none());

        run(&mut game, "enter basement");
        let (second_shift, _) = game.instance_info().unwrap();
        assert_ne!(first_shift, second_shift);
        assert!(live_entity_text(
            game.store(),
            game.r.named,
            shifted(DUNGEON_ENTRY, second_shift)
        )
        .is_some());
        assert!(live_entity_text(game.store(), game.r.named, first_entry).is_none());
    }

    #[test]
    fn stale_combat_patches_have_exactly_one_winner() {
        let (_dir, mut game) = fresh_game();
        go_to_cinema(&mut game);
        run(&mut game, "enter basement");
        run(&mut game, "go north");
        let before_hp = game
            .entity_int(
                game.r.hp,
                shifted(RECEIPT_MOTH, game.instance_info().unwrap().0),
            )
            .unwrap();
        let at = game.store().current();
        let snapshot = Snapshot::new(game.store(), at);
        let behavior_a = Program::behavior(&game.program, "inbox", PLAYER).unwrap();
        let behavior_b = Program::behavior(&game.program, "inbox", PLAYER).unwrap();
        let patch_a = behavior_a(&snapshot, &tokens("attack moth")).unwrap();
        let patch_b = behavior_b(&snapshot, &tokens("attack moth")).unwrap();
        let first = commit_patch(
            game.store(),
            game.store(),
            &patch_a,
            &player_authority(game.r),
        )
        .unwrap();
        let second = commit_patch(
            game.store(),
            game.store(),
            &patch_b,
            &player_authority(game.r),
        )
        .unwrap();
        assert!(matches!(first, CommitOutcome::Committed(_)));
        assert_eq!(second, CommitOutcome::Rejected);
        let monster = shifted(RECEIPT_MOTH, game.instance_info().unwrap().0);
        assert_eq!(game.entity_int(game.r.hp, monster), Some(before_hp - 4));
    }

    #[test]
    fn player_defeat_returns_to_the_sento_without_losing_job_progress() {
        let (_dir, mut game) = fresh_game();
        go_to_cinema(&mut game);
        run(&mut game, "enter basement");
        run(&mut game, "go north");
        let hp = game.entity_int(game.r.hp, PLAYER).unwrap();
        game.store()
            .commit(&[
                (
                    game.r.hp,
                    Tuple::from([Value::Ent(PLAYER), Value::Int(hp)]),
                    -1,
                ),
                (
                    game.r.hp,
                    Tuple::from([Value::Ent(PLAYER), Value::Int(1)]),
                    1,
                ),
            ])
            .unwrap();
        let result = run(&mut game, "attack moth").join("\n");
        assert!(
            result.contains("Kasumi Sento") || result.contains("white tile"),
            "{result}"
        );
        assert_eq!(game.room_of(PLAYER), Some(SENTO));
        assert!(game.instance_info().is_none());
        assert_eq!(game.entity_int(game.r.hp, PLAYER), Some(20));
        assert_eq!(game.keyed_int(game.r.job_rank, PLAYER, COURIER, 2), Some(1));
    }

    #[test]
    fn committed_script_replays_to_the_same_logical_world() {
        fn play(game: &mut Game) {
            for command in [
                "greet cat",
                "take umbrella",
                "go north",
                "go east",
                "deal",
                "go west",
                "go north",
                "change repairer",
                "go west",
                "enter basement",
                "go north",
                "use live",
            ] {
                run(game, command);
            }
        }

        let (_a_dir, mut a) = fresh_game();
        let (_b_dir, mut b) = fresh_game();
        play(&mut a);
        play(&mut b);
        assert_eq!(a.store().current(), b.store().current());
        for rel in a.r.all() {
            assert_eq!(
                a.store().read_at(rel, a.store().current()).unwrap(),
                b.store().read_at(rel, b.store().current()).unwrap(),
                "replay diverged in relation {}",
                rel.0
            );
        }
    }

    #[test]
    fn mirror_forks_recursively_and_route_survives_reopen() {
        let (dir, mut game) = fresh_game();
        reach_mirror(&mut game);
        let parent_room = game.room_of(PLAYER).unwrap();
        let cut = game.store().current();
        let first = run(&mut game, "activate mirror").join("\n");
        assert!(first.contains("child branch 1"), "{first}");
        assert_eq!(game.family.active_branch(), 1);
        assert_eq!(game.echo_depth(), 1);
        assert!(game.store().descends_from(&game.family.root, cut));

        // Diverge the child by walking west. The parent remains in the mirror chamber.
        run(&mut game, "go west");
        assert_ne!(game.room_of(PLAYER), Some(parent_room));
        assert_eq!(
            live_row(&game.family.root, game.r.located, |tuple| {
                tuple.as_slice().first() == Some(&Value::Ent(PLAYER))
            })
            .and_then(|tuple| entity_at(&tuple, 1)),
            Some(parent_room)
        );
        run(&mut game, "go east");
        drop(game);

        let mut reopened = Game::open(dir.path()).unwrap();
        assert_eq!(reopened.family.active_branch(), 1);
        assert_eq!(reopened.echo_depth(), 1);
        let second = run(&mut reopened, "activate mirror").join("\n");
        assert!(second.contains("child branch 2"), "{second}");
        assert_eq!(reopened.echo_depth(), 2);
        assert_eq!(
            reopened
                .store()
                .dag()
                .lineage(2, reopened.store().current().0)
                .len(),
            3
        );
    }
}
