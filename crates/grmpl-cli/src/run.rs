//! `grmpl run [WORLD.grmpl] [STORE_DIR]` — stand up a language-defined world and
//! drive it from an interactive REPL.
//!
//! The host is deliberately thin: it does terminal I/O, seeds world + rule DATA,
//! and drives the clock. Every rule lives in `worlds/moo.grmpl`:
//!   * reads are `view`s (the host runs the query and prints the rows),
//!   * the player's verbs are the compiled `on parse` behavior,
//!   * the wandering cat is *also* just a behavior — the host enqueues it a
//!     `tick` each turn and the `Tick` handler walks it along the `patrol`
//!     relation,
//!   * cribbage scoring is a set of views; the host deals cards (data) and only
//!     counts the rows each scoring view returns.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Arc;

use grmpl_core::{
    Authority, DomainId, Diff, Edition, EditionStore, Entity, NoSchemas, RelId, Scope, TraceStore,
    Tuple, Value,
};
use grmpl_diff::Snapshot;
use grmpl_lang::Program;
use grmpl_proc::{decode_activation, enqueue, OnWatch, Process};
use grmpl_store::FjallStore;

const BUILTIN_MOO: &str = include_str!("../../../worlds/moo.grmpl");

// Entities ------------------------------------------------------------------
const PLAYER: Entity = Entity(1);
// Rooms
const FOYER: Entity = Entity(10);
const LIBRARY: Entity = Entity(11);
const GARDEN: Entity = Entity(12);
const MARKET: Entity = Entity(13);
const OBSERVATORY: Entity = Entity(14);
const KITCHEN: Entity = Entity(15);
// Things
const LAMP: Entity = Entity(20);
const KEY: Entity = Entity(21);
const BOOK: Entity = Entity(22);
const ROSE: Entity = Entity(23);
const COIN: Entity = Entity(24);
const CAKE: Entity = Entity(25);
// NPCs
const CAT: Entity = Entity(30);
const MERCHANT: Entity = Entity(31);

/// The cat's patrol loop (each `tick` follows one edge).
const PATROL: [(Entity, Entity); 4] = [
    (FOYER, LIBRARY),
    (LIBRARY, GARDEN),
    (GARDEN, MARKET),
    (MARKET, FOYER),
];

pub fn run(world: Option<String>, store_dir: Option<String>) -> Result<(), String> {
    let src = match &world {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("cannot read world `{path}`: {e}"))?
        }
        None => BUILTIN_MOO.to_string(),
    };
    let prog = Arc::new(Program::compile(&src, 1)?);
    let r = Rels::resolve(&prog)?;

    let store = open_store(store_dir.as_deref())?;
    seed_world(&store, &r)?;
    seed_rules(&store, &r)?;

    let player = process(&prog, &r, PLAYER, player_authority(&r))?;
    let cat = process(&prog, &r, CAT, npc_authority(&r))?;

    let mut repl = Repl {
        prog: &prog,
        store: &store,
        r,
        player,
        cat,
        player_seq: 0,
        cat_seq: 0,
        out_cursor: store.current(),
        watch: None,
        delivered: 0,
        rng: 0x1234_5678_9abc_def0,
    };
    repl.banner(world.as_deref());
    repl.loop_forever()
}

fn open_store(dir: Option<&str>) -> Result<FjallStore, String> {
    let path = match dir {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::temp_dir().join(format!("grmpl-run-{}", std::process::id())),
    };
    FjallStore::open(&path).map_err(|e| format!("cannot open store at {}: {e:?}", path.display()))
}

// ===========================================================================
// Relation handles
// ===========================================================================
struct Rels {
    located: RelId,
    named: RelId,
    held: RelId,
    exits: RelId,
    players: RelId,
    value: RelId,
    person: RelId,
    label: RelId,
    knows: RelId,
    patrol: RelId,
    card: RelId,
    cardval: RelId,
    cardrank: RelId,
    slotpair: RelId,
    slottrio: RelId,
    sum15two: RelId,
    sum15three: RelId,
    tell: RelId,
    inbox: RelId,
    cursor: RelId,
    wmail: RelId,
    wcursor: RelId,
    wseq: RelId,
}

