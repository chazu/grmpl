//! **Churn throughput.** How fast can the domain absorb committed changes?
//!
//! Two paths, same workload (fresh rows into one relation):
//!
//! * `raw commit` — [`TraceStore::commit`] straight to the store: the write
//!   floor every higher path pays (edition-lock + fjall batch + persist).
//! * `commit_patch` — the [`grmpl_proc::commit_patch`] path with an [`Authority`]
//!   check and [`NoSchemas`]: the overhead the process layer adds above the
//!   store per commit.
//!
//! Batch size (`facts_per_commit`) is the knob: it separates *per-commit* fixed
//! cost (the edition lock, the persist) from *per-fact* cost (encode + insert),
//! so a reader can see where churn time actually goes.

use grmpl_core::{
    Authority, DomainId, Entity, Fact, NoSchemas, Patch, RelId, Scope, TraceStore, Tuple, Value,
};
use grmpl_proc::{commit_patch, CommitOutcome};

use crate::harness::{measure, warmup, Report, Sample};
use crate::scenarios::{open_store, per_sec};

const R: RelId = RelId(1);

fn fresh_row(key: i64) -> Tuple {
    Tuple::from([Value::Ent(Entity(key as u64)), Value::Int(key)])
}

/// Raw `store.commit` churn: `commits` commits of `facts_per_commit` fresh rows.
pub fn raw_commit(store: &dyn TraceStore, commits: u64, facts_per_commit: u64) -> Sample {
    let mut key: i64 = 0;
    let total_facts = commits * facts_per_commit;
    let s = measure(format!("raw commit  ({facts_per_commit}/patch)"), commits, || {
        for _ in 0..commits {
            let mut updates = Vec::with_capacity(facts_per_commit as usize);
            for _ in 0..facts_per_commit {
                key += 1;
                updates.push((R, fresh_row(key), 1));
            }
            store.commit(&updates).expect("commit");
        }
    });
    let facts_s = per_sec(total_facts, s.elapsed.as_secs_f64());
    s.note("facts", total_facts as f64).note("facts/s", facts_s)
}

/// `commit_patch` churn: same workload through the process layer's authority +
/// schema commit boundary.
pub fn patch_commit(store: &dyn TraceStore, commits: u64, facts_per_commit: u64) -> Sample {
    let authority = Authority::new(DomainId(1), vec![Scope::whole(R)]);
    let mut key: i64 = 0;
    let total_facts = commits * facts_per_commit;
    let s = measure(format!("commit_patch ({facts_per_commit}/patch)"), commits, || {
        for _ in 0..commits {
            let mut patch = Patch::new();
            for _ in 0..facts_per_commit {
                key += 1;
                patch = patch.assert(Fact::new(R, fresh_row(key)));
            }
            match commit_patch(store, &NoSchemas, &patch, &authority).expect("commit_patch") {
                CommitOutcome::Committed(_) => {}
                CommitOutcome::Rejected => panic!("unconditional patch rejected"),
            }
        }
    });
    let facts_s = per_sec(total_facts, s.elapsed.as_secs_f64());
    s.note("facts", total_facts as f64).note("facts/s", facts_s)
}

/// Churn throughput at a few batch sizes on both paths.
pub fn report(commits: u64) -> Report {
    let mut rep = Report::new("Churn throughput (fresh rows, real fjall store)");
    for &batch in &[1u64, 16, 256] {
        let (_d, store) = open_store();
        // Warm the keyspace handle + page cache so the first timed commit does
        // not pay one-time setup.
        warmup(1, || {
            store.commit(&[(R, fresh_row(-1), 1)]).unwrap();
        });
        rep.push(raw_commit(&store, commits, batch));
    }
    for &batch in &[1u64, 16, 256] {
        let (_d, store) = open_store();
        warmup(1, || {
            store.commit(&[(R, fresh_row(-1), 1)]).unwrap();
        });
        rep.push(patch_commit(&store, commits, batch));
    }
    rep
}
