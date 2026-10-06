//! `grmpl store …` — offline tools over a store directory.
//!
//! * `grmpl store verify DIR` — check every node frame the root record reaches:
//!   stored, hashing to its key, decoding as a frame. Exits nonzero on damage.
//! * `grmpl store info DIR` — the branch DAG, each branch's clock, the catalog,
//!   and each relation's rows, shape and schema; plus node-store totals.
//! * `grmpl store history DIR REL [EDITION] [--branch B]` — every version, on
//!   any branch, that shares nodes with `REL` as of `EDITION`: backfollow.
//!
//! None of them writes. `verify` reads frames straight from the node store, so
//! a damaged one is reported rather than panicking; `info` and `history` read
//! through the Ent and page nodes in, so on a damaged store they stop at the
//! first bad frame and point at `verify`. `history` catches the history index
//! up in memory only, as every query does, and never stores the progress.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use grmpl_core::{wire, Catalog, Edition, EditionStore, RelId, SchemaCatalog, TraceStore, Tuple, Value};
use grmpl_ent::{Dag, EntStore, Granfilade, Layout};

/// Usage of the `store` subcommands, also folded into the top-level help.
pub const USAGE: &str = "\
    grmpl store verify DIR                Check every node frame a store's root
                                          reaches: present, hashing to its key,
                                          decodable. Exits nonzero on damage.
    grmpl store info DIR                  Branches, clocks, catalog, relations
                                          (rows, layout, runs, schema), totals.
    grmpl store history DIR REL [EDITION] [--branch B]
                                          The versions, on every branch, that
                                          share nodes with REL as of EDITION.";

pub fn run(args: &[String]) -> Result<(), String> {
    let usage = || format!("usage:\n{USAGE}");
    let (cmd, dir) = match args {
        [cmd, dir, ..] => (cmd.as_str(), existing_store(dir)?),
        _ => return Err(usage()),
    };
    match (cmd, &args[2..]) {
        ("verify", []) => verify(&dir),
        ("info", []) => paging(&dir, || info(&dir)),
        ("history", rest) if !rest.is_empty() => {
            let (rel, edition, branch) = history_args(rest).ok_or_else(usage)?;
            paging(&dir, || history(&dir, &rel, edition, branch))
        }
        _ => Err(usage()),
    }
}

/// `dir`, if it holds a store. Opening creates a missing store, which a tool
/// that promises not to write must never do.
fn existing_store(dir: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(dir);
    let occupied = std::fs::read_dir(&path).map(|mut d| d.next().is_some()).unwrap_or(false);
    if !occupied {
        return Err(format!("no store at {dir}"));
    }
    Ok(path)
}

/// Run a command that reads through the Ent. A frame that is missing or
/// damaged panics the pager mid-read; say what that means instead of leaving
/// only the panic.
fn paging(dir: &Path, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err(format!(
            "a node frame could not be read; the store is damaged. \
             Run `grmpl store verify {}` for the full account.",
            dir.display()
        ))
    })
}

