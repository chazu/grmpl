//! `DeltaStream` — a maintained query (DESIGN.md §2.3 #2, §5.1).
//!
//! The Snapshot–stream law: the first `poll` returns `find(query)` at the
//! registration edition (as positive weights); each subsequent `poll` returns
//! the consolidated signed change up to the store's current edition. Therefore
//! `initial + Σ deltas = find(current)`, with no missing interval — registration
//! pins the cursor before the first read.
//!
//! v1 is poll-driven; a push-driven subscription over shared arrangements is a
//! later optimization behind the same law.

use grmpl_core::{Diff, Edition, Result, TraceStore, Tuple};

use crate::multiset;
use crate::query::{eval_delta, eval_snapshot, Query};

pub struct DeltaStream<'a> {
    query: Query,
    store: &'a dyn TraceStore,
    cursor: Edition,
    started: bool,
}

impl<'a> DeltaStream<'a> {
    pub fn new(query: Query, store: &'a dyn TraceStore, from: Edition) -> DeltaStream<'a> {
        DeltaStream { query, store, cursor: from, started: false }
    }

    /// The edition through which this stream has delivered.
    pub fn edition(&self) -> Edition {
        self.cursor
    }

    /// Deliver the next batch: the initial snapshot on the first call, then
    /// consolidated deltas up to the current edition. Empty if nothing changed.
    pub fn poll(&mut self) -> Result<Vec<(Tuple, Diff)>> {
        if !self.started {
            self.started = true;
            let snap = eval_snapshot(&self.query, self.store, self.cursor)?;
            return Ok(multiset::to_sorted_vec(&snap));
        }
        let to = self.store.current();
        if to == self.cursor {
            return Ok(Vec::new());
        }
        let delta = eval_delta(&self.query, self.store, self.cursor, to)?;
        self.cursor = to;
        Ok(multiset::to_sorted_vec(&delta))
    }
}

impl Query {
    /// Install this query as a maintained view starting from `from`.
    pub fn watch<'a>(&self, store: &'a dyn TraceStore, from: Edition) -> DeltaStream<'a> {
        DeltaStream::new(self.clone(), store, from)
    }
}
