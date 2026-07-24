//! Lowering the AST to core constructors.
//!
//! * `rel`  → a relation schema (name → `RelId` + columns).
//! * `view` → a `Query`: the conjunctive body is planned left-to-right, joining
//!   each atom to the accumulated relation on their shared variables, then
//!   projecting the `yield` variables and de-duplicating. Literals and repeated
//!   variables within an atom become filters; view parameters pin a column to a
//!   supplied value at instantiation. This is the concrete realization of
//!   "`lamp.location` compiles to a relational query" (DESIGN.md §3).
//! * `form` → a `grmpl_pattern::Form`.

use std::collections::HashMap;
use std::sync::Arc;

use grmpl_core::{Entity, Error, Fact, Message, Patch, Result as CoreResult, RelId, Tuple, Value};
use grmpl_diff::{Query, Snapshot};
use grmpl_pattern::{Bindings, Form, Pattern, Rule, VarId};
use grmpl_proc::Behavior;

use crate::ast::{Arg, Arm, Decl, MatchOp, PAtom, SArg, Stmt};
use crate::parser::parse;

struct RelInfo {
    id: RelId,
    arity: usize,
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
                    rels.insert(name, RelInfo { id: RelId(next_id), arity: cols.len() });
                    next_id += 1;
                }
                Decl::View { name, params, atoms, yields } => {
                    views.insert(name, ViewDef { params, atoms, yields });
                }
                Decl::Form { name, rules } => {
                    forms.insert(name, rules);
                }
                Decl::On { inbox, form, arms } => {
                    ons.insert(inbox, OnDef { form, arms });
                }
            }
        }
        Ok(Program { rels, views, forms, ons })
    }

    /// The yielded column names of a view (in order).
    pub fn view_yields(&self, name: &str) -> Option<&[String]> {
        self.views.get(name).map(|v| v.yields.as_slice())
    }

    /// The `RelId` assigned to a declared relation.
    pub fn rel_id(&self, name: &str) -> Option<RelId> {
        self.rels.get(name).map(|r| r.id)
    }

    /// Instantiate a view as a `Query`, binding its parameters to `args`.
    pub fn view(&self, name: &str, args: &[Value]) -> Result<Query, String> {
        let v = self.views.get(name).ok_or_else(|| format!("no view `{name}`"))?;
        if args.len() != v.params.len() {
            return Err(format!(
                "view `{name}` takes {} argument(s), got {}",
                v.params.len(),
                args.len()
            ));
        }
        let params: HashMap<&str, Value> =
            v.params.iter().map(|s| s.as_str()).zip(args.iter().cloned()).collect();

        let mut varcol: HashMap<String, usize> = HashMap::new();
        let mut acc: Option<Query> = None;
        let mut width = 0usize;

        for atom in &v.atoms {
            let info = self
                .rels
                .get(&atom.rel)
                .ok_or_else(|| format!("view `{name}` uses undeclared relation `{}`", atom.rel))?;
            if atom.args.len() != info.arity {
                return Err(format!(
                    "`{}` has arity {} but was used with {} args",
                    atom.rel,
                    info.arity,
                    atom.args.len()
                ));
            }

            let mut q = Query::rel(info.id);
            let mut local_first: Vec<(String, usize)> = Vec::new();
            let mut lit_filters: Vec<(usize, Value)> = Vec::new();
            let mut eqs: Vec<(usize, usize)> = Vec::new();

            for (i, arg) in atom.args.iter().enumerate() {
                match arg {
                    Arg::Str(s) => lit_filters.push((i, Value::text(s))),
                    Arg::Int(n) => lit_filters.push((i, Value::Int(*n))),
                    Arg::Var(vn) => {
                        if let Some(val) = params.get(vn.as_str()) {
                            lit_filters.push((i, val.clone()));
                        }
                        match local_first.iter().find(|(n, _)| n == vn) {
                            Some((_, first)) => eqs.push((*first, i)),
                            None => local_first.push((vn.clone(), i)),
                        }
                    }
                }
            }

            for (col, val) in lit_filters {
                q = q.filter(move |t| t.as_slice()[col] == val);
            }
            for (a, b) in eqs {
                q = q.filter(move |t| t.as_slice()[a] == t.as_slice()[b]);
            }

            match acc.take() {
                None => {
                    for (vn, pos) in &local_first {
                        varcol.insert(vn.clone(), *pos);
                    }
                    acc = Some(q);
                    width = info.arity;
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
                    acc = Some(prev.join(q, lk, rk));
                    for (vn, apos) in &local_first {
                        varcol.entry(vn.clone()).or_insert(width + *apos);
                    }
                    width += info.arity;
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
        Ok(base.project(cols).distinct())
    }

    /// Build a parser from a declared `form`.
    pub fn form(&self, name: &str) -> Result<Form, String> {
        let rules_ast = self.forms.get(name).ok_or_else(|| format!("no form `{name}`"))?;
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
            let tag = r.tag.clone();
            let arg_ids: Vec<u32> = r
                .ctor_args
                .iter()
                .map(|a| ids.get(a).copied().ok_or_else(|| format!("ctor arg `{a}` is not bound in the pattern")))
                .collect::<Result<_, _>>()?;

            let ctor = move |b: &Bindings| {
                let mut vals = vec![Value::text(&tag)];
                for id in &arg_ids {
                    vals.push(b.get(&VarId(*id)).cloned().unwrap_or(Value::text("")));
                }
                Value::Tuple(Arc::from(vals))
            };
            rules.push(Rule::new(Pattern::Seq(pats), ctor));
        }
        Ok(Form::new(rules))
    }

    /// Compile an `on` handler into a runnable [`Behavior`], bound to the process
    /// entity `self_entity`. The behavior parses each incoming message with the
    /// handler's `form`, dispatches to the matching arm, runs its statements
    /// (resolving nouns via views, looking up base facts, building the patch),
    /// and returns the resulting `Patch`. A failed `resolve`/`find` yields an
    /// empty patch (no effect).
    pub fn behavior(prog: &Arc<Program>, inbox: &str, self_entity: Entity) -> Result<Behavior, String> {
        let on = prog.ons.get(inbox).ok_or_else(|| format!("no on-handler for `{inbox}`"))?;
        let form = prog.form(&on.form)?;
        let arms = on.arms.clone();
        let prog = Arc::clone(prog);
        Ok(Box::new(move |snap: &Snapshot, body: &Tuple| -> CoreResult<Patch> {
            run_behavior(&prog, &form, &arms, self_entity, snap, body)
        }))
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
            Stmt::Resolve { view, args, col, op, rhs } => {
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
                let rid = prog.rel_id(rel).ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
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
                let rid = prog.rel_id(rel).ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
                patch = patch.emit(Message { inbox: rid, body: tuple(args, env)? });
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
    let rid = prog.rel_id(rel).ok_or_else(|| rt_err(format!("no relation `{rel}`")))?;
    Ok(Fact::new(rid, tuple(args, env)?))
}
