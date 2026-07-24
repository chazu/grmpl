//! Cross-domain routing (DESIGN.md §4, §4.2).
//!
//! A `Domain` ties a local store to a `Transport`. A patch's `emit`s are
//! partitioned: messages to a *local* inbox are written straight into the local
//! store; messages to a *remote* inbox are written into a durable **outbox** in
//! the same atomic commit (the durable-outbox pattern). A delivery pass ships
//! outbox rows over the transport (at-least-once) and retracts them on success;
//! the receiving domain drains the transport and applies each message into its
//! own inbox, deduplicating by `(sender, seq)` so redelivery is idempotent —
//! exactly-once apply without a distributed transaction.

use std::collections::HashMap;

use grmpl_core::{
    Authority, Diff, DomainId, Edition, Error, Message, Patch, RelId, Result, SchemaCatalog,
    TraceStore, Tuple, Transport, Value,
};

use crate::commit::{check_schema, CommitOutcome};
use crate::SeqAlloc;

/// Envelope on the wire: `seq(i64, BE) || encoded_message`.
fn encode_envelope(seq: i64, m: &Message) -> Vec<u8> {
    let mut out = seq.to_be_bytes().to_vec();
    out.extend(grmpl_core::wire::encode_message(m));
    out
}

fn decode_envelope(bytes: &[u8]) -> Result<(i64, Message)> {
    if bytes.len() < 8 {
        return Err(Error::Codec("envelope shorter than seq header".into()));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    let seq = i64::from_be_bytes(buf);
    let msg = grmpl_core::wire::decode_message(&bytes[8..])?;
    Ok((seq, msg))
}

/// An authority domain with cross-domain messaging.
pub struct Domain<'a> {
    pub id: DomainId,
    pub store: &'a dyn TraceStore,
    pub transport: &'a dyn Transport,
    /// Which domain owns each inbox relation. An inbox not listed (or owned by
    /// `self`) is local.
    pub routes: HashMap<RelId, DomainId>,
    /// `(seq, target_domain, inbox_rel, body)` — pending outbound messages.
    pub outbox: RelId,
    /// Single-row outbox seq counter `(next_seq)`, allocated through the durable,
    /// race-safe [`SeqAlloc`] (a single global key, so `key = []`).
    pub outseq: RelId,
    /// `(sender_domain, seq)` — inbound messages already applied (dedup).
    pub seen: RelId,
}

impl<'a> Domain<'a> {
    fn is_remote(&self, inbox: RelId) -> Option<DomainId> {
        match self.routes.get(&inbox) {
            Some(d) if *d != self.id => Some(*d),
            _ => None,
        }
    }

