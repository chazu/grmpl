//! The built-in MOO world package on the public [`Runtime`](crate::Runtime).
//!
//! Its rules live in `worlds/moo.grmpl`. Rust supplies only initial facts and
//! the operations the current language cannot express: entity allocation for
//! `dig`/`create` and formatted `look` output.

use std::sync::Arc;

use grmpl_core::{
    Authority, Diff, DomainId, Entity, Fact, Message, Patch, RelId, Result, Scope, Tuple, Value,
    WorldStore,
};
use grmpl_diff::Snapshot;
use grmpl_lang::Program;
use grmpl_proc::{seed_seq, Alloc, Behavior, OnWatch, Process};

use crate::runtime::Runtime;
use crate::watch::{Subscription, WatchRelations};

pub const SOURCE: &str = include_str!("../../../worlds/moo.grmpl");

pub const PLAYER: Entity = Entity(1);
pub const FOYER: Entity = Entity(10);
pub const LIBRARY: Entity = Entity(11);
pub const GARDEN: Entity = Entity(12);
pub const MARKET: Entity = Entity(13);
pub const OBSERVATORY: Entity = Entity(14);
pub const KITCHEN: Entity = Entity(15);
pub const LAMP: Entity = Entity(20);
pub const KEY: Entity = Entity(21);
pub const BOOK: Entity = Entity(22);
pub const ROSE: Entity = Entity(23);
pub const COIN: Entity = Entity(24);
pub const CAKE: Entity = Entity(25);
pub const CAT: Entity = Entity(30);
pub const MERCHANT: Entity = Entity(31);

pub const VAULT_BASE: u64 = 1_000;
pub const VAULT_SPAN: u64 = 10;
pub const VAULT_ANTE: Entity = Entity(1_000);
pub const VAULT_INNER: Entity = Entity(1_001);
pub const VAULT_TORCH: Entity = Entity(1_002);
pub const VAULT_GEM: Entity = Entity(1_003);
pub const INSTANCE_BASE: u64 = 100_000;
pub const INSTANCE_STRIDE: u64 = 1_000;

/// Dynamic players and builder-created entities are kept above every static and
/// instanced MOO block.
pub const ID_BASE: i64 = 1_000_000;

const PATROL: [(Entity, Entity); 4] = [
    (FOYER, LIBRARY),
    (LIBRARY, GARDEN),
    (GARDEN, MARKET),
    (MARKET, FOYER),
];

/// Durable relation ids resolved from the world program's catalog.
#[derive(Clone, Copy, Debug)]
pub struct MooRelations {
    pub located: RelId,
    pub named: RelId,
    pub held: RelId,
    pub exits: RelId,
    pub players: RelId,
    pub value: RelId,
    pub person: RelId,
    pub label: RelId,
    pub knows: RelId,
    pub patrol: RelId,
    pub card: RelId,
    pub cardval: RelId,
    pub cardrank: RelId,
    pub slotpair: RelId,
    pub slottrio: RelId,
    pub sum15two: RelId,
    pub sum15three: RelId,
    pub tell: RelId,
    pub inbox: RelId,
    pub cursor: RelId,
    pub inbox_seq: RelId,
    pub entity_seq: RelId,
    pub wmail: RelId,
    pub wcursor: RelId,
    pub wseq: RelId,
    pub wdelivery: RelId,
}

impl MooRelations {
    fn resolve(runtime: &Runtime) -> Result<MooRelations> {
        let r = |name| runtime.relation(name);
        Ok(MooRelations {
            located: r("located")?,
            named: r("named")?,
            held: r("held")?,
            exits: r("exits")?,
            players: r("players")?,
            value: r("value")?,
            person: r("person")?,
            label: r("label")?,
            knows: r("knows")?,
            patrol: r("patrol")?,
            card: r("card")?,
            cardval: r("cardval")?,
            cardrank: r("cardrank")?,
            slotpair: r("slotpair")?,
            slottrio: r("slottrio")?,
            sum15two: r("sum15two")?,
            sum15three: r("sum15three")?,
            tell: r("tell")?,
            inbox: r("inbox")?,
            cursor: r("cursor")?,
            inbox_seq: r("inbox_seq")?,
            entity_seq: r("entity_seq")?,
            wmail: r("wmail")?,
            wcursor: r("wcursor")?,
            wseq: r("wseq")?,
            wdelivery: r("wdelivery")?,
        })
    }

