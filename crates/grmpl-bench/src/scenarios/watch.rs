//! **Watch fan-out.** What does a change cost when many maintained views are
//! watching?
//!
//! v1 watch is poll-driven and each [`DeltaStream`] re-evaluates its own
//! `eval_delta` over the interval independently — there is *no* shared
//! arrangement between streams yet (the push-driven subscription over shared
//! arrangements is the deferred optimization noted in `grmpl-diff::watch`). So
//! the headline number here is deliberately the pessimistic one: total delta
//! cost grows ~linearly in the number of watchers. Measuring it is the point —
//! it is the baseline the P13 engine-statefulness work must beat.
//!
//! A second sample times the reactive proc path: one [`OnWatch::pump`]
//! materializing a batch of deltas as inbox messages in a single atomic commit.

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, RelId, Scope, TraceStore, Tuple, Value, NoSchemas,
};
use grmpl_diff::Query;
use grmpl_proc::OnWatch;

use crate::harness::{measure, Report, Sample};
use crate::scenarios::open_store;

const BASE: RelId = RelId(10);
const INBOX: RelId = RelId(11);
const CURSOR: RelId = RelId(12);
const SEQS: RelId = RelId(13);
const WATCH: Entity = Entity(1);
const TARGET: Entity = Entity(2);

fn row(a: i64, b: i64) -> Tuple {
    Tuple::from([Value::Int(a), Value::Int(b)])
}

/// The watched view: distinct `a` present in `BASE`. `project` + `distinct` puts
/// a non-linear boundary in the plan, so deltas are genuine (not a passthrough).
fn view() -> Query {
    Query::rel(BASE).project([0]).distinct()
}

/// Fan `deltas` fresh changes out to `watchers` independent `DeltaStream`s and
/// time the aggregate poll that delivers the batch to every watcher.
pub fn deltastream_fanout(watchers: u64, deltas: u64, base_rows: u64) -> Sample {
    let (_d, store) = open_store();
    // Pre-existing world so `eval_delta` is not trivial.
    for a in 0..base_rows as i64 {
        store.commit(&[(BASE, row(a, a), 1)]).unwrap();
    }
    let from = store.current();

    // Install every watcher at `from` and prime it (first poll = snapshot).
    let mut streams: Vec<_> = (0..watchers).map(|_| view().watch(&store, from)).collect();
    for s in &mut streams {
        s.poll().unwrap();
    }

    // The world moves: `deltas` new `a` values (each a real view change).
    for i in 0..deltas as i64 {
        store.commit(&[(BASE, row(base_rows as i64 + i, i), 1)]).unwrap();
    }

    // Fan-out: every watcher independently computes and delivers the batch.
    let mut delivered = 0u64;
    let s = measure(format!("deltastream x{watchers} watchers"), watchers, || {
        for st in &mut streams {
            delivered += st.poll().unwrap().len() as u64;
        }
    });
    let per_watcher_ns = s.ns_per_op();
    s.note("deltas", deltas as f64)
        .note("delivered", delivered as f64)
        .note("ns/watcher", per_watcher_ns)
}

/// One `OnWatch::pump` delivering `deltas` activations as a single atomic
/// commit — the reactive proc path (durable cursor + inbox seqs).
pub fn onwatch_pump(deltas: u64, base_rows: u64) -> Sample {
    let (_d, store) = open_store();
    for a in 0..base_rows as i64 {
        store.commit(&[(BASE, row(a, a), 1)]).unwrap();
    }
    let ow = OnWatch {
        view: view(),
        watch: WATCH,
        target: TARGET,
        inbox: INBOX,
        cursor_rel: CURSOR,
        seqs: SEQS,
        authority: Authority::new(
            DomainId(1),
            vec![Scope::whole(INBOX), Scope::whole(CURSOR), Scope::whole(SEQS)],
        ),
    };
    ow.install(&store, &NoSchemas).unwrap();
    // Accumulate a batch of view changes for one pump to deliver at once.
    for i in 0..deltas as i64 {
        store.commit(&[(BASE, row(base_rows as i64 + i, i), 1)]).unwrap();
    }
    let mut n = 0usize;
    let s = measure("onwatch pump (1 batch commit)", deltas, || {
        n = ow.pump(&store, &NoSchemas).unwrap();
    });
    s.note("activations", n as f64)
}

/// Watch fan-out across a range of watcher counts, plus the reactive path.
pub fn report(deltas: u64, base_rows: u64) -> Report {
    let mut rep = Report::new("Watch fan-out (independent v1 streams — no shared arrangement)");
    for &w in &[1u64, 8, 64, 256] {
        rep.push(deltastream_fanout(w, deltas, base_rows));
    }
    rep.push(onwatch_pump(deltas, base_rows));
    rep
}
