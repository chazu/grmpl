//! Session replay (P10): the whole world is a deterministic function of its
//! message log. Replaying the same client script into a fresh store reproduces
//! the world **bit for bit** — the enablers are P0 determinism and P3/P4's rule
//! that every input (commands, allocation, timers) enters only as committed
//! data. A fork is a checkpoint that same replay can start from.

use std::sync::Arc;

use grmpl::{MooRuntime, Server};
use grmpl_core::{EditionStore, RelId, TraceStore, WorldStore};

/// The store's logical projection: every relation's raw updates in commit
/// order. Plan v4 §2 defines replay/fork identity here rather than over
/// physical bytes, so the law can be stated once for every substrate.
fn dump(store: &dyn TraceStore, relations: &[RelId]) -> Vec<(RelId, Vec<grmpl_core::Update>)> {
    grmpl_conformance::logical_dump(store, relations).unwrap()
}

/// A fixed client script: two players build a room, walk around, and race for a
/// lamp — the P3 acceptance scenario, driven entirely through client commands so
/// there is no privileged setup path to make replay cheat.
fn run_script(store: Arc<dyn WorldStore>) -> Arc<Server> {
    let server = Server::new(MooRuntime::builtin(store).unwrap());

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
    server
}

/// A store the script has been played into once, plus its temp dir guard.
fn played(
    case: &grmpl_conformance::Case,
) -> (Arc<dyn TraceStore>, Vec<RelId>, grmpl_conformance::Case) {
    // A fresh store of the same substrate: replay compares two independent
    // plays of the same script, so each needs its own empty world.
    let sib = case.sibling();
    let server = run_script(sib.shared());
    let relations = server.world().relations().all();
    (sib.trace(), relations, sib)
}

#[test]
fn replaying_a_session_reproduces_the_world() {
    grmpl_conformance::for_each_store(|c| {
        let (a, relations, _ag) = played(c);
        let (b, _, _bg) = played(c);

        assert!(a.current().0 > 0, "the script committed real editions");
        assert_eq!(
            a.current(),
            b.current(),
            "two runs allocate the same editions"
        );
        assert_eq!(
            dump(a.as_ref(), &relations),
            dump(b.as_ref(), &relations),
            "{}: replaying the same client script must reproduce the trace",
            c.name
        );
    });
}

#[test]
fn a_fork_is_a_replay_checkpoint() {
    // Play a session, fork it mid-life, then drive both the source and the fork
    // with the *same* remaining commands: they stay identical. The fork is a
    // checkpoint the session's log can be replayed from (roadmap: "a checkpoint
    // is a fork point"), and on the Ent it is a new branch in the same
    // granfilade — O(edit), sharing every node, not an O(state) copy.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(grmpl_ent::EntStore::open(dir.path()).unwrap());
    let shared: Arc<dyn WorldStore> = store.clone();
    let server = Server::new(MooRuntime::builtin(shared).unwrap());
    let relations = server.world().relations().all();
    let mut builder = server.login("builder").unwrap();
    builder.submit("dig hall").unwrap();

    let fork = Arc::new(store.fork_at(store.current()).unwrap());
    assert_eq!(
        dump(store.as_ref(), &relations),
        dump(fork.as_ref(), &relations),
        "the fork checkpoint must be a faithful copy"
    );

    // Replay identical remaining commands onto both, through independent servers
    // (the fork reconnects `builder` by identity — no re-spawn).
    for target in [Arc::clone(&store), Arc::clone(&fork)] {
        let shared: Arc<dyn WorldStore> = target;
        let server = Server::new(MooRuntime::builtin(shared).unwrap());
        let mut builder = server.login("builder").unwrap();
        builder.submit("go hall").unwrap();
        builder.submit("create lamp").unwrap();
    }

    assert_eq!(
        dump(store.as_ref(), &relations),
        dump(fork.as_ref(), &relations),
        "replaying the same commands from the fork reproduces the source"
    );
    assert_eq!(store.current(), fork.current());
}
