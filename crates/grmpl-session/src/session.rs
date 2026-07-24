//! The session engine: provisioning and the single-writer command loop (P3).
//!
//! A connection becomes a player in three steps (DESIGN.md §5, the ticket):
//!   1. **identity** — a name is the minimal credential; the same name rebinds
//!      to the same player entity across reconnects (durable identity in the
//!      `PLAYER` relation);
//!   2. **spawn as a commit** — a brand-new player is allocated an entity id
//!      from the replay-safe counter and placed in the root room, all in one
//!      atomic patch;
//!   3. **a `Process`** — bound to a per-player inbox slice, driving the actor
//!      loop.
//!
//! Commits are serialized behind one writer (`lock`). That is the interim
//! **single-writer inbox seq scheme**: inbox positions and the entity counter
//! advance without cross-writer races. P4 replaces it with a durable allocator.

use std::io::Write;
use std::sync::{Arc, Mutex};

use grmpl_core::{
    Edition, Entity, Error, Fact, NoSchemas, Patch, Result, TraceStore, Tuple, Value,
};
use grmpl_diff::Query;
use grmpl_proc::{commit_patch, enqueue, Alloc, CommitOutcome, Process};

use crate::watch::Subscription;
use crate::world::{
    player_behavior, world_authority, CURSOR, ENTITY_SEQ, ID_BASE, INBOX, LOCATED, NAMED, PLAYER,
    ROOT_ROOM, TELL,
};

/// A running world server. Holds the store and serializes all commits behind a
/// single writer (the interim P3 scheme).
pub struct Server {
    store: Arc<dyn TraceStore>,
    lock: Mutex<()>,
}

impl Server {
    /// Wrap a store. Call [`init`](Self::init) once to seed the world.
    pub fn new(store: Arc<dyn TraceStore>) -> Arc<Server> {
        Arc::new(Server { store, lock: Mutex::new(()) })
    }

    /// The underlying store (for tests and observers).
    pub fn store(&self) -> &Arc<dyn TraceStore> {
        &self.store
    }

    /// Seed the world if it is empty: name the root room and start the entity
    /// counter above the fixed ids. Idempotent — a no-op once the root room
    /// exists (safe across reopens).
    pub fn init(&self) -> Result<()> {
        let _w = self.lock.lock().unwrap();
        let at = self.store.current();
        let seeded = self
            .store
            .read_at(NAMED, at)?
            .into_iter()
            .any(|(t, d)| d > 0 && t.as_slice().first() == Some(&Value::Ent(ROOT_ROOM)));
        if seeded {
            return Ok(());
        }
        self.store.commit(&[
            (NAMED, Tuple::from([Value::Ent(ROOT_ROOM), Value::text("The Void")]), 1),
            (ENTITY_SEQ, Tuple::from([Value::Int(ID_BASE)]), 1),
        ])?;
        Ok(())
    }

    /// Provision a connection into a player session. Reconnects rebind an
    /// existing identity; a new name spawns a fresh player as one commit.
    pub fn login(self: &Arc<Self>, name: &str) -> Result<Session> {
        let _w = self.lock.lock().unwrap();
        let player = match self.lookup_identity(name)? {
            Some(e) => e, // reconnect: identity persists
            None => self.spawn_player(name)?,
        };
        Ok(Session {
            server: Arc::clone(self),
            player,
            process: Process {
                entity: player,
                authority: world_authority(),
                inbox: INBOX,
                cursor_rel: CURSOR,
                behavior: player_behavior(player),
            },
            out_cursor: self.store.current(),
            subscription: None,
        })
    }

    /// The player entity previously bound to `name`, if any.
    fn lookup_identity(&self, name: &str) -> Result<Option<Entity>> {
        let at = self.store.current();
        let want = Value::text(name);
        for (t, d) in self.store.read_at(PLAYER, at)? {
            if d > 0 && t.as_slice().get(1) == Some(&want) {
                if let Some(Value::Ent(e)) = t.as_slice().first() {
                    return Ok(Some(*e));
                }
            }
        }
        Ok(None)
    }

    /// Allocate a player entity, record its identity, and place it in the root
    /// room — one atomic commit ("spawn as a commit").
    fn spawn_player(&self, name: &str) -> Result<Entity> {
        let mut alloc = Alloc::read(&*self.store, ENTITY_SEQ, ID_BASE)?;
        let player = alloc.fresh();
        let patch = Patch::new()
            .assert(Fact::new(NAMED, Tuple::from([Value::Ent(player), Value::text(name)])))
            .assert(Fact::new(PLAYER, Tuple::from([Value::Ent(player), Value::text(name)])))
            .assert(Fact::new(LOCATED, Tuple::from([Value::Ent(player), Value::Ent(ROOT_ROOM)])));
        let patch = alloc.seal(patch);
        match commit_patch(&*self.store, &NoSchemas, &patch, &world_authority())? {
            CommitOutcome::Committed(_) => Ok(player),
            CommitOutcome::Rejected => {
                Err(Error::Store("player spawn was rejected".into()))
            }
        }
    }
}

