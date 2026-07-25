//! **The ent-native store (E1a, in-memory).**
//!
//! The first realization of "the Ent *is* the store" (plan §1), built from the
//! [`Tree`] enfilade primitive, over two coordinated enfilades per relation:
//!
//! * the **Fact enfilade** — net-per-tuple state, tuple-keyed, *versioned by
//!   edition* (one persistent root per edition, sharing structure with its
//!   predecessor), so `read_at(rel, at)` is a root lookup + in-order walk (MVCC by
//!   root); zero-weight tuples are absent.
//! * the **Edition enfilade** — the raw commit-order delta log, keyed by
//!   `(edition, submit_index)` with the submit index as immutable payload, so
//!   `scan_updates` returns the raw, per-multiplicity updates in exact submit
//!   order (plan v4.1 CRITICAL-2/MAJOR fixes).
//!
//! It implements the `grmpl-core` store traits, so the whole semantic core runs
//! over it unchanged. This in-memory cut validates the architecture against
//! `FjallStore` as an oracle (see the conformance test); granfilade persistence
//! (content-interned nodes on fjall) is the next increment.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use grmpl_core::{
    Diff, Edition, EditionStore, Error, RelId, Result, Time, TraceStore, Tuple, Update,
};

use crate::measure::Count;
use crate::tree::Tree;

/// The Fact enfilade: `tuple → net Σdiff` (nonzero only).
type FactTree = Tree<Tuple, Diff, Count>;
/// The Edition enfilade: `(edition, submit_index) → (tuple, diff)` raw log.
type LogTree = Tree<(u64, u64), (Tuple, Diff), Count>;

/// An in-memory ent store: a family of per-relation Fact + Edition enfilades
/// behind one commit clock (single-writer, like the domain's edition lock).
pub struct EntStore {
    inner: Mutex<Inner>,
}

struct Inner {
    current: u64,
    watermark: u64,
    /// Fact enfilade per rel: `edition → net-state root` (versioned for as-of).
    fact: HashMap<RelId, BTreeMap<u64, FactTree>>,
    /// Edition enfilade per rel: the raw commit-order log.
    log: HashMap<RelId, LogTree>,
}

impl Default for EntStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EntStore {
    pub fn new() -> EntStore {
        EntStore {
            inner: Mutex::new(Inner {
                current: 0,
                watermark: 0,
                fact: HashMap::new(),
                log: HashMap::new(),
            }),
        }
    }
}

impl Inner {
    /// Apply `updates` as edition `e` (= current + 1): append each to the Edition
    /// enfilade in submit order and fold each into a fresh Fact root. A later
    /// update to the same rel in the batch builds on the root just produced at `e`
    /// (so repeated tuples net correctly); a tuple reaching net 0 is removed.
    fn apply(&mut self, e: u64, updates: &[(RelId, Tuple, Diff)]) {
        for (i, (rel, tuple, diff)) in updates.iter().enumerate() {
            let log = self.log.entry(*rel).or_default();
            *log = log.insert((e, i as u64), (tuple.clone(), *diff));

            let facts = self.fact.entry(*rel).or_default();
            let base = facts.range(..=e).next_back().map(|(_, t)| t.clone()).unwrap_or_default();
            let cur = base.get(tuple).copied().unwrap_or(0);
            let net = cur + *diff;
            let root = if net == 0 { base.remove(tuple) } else { base.insert(tuple.clone(), net) };
            facts.insert(e, root);
        }
        self.current = e;
    }

    /// Net weight of `tuple` in `rel` as-of `current` > 0.
    fn holds_now(&self, rel: RelId, tuple: &Tuple) -> bool {
        self.fact
            .get(&rel)
            .and_then(|f| f.range(..=self.current).next_back())
            .and_then(|(_, t)| t.get(tuple))
            .is_some_and(|n| *n > 0)
    }
}

impl EditionStore for EntStore {
    fn current(&self) -> Edition {
        Edition(self.inner.lock().unwrap().current)
    }
}

