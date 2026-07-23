//! iroh-backed transport loopback (feature `iroh`): two real QUIC endpoints in
//! one process exchange a sender-tagged payload directly over loopback — the
//! `EndpointId`-per-domain durable-outbox path from DESIGN.md §4.2, proven end
//! to end on the wire.
//!
//! Run with: `cargo test -p grmpl-transport --features iroh`.
#![cfg(feature = "iroh")]

use std::time::Duration;

use grmpl_core::{DomainId, Entity, Message, RelId, Transport, Tuple, Value};
use grmpl_transport::{decode_message, encode_message, IrohTransport};

fn poll_until(t: &IrohTransport) -> Option<(DomainId, Vec<u8>)> {
    for _ in 0..100 {
        if let Some(m) = t.poll().unwrap() {
            return Some(m);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn quic_loopback_delivers_a_tagged_message() {
    let a = IrohTransport::bind(DomainId(1)).unwrap();
    let b = IrohTransport::bind(DomainId(2)).unwrap();

    // A learns how to reach B (its dialable address).
    a.add_route(DomainId(2), b.addr());

    // A ships a serialized Message to B over QUIC; `send` blocks until B acks.
    let msg = Message {
        inbox: RelId(10),
        body: Tuple::from([Value::Ent(Entity(7)), Value::text("take lamp")]),
    };
    a.send(DomainId(2), &encode_message(&msg)).unwrap();

    // B receives it, tagged with A as the sender, and it round-trips.
    let (from, payload) = poll_until(&b).expect("B should receive the message");
    assert_eq!(from, DomainId(1));
    assert_eq!(decode_message(&payload).unwrap(), msg);
}
