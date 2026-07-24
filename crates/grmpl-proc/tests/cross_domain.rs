//! Cross-domain routing (DESIGN.md §4.2): a patch that emits to a *remote*
//! inbox lands in a durable outbox in the same commit, is shipped over the
//! transport, and is applied exactly once into the receiver's inbox — even if
//! delivery is retried.

use std::collections::HashMap;

use grmpl_core::{
    Authority, DomainId, EditionStore, Entity, Fact, Message, Patch, RelId, Scope, TraceStore,
    Tuple, Value,
};
use grmpl_core::NoSchemas;
use grmpl_proc::{outbox_len, Domain};
use grmpl_store::FjallStore;
use grmpl_transport::InProcessNet;

const A: DomainId = DomainId(1);
const B: DomainId = DomainId(2);

// A's local bookkeeping relations.
const A_OUTBOX: RelId = RelId(90);
const A_OUTSEQ: RelId = RelId(91);
const A_SEEN: RelId = RelId(92);
// B's.
const B_OUTBOX: RelId = RelId(90);
const B_OUTSEQ: RelId = RelId(91);
const B_SEEN: RelId = RelId(92);

const B_INBOX: RelId = RelId(10); // owned by domain B
const WORLD: RelId = RelId(1); // some world relation A owns

fn domain_a<'a>(store: &'a FjallStore, tx: &'a dyn grmpl_core::Transport) -> Domain<'a> {
    let mut routes = HashMap::new();
    routes.insert(B_INBOX, B); // B_INBOX lives on domain B
    Domain { id: A, store, transport: tx, routes, outbox: A_OUTBOX, outseq: A_OUTSEQ, seen: A_SEEN }
}

fn domain_b<'a>(store: &'a FjallStore, tx: &'a dyn grmpl_core::Transport) -> Domain<'a> {
    Domain {
        id: B,
        store,
        transport: tx,
        routes: HashMap::new(),
        outbox: B_OUTBOX,
        outseq: B_OUTSEQ,
        seen: B_SEEN,
    }
}

fn wave_to_b() -> Patch {
    Patch::new()
        .assert(Fact::new(WORLD, Tuple::from([Value::Ent(Entity(1)), Value::text("did wave")])))
        .emit(Message { inbox: B_INBOX, body: Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]) })
}

fn a_authority() -> Authority {
    Authority::new(A, vec![Scope::whole(WORLD)])
}

#[test]
fn remote_emit_is_outboxed_shipped_and_applied_once() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let store_a = FjallStore::open(dir_a.path()).unwrap();
    let store_b = FjallStore::open(dir_b.path()).unwrap();

    let net = InProcessNet::new();
    let tx_a = net.endpoint(A);
    let tx_b = net.endpoint(B);
    let da = domain_a(&store_a, &tx_a);
    let db = domain_b(&store_b, &tx_b);

    // A commits: world change is local, the emit is routed to B's outbox.
    da.commit(&wave_to_b(), &a_authority(), &NoSchemas).unwrap();

    // The message is NOT yet in B; it sits in A's durable outbox.
    assert!(store_b.read_at(B_INBOX, store_b.current()).unwrap().is_empty());
    assert_eq!(outbox_len(&store_a, A_OUTBOX, store_a.current()).unwrap(), 1);
    // A's own world write did land locally.
    assert_eq!(store_a.read_at(WORLD, store_a.current()).unwrap().len(), 1);

    // Delivery pass ships it; outbox drains.
    assert_eq!(da.flush_outbox().unwrap(), 1);
    assert_eq!(outbox_len(&store_a, A_OUTBOX, store_a.current()).unwrap(), 0);

    // B receives and applies it into its inbox.
    assert_eq!(db.receive().unwrap(), 1);
    let inbox = store_b.read_at(B_INBOX, store_b.current()).unwrap();
    assert_eq!(inbox, vec![(Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]), 1)]);

    // Idempotent redelivery: resend the same envelope, B applies nothing new.
    let seq = 0i64; // first outbox seq
    let env = {
        // reconstruct the envelope A would have sent
        let mut b = seq.to_be_bytes().to_vec();
        b.extend(grmpl_core::wire::encode_message(&Message {
            inbox: B_INBOX,
            body: Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]),
        }));
        b
    };
    tx_a.send_raw_for_test(B, &env); // helper below
    assert_eq!(db.receive().unwrap(), 0, "duplicate is deduped");
    assert_eq!(store_b.read_at(B_INBOX, store_b.current()).unwrap().len(), 1);
}

// Small shim so the test can inject a duplicate without going through a Domain.
trait SendRaw {
    fn send_raw_for_test(&self, to: DomainId, payload: &[u8]);
}
impl<T: grmpl_core::Transport> SendRaw for T {
    fn send_raw_for_test(&self, to: DomainId, payload: &[u8]) {
        self.send(to, payload).unwrap();
    }
}

#[test]
fn two_remote_emits_get_distinct_seqs() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let store_a = FjallStore::open(dir_a.path()).unwrap();
    let store_b = FjallStore::open(dir_b.path()).unwrap();

    let net = InProcessNet::new();
    let tx_a = net.endpoint(A);
    let tx_b = net.endpoint(B);
    let da = domain_a(&store_a, &tx_a);
    let db = domain_b(&store_b, &tx_b);

    // Two separate commits, each emitting to B.
    da.commit(&wave_to_b(), &a_authority(), &NoSchemas).unwrap();
    da.commit(&wave_to_b(), &a_authority(), &NoSchemas).unwrap();
    assert_eq!(outbox_len(&store_a, A_OUTBOX, store_a.current()).unwrap(), 2);

    da.flush_outbox().unwrap();
    db.receive().unwrap();

    // Both distinct messages applied (weight 2 for the same body).
    let inbox = store_b.read_at(B_INBOX, store_b.current()).unwrap();
    assert_eq!(inbox, vec![(Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]), 2)]);
}
