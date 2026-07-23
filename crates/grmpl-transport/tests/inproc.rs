//! The in-process transport moves bytes between domains, and closes the
//! durable cross-domain message loop: a `Message` emitted in domain A is
//! serialized, delivered, and persisted into domain B's inbox by B's own commit
//! (DESIGN.md §4.2).

use grmpl_core::{DomainId, EditionStore, Entity, Message, RelId, TraceStore, Tuple, Transport, Value};
use grmpl_transport::{decode_message, encode_message, InProcessNet};

const A: DomainId = DomainId(1);
const B: DomainId = DomainId(2);
const B_INBOX: RelId = RelId(10);

#[test]
fn bytes_route_between_domains_tagged_with_sender() {
    let net = InProcessNet::new();
    let a = net.endpoint(A);
    let b = net.endpoint(B);

    assert!(b.poll().unwrap().is_none()); // nothing yet
    a.send(B, b"hello").unwrap();

    let (from, payload) = b.poll().unwrap().expect("B should have a message");
    assert_eq!(from, A);
    assert_eq!(payload, b"hello");
    assert!(b.poll().unwrap().is_none()); // drained
}

#[test]
fn message_survives_a_wire_round_trip() {
    let m = Message {
        inbox: B_INBOX,
        body: Tuple::from([Value::Ent(Entity(42)), Value::text("take lamp"), Value::Int(-7)]),
    };
    let bytes = encode_message(&m);
    assert_eq!(decode_message(&bytes).unwrap(), m);
}

#[test]
fn durable_cross_domain_delivery_into_the_receiver_store() {
    let dir = tempfile::tempdir().unwrap();
    let store_b = grmpl_store::FjallStore::open(dir.path()).unwrap();

    let net = InProcessNet::new();
    let a = net.endpoint(A);
    let b = net.endpoint(B);

    // A emits a message destined for B's inbox and ships it.
    let msg = Message {
        inbox: B_INBOX,
        body: Tuple::from([Value::Ent(Entity(1)), Value::text("wave")]),
    };
    a.send(B, &encode_message(&msg)).unwrap();

    // B receives and persists it in its own commit (durable, exactly-once locally).
    let (from, payload) = b.poll().unwrap().unwrap();
    assert_eq!(from, A);
    let received = decode_message(&payload).unwrap();
    store_b.commit(&[(received.inbox, received.body.clone(), 1)]).unwrap();

    // The message is now in B's inbox relation.
    let inbox = store_b.read_at(B_INBOX, store_b.current()).unwrap();
    assert_eq!(inbox, vec![(msg.body, 1)]);
}
