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

use std::collections::HashMap;
use std::sync::Arc;

use grmpl_core::{
    Catalog, Column, Edition, Entity, Error, Fact, Message, Patch, RelId, Result as CoreResult,
    Schema, SchemaCatalog, Tuple, Ty, Value,
};
use grmpl_diff::{Agg, Query, Snapshot};
use grmpl_pattern::{Form, Pattern, VarId};
use grmpl_proc::Behavior;

use crate::ast::{Arg, Arm, Decl, MatchOp, PAtom, SArg, Stmt};
use crate::ir::{Comp, CtorSpec, FormIr, PredExpr, QueryIr, RowExpr, RuleIr};
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
}

struct OnDef {
    form: String,
    arms: Vec<Arm>,
}

/// A compiled program: relation schemas, views, forms, and on-handlers.
pub struct Program {
    rels: HashMap<String, RelInfo>,
    views: HashMap<String, ViewDef>,
    forms: HashMap<String, Vec<crate::ast::FormRule>>,
    ons: HashMap<String, OnDef>,
}

impl Program {
    /// Parse and compile source text. `rel`s are assigned `RelId`s sequentially
    /// starting at `rel_base`, so callers can align a store's relations with it.
    pub fn compile(src: &str, rel_base: u32) -> Result<Program, String> {
        let decls = parse(src)?;
        let mut rels = HashMap::new();
        let mut views = HashMap::new();
        let mut forms = HashMap::new();
        let mut ons = HashMap::new();
        let mut next_id = rel_base;

        for decl in decls {
            match decl {
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
                                     (expected Ent, Int, Text, Bool, Tuple, or Any)",
                                    c.name
                                )
                            })?,
                        };
                        columns.push(Column::new(c.name.clone(), ty));
                    }
                    rels.insert(
                        name,
                        RelInfo {
                            id: RelId(next_id),
                            columns,
                        },
                    );
                    next_id += 1;
                }
                Decl::View {
                    name,
                    params,
                    atoms,
                    yields,
                } => {
                    views.insert(
                        name,
                        ViewDef {
                            params,
                            atoms,
                            yields,
                        },
                    );
                }
                Decl::Form { name, rules } => {
                    forms.insert(name, rules);
                }
                Decl::On { inbox, form, arms } => {
                    ons.insert(inbox, OnDef { form, arms });
                }
            }
        }
        Ok(Program {
            rels,
            views,
            forms,
            ons,
        })
    }

    /// The yielded column names of a view (in order).
    pub fn view_yields(&self, name: &str) -> Option<&[String]> {
        self.views.get(name).map(|v| v.yields.as_slice())
    }

    /// The arms of the `on` handler bound to `inbox`, or `None` if none is
    /// declared. Exposed so P8b effect-row inference can walk a handler's write
    /// statements (`assert`/`retract`/`emit`) without re-parsing the source.
    pub fn on_arms(&self, inbox: &str) -> Option<&[Arm]> {
        self.ons.get(inbox).map(|o| o.arms.as_slice())
    }

    /// The `RelId` assigned to a declared relation.
    pub fn rel_id(&self, name: &str) -> Option<RelId> {
        self.rels.get(name).map(|r| r.id)
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
    /// Id allocation still uses the compile-time `rel_base`; *consuming* the
    /// catalog to recover stable ids across reopens is a follow-on (TKT-90).
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
                    Arg::Var(vn) => {
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
        let mut cols = Vec::new();
        for y in &v.yields {
            let c = *varcol
                .get(y)
                .ok_or_else(|| format!("view `{name}` yields unbound variable `{y}`"))?;
            cols.push(c);
        }
        Ok(QueryIr::Distinct(Box::new(QueryIr::Project {
            input: Box::new(base),
            cols,
        })))
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

    /// Name the computation of *parsing* with a declared `form`: its [`FormIr`]
    /// wrapped as [`Comp::Parse`].
    pub fn parse_form(&self, name: &str) -> Result<Comp, String> {
        Ok(Comp::Parse(self.form_ir(name)?))
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
        let on = prog
            .ons
            .get(inbox)
            .ok_or_else(|| format!("no on-handler for `{inbox}`"))?;
        let form = prog.form(&on.form)?;
        let arms = on.arms.clone();
        let prog = Arc::clone(prog);
        Ok(Box::new(
            move |snap: &Snapshot, body: &Tuple| -> CoreResult<Patch> {
                run_behavior(&prog, &form, &arms, self_entity, snap, body)
            },
        ))
    }
}

fn rt_err(s: impl Into<String>) -> Error {
    Error::Codec(s.into())
}

fn run_behavior(
    prog: &Program,
    form: &Form,
    arms: &[Arm],
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
            let mut env: HashMap<String, Value> = HashMap::new();
            env.insert("self".into(), Value::Ent(self_entity));
            for (i, v) in arm.vars.iter().enumerate() {
                env.insert(v.clone(), parts[i + 1].clone());
            }
            return Ok(exec_stmts(prog, &arm.stmts, &mut env, snap)?.unwrap_or_default());
        }
    }
    Ok(Patch::new())
}

