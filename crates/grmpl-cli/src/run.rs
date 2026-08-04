//! `grmpl run [WORLD.grmpl] [STORE_DIR]` — stand up a language-defined world and
//! drive it from an interactive REPL.
//!
//! The host is deliberately thin: it does terminal I/O, grants native powers,
//! and drives the clock. Initial world/rule data and every portable rule live
//! in `worlds/moo.grmpl`:
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

use grmpl::moo::{
    CAT, INSTANCE_BASE, INSTANCE_STRIDE, OBSERVATORY, PLAYER, VAULT_ANTE, VAULT_BASE, VAULT_SPAN,
};
use grmpl::{MooRelations as Rels, MooRuntime};
use grmpl_core::{
    Diff, Edition, EditionStore, Entity, RelId, TraceStore, Tuple, Value, WorldStore,
};
use grmpl_ent::EntStore;
use grmpl_proc::{decode_activation, OnWatch, Process};

pub fn run(world: Option<String>, store_dir: Option<String>) -> Result<(), String> {
    let src = match &world {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("cannot read world `{path}`: {e}"))?
        }
        None => grmpl::moo::SOURCE.to_string(),
    };
    // Open the world *first*, then compile against its durable catalog: names
    // resolve to the ids the store already bound, so a relation keeps its id
    // across reopens however the source is later reordered. Compiling first and
    // ignoring the catalog would have made the ids a function of source layout.
    let store = Arc::new(open_store(store_dir.as_deref())?);
    let shared: Arc<dyn WorldStore> = store.clone();
    let runtime = MooRuntime::compile(shared, &src)?;
    let r = runtime.relations();
    let player = runtime.player_process(PLAYER)?;
    let cat = runtime.patrol_process(CAT)?;

    let mut repl = Repl {
        world: &runtime,
        store: &store,
        r,
        player,
        cat,
        out_cursor: store.current(),
        watch: None,
        delivered: 0,
        rng: 0x1234_5678_9abc_def0,
        instances: 0,
        vault: None,
    };
    repl.banner(world.as_deref());
    repl.loop_forever()
}

fn open_store(dir: Option<&str>) -> Result<EntStore, String> {
    let path = match dir {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::temp_dir().join(format!("grmpl-run-{}", std::process::id())),
    };
    EntStore::open(&path).map_err(|e| format!("cannot open store at {}: {e:?}", path.display()))
}

