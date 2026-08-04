//! `grmpl shotengai [STORE_DIR]` — the durable Kasumi Shotengai game.
//!
//! The world package, initial conditions, finite rule tables, player combat,
//! and scalar omen RNG live in `worlds/shotengai.grmpl`. This edge host owns
//! terminal I/O, actor driving, card shuffling, DSP instancing, presentation,
//! and whole-store branch routing.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use grmpl::{DriveStatus, NamedAuthority, NamedScope, Runtime, RuntimePolicy};
use grmpl_core::{
    Authority, Diff, DomainId, Edition, EditionStore, Entity, Fact, Patch, RelId, Scope,
    TraceStore, Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_ent::{BranchId, Dag, EntStore};
#[cfg(test)]
use grmpl_lang::ResolvedGrantSet;
use grmpl_lang::{GrantSet, Program};
use grmpl_proc::{commit_patch, decode_activation, CommitOutcome, OnWatch};

const WORLD_SOURCE: &str = include_str!("../../../worlds/shotengai.grmpl");

const PLAYER: Entity = Entity(1);
const WORLD: Entity = Entity(2);

// Surface rooms.
const EAST_GATE: Entity = Entity(10);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const ARCADE: Entity = Entity(11);
const KISSATEN: Entity = Entity(12);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const REPAIR_SHOP: Entity = Entity(13);
const SENTO: Entity = Entity(14);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const COOPERATIVE: Entity = Entity(15);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const SHRINE_ALLEY: Entity = Entity(16);
const CINEMA: Entity = Entity(17);
const ROOFTOP: Entity = Entity(18);

// Surface things and residents.
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const RED_UMBRELLA: Entity = Entity(20);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const BRASS_TOKEN: Entity = Entity(21);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const OLD_RADIO: Entity = Entity(22);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const TEA_TIN: Entity = Entity(23);
#[allow(dead_code)] // used by the headless time-driver regression
const CAT: Entity = Entity(30);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const PROPRIETOR: Entity = Entity(31);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const REPAIRER: Entity = Entity(32);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const CARETAKER: Entity = Entity(33);
#[cfg(test)]
const COMBAT: Entity = Entity(34);

// Jobs and their signature abilities.
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const COURIER: Entity = Entity(100);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const COOK: Entity = Entity(101);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const REPAIRER_JOB: Entity = Entity(102);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const SHRINE_ATTENDANT: Entity = Entity(103);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const NIGHT_WATCH: Entity = Entity(104);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const QUICK_STEP: Entity = Entity(200);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const SEARING_PAN: Entity = Entity(201);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const LIVE_WIRE: Entity = Entity(202);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const PAPER_WARD: Entity = Entity(203);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const LANTERN_WALL: Entity = Entity(204);

// The self-contained dungeon template. Every template fact is keyed by an
// entity in this block, allowing `instance_template` to relocate all entity
// coordinates together.
const DUNGEON_BASE: u64 = 1_000;
const DUNGEON_SPAN: u64 = 100;
const DUNGEON_ENTRY: Entity = Entity(1_000);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const FLOODED_STORE: Entity = Entity(1_001);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const FORGOTTEN_ARCADE: Entity = Entity(1_002);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const LEDGER_VAULT: Entity = Entity(1_003);
const MIRROR_CHAMBER: Entity = Entity(1_004);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const MANNEQUIN_STOCKROOM: Entity = Entity(1_005);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const RECEIPT_MOTH: Entity = Entity(1_010);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const SHUTTER_MAW: Entity = Entity(1_011);
const LAST_CUSTOMER: Entity = Entity(1_012);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const MANNEQUIN_SHELL: Entity = Entity(1_013);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const DUNGEON_COIN: Entity = Entity(1_020);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const RED_THREAD: Entity = Entity(1_021);
#[allow(dead_code)] // mirrors the package's stable source-level entity id
const COPYING_MIRROR: Entity = Entity(1_022);
const INSTANCE_BASE: u64 = 100_000;
const INSTANCE_STRIDE: u64 = 1_000;

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
    combat_event: RelId,
    combat_claimed: RelId,
    player_defeat: RelId,
    boss_unlock: RelId,
    session_branch: RelId,
    echo_depth: RelId,
    instance_state: RelId,
    instance_counter: RelId,
    rng_state: RelId,
    clock: RelId,
    timers: RelId,
    patrol_due: RelId,
    card: RelId,
    cardval: RelId,
    cardrank: RelId,
    slotpair: RelId,
    slottrio: RelId,
    sum15two: RelId,
    sum15three: RelId,
    tell: RelId,
    inbox: RelId,
    patrol_inbox: RelId,
    combat_inbox: RelId,
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
            combat_event: get("combat_event")?,
            combat_claimed: get("combat_claimed")?,
            player_defeat: get("player_defeat")?,
            boss_unlock: get("boss_unlock")?,
            session_branch: get("session_branch")?,
            echo_depth: get("echo_depth")?,
            instance_state: get("instance_state")?,
            instance_counter: get("instance_counter")?,
            rng_state: get("rng_state")?,
            clock: get("clock")?,
            timers: get("timers")?,
            patrol_due: get("patrol_due")?,
            card: get("card")?,
            cardval: get("cardval")?,
            cardrank: get("cardrank")?,
            slotpair: get("slotpair")?,
            slottrio: get("slottrio")?,
            sum15two: get("sum15two")?,
            sum15three: get("sum15three")?,
            tell: get("tell")?,
            inbox: get("inbox")?,
            patrol_inbox: get("patrol_inbox")?,
            combat_inbox: get("combat_inbox")?,
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
            self.combat_event,
            self.combat_claimed,
            self.player_defeat,
            self.boss_unlock,
            self.session_branch,
            self.echo_depth,
            self.instance_state,
            self.instance_counter,
            self.rng_state,
            self.clock,
            self.timers,
            self.patrol_due,
            self.card,
            self.cardval,
            self.cardrank,
            self.slotpair,
            self.slottrio,
            self.sum15two,
            self.sum15three,
            self.tell,
            self.inbox,
            self.patrol_inbox,
            self.combat_inbox,
            self.inbox_seq,
            self.cursor,
            self.wmail,
            self.wcursor,
            self.wseq,
        ]
    }

    fn template(self) -> [RelId; 13] {
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
            self.boss_unlock,
            self.combat_claimed,
        ]
    }
}

