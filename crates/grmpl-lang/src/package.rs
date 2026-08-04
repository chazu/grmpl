//! Versioned world packages: constants, host grants, canonical bootstrap data,
//! and the package-private installation marker.

use std::collections::{BTreeMap, BTreeSet};

use grmpl_core::{
    sha256, wire, Catalog, Column, Entity, Fact, RelId, Schema, Sha256Digest, Tuple, Ty, Value,
};

use crate::ast::{BootstrapValue, Decl};
use crate::{parse, Program};

/// Source cannot spell this identifier because relation names are identifiers;
/// it is allocated through the same durable catalog as ordinary relations.
pub const INSTALL_MARKER_RELATION: &str = "grmpl:package/install";

/// A persistent capability requested by package source.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CapabilityRequirement {
    Allocate {
        name: String,
        counter: String,
        first: i64,
        last: i64,
    },
    Random {
        name: String,
        state: String,
        owner: Entity,
        algorithm: String,
    },
    Schedule {
        name: String,
        clock: String,
        timers: String,
        sequences: String,
    },
}

impl CapabilityRequirement {
    pub fn name(&self) -> &str {
        match self {
            CapabilityRequirement::Allocate { name, .. }
            | CapabilityRequirement::Random { name, .. }
            | CapabilityRequirement::Schedule { name, .. } => name,
        }
    }
}

/// A host grant. Stable relation names are used so callers can construct the
/// grant before the package's physical relation ids have been resolved.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CapabilityGrant {
    Allocate {
        name: String,
        counter: String,
        first: i64,
        last: i64,
    },
    Random {
        name: String,
        state: String,
        owner: Entity,
        algorithm: String,
    },
    Schedule {
        name: String,
        clock: String,
        timers: String,
        sequences: String,
        targets: BTreeSet<String>,
    },
}

impl CapabilityGrant {
    fn name(&self) -> &str {
        match self {
            CapabilityGrant::Allocate { name, .. }
            | CapabilityGrant::Random { name, .. }
            | CapabilityGrant::Schedule { name, .. } => name,
        }
    }

    fn kind(&self) -> u8 {
        match self {
            CapabilityGrant::Allocate { .. } => 0,
            CapabilityGrant::Random { .. } => 1,
            CapabilityGrant::Schedule { .. } => 2,
        }
    }
}

/// Host authority for semantic primitives. Extra grants are deliberately not
/// exposed to a package; only matching source requirements are resolved.
#[derive(Clone, Default, Debug)]
pub struct GrantSet {
    grants: BTreeMap<(u8, String), CapabilityGrant>,
}

impl GrantSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant_allocate(
        mut self,
        name: impl Into<String>,
        counter: impl Into<String>,
        first: i64,
        last: i64,
    ) -> Result<Self, String> {
        self.insert(CapabilityGrant::Allocate {
            name: name.into(),
            counter: counter.into(),
            first,
            last,
        })?;
        Ok(self)
    }

    pub fn grant_random(
        mut self,
        name: impl Into<String>,
        state: impl Into<String>,
        owner: Entity,
        algorithm: impl Into<String>,
    ) -> Result<Self, String> {
        self.insert(CapabilityGrant::Random {
            name: name.into(),
            state: state.into(),
            owner,
            algorithm: algorithm.into(),
        })?;
        Ok(self)
    }

    pub fn grant_schedule<I, S>(
        mut self,
        name: impl Into<String>,
        clock: impl Into<String>,
        timers: impl Into<String>,
        sequences: impl Into<String>,
        targets: I,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.insert(CapabilityGrant::Schedule {
            name: name.into(),
            clock: clock.into(),
            timers: timers.into(),
            sequences: sequences.into(),
            targets: targets.into_iter().map(Into::into).collect(),
        })?;
        Ok(self)
    }

    fn insert(&mut self, grant: CapabilityGrant) -> Result<(), String> {
        let key = (grant.kind(), grant.name().to_owned());
        if self.grants.insert(key, grant).is_some() {
            return Err("capability grant declared twice".into());
        }
        Ok(())
    }

    fn get(&self, kind: u8, name: &str) -> Option<&CapabilityGrant> {
        self.grants.get(&(kind, name.to_owned()))
    }
}

