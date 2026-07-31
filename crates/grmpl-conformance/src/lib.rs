//! # grmpl-conformance
//!
//! **One law suite, every substrate.**
//!
//! The whole point of `grmpl` is a *language* running on the Ent — not an Ent
//! crate with good asymptotics sitting beside a language that runs on something
//! else. The way to keep that honest is to stop writing tests against a
//! particular store: a law of the language is a law over the *substrate traits*,
//! so it should be stated once and run against every implementation of them.
//!
//! Before this harness, `grmpl-proc`, `grmpl-lang` and `grmpl-session` tests
//! opened a `FjallStore` by name, so the process layer, the reactive pump, the
//! scheduler, replay and the session runtime were only ever *proven* on the LSM.
//! `grmpl-ent` had a handful of its own end-to-end tests, which is not the same
//! claim. Now a test writes its body once against `&dyn WorldStore` and
//! [`each_store`] runs it on both — the ent and the LSM — so "the language runs
//! on the Ent" is a property the suite checks rather than a sentence in a doc.
//!
//! **This crate is above the bright line and dev-only.** It names both `fjall`
//! (via `grmpl-store`) and `grmpl-ent`, exactly as a harness must in order to
//! compare them — which is precisely why the semantic core's crates can go on
//! naming neither. It is a `dev-dependency` everywhere and is never published.

use std::sync::Arc;

use grmpl_core::{Catalog, SchemaCatalog, TraceStore};
use grmpl_ent::EntStore;
use grmpl_store::FjallStore;
use tempfile::TempDir;

/// Everything a world needs from its substrate: the edition/tuple trace, the
/// durable name catalog, and the edition-versioned schema registry. Both
/// implementations provide all three, so a law may use any of them.
pub trait WorldStore: TraceStore + Catalog + SchemaCatalog {}
impl<T: TraceStore + Catalog + SchemaCatalog> WorldStore for T {}

/// One substrate under test: a named store in its own fresh directory.
///
/// The directory is held here and dropped with the case, so a test never has to
/// thread a `TempDir` through its helpers just to keep the store alive.
pub struct Case {
    /// Which substrate this is — put it in assertion messages so a failure says
    /// *which* store broke the law.
    pub name: &'static str,
    store: Arc<dyn WorldStore>,
    _dir: TempDir,
}

impl Case {
    /// The store under test, as the trace substrate — what almost every law
    /// wants, since `commit`/`read_at`/`scan_updates` are `TraceStore`.
    pub fn store(&self) -> &dyn TraceStore {
        self.store.as_ref()
    }

    /// The store with its catalog and schema registry too, for laws about
    /// naming, schema evolution, or commit-boundary schema enforcement.
    pub fn world(&self) -> &dyn WorldStore {
        self.store.as_ref()
    }

    /// The store as a schema registry — the second argument of `commit_patch`
    /// and friends.
    pub fn schemas(&self) -> &dyn SchemaCatalog {
        self.store.as_ref()
    }

    /// The store as a name catalog.
    pub fn catalog(&self) -> &dyn Catalog {
        self.store.as_ref()
    }

    /// The store as a shared handle — for laws that hand it to threads (the
    /// contention and race oracles) or park it in a `Process`.
    pub fn shared(&self) -> Arc<dyn WorldStore> {
        Arc::clone(&self.store)
    }

    /// The store as a plain shared trace handle, for the many APIs that ask for
    /// `Arc<dyn TraceStore>`.
    pub fn trace(&self) -> Arc<dyn TraceStore> {
        Arc::clone(&self.store) as Arc<dyn TraceStore>
    }

    /// A **second, independent** store of the same substrate.
    ///
    /// Some laws need two worlds at once — cross-domain messaging, the
    /// contention oracle's two authority domains, a fork's target. Both must be
    /// the same kind of store, or the law would be comparing an ent against an
    /// LSM instead of testing either.
    pub fn sibling(&self) -> Case {
        match self.name {
            "ent" => ent(),
            _ => fjall(),
        }
    }
}