impl Rels {
    fn resolve(prog: &Program) -> Result<Rels, String> {
        let g = |n: &str| prog.rel_id(n).ok_or_else(|| format!("world has no `rel {n}`"));
        Ok(Rels {
            located: g("located")?,
            named: g("named")?,
            held: g("held")?,
            exits: g("exits")?,
            players: g("players")?,
            value: g("value")?,
            person: g("person")?,
            label: g("label")?,
            knows: g("knows")?,
            patrol: g("patrol")?,
            card: g("card")?,
            cardval: g("cardval")?,
            cardrank: g("cardrank")?,
            slotpair: g("slotpair")?,
            slottrio: g("slottrio")?,
            sum15two: g("sum15two")?,
            sum15three: g("sum15three")?,
            tell: g("tell")?,
            inbox: g("inbox")?,
            cursor: g("cursor")?,
            wmail: g("wmail")?,
            wcursor: g("wcursor")?,
            wseq: g("wseq")?,
        })
    }
}

fn player_authority(r: &Rels) -> Authority {
    Authority::new(
        DomainId(1),
        vec![
            Scope::whole(r.located),
            Scope::whole(r.held),
            Scope::whole(r.knows),
            Scope::whole(r.tell),
            Scope::whole(r.cursor),
        ],
    )
}

fn npc_authority(r: &Rels) -> Authority {
    Authority::new(DomainId(1), vec![Scope::whole(r.located), Scope::whole(r.cursor)])
}

fn process(
    prog: &Arc<Program>,
    r: &Rels,
    who: Entity,
    authority: Authority,
) -> Result<Process, String> {
    Ok(Process {
        entity: who,
        authority,
        inbox: r.inbox,
        cursor_rel: r.cursor,
        behavior: Program::behavior(prog, "inbox", who)?,
    })
}

// ===========================================================================
// Seeding (all data — no rules)
// ===========================================================================
fn seed_world(store: &FjallStore, r: &Rels) -> Result<(), String> {
    let at = store.current();
    let seeded = store
        .read_at(r.named, at)
        .map_err(err)?
        .into_iter()
        .any(|(t, d)| d > 0 && t.as_slice().first() == Some(&Value::Ent(PLAYER)));
    if seeded {
        return Ok(());
    }
    let e = Value::Ent;
    let t = Value::text;
    let mut b: Vec<(RelId, Tuple, Diff)> = Vec::new();
    let loc = |who: Entity, room: Entity, b: &mut Vec<_>| {
        b.push((r.located, Tuple::from([e(who), e(room)]), 1));
    };
    // Player + things
    loc(PLAYER, FOYER, &mut b);
    loc(LAMP, FOYER, &mut b);
    loc(KEY, FOYER, &mut b);
    loc(BOOK, LIBRARY, &mut b);
    loc(ROSE, GARDEN, &mut b);
    loc(COIN, MARKET, &mut b);
    loc(CAKE, KITCHEN, &mut b);
    // NPCs
    loc(CAT, FOYER, &mut b);
    loc(MERCHANT, MARKET, &mut b);

    for (who, name) in [
        (PLAYER, "you"),
        (FOYER, "Foyer"),
        (LIBRARY, "Library"),
        (GARDEN, "Garden"),
        (MARKET, "Market"),
        (OBSERVATORY, "Observatory"),
        (KITCHEN, "Kitchen"),
        (LAMP, "brass lamp"),
        (KEY, "iron key"),
        (BOOK, "old book"),
        (ROSE, "red rose"),
        (COIN, "gold coin"),
        (CAKE, "iced cake"),
        (CAT, "Whiskers"),
        (MERCHANT, "Bartleby"),
    ] {
        b.push((r.named, Tuple::from([e(who), t(name)]), 1));
    }
    b.push((r.players, Tuple::from([e(PLAYER), t("you")]), 1));

    // People wear a public label until you learn their name.
    for (who, lbl) in [(CAT, "a cat"), (MERCHANT, "a merchant")] {
        b.push((r.person, Tuple::from([e(who)]), 1));
        b.push((r.label, Tuple::from([e(who), t(lbl)]), 1));
    }
    // You always know yourself.
    b.push((r.knows, Tuple::from([e(PLAYER), e(PLAYER)]), 1));

    // Object values (coins).
    for (obj, coins) in [(LAMP, 10), (KEY, 1), (BOOK, 3), (ROSE, 5), (COIN, 20), (CAKE, 2)] {
        b.push((r.value, Tuple::from([e(obj), Value::Int(coins)]), 1));
    }

    // The map (one-way passages, seeded both ways).
    for (from, way, to) in [
        (FOYER, "north", LIBRARY),
        (LIBRARY, "south", FOYER),
        (FOYER, "east", GARDEN),
        (GARDEN, "west", FOYER),
        (FOYER, "west", KITCHEN),
        (KITCHEN, "east", FOYER),
        (FOYER, "up", OBSERVATORY),
        (OBSERVATORY, "down", FOYER),
        (GARDEN, "south", MARKET),
        (MARKET, "north", GARDEN),
    ] {
        b.push((r.exits, Tuple::from([e(from), t(way), e(to)]), 1));
    }

    // The cat's patrol route (its whole "AI", as data).
    for (from, to) in PATROL {
        b.push((r.patrol, Tuple::from([e(from), e(to)]), 1));
    }

    store.commit(&b).map_err(|e| format!("seed_world: {e:?}"))?;
    Ok(())
}

