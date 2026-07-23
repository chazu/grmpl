//! An iroh-backed QUIC `Transport` (DESIGN.md §4.2), feature-gated behind
//! `iroh`. An authority domain *is* an iroh endpoint; `DomainId` maps to an
//! `EndpointId` (Ed25519 pubkey) below the line. Delivery is reliable per call:
//! `send` opens a bi-stream, writes the sender-tagged payload, and waits for a
//! one-byte ack before returning, so the bytes are in the peer's hands.
//!
//! Routes (`DomainId → EndpointAddr`) are supplied by the caller; the language
//! never sees an address or a stream.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use grmpl_core::{DomainId, Error, Result, Transport};
use iroh::{Endpoint, NodeAddr, Watcher};
use tokio::runtime::Runtime;

/// ALPN identifying the grmpl transport protocol.
pub const GRMPL_ALPN: &[u8] = b"grmpl/transport/0";

/// Cap on a single message payload (defensive bound for `read_to_end`).
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

pub struct IrohTransport {
    domain: DomainId,
    rt: Arc<Runtime>,
    ep: Endpoint,
    routes: Arc<Mutex<HashMap<DomainId, NodeAddr>>>,
    inbound: Mutex<Receiver<(DomainId, Vec<u8>)>>,
}

impl IrohTransport {
    /// Bind an endpoint for `domain` and start accepting connections.
    pub fn bind(domain: DomainId) -> Result<IrohTransport> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(store_err)?,
        );

        let ep = rt
            .block_on(async {
                Endpoint::builder()
                    .alpns(vec![GRMPL_ALPN.to_vec()])
                    .bind()
                    .await
            })
            .map_err(store_err)?;

        let (tx, rx) = channel::<(DomainId, Vec<u8>)>();

        // Accept loop: for each connection, read one sender-tagged message and
        // ack it.
        let accept_ep = ep.clone();
        rt.spawn(async move {
            while let Some(incoming) = accept_ep.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let (mut send, mut recv) = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let buf = match recv.read_to_end(MAX_PAYLOAD).await {
                        Ok(b) => b,
                        Err(_) => return,
                    };
                    if buf.len() >= 8 {
                        let mut d = [0u8; 8];
                        d.copy_from_slice(&buf[..8]);
                        let from = DomainId(u64::from_be_bytes(d));
                        let _ = tx.send((from, buf[8..].to_vec()));
                    }
                    // Acknowledge receipt so the sender's `send` can return.
                    let _ = send.write_all(b"k").await;
                    let _ = send.finish();
                    // Keep the connection alive until the peer has the ack.
                    conn.closed().await;
                });
            }
        });

        Ok(IrohTransport {
            domain,
            rt,
            ep,
            routes: Arc::new(Mutex::new(HashMap::new())),
            inbound: Mutex::new(rx),
        })
    }

    /// This endpoint's dialable address (hand it to peers via [`add_route`]).
    /// Waits until at least one direct address is known so local dialing works
    /// without a relay.
    pub fn addr(&self) -> NodeAddr {
        self.rt.block_on(async {
            let mut w = self.ep.node_addr();
            w.initialized().await
        })
    }

    /// Register how to reach `domain`.
    pub fn add_route(&self, domain: DomainId, addr: NodeAddr) {
        self.routes.lock().unwrap().insert(domain, addr);
    }
}

impl Transport for IrohTransport {
    fn send(&self, to: DomainId, payload: &[u8]) -> Result<()> {
        let addr = self
            .routes
            .lock()
            .unwrap()
            .get(&to)
            .cloned()
            .ok_or_else(|| Error::Store(format!("no route to domain {to:?}")))?;

        let mut framed = self.domain.0.to_be_bytes().to_vec();
        framed.extend_from_slice(payload);

        self.rt.block_on(async {
            let conn = self.ep.connect(addr, GRMPL_ALPN).await.map_err(store_err)?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(store_err)?;
            send.write_all(&framed).await.map_err(store_err)?;
            send.finish().map_err(store_err)?;
            // Wait for the receiver's ack: the bytes are delivered.
            let _ack = recv.read_to_end(8).await.map_err(store_err)?;
            conn.close(0u8.into(), b"done");
            Ok::<(), Error>(())
        })
    }

    fn poll(&self) -> Result<Option<(DomainId, Vec<u8>)>> {
        match self.inbound.lock().unwrap().try_recv() {
            Ok(v) => Ok(Some(v)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Ok(None),
        }
    }
}
