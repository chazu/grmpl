//! The public world runtime: one deep module binding a compiled world program
//! to its durable substrate.
//!
//! Terminal, TCP, and embedded Rust are adapters over this interface. They do
//! not compile programs, register schemas, construct processes, allocate inbox
//! sequence numbers, or wire reactive watches independently.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use grmpl_core::{
    Authority, Diff, DomainId, Edition, Entity, Error, KeyRange, RelId, Result, Scope, Tuple,
    Value, WorldStore,
};
use grmpl_diff::{Query, Snapshot};
use grmpl_lang::{
    AuthorityRequest, CompiledPackage, GrantSet, Program, ResolvedCapabilityGrant, ResolvedGrantSet,
};
use grmpl_proc::{enqueue_seq, Backoff, ClockDriver, FireNextOutcome, OnWatch, Process, Scheduler};
use grmpl_type::check_handler_authority;

const DEFAULT_DRIVE_FUEL: usize = 1_024;

/// Host-owned authority and capability policy for a driven package.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NamedScope {
    pub relation: String,
    pub range: Option<KeyRange>,
}

impl NamedScope {
    pub fn whole(relation: impl Into<String>) -> Self {
        Self {
            relation: relation.into(),
            range: None,
        }
    }

    pub fn slice(relation: impl Into<String>, range: KeyRange) -> Self {
        Self {
            relation: relation.into(),
            range: Some(range),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NamedAuthority {
    pub domain: DomainId,
    pub owns: Vec<NamedScope>,
}

impl NamedAuthority {
    pub fn new(domain: DomainId, owns: Vec<NamedScope>) -> Self {
        Self { domain, owns }
    }

    fn resolve(&self, program: &Program) -> std::result::Result<Authority, String> {
        let owns = self
            .owns
            .iter()
            .map(|scope| {
                let relation = program.rel_id(&scope.relation).ok_or_else(|| {
                    format!("authority names undeclared relation `{}`", scope.relation)
                })?;
                Ok(match &scope.range {
                    Some(range) => Scope::slice(relation, range.clone()),
                    None => Scope::whole(relation),
                })
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        Ok(Authority::new(self.domain, owns))
    }
}

pub struct RuntimePolicy {
    pub capabilities: GrantSet,
    pub actor_authorities: BTreeMap<String, NamedAuthority>,
    pub driver_authority: NamedAuthority,
    pub default_fuel: NonZeroUsize,
}

impl RuntimePolicy {
    pub fn new(
        capabilities: GrantSet,
        actor_authorities: BTreeMap<String, NamedAuthority>,
        driver_authority: NamedAuthority,
    ) -> Self {
        Self {
            capabilities,
            actor_authorities,
            driver_authority,
            default_fuel: NonZeroUsize::new(DEFAULT_DRIVE_FUEL).unwrap(),
        }
    }

    pub fn with_default_fuel(mut self, fuel: NonZeroUsize) -> Self {
        self.default_fuel = fuel;
        self
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DriveStatus {
    Idle,
    FuelExhausted,
    ActorFault {
        actor: Entity,
        sequence: i64,
        message: String,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DriveReport {
    pub status: DriveStatus,
    pub committed: usize,
    pub timers_fired: usize,
    pub actor_steps: usize,
}

struct DrivenRuntime {
    actors: Vec<(String, Process)>,
    clock: ClockDriver,
    scheduler: Scheduler,
    default_fuel: NonZeroUsize,
}

/// A compiled world program bound to one durable world store.
pub struct Runtime {
    store: Arc<dyn WorldStore>,
    program: Arc<Program>,
    grants: Arc<ResolvedGrantSet>,
    policy: Backoff,
    driven: Option<DrivenRuntime>,
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
            grants: Arc::new(ResolvedGrantSet::default()),
            policy: Backoff::default(),
            driven: None,
        }))
    }

    /// Compile, authorize, and atomically install a versioned package.
    ///
    /// Provisioning metadata may be written before the world commit, but the
    /// bootstrap facts and package marker always land together as edition 1.
    /// Reopening an exact marker is an edition-preserving no-op. An unmarked
    /// nonzero store or mismatched marker fails closed; phase 1 has no migrator.
    pub fn load_package(
        store: Arc<dyn WorldStore>,
        source: &str,
        rel_base: u32,
        host_grants: &GrantSet,
    ) -> std::result::Result<Arc<Runtime>, String> {
        let (package, grants) =
            Self::compile_and_install_package(&store, source, rel_base, host_grants)?;
        Ok(Arc::new(Runtime {
            store,
            program: Arc::new(package.program),
            grants,
            policy: Backoff::default(),
            driven: None,
        }))
    }

    fn compile_and_install_package(
        store: &Arc<dyn WorldStore>,
        source: &str,
        rel_base: u32,
        host_grants: &GrantSet,
    ) -> std::result::Result<(CompiledPackage, Arc<ResolvedGrantSet>), String> {
        let package = CompiledPackage::compile_with_catalog(source, store.as_ref(), rel_base)?;
        let grants = Arc::new(package.resolve_grants(host_grants)?);
        let effective = Edition(store.current().0 + 1);
        package
            .program
            .register_schemas(store.as_ref(), store.as_ref(), effective)
            .map_err(|error| error.to_string())?;

        let expected = package.marker_tuple();
        let live: Vec<_> = store
            .read_at(package.marker_relation, store.current())
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|(_, weight)| *weight != 0)
            .collect();
        match live.as_slice() {
            [] => {
                if store.current() != Edition::ZERO {
                    return Err(format!(
                        "store is at edition {} but has no package marker; use a fresh v4 store",
                        store.current().0
                    ));
                }
                let mut updates: Vec<(RelId, Tuple, Diff)> = package
                    .bootstrap_facts
                    .iter()
                    .map(|compiled| (compiled.fact.rel, compiled.fact.tuple.clone(), 1))
                    .collect();
                updates.push((package.marker_relation, expected.clone(), 1));
                updates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
                let installed = store.commit(&updates).map_err(|error| error.to_string())?;
                if installed != Edition(1) {
                    return Err(format!(
                        "package bootstrap committed at edition {}, expected edition 1",
                        installed.0
                    ));
                }
            }
            [(actual, 1)] if *actual == expected => {}
            [(actual, weight)] => {
                return Err(format!(
                    "package marker mismatch or corrupt weight {weight}: expected {expected:?}, \
                     found {actual:?}; phase 1 does not migrate stores"
                ));
            }
            _ => {
                return Err(format!(
                    "package marker relation contains {} live rows; store is corrupt",
                    live.len()
                ));
            }
        }

        Ok((package, grants))
    }

    /// Compile, authorize, install, and construct the package's static actor
    /// driver. No thread is started; callers explicitly sample and drain.
    pub fn load_driven_package(
        store: Arc<dyn WorldStore>,
        source: &str,
        rel_base: u32,
        policy: &RuntimePolicy,
    ) -> std::result::Result<Arc<Runtime>, String> {
        let (package, grants) =
            Self::compile_and_install_package(&store, source, rel_base, &policy.capabilities)?;
        let schedule_grants: Vec<_> = grants.schedules().collect();
        let [(
            _,
            ResolvedCapabilityGrant::Schedule {
                clock,
                timers,
                sequences,
                ..
            },
        )] = schedule_grants.as_slice()
        else {
            return Err("a driven package requires exactly one schedule capability".into());
        };
        let (clock, timers, sequences) = (*clock, *timers, *sequences);

        let request_by_name: BTreeMap<&str, &AuthorityRequest> = package
            .authority_requests
            .iter()
            .map(|request| (request.name.as_str(), request))
            .collect();
        let program = Arc::new(package.program);
        let driver_authority = policy.driver_authority.resolve(&program)?;
        let mut actors = Vec::new();
        for actor in &package.actors {
            let authority = policy
                .actor_authorities
                .get(&actor.name)
                .ok_or_else(|| format!("missing host authority for actor `{}`", actor.name))?
                .resolve(&program)?;
            if authority.domain != driver_authority.domain {
                return Err(format!(
                    "actor `{}` and its driver must share one authority domain",
                    actor.name
                ));
            }
            let request = request_by_name
                .get(actor.authority.as_str())
                .ok_or_else(|| format!("actor `{}` has no authority request", actor.name))?;
            let mut requested = std::collections::BTreeSet::new();
            for relation_name in &request.writes {
                let relation = program
                    .rel_id(relation_name)
                    .ok_or_else(|| format!("authority names no `{relation_name}` relation"))?;
                requested.insert(relation);
                require_relation_scope(&authority, relation, &format!("actor `{}`", actor.name))?;
            }
            if !requested.contains(&actor.cursor) {
                return Err(format!(
                    "actor `{}` authority request must include cursor `{}`",
                    actor.name, actor.cursor_name
                ));
            }
            require_relation_scope(
                &authority,
                actor.cursor,
                &format!("actor `{}` cursor", actor.name),
            )?;
            let effects = check_handler_authority(&program, &actor.inbox_name, &authority)
                .map_err(|error| format!("actor `{}`: {error}", actor.name))?;
            for relation in effects.writes() {
                if !requested.contains(&relation) {
                    return Err(format!(
                        "actor `{}` handler writes relation {} absent from authority request `{}`",
                        actor.name, relation.0, request.name
                    ));
                }
            }
            actors.push((
                actor.name.clone(),
                Process {
                    entity: actor.entity,
                    authority,
                    inbox: actor.inbox,
                    cursor_rel: actor.cursor,
                    behavior: Program::behavior_with_grants(
                        &program,
                        &actor.inbox_name,
                        actor.entity,
                        Arc::clone(&grants),
                    )?,
                },
            ));
        }
        actors.sort_by_key(|(_, actor)| (actor.inbox, actor.entity));

        for (relation, role) in [
            (clock, "clock"),
            (timers, "timers"),
            (sequences, "sequences"),
        ] {
            require_relation_scope(&driver_authority, relation, role)?;
        }
        for (_, actor) in &actors {
            require_relation_scope(&driver_authority, actor.inbox, "actor inbox")?;
        }

        Ok(Arc::new(Runtime {
            store,
            program,
            grants,
            policy: Backoff::default(),
            driven: Some(DrivenRuntime {
                actors,
                clock: ClockDriver::new(clock, driver_authority.clone()),
                scheduler: Scheduler::new(timers, sequences, driver_authority),
                default_fuel: policy.default_fuel,
            }),
        }))
    }

    /// Commit one nondecreasing simulation-time sample without driving work.
    pub fn record_sample(
        &self,
        wall_ms: i64,
        environmental_sample: i64,
    ) -> Result<(i64, i64, i64)> {
        let driven = self
            .driven
            .as_ref()
            .ok_or_else(|| Error::Store("runtime was not loaded with actor driving".into()))?;
        driven
            .clock
            .sample(self.store(), self.store(), wall_ms, environmental_sample)
    }

    pub fn drive_to_idle(&self) -> Result<DriveReport> {
        let driven = self
            .driven
            .as_ref()
            .ok_or_else(|| Error::Store("runtime was not loaded with actor driving".into()))?;
        self.drive_with_fuel(driven.default_fuel)
    }

    pub fn drive_with_fuel(&self, fuel: NonZeroUsize) -> Result<DriveReport> {
        let driven = self
            .driven
            .as_ref()
            .ok_or_else(|| Error::Store("runtime was not loaded with actor driving".into()))?;
        let mut remaining = fuel.get();
        let mut timers_fired = 0;
        let mut actor_steps = 0;

        loop {
            if remaining == 0 {
                return Ok(DriveReport {
                    status: DriveStatus::FuelExhausted,
                    committed: timers_fired + actor_steps,
                    timers_fired,
                    actor_steps,
                });
            }

            if let Some((_, now, _)) = driven.clock.latest(self.store())? {
                match driven
                    .scheduler
                    .fire_next_due(self.store(), self.store(), now)?
                {
                    FireNextOutcome::Fired => {
                        remaining -= 1;
                        timers_fired += 1;
                        continue;
                    }
                    FireNextOutcome::NoneDue => {}
                    FireNextOutcome::Contended => {
                        return Err(Error::Store(
                            "canonical timer fire did not settle under contention".into(),
                        ));
                    }
                }
            }

            let mut ready = Vec::new();
            for (index, (_, actor)) in driven.actors.iter().enumerate() {
                if let Some(sequence) = actor.next_pending_sequence(self.store())? {
                    ready.push((actor.inbox, actor.entity, sequence, index));
                }
            }
            ready.sort();
            let Some((_, actor_entity, sequence, index)) = ready.first().copied() else {
                return Ok(DriveReport {
                    status: DriveStatus::Idle,
                    committed: timers_fired + actor_steps,
                    timers_fired,
                    actor_steps,
                });
            };
            let actor = &driven.actors[index].1;
            match actor.step_retrying(self.store(), self.store(), self.policy.clone()) {
                Ok(Some(_)) => {
                    remaining -= 1;
                    actor_steps += 1;
                }
                Ok(None) => continue,
                Err(Error::Behavior(message)) => {
                    return Ok(DriveReport {
                        status: DriveStatus::ActorFault {
                            actor: actor_entity,
                            sequence,
                            message,
                        },
                        committed: timers_fired + actor_steps,
                        timers_fired,
                        actor_steps,
                    });
                }
                Err(error) => return Err(error),
            }
        }
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

    /// Capabilities resolved from source requirements and host grants.
    pub fn grants(&self) -> &ResolvedGrantSet {
        self.grants.as_ref()
    }

    pub fn shared_grants(&self) -> Arc<ResolvedGrantSet> {
        Arc::clone(&self.grants)
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
            behavior: Program::behavior_with_grants(
                &self.program,
                inbox,
                entity,
                Arc::clone(&self.grants),
            )?,
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

fn require_relation_scope(
    authority: &Authority,
    relation: RelId,
    role: &str,
) -> std::result::Result<(), String> {
    if authority.owns.iter().any(|scope| scope.rel == relation) {
        Ok(())
    } else {
        Err(format!(
            "{role} authority has no scope for relation {}",
            relation.0
        ))
    }
}

/// Split one command line into the tuple consumed by a source `form`.
pub fn tokenize(line: &str) -> Tuple {
    Tuple::new(line.split_whitespace().map(Value::text).collect::<Vec<_>>())
}