/// Seed the cribbage rule tables: the *arithmetic*, precomputed as relations the
/// scoring views join against. This is data entry, not scoring.
fn seed_rules(store: &FjallStore, r: &Rels) -> Result<(), String> {
    let at = store.current();
    let already = !store.read_at(r.cardval, at).map_err(err)?.is_empty();
    if already {
        return Ok(());
    }
    let i = Value::Int;
    let mut b: Vec<(RelId, Tuple, Diff)> = Vec::new();
    // card code -> pip value and rank
    for code in 0..52i64 {
        let rank = code % 13;
        let pips = (rank + 1).min(10);
        b.push((r.cardval, Tuple::from([i(code), i(pips)]), 1));
        b.push((r.cardrank, Tuple::from([i(code), i(rank)]), 1));
    }
    // slot combinations (views can't say i != j, so we enumerate them as data)
    for a in 0..5i64 {
        for c in (a + 1)..5 {
            b.push((r.slotpair, Tuple::from([i(a), i(c)]), 1));
        }
    }
    for a in 0..5i64 {
        for c in (a + 1)..5 {
            for d in (c + 1)..5 {
                b.push((r.slottrio, Tuple::from([i(a), i(c), i(d)]), 1));
            }
        }
    }
    // value combos totalling 15 (all orders, since slot order is arbitrary)
    for a in 1..=10i64 {
        for c in 1..=10i64 {
            if a + c == 15 {
                b.push((r.sum15two, Tuple::from([i(a), i(c)]), 1));
            }
            for d in 1..=10i64 {
                if a + c + d == 15 {
                    b.push((r.sum15three, Tuple::from([i(a), i(c), i(d)]), 1));
                }
            }
        }
    }
    store.commit(&b).map_err(|e| format!("seed_rules: {e:?}"))?;
    Ok(())
}

// ===========================================================================
// The REPL
// ===========================================================================
struct Repl<'a> {
    prog: &'a Arc<Program>,
    store: &'a FjallStore,
    r: Rels,
    player: Process,
    cat: Process,
    player_seq: i64,
    cat_seq: i64,
    out_cursor: Edition,
    watch: Option<OnWatch>,
    delivered: usize,
    rng: u64,
}