#[cfg(test)]
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
            Scope::whole(r.rng_state),
            Scope::whole(r.timers),
        ],
    )
}

#[cfg(test)]
fn combat_authority(r: Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(r.ability_rank),
            Scope::whole(r.combat_claimed),
            Scope::whole(r.combat_event),
            Scope::whole(r.defeated),
            Scope::whole(r.exits),
            Scope::whole(r.hp),
            Scope::whole(r.job_points),
            Scope::whole(r.job_rank),
            Scope::whole(r.monster_reward),
            Scope::whole(r.player_defeat),
            Scope::whole(r.tell),
        ],
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

fn actor_runtime_policy() -> Result<RuntimePolicy, String> {
    let capabilities = GrantSet::new()
        .grant_random("omens", "rng_state", WORLD, "xorshift64star_v1")?
        .grant_schedule(
            "world_clock",
            "clock",
            "timers",
            "inbox_seq",
            ["PLAYER", "CAT", "COMBAT"],
        )?;
    let named = |relations: &[&str]| {
        NamedAuthority::new(
            DomainId(1),
            relations.iter().copied().map(NamedScope::whole).collect(),
        )
    };
    let actor_authorities = std::collections::BTreeMap::from([
        (
            "PLAYER".into(),
            named(&[
                "active_job",
                "combat_event",
                "cursor",
                "held",
                "hp",
                "knows",
                "located",
                "rng_state",
                "tell",
                "timers",
            ]),
        ),
        (
            "CAT".into(),
            named(&["cursor", "located", "patrol_due", "timers"]),
        ),
        (
            "COMBAT".into(),
            named(&[
                "ability_rank",
                "combat_claimed",
                "combat_event",
                "cursor",
                "defeated",
                "exits",
                "hp",
                "job_points",
                "job_rank",
                "monster_reward",
                "player_defeat",
                "tell",
            ]),
        ),
    ]);
    Ok(RuntimePolicy::new(
        capabilities,
        actor_authorities,
        named(&[
            "clock",
            "timers",
            "inbox_seq",
            "inbox",
            "patrol_inbox",
            "combat_inbox",
        ]),
    ))
}

/// One granfilade family with the root control branch kept open and at most one
/// non-root branch selected for this single-player terminal session.
struct WorldFamily {
    root: Arc<EntStore>,
    active: Option<Arc<EntStore>>,
}

