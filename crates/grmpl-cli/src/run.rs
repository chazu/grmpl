//! `grmpl run WORLD.grmpl [STORE_DIR]` — stand up any language-defined world and
//! inspect and drive it from a generic REPL.
//!
//! The host knows nothing about the world it runs; everything it does goes
//! through the public [`Runtime`]:
//!   * a source with a `package` declaration loads as a package. Every
//!     capability it `requires` is granted exactly as declared, and a package
//!     that declares actors is loaded *driven*: each actor holds the writes its
//!     `authority` request names, and the schedule may address every actor;
//!   * any other source compiles as a plain program;
//!   * commands are plain lines — `:rels`, `:views`, `? VIEW ARG…`, `:read REL`,
//!     `:at N ? VIEW ARG…`, `:send ENTITY INBOX SEQS TEXT…`, `:watch VIEW ARG…`,
//!     `:edition` and `quit` (`help` lists them). A bad line prints an error and
//!     the REPL carries on.
//!
//! `grmpl moo` is the MOO's own, world-specific REPL.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use grmpl::{DriveStatus, NamedAuthority, NamedScope, Runtime, RuntimePolicy};
use grmpl_core::{Authority, Diff, DomainId, Edition, Entity, Scope, Tuple, Value, WorldStore};
use grmpl_diff::Snapshot;
use grmpl_lang::ast::Decl;
use grmpl_lang::{CapabilityRequirement, CompiledActor, CompiledPackage, GrantSet};
use grmpl_proc::{decode_activation, OnWatch, Process};
use grmpl_type::infer_handler_effects;

/// Relation ids start here on a fresh store, as for every bundled host.
const REL_BASE: u32 = 1;

/// The one authority domain every process, actor and watch of the world
/// commits in, as for every bundled host.
const DOMAIN: DomainId = DomainId(1);

/// The cursor-key (and addressee) entities of this REPL's watches start here:
/// far above any id a world declares, and skipped while a reused store already
/// holds a cursor for them.
const WATCH_BASE: u64 = 1 << 48;

pub fn run(world: Option<String>, store_dir: Option<String>) -> Result<(), String> {
    let path = world.ok_or("usage: grmpl run WORLD.grmpl [STORE_DIR]")?;
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read world `{path}`: {e}"))?;
    let store: Arc<dyn WorldStore> = Arc::new(crate::moo::open_store(store_dir.as_deref())?);
    let (runtime, actors, kind) = load(store, &source)?;
    let mut repl = Repl {
        runtime,
        actors,
        watches: Vec::new(),
        next_watch: WATCH_BASE,
    };
    repl.banner(&path, &kind);
    repl.loop_forever()
}

/// Load `source` the way its declarations ask: a package (driven when it
/// declares actors) or a plain program. Returns the runtime, the package's
/// actors, and a one-line description for the banner.
fn load(
    store: Arc<dyn WorldStore>,
    source: &str,
) -> Result<(Arc<Runtime>, Vec<CompiledActor>, String), String> {
    let declarations = grmpl_lang::parse(source).map_err(|d| d.render(source, None))?;
    if !declarations.iter().any(|d| matches!(d.node, Decl::Package { .. })) {
        let runtime = Runtime::compile(store, source, REL_BASE)?;
        return Ok((runtime, Vec::new(), "a program".into()));
    }
    // Compile once to read what the package declares. The runtime compiles it
    // again against the same durable catalog, so it recovers the same ids.
    let package = CompiledPackage::compile_with_catalog(source, store.as_ref(), REL_BASE)?;
    let grants = declared_grants(&package)?;
    let id = &package.package_id;
    if package.actors.is_empty() {
        let runtime = Runtime::load_package(store, source, REL_BASE, &grants)?;
        return Ok((runtime, Vec::new(), format!("package `{id}`")));
    }
    let policy = declared_policy(&package, grants);
    let runtime = Runtime::load_driven_package(store, source, REL_BASE, &policy)?;
    let names: Vec<&str> = package.actors.iter().map(|a| a.name.as_str()).collect();
    let kind = format!("package `{id}`, driving actors {}", names.join(", "));
    Ok((runtime, package.actors, kind))
}