    /// Do all of `preconds` still hold (positive weight) at the current edition?
    /// Used to tell a lost outbox-seq race (retry) from a genuine caller
    /// precondition failure (`Rejected`).
    fn all_hold(&self, preconds: &[(RelId, Tuple)]) -> Result<bool> {
        let at = self.store.current();
        for (rel, tuple) in preconds {
            let held =
                self.store.read_at(*rel, at)?.into_iter().any(|(t, d)| d > 0 && &t == tuple);
            if !held {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Seed the outbox seq counter once, on an un-raced path (domain setup,
    /// before any concurrent [`commit`](Self::commit)). The very first outbox
    /// allocation has no counter row to precondition on; seeding here makes every
    /// later allocation fully race-safe. Idempotent — a no-op once the counter
    /// exists.
    pub fn seed_outseq(&self) -> Result<()> {
        let alloc = SeqAlloc::read(self.store, self.outseq, Vec::new())?;
        let patch = alloc.seed(Patch::new());
        if !patch.asserts.is_empty() {
            let updates: Vec<(RelId, Tuple, Diff)> =
                patch.asserts.iter().map(|f| (f.rel, f.tuple.clone(), 1)).collect();
            self.store.commit(&updates)?;
        }
        Ok(())
    }

    fn seen_holds(&self, sender: DomainId, seq: i64) -> Result<bool> {
        let at = self.store.current();
        let key = Tuple::from([Value::Int(sender.0 as i64), Value::Int(seq)]);
        Ok(self
            .store
            .read_at(self.seen, at)?
            .into_iter()
            .any(|(t, d)| d > 0 && t == key))
    }

    /// Commit a patch, routing remote emits into the durable outbox atomically.
    /// Enforces `schemas` at the commit boundary (pass [`grmpl_core::NoSchemas`]
    /// to opt out of arity/type checking).
    pub fn commit(
        &self,
        patch: &Patch,
        authority: &Authority,
        schemas: &dyn SchemaCatalog,
    ) -> Result<CommitOutcome> {
        // Scheduled timers land as durable world writes in this same commit.
        let sched: Vec<grmpl_core::Fact> = patch
            .scheduled
            .iter()
            .map(|s| grmpl_core::Fact::new(s.timers, crate::schedule::timer_row(s)))
            .collect();

        // Authority law: world writes must be owned.
        for f in patch.asserts.iter().chain(patch.retracts.iter()).chain(sched.iter()) {
            if !authority.permits(f) {
                return Err(Error::Authority(format!(
                    "write to relation {:?} outside authority domain {:?}",
                    f.rel, authority.domain
                )));
            }
        }

        // Schema law (P1): world writes must conform to their registered schema.
        check_schema(
            schemas,
            patch.asserts.iter().chain(patch.retracts.iter()).chain(sched.iter()),
        )?;

        let base_preconditions: Vec<(RelId, Tuple)> =
            patch.preconditions.iter().map(|f| (f.rel, f.tuple.clone())).collect();

        // Seq-independent effects: world writes, the cursor move, timers, and any
        // *local* emits all ride every commit attempt unchanged.
        let mut base_updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for f in &patch.asserts {
            base_updates.push((f.rel, f.tuple.clone(), 1));
        }
        for f in &patch.retracts {
            base_updates.push((f.rel, f.tuple.clone(), -1));
        }
        if let Some(cm) = &patch.cursor_advance {
            if let Some(old) = &cm.retract {
                base_updates.push((cm.rel, old.clone(), -1));
            }
            base_updates.push((cm.rel, cm.assert.clone(), 1));
        }
        for f in &sched {
            base_updates.push((f.rel, f.tuple.clone(), 1));
        }

        // Partition emits: local inbox writes ride the base updates; remote emits
        // become durable outbox rows whose seqs come from the guarded `SeqAlloc`.
        let mut remotes: Vec<(DomainId, RelId, Tuple)> = Vec::new();
        for m in &patch.emits {
            match self.is_remote(m.inbox) {
                None => base_updates.push((m.inbox, m.body.clone(), 1)), // local
                Some(target) => remotes.push((target, m.inbox, m.body.clone())),
            }
        }

        // No remote emit: no outbox seq to allocate — commit exactly as before,
        // in one atomic `commit_if`.
        if remotes.is_empty() {
            return Ok(match self.store.commit_if(&base_preconditions, &base_updates)? {
                Some(e) => CommitOutcome::Committed(e),
                None => CommitOutcome::Rejected,
            });
        }

        // Remote emits: draw outbox seqs from the durable, race-safe `SeqAlloc`.
        // A commit that loses only the seq race retries against the winner's
        // counter (so the caller's world writes are never dropped by contention);
        // a genuine caller precondition failure still surfaces as `Rejected`
        // without spinning.
        for _ in 0..64 {
            let mut alloc = SeqAlloc::read(self.store, self.outseq, Vec::new())?;
            let mut preconditions = base_preconditions.clone();
            let mut updates = base_updates.clone();
            for (target, inbox, body) in &remotes {
                let seq = alloc.fresh();
                let row = Tuple::from([
                    Value::Int(seq),
                    Value::Int(target.0 as i64),
                    Value::Int(inbox.0 as i64),
                    Value::Tuple(body.0.clone()),
                ]);
                updates.push((self.outbox, row, 1));
            }
            // Fold the guarded counter advance (precondition + retract + assert)
            // in, so the outbox rows and their counter bump commit atomically.
            let seq_patch = alloc.seal(Patch::new());
            for f in &seq_patch.preconditions {
                preconditions.push((f.rel, f.tuple.clone()));
            }
            for f in &seq_patch.retracts {
                updates.push((f.rel, f.tuple.clone(), -1));
            }
            for f in &seq_patch.asserts {
                updates.push((f.rel, f.tuple.clone(), 1));
            }

            match self.store.commit_if(&preconditions, &updates)? {
                Some(e) => return Ok(CommitOutcome::Committed(e)),
                None => {
                    // A lost seq race leaves the caller's preconditions intact;
                    // retry. A caller `expect` that no longer holds is a real
                    // rejection — return it (matching the pre-swap contract).
                    if self.all_hold(&base_preconditions)? {
                        continue;
                    }
                    return Ok(CommitOutcome::Rejected);
                }
            }
        }
        // Contention did not settle within the cap; the caller retries.
        Ok(CommitOutcome::Rejected)
    }

    /// Ship every pending outbox message over the transport, retracting each on
    /// success. At-least-once: a crash after send but before retract redelivers.
    /// Returns the number delivered.
    pub fn flush_outbox(&self) -> Result<usize> {
        let at = self.store.current();
        let rows = self.store.read_at(self.outbox, at)?;
        let mut n = 0;
        for (row, d) in rows {
            if d <= 0 {
                continue;
            }
            let s = row.as_slice();
            let (seq, target, inbox, body) = match (s.first(), s.get(1), s.get(2), s.get(3)) {
                (Some(Value::Int(seq)), Some(Value::Int(t)), Some(Value::Int(ib)), Some(Value::Tuple(b))) => {
                    (*seq, DomainId(*t as u64), RelId(*ib as u32), Tuple(b.clone()))
                }
                _ => return Err(Error::Codec("malformed outbox row".into())),
            };
            let envelope = encode_envelope(seq, &Message { inbox, body });
            self.transport.send(target, &envelope)?;
            // Delivered: remove from the outbox.
            self.store.commit(&[(self.outbox, row, -1)])?;
            n += 1;
        }
        Ok(n)
    }

    /// Drain the transport, applying each message into the local inbox with
    /// dedup by `(sender, seq)`. Returns the number of newly-applied messages.
    pub fn receive(&self) -> Result<usize> {
        let mut n = 0;
        while let Some((sender, payload)) = self.transport.poll()? {
            let (seq, msg) = decode_envelope(&payload)?;
            if self.seen_holds(sender, seq)? {
                continue; // already applied — idempotent
            }
            self.store.commit(&[
                (msg.inbox, msg.body, 1),
                (self.seen, Tuple::from([Value::Int(sender.0 as i64), Value::Int(seq)]), 1),
            ])?;
            n += 1;
        }
        Ok(n)
    }
}

/// A pending outbox count helper (for tests/observability).
pub fn outbox_len(store: &dyn TraceStore, outbox: RelId, at: Edition) -> Result<usize> {
    Ok(store.read_at(outbox, at)?.into_iter().filter(|(_, d)| *d > 0).count())
}