/// A grant after stable relation names have been bound to this store's ids.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResolvedCapabilityGrant {
    Allocate {
        counter: RelId,
        first: i64,
        last: i64,
    },
    Random {
        state: RelId,
        owner: Entity,
        algorithm: String,
    },
    Schedule {
        clock: RelId,
        timers: RelId,
        sequences: RelId,
        targets: BTreeMap<String, (Entity, RelId)>,
    },
}

/// The immutable capability environment closed over by loaded behaviors.
#[derive(Clone, Default, Debug)]
pub struct ResolvedGrantSet {
    grants: BTreeMap<String, ResolvedCapabilityGrant>,
}

impl ResolvedGrantSet {
    pub fn get(&self, name: &str) -> Option<&ResolvedCapabilityGrant> {
        self.grants.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    pub fn schedules(&self) -> impl Iterator<Item = (&str, &ResolvedCapabilityGrant)> + '_ {
        self.grants.iter().filter_map(|(name, grant)| {
            matches!(grant, ResolvedCapabilityGrant::Schedule { .. })
                .then_some((name.as_str(), grant))
        })
    }

    pub fn validate_invocation(
        &self,
        capability: &str,
        arguments: &[crate::ExprIr],
    ) -> Result<(), String> {
        let Some(grant) = self.get(capability) else {
            return Err(format!("capability `{capability}` has no resolved grant"));
        };
        if let ResolvedCapabilityGrant::Schedule { targets, .. } = grant {
            let Some(crate::ExprIr::Value(crate::ValueExpr::Literal(Value::Text(target)))) =
                arguments.first()
            else {
                return Err(format!(
                    "schedule capability `{capability}` needs a literal target actor"
                ));
            };
            if !targets.contains_key(target.as_ref()) {
                return Err(format!(
                    "schedule capability `{capability}` disallows target actor `{target}`"
                ));
            }
        }
        Ok(())
    }
}