/// Grant every capability the package requires, exactly as it declares it. A
/// schedule may address every declared actor.
fn declared_grants(package: &CompiledPackage) -> Result<GrantSet, String> {
    let mut grants = GrantSet::new();
    for requirement in &package.requirements {
        grants = match requirement {
            CapabilityRequirement::Allocate {
                name,
                counter,
                first,
                last,
            } => grants.grant_allocate(name, counter, *first, *last)?,
            CapabilityRequirement::Random {
                name,
                state,
                owner,
                algorithm,
            } => grants.grant_random(name, state, *owner, algorithm)?,
            CapabilityRequirement::Schedule {
                name,
                clock,
                timers,
                sequences,
            } => grants.grant_schedule(
                name,
                clock,
                timers,
                sequences,
                package.actors.iter().map(|a| a.name.clone()),
            )?,
        };
    }
    Ok(grants)
}

/// The driven package's policy: each actor owns the relations its authority
/// request writes, and the driver owns the schedule's clock, timers and
/// sequences and every actor's inbox.
fn declared_policy(package: &CompiledPackage, grants: GrantSet) -> RuntimePolicy {
    let named = |relations: BTreeSet<String>| {
        NamedAuthority::new(DOMAIN, relations.into_iter().map(NamedScope::whole).collect())
    };
    let actor_authorities = package
        .actors
        .iter()
        .map(|actor| {
            let writes = package
                .authority_requests
                .iter()
                .find(|request| request.name == actor.authority)
                .map(|request| request.writes.iter().cloned().collect())
                .unwrap_or_default();
            (actor.name.clone(), named(writes))
        })
        .collect();
    let mut driver: BTreeSet<String> =
        package.actors.iter().map(|a| a.inbox_name.clone()).collect();
    for requirement in &package.requirements {
        if let CapabilityRequirement::Schedule {
            clock,
            timers,
            sequences,
            ..
        } = requirement
        {
            driver.extend([clock.clone(), timers.clone(), sequences.clone()]);
        }
    }
    RuntimePolicy::new(grants, actor_authorities, named(driver))
}

// ===========================================================================
// The REPL
// ===========================================================================
struct Repl {
    runtime: Arc<Runtime>,
    /// A driven package's actors; empty otherwise.
    actors: Vec<CompiledActor>,
    watches: Vec<Watching>,
    /// The next candidate watch entity.
    next_watch: u64,
}

/// One installed watch and the first inbox sequence not yet printed.
struct Watching {
    label: String,
    watch: OnWatch,
    next_seq: i64,
}

impl Repl {
    fn banner(&self, path: &str, kind: &str) {
        let program = self.runtime.program();
        println!("grmpl — running {path} ({kind})");
        println!(
            "{}, {}, at edition {}.",
            count(program.rel_names().len(), "relation"),
            count(program.view_names().len(), "view"),
            self.edition().0
        );
        println!("Type `help` for commands, `quit` to leave.");
    }

    fn loop_forever(&mut self) -> Result<(), String> {
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            print!("\n> ");
            io::stdout().flush().ok();
            let line = match lines.next() {
                None => {
                    println!("\nGoodbye.");
                    return Ok(());
                }
                Some(Ok(line)) => line,
                Some(Err(e)) => return Err(format!("stdin: {e}")),
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if matches!(line, "quit" | "exit") {
                println!("Goodbye.");
                return Ok(());
            }
            if let Err(e) = self.command(line) {
                println!("error: {e}");
            }
            if let Err(e) = self.pump_watches() {
                println!("error: watch: {e}");
            }
        }
    }

    fn command(&mut self, line: &str) -> Result<(), String> {
        if let Some(rest) = line.strip_prefix('?') {
            return self.cmd_view(None, rest);
        }
        let (verb, rest) = split_word(line);
        match verb {
            "help" => {
                print_help();
                Ok(())
            }
            ":rels" => {
                self.cmd_rels();
                Ok(())
            }
            ":views" => {
                self.cmd_views();
                Ok(())
            }
            ":read" => self.cmd_read(rest),
            ":at" => {
                let (edition, query) = split_word(rest);
                let edition = edition
                    .parse::<u64>()
                    .map_err(|_| format!("`{edition}` is not an edition number"))?;
                let query = query
                    .strip_prefix('?')
                    .ok_or("usage: :at N ? VIEW ARG…")?;
                self.cmd_view(Some(Edition(edition)), query)
            }
            ":send" => self.cmd_send(rest),
            ":watch" => self.cmd_watch(rest),
            ":edition" => {
                println!("edition {}", self.edition().0);
                Ok(())
            }
            other => Err(format!("unknown command `{other}`; type `help`")),
        }
    }