// ===========================================================================
// The REPL
// ===========================================================================
struct Repl<'a> {
    world: &'a MooRuntime,
    store: &'a EntStore,
    r: Rels,
    player: Process,
    cat: Process,
    out_cursor: Edition,
    watch: Option<OnWatch>,
    delivered: usize,
    rng: u64,
    /// How many vault instances have been minted this session (picks the block).
    instances: u64,
    /// If inside a vault: the DSP shift of the current instance and the room to
    /// return to on `leave`.
    vault: Option<(i64, Entity)>,
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
                "enter" => self.cmd_enter(line)?,
                "leave" => self.cmd_leave()?,
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
  enter vault        step into a PRIVATE instance of the vault (DSP-relocated copy)
  leave              climb back out; the instance fades
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
        let ways = self
            .view_rows("ways", &[Value::Ent(PLAYER)])
            .unwrap_or_default();
        let mut dirs: Vec<String> = ways.iter().map(|(t, _)| text_at(t, 0)).collect();
        dirs.sort();
        println!(
            "Exits: {}.",
            if dirs.is_empty() {
                "none".into()
            } else {
                dirs.join(", ")
            }
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
            let who = if known.contains(&CAT) {
                self.name_of(CAT)
            } else {
                "The cat".into()
            };
            println!("  · {who} prowls the {}.", self.name_of(catroom));
        }
        let passages = self.count(self.r.exits);
        println!("  · {passages} passages thread the six rooms.");
        println!("(Every line above is a live query over the world — the room IS a view.)");
    }

    fn cmd_inventory(&self) {
        let inv = self
            .view_rows("inventory", &[Value::Ent(PLAYER)])
            .unwrap_or_default();
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
                Tuple::from([
                    Value::Ent(PLAYER),
                    Value::Int(slot as i64),
                    Value::Int(*code),
                ]),
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
        let show: Vec<String> = (0..4)
            .filter_map(|s| hand.get(&s).map(|c| card_name(*c)))
            .collect();
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
        let pairs = self
            .view_rows("crib_pairs", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .len();
        let fif2 = self
            .view_rows("crib_fif2", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .len();
        let fif3 = self
            .view_rows("crib_fif3", &[Value::Ent(PLAYER)])
            .unwrap_or_default()
            .len();
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
        self.world.enqueue(PLAYER, line).map_err(err)?;
        self.player
            .run_to_idle_retrying(self.store, self.store, self.world.runtime().policy())
            .map_err(err)?;

        let told = self.drain_tell(before)?;
        let verb = line.split_whitespace().next().unwrap_or("");
        if verb == "greet" {
            match told.last() {
                Some(name) => {
                    println!("You introduce yourself. You are now acquainted with {name}.")
                }
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

    // --- instanced dungeons: DSP relocation (the Ent's virtual copy) ------

    /// `enter vault` — mint a **private instance** of the vault template by
    /// relocating its whole sub-world into a fresh id block (DSP `apply_all`), then
    /// step the player into the instanced antechamber. Every visit is disjoint, so
    /// two players never collide — the store's `instance_template` does the WID
    /// read + coordinate shift; the host only picks the block and walks the player.
    fn cmd_enter(&mut self, line: &str) -> Result<(), String> {
        if line.split_whitespace().nth(1) != Some("vault") {
            println!("You can only `enter vault` from here.");
            return Ok(());
        }
        if self.vault.is_some() {
            println!("You are already inside a vault. `leave` first.");
            return Ok(());
        }
        let home = match self.room_of(PLAYER)? {
            Some(room) => room,
            None => {
                println!("You are nowhere in particular.");
                return Ok(());
            }
        };
        // Allocate this session's next instance block and the shift onto it.
        let target_base = INSTANCE_BASE + self.instances * INSTANCE_STRIDE;
        let shift = target_base as i64 - VAULT_BASE as i64;
        self.instances += 1;
        self.store
            .instance_template(
                &[self.r.located, self.r.named, self.r.exits],
                VAULT_BASE,
                VAULT_BASE + VAULT_SPAN,
                shift,
            )
            .map_err(err)?;
        // Walk the player out of `home` and into the instanced antechamber.
        let ante = Entity(VAULT_ANTE.0.wrapping_add(shift as u64));
        self.store
            .commit(&[
                (
                    self.r.located,
                    Tuple::from([Value::Ent(PLAYER), Value::Ent(home)]),
                    -1,
                ),
                (
                    self.r.located,
                    Tuple::from([Value::Ent(PLAYER), Value::Ent(ante)]),
                    1,
                ),
            ])
            .map_err(err)?;
        self.vault = Some((shift, home));
        println!(
            "You slip into a private instance of the vault — a DSP-relocated copy, yours alone."
        );
        self.pump_watch()?;
        self.cmd_look();
        Ok(())
    }

    /// `leave` — walk home and tear the current instance down: retract every fact
    /// in its id block plus any vault loot you were carrying. The private sub-world
    /// fades; nothing of it follows you out.
    fn cmd_leave(&mut self) -> Result<(), String> {
        let (shift, home) = match self.vault {
            Some(v) => v,
            None => {
                println!("You are not inside a vault.");
                return Ok(());
            }
        };
        let block_lo = VAULT_BASE.wrapping_add(shift as u64);
        let block_hi = block_lo + VAULT_SPAN;
        let in_block = |e: Entity| e.0 >= block_lo && e.0 < block_hi;
        let cur = self.room_of(PLAYER)?.unwrap_or(Entity(block_lo));
        let at = self.store.current();
        let mut b: Vec<(RelId, Tuple, Diff)> = Vec::new();
        // Retract the instance's rooms/items/passages (block-keyed lead entity).
        for rel in [self.r.located, self.r.named, self.r.exits] {
            for (t, d) in self.store.read_at(rel, at).map_err(err)? {
                if d > 0 && matches!(t.as_slice().first(), Some(Value::Ent(e)) if in_block(*e)) {
                    b.push((rel, t, -d));
                }
            }
        }
        // Drop any vault item you carried out (held owner is you, thing in block).
        for (t, d) in self.store.read_at(self.r.held, at).map_err(err)? {
            if d > 0 && matches!(t.as_slice().get(1), Some(Value::Ent(e)) if in_block(*e)) {
                b.push((self.r.held, t, -d));
            }
        }
        // Walk the player home.
        b.push((
            self.r.located,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(cur)]),
            -1,
        ));
        b.push((
            self.r.located,
            Tuple::from([Value::Ent(PLAYER), Value::Ent(home)]),
            1,
        ));
        self.store.commit(&b).map_err(err)?;
        self.vault = None;
        println!("You climb back out. The instance fades — it was only ever yours.");
        self.pump_watch()?;
        self.cmd_look();
        Ok(())
    }

    /// Advance the cat by one patrol step — driven entirely by its `Tick`
    /// behavior — and narrate its comings and goings.
    fn tick_cat(&mut self) -> Result<(), String> {
        let player_room = self.room_of(PLAYER).ok().flatten();
        let before = self.room_of(CAT).ok().flatten();
        self.world.enqueue(CAT, "tick").map_err(err)?;
        self.cat
            .run_to_idle_retrying(self.store, self.store, self.world.runtime().policy())
            .map_err(err)?;
        let after = self.room_of(CAT).ok().flatten();

        let known = self.known_by(PLAYER);
        let catname = if known.contains(&CAT) {
            self.name_of(CAT)
        } else {
            "The cat".into()
        };
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
        let ow = self.world.install_world_watch(PLAYER, PLAYER)?;
        ow.pump(self.store, self.store).map_err(err)?;
        self.delivered = self.count_for(self.r.wmail, PLAYER);
        self.watch = Some(ow);
        println!("Now watching the world — changes will stream as [world] deltas.");
        Ok(())
    }

    fn pump_watch(&mut self) -> Result<(), String> {
        if let Some(ow) = &self.watch {
            ow.pump(self.store, self.store).map_err(err)?;
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
        self.world.runtime().view(name, args)
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
        self.store
            .read_at(rel, at)
            .unwrap_or_default()
            .iter()
            .filter(|(_, d)| *d > 0)
            .count()
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
        for u in self
            .store
            .scan_updates(self.r.tell, since, to)
            .map_err(err)?
        {
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

const RANKS: [&str; 13] = [
    "A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K",
];
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
