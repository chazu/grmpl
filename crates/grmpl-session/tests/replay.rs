//! Session replay (P10): the whole world is a deterministic function of its
//! message log. Replaying the same client script into a fresh store reproduces
//! the world **bit for bit** — the enablers are P0 determinism and P3/P4's rule
//! that every input (commands, allocation, timers) enters only as committed
//! data. A fork is a checkpoint that same replay can start from.

use std::sync::Arc;

use grmpl_core::{EditionStore, TraceStore};
use grmpl_session::Server;

/// Every relation the session world writes — the domain of the logical
/// projection replay identity is defined over.
const WORLD_RELS: &[grmpl_core::RelId] = &[
    grmpl_session::world::LOCATED,
    grmpl_session::world::NAMED,
    grmpl_session::world::HELD,
    grmpl_session::world::EXITS,
    grmpl_session::world::PLAYER,
    grmpl_session::world::INBOX,
    grmpl_session::world::CURSOR,
    grmpl_session::world::INBOX_SEQ,
    grmpl_session::world::TELL,
    grmpl_session::world::ENTITY_SEQ,
];

/// The store's logical projection: every relation's raw updates in commit
/// order. Plan v4 §2 defines replay/fork identity here rather than over
/// physical bytes, so the law can be stated once for every substrate.
fn dump(store: &dyn grmpl_core::TraceStore) -> Vec<(grmpl_core::RelId, Vec<grmpl_core::Update>)> {
    grmpl_conformance::logical_dump(store, WORLD_RELS).unwrap()
}

/// A fixed client script: two players build a room, walk around, and race for a
/// lamp — the P3 acceptance scenario, driven entirely through client commands so
/// there is no privileged setup path to make replay cheat.
fn run_script(store: Arc<dyn TraceStore>) {
    let server = Server::new(store as Arc<dyn TraceStore>);
    server.init().unwrap();

    let mut builder = server.login("builder").unwrap();
    builder.submit("dig hall").unwrap();
    builder.submit("go hall").unwrap();
    builder.submit("create lamp").unwrap();

    let mut alice = server.login("alice").unwrap();
    let mut bob = server.login("bob").unwrap();
    alice.submit("go hall").unwrap();
    bob.submit("go hall").unwrap();
    alice.submit("look").unwrap();
    alice.submit("take lamp").unwrap();
    bob.submit("take lamp").unwrap(); // the loser: exactly one commit wins
    builder.submit("look").unwrap();
}

/// A store the script has been played into once, plus its temp dir guard.
fn played(case: &grmpl_conformance::Case) -> (Arc<dyn TraceStore>, grmpl_conformance::Case) {
    // A fresh store of the same substrate: replay compares two independent
    // plays of the same script, so each needs its own empty world.
    let sib = case.sibling();
    let store = sib.trace();
    run_script(Arc::clone(&store));
    (store, sib)
}

#[test]
fn replaying_a_session_reproduces_the_world() {
    grmpl_conformance::for_each_store(|c| {
        let (a, _ag) = played(c);
        let (b, _bg) = played(c);

        assert!(a.current().0 > 0, "the script committed real editions");
        assert_eq!(
            a.current(),
            b.current(),
            "two runs allocate the same editions"
        );
        assert_eq!(
            dump(a.as_ref()),
            dump(b.as_ref()),
            "{}: replaying the same client script must reproduce the trace",
            c.name
        );
    });
}

#[test]
fn a_fork_is_a_replay_checkpoint() {
    // LSM-only for now. Forking a *durable* store into a new location is
    // `FjallStore::fork`; the ent's `fork_at` shares structure but yields an
    // in-memory store, so it cannot yet stand in here. Plan v5 G-6 (durable fork
    // sharing one granfilade) is what makes this law substrate-neutral; until
    // then, saying so is more honest than quietly testing one substrate twice.
    {
        let dir = tempfile::tempdir().unwrap();
        // Play a session, fork it mid-life, then drive both the source and the fork
        // with the *same* remaining commands: they stay identical. The fork is a
        // checkpoint the session's log can be replayed from (roadmap: "a checkpoint
        // is a fork point").
        let store = Arc::new(grmpl_store::FjallStore::open(dir.path()).unwrap());
        let server = Server::new(Arc::clone(&store) as Arc<dyn TraceStore>);
        server.init().unwrap();
        let mut builder = server.login("builder").unwrap();
        builder.submit("dig hall").unwrap();

        // Fork here — the checkpoint. It copies the whole domain O(domain).
        let fork_dir = tempfile::tempdir().unwrap();
        let fork = Arc::new(store.fork(fork_dir.path()).unwrap());
        assert_eq!(
            dump(store.as_ref()),
            dump(fork.as_ref()),
            "the fork checkpoint must be a faithful copy"
        );

        // Replay identical remaining commands onto both, through independent servers
        // (the fork reconnects `builder` by identity — no re-spawn).
        for target in [Arc::clone(&store), Arc::clone(&fork)] {
            let server = Server::new(target as Arc<dyn TraceStore>);
            let mut builder = server.login("builder").unwrap();
            builder.submit("go hall").unwrap();
            builder.submit("create lamp").unwrap();
        }

        assert_eq!(
            dump(store.as_ref()),
            dump(fork.as_ref()),
            "replaying from the fork must reproduce the source"
        );
        assert_eq!(store.current(), fork.current());
    }
}