/// One connected player: an inbox writer plus its driving process. Not `Clone`
/// (it owns a behavior); one per connection.
pub struct Session {
    server: Arc<Server>,
    player: Entity,
    process: Process,
    /// The edition through which this session has already delivered `TELL` text.
    out_cursor: Edition,
    /// The player's live view subscription, if it has issued `watch`. Its drain
    /// cursor is durable (in the store), so this is only the in-memory handle —
    /// a reconnect re-subscribes and resumes from the persisted cursor.
    subscription: Option<Subscription>,
}

impl Session {
    /// The player entity this session drives.
    pub fn player(&self) -> Entity {
        self.player
    }

    /// Submit one command line: enqueue it, run the process to idle, and return
    /// everything the player was told as a result. Serialized behind the
    /// server's single writer.
    pub fn submit(&mut self, line: &str) -> Result<Vec<String>> {
        // Hold the store/lock via a clone so the writer guard doesn't borrow
        // `self` (which we mutate to advance the output cursor).
        let server = Arc::clone(&self.server);
        let _w = server.lock.lock().unwrap();
        let store_arc = Arc::clone(&server.store);
        let store: &dyn TraceStore = &*store_arc;

        let seq = self.next_inbox_seq(store)?;
        enqueue(store, INBOX, self.player, seq, tokenize(line))?;
        self.process.run_to_idle(store, &NoSchemas)?;

        self.drain_output(store)
    }

    /// Subscribe this player to the default reactive view — the set of named
    /// things in the world — so subsequent world changes (any player's `create`
    /// / `dig`) stream to this socket as activations. Idempotent: installs the
    /// on-watch once (skip-initial), and a reconnect resumes the durable stream
    /// without re-installing. The `watch` key is the player entity (one
    /// subscription per player).
    pub fn subscribe(&mut self) -> Result<()> {
        let server = Arc::clone(&self.server);
        let _w = server.lock.lock().unwrap();
        let store: &dyn TraceStore = &*server.store;

        let sub = Subscription::new(Query::rel(NAMED), self.player, self.player);
        if !sub.is_installed(store)? {
            sub.install(store)?;
        }
        self.subscription = Some(sub);
        Ok(())
    }

    /// Pump the player's subscription and stream any newly-materialized
    /// activations to `out`, exactly as `TELL` text is streamed — reactive push.
    /// A no-op (returns `0`) if the player has not subscribed. Serialized behind
    /// the server's single writer.
    pub fn push_activations(&mut self, out: &mut dyn Write) -> Result<usize> {
        let sub = match &self.subscription {
            Some(s) => s,
            None => return Ok(0),
        };
        let server = Arc::clone(&self.server);
        let _w = server.lock.lock().unwrap();
        let store: &dyn TraceStore = &*server.store;
        Ok(sub.pump_and_drain(store, out)?.len())
    }

    /// The next free inbox position for this player: one past the highest seq
    /// present (single-writer, so no gap or collision).
    fn next_inbox_seq(&self, store: &dyn TraceStore) -> Result<i64> {
        let at = store.current();
        let mut next = 0;
        for (t, d) in store.read_at(INBOX, at)? {
            let s = t.as_slice();
            if d > 0 && s.first() == Some(&Value::Ent(self.player)) {
                if let Some(Value::Int(seq)) = s.get(1) {
                    next = next.max(*seq + 1);
                }
            }
        }
        Ok(next)
    }

    /// Collect `TELL` text addressed to this player committed since the last
    /// drain, advancing the cursor. The player filter keeps another player's
    /// interleaved output out of this stream.
    fn drain_output(&mut self, store: &dyn TraceStore) -> Result<Vec<String>> {
        let to = store.current();
        let mut out = Vec::new();
        for u in store.scan_updates(TELL, self.out_cursor, to)? {
            let s = u.tuple.as_slice();
            if u.diff > 0 && s.first() == Some(&Value::Ent(self.player)) {
                if let Some(Value::Text(text)) = s.get(1) {
                    out.push(text.to_string());
                }
            }
        }
        self.out_cursor = to;
        Ok(out)
    }
}

/// Split a command line into a tuple of text tokens the grammar parses.
fn tokenize(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}
