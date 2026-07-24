//! Reactive push of on-watch activations to a session client (P5 follow-on).
//!
//! P5 (`grmpl_proc::OnWatch`, TKT-77) made a reactive handler a *durable
//! materialization*: the pump reads the signed deltas a maintained view has
//! produced since a durable watch-cursor and, in **one atomic commit**, appends
//! each delta as an inbox message. This module closes the loop to a connected
//! client. A [`Subscription`] pumps its view and then **drains** the resulting
//! inbox to the socket — exactly as [`Session`](crate::Session) drains `TELL`
//! text — but over a *durable* delivery cursor, so a reconnecting client resumes
//! from where it left off with no loss and no re-delivery.
//!
//! Two laws hold (asserted by the seeded law-oracle in `tests/reactive_push.rs`):
//!
//! * **Exactly-once, in-order delivery.** The sequence streamed to the socket
//!   equals the sequence of inbox messages the pump materialized — in inbox-seq
//!   order, which *is* the pump's commit order `(edition, counter)`, never the
//!   store's physical scan order — with no loss and no duplication.
//! * **Durable resume.** The drain is a **prefix-consume** of the durable inbox
//!   guarded by a durable [`WATCH_DELIVERY`] cursor. After a disconnect +
//!   reconnect a fresh `Subscription` over the same store resumes at the cursor:
//!   already-drained activations are not re-delivered and none are skipped.
//!
//! Reactivity is still *pump then drain*, never a callback: the pump only moves
//! deltas into the inbox, and the drain only moves a durable prefix of the inbox
//! to the socket. Neither runs a behavior, so both are replay-deterministic
//! functions of committed data.

use std::io::Write;

use grmpl_core::{
    CursorMove, Edition, Entity, Error, Fact, NoSchemas, Patch, Result, TraceStore, Tuple, Value,
};
use grmpl_diff::Query;
use grmpl_proc::{commit_patch, decode_activation, CommitOutcome, OnWatch};

use crate::world::{watch_authority, WATCH_CURSOR, WATCH_DELIVERY, WATCH_INBOX, WATCH_SEQS};

/// One activation handed to the socket: the signed view delta `diff` on `row`,
/// at its durable inbox position `seq`. The full Z-weight `diff` is preserved
/// (a consolidated delta may have magnitude `> 1` or be negative).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delivered {
    pub seq: i64,
    pub diff: i64,
    pub row: Tuple,
}

impl Delivered {
    /// The line streamed to the socket for this activation: `"{seq} {diff}
    /// {cells…}"`, cells space-joined. A stable, whitespace-delimited framing a
    /// line client (or a test) can parse back without a codec.
    pub fn render(&self) -> String {
        let mut s = format!("{} {}", self.seq, self.diff);
        for cell in self.row.as_slice() {
            s.push(' ');
            match cell {
                Value::Int(n) => s.push_str(&n.to_string()),
                Value::Text(t) => s.push_str(t),
                Value::Ent(e) => s.push_str(&format!("e{}", e.0)),
                other => s.push_str(&format!("{other:?}")),
            }
        }
        s
    }
}

/// A client's live view subscription: an [`OnWatch`] pump plus the durable
/// delivery cursor that tracks how much of its inbox the socket has consumed.
///
/// The [`watch`](OnWatch::watch) entity is the *stable identity* of the
/// subscription — its on-watch cursor and its delivery cursor are both keyed by
/// it — so the same `watch` across a reconnect resumes the same durable stream.
pub struct Subscription {
    on_watch: OnWatch,
}

impl Subscription {
    /// A subscription for `player` (the inbox addressee) over `view`, keyed by
    /// `watch` (the durable cursor identity). Use the player entity as `watch`
    /// for one subscription per player, or a distinct entity per view for
    /// several. Does not install — call [`install`](Self::install) or
    /// [`install_including_current`](Self::install_including_current) once.
    pub fn new(view: Query, watch: Entity, player: Entity) -> Subscription {
        Subscription {
            on_watch: OnWatch {
                view,
                watch,
                target: player,
                inbox: WATCH_INBOX,
                cursor_rel: WATCH_CURSOR,
                seqs: WATCH_SEQS,
                authority: watch_authority(),
            },
        }
    }

    /// The watch identity this subscription's durable cursors are keyed by.
    pub fn watch(&self) -> Entity {
        self.on_watch.watch
    }

    /// Whether this subscription's on-watch is already installed (its durable
    /// pump cursor exists). Lets a reconnect skip a duplicate install.
    pub fn is_installed(&self, store: &dyn TraceStore) -> Result<bool> {
        Ok(self.on_watch.cursor(store)?.is_some())
    }

    /// Install **skip-initial** (the default): only view changes committed after
    /// this call are delivered. A one-time setup commit — not meant to be raced.
    pub fn install(&self, store: &dyn TraceStore) -> Result<CommitOutcome> {
        self.on_watch.install(store, &NoSchemas)
    }

    /// Install **`including current`**: the first pump delivers the whole current
    /// view as `+` rows (the initial snapshot) before streaming deltas.
    pub fn install_including_current(&self, store: &dyn TraceStore) -> Result<CommitOutcome> {
        self.on_watch.install_including_current(store, &NoSchemas)
    }

