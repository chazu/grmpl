//! **Golden fixtures for the on-disk format.** Small trees built by hand, so
//! no change to how trees balance can move them, are written to a granfilade
//! and their frames read back: a leaf of a row, a run and a hole, a leaf of
//! links, a B+ internal node with a displaced child, a k-d split, and the root
//! record naming them. Each frame's bytes and content key are compared with
//! `tests/golden/v{FORMAT_VERSION}.txt`, as `grmpl-core`'s fixtures are for
//! the wire: a framing that changes under the same version fails here, since
//! a round trip cannot see it. `GRMPL_BLESS=1` writes the file instead.
//!
//! In the crate rather than under `tests/` because the root record's codec is
//! private.

use std::fmt::Write as _;
use std::path::PathBuf;

use grmpl_core::wire::FORMAT_VERSION;
use grmpl_core::{Entity, Tuple, Value};

use crate::granfilade::{Durability, Granfilade, StagedWrite};
use crate::measure::{Count, Extent};
use crate::tree::{Item, Span, Tree};

type Fact = Tree<Tuple, i64, (Count, Extent)>;
type Links = Tree<u64, Fact, Count>;

fn ent(n: u64) -> Value {
    Value::Ent(Entity(n))
}

fn row(a: u64, b: i64) -> Tuple {
    Tuple::from([ent(a), Value::Int(b)])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One line per frame and per content key, then the root record.
fn corpus() -> String {
    let dir = tempfile::tempdir().unwrap();
    let gran = Granfilade::open_with(dir.path(), Durability::Os).unwrap();

    let step = Tuple::from([Value::Int(1), Value::Int(0)]);
    let items = Fact::leaf_of(vec![
        Item::One(row(1, 0), 1),
        Item::Run(Span { first: row(10, 0), stride: step.clone(), n: 3 }, 2),
        Item::Hole(Span { first: row(20, 0), stride: step, n: 4 }),
    ]);
    let lo = Fact::leaf_of(vec![Item::One(row(1, 5), 1), Item::One(row(2, 5), -1)]);
    let hi = Fact::leaf_of(vec![Item::One(row(0, 6), 1), Item::One(row(3, 6), 1)]);
    let internal = Fact::internal_of(vec![Tuple::from([ent(100)])], vec![lo.clone(), hi.clone().relocate(100)]);
    let split = Fact::split_of(1, Tuple::from([Value::Int(6)]), lo, hi);
    let links = Links::leaf_of(vec![Item::One(1, items.clone()), Item::One(2, Fact::new())]);

    let named = [
        ("leaf.items", gran.collect_tree(&items)),
        ("leaf.links", gran.collect_tree(&links)),
        ("internal", gran.collect_tree(&internal)),
        ("split", gran.collect_tree(&split)),
    ];
    // The root names each tree, with an empty slot among them.
    let mut root: Vec<_> = named.iter().map(|(_, (ck, _))| *ck).collect();
    root.insert(1, None);
    let nodes = named.iter().flat_map(|(_, (_, nodes))| nodes.clone()).collect();
    gran.write_group(vec![StagedWrite { nodes, root }]).unwrap();

    let mut out = format!("format: {FORMAT_VERSION}\n");
    for (name, (ck, _)) in &named {
        let ck = ck.expect("a non-empty tree");
        writeln!(out, "{name}: {}", hex(&gran.frame(&ck).expect("a written node"))).unwrap();
        writeln!(out, "{name}.key: {}", hex(&ck)).unwrap();
    }
    writeln!(out, "root: {}", hex(&gran.root_record().expect("a root record"))).unwrap();
    out
}

#[test]
fn the_frames_are_the_ones_this_format_version_fixed() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/v{FORMAT_VERSION}.txt"));
    let got = corpus();
    if std::env::var_os("GRMPL_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let Ok(want) = std::fs::read_to_string(&path) else {
        panic!("no golden file for format v{FORMAT_VERSION}: bless it with GRMPL_BLESS=1");
    };
    for (want, got) in want.lines().zip(got.lines()) {
        assert_eq!(
            got, want,
            "encoding changed under the same FORMAT_VERSION: bump it (see CLAUDE.md), then bless with GRMPL_BLESS=1"
        );
    }
    assert_eq!(
        got.lines().count(),
        want.lines().count(),
        "encoding changed under the same FORMAT_VERSION: bump it (see CLAUDE.md), then bless with GRMPL_BLESS=1"
    );
}