/// Which substrate a location holds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Kind {
    Ent,
    Fjall,
}

/// A substrate **location** with no store currently open on it.
///
/// Crash-recovery laws — "process one message, crash, reopen, and neither lose
/// nor duplicate it" — need to drop a store and open another at the same place.
/// A [`Case`] cannot serve that: it holds an open handle, and both stores take
/// an exclusive lock on their directory. So those laws take a `World` and open
/// handles themselves, using `drop` as the crash.
pub struct World {
    /// Which substrate this is — put it in assertion messages.
    pub name: &'static str,
    kind: Kind,
    dir: TempDir,
}

impl World {
    /// Open a handle on this location. Dropping it is the crash; calling again
    /// is the recovery. Never hold two at once — the directory lock is
    /// exclusive, which is exactly the property that makes this a faithful
    /// crash test.
    pub fn open(&self) -> Box<dyn WorldStore> {
        match self.kind {
            Kind::Ent => Box::new(EntStore::open(self.dir.path()).expect("reopen ent store")),
            Kind::Fjall => Box::new(FjallStore::open(self.dir.path()).expect("reopen fjall store")),
        }
    }
}

/// Run one crash-recovery law against every substrate.
pub fn for_each_world(mut body: impl FnMut(&World)) {
    for kind in [Kind::Ent, Kind::Fjall] {
        let name = match kind {
            Kind::Ent => "ent",
            Kind::Fjall => "fjall",
        };
        let dir = tempfile::tempdir().expect("tempdir");
        body(&World { name, kind, dir });
    }
}

/// Every substrate implementation, each freshly created.
///
/// The **Ent first**: it is the authoritative substrate, so when a law breaks on
/// both, the ent's failure is the one reported first.
pub fn each_store() -> Vec<Case> {
    vec![ent(), fjall()]
}

/// Just the ent-native store — for laws that are specifically about it.
pub fn ent() -> Case {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = EntStore::open(dir.path()).expect("open ent store");
    Case { name: "ent", store: Arc::new(store), _dir: dir }
}

/// Just the LSM store — the differential oracle.
pub fn fjall() -> Case {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FjallStore::open(dir.path()).expect("open fjall store");
    Case { name: "fjall", store: Arc::new(store), _dir: dir }
}

/// The **logical projection** of a store: for each relation, its raw updates in
/// `(edition, submit_index)` order — the concatenation of
/// `scan_updates(rel, ZERO, current)`.
///
/// This is the identity replay and fork must prove, and plan v4 §2 says so
/// explicitly: identity is defined here and **not** over raw node bytes, because
/// two substrates (and two tree shapes) legitimately lay the same world out
/// differently — Gold guarantees content/version identity, never byte-identical
/// node layout. `FjallStore::canonical_dump` compares physical keyspace bytes,
/// which no other substrate can answer and which would fail across a rebalance
/// even on one.
///
/// It is also the *stronger* statement in the way that matters: the projection
/// keeps same-edition submit order, so it witnesses Determinism, where a dump
/// re-sorted by `(tuple, diff)` would not.
pub fn logical_dump(
    store: &dyn TraceStore,
    rels: &[grmpl_core::RelId],
) -> grmpl_core::Result<Vec<(grmpl_core::RelId, Vec<grmpl_core::Update>)>> {
    let current = store.current();
    let from = store.watermark();
    let mut out = Vec::with_capacity(rels.len());
    for &rel in rels {
        out.push((rel, store.scan_updates(rel, from, current)?));
    }
    Ok(out)
}

/// Run one law body against every substrate.
///
/// ```ignore
/// #[test]
/// fn the_lamp_can_only_be_taken_once() {
///     grmpl_conformance::for_each_store(|c| {
///         let store = c.store();
///         // …the law, stated once…
///         assert!(outcome, "{}: the lamp was taken twice", c.name);
///     });
/// }
/// ```
pub fn for_each_store(mut body: impl FnMut(&Case)) {
    for case in each_store() {
        body(&case);
    }
}
