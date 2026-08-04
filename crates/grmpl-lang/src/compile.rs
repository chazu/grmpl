//! Lowering the AST to core constructors.
//!
//! Every executable fragment is lowered in two steps: the AST is first compiled
//! into the inspectable [core IR](crate::ir) (a [`QueryIr`] for a `view`, a
//! [`FormIr`] for a `form`), and only then is that IR `lower`ed to a runnable
//! `Query` / `Form`, which is the single point where `Fn` closures are
//! generated. The public [`Program::view`] / [`Program::form`] chain the two;
//! [`Program::view_ir`] / [`Program::form_ir`] hand back the IR itself for the
//! phases that consume it (typing, constructor inversion, storage).
//!
//! * `rel`  → a relation schema (name → `RelId` + **named, typed columns**).
//!   The optional `col: Ty` annotation lowers to a `grmpl_core::Ty`; an
//!   unannotated column defaults to the permissive `Ty::Any`. A compiled
//!   program can hand each relation's `Schema` to a durable `SchemaCatalog`
//!   (see [`Program::register_schemas`]) so the commit boundary enforces
//!   arity/type. Column *names* drive resolution in `view`/`on`.
//! * `view` → a `Query`: the conjunctive body is planned left-to-right, joining
//!   each atom to the accumulated relation on their shared variables, then
//!   projecting the `yield` variables and de-duplicating. Literals and repeated
//!   variables within an atom become filters; view parameters pin a column to a
//!   supplied value at instantiation. This is the concrete realization of
//!   "`lamp.location` compiles to a relational query" (DESIGN.md §3).
//! * `form` → a `grmpl_pattern::Form`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use grmpl_core::{
    Authority, Catalog, Column, Edition, Entity, Patch, RelId, Result as CoreResult, Schema,
    SchemaCatalog, TraceStore, Tuple, Ty, Value,
};
use grmpl_diff::{Agg, Query, Snapshot};
use grmpl_pattern::{Form, Pattern, VarId};
use grmpl_proc::{Behavior, CommitOutcome, OnWatch};

use crate::ast::{
    AggFunc, AggYield, Arg, Arm, BinaryOp, Decl, Expr, MatchOp, PAtom, SArg, Stmt, UnaryOp,
};
use crate::behavior_ir::{BehaviorIr, BehaviorOp, BoolExpr, CompareOp, ExprIr, FindArg, ValueExpr};
use crate::concat::{ConcatArm, Schemas, Word};
use crate::ir::{Comp, CtorSpec, FormIr, PredExpr, QueryIr, RowExpr, RuleIr};
use crate::package::ResolvedGrantSet;
use crate::parser::parse;

/// An aggregate named by *column* — the P1 named-column surface for
/// [`Program::reduce_view`]. `Sum`/`Min`/`Max` name a yielded column; `Count`
/// ignores values. Resolved to a positional [`grmpl_diff::Agg`] against the
/// view's `yield` list.
#[derive(Clone, PartialEq, Debug)]
pub enum NamedAgg {
    Count,
    Sum(String),
    Min(String),
    Max(String),
}

struct RelInfo {
    id: RelId,
    columns: Vec<Column>,
}

impl RelInfo {
    fn arity(&self) -> usize {
        self.columns.len()
    }
}

struct ViewDef {
    params: Vec<String>,
    atoms: Vec<crate::ast::Atom>,
    yields: Vec<String>,
    /// The optional single aggregate of the `yield` clause. `None` → a plain
    /// projection view; `Some(_)` → the view groups by `yields` and lowers to a
    /// `Query::Reduce` (P2 aggregate yield surface).
    agg: Option<AggYield>,
}

struct OnDef {
    form: String,
    /// v1 statement arms and P11 concatenative arms coexist in one handler.
    arms: Vec<Arm>,
    concat_arms: Vec<ConcatArm>,
}

/// The static wiring of an `on watch` declaration (P5). It names the watched
/// `view` and the three relations the pump touches; the runtime identities the
/// source cannot name (view args, the cursor-key/target entities, the pump
/// authority) are supplied at lowering time by [`Program::on_watch`]. Keyed by
/// view name — v1 allows at most one `on watch` per view.
struct WatchDef {
    inbox: String,
    cursor: String,
    seqs: String,
    /// The `including current` opt-in: `true` lowers to
    /// [`OnWatch::install_including_current`] (deliver the current view once),
    /// `false` to the skip-initial default [`OnWatch::install`].
    including_current: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapabilityKind {
    Allocate,
    Random,
    Schedule,
}

#[derive(Clone, Copy)]
struct CapabilityDef {
    kind: CapabilityKind,
    state_relation: RelId,
}

#[derive(Clone)]
struct ActorDef {
    inbox: String,
}

/// A compiled program: relation schemas, views, forms, and on-handlers.
pub struct Program {
    rels: HashMap<String, RelInfo>,
    views: HashMap<String, ViewDef>,
    forms: HashMap<String, Vec<crate::ast::FormRule>>,
    ons: HashMap<String, OnDef>,
    /// `on watch` reactive handlers, keyed by watched view name.
    watches: HashMap<String, WatchDef>,
    /// Package compile-time entity constants. Legacy programs leave this empty.
    entities: BTreeMap<String, Entity>,
    capabilities: BTreeMap<String, CapabilityDef>,
    actors: BTreeMap<String, ActorDef>,
}

/// How a `compile` run assigns a [`RelId`] to each declared relation. The two
/// implementations are the two id-allocation policies: [`SeqAlloc`] hands out
/// ids sequentially from a base (declaration order *is* the id order), while
/// [`CatalogAlloc`] resolves each name against a durable [`Catalog`] so an id,
/// once bound, is recovered on reopen no matter where the `rel` now sits in the
/// source — the TKT-90 stability property.
trait RelAlloc {
    /// The id for `name`, allocating (and, for the catalog, durably binding) a
    /// fresh one if this is the first time the name is seen.
    fn assign(&mut self, name: &str) -> Result<RelId, String>;
}

/// Sequential allocation from a base — the catalog-free policy. `next` is the
/// id the *next* fresh relation receives.
struct SeqAlloc {
    next: u32,
}

impl RelAlloc for SeqAlloc {
    fn assign(&mut self, _name: &str) -> Result<RelId, String> {
        let id = RelId(self.next);
        self.next += 1;
        Ok(id)
    }
}

/// Catalog-backed allocation: an already-bound name resolves to its durable id;
/// an unbound name is assigned a fresh id *above every id already in the
/// catalog* and registered on the spot, so the binding survives the reopen and
/// no fresh id ever collides with an existing one.
struct CatalogAlloc<'a> {
    catalog: &'a dyn Catalog,
    /// The id the next *fresh* (unbound) relation receives — kept above every
    /// id in the catalog and above every id handed out earlier in this run.
    next: u32,
}

impl<'a> CatalogAlloc<'a> {
    /// Snapshot the catalog's high-water id once, up front, then hand out fresh
    /// ids above it. This read-then-register sequence is **not** atomic against
    /// a *concurrent* compile sharing the same catalog: two compiles that each
    /// snapshot the same high-water mark could assign the same fresh id to two
    /// distinct names. `compile_with_catalog` is therefore a **provisioning-time,
    /// single-threaded** operation — run it while nothing else is registering
    /// names, not from racing threads against one live catalog.
    fn new(catalog: &'a dyn Catalog, rel_base: u32) -> Result<Self, String> {
        let highest = catalog
            .entries()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|(_, id)| id.0)
            .max();
        // Fresh ids start at `rel_base`, but never at or below an id already
        // bound — otherwise a new relation could shadow an existing one.
        let next = match highest {
            Some(h) => rel_base.max(h + 1),
            None => rel_base,
        };
        Ok(Self { catalog, next })
    }
}

impl RelAlloc for CatalogAlloc<'_> {
    fn assign(&mut self, name: &str) -> Result<RelId, String> {
        if let Some(id) = self.catalog.rel_id(name).map_err(|e| e.to_string())? {
            return Ok(id);
        }
        let id = RelId(self.next);
        self.next += 1;
        self.catalog.register(name, id).map_err(|e| e.to_string())?;
        Ok(id)
    }
}

impl Program {
    /// Parse and compile source text. `rel`s are assigned `RelId`s sequentially
    /// starting at `rel_base`, so callers can align a store's relations with it.
    /// Ids follow **declaration order**; to instead recover stable ids across
    /// reopens from a durable catalog, use
    /// [`compile_with_catalog`](Self::compile_with_catalog).
    pub fn compile(src: &str, rel_base: u32) -> Result<Program, String> {
        Program::compile_alloc(src, &mut SeqAlloc { next: rel_base })
    }