impl Repl<'_> {
    fn banner(&self, world: Option<&str>) {
        let which = world.unwrap_or("the built-in MOO");
        println!("grmpl — running {which}");
        println!("A manor of six rooms, a wandering cat, a merchant, and a deck of cards.");
        println!("Type `help` for commands, `quit` to leave.\n");
        self.cmd_look();
    }

    fn loop_forever(&mut self) -> Result<(), String> {
        let stdin = io::stdin();
        loop {
            print!("\n> ");
            io::stdout().flush().ok();
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => {
                    println!("\nGoodbye.");
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => return Err(format!("stdin: {e}")),
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let verb = line.split_whitespace().next().unwrap_or("");
            match verb {
                "quit" | "q" | "exit" => {
                    println!("Goodbye.");
                    return Ok(());
                }
                "help" | "?" => self.cmd_help(),
                "look" | "l" => self.cmd_look(),
                "inventory" | "inv" | "i" => self.cmd_inventory(),
                "who" => self.cmd_who(),
                "treasure" => self.cmd_treasure(),
                "deal" => self.cmd_deal()?,
                "hand" | "cards" => self.cmd_cards(),
                "score" => self.cmd_score(),
                "watch" => self.cmd_watch()?,
                "take" | "drop" | "go" | "say" | "greet" => self.cmd_turn(line)?,
                other => println!("I don't understand `{other}`. Try `help`."),
            }
        }
    }

    fn cmd_help(&self) {
        println!(
            "\
Moving about (each is a TURN — the cat moves too):
  look | l           describe your surroundings (a view; the Observatory is meta)
  go <way>           move through an exit
  take <thing>       pick something up            drop <thing>   put it down
  inventory | i      what you're carrying
  greet <who>        introduce yourself — lifts the 'fog of identity'
  say <word>         speak
People & world:
  who                the player roster            treasure       coin value per room
Cribbage (scoring is grmpl views; the host only tallies the rows):
  deal               deal a fresh 5-card hand     cards          show your hand
  score              count fifteens + pairs
  watch              stream reactive world changes
  help | quit"
        );
    }

    // --- reads (views) ----------------------------------------------------

    fn cmd_look(&self) {
        let room = match self.room_of(PLAYER) {
            Ok(Some(r)) => r,
            _ => {
                println!("You are nowhere in particular.");
                return;
            }
        };
        if room == OBSERVATORY {
            self.render_observatory();
            return;
        }
        println!("You are in the {}.", self.name_of(room));
        // Everything sharing the room, rendered through the fog of identity.
        let names = self.read_pairs(self.r.named); // entity -> true name
        let labels = self.read_pairs(self.r.label); // person -> public label
        let persons = self.read_set(self.r.person);
        let known = self.known_by(PLAYER);
        let mut here: Vec<String> = self
            .contents(PLAYER)
            .into_iter()
            .filter(|thing| *thing != PLAYER)
            .map(|thing| render_identity(thing, &names, &labels, &persons, &known))
            .collect();
        here.sort();
        if here.is_empty() {
            println!("You see nothing of note.");
        } else {
            println!("You see: {}.", here.join(", "));
        }
        let ways = self.view_rows("ways", &[Value::Ent(PLAYER)]).unwrap_or_default();
        let mut dirs: Vec<String> = ways.iter().map(|(t, _)| text_at(t, 0)).collect();
        dirs.sort();
        println!(
            "Exits: {}.",
            if dirs.is_empty() { "none".into() } else { dirs.join(", ") }
        );
    }

    fn render_observatory(&self) {
        println!("You are in the Observatory. A great orrery mirrors the whole world:");
        // Live aggregate: total coins across every room.
        let total: i64 = self
            .view_rows("treasure", &[])
            .unwrap_or_default()
            .iter()
            .map(|(t, _)| int_at(t, 1))
            .sum();
        println!("  · {total} coins of treasure lie scattered about the manor.");
        // Souls abroad: the people.
        let souls = self.read_set(self.r.person).len();
        println!("  · {souls} souls wander the halls.");
        // Where the cat prowls (name if you've met it, else 'the cat').
        if let Ok(Some(catroom)) = self.room_of(CAT) {
            let known = self.known_by(PLAYER);
            let who = if known.contains(&CAT) { self.name_of(CAT) } else { "The cat".into() };
            println!("  · {who} prowls the {}.", self.name_of(catroom));
        }
        let passages = self.count(self.r.exits);
        println!("  · {passages} passages thread the six rooms.");
        println!("(Every line above is a live query over the world — the room IS a view.)");
    }

    fn cmd_inventory(&self) {
        let inv = self.view_rows("inventory", &[Value::Ent(PLAYER)]).unwrap_or_default();
        let mut names: Vec<String> = inv.iter().map(|(t, _)| text_at(t, 1)).collect();
        names.sort();
        if names.is_empty() {
            println!("You are carrying nothing.");
        } else {
            println!("You are carrying: {}.", names.join(", "));
        }
    }

    fn cmd_who(&self) {
        let roster = self.view_rows("roster", &[]).unwrap_or_default();
        let mut names: Vec<String> = roster.iter().map(|(t, _)| text_at(t, 0)).collect();
        names.sort();
        println!("{} online: {}", names.len(), names.join(", "));
    }

    fn cmd_treasure(&self) {
        let rows = self.view_rows("treasure", &[]).unwrap_or_default();
        let mut lines: Vec<String> = rows
            .iter()
            .map(|(t, _)| format!("  {}: {} coins", text_at(t, 0), int_at(t, 1)))
            .collect();
        lines.sort();
        println!("Treasure per room (aggregate view — sum):");
        for l in lines {
            println!("{l}");
        }
    }

    // --- cribbage ---------------------------------------------------------

    fn cmd_deal(&mut self) -> Result<(), String> {
        // Shuffle a deck deterministically and take five cards; store as facts.
        let mut deck: Vec<i64> = (0..52).collect();
        for k in (1..deck.len()).rev() {
            let j = (self.next_rand() % (k as u64 + 1)) as usize;
            deck.swap(k, j);
        }
        // Retract any prior hand, assert the new one.
        let at = self.store.current();
        let mut b: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for (t, d) in self.store.read_at(self.r.card, at).map_err(err)? {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(PLAYER)) {
                b.push((self.r.card, t, -1));
            }
        }
        for (slot, code) in deck.iter().take(5).enumerate() {
            b.push((
                self.r.card,
                Tuple::from([Value::Ent(PLAYER), Value::Int(slot as i64), Value::Int(*code)]),
                1,
            ));
        }
        self.store.commit(&b).map_err(err)?;
        println!("Dealt a fresh hand. `cards` to see it, `score` to count it.");
        Ok(())
    }

    fn cmd_cards(&self) {
        let hand = self.hand();
        if hand.is_empty() {
            println!("You have no cards. Type `deal` first.");
            return;
        }
        let show: Vec<String> = (0..4).filter_map(|s| hand.get(&s).map(|c| card_name(*c))).collect();
        print!("Hand: {}", show.join(" "));
        if let Some(starter) = hand.get(&4) {
            print!("   |   starter: {}", card_name(*starter));
        }
        println!();
    }

    fn cmd_score(&self) {
        let hand = self.hand();
        if hand.len() < 5 {
            println!("Deal a full hand first (`deal`).");
            return;
        }
        // Scoring is the grmpl views; we only COUNT the combos they return.
        let pairs = self.view_rows("crib_pairs", &[Value::Ent(PLAYER)]).unwrap_or_default().len();
        let fif2 = self.view_rows("crib_fif2", &[Value::Ent(PLAYER)]).unwrap_or_default().len();
        let fif3 = self.view_rows("crib_fif3", &[Value::Ent(PLAYER)]).unwrap_or_default().len();
        let fifteens = fif2 + fif3;
        let total = 2 * pairs + 2 * fifteens;
        self.cmd_cards();
        println!("Score (counted from the scoring views):");
        println!("  fifteens: {fifteens} × 2 = {}", 2 * fifteens);
        println!("  pairs:    {pairs} × 2 = {}", 2 * pairs);
        println!("  ── total: {total}");
        if total == 0 {
            println!("  (nineteen — the cribbage 'zero'. Deal again.)");
        }
    }

    // --- turns (writes: the compiled behavior + the cat's tick) -----------

    fn cmd_turn(&mut self, line: &str) -> Result<(), String> {
        // The player's verb, through the compiled behavior.
        let before = self.store.current();
        enqueue(self.store, self.r.inbox, PLAYER, self.player_seq, tokens(line)).map_err(err)?;
        self.player_seq += 1;
        self.player.run_to_idle(self.store, &NoSchemas).map_err(err)?;

        let told = self.drain_tell(before)?;
        let verb = line.split_whitespace().next().unwrap_or("");
        if verb == "greet" {
            match told.last() {
                Some(name) => println!("You introduce yourself. You are now acquainted with {name}."),
                None => println!("There's no one like that here to greet."),
            }
        } else if told.is_empty() {
            println!("Nothing happens.");
        } else {
            for t in &told {
                println!("{t}");
            }
        }

        self.tick_cat()?;
        self.pump_watch()?;
        Ok(())
    }

    /// Advance the cat by one patrol step — driven entirely by its `Tick`
    /// behavior — and narrate its comings and goings.
    fn tick_cat(&mut self) -> Result<(), String> {
        let player_room = self.room_of(PLAYER).ok().flatten();
        let before = self.room_of(CAT).ok().flatten();
        enqueue(self.store, self.r.inbox, CAT, self.cat_seq, tokens("tick")).map_err(err)?;
        self.cat_seq += 1;
        self.cat.run_to_idle(self.store, &NoSchemas).map_err(err)?;
        let after = self.room_of(CAT).ok().flatten();

        let known = self.known_by(PLAYER);
        let catname = if known.contains(&CAT) { self.name_of(CAT) } else { "The cat".into() };
        if before == player_room && after != player_room && player_room.is_some() {
            println!("{catname} pads out of the room.");
        } else if after == player_room && before != player_room && player_room.is_some() {
            println!("{catname} slinks into the room.");
        }
        Ok(())
    }

    // --- reactive on-watch ------------------------------------------------

    fn cmd_watch(&mut self) -> Result<(), String> {
        if self.watch.is_some() {
            println!("Already watching the world.");
            return Ok(());
        }
        let auth = Authority::new(
            DomainId(1),
            vec![Scope::whole(self.r.wmail), Scope::whole(self.r.wcursor), Scope::whole(self.r.wseq)],
        );
        let (ow, _) = self
            .prog
            .install_watch("world", &[], PLAYER, PLAYER, auth, self.store, &NoSchemas)
            .map_err(err)?;
        ow.pump(self.store, &NoSchemas).map_err(err)?;
        self.delivered = self.count_for(self.r.wmail, PLAYER);
        self.watch = Some(ow);
        println!("Now watching the world — changes will stream as [world] deltas.");
        Ok(())
    }

    fn pump_watch(&mut self) -> Result<(), String> {
        if let Some(ow) = &self.watch {
            ow.pump(self.store, &NoSchemas).map_err(err)?;
            let at = self.store.current();
            let mut rows: Vec<(i64, i64, String)> = Vec::new();
            for (t, d) in self.store.read_at(self.r.wmail, at).map_err(err)? {
                if d <= 0 || t.as_slice().first() != Some(&Value::Ent(PLAYER)) {
                    continue;
                }
                let seq = int_at(&t, 1);
                if (seq as usize) < self.delivered {
                    continue;
                }
                if let Some(Value::Tuple(body)) = t.as_slice().get(2) {
                    if let Some((diff, row)) = decode_activation(&Tuple(body.clone())) {
                        rows.push((seq, diff, text_at(&row, 1)));
                    }
                }
            }
            rows.sort();
            for (_, diff, name) in &rows {
                println!("  [world] {} {name}", if *diff > 0 { "+" } else { "-" });
            }
            self.delivered = self.count_for(self.r.wmail, PLAYER);
        }
        Ok(())
    }

    // --- small query helpers ---------------------------------------------

    fn view_rows(&self, name: &str, args: &[Value]) -> Result<Vec<(Tuple, Diff)>, String> {
        let q = self.prog.view(name, args)?;
        q.find(&Snapshot::at_current(self.store)).map_err(err)
    }

    /// The entities sharing the viewer's room (via the `contents` view).
    fn contents(&self, viewer: Entity) -> Vec<Entity> {
        self.view_rows("contents", &[Value::Ent(viewer)])
            .unwrap_or_default()
            .iter()
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect()
    }

    fn known_by(&self, viewer: Entity) -> HashSet<Entity> {
        self.view_rows("acquainted", &[Value::Ent(viewer)])
            .unwrap_or_default()
            .iter()
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect()
    }

    fn room_of(&self, who: Entity) -> Result<Option<Entity>, String> {
        let at = self.store.current();
        for (t, d) in self.store.read_at(self.r.located, at).map_err(err)? {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(who)) {
                if let Some(Value::Ent(room)) = t.as_slice().get(1) {
                    return Ok(Some(*room));
                }
            }
        }
        Ok(None)
    }

    fn name_of(&self, who: Entity) -> String {
        let at = self.store.current();
        self.store
            .read_at(self.r.named, at)
            .unwrap_or_default()
            .into_iter()
            .find(|(t, d)| *d > 0 && t.as_slice().first() == Some(&Value::Ent(who)))
            .map(|(t, _)| text_at(&t, 1))
            .unwrap_or_else(|| "something".into())
    }

    /// The player's cards as slot -> code.
    fn hand(&self) -> HashMap<i64, i64> {
        let at = self.store.current();
        let mut m = HashMap::new();
        for (t, d) in self.store.read_at(self.r.card, at).unwrap_or_default() {
            if d > 0 && t.as_slice().first() == Some(&Value::Ent(PLAYER)) {
                m.insert(int_at(&t, 1), int_at(&t, 2));
            }
        }
        m
    }

    /// Read an (Ent, Text) relation into a map.
    fn read_pairs(&self, rel: RelId) -> HashMap<Entity, String> {
        let at = self.store.current();
        let mut m = HashMap::new();
        for (t, d) in self.store.read_at(rel, at).unwrap_or_default() {
            if d > 0 {
                if let Some(Value::Ent(e)) = t.as_slice().first() {
                    m.insert(*e, text_at(&t, 1));
                }
            }
        }
        m
    }

    /// Read the first Ent column of a relation into a set.
    fn read_set(&self, rel: RelId) -> HashSet<Entity> {
        let at = self.store.current();
        let mut s = HashSet::new();
        for (t, d) in self.store.read_at(rel, at).unwrap_or_default() {
            if d > 0 {
                if let Some(Value::Ent(e)) = t.as_slice().first() {
                    s.insert(*e);
                }
            }
        }
        s
    }

    fn count(&self, rel: RelId) -> usize {
        let at = self.store.current();
        self.store.read_at(rel, at).unwrap_or_default().iter().filter(|(_, d)| *d > 0).count()
    }

    fn count_for(&self, rel: RelId, key: Entity) -> usize {
        let at = self.store.current();
        self.store
            .read_at(rel, at)
            .unwrap_or_default()
            .iter()
            .filter(|(t, d)| *d > 0 && t.as_slice().first() == Some(&Value::Ent(key)))
            .count()
    }

    fn drain_tell(&mut self, since: Edition) -> Result<Vec<String>, String> {
        let to = self.store.current();
        let mut out = Vec::new();
        for u in self.store.scan_updates(self.r.tell, since, to).map_err(err)? {
            let s = u.tuple.as_slice();
            if u.diff > 0 && s.first() == Some(&Value::Ent(PLAYER)) {
                if let Some(Value::Text(text)) = s.get(1) {
                    out.push(text.to_string());
                }
            }
        }
        self.out_cursor = to;
        Ok(out)
    }

    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