/// One checked bootstrap fact, retaining the stable source relation name for
/// digesting as well as the physical id used for commit ordering.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompiledBootstrapFact {
    pub relation_name: String,
    pub fact: Fact,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuthorityRequest {
    pub name: String,
    pub writes: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompiledActor {
    pub name: String,
    pub entity: Entity,
    pub inbox_name: String,
    pub inbox: RelId,
    pub cursor_name: String,
    pub cursor: RelId,
    pub authority: String,
}

/// The product package compiler hands to the public runtime.
pub struct CompiledPackage {
    pub package_id: String,
    pub bootstrap_version: u32,
    pub program: Program,
    pub entities: BTreeMap<String, Entity>,
    pub bootstrap_facts: Vec<CompiledBootstrapFact>,
    pub requirements: Vec<CapabilityRequirement>,
    pub authority_requests: Vec<AuthorityRequest>,
    pub actors: Vec<CompiledActor>,
    pub bootstrap_digest: Sha256Digest,
    pub marker_relation: RelId,
}

impl CompiledPackage {
    /// Compile against the durable catalog. This is a provisioning-time,
    /// single-writer operation, just like `Program::compile_with_catalog`.
    pub fn compile_with_catalog(
        source: &str,
        catalog: &dyn Catalog,
        rel_base: u32,
    ) -> Result<Self, String> {
        let declarations = parse(source)?;
        let (package_id, bootstrap_version) = package_header(&declarations)?;
        let entities = compile_entities(&declarations)?;
        let requirements = compile_requirements(&declarations, &entities)?;
        let authority_requests = compile_authorities(&declarations)?;

        let mut program = Program::compile_with_catalog(source, catalog, rel_base)?;
        program.validate_entity_namespace(&entities)?;
        program.set_entities(entities.clone());

        let marker_relation = assign_reserved(catalog, rel_base, INSTALL_MARKER_RELATION)?;
        program.insert_reserved_relation(
            INSTALL_MARKER_RELATION.to_owned(),
            marker_relation,
            vec![
                Column::new("package", Ty::Text),
                Column::new("bootstrap_version", Ty::Int),
                Column::new("bootstrap_digest", Ty::Bytes),
            ],
        )?;

        validate_requirements(&program, &entities, &requirements)?;
        program.validate_behaviors()?;
        let bootstrap_facts = compile_bootstrap(&declarations, &program, &entities)?;
        validate_resource_seeds(&bootstrap_facts, &requirements)?;
        let actors = compile_actors(
            &declarations,
            &program,
            &entities,
            &authority_requests,
            &requirements,
            &bootstrap_facts,
        )?;
        let bootstrap_digest = digest(
            &package_id,
            bootstrap_version,
            &entities,
            &bootstrap_facts,
            &requirements,
        );

        Ok(Self {
            package_id,
            bootstrap_version,
            program,
            entities,
            bootstrap_facts,
            requirements,
            authority_requests,
            actors,
            bootstrap_digest,
            marker_relation,
        })
    }

    /// Check source requirements against host grants and bind their relation
    /// names to this store's physical ids.
    pub fn resolve_grants(&self, host: &GrantSet) -> Result<ResolvedGrantSet, String> {
        let mut resolved = BTreeMap::new();
        for requirement in &self.requirements {
            let (name, grant) = match requirement {
                CapabilityRequirement::Allocate {
                    name,
                    counter,
                    first,
                    last,
                } => {
                    let Some(CapabilityGrant::Allocate {
                        counter: granted_counter,
                        first: granted_first,
                        last: granted_last,
                        ..
                    }) = host.get(0, name)
                    else {
                        return Err(format!("missing allocate grant `{name}`"));
                    };
                    if granted_counter != counter || granted_first > first || granted_last < last {
                        return Err(format!(
                            "allocate grant `{name}` does not contain required counter/range"
                        ));
                    }
                    let relation = self
                        .program
                        .rel_id(counter)
                        .ok_or_else(|| format!("allocate counter `{counter}` is undeclared"))?;
                    (
                        name,
                        ResolvedCapabilityGrant::Allocate {
                            counter: relation,
                            first: *first,
                            last: *last,
                        },
                    )
                }
                CapabilityRequirement::Random {
                    name,
                    state,
                    owner,
                    algorithm,
                } => {
                    let Some(CapabilityGrant::Random {
                        state: granted_state,
                        owner: granted_owner,
                        algorithm: granted_algorithm,
                        ..
                    }) = host.get(1, name)
                    else {
                        return Err(format!("missing random grant `{name}`"));
                    };
                    if granted_state != state
                        || granted_owner != owner
                        || granted_algorithm != algorithm
                    {
                        return Err(format!(
                            "random grant `{name}` does not match required state/owner/algorithm"
                        ));
                    }
                    let relation = self
                        .program
                        .rel_id(state)
                        .ok_or_else(|| format!("random state `{state}` is undeclared"))?;
                    (
                        name,
                        ResolvedCapabilityGrant::Random {
                            state: relation,
                            owner: *owner,
                            algorithm: algorithm.clone(),
                        },
                    )
                }
                CapabilityRequirement::Schedule {
                    name,
                    clock,
                    timers,
                    sequences,
                } => {
                    let Some(CapabilityGrant::Schedule {
                        clock: granted_clock,
                        timers: granted_timers,
                        sequences: granted_sequences,
                        targets: granted_targets,
                        ..
                    }) = host.get(2, name)
                    else {
                        return Err(format!("missing schedule grant `{name}`"));
                    };
                    if granted_clock != clock
                        || granted_timers != timers
                        || granted_sequences != sequences
                    {
                        return Err(format!(
                            "schedule grant `{name}` does not match required relations"
                        ));
                    }
                    let mut targets = BTreeMap::new();
                    for actor in &self.actors {
                        if granted_targets.contains(&actor.name) {
                            targets.insert(actor.name.clone(), (actor.entity, actor.inbox));
                        }
                    }
                    let used = self.program.schedule_targets(name)?;
                    for target in used {
                        if !targets.contains_key(&target) {
                            return Err(format!(
                                "schedule grant `{name}` disallows target actor `{target}`"
                            ));
                        }
                    }
                    let relation = |relation: &str| {
                        self.program
                            .rel_id(relation)
                            .ok_or_else(|| format!("schedule relation `{relation}` is undeclared"))
                    };
                    (
                        name,
                        ResolvedCapabilityGrant::Schedule {
                            clock: relation(clock)?,
                            timers: relation(timers)?,
                            sequences: relation(sequences)?,
                            targets,
                        },
                    )
                }
            };
            if resolved.insert(name.clone(), grant).is_some() {
                return Err(format!("capability name `{name}` is used more than once"));
            }
        }
        Ok(ResolvedGrantSet { grants: resolved })
    }

    pub fn marker_tuple(&self) -> Tuple {
        Tuple::from([
            Value::text(&self.package_id),
            Value::Int(self.bootstrap_version as i64),
            Value::bytes(self.bootstrap_digest),
        ])
    }
}

fn package_header(declarations: &[Decl]) -> Result<(String, u32), String> {
    let headers: Vec<_> = declarations
        .iter()
        .filter_map(|decl| match decl {
            Decl::Package {
                id,
                bootstrap_version,
            } => Some((id.clone(), *bootstrap_version)),
            _ => None,
        })
        .collect();
    match headers.as_slice() {
        [header] => Ok(header.clone()),
        [] => Err("a loaded package requires exactly one `package` declaration".into()),
        _ => Err("a package source declares `package` more than once".into()),
    }
}

fn compile_entities(declarations: &[Decl]) -> Result<BTreeMap<String, Entity>, String> {
    let mut entities = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for declaration in declarations {
        let Decl::Entity { name, id } = declaration else {
            continue;
        };
        if *id < 0 {
            return Err(format!("entity `{name}` has negative id {id}"));
        }
        let entity = Entity(*id as u64);
        if entities.insert(name.clone(), entity).is_some() {
            return Err(format!("entity `{name}` is declared twice"));
        }
        if !ids.insert(entity) {
            return Err(format!("entity id {id} is declared twice"));
        }
    }
    Ok(entities)
}

fn compile_authorities(declarations: &[Decl]) -> Result<Vec<AuthorityRequest>, String> {
    let mut requests = Vec::new();
    let mut names = BTreeSet::new();
    for declaration in declarations {
        let Decl::Authority { name, writes } = declaration else {
            continue;
        };
        if !names.insert(name.clone()) {
            return Err(format!("authority `{name}` is declared twice"));
        }
        let mut writes = writes.clone();
        writes.sort();
        if writes.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(format!("authority `{name}` requests a relation twice"));
        }
        requests.push(AuthorityRequest {
            name: name.clone(),
            writes,
        });
    }
    requests.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(requests)
}