    /// Pump the view once: materialize the deltas produced since the durable
    /// watch-cursor as inbox messages, advancing the cursor in one atomic commit
    /// (delegates to [`OnWatch::pump`]). Returns how many activations were
    /// appended. Draining them to the socket is a separate, later step, so an
    /// activation can be materialized (durable) yet not-yet-delivered — the state
    /// a mid-stream disconnect leaves behind.
    pub fn pump(&self, store: &dyn TraceStore) -> Result<usize> {
        self.on_watch.pump(store, &NoSchemas)
    }

    /// Pump the view (materialize any new deltas as durable inbox messages),
    /// then drain the inbox prefix the socket has not yet seen to `out`.
    /// Returns the activations streamed, in delivery order.
    pub fn pump_and_drain(
        &self,
        store: &dyn TraceStore,
        out: &mut dyn Write,
    ) -> Result<Vec<Delivered>> {
        self.on_watch.pump(store, &NoSchemas)?;
        self.drain(store, out)
    }

    /// The exclusive inbox frontier the socket has already consumed (`0` if no
    /// [`WATCH_DELIVERY`] row exists — the delivery cursor is only ever written
    /// forward, so a stored value is always `>= 1`, and `0` means "absent").
    pub fn delivered_through(&self, store: &dyn TraceStore) -> Result<i64> {
        let at = store.current();
        for (t, d) in store.read_at(WATCH_DELIVERY, at)? {
            let s = t.as_slice();
            if d > 0 && s.first() == Some(&Value::Ent(self.on_watch.watch)) {
                if let Some(Value::Int(next)) = s.get(1) {
                    return Ok(*next);
                }
            }
        }
        Ok(0)
    }

    fn delivery_tuple(&self, next: i64) -> Tuple {
        Tuple::from([Value::Ent(self.on_watch.watch), Value::Int(next)])
    }

    /// Stream the durable inbox prefix at `seq >= delivered_through` to `out`,
    /// **in inbox-seq order** (the pump's materialization order, independent of
    /// physical scan order), then advance the durable delivery cursor over
    /// exactly that batch in one guarded commit. A prefix-consume: nothing before
    /// the cursor is re-read, so a reconnect resumes cleanly.
    ///
    /// The socket write happens *before* the cursor advance, so a crash between
    /// them re-delivers the batch on reconnect (at-least-once) rather than losing
    /// it; a clean drain at a batch boundary leaves the cursor reflecting exactly
    /// what was streamed.
    pub fn drain(&self, store: &dyn TraceStore, out: &mut dyn Write) -> Result<Vec<Delivered>> {
        let from = self.delivered_through(store)?;
        let at = store.current();

        let mut pending: Vec<Delivered> = Vec::new();
        for (t, d) in store.read_at(WATCH_INBOX, at)? {
            let s = t.as_slice();
            if d > 0 && s.first() == Some(&Value::Ent(self.on_watch.target)) {
                if let (Some(Value::Int(seq)), Some(Value::Tuple(body))) = (s.get(1), s.get(2)) {
                    if *seq >= from {
                        if let Some((diff, row)) = decode_activation(&Tuple(body.clone())) {
                            pending.push(Delivered { seq: *seq, diff, row });
                        }
                    }
                }
            }
        }
        // Deliver in seq order — the order the pump materialized them, which is
        // its commit order (edition, counter). NOT the HashMap-consolidated scan
        // order `read_at` may surface.
        pending.sort_by_key(|d| d.seq);
        if pending.is_empty() {
            return Ok(pending);
        }
        // The pump appends contiguous seqs, so the drained prefix must run
        // `from, from+1, …` with no gap; a gap would mean a lost inbox row.
        for (i, d) in pending.iter().enumerate() {
            let want = from + i as i64;
            if d.seq != want {
                return Err(Error::Store(format!(
                    "watch inbox gap: expected seq {want}, found {}",
                    d.seq
                )));
            }
            writeln!(out, "{}", d.render())
                .map_err(|e| Error::Store(format!("socket write failed: {e}")))?;
        }

        // Advance the durable delivery cursor over exactly the streamed batch,
        // guarded on its present value so a re-drain resolves idempotently. From
        // absent (`from == 0`) there is no prior row to retract.
        let next = from + pending.len() as i64;
        let mut patch = Patch::new();
        if from > 0 {
            patch = patch.expect(Fact::new(WATCH_DELIVERY, self.delivery_tuple(from)));
        }
        patch = patch.advance_cursor(CursorMove {
            rel: WATCH_DELIVERY,
            retract: (from > 0).then(|| self.delivery_tuple(from)),
            assert: self.delivery_tuple(next),
        });
        match commit_patch(store, &NoSchemas, &patch, &self.on_watch.authority)? {
            CommitOutcome::Committed(_) => Ok(pending),
            // Serialized behind the server's single writer, no peer advances the
            // delivery cursor under us — a rejection here is a real invariant
            // break, not benign contention.
            CommitOutcome::Rejected => {
                Err(Error::Store("delivery cursor advance was rejected".into()))
            }
        }
    }
}

/// The edition the on-watch pump has delivered through, for observers/tests.
pub fn watch_cursor(store: &dyn TraceStore, watch: Entity) -> Result<Option<Edition>> {
    let at = store.current();
    for (t, d) in store.read_at(WATCH_CURSOR, at)? {
        let s = t.as_slice();
        if d > 0 && s.first() == Some(&Value::Ent(watch)) {
            if let Some(Value::Int(e)) = s.get(1) {
                return Ok(Some(Edition((*e).max(0) as u64)));
            }
        }
    }
    Ok(None)
}
