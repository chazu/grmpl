//! `grmpl run` is a generic REPL: it stands up any world — a package or a plain
//! program — and answers its meta-commands from the compiled program and the
//! store alone. These pipe a script into the binary and read what it printed.

use std::io::Write;
use std::process::{Command, Stdio};

/// A small package: two rooms, a view over them, one verb, and a watch.
const TINY: &str = r#"
package tiny bootstrap 1

entity ALICE = 1
entity HALL = 10
entity YARD = 11

rel located(thing: Ent, place: Ent)
rel named(thing: Ent, name: Text)
rel tell(who: Ent, text: Text)
rel inbox(process: Ent, seq: Int, body: Tuple)
rel cursor(process: Ent, pos: Int)
rel inbox_seq(process: Ent, n: Int)
rel wmail(who: Ent, seq: Int, body: Tuple)
rel wcursor(watch: Ent, edition: Int)
rel wseq(inbox: Int, target: Ent, n: Int)

view whereis(thing) {
    located(thing, place)
    named(place, name)
    yield place, name
}

view rooms() {
    named(place, name)
    yield place, name
}

form command {
    "go" dest -> Go(dest)
}

on inbox parse command {
    match Go(dest) {
        find located(self, here)
        resolve rooms() where name = dest
        expect located(self, here)
        retract located(self, here)
        assert located(self, place)
        emit tell(self, "Moved.")
    }
}

on watch whereis { inbox wmail cursor wcursor seqs wseq }

bootstrap {
    located(ALICE, HALL)
    named(ALICE, "Alice")
    named(HALL, "Hall")
    named(YARD, "Yard")
    inbox_seq(ALICE, 0)
}
"#;

/// Run `grmpl run WORLD STORE` over `script`, returning stdout.
fn run(world: &std::path::Path, script: &str) -> String {
    let store = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_grmpl"))
        .arg("run")
        .arg(world)
        .arg(store.path().join("store"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("grmpl runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "grmpl run failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

fn write_world(dir: &tempfile::TempDir, source: &str) -> std::path::PathBuf {
    let path = dir.path().join("world.grmpl");
    std::fs::write(&path, source).unwrap();
    path
}

#[test]
fn a_package_answers_every_meta_command() {
    let dir = tempfile::tempdir().unwrap();
    let world = write_world(&dir, TINY);
    let out = run(
        &world,
        ":rels\n:views\n? whereis ALICE\n:watch whereis #1\n\
         :send ALICE inbox inbox_seq go Yard\n? whereis #1\n:at 1 ? whereis ALICE\n\
         :read located\n? nope\n:at 99 ? rooms\n:edition\nquit\n",
    );
    assert!(out.contains("package `tiny`"), "{out}");
    // `:rels`: every declared relation with its typed columns, and no
    // compiler-reserved one.
    assert!(out.contains("located(thing: Ent, place: Ent)"), "{out}");
    assert!(out.contains("named(thing: Ent, name: Text)"), "{out}");
    assert!(!out.contains("grmpl:package"), "{out}");
    // `:views`: parameters and yields.
    assert!(out.contains("rooms() -> place, name"), "{out}");
    assert!(out.contains("whereis(thing) -> place, name"), "{out}");
    // `? whereis ALICE` binds the entity constant and prints aligned rows.
    assert!(out.contains("place  name\n-----  ----\n#10    Hall\n(1 row)"), "{out}");
    // `:send` runs the handler, prints what it told, and the watch fires.
    assert!(out.contains("tell #1: Moved."), "{out}");
    assert!(out.contains("[watch whereis(#1)] - #10, Hall"), "{out}");
    assert!(out.contains("[watch whereis(#1)] + #11, Yard"), "{out}");
    assert!(out.contains("#11    Yard"), "{out}");
    // `:at 1` reads the bootstrap edition: Alice is back in the hall.
    let after_send = &out[out.find("tell #1: Moved.").unwrap()..];
    assert!(after_send.contains("#10    Hall"), "{out}");
    // Errors print and the REPL carries on to `quit`.
    assert!(out.contains("error: no view `nope`"), "{out}");
    assert!(out.contains("error: edition 99 is in the future"), "{out}");
    assert!(out.trim_end().ends_with("Goodbye."), "{out}");
}

#[test]
fn a_one_line_program_runs() {
    let dir = tempfile::tempdir().unwrap();
    let world = write_world(&dir, "rel at(a: Ent, b: Ent)\n");
    let out = run(&world, ":rels\n:views\n:read at\n:edition\nquit\n");
    assert!(out.contains("(a program)"), "{out}");
    assert!(out.contains("at(a: Ent, b: Ent)"), "{out}");
    assert!(out.contains("(no views)"), "{out}");
    assert!(out.contains("a  b\n-  -\n(0 rows)"), "{out}");
    assert!(out.contains("edition 0"), "{out}");
}

#[test]
fn the_bundled_worlds_load_and_list_their_views() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../worlds");
    let moo = run(&root.join("moo.grmpl"), ":views\n? treasure\nquit\n");
    assert!(moo.contains("package `manor`"), "{moo}");
    assert!(moo.contains("here(viewer) -> thing, name  [materialized]"), "{moo}");
    assert!(moo.contains("Market   20"), "{moo}");

    let shotengai = run(&root.join("shotengai.grmpl"), ":views\nquit\n");
    assert!(shotengai.contains("driving actors CAT, COMBAT, PLAYER"), "{shotengai}");
    assert!(shotengai.contains("jobs() -> which, name"), "{shotengai}");
}
