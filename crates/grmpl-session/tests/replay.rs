//! Session replay (P10): the whole world is a deterministic function of its
//! message log. Replaying the same client script into a fresh store reproduces
//! the world **bit for bit** — the enablers are P0 determinism and P3/P4's rule
//! that every input (commands, allocation, timers) enters only as committed
//! data. A fork is a checkpoint that same replay can start from.

use std::sync::Arc;

use grmpl_core::{EditionStore, TraceStore};
use grmpl_session::Server;
use grmpl_store::FjallStore;

/// A fixed client script: two players build a room, walk around, and race for a
/// lamp — the P3 acceptance scenario, driven entirely through client commands so
/// there is no privileged setup path to make replay cheat.
fn run_script(store: Arc<FjallStore>) {
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
fn played() -> (Arc<FjallStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FjallStore::open(dir.path()).unwrap());
    run_script(Arc::clone(&store));
    (store, dir)
}

#[test]
fn replaying_a_session_reproduces_the_world_bit_for_bit() {
    let (a, _da) = played();
    let (b, _db) = played();

    assert!(a.current().0 > 0, "the script committed real editions");
    assert_eq!(
        a.current(),
        b.current(),
        "two runs allocate the same editions"
    );
    assert_eq!(
        a.canonical_dump().unwrap(),
        b.canonical_dump().unwrap(),
        "replaying the same client script yields a bit-identical trace"
    );
}

#[test]
fn a_fork_is_a_replay_checkpoint() {
    // Play a session, fork it mid-life, then drive both the source and the fork
    // with the *same* remaining commands: they stay bit-identical. The fork is a
    // checkpoint the session's log can be replayed from (roadmap: "a checkpoint
    // is a fork point").
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FjallStore::open(dir.path()).unwrap());
    let server = Server::new(Arc::clone(&store) as Arc<dyn TraceStore>);
    server.init().unwrap();
    let mut builder = server.login("builder").unwrap();
    builder.submit("dig hall").unwrap();

    // Fork here — the checkpoint. It copies the whole domain O(domain).
    let fork_dir = tempfile::tempdir().unwrap();
    let fork = Arc::new(store.fork(fork_dir.path()).unwrap());
    assert_eq!(
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "the fork checkpoint is a bit-identical copy"
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
        store.canonical_dump().unwrap(),
        fork.canonical_dump().unwrap(),
        "replaying the same commands from the fork reproduces the source bit for bit"
    );
    assert_eq!(store.current(), fork.current());
}
