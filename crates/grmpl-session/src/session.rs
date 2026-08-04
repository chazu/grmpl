//! Player provisioning and command sessions over a compiled MOO world.
//!
//! [`Server`] is transport-independent. Terminal and TCP adapters both bind a
//! player identity, enqueue text into the same language-defined inbox, and run
//! the same process to quiescence. Rust only supplies capabilities the language
//! cannot yet express: durable entity allocation and formatted `look` output.

use std::io::Write;
use std::sync::Arc;

use grmpl_core::{Edition, Entity, Error, Fact, Patch, Result, Tuple, Value, WorldStore};
use grmpl_diff::Snapshot;
use grmpl_proc::{commit_patch, commit_retrying, Alloc, Backoff, CommitOutcome, Process, SeqAlloc};

use crate::moo::{MooRuntime, FOYER, ID_BASE};
use crate::watch::Subscription;

/// A running compiled world, independent of its terminal or TCP adapter.
pub struct Server {
    world: Arc<MooRuntime>,
    policy: Backoff,
}

impl Server {
    pub fn new(world: Arc<MooRuntime>) -> Arc<Server> {
        Arc::new(Server {
            world,
            policy: Backoff::default(),
        })
    }

    pub fn with_policy(world: Arc<MooRuntime>, policy: Backoff) -> Arc<Server> {
        Arc::new(Server { world, policy })
    }

    pub fn world(&self) -> &Arc<MooRuntime> {
        &self.world
    }

    pub fn store(&self) -> Arc<dyn WorldStore> {
        self.world.shared_store()
    }

    /// Bind an existing durable identity or allocate a new player atomically.
    pub fn login(self: &Arc<Self>, name: &str) -> Result<Session> {
        let store = self.world.store();
        let player = match self.lookup_identity(&Snapshot::at_current(store), name)? {
            Some(entity) => entity,
            None => self.spawn_player(name)?,
        };
        let process = self
            .world
            .player_process(player)
            .map_err(|error| Error::Store(format!("could not bind player process: {error}")))?;
        Ok(Session {
            server: Arc::clone(self),
            player,
            process,
            out_cursor: store.current(),
            subscription: None,
        })
    }

    fn lookup_identity(&self, snap: &Snapshot, name: &str) -> Result<Option<Entity>> {
        let want = Value::text(name);
        for (tuple, diff) in snap.read(self.world.relations().players)? {
            if diff > 0 && tuple.as_slice().get(1) == Some(&want) {
                if let Some(Value::Ent(entity)) = tuple.as_slice().first() {
                    return Ok(Some(*entity));
                }
            }
        }
        Ok(None)
    }

    /// Allocate the player, seed its inbox sequence, bind its name, and place it
    /// in the foyer in one guarded commit. Every retry reads identity and entity
    /// counter from the same snapshot, so racing logins for one name converge.
    fn spawn_player(&self, name: &str) -> Result<Entity> {
        let rels = self.world.relations();
        let store = self.world.store();
        let authority = self.world.player_authority();
        let mut spawned = None;
        commit_retrying(self.policy.clone(), || {
            let snap = Snapshot::at_current(store);
            if let Some(existing) = self.lookup_identity(&snap, name)? {
                spawned = Some(existing);
                return Ok(CommitOutcome::Committed(snap.edition));
            }
            let mut alloc = Alloc::from_snapshot(&snap, rels.entity_seq, ID_BASE)?;
            let player = alloc.fresh();
            let seqs = SeqAlloc::from_snapshot(&snap, rels.inbox_seq, vec![Value::Ent(player)])?;
            let patch = alloc.seal(
                seqs.seed(Patch::new())
                    .assert(Fact::new(
                        rels.named,
                        Tuple::from([Value::Ent(player), Value::text(name)]),
                    ))
                    .assert(Fact::new(
                        rels.players,
                        Tuple::from([Value::Ent(player), Value::text(name)]),
                    ))
                    .assert(Fact::new(
                        rels.located,
                        Tuple::from([Value::Ent(player), Value::Ent(FOYER)]),
                    ))
                    .assert(Fact::new(
                        rels.knows,
                        Tuple::from([Value::Ent(player), Value::Ent(player)]),
                    )),
            );
            let outcome = commit_patch(store, store, &patch, &authority)?;
            if matches!(outcome, CommitOutcome::Committed(_)) {
                spawned = Some(player);
            }
            Ok(outcome)
        })?;
        spawned.ok_or_else(|| Error::Store("player spawn committed without an entity".into()))
    }
}

/// One connected player and its durable process cursor.
pub struct Session {
    server: Arc<Server>,
    player: Entity,
    process: Process,
    out_cursor: Edition,
    subscription: Option<Subscription>,
}

impl Session {
    pub fn player(&self) -> Entity {
        self.player
    }

    /// Enqueue one command, run the compiled world process to idle, and return
    /// all text emitted to this player by that command.
    pub fn submit(&mut self, line: &str) -> Result<Vec<String>> {
        let shared = self.server.world.shared_store();
        let store = &*shared;
        self.server.world.enqueue(self.player, line)?;
        self.process
            .run_to_idle_retrying(store, store, self.server.policy.clone())?;
        self.drain_output(store)
    }

    /// Subscribe to the MOO program's default `watch world` declaration.
    pub fn subscribe(&mut self) -> Result<()> {
        let store = self.server.world.store();
        let sub = self
            .server
            .world
            .subscription(self.player, self.player)
            .map_err(|error| Error::Store(format!("could not bind world watch: {error}")))?;
        if !sub.is_installed(store)? {
            sub.install(store)?;
        }
        self.subscription = Some(sub);
        Ok(())
    }

    pub fn push_activations(&mut self, out: &mut dyn Write) -> Result<usize> {
        let Some(subscription) = &self.subscription else {
            return Ok(0);
        };
        Ok(subscription
            .pump_and_drain(self.server.world.store(), out)?
            .len())
    }

    fn drain_output(&mut self, store: &dyn WorldStore) -> Result<Vec<String>> {
        let to = store.current();
        let mut out = Vec::new();
        for update in store.scan_updates(self.server.world.relations().tell, self.out_cursor, to)? {
            let cells = update.tuple.as_slice();
            if update.diff > 0 && cells.first() == Some(&Value::Ent(self.player)) {
                if let Some(Value::Text(text)) = cells.get(1) {
                    out.push(text.to_string());
                }
            }
        }
        self.out_cursor = to;
        Ok(out)
    }
}