fn open_ent(dir: &Path) -> Result<EntStore, String> {
    EntStore::open(dir).map_err(|e| format!("cannot open store at {}: {e:?}", dir.display()))
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

fn verify(dir: &Path) -> Result<(), String> {
    let gran = Granfilade::open(dir).map_err(|e| format!("cannot open store at {}: {e:?}", dir.display()))?;
    let v = gran.verify().map_err(|e| format!("cannot verify {}: {e:?}", dir.display()))?;
    println!("store {} (format v{})", dir.display(), wire::FORMAT_VERSION);
    println!("  frames stored     {} ({} bytes)", v.frames, v.bytes);
    println!("  reachable         {}", v.reachable);
    println!("  missing           {}", v.missing.len());
    println!("  hash mismatch     {}", v.mismatched.len());
    println!("  undecodable       {}", v.undecodable.len());
    println!("  unreachable       {} (a gc would sweep these)", v.unreachable);
    for ck in &v.missing {
        println!("  missing frame     {}", hex(ck));
    }
    for ck in &v.mismatched {
        println!("  hash mismatch     {}", hex(ck));
    }
    for (ck, why) in &v.undecodable {
        println!("  undecodable       {}: {why}", hex(ck));
    }
    if v.is_sound() {
        println!("ok");
        Ok(())
    } else {
        let bad = v.missing.len() + v.mismatched.len() + v.undecodable.len();
        Err(format!("store is damaged: {bad} problem frame(s)"))
    }
}

fn hex(ck: &[u8]) -> String {
    ck.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

fn info(dir: &Path) -> Result<(), String> {
    let store = open_ent(dir)?;
    let err = |e: grmpl_core::Error| format!("{e:?}");
    let frames = store.stored_nodes().map_err(err)?;
    let bytes = store.stored_bytes().map_err(err)?;
    println!("store {} (format v{})", dir.display(), wire::FORMAT_VERSION);
    println!("  nodes   {frames} frames, {bytes} bytes in frames, {} bytes on disk", disk_bytes(dir));

    let dag = store.dag();
    println!("branches:");
    for b in dag.branches() {
        let handle = store.branch(b.id).map_err(err)?;
        let origin = match (b.parent, b.merged) {
            (None, _) => "root".to_string(),
            (Some((p, at)), None) => format!("fork of {p} @ {at}"),
            (Some((p, at)), Some((o, oat, since))) => {
                format!("merge of {p} @ {at} with {o} @ {oat} (all in from {since})")
            }
        };
        println!(
            "  {:<3} {origin:<28} current {}  durable {}  watermark {}",
            b.id,
            handle.current().0,
            handle.durable_edition().0,
            handle.watermark().0
        );
    }

    let catalog = store.entries().map_err(err)?;
    println!("catalog (branch {}): {} name(s)", Dag::ROOT, catalog.len());
    for (name, id) in &catalog {
        println!("  {name:<20} rel {}", id.0);
    }

    for b in dag.branches() {
        let handle = store.branch(b.id).map_err(err)?;
        let names = handle.entries().map_err(err)?;
        let name_of = |rel: RelId| names.iter().find(|(_, id)| *id == rel).map(|(n, _)| n.as_str());
        let mut rels = handle.relations();
        // A relation can be named and schema'd before its first row.
        rels.extend(names.iter().map(|(_, id)| *id));
        rels.sort_unstable();
        rels.dedup();
        let at = handle.current();
        println!("relations on branch {} @ {}: {}", b.id, at.0, rels.len());
        if rels.is_empty() {
            continue;
        }
        println!("  {:<6} {:<20} {:>8}  {:<6} {:<5} schema", "rel", "name", "rows", "layout", "runs");
        for rel in rels {
            let rows = handle.rows_at(rel, at).map_err(err)?;
            let shape = handle.shape(rel);
            let layout = match shape.layout {
                Layout::Ordered => "B+",
                Layout::Kd => "k-d",
            };
            let schema = match handle.schema(rel).map_err(err)? {
                None => "-".to_string(),
                Some(s) => {
                    let cols: Vec<String> = s.columns.iter().map(|c| format!("{}: {}", c.name, c.ty.name())).collect();
                    format!("({})", cols.join(", "))
                }
            };
            println!(
                "  {:<6} {:<20} {rows:>8}  {layout:<6} {:<5} {schema}",
                rel.0,
                name_of(rel).unwrap_or("-"),
                if shape.runs { "on" } else { "off" },
            );
        }
    }
    Ok(())
}

/// Bytes in the store directory's files, as the filesystem reports them.
fn disk_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => disk_bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

/// `REL [EDITION] [--branch B]`, in any order after `REL`.
fn history_args(args: &[String]) -> Option<(String, Option<u64>, u64)> {
    let mut it = args.iter();
    let rel = it.next()?.clone();
    let (mut edition, mut branch) = (None, Dag::ROOT);
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = it.next()?.parse().ok()?;
        } else if edition.is_none() {
            edition = Some(a.parse().ok()?);
        } else {
            return None;
        }
    }
    Some((rel, edition, branch))
}

fn history(dir: &Path, rel: &str, edition: Option<u64>, branch: u64) -> Result<(), String> {
    let root = open_ent(dir)?;
    let err = |e: grmpl_core::Error| format!("{e:?}");
    let store = root.branch(branch).map_err(err)?;
    let names = store.entries().map_err(err)?;
    let rel_id = match rel.parse::<u32>() {
        Ok(n) => RelId(n),
        Err(_) => store.rel_id(rel).map_err(err)?.ok_or_else(|| format!("no relation named `{rel}` on branch {branch}"))?,
    };
    let name_of = |r: RelId| names.iter().find(|(_, id)| *id == r).map_or_else(|| format!("rel {}", r.0), |(n, _)| n.clone());
    let current = store.current().0;
    let at = edition.unwrap_or(current);
    if at > current {
        return Err(format!("edition {at} is past branch {branch}'s current edition {current}"));
    }

    // Backfollow takes a key span: the relation's first row to just past its
    // last (a tuple extending a row sorts after it and before its successor).
    let rows = store.read_at(rel_id, Edition(at)).map_err(err)?;
    println!("{} on branch {branch} @ {at}: {} row(s)", name_of(rel_id), rows.len());
    let (Some((lo, _)), Some((last, _))) = (rows.first(), rows.last()) else {
        return Ok(());
    };
    let hi = Tuple::new(last.as_slice().iter().cloned().chain([Value::Bool(false)]).collect::<Vec<_>>());

    let backlog = store.history_backlog();
    let found = store.backfollow(rel_id, Edition(at), lo, &hi).map_err(err)?;
    println!("  history index: {backlog} version(s) caught up in memory, not written");
    if found.is_empty() {
        println!("  no version shares its nodes");
        return Ok(());
    }
    println!("versions sharing its nodes (each stands until the relation next changes on its branch):");
    println!("  {:<6} {:<8} {:<20} {:>12} {:>8}", "branch", "edition", "relation", "shift", "rows");
    for h in &found {
        let all = if h.shift == 0 && h.rows == rows.len() { "  all, in place" } else { "" };
        println!(
            "  {:<6} {:<8} {:<20} {:>12} {:>8}{all}",
            h.version.branch,
            h.version.edition.0,
            name_of(h.version.rel),
            h.shift,
            h.rows
        );
    }
    Ok(())
}