    // --- inspection ------------------------------------------------------

    fn cmd_rels(&self) {
        let program = self.runtime.program();
        for name in program.rel_names() {
            let columns: Vec<String> = program
                .rel_columns(name)
                .unwrap_or_default()
                .iter()
                .map(|c| format!("{}: {:?}", c.name, c.ty))
                .collect();
            println!("{name}({})", columns.join(", "));
        }
    }

    fn cmd_views(&self) {
        let program = self.runtime.program();
        let materialized = program.materialized_views();
        if program.view_names().is_empty() {
            println!("(no views)");
        }
        for name in program.view_names() {
            let params = program.view_params(name).unwrap_or_default().join(", ");
            let yields = program.view_yields(name).unwrap_or_default().join(", ");
            let note = if materialized.contains(&name) {
                "  [materialized]"
            } else {
                ""
            };
            println!("{name}({params}) -> {yields}{note}");
        }
    }

    /// `? VIEW ARG…`, now or (`:at N`) as of edition `at`.
    fn cmd_view(&self, at: Option<Edition>, query: &str) -> Result<(), String> {
        let (name, rest) = split_word(query.trim());
        if name.is_empty() {
            return Err("usage: ? VIEW ARG…".into());
        }
        let args = self.parse_args(rest)?;
        let rows = match at {
            None => self.runtime.view(name, &args)?,
            Some(at) => {
                let now = self.edition();
                if at > now {
                    return Err(format!(
                        "edition {} is in the future; the world is at {}",
                        at.0, now.0
                    ));
                }
                let snapshot = Snapshot::new(self.runtime.store(), at);
                self.runtime
                    .query(name, &args)?
                    .find(&snapshot)
                    .map_err(|e| e.to_string())?
            }
        };
        let headers = self
            .runtime
            .program()
            .view_yields(name)
            .unwrap_or_default()
            .to_vec();
        print_table(&headers, &rows);
        Ok(())
    }