    /// Every relation participating in the MOO's logical replay projection.
    pub fn all(self) -> Vec<RelId> {
        vec![
            self.located,
            self.named,
            self.held,
            self.exits,
            self.players,
            self.value,
            self.person,
            self.label,
            self.knows,
            self.patrol,
            self.card,
            self.cardval,
            self.cardrank,
            self.slotpair,
            self.slottrio,
            self.sum15two,
            self.sum15three,
            self.tell,
            self.inbox,
            self.cursor,
            self.inbox_seq,
            self.entity_seq,
            self.wmail,
            self.wcursor,
            self.wseq,
            self.wdelivery,
        ]
    }
}

/// The built-in MOO bound to the one public world runtime.
pub struct MooRuntime {
    runtime: Arc<Runtime>,
    rels: MooRelations,
}

impl MooRuntime {
    pub fn builtin(store: Arc<dyn WorldStore>) -> std::result::Result<Arc<MooRuntime>, String> {
        Self::compile(store, SOURCE)
    }

    pub fn compile(
        store: Arc<dyn WorldStore>,
        source: &str,
    ) -> std::result::Result<Arc<MooRuntime>, String> {
        let runtime = Runtime::compile(store, source, 1)?;
        let rels = MooRelations::resolve(&runtime).map_err(|e| e.to_string())?;
        let moo = Arc::new(MooRuntime { runtime, rels });
        moo.seed().map_err(|e| e.to_string())?;
        Ok(moo)
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub fn store(&self) -> &dyn WorldStore {
        self.runtime.store()
    }

    pub fn shared_store(&self) -> Arc<dyn WorldStore> {
        self.runtime.shared_store()
    }

    pub fn program(&self) -> &Arc<Program> {
        self.runtime.program()
    }

    pub fn relations(&self) -> MooRelations {
        self.rels
    }

    pub fn player_authority(&self) -> Authority {
        let r = self.rels;
        Authority::new(
            DomainId(1),
            vec![
                Scope::whole(r.located),
                Scope::whole(r.named),
                Scope::whole(r.held),
                Scope::whole(r.exits),
                Scope::whole(r.players),
                Scope::whole(r.knows),
                Scope::whole(r.tell),
                Scope::whole(r.cursor),
                Scope::whole(r.inbox_seq),
                Scope::whole(r.entity_seq),
            ],
        )
    }

    pub fn patrol_authority(&self) -> Authority {
        Authority::new(
            DomainId(1),
            vec![
                Scope::whole(self.rels.located),
                Scope::whole(self.rels.cursor),
            ],
        )
    }

    pub fn watch_authority(&self) -> Authority {
        Authority::new(
            DomainId(1),
            vec![
                Scope::whole(self.rels.wmail),
                Scope::whole(self.rels.wcursor),
                Scope::whole(self.rels.wseq),
                Scope::whole(self.rels.wdelivery),
            ],
        )
    }

    pub fn player_process(&self, player: Entity) -> std::result::Result<Process, String> {
        Ok(Process {
            entity: player,
            authority: self.player_authority(),
            inbox: self.rels.inbox,
            cursor_rel: self.rels.cursor,
            behavior: self.player_behavior(player)?,
        })
    }

    pub fn patrol_process(&self, entity: Entity) -> std::result::Result<Process, String> {
        Ok(Process {
            entity,
            authority: self.patrol_authority(),
            inbox: self.rels.inbox,
            cursor_rel: self.rels.cursor,
            behavior: Program::behavior(self.program(), "inbox", entity)?,
        })
    }

    pub fn enqueue(&self, process: Entity, line: &str) -> Result<i64> {
        self.runtime.enqueue(process, "inbox", "inbox_seq", line)
    }

    pub fn install_world_watch(
        &self,
        watch: Entity,
        target: Entity,
    ) -> std::result::Result<OnWatch, String> {
        self.runtime
            .install_watch("world", &[], watch, target, self.watch_authority())
    }

    pub fn subscription(
        &self,
        watch: Entity,
        target: Entity,
    ) -> std::result::Result<Subscription, String> {
        let on_watch =
            self.program()
                .on_watch("world", &[], watch, target, self.watch_authority())?;
        Ok(Subscription::from_watch(
            on_watch,
            WatchRelations {
                inbox: self.rels.wmail,
                cursor: self.rels.wcursor,
                seqs: self.rels.wseq,
                delivery: self.rels.wdelivery,
            },
        ))
    }

    fn seed(&self) -> Result<()> {
        seed_world(self.store(), self.rels)?;
        seed_rules(self.store(), self.rels)?;
        seed_entity_counter(self.store(), self.rels)?;
        seed_seq(self.store(), self.rels.inbox_seq, PLAYER)?;
        seed_seq(self.store(), self.rels.inbox_seq, CAT)?;
        Ok(())
    }

    fn player_behavior(&self, player: Entity) -> std::result::Result<Behavior, String> {
        let language = Program::behavior(self.program(), "inbox", player)?;
        let rels = self.rels;
        Ok(Box::new(move |snap, body| {
            let verb = body.as_slice().first().and_then(|v| match v {
                Value::Text(text) => Some(text.as_ref()),
                _ => None,
            });
            match verb {
                Some("look") => native_look(snap, rels, player),
                Some("create") => native_create(snap, rels, player, body),
                Some("dig") => native_dig(snap, rels, player, body),
                _ => {
                    let patch = language(snap, body)?;
                    if patch_is_empty(&patch) {
                        let text = match verb {
                            Some("take") => "You don't see that here.",
                            Some("go") => "You can't go that way.",
                            Some("drop") => "You aren't carrying that.",
                            Some("greet") => "There's no one like that here.",
                            _ => "Huh?",
                        };
                        Ok(Patch::new().emit(tell(rels, player, text)))
                    } else {
                        Ok(patch)
                    }
                }
            }
        }))
    }
}

fn patch_is_empty(patch: &Patch) -> bool {
    patch.preconditions.is_empty()
        && patch.asserts.is_empty()
        && patch.retracts.is_empty()
        && patch.emits.is_empty()
        && patch.scheduled.is_empty()
        && patch.cursor_advance.is_none()
}

fn tell(rels: MooRelations, player: Entity, text: impl AsRef<str>) -> Message {
    Message {
        inbox: rels.tell,
        body: Tuple::from([Value::Ent(player), Value::text(text)]),
    }
}

fn room_of(snap: &Snapshot, rels: MooRelations, entity: Entity) -> Result<Option<Entity>> {
    Ok(snap
        .read(rels.located)?
        .into_iter()
        .find_map(|(tuple, diff)| {
            let cells = tuple.as_slice();
            (diff > 0 && cells.first() == Some(&Value::Ent(entity)))
                .then(|| cells.get(1))
                .flatten()
                .and_then(|value| match value {
                    Value::Ent(room) => Some(*room),
                    _ => None,
                })
        }))
}

fn noun(body: &Tuple, fallback: &str) -> Value {
    body.as_slice()
        .get(1)
        .cloned()
        .unwrap_or_else(|| Value::text(fallback))
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Text(text) => text.to_string(),
        other => format!("{other:?}"),
    }
}