fn compile_actors(
    declarations: &[Decl],
    program: &Program,
    entities: &BTreeMap<String, Entity>,
    authorities: &[AuthorityRequest],
    requirements: &[CapabilityRequirement],
    bootstrap: &[CompiledBootstrapFact],
) -> Result<Vec<CompiledActor>, String> {
    let authority_names: BTreeSet<_> = authorities.iter().map(|a| a.name.as_str()).collect();
    for authority in authorities {
        for relation in &authority.writes {
            if program.rel_id(relation).is_none() {
                return Err(format!(
                    "authority `{}` requests undeclared relation `{relation}`",
                    authority.name
                ));
            }
        }
    }
    let sequence_names: BTreeSet<_> = requirements
        .iter()
        .filter_map(|requirement| match requirement {
            CapabilityRequirement::Schedule { sequences, .. } => Some(sequences.as_str()),
            _ => None,
        })
        .collect();
    if declarations
        .iter()
        .any(|declaration| matches!(declaration, Decl::Actor { .. }))
        && sequence_names.len() != 1
    {
        return Err("static actors require exactly one schedule sequence relation".into());
    }
    let sequence_name = sequence_names.iter().next().copied();

    let mut actors = Vec::new();
    let mut actor_names = BTreeSet::new();
    for declaration in declarations {
        let Decl::Actor {
            entity,
            inbox,
            cursor,
            authority,
        } = declaration
        else {
            continue;
        };
        if !actor_names.insert(entity.clone()) {
            return Err(format!("actor `{entity}` is declared twice"));
        }
        let entity_id = entities
            .get(entity)
            .copied()
            .ok_or_else(|| format!("actor `{entity}` names no entity constant"))?;
        if !authority_names.contains(authority.as_str()) {
            return Err(format!(
                "actor `{entity}` names undeclared authority `{authority}`"
            ));
        }
        let inbox_id = program
            .rel_id(inbox)
            .ok_or_else(|| format!("actor `{entity}` names undeclared inbox `{inbox}`"))?;
        let cursor_id = program
            .rel_id(cursor)
            .ok_or_else(|| format!("actor `{entity}` names undeclared cursor `{cursor}`"))?;
        if program.schema(inbox)
            != Some(Schema::new(vec![
                Column::new("process", Ty::Ent),
                Column::new("seq", Ty::Int),
                Column::new("body", Ty::Tuple),
            ]))
        {
            return Err(format!(
                "actor `{entity}` inbox `{inbox}` must have schema `(process: Ent, seq: Int, body: Tuple)`"
            ));
        }
        if program.schema(cursor)
            != Some(Schema::new(vec![
                Column::new("process", Ty::Ent),
                Column::new("pos", Ty::Int),
            ]))
        {
            return Err(format!(
                "actor `{entity}` cursor `{cursor}` must have schema `(process: Ent, pos: Int)`"
            ));
        }
        program.handler_irs(inbox)?;

        let sequence_name = sequence_name.expect("checked above");
        let sequence_rows: Vec<_> = bootstrap
            .iter()
            .filter(|fact| {
                fact.relation_name == sequence_name
                    && fact.fact.tuple.as_slice().first() == Some(&Value::Ent(entity_id))
            })
            .collect();
        let next = match sequence_rows.as_slice() {
            [row] => match row.fact.tuple.as_slice() {
                [Value::Ent(_), Value::Int(next)] if *next >= 0 => *next,
                _ => return Err(format!("actor `{entity}` has invalid sequence seed")),
            },
            _ => {
                return Err(format!(
                    "actor `{entity}` needs exactly one non-negative bootstrap sequence row"
                ))
            }
        };
        let mut inbox_sequences: Vec<_> = bootstrap
            .iter()
            .filter_map(|fact| {
                if fact.relation_name != *inbox {
                    return None;
                }
                match fact.fact.tuple.as_slice() {
                    [Value::Ent(actor), Value::Int(sequence), Value::Tuple(_)]
                        if *actor == entity_id =>
                    {
                        Some(*sequence)
                    }
                    _ => None,
                }
            })
            .collect();
        inbox_sequences.sort();
        if inbox_sequences != (0..next).collect::<Vec<_>>() {
            return Err(format!(
                "actor `{entity}` bootstrap inbox must occupy contiguous sequence range 0..{next}"
            ));
        }
        let cursor_rows: Vec<_> = bootstrap
            .iter()
            .filter_map(|fact| {
                if fact.relation_name != *cursor {
                    return None;
                }
                match fact.fact.tuple.as_slice() {
                    [Value::Ent(actor), Value::Int(position)] if *actor == entity_id => {
                        Some(*position)
                    }
                    _ => None,
                }
            })
            .collect();
        if cursor_rows.len() > 1
            || cursor_rows
                .first()
                .is_some_and(|position| *position < 0 || *position > next)
        {
            return Err(format!("actor `{entity}` has invalid bootstrap cursor"));
        }
        actors.push(CompiledActor {
            name: entity.clone(),
            entity: entity_id,
            inbox_name: inbox.clone(),
            inbox: inbox_id,
            cursor_name: cursor.clone(),
            cursor: cursor_id,
            authority: authority.clone(),
        });
    }
    actors.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(actors)
}