// ===========================================================================
// Free helpers
// ===========================================================================

fn tokens(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}

/// Render one thing through the fog of identity: objects by name; people by name
/// once you know them, else by their public label.
fn render_identity(
    thing: Entity,
    names: &HashMap<Entity, String>,
    labels: &HashMap<Entity, String>,
    persons: &HashSet<Entity>,
    known: &HashSet<Entity>,
) -> String {
    if persons.contains(&thing) && !known.contains(&thing) {
        labels.get(&thing).cloned().unwrap_or_else(|| "someone".into())
    } else {
        names.get(&thing).cloned().unwrap_or_else(|| "something".into())
    }
}

const RANKS: [&str; 13] =
    ["A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K"];
const SUITS: [&str; 4] = ["♠", "♥", "♦", "♣"];

/// A card code's display name (pure formatting — the scoring lives in grmpl).
fn card_name(code: i64) -> String {
    let c = code.rem_euclid(52);
    format!("{}{}", RANKS[(c % 13) as usize], SUITS[(c / 13) as usize])
}

fn text_at(t: &Tuple, i: usize) -> String {
    match t.as_slice().get(i) {
        Some(Value::Text(s)) => s.to_string(),
        Some(other) => format!("{other:?}"),
        None => "?".to_string(),
    }
}

fn int_at(t: &Tuple, i: usize) -> i64 {
    match t.as_slice().get(i) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

fn err<E: std::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}
