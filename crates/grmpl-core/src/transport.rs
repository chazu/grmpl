//! The cross-domain transport boundary (DESIGN.md §4.2).
//!
//! Distribution is *secondary*: nothing in the language mentions this trait. It
//! moves opaque byte payloads between authority domains; the interpretation
//! (a `Message` ↔ bytes) and durability (writing into a store inbox) are the
//! caller's concern. In v1 the impl is in-process; iroh is the designed-for
//! real impl, mapping `DomainId` ↔ an `EndpointId` (Ed25519 pubkey) below the
//! line. Delivery is at-least-once; exactly-once is recovered by the receiver
//! persisting each payload in its own commit (DESIGN.md §4.2).

use crate::error::Result;
use crate::value::DomainId;

pub trait Transport: Send + Sync {
    /// Deliver `payload` to authority domain `to` (at-least-once).
    fn send(&self, to: DomainId, payload: &[u8]) -> Result<()>;

    /// Non-blocking receive of the next delivered `(sender, payload)`, if any.
    fn poll(&self) -> Result<Option<(DomainId, Vec<u8>)>>;
}