fn native_create(
    snap: &Snapshot,
    rels: MooRelations,
    player: Entity,
    body: &Tuple,
) -> Result<Patch> {
    let Some(room) = room_of(snap, rels, player)? else {
        return Ok(Patch::new().emit(tell(rels, player, "You are nowhere.")));
    };
    let name = noun(body, "thing");
    let mut alloc = Alloc::from_snapshot(snap, rels.entity_seq, ID_BASE)?;
    let entity = alloc.fresh();
    Ok(alloc.seal(
        Patch::new()
            .assert(Fact::new(
                rels.named,
                Tuple::from([Value::Ent(entity), name.clone()]),
            ))
            .assert(Fact::new(
                rels.located,
                Tuple::from([Value::Ent(entity), Value::Ent(room)]),
            ))
            .emit(tell(rels, player, format!("Created {}.", text_of(&name)))),
    ))
}

fn native_dig(snap: &Snapshot, rels: MooRelations, player: Entity, body: &Tuple) -> Result<Patch> {
    let Some(room) = room_of(snap, rels, player)? else {
        return Ok(Patch::new().emit(tell(rels, player, "You are nowhere.")));
    };
    let name = noun(body, "room");
    let mut alloc = Alloc::from_snapshot(snap, rels.entity_seq, ID_BASE)?;
    let destination = alloc.fresh();
    Ok(alloc.seal(
        Patch::new()
            .assert(Fact::new(
                rels.named,
                Tuple::from([Value::Ent(destination), name.clone()]),
            ))
            .assert(Fact::new(
                rels.exits,
                Tuple::from([Value::Ent(room), name.clone(), Value::Ent(destination)]),
            ))
            .assert(Fact::new(
                rels.exits,
                Tuple::from([
                    Value::Ent(destination),
                    Value::text("back"),
                    Value::Ent(room),
                ]),
            ))
            .emit(tell(rels, player, format!("Dug {}.", text_of(&name)))),
    ))
}