    /// `:read REL` — the relation's consolidated rows at the current edition.
    fn cmd_read(&self, rest: &str) -> Result<(), String> {
        let name = rest.trim();
        if name.is_empty() {
            return Err("usage: :read REL".into());
        }
        let rel = self.runtime.relation(name).map_err(|e| e.to_string())?;
        let rows: Vec<(Tuple, Diff)> = self
            .runtime
            .store()
            .read_at(rel, self.edition())
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|(_, weight)| *weight != 0)
            .collect();
        let headers: Vec<String> = self
            .runtime
            .program()
            .rel_columns(name)
            .unwrap_or_default()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        print_table(&headers, &rows);
        Ok(())
    }

    // --- writes ----------------------------------------------------------

    /// `:send ENTITY INBOX SEQS TEXT…` — enqueue one command, then run the
    /// world to rest: drive the actors of a driven package, and step a process
    /// for any inbox no actor owns.
    fn cmd_send(&self, rest: &str) -> Result<(), String> {
        const USAGE: &str = "usage: :send ENTITY INBOX SEQS TEXT…";
        let (entity, rest) = split_word(rest);
        let (inbox, rest) = split_word(rest);
        let (seqs, text) = split_word(rest);
        if entity.is_empty() || inbox.is_empty() || seqs.is_empty() {
            return Err(USAGE.into());
        }
        let entity = match self.parse_value(entity, false) {
            Value::Ent(e) => e,
            Value::Int(n) if n >= 0 => Entity(n as u64),
            _ => return Err(format!("`{entity}` is not an entity; {USAGE}")),
        };
        let store = self.runtime.store();
        let before = store.current();
        let seq = self
            .runtime
            .enqueue(entity, inbox, seqs, text)
            .map_err(|e| e.to_string())?;
        println!("queued #{} {inbox} seq {seq}", entity.0);

        let actor = self
            .actors
            .iter()
            .any(|a| a.entity == entity && a.inbox_name == inbox);
        if !actor {
            let process = self.process(entity, inbox)?;
            self.runtime.run_to_idle(&process).map_err(|e| e.to_string())?;
        }
        if self.actors.is_empty() {
            self.runtime.refresh_views().map_err(|e| e.to_string())?;
        } else {
            let report = self.runtime.drive_to_idle().map_err(|e| e.to_string())?;
            match report.status {
                DriveStatus::Idle => {}
                DriveStatus::FuelExhausted => println!("(driver ran out of fuel)"),
                DriveStatus::ActorFault {
                    actor,
                    sequence,
                    message,
                } => println!(
                    "(actor #{} faulted at inbox sequence {sequence}: {message})",
                    actor.0
                ),
            }
        }
        self.print_told(before)?;
        println!("edition {} -> {}", before.0, store.current().0);
        Ok(())
    }

    /// A process for an inbox no actor owns, holding exactly the relations
    /// its handler writes or sends to, and its cursor. The cursor is the one an
    /// actor reading the same inbox declares, else the relation named `cursor`.
    fn process(&self, entity: Entity, inbox: &str) -> Result<Process, String> {
        let cursor = self
            .actors
            .iter()
            .find(|a| a.inbox_name == inbox)
            .map_or("cursor", |a| a.cursor_name.as_str());
        let cursor_rel = self
            .runtime
            .relation(cursor)
            .map_err(|_| format!("no `rel {cursor}` to hold inbox `{inbox}`'s read cursor"))?;
        let effects = infer_handler_effects(self.runtime.program(), inbox)
            .map_err(|e| e.to_string())?;
        let mut owns: BTreeSet<_> = effects.writes().chain(effects.sends()).collect();
        owns.insert(cursor_rel);
        let authority = Authority::new(DOMAIN, owns.into_iter().map(Scope::whole).collect());
        self.runtime.process(entity, authority, inbox, cursor)
    }

    /// Print what the world told since `since`, if it has a `tell` relation.
    fn print_told(&self, since: Edition) -> Result<(), String> {
        let Ok(tell) = self.runtime.relation("tell") else {
            return Ok(());
        };
        let store = self.runtime.store();
        for update in store
            .scan_updates(tell, since, store.current())
            .map_err(|e| e.to_string())?
        {
            if update.diff <= 0 {
                continue;
            }
            match update.tuple.as_slice() {
                [Value::Ent(to), Value::Text(text)] => println!("tell #{}: {text}", to.0),
                cells => println!("tell {}", render_cells(cells)),
            }
        }
        Ok(())
    }

    // --- watches ---------------------------------------------------------

    /// `:watch VIEW ARG…` — install the view's source-declared `on watch`
    /// under a fresh entity. Its activations print after every command.
    fn cmd_watch(&mut self, rest: &str) -> Result<(), String> {
        let (view, rest) = split_word(rest.trim());
        if view.is_empty() {
            return Err("usage: :watch VIEW ARG…".into());
        }
        let args = self.parse_args(rest)?;
        let program = self.runtime.program();
        let store = self.runtime.store();
        // Lower first to learn the watch's relations and to skip any entity a
        // reused store already holds a cursor for.
        let (entity, lowered) = loop {
            let entity = Entity(self.next_watch);
            self.next_watch += 1;
            let lowered = program.on_watch(view, &args, entity, entity, Authority::new(DOMAIN, vec![]))?;
            if lowered.cursor(store).map_err(|e| e.to_string())?.is_none() {
                break (entity, lowered);
            }
        };
        let authority = Authority::new(
            DOMAIN,
            vec![
                Scope::whole(lowered.inbox),
                Scope::whole(lowered.cursor_rel),
                Scope::whole(lowered.seqs),
            ],
        );
        let watch = self
            .runtime
            .install_watch(view, &args, entity, entity, authority)?;
        let label = format!("{view}({})", render_cells(&args));
        println!("watching {label} as #{}", entity.0);
        self.watches.push(Watching {
            label,
            watch,
            next_seq: i64::MIN,
        });
        Ok(())
    }

    /// Pump every watch and print the activations it delivered.
    fn pump_watches(&mut self) -> Result<(), String> {
        let store = self.runtime.store();
        for watching in &mut self.watches {
            let watch = &watching.watch;
            watch.pump(store, store).map_err(|e| e.to_string())?;
            for (row, weight) in store
                .read_at(watch.inbox, store.current())
                .map_err(|e| e.to_string())?
            {
                let [Value::Ent(target), Value::Int(seq), Value::Tuple(body)] = row.as_slice()
                else {
                    continue;
                };
                if weight <= 0 || *target != watch.target || *seq < watching.next_seq {
                    continue;
                }
                watching.next_seq = seq + 1;
                if let Some((diff, delta)) = decode_activation(&Tuple(body.clone())) {
                    let sign = if diff > 0 { "+" } else { "-" };
                    println!("[watch {}] {sign} {}", watching.label, render_cells(delta.as_slice()));
                }
            }
        }
        Ok(())
    }

    // --- helpers ---------------------------------------------------------

    fn edition(&self) -> Edition {
        self.runtime.store().current()
    }

    fn parse_args(&self, rest: &str) -> Result<Vec<Value>, String> {
        Ok(split_args(rest)?
            .into_iter()
            .map(|(word, quoted)| self.parse_value(&word, quoted))
            .collect())
    }

    /// One argument: an integer, `#N` (an entity), a package entity constant,
    /// `"quoted"` text, `true`/`false`, or else bare text.
    fn parse_value(&self, word: &str, quoted: bool) -> Value {
        if quoted {
            return Value::text(word);
        }
        if let Ok(n) = word.parse::<i64>() {
            return Value::Int(n);
        }
        if let Some(Ok(id)) = word.strip_prefix('#').map(str::parse::<u64>) {
            return Value::Ent(Entity(id));
        }
        if let Some(entity) = self.runtime.program().entity(word) {
            return Value::Ent(entity);
        }
        match word {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::text(word),
        }
    }
}

