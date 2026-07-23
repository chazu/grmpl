//! An in-process `Transport`: the v1 default, and the reference the iroh impl
//! must match. Domains register a queue with a shared hub; `send` routes bytes
//! to the target's queue tagged with the sender.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use grmpl_core::{DomainId, Error, Result, Transport};

type Queues = Arc<Mutex<HashMap<DomainId, Sender<(DomainId, Vec<u8>)>>>>;

/// A shared in-process network. Create one, then mint an endpoint per domain.
#[derive(Clone, Default)]
pub struct InProcessNet {
    queues: Queues,
}

impl InProcessNet {
    pub fn new() -> InProcessNet {
        InProcessNet::default()
    }

    /// Mint the transport endpoint for `domain`, registering its inbound queue.
    pub fn endpoint(&self, domain: DomainId) -> InProcessTransport {
        let (tx, rx) = channel();
        self.queues.lock().unwrap().insert(domain, tx);
        InProcessTransport { domain, queues: self.queues.clone(), rx: Mutex::new(rx) }
    }
}

pub struct InProcessTransport {
    domain: DomainId,
    queues: Queues,
    rx: Mutex<Receiver<(DomainId, Vec<u8>)>>,
}

impl Transport for InProcessTransport {
    fn send(&self, to: DomainId, payload: &[u8]) -> Result<()> {
        let q = self
            .queues
            .lock()
            .unwrap()
            .get(&to)
            .cloned()
            .ok_or_else(|| Error::Store(format!("no route to domain {to:?}")))?;
        q.send((self.domain, payload.to_vec()))
            .map_err(|e| Error::Store(e.to_string()))
    }

    fn poll(&self) -> Result<Option<(DomainId, Vec<u8>)>> {
        match self.rx.lock().unwrap().try_recv() {
            Ok(v) => Ok(Some(v)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Ok(None),
        }
    }
}