impl WorldFamily {
    fn open(root: Arc<EntStore>, r: Rels) -> Result<WorldFamily, String> {
        let branch = live_int_for(&root, r.session_branch, PLAYER, 1)
            .unwrap_or(Dag::ROOT as i64)
            .max(0) as BranchId;
        let active = if branch == Dag::ROOT {
            None
        } else {
            Some(Arc::new(root.branch(branch).map_err(err)?))
        };
        Ok(WorldFamily { root, active })
    }

    fn active(&self) -> &EntStore {
        self.active.as_ref().map_or(self.root.as_ref(), Arc::as_ref)
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
        self.active = Some(Arc::new(child));
        Ok(())
    }

    fn shared_active(&self) -> Arc<dyn grmpl_core::WorldStore> {
        match &self.active {
            Some(active) => active.clone(),
            None => self.root.clone(),
        }
    }
}

/// The headless game. The interactive terminal and the test suite both drive
/// this API, so the tested command path is the player-facing one.
struct Game {
    runtime: Arc<Runtime>,
    program: Arc<Program>,
    #[cfg(test)]
    grants: Arc<ResolvedGrantSet>,
    family: WorldFamily,
    r: Rels,
    watch: Option<OnWatch>,
    delivered: usize,
}

impl Game {
    fn open(path: &Path) -> Result<Game, String> {
        let root =
            Arc::new(EntStore::open(path).map_err(|e| {
                format!("cannot open shotengai store at {}: {e:?}", path.display())
            })?);
        let store: Arc<dyn grmpl_core::WorldStore> = root.clone();
        let runtime =
            Runtime::load_driven_package(store, WORLD_SOURCE, 1, &actor_runtime_policy()?)?;
        let program = Arc::clone(runtime.program());
        let r = Rels::resolve(&program)?;

        let family = WorldFamily::open(root, r)?;
        let runtime = if family.active_branch() == Dag::ROOT {
            runtime
        } else {
            Runtime::load_driven_package(
                family.shared_active(),
                WORLD_SOURCE,
                1,
                &actor_runtime_policy()?,
            )?
        };
        let program = Arc::clone(runtime.program());
        Ok(Game {
            runtime: Arc::clone(&runtime),
            program,
            #[cfg(test)]
            grants: runtime.shared_grants(),
            family,
            r,
            watch: None,
            delivered: 0,
        })
    }

    fn store(&self) -> &EntStore {
        self.family.active()
    }

    fn rebind_runtime(&mut self) -> Result<(), String> {
        self.runtime = Runtime::load_driven_package(
            self.family.shared_active(),
            WORLD_SOURCE,
            1,
            &actor_runtime_policy()?,
        )?;
        self.program = Arc::clone(self.runtime.program());
        #[cfg(test)]
        {
            self.grants = self.runtime.shared_grants();
        }
        Ok(())
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

    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        loop {
            let mut line = String::new();
            let message = match stdin.read_line(&mut line) {
                Ok(0) => Ok(None),
                Ok(_) => Ok(Some(line)),
                Err(error) => Err(format!("stdin: {error}")),
            };
            let done = !matches!(message, Ok(Some(_)));
            if send.send(message).is_err() || done {
                break;
            }
        }
    });
    print!("\n> ");
    io::stdout().flush().ok();
    loop {
        let line = match receive.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                println!("\nThe arcade lights click off behind you.");
                return Ok(());
            }
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {
                let output = game.advance_time(250)?;
                if !output.is_empty() {
                    println!();
                    print_lines(output);
                    print!("\n> ");
                    io::stdout().flush().ok();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let line = line.trim();
        if line.is_empty() {
            print!("\n> ");
            io::stdout().flush().ok();
            continue;
        }
        if matches!(line, "quit" | "q" | "exit") {
            println!("The arcade lights click off behind you.");
            return Ok(());
        }
        print_lines(game.execute(line)?);
        print!("\n> ");
        io::stdout().flush().ok();
    }
}

