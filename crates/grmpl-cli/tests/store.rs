//! `grmpl store …` against a real store: `info` and `history` describe a small
//! world with a fork, and `verify` passes it, then reports a damaged frame and
//! a missing one, exiting nonzero rather than panicking.

use std::path::Path;
use std::process::{Command, Output};

use grmpl_core::{
    Catalog, Column, EditionStore, Entity, RelId, Schema, SchemaCatalog, TraceStore, Tuple, Ty, Value,
};
use grmpl_ent::{Durability, EntStore, Granfilade, Layout};

const PLACE: RelId = RelId(1);
const NAME: RelId = RelId(2);
const GRID: RelId = RelId(3);

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn grmpl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grmpl")).args(args).output().expect("run grmpl")
}

fn text(o: &Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

/// Three relations (one k-d, one with runs, one schema'd), three commits on
/// the root, and a fork at the second with one commit of its own.
fn build(dir: &Path) {
    let s = EntStore::open_with(dir, Durability::Os).unwrap();
    s.register("place", PLACE).unwrap();
    s.register("name", NAME).unwrap();
    s.register("grid", GRID).unwrap();
    s.set_layout(GRID, Layout::Kd).unwrap();
    s.set_runs(NAME, true).unwrap();
    let schema = Schema::new(vec![Column::new("who", Ty::Ent), Column::new("where", Ty::Ent)]);
    s.put_schema(PLACE, &schema, s.current()).unwrap();
    s.commit(&(0..40).map(|e| (PLACE, Tuple::from([ent(e), ent(1000)]), 1)).collect::<Vec<_>>()).unwrap();
    s.commit(&(0..10).map(|e| (NAME, Tuple::from([ent(e), Value::text(format!("thing {e}"))]), 1)).collect::<Vec<_>>())
        .unwrap();
    let fork_at = s.current();
    s.commit(&(0..5).map(|e| (GRID, Tuple::from([Value::Int(e), Value::Int(e * e)]), 1)).collect::<Vec<_>>()).unwrap();
    let fork = s.fork_at(fork_at).unwrap();
    fork.commit(&[(PLACE, Tuple::from([ent(99), ent(1000)]), 1)]).unwrap();
}

#[test]
fn info_and_history_describe_the_world() {
    let dir = tempfile::tempdir().unwrap();
    build(dir.path());
    let path = dir.path().to_str().unwrap();

    let out = grmpl(&["store", "info", path]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "info failed: {stderr}");
    for want in ["branches:", "fork of 0 @", "catalog (branch 0): 3 name(s)", "relations on branch 1"] {
        assert!(stdout.contains(want), "info lacks `{want}`:\n{stdout}");
    }
    let line = |name: &str, branch: &str| {
        let section = stdout.split(&format!("relations on branch {branch} ")).nth(1).unwrap();
        let section = section.split("relations on branch").next().unwrap();
        let row = |l: &&str| l.split_whitespace().nth(1) == Some(name) && !l.contains("layout");
        section.lines().find(row).unwrap_or("").to_string()
    };
    assert!(line("place", "0").contains("40") && line("place", "0").contains("(who: Ent, where: Ent)"), "{stdout}");
    assert!(line("place", "1").contains("41"), "the fork's own row:\n{stdout}");
    assert!(line("grid", "0").contains("k-d"), "{stdout}");
    assert!(line("name", "0").split_whitespace().nth(4) == Some("on"), "runs:\n{stdout}");

    // The fork inherited the root's `place` version: backfollow finds it on
    // both branches, in place.
    let out = grmpl(&["store", "history", path, "place", "1"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "history failed: {stderr}");
    assert!(stdout.contains("place on branch 0 @ 1: 40 row(s)"), "{stdout}");
    assert!(stdout.contains("all, in place"), "{stdout}");
    assert!(stdout.contains("not written"), "{stdout}");
}

#[test]
fn verify_passes_a_sound_store_and_reports_damage() {
    let dir = tempfile::tempdir().unwrap();
    build(dir.path());
    let path = dir.path().to_str().unwrap();

    let out = grmpl(&["store", "verify", path]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "a sound store failed verify: {stdout}{stderr}");
    assert!(stdout.contains("missing           0") && stdout.trim_end().ends_with("ok"), "{stdout}");

    // Overwrite the branch enfilade's root frame with garbage.
    let slots = {
        let gran = Granfilade::open_with(dir.path(), Durability::Os).unwrap();
        let slots: Vec<_> = gran.root().unwrap();
        gran.clobber_frame(&slots[1].unwrap(), Some(b"not a frame")).unwrap();
        slots
    };
    let out = grmpl(&["store", "verify", path]);
    let (stdout, stderr) = text(&out);
    assert!(!out.status.success(), "verify passed a damaged store:\n{stdout}");
    assert!(!stderr.contains("panicked"), "verify panicked:\n{stderr}");
    assert!(stdout.contains("hash mismatch     1") && stdout.contains("undecodable       1"), "{stdout}");
    assert!(stderr.contains("store is damaged"), "{stderr}");

    // Delete the DAG's root frame too.
    {
        let gran = Granfilade::open_with(dir.path(), Durability::Os).unwrap();
        gran.clobber_frame(&slots[0].unwrap(), None).unwrap();
    }
    let out = grmpl(&["store", "verify", path]);
    let (stdout, stderr) = text(&out);
    assert!(!out.status.success() && !stderr.contains("panicked"), "{stdout}{stderr}");
    assert!(stdout.contains("missing           1"), "{stdout}");

    // `info` refuses the damaged store with an error, not a panic.
    let out = grmpl(&["store", "info", path]);
    let (_, stderr) = text(&out);
    assert!(!out.status.success() && stderr.contains("error"), "{stderr}");
}

#[test]
fn a_missing_store_is_refused_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing-here");
    let out = grmpl(&["store", "verify", missing.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(!missing.exists(), "verify created a store");
}
