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
//! **There is no writer lock.** Every commit this module makes is guarded by a
//! precondition and resolves under the ordinary optimistic protocol:
//!
//! * inbox seqs come from the durable, race-safe P4
//!   [`SeqAlloc`](grmpl_proc::SeqAlloc) via
//!   [`enqueue_seq`](grmpl_proc::enqueue_seq), seeded once per player at spawn;
//! * entity ids come from the now-guarded [`Alloc`], whose `seal` preconditions
//!   the counter row, so two spawns (or two `dig`s) racing the counter resolve to
//!   one winner and the loser retries against the winner's value;
//! * a rejected command is retried by
//!   [`run_to_idle_retrying`](Process::run_to_idle_retrying) under a
//!   [`Backoff`], which rebuilds the patch from the *current* world — so a lost
//!   `take lamp` race re-decides against the state that beat it rather than
//!   silently doing nothing.
//!
//! The one un-raced path is [`Server::init`], which seeds the world's counters.
//! That is the sanctioned exception the substrate forces on any counter (there is
//! no "assert-if-absent" to precondition a first allocation on) and it is
//! precisely where [`SeqAlloc::seed`](grmpl_proc::SeqAlloc::seed) and
//! [`Alloc::seed`](grmpl_proc::Alloc::seed) exist to be used.
//!
//! The session layer is therefore serialized only by the *store*, which is what
//! makes the substrate's own commit throughput observable through it.

use std::io::Write;
use std::sync::Arc;

use grmpl_core::{
    Edition, Entity, Error, Fact, NoSchemas, Patch, Result, TraceStore, Tuple, Value,
};
use grmpl_diff::{Query, Snapshot};
use grmpl_proc::{
    commit_patch, commit_retrying, enqueue_seq, Alloc, Backoff, CommitOutcome, Process, SeqAlloc,
};

use crate::watch::Subscription;
use crate::world::{
    player_behavior, world_authority, CURSOR, ENTITY_SEQ, ID_BASE, INBOX, INBOX_SEQ, LOCATED,
    NAMED, PLAYER, ROOT_ROOM, TELL,
};

/// A running world server. Holds the store; commits are ordinary optimistic
/// commits, not serialized here.
pub struct Server {
    store: Arc<dyn TraceStore>,
    /// The retry policy every commit this server makes runs under.
    policy: Backoff,
}

impl Server {
    /// Wrap a store. Call [`init`](Self::init) once to seed the world.
    pub fn new(store: Arc<dyn TraceStore>) -> Arc<Server> {
        Arc::new(Server { store, policy: Backoff::default() })
    }

    /// Wrap a store with an explicit retry policy — for tests that want to pin
    /// contention behavior, and for deployments that want a different budget.
    pub fn with_policy(store: Arc<dyn TraceStore>, policy: Backoff) -> Arc<Server> {
        Arc::new(Server { store, policy })
    }

    /// The underlying store (for tests and observers).
    pub fn store(&self) -> &Arc<dyn TraceStore> {
        &self.store
    }