/// Run an arm's statements, building a patch. `Ok(None)` means a `resolve`/`find`
/// found nothing — the arm makes no change.
fn exec_stmts(
    prog: &Program,
    stmts: &[Stmt],
    env: &mut HashMap<String, Value>,
    snap: &Snapshot,
) -> CoreResult<Option<Patch>> {
    let mut patch = Patch::new();
    for stmt in stmts {
        match stmt {
            Stmt::Resolve {
                view,
                args,
                col,
                op,
                rhs,
            } => {
                let argvals = sargs(args, env)?;
                let q = prog.view(view, &argvals).map_err(rt_err)?;
                let yields = prog
                    .view_yields(view)
                    .ok_or_else(|| rt_err(format!("no view `{view}`")))?
                    .to_vec();
                let ci = yields
                    .iter()
                    .position(|y| y == col)
                    .ok_or_else(|| rt_err(format!("view `{view}` has no column `{col}`")))?;
                let want = sarg(rhs, env)?;
                let rows = q.find(snap)?;
                // Deterministic choice: bind to the *least* matching tuple, not
                // whichever the scan happened to surface first.
                let picked = rows
                    .into_iter()
                    .filter(|(t, _)| col_match(*op, &t.as_slice()[ci], &want))
                    .min_by(|(a, _), (b, _)| a.cmp(b));
                match picked {
                    Some((t, _)) => {
                        for (i, y) in yields.iter().enumerate() {
                            env.insert(y.clone(), t.as_slice()[i].clone());
                        }
                    }
                    None => return Ok(None),
                }
            }
            Stmt::Find { rel, args } => {
                let rid = prog
                    .rel_id(rel)
                    .ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
                let rows = snap.read(rid)?;
                let bound: Vec<(usize, Value)> = args
                    .iter()
                    .enumerate()
                    .filter_map(|(i, a)| sarg_opt(a, env).map(|v| (i, v)))
                    .collect();
                // Deterministic choice: bind to the *least* matching tuple.
                let picked = rows
                    .into_iter()
                    .filter(|(t, _)| bound.iter().all(|(i, v)| t.as_slice().get(*i) == Some(v)))
                    .min_by(|(a, _), (b, _)| a.cmp(b));
                match picked {
                    Some((t, _)) => {
                        for (i, a) in args.iter().enumerate() {
                            if let SArg::Var(name) = a {
                                if !env.contains_key(name) {
                                    if let Some(v) = t.as_slice().get(i) {
                                        env.insert(name.clone(), v.clone());
                                    }
                                }
                            }
                        }
                    }
                    None => return Ok(None),
                }
            }
            Stmt::Expect { rel, args } => patch = patch.expect(fact(prog, rel, args, env)?),
            Stmt::Assert { rel, args } => patch = patch.assert(fact(prog, rel, args, env)?),
            Stmt::Retract { rel, args } => patch = patch.retract(fact(prog, rel, args, env)?),
            Stmt::Emit { rel, args } => {
                let rid = prog
                    .rel_id(rel)
                    .ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
                patch = patch.emit(Message {
                    inbox: rid,
                    body: tuple(args, env)?,
                });
            }
        }
    }
    Ok(Some(patch))
}

fn col_match(op: MatchOp, have: &Value, want: &Value) -> bool {
    match (op, have, want) {
        (MatchOp::Word, Value::Text(h), Value::Text(w)) => {
            h.as_ref() == w.as_ref() || h.split_whitespace().any(|word| word == w.as_ref())
        }
        _ => have == want,
    }
}

fn sarg(a: &SArg, env: &HashMap<String, Value>) -> CoreResult<Value> {
    match a {
        SArg::Var(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| rt_err(format!("unbound variable `{name}`"))),
        SArg::Str(s) => Ok(Value::text(s)),
        SArg::Int(n) => Ok(Value::Int(*n)),
    }
}

fn sarg_opt(a: &SArg, env: &HashMap<String, Value>) -> Option<Value> {
    match a {
        SArg::Var(name) => env.get(name).cloned(),
        SArg::Str(s) => Some(Value::text(s)),
        SArg::Int(n) => Some(Value::Int(*n)),
    }
}

fn sargs(args: &[SArg], env: &HashMap<String, Value>) -> CoreResult<Vec<Value>> {
    args.iter().map(|a| sarg(a, env)).collect()
}

fn tuple(args: &[SArg], env: &HashMap<String, Value>) -> CoreResult<Tuple> {
    Ok(Tuple::new(sargs(args, env)?))
}

fn fact(
    prog: &Program,
    rel: &str,
    args: &[SArg],
    env: &HashMap<String, Value>,
) -> CoreResult<Fact> {
    let rid = prog
        .rel_id(rel)
        .ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
    Ok(Fact::new(rid, tuple(args, env)?))
}
