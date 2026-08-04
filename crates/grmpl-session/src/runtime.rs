//! The public world runtime: one deep module binding a compiled world program
//! to its durable substrate.
//!
//! Terminal, TCP, and embedded Rust are adapters over this interface. They do
//! not compile programs, register schemas, construct processes, allocate inbox
//! sequence numbers, or wire reactive watches independently.

use std::sync::Arc;

use grmpl_core::{Authority, Edition, Entity, Error, RelId, Result, Tuple, Value, WorldStore};
use grmpl_diff::{Query, Snapshot};
use grmpl_lang::Program;
use grmpl_proc::{enqueue_seq, Backoff, OnWatch, Process};

/// A compiled world program bound to one durable world store.
pub struct Runtime {
    store: Arc<dyn WorldStore>,
    program: Arc<Program>,
    policy: Backoff,
}

impl Runtime {
    /// Compile and bind `source`, recovering stable relation ids from the
    /// store's durable catalog and registering every declared schema.
    ///
    /// Compilation is provisioning-time and must not race another compiler on
    /// the same store, matching `Program::compile_with_catalog`'s contract.
    pub fn compile(
        store: Arc<dyn WorldStore>,
        source: &str,
        rel_base: u32,
    ) -> std::result::Result<Arc<Runtime>, String> {
        let program = Arc::new(Program::compile_with_catalog(
            source,
            store.as_ref(),
            rel_base,
        )?);
        let effective = Edition(store.current().0 + 1);
        program
            .register_schemas(store.as_ref(), store.as_ref(), effective)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Runtime {
            store,
            program,
            policy: Backoff::default(),
        }))
    }

    /// The substrate holding this world.
    pub fn store(&self) -> &dyn WorldStore {
        self.store.as_ref()
    }

    /// A shared substrate handle for adapters and observers.
    pub fn shared_store(&self) -> Arc<dyn WorldStore> {
        Arc::clone(&self.store)
    }

    /// The compiled world program.
    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The retry policy used by processes driven through this runtime.
    pub fn policy(&self) -> Backoff {
        self.policy.clone()
    }

    /// Resolve a declared relation through the program's durable catalog ids.
    pub fn relation(&self, name: &str) -> Result<RelId> {
        self.program
            .rel_id(name)
            .ok_or_else(|| Error::Store(format!("world program has no `rel {name}`")))
    }

    /// Instantiate and evaluate a named view at the current world edition.
    pub fn view(
        &self,
        name: &str,
        args: &[Value],
    ) -> std::result::Result<Vec<(Tuple, i64)>, String> {
        let query = self.program.view(name, args)?;
        query
            .find(&Snapshot::at_current(self.store()))
            .map_err(|e| e.to_string())
    }

    /// Instantiate a query without evaluating it.
    pub fn query(&self, name: &str, args: &[Value]) -> std::result::Result<Query, String> {
        self.program.view(name, args)
    }

    /// Construct a language-defined actor bound to named inbox/cursor relations.
    pub fn process(
        &self,
        entity: Entity,
        authority: Authority,
        inbox: &str,
        cursor: &str,
    ) -> std::result::Result<Process, String> {
        Ok(Process {
            entity,
            authority,
            inbox: self.relation(inbox).map_err(|e| e.to_string())?,
            cursor_rel: self.relation(cursor).map_err(|e| e.to_string())?,
            behavior: Program::behavior(&self.program, inbox, entity)?,
        })
    }

    /// Append one tokenized command at a race-safe durable inbox sequence.
    pub fn enqueue(&self, process: Entity, inbox: &str, seqs: &str, line: &str) -> Result<i64> {
        enqueue_seq(
            self.store(),
            self.relation(inbox)?,
            self.relation(seqs)?,
            process,
            tokenize(line),
        )
    }

    /// Lower and install a source-declared reactive watch.
    pub fn install_watch(
        &self,
        view: &str,
        args: &[Value],
        watch: Entity,
        target: Entity,
        authority: Authority,
    ) -> std::result::Result<OnWatch, String> {
        let (watch, _) = self.program.install_watch(
            view,
            args,
            watch,
            target,
            authority,
            self.store(),
            self.store(),
        )?;
        Ok(watch)
    }
}

/// Split one command line into the tuple consumed by a source `form`.
pub fn tokenize(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}