fn compile_requirements(
    declarations: &[Decl],
    entities: &BTreeMap<String, Entity>,
) -> Result<Vec<CapabilityRequirement>, String> {
    let mut requirements = Vec::new();
    let mut names = BTreeSet::new();
    let mut ranges = Vec::new();
    for declaration in declarations {
        let requirement = match declaration {
            Decl::RequireAllocate {
                name,
                counter,
                first,
                last,
            } => {
                if *first < 0 || first > last || *last == i64::MAX {
                    return Err(format!(
                        "allocate requirement `{name}` has invalid range; the final counter successor must fit Int"
                    ));
                }
                for entity in entities.values() {
                    let id = entity.0 as i64;
                    if (*first..=*last).contains(&id) {
                        return Err(format!(
                            "entity id {id} lies inside allocation range `{name}`"
                        ));
                    }
                }
                for (other_name, other_first, other_last) in &ranges {
                    if *first <= *other_last && *other_first <= *last {
                        return Err(format!(
                            "allocation ranges `{other_name}` and `{name}` overlap"
                        ));
                    }
                }
                ranges.push((name.clone(), *first, *last));
                CapabilityRequirement::Allocate {
                    name: name.clone(),
                    counter: counter.clone(),
                    first: *first,
                    last: *last,
                }
            }
            Decl::RequireRandom {
                name,
                state,
                owner,
                algorithm,
            } => {
                let owner = entities.get(owner).copied().ok_or_else(|| {
                    format!("random requirement `{name}` has unknown owner `{owner}`")
                })?;
                CapabilityRequirement::Random {
                    name: name.clone(),
                    state: state.clone(),
                    owner,
                    algorithm: algorithm.clone(),
                }
            }
            Decl::RequireSchedule {
                name,
                clock,
                timers,
                sequences,
            } => CapabilityRequirement::Schedule {
                name: name.clone(),
                clock: clock.clone(),
                timers: timers.clone(),
                sequences: sequences.clone(),
            },
            _ => continue,
        };
        if !names.insert(requirement.name().to_owned()) {
            return Err(format!(
                "capability `{}` is declared twice",
                requirement.name()
            ));
        }
        requirements.push(requirement);
    }
    requirements.sort();
    Ok(requirements)
}

