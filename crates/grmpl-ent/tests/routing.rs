//! **G-4 acceptance: the reactive pump is routed, not re-evaluated.**
//!
//! The canopy's whole purpose is that a change reaches only the watchers it
//! could concern — grmpl's Attention law, and Xanadu's sensor canopy. But the
//! pump used to run `eval_delta` over its view on *every* pump, whatever the
//! commit touched, so with N installed watches one unrelated commit cost N full
//! differential evaluations.
//!
//! The substrate now answers the routing question directly. `touched_since` is
//! **conservative** — `false` is a proof of no change, `true` merely means
//! "possibly" — so the default (`true`) leaves every store correct, and only a
//! store that can *prove* a negative saves the work. The Ent proves it from the
//! Edition enfilade's cached subtree measures, in `O(log n)`, materializing
//! nothing.
//!
//! These tests wrap the store in a tally of the calls `eval_delta` makes, so the
//! saving is asserted rather than asserted-in-prose.

use std::sync::Mutex;

use grmpl_core::{
    Authority, Diff, DomainId, Edition, EditionStore, Entity, NoSchemas, RelId, Result, Scope,
    TraceStore, Tuple, Update, Value,
};
use grmpl_diff::Query;
use grmpl_ent::EntStore;
use grmpl_proc::OnWatch;

const WATCHED: RelId = RelId(30);
const UNWATCHED: RelId = RelId(31);
const INBOX: RelId = RelId(40);
const CURSOR: RelId = RelId(41);
const SEQS: RelId = RelId(42);

/// A store that delegates to the Ent but tallies the `scan_updates` calls the
/// differential evaluator makes — the witness that a routed-away pump does no
/// differential work at all.
struct Tally {
    inner: EntStore,
    scans: Mutex<usize>,
}

impl Tally {
    fn scans(&self) -> usize {
        *self.scans.lock().unwrap()
    }
    fn reset(&self) {
        *self.scans.lock().unwrap() = 0;
    }
}

impl EditionStore for Tally {
    fn current(&self) -> Edition {
        self.inner.current()
    }
}

impl TraceStore for Tally {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        self.inner.commit(updates)
    }
    fn commit_if(
        &self,
        pre: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        self.inner.commit_if(pre, updates)
    }
    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        self.inner.read_at(rel, at)
    }
    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        *self.scans.lock().unwrap() += 1;
        self.inner.scan_updates(rel, from, to)
    }
    fn read_range(&self, rel: RelId, at: Edition, lo: &Tuple, hi: &Tuple) -> Result<Vec<(Tuple, Diff)>> {
        self.inner.read_range(rel, at, lo, hi)
    }
    fn touched_since(&self, from: Edition, to: Edition, rels: &[RelId]) -> Result<bool> {
        self.inner.touched_since(from, to, rels)
    }
    fn watermark(&self) -> Edition {
        self.inner.watermark()
    }
    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        self.inner.consolidate(up_to)
    }
}

fn watch(n: u64) -> OnWatch {
    OnWatch {
        view: Query::rel(WATCHED).project([0]).distinct(),
        watch: Entity(1000 + n),
        target: Entity(2000 + n),
        inbox: INBOX,
        cursor_rel: CURSOR,
        seqs: SEQS,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(INBOX), Scope::whole(CURSOR), Scope::whole(SEQS)],
        ),
    }
}

fn churn(store: &dyn TraceStore, rel: RelId, a: i64) {
    store.commit(&[(rel, Tuple::from([Value::Int(a)]), 1)]).unwrap();
}

/// A commit to a relation no watch reads must cost **no** differential work,
/// however many watches are installed.
#[test]
fn a_commit_no_watch_reads_evaluates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Tally { inner: EntStore::open(dir.path()).unwrap(), scans: Mutex::new(0) };

    const WATCHES: u64 = 50;
    let watches: Vec<OnWatch> = (0..WATCHES).map(watch).collect();
    for w in &watches {
        w.install(&store, &NoSchemas).unwrap();
    }

    // Move a relation none of them watches.
    churn(&store, UNWATCHED, 1);
    store.reset();
    for w in &watches {
        assert_eq!(w.pump(&store, &NoSchemas).unwrap(), 0, "nothing should be delivered");
    }
    assert_eq!(
        store.scans(),
        0,
        "{WATCHES} watches did differential work for a commit to a relation none of them reads"
    );
}

/// …and a commit a watch *does* read is still delivered in full. Routing may
/// only ever save work, never lose a delta.
#[test]
fn a_commit_a_watch_reads_is_still_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let store = Tally { inner: EntStore::open(dir.path()).unwrap(), scans: Mutex::new(0) };
    let w = watch(0);
    w.install(&store, &NoSchemas).unwrap();

    churn(&store, UNWATCHED, 1);
    assert_eq!(w.pump(&store, &NoSchemas).unwrap(), 0);

    churn(&store, WATCHED, 7);
    assert_eq!(w.pump(&store, &NoSchemas).unwrap(), 1, "a real view change must be delivered");

    // The delivered activation is the row that changed.
    let rows = store.read_at(INBOX, store.current()).unwrap();
    assert_eq!(rows.len(), 1);

    // Routing is per-interval, not one-shot: a later unrelated commit is routed
    // away again, and a later relevant one still gets through.
    churn(&store, UNWATCHED, 2);
    assert_eq!(w.pump(&store, &NoSchemas).unwrap(), 0);
    churn(&store, WATCHED, 8);
    assert_eq!(w.pump(&store, &NoSchemas).unwrap(), 1);
}

/// The routing primitive itself: `false` only ever when the span is genuinely
/// empty for those relations, checked against the updates the store actually
/// holds, over every span.
#[test]
fn touched_since_never_reports_a_false_negative() {
    let dir = tempfile::tempdir().unwrap();
    let store = EntStore::open(dir.path()).unwrap();

    // Interleave commits across two relations so spans differ per relation.
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    for k in 0..40i64 {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        let rel = if rng.is_multiple_of(3) { UNWATCHED } else { WATCHED };
        churn(&store, rel, k);
    }

    let cur = store.current().0;
    for from in 0..=cur {
        for to in from..=cur {
            for rels in [vec![WATCHED], vec![UNWATCHED], vec![WATCHED, UNWATCHED]] {
                let said = store.touched_since(Edition(from), Edition(to), &rels).unwrap();
                // Ground truth: did any of those relations actually change?
                let truth = rels.iter().any(|r| {
                    !store.scan_updates(*r, Edition(from), Edition(to)).unwrap().is_empty()
                });
                assert!(
                    said || !truth,
                    "false negative: touched_since({from},{to},{rels:?}) said no, but updates exist"
                );
                // And it should be exact here, not merely conservative — the Ent
                // answers from the Edition enfilade's measure.
                assert_eq!(said, truth, "touched_since({from},{to},{rels:?}) was not exact");
            }
        }
    }
}