fn native_look(snap: &Snapshot, rels: MooRelations, player: Entity) -> Result<Patch> {
    let Some(room) = room_of(snap, rels, player)? else {
        return Ok(Patch::new().emit(tell(rels, player, "You are nowhere.")));
    };
    let names = snap.read(rels.named)?;
    let mut things = Vec::new();
    for (located, diff) in snap.read(rels.located)? {
        let cells = located.as_slice();
        if diff <= 0 || cells.get(1) != Some(&Value::Ent(room)) {
            continue;
        }
        let Some(Value::Ent(entity)) = cells.first() else {
            continue;
        };
        if *entity == player {
            continue;
        }
        if let Some(name) = names.iter().find_map(|(tuple, weight)| {
            (*weight > 0 && tuple.as_slice().first() == Some(&Value::Ent(*entity)))
                .then(|| tuple.as_slice().get(1))
                .flatten()
        }) {
            things.push(text_of(name));
        }
    }
    things.sort();
    let mut exits = snap
        .read(rels.exits)?
        .into_iter()
        .filter_map(|(tuple, diff)| {
            let cells = tuple.as_slice();
            (diff > 0 && cells.first() == Some(&Value::Ent(room)))
                .then(|| cells.get(1))
                .flatten()
                .map(text_of)
        })
        .collect::<Vec<_>>();
    exits.sort();
    let things = if things.is_empty() {
        "You see nothing here.".to_string()
    } else {
        format!("You see: {}.", things.join(", "))
    };
    let exits = if exits.is_empty() {
        "There are no exits.".to_string()
    } else {
        format!("Exits: {}.", exits.join(", "))
    };
    Ok(Patch::new()
        .emit(tell(rels, player, things))
        .emit(tell(rels, player, exits)))
}

fn seed_world(store: &dyn WorldStore, r: MooRelations) -> Result<()> {
    let at = store.current();
    let seeded = store
        .read_at(r.named, at)?
        .into_iter()
        .any(|(t, d)| d > 0 && t.as_slice().first() == Some(&Value::Ent(PLAYER)));
    if seeded {
        return Ok(());
    }
    let e = Value::Ent;
    let t = Value::text;
    let mut facts: Vec<(RelId, Tuple, Diff)> = Vec::new();
    let located = |who: Entity, room: Entity, facts: &mut Vec<_>| {
        facts.push((r.located, Tuple::from([e(who), e(room)]), 1));
    };
    for (who, room) in [
        (PLAYER, FOYER),
        (LAMP, FOYER),
        (KEY, FOYER),
        (BOOK, LIBRARY),
        (ROSE, GARDEN),
        (COIN, MARKET),
        (CAKE, KITCHEN),
        (CAT, FOYER),
        (MERCHANT, MARKET),
    ] {
        located(who, room, &mut facts);
    }
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
        facts.push((r.named, Tuple::from([e(who), t(name)]), 1));
    }
    facts.push((r.players, Tuple::from([e(PLAYER), t("you")]), 1));
    for (who, label) in [(CAT, "a cat"), (MERCHANT, "a merchant")] {
        facts.push((r.person, Tuple::from([e(who)]), 1));
        facts.push((r.label, Tuple::from([e(who), t(label)]), 1));
    }
    facts.push((r.knows, Tuple::from([e(PLAYER), e(PLAYER)]), 1));
    for (obj, coins) in [
        (LAMP, 10),
        (KEY, 1),
        (BOOK, 3),
        (ROSE, 5),
        (COIN, 20),
        (CAKE, 2),
    ] {
        facts.push((r.value, Tuple::from([e(obj), Value::Int(coins)]), 1));
    }
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
        facts.push((r.exits, Tuple::from([e(from), t(way), e(to)]), 1));
    }
    for (from, to) in PATROL {
        facts.push((r.patrol, Tuple::from([e(from), e(to)]), 1));
    }
    located(VAULT_TORCH, VAULT_ANTE, &mut facts);
    located(VAULT_GEM, VAULT_INNER, &mut facts);
    for (who, name) in [
        (VAULT_ANTE, "Vault Antechamber"),
        (VAULT_INNER, "Inner Vault"),
        (VAULT_TORCH, "a guttering torch"),
        (VAULT_GEM, "a great gemstone"),
    ] {
        facts.push((r.named, Tuple::from([e(who), t(name)]), 1));
    }
    for (from, way, to) in [
        (VAULT_ANTE, "north", VAULT_INNER),
        (VAULT_INNER, "south", VAULT_ANTE),
    ] {
        facts.push((r.exits, Tuple::from([e(from), t(way), e(to)]), 1));
    }
    store.commit(&facts)?;
    Ok(())
}

fn seed_entity_counter(store: &dyn WorldStore, r: MooRelations) -> Result<()> {
    if store.read_at(r.entity_seq, store.current())?.is_empty() {
        store.commit(&[(r.entity_seq, Tuple::from([Value::Int(ID_BASE)]), 1)])?;
    }
    Ok(())
}

fn seed_rules(store: &dyn WorldStore, r: MooRelations) -> Result<()> {
    if !store.read_at(r.cardval, store.current())?.is_empty() {
        return Ok(());
    }
    let i = Value::Int;
    let mut facts = Vec::new();
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
    store.commit(&facts)?;
    Ok(())
}