    /// Parse and compile `src`, resolving each relation's `RelId` through the
    /// durable [`Catalog`] instead of from declaration order. A name already
    /// bound in `catalog` keeps its id; a name not yet bound is assigned a fresh
    /// id (above every id in the catalog, at least `rel_base`) and **registered
    /// on the spot**, so recompiling the same world after a reopen — even with
    /// the `rel` declarations reordered or new ones interleaved — yields the
    /// same id for every previously-seen relation. This is the stability
    /// property TKT-72's catalog was built for: physical ids no longer depend on
    /// source layout. Schemas are still recorded separately via
    /// [`register_schemas`](Self::register_schemas).
    ///
    /// **Provisioning-time / single-threaded.** Fresh-id assignment snapshots
    /// the catalog's high-water mark and then registers above it; that
    /// read-then-register is not atomic across *concurrent* compiles, so two
    /// threads compiling against one shared catalog could hand the same fresh id
    /// to distinct names. Compile while nothing else is registering names.
    pub fn compile_with_catalog(
        src: &str,
        catalog: &dyn Catalog,
        rel_base: u32,
    ) -> Result<Program, String> {
        let mut alloc = CatalogAlloc::new(catalog, rel_base)?;
        Program::compile_alloc(src, &mut alloc)
    }

    /// The shared compile body, parameterized over the id-allocation policy
    /// ([`SeqAlloc`] vs [`CatalogAlloc`]). Everything else — parsing, duplicate
    /// checks, column typing, concatenative stack-effect checking — is identical
    /// regardless of where ids come from.
    fn compile_alloc(src: &str, alloc: &mut dyn RelAlloc) -> Result<Program, String> {
        let decls = parse(src)?;
        let mut rels = HashMap::new();
        let mut views = HashMap::new();
        let mut forms = HashMap::new();
        let mut ons = HashMap::new();
        let mut watches = HashMap::new();
        let mut capabilities = Vec::new();
        let mut actors = BTreeMap::new();

        for decl in decls {
            match decl {
                Decl::Package { .. }
                | Decl::Entity { .. }
                | Decl::Authority { .. }
                | Decl::Bootstrap { .. } => {}
                Decl::RequireAllocate { name, counter, .. } => {
                    capabilities.push((name, counter, CapabilityKind::Allocate));
                }
                Decl::RequireRandom { name, state, .. } => {
                    capabilities.push((name, state, CapabilityKind::Random));
                }
                Decl::RequireSchedule { name, timers, .. } => {
                    capabilities.push((name, timers, CapabilityKind::Schedule));
                }
                Decl::Actor { entity, inbox, .. } => {
                    if actors.insert(entity.clone(), ActorDef { inbox }).is_some() {
                        return Err(format!("actor `{entity}` is declared twice"));
                    }
                }
                Decl::Rel { name, cols } => {
                    if rels.contains_key(&name) {
                        return Err(format!("relation `{name}` declared twice"));
                    }
                    let mut columns = Vec::with_capacity(cols.len());
                    for c in &cols {
                        if columns.iter().any(|col: &Column| col.name == c.name) {
                            return Err(format!(
                                "relation `{name}` has a duplicate column `{}`",
                                c.name
                            ));
                        }
                        let ty = match &c.ty {
                            None => Ty::Any,
                            Some(t) => Ty::parse(t).ok_or_else(|| {
                                format!(
                                    "relation `{name}` column `{}` has unknown type `{t}` \
                                     (expected Ent, Int, Text, Bool, Tuple, Bytes, or Any)",
                                    c.name
                                )
                            })?,
                        };
                        columns.push(Column::new(c.name.clone(), ty));
                    }
                    let id = alloc.assign(&name)?;
                    rels.insert(name, RelInfo { id, columns });
                }
                Decl::View {
                    name,
                    params,
                    atoms,
                    yields,
                    agg,
                } => {
                    views.insert(
                        name,
                        ViewDef {
                            params,
                            atoms,
                            yields,
                            agg,
                        },
                    );
                }
                Decl::Form { name, rules } => {
                    forms.insert(name, rules);
                }
                Decl::On {
                    inbox,
                    form,
                    stmt_arms,
                    word_arms,
                } => {
                    ons.insert(
                        inbox,
                        OnDef {
                            form,
                            arms: stmt_arms,
                            concat_arms: word_arms,
                        },
                    );
                }
                Decl::OnWatch {
                    view,
                    inbox,
                    cursor,
                    seqs,
                    including_current,
                } => {
                    if watches.contains_key(&view) {
                        return Err(format!("view `{view}` is watched twice"));
                    }
                    watches.insert(
                        view,
                        WatchDef {
                            inbox,
                            cursor,
                            seqs,
                            including_current,
                        },
                    );
                }
            }
        }
        let mut prog = Program {
            rels,
            views,
            forms,
            ons,
            watches,
            entities: BTreeMap::new(),
            capabilities: BTreeMap::new(),
            actors,
        };
        for (name, relation, kind) in capabilities {
            let relation_id = prog.rel_id(&relation).ok_or_else(|| {
                format!("capability `{name}` names undeclared relation `{relation}`")
            })?;
            prog.insert_capability(name, kind, relation_id)?;
        }
        // "Declared stack effects first": statically check every concatenative
        // arm's cell arithmetic now, so a malformed point-free body fails at
        // compile time rather than mid-commit.
        for on in prog.ons.values() {
            for arm in &on.concat_arms {
                arm.check(&prog)?;
            }
        }
        Ok(prog)
    }

    /// The yielded column names of a view (in order).
    pub fn view_yields(&self, name: &str) -> Option<&[String]> {
        self.views.get(name).map(|v| v.yields.as_slice())
    }

    /// The statement arms of the `on` handler bound to `inbox`, or `None` if none
    /// is declared. Exposed so P8b effect-row inference can walk a handler's
    /// write statements (`assert`/`retract`/`emit`) without re-parsing the
    /// source. Concatenative arms are exposed separately via
    /// [`on_concat_arms`](Self::on_concat_arms).
    pub fn on_arms(&self, inbox: &str) -> Option<&[Arm]> {
        self.ons.get(inbox).map(|o| o.arms.as_slice())
    }

    /// The concatenative (point-free) arms of the `on` handler bound to `inbox`.
    /// Exposed alongside [`on_arms`](Self::on_arms) so effect inference and other
    /// passes treat both surfaces uniformly — the two coexist over one handler.
    pub fn on_concat_arms(&self, inbox: &str) -> Option<&[ConcatArm]> {
        self.ons.get(inbox).map(|o| o.concat_arms.as_slice())
    }

    /// The `RelId` assigned to a declared relation.
    pub fn rel_id(&self, name: &str) -> Option<RelId> {
        self.rels.get(name).map(|r| r.id)
    }

    /// Resolve a package entity constant.
    pub fn entity(&self, name: &str) -> Option<Entity> {
        self.entities.get(name).copied()
    }

    pub(crate) fn set_entities(&mut self, entities: BTreeMap<String, Entity>) {
        self.entities = entities;
    }

    pub(crate) fn insert_capability(
        &mut self,
        name: String,
        kind: CapabilityKind,
        state_relation: RelId,
    ) -> Result<(), String> {
        if self
            .capabilities
            .insert(
                name.clone(),
                CapabilityDef {
                    kind,
                    state_relation,
                },
            )
            .is_some()
        {
            return Err(format!("capability `{name}` is declared twice"));
        }
        Ok(())
    }

    pub fn capability(&self, name: &str) -> Option<(CapabilityKind, RelId)> {
        self.capabilities
            .get(name)
            .map(|definition| (definition.kind, definition.state_relation))
    }