impl TraceStore for EntStore {
    fn commit(&self, updates: &[(RelId, Tuple, Diff)]) -> Result<Edition> {
        let mut inner = self.inner.lock().unwrap();
        let e = inner.current + 1;
        inner.apply(e, updates);
        Ok(Edition(e))
    }

    fn commit_if(
        &self,
        preconditions: &[(RelId, Tuple)],
        updates: &[(RelId, Tuple, Diff)],
    ) -> Result<Option<Edition>> {
        let mut inner = self.inner.lock().unwrap();
        for (rel, tuple) in preconditions {
            if !inner.holds_now(*rel, tuple) {
                return Ok(None);
            }
        }
        let e = inner.current + 1;
        inner.apply(e, updates);
        Ok(Some(Edition(e)))
    }

    fn read_at(&self, rel: RelId, at: Edition) -> Result<Vec<(Tuple, Diff)>> {
        let inner = self.inner.lock().unwrap();
        if at.0 < inner.watermark {
            return Err(door("read_at", at.0, inner.watermark));
        }
        // The Fact root as-of `at` is the latest one at an edition ≤ `at`; its
        // in-order walk is tuple-sorted and holds only nonzero net weights.
        let rows = inner
            .fact
            .get(&rel)
            .and_then(|f| f.range(..=at.0).next_back())
            .map(|(_, t)| t.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        Ok(rows)
    }

    fn scan_updates(&self, rel: RelId, from: Edition, to: Edition) -> Result<Vec<Update>> {
        let inner = self.inner.lock().unwrap();
        if from.0 < inner.watermark {
            return Err(door("scan_updates", from.0, inner.watermark));
        }
        let mut out = Vec::new();
        if let Some(log) = inner.log.get(&rel) {
            // Editions in (from, to] = keys in [(from+1, 0), (to+1, 0)); the tree
            // yields them in (edition, submit_index) order — the raw commit order.
            for ((edition, _submit), (tuple, diff)) in log.range_collect(&(from.0 + 1, 0), &(to.0 + 1, 0)) {
                out.push(Update { tuple, time: Time::input(edition), diff });
            }
        }
        Ok(out)
    }

    fn watermark(&self) -> Edition {
        Edition(self.inner.lock().unwrap().watermark)
    }

    fn consolidate(&self, up_to: Edition) -> Result<Edition> {
        let mut inner = self.inner.lock().unwrap();
        let new_wm = up_to.0.min(inner.current);
        if new_wm <= inner.watermark {
            return Ok(Edition(inner.watermark));
        }
        // Fact: keep the consolidated state as a checkpoint root at `new_wm`, plus
        // every root strictly after it; drop the rest (their as-of state is gone).
        for facts in inner.fact.values_mut() {
            let checkpoint = facts.range(..=new_wm).next_back().map(|(_, t)| t.clone());
            let mut next: BTreeMap<u64, FactTree> = BTreeMap::new();
            if let Some(t) = checkpoint {
                next.insert(new_wm, t);
            }
            for (e, t) in facts.range((new_wm + 1)..) {
                next.insert(*e, t.clone());
            }
            *facts = next;
        }
        // Edition log: drop every update at an edition ≤ `new_wm` (its raw history
        // is discarded); keep the tail.
        for log in inner.log.values_mut() {
            let tail = log.range_collect(&(new_wm + 1, 0), &(u64::MAX, u64::MAX));
            let mut next = LogTree::new();
            for (k, v) in tail {
                next = next.insert(k, v);
            }
            *log = next;
        }
        inner.watermark = new_wm;
        Ok(Edition(new_wm))
    }
}

/// The edition-door error: a read/scan below the consolidation watermark is
/// answered loudly rather than from truncated history.
fn door(op: &str, at: u64, watermark: u64) -> Error {
    Error::Store(format!("{op} at edition {at} below watermark {watermark}"))
}
