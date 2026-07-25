//! **E7: the full MOO session runtime runs over the ent-native store.**
//!
//! The strongest cutover check: `grmpl-session` — provisioning (spawn-as-a-commit),
//! the single-writer command loop, and the world verbs (`dig`/`go`/`create`/
//! `take`/`look`) — depends only on `Arc<dyn TraceStore>`. Standing a `Server` up
//! over [`EntStore`] and playing the MOO exercises `commit_patch`, the replay-safe
//! allocator, `enqueue_seq`, and reactive drains all against the Ent as the
//! substrate, with no LSM in sight.

use std::sync::Arc;

use grmpl_core::TraceStore;
use grmpl_ent::EntStore;
use grmpl_session::Server;

#[test]
fn moo_session_runtime_runs_over_the_ent_store() {
    let store: Arc<dyn TraceStore> = Arc::new(EntStore::new());
    let server = Server::new(store);
    server.init().unwrap();

    let mut alice = server.login("alice").unwrap();
    // Provisioned into the root room; build and explore, all over the ent store.
    alice.submit("dig hall").unwrap();
    let moved = alice.submit("go hall").unwrap();
    assert!(moved.iter().any(|s| s.contains("move")), "go did not move: {moved:?}");

    let created = alice.submit("create lamp").unwrap();
    assert!(created.iter().any(|s| s.contains("Created")), "create failed: {created:?}");

    let taken = alice.submit("take lamp").unwrap();
    assert!(taken.iter().any(|s| s.contains("Taken")), "take failed: {taken:?}");

    // A second player provisions and sees the world independently — durable,
    // shared state on the ent store.
    let mut bob = server.login("bob").unwrap();
    let looked = bob.submit("look").unwrap();
    // bob starts in the root room (not the hall), so he does not see alice's lamp.
    assert!(!looked.iter().any(|s| s.contains("lamp")), "bob wrongly saw the lamp: {looked:?}");
}

#[test]
fn moo_session_runtime_is_durable_across_reopen_on_the_ent_store() {
    let dir = tempfile::tempdir().unwrap();
    // Build a little world, then drop the server + store.
    {
        let store: Arc<dyn TraceStore> = Arc::new(EntStore::open(dir.path()).unwrap());
        let server = Server::new(store);
        server.init().unwrap();
        let mut alice = server.login("alice").unwrap();
        alice.submit("dig library").unwrap();
        alice.submit("create tome").unwrap();
    }
    // Reopen the ent store: the same identity and world are still there.
    let store: Arc<dyn TraceStore> = Arc::new(EntStore::open(dir.path()).unwrap());
    let server = Server::new(store);
    server.init().unwrap();
    let mut alice = server.login("alice").unwrap(); // reconnect rebinds the identity
    let looked = alice.submit("look").unwrap();
    // alice is back in the root room; the `library` exit she dug persists.
    assert!(
        looked.iter().any(|s| s.to_lowercase().contains("library")),
        "the dug exit did not survive reopen: {looked:?}"
    );
}
