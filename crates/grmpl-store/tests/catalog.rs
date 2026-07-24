//! The name→`RelId` catalog is persisted in `__meta` and survives a reopen
//! (TKT-72, the `Catalog` boundary — `grmpl_core::store`).

use grmpl_core::{Catalog, RelId};
use grmpl_store::FjallStore;

#[test]
fn catalog_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FjallStore::open(dir.path()).unwrap();
        // Unknown names resolve to None.
        assert_eq!(store.rel_id("located").unwrap(), None);

        store.register("located", RelId(1)).unwrap();
        store.register("named", RelId(2)).unwrap();
        // Idempotent: re-registering the same binding is a no-op, not an error.
        store.register("located", RelId(1)).unwrap();

        assert_eq!(store.rel_id("located").unwrap(), Some(RelId(1)));
        assert_eq!(
            store.entries().unwrap(),
            vec![("located".to_string(), RelId(1)), ("named".to_string(), RelId(2))]
        );
    }
    // Reopen: the catalog is durable and identical.
    let store = FjallStore::open(dir.path()).unwrap();
    assert_eq!(store.rel_id("named").unwrap(), Some(RelId(2)));
    assert_eq!(
        store.entries().unwrap(),
        vec![("located".to_string(), RelId(1)), ("named".to_string(), RelId(2))]
    );
}

#[test]
fn catalog_rejects_conflicting_rebind() {
    let dir = tempfile::tempdir().unwrap();
    let store = FjallStore::open(dir.path()).unwrap();
    store.register("thing", RelId(3)).unwrap();
    // The catalog is append-only: a name's id never silently changes.
    assert!(store.register("thing", RelId(9)).is_err());
    assert_eq!(store.rel_id("thing").unwrap(), Some(RelId(3)));
}