fn print_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
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
            "take" | "drop" | "go" | "say" | "greet" | "change" | "attack" | "use" | "omen" => {
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

#[cfg(test)]
fn live_entity_text(store: &EntStore, rel: RelId, key: Entity) -> Option<String> {
    live_row(store, rel, |tuple| {
        tuple.as_slice().first() == Some(&Value::Ent(key))
    })
    .map(|tuple| text_at(&tuple, 1))
}

#[cfg(test)]
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
        self.ensure_clock_sample()?;
        self.runtime
            .enqueue(PLAYER, "inbox", "inbox_seq", line)
            .map_err(err)?;
        self.drive_actors()?;

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
                }
            }
            _ if told.is_empty() => out.push("Nothing happens.".into()),
            _ => out.extend(told),
        }

        if self.read_entity_set(self.r.player_defeat).contains(&PLAYER) {
            self.store()
                .commit(&[(self.r.player_defeat, Tuple::from([Value::Ent(PLAYER)]), -1)])
                .map_err(err)?;
            out.push("The basement folds shut around you.".into());
            out.extend(self.leave_dungeon(true)?);
        }
        self.pump_watch(&mut out)?;
        Ok(out)
    }

    fn ensure_clock_sample(&self) -> Result<(), String> {
        if self
            .store()
            .read_at(self.r.clock, self.store().current())
            .map_err(err)?
            .into_iter()
            .all(|(_, weight)| weight <= 0)
        {
            self.runtime.record_sample(0, 0).map_err(err)?;
        }
        Ok(())
    }

    fn drive_actors(&self) -> Result<(), String> {
        match self.runtime.drive_to_idle().map_err(err)?.status {
            DriveStatus::Idle => Ok(()),
            DriveStatus::FuelExhausted => {
                Err("shotengai actor driver exhausted its fuel budget".into())
            }
            DriveStatus::ActorFault {
                actor,
                sequence,
                message,
            } => Err(format!(
                "shotengai actor {} faulted at inbox sequence {sequence}: {message}",
                actor.0
            )),
        }
    }

    fn advance_time(&mut self, delta_ms: i64) -> Result<Vec<String>, String> {
        self.ensure_clock_sample()?;
        let before_edition = self.store().current();
        let before_room = self.room_of(CAT);
        let latest = self
            .store()
            .read_at(self.r.clock, self.store().current())
            .map_err(err)?
            .into_iter()
            .filter_map(|(tuple, weight)| match tuple.as_slice() {
                [Value::Int(seq), Value::Int(ms), Value::Int(_)] if weight > 0 => Some((*seq, *ms)),
                _ => None,
            })
            .max()
            .map(|(_, ms)| ms)
            .unwrap_or(0);
        self.runtime
            .record_sample(latest.saturating_add(delta_ms), 0)
            .map_err(err)?;
        self.drive_actors()?;
        let after_room = self.room_of(CAT);
        let mut out = self.drain_tell(before_edition)?;
        if before_room != after_room {
            out.push("The cat follows its patrol clock.".into());
        }
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
        self.rebind_runtime()?;
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
        let cat_before_command = game.room_of(CAT);
        let greeting = run(&mut game, "greet cat").join("\n");
        assert!(greeting.contains("Mugi"));
        assert_eq!(
            game.room_of(CAT),
            cat_before_command,
            "player input must no longer inject a patrol tick"
        );
        let clock_output = game.advance_time(1_000).unwrap().join("\n");
        assert_ne!(
            game.room_of(CAT),
            cat_before_command,
            "committed time must move the cat without another player command"
        );
        assert!(clock_output.contains("patrol clock"), "{clock_output}");
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
    fn package_reopen_is_an_edition_preserving_noop_and_omen_uses_committed_rng() {
        let dir = tempfile::tempdir().unwrap();
        let mut game = Game::open(dir.path()).unwrap();
        let installed = game.store().current();
        assert_eq!(installed, Edition(1));
        let old_state = game.entity_int(game.r.rng_state, WORLD).unwrap();
        let output = run(&mut game, "omen").join("\n");
        assert!(output.contains("arcade"), "{output}");
        let successor = game.entity_int(game.r.rng_state, WORLD).unwrap();
        assert_ne!(successor, old_state);
        let after_omen = game.store().current();
        drop(game);

        let reopened = Game::open(dir.path()).unwrap();
        assert_eq!(reopened.store().current(), after_omen);
        assert_eq!(
            reopened.entity_int(reopened.r.rng_state, WORLD),
            Some(successor)
        );
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
        let mut shutter_transcript = Vec::new();
        shutter_transcript.extend(run(&mut game, "use live"));
        shutter_transcript.extend(run(&mut game, "use live"));
        shutter_transcript.extend(run(&mut game, "use live"));
        let shutter = shifted(SHUTTER_MAW, game.instance_info().unwrap().0);
        assert_eq!(
            game.entity_int(game.r.monster_reward, shutter),
            None,
            "three live-wire uses must claim the shutter maw reward; hp={:?}; transcript={shutter_transcript:?}",
            game.entity_int(game.r.hp, shutter)
        );
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
        let behavior_a =
            Program::behavior_with_grants(&game.program, "inbox", PLAYER, Arc::clone(&game.grants))
                .unwrap();
        let behavior_b =
            Program::behavior_with_grants(&game.program, "inbox", PLAYER, Arc::clone(&game.grants))
                .unwrap();
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
    fn racing_victory_claims_award_the_reward_exactly_once() {
        let (_dir, mut game) = fresh_game();
        go_to_cinema(&mut game);
        run(&mut game, "enter basement");
        run(&mut game, "go north");
        let monster = shifted(RECEIPT_MOTH, game.instance_info().unwrap().0);
        let old_hp = game.entity_int(game.r.hp, monster).unwrap();
        game.store()
            .commit(&[
                (
                    game.r.hp,
                    Tuple::from([Value::Ent(monster), Value::Int(old_hp)]),
                    -1,
                ),
                (
                    game.r.hp,
                    Tuple::from([Value::Ent(monster), Value::Int(0)]),
                    1,
                ),
                (
                    game.r.combat_event,
                    Tuple::from([
                        Value::Ent(PLAYER),
                        Value::Ent(monster),
                        Value::text("defeated"),
                    ]),
                    1,
                ),
            ])
            .unwrap();

        let snapshot = Snapshot::at_current(game.store());
        let behavior_a = Program::behavior_with_grants(
            &game.program,
            "combat_inbox",
            COMBAT,
            Arc::clone(&game.grants),
        )
        .unwrap();
        let behavior_b = Program::behavior_with_grants(
            &game.program,
            "combat_inbox",
            COMBAT,
            Arc::clone(&game.grants),
        )
        .unwrap();
        let patch_a = behavior_a(&snapshot, &tokens("combat-step")).unwrap();
        let patch_b = behavior_b(&snapshot, &tokens("combat-step")).unwrap();
        drop(snapshot);

        let first = commit_patch(
            game.store(),
            game.store(),
            &patch_a,
            &combat_authority(game.r),
        )
        .unwrap();
        let second = commit_patch(
            game.store(),
            game.store(),
            &patch_b,
            &combat_authority(game.r),
        )
        .unwrap();
        assert!(matches!(first, CommitOutcome::Committed(_)));
        assert_eq!(second, CommitOutcome::Rejected);
        assert_eq!(game.entity_int(game.r.monster_reward, monster), None);
        assert_eq!(
            game.keyed_int(game.r.job_points, PLAYER, COURIER, 2),
            Some(1)
        );
        assert_eq!(
            game.store()
                .read_at(game.r.combat_claimed, game.store().current())
                .unwrap()
                .into_iter()
                .find(|(tuple, diff)| {
                    *diff > 0 && tuple.as_slice() == [Value::Ent(monster), Value::Bool(true)]
                })
                .map(|(_, diff)| diff),
            Some(1)
        );
    }

    #[test]
    fn combat_timer_survives_reopen_before_actor_attention() {
        let (dir, mut game) = fresh_game();
        go_to_cinema(&mut game);
        run(&mut game, "enter basement");
        run(&mut game, "go north");
        let player_hp = game.entity_int(game.r.hp, PLAYER).unwrap();
        let snapshot = Snapshot::at_current(game.store());
        let behavior =
            Program::behavior_with_grants(&game.program, "inbox", PLAYER, Arc::clone(&game.grants))
                .unwrap();
        let patch = behavior(&snapshot, &tokens("attack moth")).unwrap();
        drop(snapshot);
        assert!(matches!(
            commit_patch(
                game.store(),
                game.store(),
                &patch,
                &player_authority(game.r)
            )
            .unwrap(),
            CommitOutcome::Committed(_)
        ));
        let fired = game
            .runtime
            .drive_with_fuel(std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert_eq!((fired.timers_fired, fired.actor_steps), (1, 0));
        drop(game);

        let reopened = Game::open(dir.path()).unwrap();
        reopened.drive_actors().unwrap();
        assert!(reopened
            .store()
            .read_at(reopened.r.combat_event, reopened.store().current())
            .unwrap()
            .into_iter()
            .all(|(_, weight)| weight <= 0));
        assert!(
            reopened.entity_int(reopened.r.hp, PLAYER).unwrap() < player_hp,
            "the pending combat message must counterattack exactly once after reopen"
        );
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