fn print_help() {
    println!(
        "\
:rels                       every relation with its typed columns
:views                      every view with its parameters and yields
? VIEW ARG…                 evaluate a view now
:read REL                   a relation's consolidated rows now
:at N ? VIEW ARG…           evaluate a view as of edition N
:send ENTITY INBOX SEQS TEXT…
                            enqueue TEXT in ENTITY's INBOX (sequenced by SEQS),
                            run the world to rest, and print what it told
:watch VIEW ARG…            install the view's `on watch`; its activations print
                            after every command
:edition                    the current edition
help | quit
Arguments: 42 (Int), #42 (entity), NAME (a package entity constant),
\"some text\", true/false; any other word is text."
    );
}

/// Print rows as aligned columns under `headers`. A column past the headers
/// (a view's aggregate) is headed `agg`; a weight column appears only when
/// some row's weight is not 1.
fn print_table(headers: &[String], rows: &[(Tuple, Diff)]) {
    let arity = rows
        .iter()
        .map(|(row, _)| row.as_slice().len())
        .max()
        .unwrap_or(0)
        .max(headers.len());
    let mut head: Vec<String> = (0..arity)
        .map(|i| headers.get(i).cloned().unwrap_or_else(|| "agg".into()))
        .collect();
    let weighted = rows.iter().any(|(_, weight)| *weight != 1);
    if weighted {
        head.push("weight".into());
    }
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|(row, weight)| {
            let mut cells: Vec<String> = (0..arity)
                .map(|i| row.as_slice().get(i).map(render).unwrap_or_default())
                .collect();
            if weighted {
                cells.push(weight.to_string());
            }
            cells
        })
        .collect();
    let widths: Vec<usize> = (0..head.len())
        .map(|i| {
            body.iter()
                .map(|cells| cells[i].chars().count())
                .chain([head[i].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(&head);
    line(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>());
    for cells in &body {
        line(cells);
    }
    println!("({})", count(rows.len(), "row"));
}

/// `n` things, singular or plural.
fn count(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}

/// A value as the REPL prints it; entities as `#N`, which parse back.
fn render(value: &Value) -> String {
    match value {
        Value::Ent(e) => format!("#{}", e.0),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Tuple(cells) => format!("({})", render_cells(cells)),
        Value::Bytes(bytes) => bytes.iter().fold("0x".to_string(), |mut out, b| {
            out.push_str(&format!("{b:02x}"));
            out
        }),
        Value::Code(bytes) => format!("<code, {} bytes>", bytes.len()),
    }
}

fn render_cells(cells: &[Value]) -> String {
    cells.iter().map(render).collect::<Vec<_>>().join(", ")
}

/// Split off the first whitespace-delimited word.
fn split_word(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    match line.find(char::is_whitespace) {
        Some(end) => (&line[..end], line[end..].trim_start()),
        None => (line, ""),
    }
}

/// Split arguments on whitespace, keeping a `"double-quoted"` run whole.
/// Each word comes back with whether it was quoted.
fn split_args(line: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.peek() {
            None => return Ok(out),
            Some('"') => {
                chars.next();
                let mut word = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated `\"`".into()),
                    }
                }
                out.push((word, true));
            }
            Some(_) => {
                let mut word = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    word.push(c);
                }
                out.push((word, false));
            }
        }
    }
}