    pub(crate) fn validate_entity_namespace(
        &self,
        entities: &BTreeMap<String, Entity>,
    ) -> Result<(), String> {
        if entities.contains_key("self") {
            return Err("entity constant `self` is reserved by behaviors".into());
        }
        for (view_name, view) in &self.views {
            for name in view.params.iter().chain(view.yields.iter()) {
                if entities.contains_key(name) {
                    return Err(format!(
                        "view `{view_name}` binds `{name}`, which is an entity constant"
                    ));
                }
            }
        }
        for (inbox, on) in &self.ons {
            for arm in &on.arms {
                for name in &arm.vars {
                    if entities.contains_key(name) {
                        return Err(format!(
                            "handler `{inbox}` binds `{name}`, which is an entity constant"
                        ));
                    }
                }
            }
            for arm in &on.concat_arms {
                for name in &arm.vars {
                    if entities.contains_key(name) {
                        return Err(format!(
                            "handler `{inbox}` binds `{name}`, which is an entity constant"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn insert_reserved_relation(
        &mut self,
        name: String,
        id: RelId,
        columns: Vec<Column>,
    ) -> Result<(), String> {
        if self
            .rels
            .insert(name.clone(), RelInfo { id, columns })
            .is_some()
        {
            return Err(format!("reserved relation `{name}` collides with source"));
        }
        Ok(())
    }

    /// The declared, named, typed columns of a relation, in order.
    pub fn rel_columns(&self, name: &str) -> Option<&[Column]> {
        self.rels.get(name).map(|r| r.columns.as_slice())
    }

    /// The `Schema` (named, typed columns) of a declared relation.
    pub fn schema(&self, name: &str) -> Option<Schema> {
        self.rels.get(name).map(|r| Schema::new(r.columns.clone()))
    }

    /// Named-column resolution: the tuple position of column `col` in relation
    /// `rel`, or `None` if either is undeclared. This is the primitive that
    /// turns a column *name* into an index for `view`/`on`.
    pub fn resolve_column(&self, rel: &str, col: &str) -> Option<usize> {
        self.rels
            .get(rel)
            .and_then(|r| r.columns.iter().position(|c| c.name == col))
    }

    /// Register every declared relation into the durable registry: bind its
    /// `name → RelId` in `catalog` and record its `Schema` in `schemas`,
    /// effective as-of `at`. After this, the commit boundary enforces each
    /// relation's arity/types. Relations are registered in a stable order (by
    /// id) so the effect is deterministic.
    ///
    /// The `name → RelId` binding is idempotent, so this composes cleanly with
    /// [`compile_with_catalog`](Self::compile_with_catalog): if the program was
    /// compiled against the same catalog, every id already matches and the
    /// `register` calls here are no-ops that only add the schemas. If it was
    /// compiled with [`compile`](Self::compile) (declaration-order ids), this is
    /// the first time the catalog learns those ids — but from then on they are
    /// stable, so a later `compile_with_catalog` recovers them.
    pub fn register_schemas(
        &self,
        catalog: &dyn Catalog,
        schemas: &dyn SchemaCatalog,
        at: Edition,
    ) -> CoreResult<()> {
        let mut rels: Vec<(&String, &RelInfo)> = self.rels.iter().collect();
        rels.sort_by_key(|(_, r)| r.id.0);
        for (name, info) in rels {
            catalog.register(name, info.id)?;
            schemas.put_schema(info.id, &Schema::new(info.columns.clone()), at)?;
        }
        Ok(())
    }

    /// Instantiate a view as a runnable `Query`, binding its parameters to
    /// `args`. This is [`view_ir`](Self::view_ir) followed by the final
    /// [`QueryIr::lower`].
    pub fn view(&self, name: &str, args: &[Value]) -> Result<Query, String> {
        Ok(self.view_ir(name, args)?.lower())
    }

    /// Instantiate a view as the inspectable [`QueryIr`], binding its parameters
    /// to `args`. The conjunctive body is planned left-to-right: each atom
    /// becomes a `Rel` optionally wrapped in a `Filter` (literals, view
    /// parameters, and repeated variables within the atom become equality
    /// predicates), joined to the accumulated plan on their shared variables,
    /// and the whole is projected onto the `yield` variables and de-duplicated.
    /// No closures are generated here — that is deferred to `lower`.
    ///
    /// If the `yield` clause carries an aggregate (`yield t, sum(pts)`), the
    /// projection is instead wrapped in a [`QueryIr::Reduce`]: the plain
    /// `yield` identifiers become the grouping key and the aggregate folds its
    /// column. This is the text-surface counterpart of
    /// [`reduce_view`](Self::reduce_view) and lowers to the same `Query::Reduce`.
    pub fn view_ir(&self, name: &str, args: &[Value]) -> Result<QueryIr, String> {
        let v = self
            .views
            .get(name)
            .ok_or_else(|| format!("no view `{name}`"))?;
        if args.len() != v.params.len() {
            return Err(format!(
                "view `{name}` takes {} argument(s), got {}",
                v.params.len(),
                args.len()
            ));
        }
        let params: HashMap<&str, Value> = v
            .params
            .iter()
            .map(|s| s.as_str())
            .zip(args.iter().cloned())
            .collect();

        let mut varcol: HashMap<String, usize> = HashMap::new();
        let mut acc: Option<QueryIr> = None;
        let mut width = 0usize;

        for atom in &v.atoms {
            let info = self
                .rels
                .get(&atom.rel)
                .ok_or_else(|| format!("view `{name}` uses undeclared relation `{}`", atom.rel))?;
            if atom.args.len() != info.arity() {
                return Err(format!(
                    "`{}` has arity {} but was used with {} args",
                    atom.rel,
                    info.arity(),
                    atom.args.len()
                ));
            }

            let mut local_first: Vec<(String, usize)> = Vec::new();
            let mut preds: Vec<PredExpr> = Vec::new();

            for (i, arg) in atom.args.iter().enumerate() {
                match arg {
                    Arg::Str(s) => {
                        preds.push(PredExpr::Eq(RowExpr::Col(i), RowExpr::Lit(Value::text(s))));
                    }
                    Arg::Int(n) => {
                        preds.push(PredExpr::Eq(RowExpr::Col(i), RowExpr::Lit(Value::Int(*n))));
                    }
                    Arg::Float(n) => {
                        preds.push(PredExpr::Eq(
                            RowExpr::Col(i),
                            RowExpr::Lit(Value::Float(*n)),
                        ));
                    }
                    Arg::Bool(value) => {
                        preds.push(PredExpr::Eq(
                            RowExpr::Col(i),
                            RowExpr::Lit(Value::Bool(*value)),
                        ));
                    }
                    Arg::Var(vn) => {
                        if let Some(entity) = self.entity(vn) {
                            preds.push(PredExpr::Eq(
                                RowExpr::Col(i),
                                RowExpr::Lit(Value::Ent(entity)),
                            ));
                            continue;
                        }
                        if let Some(val) = params.get(vn.as_str()) {
                            preds.push(PredExpr::Eq(RowExpr::Col(i), RowExpr::Lit(val.clone())));
                        }
                        match local_first.iter().find(|(n, _)| n == vn) {
                            Some((_, first)) => {
                                preds.push(PredExpr::Eq(RowExpr::Col(*first), RowExpr::Col(i)));
                            }
                            None => local_first.push((vn.clone(), i)),
                        }
                    }
                }
            }

            let mut q = QueryIr::Rel(info.id);
            if !preds.is_empty() {
                q = QueryIr::Filter {
                    input: Box::new(q),
                    pred: PredExpr::And(preds),
                };
            }

            match acc.take() {
                None => {
                    for (vn, pos) in &local_first {
                        varcol.insert(vn.clone(), *pos);
                    }
                    acc = Some(q);
                    width = info.arity();
                }
                Some(prev) => {
                    let mut lk = Vec::new();
                    let mut rk = Vec::new();
                    for (vn, apos) in &local_first {
                        if let Some(cpos) = varcol.get(vn) {
                            lk.push(*cpos);
                            rk.push(*apos);
                        }
                    }
                    acc = Some(QueryIr::Join {
                        left: Box::new(prev),
                        right: Box::new(q),
                        left_key: lk,
                        right_key: rk,
                    });
                    for (vn, apos) in &local_first {
                        varcol.entry(vn.clone()).or_insert(width + *apos);
                    }
                    width += info.arity();
                }
            }
        }

        let base = acc.ok_or_else(|| format!("view `{name}` has no atoms"))?;
        // Project the grouping columns, then (for an aggregate yield) the
        // aggregate's column. This lays the group keys in positions
        // `0..yields.len()` and the folded value at `yields.len()`, exactly the
        // layout `reduce_view` produces over a plain view yielding
        // `[group…, col]` — so the text surface and the programmatic
        // `reduce_view`/`NamedAgg` surface lower to the same `Query::Reduce`.
        let mut proj: Vec<&str> = v.yields.iter().map(|s| s.as_str()).collect();
        if let Some(AggYield { col: Some(c), .. }) = &v.agg {
            proj.push(c.as_str());
        }
        let mut cols = Vec::new();
        for y in &proj {
            let c = *varcol
                .get(*y)
                .ok_or_else(|| format!("view `{name}` yields unbound variable `{y}`"))?;
            cols.push(c);
        }
        let projected = QueryIr::Distinct(Box::new(QueryIr::Project {
            input: Box::new(base),
            cols,
        }));
        match &v.agg {
            None => Ok(projected),
            Some(a) => {
                // Group by the plain columns; fold the aggregate over the column
                // parked right after them (`Count` ignores values).
                let key: Vec<usize> = (0..v.yields.len()).collect();
                let agg_idx = v.yields.len();
                let agg = match a.func {
                    AggFunc::Count => Agg::Count,
                    AggFunc::Sum => Agg::Sum(agg_idx),
                    AggFunc::Min => Agg::Min(agg_idx),
                    AggFunc::Max => Agg::Max(agg_idx),
                };
                Ok(QueryIr::Reduce {
                    input: Box::new(projected),
                    key,
                    agg,
                })
            }
        }
    }

    /// Aggregate over a view's yielded columns *by name*: instantiate `view`
    /// with `args`, group by the named `group` columns, and fold `agg` over its
    /// (optional) named column. The resulting `Query` yields the grouping
    /// columns followed by the aggregate. This is the P1 named-column surface for
    /// aggregates — column names resolve against the view's `yield` list, so it
    /// depends on P1's named columns. Errors if any name is not yielded.
    pub fn reduce_view(
        &self,
        view: &str,
        args: &[Value],
        group: &[&str],
        agg: NamedAgg,
    ) -> Result<Query, String> {
        Ok(self.reduce_view_ir(view, args, group, agg)?.lower())
    }

    /// [`reduce_view`](Self::reduce_view) as inspectable IR: the view's
    /// [`QueryIr`] wrapped in a [`QueryIr::Reduce`].
    pub fn reduce_view_ir(
        &self,
        view: &str,
        args: &[Value],
        group: &[&str],
        agg: NamedAgg,
    ) -> Result<QueryIr, String> {
        let base = self.view_ir(view, args)?;
        let yields = self
            .view_yields(view)
            .ok_or_else(|| format!("no view `{view}`"))?
            .to_vec();
        let idx = |col: &str| -> Result<usize, String> {
            yields
                .iter()
                .position(|y| y == col)
                .ok_or_else(|| format!("view `{view}` yields no column `{col}`"))
        };
        let mut key = Vec::with_capacity(group.len());
        for g in group {
            key.push(idx(g)?);
        }
        let agg = match agg {
            NamedAgg::Count => Agg::Count,
            NamedAgg::Sum(c) => Agg::Sum(idx(&c)?),
            NamedAgg::Min(c) => Agg::Min(idx(&c)?),
            NamedAgg::Max(c) => Agg::Max(idx(&c)?),
        };
        Ok(QueryIr::Reduce {
            input: Box::new(base),
            key,
            agg,
        })
    }

    /// Build a runnable parser from a declared `form`. This is
    /// [`form_ir`](Self::form_ir) followed by the final [`FormIr::lower`].
    pub fn form(&self, name: &str) -> Result<Form, String> {
        Ok(self.form_ir(name)?.lower())
    }

    /// Compile a declared `form` into the inspectable [`FormIr`]. Each rule's
    /// pattern is lowered to a `grmpl_pattern::Pattern` (already structural
    /// data) and its `-> Tag(args)` arrow to an inspectable [`CtorSpec`] — the
    /// constructor closure is generated only by `lower`.
    pub fn form_ir(&self, name: &str) -> Result<FormIr, String> {
        let rules_ast = self
            .forms
            .get(name)
            .ok_or_else(|| format!("no form `{name}`"))?;
        let mut rules = Vec::new();
        for r in rules_ast {
            // Assign a VarId to each distinct bind name in this rule.
            let mut ids: HashMap<String, u32> = HashMap::new();
            let mut pats = Vec::new();
            for pa in &r.seq {
                match pa {
                    PAtom::Lit(s) => pats.push(Pattern::Lit(Value::text(s))),
                    PAtom::Bind(v) => {
                        let next = ids.len() as u32;
                        let id = *ids.entry(v.clone()).or_insert(next);
                        pats.push(Pattern::Bind(VarId(id)));
                    }
                }
            }
            let args: Vec<VarId> = r
                .ctor_args
                .iter()
                .map(|a| {
                    ids.get(a)
                        .copied()
                        .map(VarId)
                        .ok_or_else(|| format!("ctor arg `{a}` is not bound in the pattern"))
                })
                .collect::<Result<_, _>>()?;
            let ctor = CtorSpec {
                tag: r.tag.clone(),
                args,
            };
            rules.push(RuleIr {
                pattern: Pattern::Seq(pats),
                ctor,
            });
        }
        Ok(FormIr { rules })
    }

    /// Name the computation of *materializing* a view at an edition: the view's
    /// [`QueryIr`] wrapped as [`Comp::Find`]. The plan is data until run.
    pub fn find_view(&self, name: &str, args: &[Value]) -> Result<Comp, String> {
        Ok(Comp::Find(self.view_ir(name, args)?))
    }

    /// Name the computation of *maintaining* a view as a delta stream — the
    /// plan a P5 `on watch` reactive handler installs. The view's [`QueryIr`]
    /// wrapped as [`Comp::Watch`]; `plan().lower()` yields the `Query` for
    /// `grmpl_proc::OnWatch`.
    pub fn watch_view(&self, name: &str, args: &[Value]) -> Result<Comp, String> {
        Ok(Comp::Watch(self.view_ir(name, args)?))
    }

    /// Whether the `on watch` over `view` opted into `including current`, or
    /// `None` if no `on watch` names that view. `true` means its lowering
    /// installs with [`OnWatch::install_including_current`] (deliver the current
    /// view once); `false` with the skip-initial default [`OnWatch::install`].
    pub fn watch_including_current(&self, view: &str) -> Option<bool> {
        self.watches.get(view).map(|w| w.including_current)
    }

    /// Lower an `on watch` declaration to a runnable [`grmpl_proc::OnWatch`],
    /// binding the runtime identities the *source* does not name: the view
    /// arguments `args`, the durable cursor-key `watch` entity, the inbox
    /// addressee `target` entity, and the pump `authority`. The view's `Query`
    /// and the `inbox` / `cursor` / `seqs` relation ids come from the compiled
    /// program.
    ///
    /// This is a **pure lowering** — it builds the wired struct and commits
    /// nothing. Installation (which chooses skip-initial vs `including current`)
    /// is the caller's step; [`install_watch`](Self::install_watch) does both,
    /// or read [`watch_including_current`](Self::watch_including_current) and call
    /// [`OnWatch::install`] / [`OnWatch::install_including_current`] directly.
    ///
    /// Naming only the substrate **traits** (`TraceStore`, `SchemaCatalog`) and
    /// core value types, this stays above the bright line: the language wires a
    /// reactive handler without ever naming a storage or transport technology,
    /// and the resulting `OnWatch` observes opaque `Edition`s.
    pub fn on_watch(
        &self,
        view: &str,
        args: &[Value],
        watch: Entity,
        target: Entity,
        authority: Authority,
    ) -> Result<OnWatch, String> {
        let w = self
            .watches
            .get(view)
            .ok_or_else(|| format!("no `on watch` over view `{view}`"))?;
        let rel = |name: &str, role: &str| -> Result<RelId, String> {
            self.rel_id(name).ok_or_else(|| {
                format!("`on watch {view}` names undeclared {role} relation `{name}`")
            })
        };
        Ok(OnWatch {
            view: self.view(view, args)?,
            watch,
            target,
            inbox: rel(&w.inbox, "inbox")?,
            cursor_rel: rel(&w.cursor, "cursor")?,
            seqs: rel(&w.seqs, "seqs")?,
            authority,
        })
    }

    /// Lower an `on watch` and **install** it, choosing the initial-delivery mode
    /// from the source: `including current` installs with
    /// [`OnWatch::install_including_current`] (the current view is delivered once
    /// on the first pump), the default with the skip-initial [`OnWatch::install`].
    /// Returns the installed [`OnWatch`] (so the caller can [`pump`](OnWatch::pump)
    /// it) alongside the install commit's [`CommitOutcome`].
    ///
    /// This is [`on_watch`](Self::on_watch) plus the one-line install branch that
    /// the `including current` opt-in selects — the single call that turns the
    /// text surface into a live reactive handler.
    #[allow(clippy::too_many_arguments)]
    pub fn install_watch(
        &self,
        view: &str,
        args: &[Value],
        watch: Entity,
        target: Entity,
        authority: Authority,
        store: &dyn TraceStore,
        schemas: &dyn SchemaCatalog,
    ) -> Result<(OnWatch, CommitOutcome), String> {
        let ow = self.on_watch(view, args, watch, target, authority)?;
        let including_current = self.watches[view].including_current;
        let outcome = if including_current {
            ow.install_including_current(store, schemas)
        } else {
            ow.install(store, schemas)
        }
        .map_err(|e| e.to_string())?;
        Ok((ow, outcome))
    }

    /// Name the computation of *parsing* with a declared `form`: its [`FormIr`]
    /// wrapped as [`Comp::Parse`].
    pub fn parse_form(&self, name: &str) -> Result<Comp, String> {
        Ok(Comp::Parse(self.form_ir(name)?))
    }

    fn view_output_types(&self, name: &str) -> Result<Vec<Ty>, String> {
        let view = self
            .views
            .get(name)
            .ok_or_else(|| format!("no view `{name}`"))?;
        let mut variables: HashMap<String, Ty> = HashMap::new();
        for atom in &view.atoms {
            let relation = self
                .rels
                .get(&atom.rel)
                .ok_or_else(|| format!("view `{name}` uses undeclared relation `{}`", atom.rel))?;
            for (argument, column) in atom.args.iter().zip(&relation.columns) {
                if let Arg::Var(variable) = argument {
                    if self.entity(variable).is_none() {
                        variables.entry(variable.clone()).or_insert(column.ty);
                    }
                }
            }
        }
        view.yields
            .iter()
            .map(|yielded| {
                variables
                    .get(yielded)
                    .copied()
                    .ok_or_else(|| format!("view `{name}` yields unbound `{yielded}`"))
            })
            .collect()
    }

    fn lower_stmt_arm(&self, arm: &Arm) -> Result<BehaviorIr, String> {
        let mut locals = BTreeMap::new();
        locals.insert("self".into(), Ty::Ent);
        for variable in &arm.vars {
            bind_type(self, &mut locals, variable, Ty::Text)?;
        }
        Ok(BehaviorIr::new(self.lower_stmts(&arm.stmts, &mut locals)?))
    }

    fn lower_concat_arm(&self, arm: &ConcatArm) -> Result<BehaviorIr, String> {
        self.lower_words(
            &arm.words,
            arm.vars
                .iter()
                .cloned()
                .map(|name| (name, Ty::Text))
                .collect(),
        )
    }

    /// Type-check and lower a concatenative body to the sole executable
    /// behavior IR. `initial` names the values present on the stack from bottom
    /// to top. Stored behaviors use this entry point with generated message
    /// column names; inline arms use their form variables.
    pub fn lower_words(
        &self,
        words: &[Word],
        initial: Vec<(String, Ty)>,
    ) -> Result<BehaviorIr, String> {
        let mut locals = BTreeMap::new();
        locals.insert("self".into(), Ty::Ent);
        let mut stack = Vec::with_capacity(initial.len());
        for (name, ty) in initial {
            bind_type(self, &mut locals, &name, ty)?;
            stack.push(TypedExpr::value(ValueExpr::Local(name), ty));
        }
        let mut operations = Vec::new();
        let mut temporary = 0usize;

        for word in words {
            match word {
                Word::SelfEntity => {
                    stack.push(TypedExpr::value(ValueExpr::Local("self".into()), Ty::Ent))
                }
                Word::Lit(value) => stack.push(TypedExpr::literal(value.clone())),
                Word::Dup => stack.push(stack_top(&stack)?.clone()),
                Word::Drop => {
                    stack_pop(&mut stack)?;
                }
                Word::Swap => {
                    let b = stack_pop(&mut stack)?;
                    let a = stack_pop(&mut stack)?;
                    stack.extend([b, a]);
                }
                Word::Over => {
                    let b = stack_pop(&mut stack)?;
                    let a = stack_pop(&mut stack)?;
                    stack.extend([a.clone(), b, a]);
                }
                Word::Rot => {
                    let c = stack_pop(&mut stack)?;
                    let b = stack_pop(&mut stack)?;
                    let a = stack_pop(&mut stack)?;
                    stack.extend([b, c, a]);
                }
                Word::Nip => {
                    let b = stack_pop(&mut stack)?;
                    stack_pop(&mut stack)?;
                    stack.push(b);
                }
                Word::Tuck => {
                    let b = stack_pop(&mut stack)?;
                    let a = stack_pop(&mut stack)?;
                    stack.extend([b.clone(), a, b]);
                }
                Word::TwoDup => {
                    let b = stack_pop(&mut stack)?;
                    let a = stack_pop(&mut stack)?;
                    stack.extend([a.clone(), b.clone(), a, b]);
                }
                Word::TwoDrop => {
                    stack_pop(&mut stack)?;
                    stack_pop(&mut stack)?;
                }
                Word::Neg | Word::ToFloat | Word::Not => {
                    let value = stack_pop(&mut stack)?;
                    let lowered = match word {
                        Word::Neg if matches!(value.ty, Ty::Int | Ty::Float) => {
                            let ty = value.ty;
                            TypedExpr::value(
                                ValueExpr::Intrinsic {
                                    name: "neg".into(),
                                    arguments: vec![value.into_value("neg")?],
                                },
                                ty,
                            )
                        }
                        Word::ToFloat if value.ty == Ty::Int => TypedExpr::value(
                            ValueExpr::Intrinsic {
                                name: "float".into(),
                                arguments: vec![value.into_value("to_float")?],
                            },
                            Ty::Float,
                        ),
                        Word::Not if value.ty == Ty::Bool => {
                            TypedExpr::boolean(BoolExpr::Not(Box::new(value.into_bool("not")?)))
                        }
                        Word::Neg => return Err("word `neg` requires Int or Float".into()),
                        Word::ToFloat => return Err("word `to_float` requires Int".into()),
                        Word::Not => return Err("word `not` requires Bool".into()),
                        _ => unreachable!(),
                    };
                    stack.push(lowered);
                }
                Word::Add
                | Word::Sub
                | Word::Mul
                | Word::Div
                | Word::Rem
                | Word::Min
                | Word::Max
                | Word::Eq
                | Word::Ne
                | Word::Lt
                | Word::Le
                | Word::Gt
                | Word::Ge
                | Word::And
                | Word::Or => {
                    let right = stack_pop(&mut stack)?;
                    let left = stack_pop(&mut stack)?;
                    let lowered = match word {
                        Word::Add => self.lower_binary(BinaryOp::Add, left, right)?,
                        Word::Sub => self.lower_binary(BinaryOp::Sub, left, right)?,
                        Word::Mul => self.lower_binary(BinaryOp::Mul, left, right)?,
                        Word::Div => self.lower_binary(BinaryOp::Div, left, right)?,
                        Word::Rem => self.lower_binary(BinaryOp::Rem, left, right)?,
                        Word::Eq => self.lower_binary(BinaryOp::Eq, left, right)?,
                        Word::Ne => self.lower_binary(BinaryOp::Ne, left, right)?,
                        Word::Lt => self.lower_binary(BinaryOp::Lt, left, right)?,
                        Word::Le => self.lower_binary(BinaryOp::Le, left, right)?,
                        Word::Gt => self.lower_binary(BinaryOp::Gt, left, right)?,
                        Word::Ge => self.lower_binary(BinaryOp::Ge, left, right)?,
                        Word::And | Word::Or => {
                            let left = left.into_bool("boolean word")?;
                            let right = right.into_bool("boolean word")?;
                            TypedExpr::boolean(if matches!(word, Word::And) {
                                BoolExpr::And(Box::new(left), Box::new(right))
                            } else {
                                BoolExpr::Or(Box::new(left), Box::new(right))
                            })
                        }
                        Word::Min | Word::Max => {
                            if left.ty != right.ty || !matches!(left.ty, Ty::Int | Ty::Float) {
                                return Err(
                                    "words `min` and `max` require matching Int or Float operands"
                                        .into(),
                                );
                            }
                            let ty = left.ty;
                            TypedExpr::value(
                                ValueExpr::Intrinsic {
                                    name: if matches!(word, Word::Min) {
                                        "min".into()
                                    } else {
                                        "max".into()
                                    },
                                    arguments: vec![
                                        left.into_value("min/max")?,
                                        right.into_value("min/max")?,
                                    ],
                                },
                                ty,
                            )
                        }
                        _ => unreachable!(),
                    };
                    stack.push(lowered);
                }
                Word::Resolve { view, col, op } => {
                    let definition = self
                        .views
                        .get(view)
                        .ok_or_else(|| format!("word `resolve` names undeclared view `{view}`"))?;
                    let key = stack_pop(&mut stack)?;
                    let arguments = stack_pop_n(&mut stack, definition.params.len())?;
                    let destinations = definition.yields.clone();
                    let types = self.view_output_types(view)?;
                    let column_index = destinations
                        .iter()
                        .position(|name| name == col)
                        .ok_or_else(|| format!("view `{view}` has no column `{col}`"))?;
                    require_assignable(types[column_index], key.ty, "resolve match key")?;
                    if *op == MatchOp::Word && key.ty != Ty::Text {
                        return Err("word-match resolve requires a Text column and key".into());
                    }
                    let mut generated = Vec::with_capacity(destinations.len());
                    for ty in types {
                        let name = fresh_ir_local(&mut temporary);
                        bind_type(self, &mut locals, &name, ty)?;
                        generated.push(name.clone());
                        stack.push(TypedExpr::value(ValueExpr::Local(name), ty));
                    }
                    operations.push(BehaviorOp::Resolve {
                        view: view.clone(),
                        arguments: arguments.into_iter().map(|value| value.ir).collect(),
                        column: col.clone(),
                        op: *op,
                        rhs: key.ir,
                        destinations: generated,
                    });
                }
                Word::Find { rel, keyn } => {
                    let schema = self
                        .schema(rel)
                        .ok_or_else(|| format!("word `find` names undeclared relation `{rel}`"))?;
                    if *keyn > schema.arity() {
                        return Err(format!(
                            "word `find {rel} {keyn}` exceeds relation arity {}",
                            schema.arity()
                        ));
                    }
                    let keys = stack_pop_n(&mut stack, *keyn)?;
                    let columns = schema.columns.clone();
                    let mut arguments = Vec::with_capacity(columns.len());
                    for (index, column) in columns.iter().enumerate() {
                        let local = fresh_ir_local(&mut temporary);
                        bind_type(self, &mut locals, &local, column.ty)?;
                        if let Some(key) = keys.get(index) {
                            require_assignable(column.ty, key.ty, &format!("find `{rel}` key"))?;
                            arguments.push(FindArg::MatchBind {
                                value: key.ir.clone(),
                                local: local.clone(),
                            });
                        } else {
                            arguments.push(FindArg::Bind(local.clone()));
                        }
                        stack.push(TypedExpr::value(ValueExpr::Local(local), column.ty));
                    }
                    operations.push(BehaviorOp::Find {
                        relation: rel.clone(),
                        arguments,
                    });
                }
                Word::Expect(rel) | Word::Assert(rel) | Word::Retract(rel) | Word::Emit(rel) => {
                    let schema = self
                        .schema(rel)
                        .ok_or_else(|| format!("word names undeclared relation `{rel}`"))?;
                    let values = stack_pop_n(&mut stack, schema.arity())?;
                    for (value, column) in values.iter().zip(&schema.columns) {
                        require_assignable(column.ty, value.ty, &format!("operation on `{rel}`"))?;
                    }
                    let arguments = values.into_iter().map(|value| value.ir).collect();
                    operations.push(match word {
                        Word::Expect(_) => BehaviorOp::Expect {
                            relation: rel.clone(),
                            arguments,
                        },
                        Word::Assert(_) => BehaviorOp::Assert {
                            relation: rel.clone(),
                            arguments,
                        },
                        Word::Retract(_) => BehaviorOp::Retract {
                            relation: rel.clone(),
                            arguments,
                        },
                        Word::Emit(_) => BehaviorOp::Emit {
                            relation: rel.clone(),
                            arguments,
                        },
                        _ => unreachable!(),
                    });
                }
            }
        }
        if !stack.is_empty() {
            return Err(format!(
                "concatenative body leaves {} value(s) on the stack",
                stack.len()
            ));
        }
        Ok(BehaviorIr::new(operations))
    }

    fn lower_stmts(
        &self,
        statements: &[Stmt],
        locals: &mut BTreeMap<String, Ty>,
    ) -> Result<Vec<BehaviorOp>, String> {
        let mut operations = Vec::new();
        for statement in statements {
            match statement {
                Stmt::Let { name, value } => {
                    let value = self.lower_expr(value, locals)?;
                    bind_type(self, locals, name, value.ty)?;
                    operations.push(BehaviorOp::Let {
                        local: name.clone(),
                        value: value.ir,
                    });
                }
                Stmt::If {
                    condition,
                    then_stmts,
                    else_stmts,
                } => {
                    let condition = self.lower_expr(condition, locals)?;
                    let condition = condition.into_bool("if condition")?;
                    let mut then_locals = locals.clone();
                    let mut else_locals = locals.clone();
                    operations.push(BehaviorOp::If {
                        condition,
                        then_ops: self.lower_stmts(then_stmts, &mut then_locals)?,
                        else_ops: self.lower_stmts(else_stmts, &mut else_locals)?,
                    });
                }
                Stmt::Fresh { capability, local } => {
                    let Some((kind, _)) = self.capability(capability) else {
                        return Err(format!("fresh names undeclared capability `{capability}`"));
                    };
                    if kind != CapabilityKind::Allocate {
                        return Err(format!("capability `{capability}` is not an allocator"));
                    }
                    bind_type(self, locals, local, Ty::Ent)?;
                    operations.push(BehaviorOp::InvokeCapability {
                        capability: capability.clone(),
                        arguments: vec![],
                        destinations: vec![local.clone()],
                    });
                }
                Stmt::Random {
                    capability,
                    bound,
                    local,
                } => {
                    let Some((kind, _)) = self.capability(capability) else {
                        return Err(format!("random names undeclared capability `{capability}`"));
                    };
                    if kind != CapabilityKind::Random {
                        return Err(format!("capability `{capability}` is not a random stream"));
                    }
                    let bound = self.lower_expr(bound, locals)?;
                    require_assignable(Ty::Int, bound.ty, "random bound")?;
                    bind_type(self, locals, local, Ty::Int)?;
                    operations.push(BehaviorOp::InvokeCapability {
                        capability: capability.clone(),
                        arguments: vec![bound.ir],
                        destinations: vec![local.clone()],
                    });
                }
                Stmt::Schedule {
                    capability,
                    due,
                    tag,
                    arguments,
                    target,
                } => {
                    let Some((kind, _)) = self.capability(capability) else {
                        return Err(format!(
                            "schedule names undeclared capability `{capability}`"
                        ));
                    };
                    if kind != CapabilityKind::Schedule {
                        return Err(format!("capability `{capability}` is not a scheduler"));
                    }
                    let due = self.lower_expr(due, locals)?;
                    require_assignable(Ty::Int, due.ty, "schedule due")?;
                    let body = self.render_actor_message(target, tag, arguments, locals)?;
                    let mut invocation = vec![
                        ExprIr::Value(ValueExpr::Literal(Value::text(target))),
                        due.ir,
                    ];
                    invocation.extend(body);
                    operations.push(BehaviorOp::InvokeCapability {
                        capability: capability.clone(),
                        arguments: invocation,
                        destinations: vec![],
                    });
                }
                Stmt::Resolve {
                    view,
                    args,
                    col,
                    op,
                    rhs,
                } => {
                    let arguments = args
                        .iter()
                        .map(|argument| self.lower_sarg(argument, locals).map(|typed| typed.ir))
                        .collect::<Result<Vec<_>, _>>()?;
                    let rhs = self.lower_sarg(rhs, locals)?.ir;
                    let destinations = self
                        .views
                        .get(view)
                        .ok_or_else(|| format!("no view `{view}`"))?
                        .yields
                        .clone();
                    let types = self.view_output_types(view)?;
                    for (destination, ty) in destinations.iter().zip(types) {
                        bind_type(self, locals, destination, ty)?;
                    }
                    operations.push(BehaviorOp::Resolve {
                        view: view.clone(),
                        arguments,
                        column: col.clone(),
                        op: *op,
                        rhs,
                        destinations,
                    });
                }
                Stmt::Find { rel, args } => {
                    let schema = self
                        .schema(rel)
                        .ok_or_else(|| format!("find names undeclared relation `{rel}`"))?;
                    if args.len() != schema.arity() {
                        return Err(format!(
                            "find `{rel}` expects {} arguments, got {}",
                            schema.arity(),
                            args.len()
                        ));
                    }
                    let mut arguments = Vec::new();
                    for (argument, column) in args.iter().zip(&schema.columns) {
                        if let SArg::Var(name) = argument {
                            if !locals.contains_key(name) && self.entity(name).is_none() {
                                bind_type(self, locals, name, column.ty)?;
                                arguments.push(FindArg::Bind(name.clone()));
                                continue;
                            }
                        }
                        let typed = self.lower_sarg(argument, locals)?;
                        require_assignable(column.ty, typed.ty, &format!("find `{rel}`"))?;
                        arguments.push(FindArg::Match(typed.ir));
                    }
                    operations.push(BehaviorOp::Find {
                        relation: rel.clone(),
                        arguments,
                    });
                }
                Stmt::Expect { rel, args }
                | Stmt::Assert { rel, args }
                | Stmt::Retract { rel, args }
                | Stmt::Emit { rel, args } => {
                    let arguments = self.lower_relation_args(rel, args, locals)?;
                    let operation = match statement {
                        Stmt::Expect { .. } => BehaviorOp::Expect {
                            relation: rel.clone(),
                            arguments,
                        },
                        Stmt::Assert { .. } => BehaviorOp::Assert {
                            relation: rel.clone(),
                            arguments,
                        },
                        Stmt::Retract { .. } => BehaviorOp::Retract {
                            relation: rel.clone(),
                            arguments,
                        },
                        Stmt::Emit { .. } => BehaviorOp::Emit {
                            relation: rel.clone(),
                            arguments,
                        },
                        _ => unreachable!(),
                    };
                    operations.push(operation);
                }
            }
        }
        Ok(operations)
    }

    fn render_actor_message(
        &self,
        target: &str,
        tag: &str,
        arguments: &[Expr],
        locals: &BTreeMap<String, Ty>,
    ) -> Result<Vec<ExprIr>, String> {
        let actor = self
            .actors
            .get(target)
            .ok_or_else(|| format!("schedule targets undeclared actor `{target}`"))?;
        let handler = self
            .ons
            .get(&actor.inbox)
            .ok_or_else(|| format!("actor `{target}` inbox has no on-handler"))?;
        let rules = self
            .forms
            .get(&handler.form)
            .ok_or_else(|| format!("actor `{target}` handler names no form"))?;

        let mut rendered = Vec::new();
        for rule in rules
            .iter()
            .filter(|rule| rule.tag == tag && rule.ctor_args.len() == arguments.len())
        {
            let unique: BTreeSet<_> = rule.ctor_args.iter().collect();
            if unique.len() != rule.ctor_args.len() {
                continue;
            }
            let mut values = BTreeMap::new();
            let mut valid = true;
            for (name, expression) in rule.ctor_args.iter().zip(arguments) {
                let typed = self.lower_expr(expression, locals)?;
                require_assignable(Ty::Text, typed.ty, "scheduled constructor argument")?;
                values.insert(name.as_str(), typed.ir);
            }
            let mut body = Vec::new();
            for atom in &rule.seq {
                match atom {
                    PAtom::Lit(value) => {
                        body.push(ExprIr::Value(ValueExpr::Literal(Value::text(value))))
                    }
                    PAtom::Bind(name) => match values.get(name.as_str()) {
                        Some(value) => body.push(value.clone()),
                        None => {
                            valid = false;
                            break;
                        }
                    },
                }
            }
            if valid {
                rendered.push(body);
            }
        }
        match rendered.len() {
            1 => Ok(rendered.pop().unwrap()),
            0 => Err(format!(
                "actor `{target}` form has no invertible `{tag}` constructor with {} Text argument(s)",
                arguments.len()
            )),
            _ => Err(format!(
                "actor `{target}` form has ambiguous `{tag}` constructors"
            )),
        }
    }

    fn lower_relation_args(
        &self,
        relation: &str,
        arguments: &[SArg],
        locals: &BTreeMap<String, Ty>,
    ) -> Result<Vec<ExprIr>, String> {
        let schema = self
            .schema(relation)
            .ok_or_else(|| format!("operation names undeclared relation `{relation}`"))?;
        if arguments.len() != schema.arity() {
            return Err(format!(
                "operation on `{relation}` expects {} arguments, got {}",
                schema.arity(),
                arguments.len()
            ));
        }
        arguments
            .iter()
            .zip(&schema.columns)
            .map(|(argument, column)| {
                let typed = self.lower_sarg(argument, locals)?;
                require_assignable(column.ty, typed.ty, &format!("operation on `{relation}`"))?;
                Ok(typed.ir)
            })
            .collect()
    }

    fn lower_sarg(
        &self,
        argument: &SArg,
        locals: &BTreeMap<String, Ty>,
    ) -> Result<TypedExpr, String> {
        match argument {
            SArg::Var(name) => {
                if let Some(entity) = self.entity(name) {
                    return Ok(TypedExpr::value(
                        ValueExpr::Literal(Value::Ent(entity)),
                        Ty::Ent,
                    ));
                }
                let ty = locals
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("unbound local `{name}`"))?;
                Ok(TypedExpr::value(ValueExpr::Local(name.clone()), ty))
            }
            SArg::Str(value) => Ok(TypedExpr::literal(Value::text(value))),
            SArg::Int(value) => Ok(TypedExpr::literal(Value::Int(*value))),
            SArg::Float(value) => Ok(TypedExpr::literal(Value::Float(*value))),
            SArg::Bool(value) => Ok(TypedExpr::literal(Value::Bool(*value))),
        }
    }

    fn lower_expr(
        &self,
        expression: &Expr,
        locals: &BTreeMap<String, Ty>,
    ) -> Result<TypedExpr, String> {
        match expression {
            Expr::Var(name) => {
                if let Some(entity) = self.entity(name) {
                    return Ok(TypedExpr::literal(Value::Ent(entity)));
                }
                let ty = locals
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("unbound local `{name}`"))?;
                Ok(TypedExpr::value(ValueExpr::Local(name.clone()), ty))
            }
            Expr::Lit(value) => Ok(TypedExpr::literal(value.clone())),
            Expr::Unary { op, value } => {
                let value = self.lower_expr(value, locals)?;
                match op {
                    UnaryOp::Neg if matches!(value.ty, Ty::Int | Ty::Float) => {
                        let ty = value.ty;
                        Ok(TypedExpr::value(
                            ValueExpr::Intrinsic {
                                name: "neg".into(),
                                arguments: vec![value.into_value("unary -")?],
                            },
                            ty,
                        ))
                    }
                    UnaryOp::Not if value.ty == Ty::Bool => Ok(TypedExpr::boolean(BoolExpr::Not(
                        Box::new(value.into_bool("unary !")?),
                    ))),
                    UnaryOp::Neg => Err("unary `-` requires Int or Float".into()),
                    UnaryOp::Not => Err("unary `!` requires Bool".into()),
                }
            }
            Expr::Binary { op, left, right } => {
                let left = self.lower_expr(left, locals)?;
                match op {
                    BinaryOp::And | BinaryOp::Or => {
                        let left = left.into_bool("boolean operator")?;
                        let right = self
                            .lower_expr(right, locals)?
                            .into_bool("boolean operator")?;
                        Ok(TypedExpr::boolean(match op {
                            BinaryOp::And => BoolExpr::And(Box::new(left), Box::new(right)),
                            BinaryOp::Or => BoolExpr::Or(Box::new(left), Box::new(right)),
                            _ => unreachable!(),
                        }))
                    }
                    _ => {
                        let right = self.lower_expr(right, locals)?;
                        self.lower_binary(*op, left, right)
                    }
                }
            }
            Expr::Call { name, args } => {
                let args = args
                    .iter()
                    .map(|argument| self.lower_expr(argument, locals))
                    .collect::<Result<Vec<_>, _>>()?;
                match (name.as_str(), args.as_slice()) {
                    ("float", [value]) if value.ty == Ty::Int => Ok(TypedExpr::value(
                        ValueExpr::Intrinsic {
                            name: "float".into(),
                            arguments: vec![value.clone().into_value("float")?],
                        },
                        Ty::Float,
                    )),
                    (intrinsic @ ("min" | "max"), [left, right])
                        if left.ty == right.ty && matches!(left.ty, Ty::Int | Ty::Float) =>
                    {
                        Ok(TypedExpr::value(
                            ValueExpr::Intrinsic {
                                name: intrinsic.into(),
                                arguments: vec![
                                    left.clone().into_value(intrinsic)?,
                                    right.clone().into_value(intrinsic)?,
                                ],
                            },
                            left.ty,
                        ))
                    }
                    _ => Err(format!("unknown or ill-typed intrinsic call `{name}`")),
                }
            }
        }
    }

    fn lower_binary(
        &self,
        operator: BinaryOp,
        left: TypedExpr,
        right: TypedExpr,
    ) -> Result<TypedExpr, String> {
        match operator {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                if left.ty != right.ty || !matches!(left.ty, Ty::Int | Ty::Float) {
                    return Err("arithmetic operands must be the same Int or Float type".into());
                }
                let ty = left.ty;
                let name = match operator {
                    BinaryOp::Add => "add",
                    BinaryOp::Sub => "sub",
                    BinaryOp::Mul => "mul",
                    BinaryOp::Div => "div",
                    BinaryOp::Rem => "rem",
                    _ => unreachable!(),
                };
                Ok(TypedExpr::value(
                    ValueExpr::Intrinsic {
                        name: name.into(),
                        arguments: vec![
                            left.into_value("arithmetic")?,
                            right.into_value("arithmetic")?,
                        ],
                    },
                    ty,
                ))
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => {
                if left.ty != right.ty || left.ty == Ty::Any {
                    return Err("comparison operands must have the same concrete type".into());
                }
                if matches!(
                    operator,
                    BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
                ) && !matches!(left.ty, Ty::Int | Ty::Float)
                {
                    return Err("ordered comparison requires Int or Float".into());
                }
                let op = match operator {
                    BinaryOp::Eq => CompareOp::Eq,
                    BinaryOp::Ne => CompareOp::Ne,
                    BinaryOp::Lt => CompareOp::Lt,
                    BinaryOp::Le => CompareOp::Le,
                    BinaryOp::Gt => CompareOp::Gt,
                    BinaryOp::Ge => CompareOp::Ge,
                    _ => unreachable!(),
                };
                Ok(TypedExpr::boolean(BoolExpr::Compare {
                    op,
                    left: left.into_value("comparison")?,
                    right: right.into_value("comparison")?,
                }))
            }
            BinaryOp::And | BinaryOp::Or => unreachable!("handled for short-circuit lowering"),
        }
    }

    /// Compile an `on` handler into a runnable [`Behavior`], bound to the process
    /// entity `self_entity`. The behavior parses each incoming message with the
    /// handler's `form`, dispatches to the matching arm, runs its statements
    /// (resolving nouns via views, looking up base facts, building the patch),
    /// and returns the resulting `Patch`. A failed `resolve`/`find` yields an
    /// empty patch (no effect).
    pub fn behavior(
        prog: &Arc<Program>,
        inbox: &str,
        self_entity: Entity,
    ) -> Result<Behavior, String> {
        Self::behavior_internal(prog, inbox, self_entity, None)
    }

    pub fn behavior_with_grants(
        prog: &Arc<Program>,
        inbox: &str,
        self_entity: Entity,
        grants: Arc<ResolvedGrantSet>,
    ) -> Result<Behavior, String> {
        Self::behavior_internal(prog, inbox, self_entity, Some(grants))
    }

    fn behavior_internal(
        prog: &Arc<Program>,
        inbox: &str,
        self_entity: Entity,
        grants: Option<Arc<ResolvedGrantSet>>,
    ) -> Result<Behavior, String> {
        let on = prog
            .ons
            .get(inbox)
            .ok_or_else(|| format!("no on-handler for `{inbox}`"))?;
        let form = prog.form(&on.form)?;
        let arms = on
            .arms
            .iter()
            .map(|arm| {
                Ok(CompiledArm {
                    tag: arm.tag.clone(),
                    vars: arm.vars.clone(),
                    ir: prog.lower_stmt_arm(arm)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut arms = arms;
        arms.extend(
            on.concat_arms
                .iter()
                .map(|arm| {
                    Ok(CompiledArm {
                        tag: arm.tag.clone(),
                        vars: arm.vars.clone(),
                        ir: prog.lower_concat_arm(arm)?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        );
        let prog = Arc::clone(prog);
        Ok(Box::new(
            move |snap: &Snapshot, body: &Tuple| -> CoreResult<Patch> {
                run_behavior(
                    &prog,
                    grants.as_deref(),
                    &form,
                    &arms,
                    self_entity,
                    snap,
                    body,
                )
            },
        ))
    }

    /// Lower every surface arm for `inbox` to the sole executable IR. This is
    /// used by effect inference so authority analysis cannot diverge from the
    /// plan that execution runs.
    pub fn handler_irs(&self, inbox: &str) -> Result<Vec<BehaviorIr>, String> {
        let on = self
            .ons
            .get(inbox)
            .ok_or_else(|| format!("no on-handler for `{inbox}`"))?;
        on.arms
            .iter()
            .map(|arm| self.lower_stmt_arm(arm))
            .chain(on.concat_arms.iter().map(|arm| self.lower_concat_arm(arm)))
            .collect()
    }

    pub(crate) fn validate_behaviors(&self) -> Result<(), String> {
        let mut inboxes: Vec<_> = self.ons.keys().cloned().collect();
        inboxes.sort();
        for inbox in inboxes {
            self.handler_irs(&inbox)?;
        }
        Ok(())
    }

    pub(crate) fn schedule_targets(&self, capability: &str) -> Result<BTreeSet<String>, String> {
        fn collect(operations: &[BehaviorOp], capability: &str, targets: &mut BTreeSet<String>) {
            for operation in operations {
                match operation {
                    BehaviorOp::InvokeCapability {
                        capability: invoked,
                        arguments,
                        ..
                    } if invoked == capability => {
                        if let Some(ExprIr::Value(ValueExpr::Literal(Value::Text(target)))) =
                            arguments.first()
                        {
                            targets.insert(target.to_string());
                        }
                    }
                    BehaviorOp::If {
                        then_ops, else_ops, ..
                    } => {
                        collect(then_ops, capability, targets);
                        collect(else_ops, capability, targets);
                    }
                    _ => {}
                }
            }
        }

        let mut targets = BTreeSet::new();
        for inbox in self.ons.keys() {
            for behavior in self.handler_irs(inbox)? {
                collect(&behavior.operations, capability, &mut targets);
            }
        }
        Ok(targets)
    }
}

#[derive(Clone)]
struct CompiledArm {
    tag: String,
    vars: Vec<String>,
    ir: BehaviorIr,
}

#[derive(Clone)]
struct TypedExpr {
    ir: ExprIr,
    ty: Ty,
}

impl TypedExpr {
    fn value(value: ValueExpr, ty: Ty) -> Self {
        Self {
            ir: ExprIr::Value(value),
            ty,
        }
    }

    fn boolean(value: BoolExpr) -> Self {
        Self {
            ir: ExprIr::Bool(value),
            ty: Ty::Bool,
        }
    }

    fn literal(value: Value) -> Self {
        let ty = value_type(&value);
        Self::value(ValueExpr::Literal(value), ty)
    }

    fn into_value(self, context: &str) -> Result<ValueExpr, String> {
        match self.ir {
            ExprIr::Value(value) => Ok(value),
            ExprIr::Bool(_) => Err(format!("{context} requires a scalar value")),
        }
    }

    fn into_bool(self, context: &str) -> Result<BoolExpr, String> {
        if self.ty != Ty::Bool {
            return Err(format!("{context} requires Bool, found {}", self.ty.name()));
        }
        Ok(match self.ir {
            ExprIr::Bool(value) => value,
            ExprIr::Value(value) => BoolExpr::Value(value),
        })
    }
}

fn value_type(value: &Value) -> Ty {
    match value {
        Value::Ent(_) => Ty::Ent,
        Value::Int(_) => Ty::Int,
        Value::Float(_) => Ty::Float,
        Value::Text(_) => Ty::Text,
        Value::Bool(_) => Ty::Bool,
        Value::Tuple(_) => Ty::Tuple,
        Value::Bytes(_) => Ty::Bytes,
        Value::Code(_) => Ty::Code,
    }
}

fn stack_top(stack: &[TypedExpr]) -> Result<&TypedExpr, String> {
    stack
        .last()
        .ok_or_else(|| "concatenative stack underflow".into())
}

fn stack_pop(stack: &mut Vec<TypedExpr>) -> Result<TypedExpr, String> {
    stack
        .pop()
        .ok_or_else(|| "concatenative stack underflow".into())
}

fn stack_pop_n(stack: &mut Vec<TypedExpr>, count: usize) -> Result<Vec<TypedExpr>, String> {
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(stack_pop(stack)?);
    }
    values.reverse();
    Ok(values)
}

fn fresh_ir_local(next: &mut usize) -> String {
    let local = format!("#stack{}", *next);
    *next += 1;
    local
}

fn bind_type(
    program: &Program,
    locals: &mut BTreeMap<String, Ty>,
    name: &str,
    ty: Ty,
) -> Result<(), String> {
    if program.entity(name).is_some() {
        return Err(format!("local `{name}` shadows an entity constant"));
    }
    if locals.insert(name.to_owned(), ty).is_some() {
        return Err(format!(
            "local `{name}` is bound more than once in one scope"
        ));
    }
    Ok(())
}

fn require_assignable(expected: Ty, actual: Ty, context: &str) -> Result<(), String> {
    if expected == Ty::Any || expected == actual {
        Ok(())
    } else {
        Err(format!(
            "{context} expects {}, found {}",
            expected.name(),
            actual.name()
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn run_behavior(
    prog: &Program,
    grants: Option<&ResolvedGrantSet>,
    form: &Form,
    arms: &[CompiledArm],
    self_entity: Entity,
    snap: &Snapshot,
    body: &Tuple,
) -> CoreResult<Patch> {
    let cmds = form.parse_all(body.as_slice());
    let cmd = match cmds.into_iter().next() {
        Some(c) => c,
        None => return Ok(Patch::new()),
    };
    let parts = match cmd {
        Value::Tuple(p) => p,
        _ => return Ok(Patch::new()),
    };
    let tag = match parts.first() {
        Some(Value::Text(t)) => t.to_string(),
        _ => return Ok(Patch::new()),
    };

    for arm in arms {
        if arm.tag == tag && parts.len() == arm.vars.len() + 1 {
            let initial = arm
                .vars
                .iter()
                .enumerate()
                .map(|(index, name)| (name.clone(), parts[index + 1].clone()));
            let patch = match grants {
                Some(grants) => {
                    arm.ir
                        .execute_with_grants(prog, grants, self_entity, snap, initial)?
                }
                None => arm.ir.execute(prog, self_entity, snap, initial)?,
            };
            return Ok(patch.unwrap_or_default());
        }
    }
    Ok(Patch::new())
}

impl Schemas for Program {
    fn view_shape(&self, view: &str) -> Option<(usize, usize)> {
        self.views.get(view).map(|v| {
            // An aggregate yield contributes one extra output column (the fold),
            // so the reduced view's arity is the group cols plus one.
            let out = v.yields.len() + usize::from(v.agg.is_some());
            (v.params.len(), out)
        })
    }

    fn rel_arity(&self, rel: &str) -> Option<usize> {
        self.rels.get(rel).map(|r| r.arity())
    }
}
