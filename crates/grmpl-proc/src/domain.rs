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
    Authority, Diff, DomainId, Edition, Error, Message, Patch, RelId, Result, TraceStore, Tuple,
    Transport, Value,
};

use crate::commit::CommitOutcome;

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
    /// Single-row counter `(next_seq)`.
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

    fn read_outseq(&self) -> Result<i64> {
        let at = self.store.current();
        for (t, d) in self.store.read_at(self.outseq, at)? {
            if d > 0 {
                if let Some(Value::Int(n)) = t.as_slice().first() {
                    return Ok(*n);
                }
            }
        }
        Ok(0)
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
    pub fn commit(&self, patch: &Patch, authority: &Authority) -> Result<CommitOutcome> {
        // Authority law: world writes must be owned.
        for f in patch.asserts.iter().chain(patch.retracts.iter()) {
            if !authority.permits(f) {
                return Err(Error::Authority(format!(
                    "write to relation {:?} outside authority domain {:?}",
                    f.rel, authority.domain
                )));
            }
        }

        let preconditions: Vec<(RelId, Tuple)> =
            patch.preconditions.iter().map(|f| (f.rel, f.tuple.clone())).collect();

        let mut updates: Vec<(RelId, Tuple, Diff)> = Vec::new();
        for f in &patch.asserts {
            updates.push((f.rel, f.tuple.clone(), 1));
        }
        for f in &patch.retracts {
            updates.push((f.rel, f.tuple.clone(), -1));
        }
        if let Some(cm) = &patch.cursor_advance {
            if let Some(old) = &cm.retract {
                updates.push((cm.rel, old.clone(), -1));
            }
            updates.push((cm.rel, cm.assert.clone(), 1));
        }

        // Partition emits into local inbox writes and durable outbox rows.
        let start_seq = self.read_outseq()?;
        let mut next_seq = start_seq;
        for m in &patch.emits {
            match self.is_remote(m.inbox) {
                None => updates.push((m.inbox, m.body.clone(), 1)), // local
                Some(target) => {
                    let row = Tuple::from([
                        Value::Int(next_seq),
                        Value::Int(target.0 as i64),
                        Value::Int(m.inbox.0 as i64),
                        Value::Tuple(m.body.0.clone()),
                    ]);
                    updates.push((self.outbox, row, 1));
                    next_seq += 1;
                }
            }
        }
        if next_seq != start_seq {
            if start_seq > 0 {
                updates.push((self.outseq, Tuple::from([Value::Int(start_seq)]), -1));
            }
            updates.push((self.outseq, Tuple::from([Value::Int(next_seq)]), 1));
        }

        match self.store.commit_if(&preconditions, &updates)? {
            Some(e) => Ok(CommitOutcome::Committed(e)),
            None => Ok(CommitOutcome::Rejected),
        }
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