fn validate_requirements(
    program: &Program,
    _entities: &BTreeMap<String, Entity>,
    requirements: &[CapabilityRequirement],
) -> Result<(), String> {
    let mut counters = BTreeSet::new();
    for requirement in requirements {
        match requirement {
            CapabilityRequirement::Allocate { name, counter, .. } => {
                if !counters.insert(counter) {
                    return Err(format!(
                        "allocation counter `{counter}` is required more than once"
                    ));
                }
                let schema = program.schema(counter).ok_or_else(|| {
                    format!("allocate requirement `{name}` names undeclared `{counter}`")
                })?;
                if schema != Schema::new(vec![Column::new("next", Ty::Int)]) {
                    return Err(format!(
                        "allocate requirement `{name}` counter `{counter}` must have schema `(next: Int)`"
                    ));
                }
            }
            CapabilityRequirement::Random {
                name,
                state,
                algorithm,
                ..
            } => {
                if algorithm != "xorshift64star_v1" {
                    return Err(format!(
                        "random requirement `{name}` uses unsupported algorithm `{algorithm}`"
                    ));
                }
                let schema = program.schema(state).ok_or_else(|| {
                    format!("random requirement `{name}` names undeclared `{state}`")
                })?;
                if schema
                    != Schema::new(vec![
                        Column::new("owner", Ty::Ent),
                        Column::new("state", Ty::Int),
                    ])
                {
                    return Err(format!(
                        "random requirement `{name}` state `{state}` must have schema \
                         `(owner: Ent, state: Int)`"
                    ));
                }
            }
            CapabilityRequirement::Schedule {
                name,
                clock,
                timers,
                sequences,
            } => {
                let expected = [
                    (
                        clock,
                        Schema::new(vec![
                            Column::new("seq", Ty::Int),
                            Column::new("wall_ms", Ty::Int),
                            Column::new("random", Ty::Int),
                        ]),
                    ),
                    (
                        timers,
                        Schema::new(vec![
                            Column::new("due", Ty::Int),
                            Column::new("inbox", Ty::Int),
                            Column::new("target", Ty::Ent),
                            Column::new("body", Ty::Tuple),
                        ]),
                    ),
                    (
                        sequences,
                        Schema::new(vec![
                            Column::new("process", Ty::Ent),
                            Column::new("next", Ty::Int),
                        ]),
                    ),
                ];
                for (relation, schema) in expected {
                    if program.schema(relation) != Some(schema) {
                        return Err(format!(
                            "schedule requirement `{name}` has invalid `{relation}` schema"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn compile_bootstrap(
    declarations: &[Decl],
    program: &Program,
    entities: &BTreeMap<String, Entity>,
) -> Result<Vec<CompiledBootstrapFact>, String> {
    let blocks: Vec<_> = declarations
        .iter()
        .filter_map(|decl| match decl {
            Decl::Bootstrap { facts } => Some(facts),
            _ => None,
        })
        .collect();
    let facts = match blocks.as_slice() {
        [facts] => *facts,
        [] => return Err("a loaded package requires exactly one `bootstrap` block".into()),
        _ => return Err("a package source declares `bootstrap` more than once".into()),
    };

    let mut compiled = Vec::new();
    let mut seen = BTreeSet::new();
    for source_fact in facts.iter() {
        if source_fact.rel == INSTALL_MARKER_RELATION {
            return Err("bootstrap cannot reference the reserved install marker".into());
        }
        let relation = program
            .rel_id(&source_fact.rel)
            .ok_or_else(|| format!("bootstrap names undeclared relation `{}`", source_fact.rel))?;
        let values = source_fact
            .values
            .iter()
            .map(|value| lower_bootstrap_value(value, entities))
            .collect::<Result<Vec<_>, _>>()?;
        let tuple = Tuple::new(values);
        let schema = program
            .schema(&source_fact.rel)
            .expect("declared relation has schema");
        schema
            .check(&tuple)
            .map_err(|error| format!("bootstrap `{}`: {error}", source_fact.rel))?;
        let key = (source_fact.rel.clone(), tuple.clone());
        if !seen.insert(key) {
            return Err(format!(
                "bootstrap fact `{}` is duplicated",
                source_fact.rel
            ));
        }
        compiled.push(CompiledBootstrapFact {
            relation_name: source_fact.rel.clone(),
            fact: Fact::new(relation, tuple),
        });
    }
    compiled.sort_by(|left, right| {
        (left.relation_name.as_str(), &left.fact.tuple)
            .cmp(&(right.relation_name.as_str(), &right.fact.tuple))
    });
    Ok(compiled)
}

fn lower_bootstrap_value(
    value: &BootstrapValue,
    entities: &BTreeMap<String, Entity>,
) -> Result<Value, String> {
    Ok(match value {
        BootstrapValue::Entity(name) => Value::Ent(
            entities
                .get(name)
                .copied()
                .ok_or_else(|| format!("bootstrap names unknown entity `{name}`"))?,
        ),
        BootstrapValue::Int(value) => Value::Int(*value),
        BootstrapValue::Float(value) => Value::Float(*value),
        BootstrapValue::Text(value) => Value::text(value),
        BootstrapValue::Bool(value) => Value::Bool(*value),
        BootstrapValue::Tuple(values) => Value::Tuple(
            values
                .iter()
                .map(|value| lower_bootstrap_value(value, entities))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        ),
    })
}

fn validate_resource_seeds(
    facts: &[CompiledBootstrapFact],
    requirements: &[CapabilityRequirement],
) -> Result<(), String> {
    for requirement in requirements {
        match requirement {
            CapabilityRequirement::Allocate {
                name,
                counter,
                first,
                ..
            } => {
                let matches: Vec<_> = facts
                    .iter()
                    .filter(|fact| fact.relation_name == *counter)
                    .collect();
                if matches.len() != 1 || matches[0].fact.tuple != Tuple::from([Value::Int(*first)])
                {
                    return Err(format!(
                        "allocate requirement `{name}` needs exactly one bootstrap row \
                         `{counter}({first})`"
                    ));
                }
            }
            CapabilityRequirement::Random {
                name, state, owner, ..
            } => {
                let matches: Vec<_> = facts
                    .iter()
                    .filter(|fact| {
                        fact.relation_name == *state
                            && fact.fact.tuple.as_slice().first() == Some(&Value::Ent(*owner))
                    })
                    .collect();
                if matches.len() != 1
                    || !matches!(matches[0].fact.tuple.as_slice().get(1), Some(Value::Int(seed)) if *seed != 0)
                {
                    return Err(format!(
                        "random requirement `{name}` needs exactly one nonzero bootstrap state row"
                    ));
                }
            }
            CapabilityRequirement::Schedule { .. } => {}
        }
    }
    Ok(())
}

fn assign_reserved(catalog: &dyn Catalog, rel_base: u32, name: &str) -> Result<RelId, String> {
    if let Some(id) = catalog.rel_id(name).map_err(|error| error.to_string())? {
        return Ok(id);
    }
    let next = catalog
        .entries()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(_, id)| id.0)
        .max()
        .map_or(rel_base, |highest| rel_base.max(highest + 1));
    let id = RelId(next);
    catalog
        .register(name, id)
        .map_err(|error| error.to_string())?;
    Ok(id)
}

fn digest(
    package_id: &str,
    version: u32,
    entities: &BTreeMap<String, Entity>,
    facts: &[CompiledBootstrapFact],
    requirements: &[CapabilityRequirement],
) -> Sha256Digest {
    let mut bytes = Vec::new();
    put_bytes(b"grmpl-package-bootstrap-v1", &mut bytes);
    bytes.push(wire::FORMAT_VERSION);
    put_bytes(package_id.as_bytes(), &mut bytes);
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&(entities.len() as u32).to_be_bytes());
    for (name, entity) in entities {
        put_bytes(name.as_bytes(), &mut bytes);
        bytes.extend_from_slice(&entity.0.to_be_bytes());
    }
    bytes.extend_from_slice(&(facts.len() as u32).to_be_bytes());
    for fact in facts {
        put_bytes(fact.relation_name.as_bytes(), &mut bytes);
        let mut tuple = Vec::new();
        wire::encode_tuple(&fact.fact.tuple, &mut tuple);
        put_bytes(&tuple, &mut bytes);
    }
    bytes.extend_from_slice(&(requirements.len() as u32).to_be_bytes());
    for requirement in requirements {
        match requirement {
            CapabilityRequirement::Allocate {
                name,
                counter,
                first,
                last,
            } => {
                bytes.push(0);
                put_bytes(name.as_bytes(), &mut bytes);
                put_bytes(counter.as_bytes(), &mut bytes);
                bytes.extend_from_slice(&first.to_be_bytes());
                bytes.extend_from_slice(&last.to_be_bytes());
            }
            CapabilityRequirement::Random {
                name,
                state,
                owner,
                algorithm,
            } => {
                bytes.push(1);
                put_bytes(name.as_bytes(), &mut bytes);
                put_bytes(state.as_bytes(), &mut bytes);
                bytes.extend_from_slice(&owner.0.to_be_bytes());
                put_bytes(algorithm.as_bytes(), &mut bytes);
            }
            CapabilityRequirement::Schedule {
                name,
                clock,
                timers,
                sequences,
            } => {
                bytes.push(2);
                put_bytes(name.as_bytes(), &mut bytes);
                put_bytes(clock.as_bytes(), &mut bytes);
                put_bytes(timers.as_bytes(), &mut bytes);
                put_bytes(sequences.as_bytes(), &mut bytes);
            }
        }
    }
    sha256(&bytes)
}

fn put_bytes(value: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}