    /// Seed the world if it is empty: name the root room and start the entity
    /// counter above the fixed ids. Idempotent — a no-op once the root room
    /// exists (safe across reopens).
    ///
    /// **The un-raced path.** This is world creation, called once before any
    /// player connects; it is the sanctioned place to seed a counter row that no
    /// precondition can guard (see the module docs). Calling it concurrently with
    /// itself is not supported and not needed.
    pub fn init(&self) -> Result<()> {
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
    ///
    /// Concurrent logins need no lock. Two new names racing contend only on the
    /// entity counter, and the guarded [`Alloc`] resolves that to one winner per
    /// commit. Two logins of the *same* new name contend on it too — and because
    /// the loser retries by re-reading the world, it finds the winner's `PLAYER`
    /// row and rebinds to that identity instead of spawning a second one. The
    /// durable-identity invariant is upheld by the counter guard, not by
    /// serialization — see [`spawn_player`](Self::spawn_player) for the part of
    /// that argument which is easy to get wrong.
    pub fn login(self: &Arc<Self>, name: &str) -> Result<Session> {
        let player = match self.lookup_identity(&Snapshot::at_current(&*self.store), name)? {
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

    /// The player entity bound to `name` as-of `snap`, if any.
    ///
    /// Takes a pinned snapshot rather than reading the store, because
    /// [`spawn_player`](Self::spawn_player) needs this answer and the entity
    /// counter to come from the *same* edition.
    fn lookup_identity(&self, snap: &Snapshot, name: &str) -> Result<Option<Entity>> {
        let want = Value::text(name);
        for (t, d) in snap.read(PLAYER)? {
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
    ///
    /// The guarded [`Alloc`] makes this a racing commit, so it runs under the
    /// server's retry policy. Every attempt re-reads the world: it re-checks
    /// whether a concurrent login already bound this name (in which case that
    /// identity is *the* identity and nothing is spawned), and otherwise draws a
    /// fresh id from the counter as it now stands. `player` is therefore whatever
    /// the winning attempt allocated, never a stale id from a rejected one.
    ///
    /// **Both reads must come from one pinned edition, and this is the subtle
    /// part.** The counter precondition is what makes "one name, one identity"
    /// hold, but only because a spawn *always* bumps the counter: if a peer
    /// spawned between our two reads, our precondition must fail. Reading the
    /// identity at one edition and the counter at a later one breaks exactly
    /// that — the lookup says "no such name", the counter read already reflects
    /// the peer's bump, so the precondition holds and a second player is spawned
    /// for the same name. Pinning one `Snapshot` for both closes it: anything
    /// that could have bound the name also moved the counter we preconditioned
    /// on, so we lose the race and adopt the winner on the retry.
    fn spawn_player(&self, name: &str) -> Result<Entity> {
        let mut spawned = None;
        commit_retrying(self.policy.clone(), || {
            // One edition for the identity lookup *and* the counter read.
            let snap = Snapshot::at_current(&*self.store);
            // A peer may have bound this name since the last attempt; if so this
            // spawn is moot and the caller must use the peer's entity.
            if let Some(existing) = self.lookup_identity(&snap, name)? {
                spawned = Some(existing);
                // Nothing to write — report a committed no-op so the loop ends.
                return Ok(CommitOutcome::Committed(snap.edition));
            }
            let mut alloc = Alloc::from_snapshot(&snap, ENTITY_SEQ, ID_BASE)?;
            let player = alloc.fresh();
            // The player's durable inbox-seq counter is seeded **in this same
            // commit**. A first `SeqAlloc` allocation has no row to precondition
            // on, so the seed must be un-raced — and the only thing that makes it
            // un-raced here is riding inside the entity-counter guard, which
            // exactly one spawn wins. Seeding it afterwards, as a second commit,
            // would let two logins of one name seed it twice and hand out a
            // duplicate seq.
            let seqs = SeqAlloc::from_snapshot(&snap, INBOX_SEQ, vec![Value::Ent(player)])?;
            let patch = alloc.seal(
                seqs.seed(Patch::new())
                    .assert(Fact::new(NAMED, Tuple::from([Value::Ent(player), Value::text(name)])))
                    .assert(Fact::new(PLAYER, Tuple::from([Value::Ent(player), Value::text(name)])))
                    .assert(Fact::new(
                        LOCATED,
                        Tuple::from([Value::Ent(player), Value::Ent(ROOT_ROOM)]),
                    )),
            );
            let outcome = commit_patch(&*self.store, &NoSchemas, &patch, &world_authority())?;
            if matches!(outcome, CommitOutcome::Committed(_)) {
                spawned = Some(player);
            }
            Ok(outcome)
        })?;
        spawned.ok_or_else(|| Error::Store("player spawn committed without an entity".into()))
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
    /// everything the player was told as a result.
    ///
    /// Nothing is serialized here. The enqueue draws a race-safe seq; the
    /// command is committed under the server's retry policy, so a command that
    /// loses a race is *re-decided against the winner's world* rather than
    /// dropped — which is what turns a lost `take lamp` into "you don't see that
    /// here" instead of silence.
    pub fn submit(&mut self, line: &str) -> Result<Vec<String>> {
        let store_arc = Arc::clone(&self.server.store);
        let store: &dyn TraceStore = &*store_arc;
        let policy = self.server.policy.clone();

        enqueue_seq(store, INBOX, INBOX_SEQ, self.player, tokenize(line))?;
        self.process.run_to_idle_retrying(store, &NoSchemas, policy)?;

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
    /// A no-op (returns `0`) if the player has not subscribed.
    ///
    /// Needs no lock: the pump's commit is preconditioned on its own watch
    /// cursor and the drain's on its own delivery cursor (P5), so concurrent
    /// pushes are already race-safe against each other by construction.
    pub fn push_activations(&mut self, out: &mut dyn Write) -> Result<usize> {
        let sub = match &self.subscription {
            Some(s) => s,
            None => return Ok(0),
        };
        let server = Arc::clone(&self.server);
        let store: &dyn TraceStore = &*server.store;
        Ok(sub.pump_and_drain(store, out)?.len())
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
