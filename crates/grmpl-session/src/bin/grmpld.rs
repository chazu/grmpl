//! `grmpld` — a minimal grmpl world server over line-based TCP.
//!
//! Usage: `grmpld [ADDR] [STORE_DIR]`
//!   ADDR       bind address (default `127.0.0.1:4000`)
//!   STORE_DIR  fjall store directory (default: a fresh temp dir)
//!
//! Connect with any line-based client (`nc 127.0.0.1 4000`): the first line is
//! your name; then `look`, `dig hall`, `go hall`, `create lamp`, `take lamp`.

use std::net::TcpListener;
use std::sync::Arc;

use grmpl_core::TraceStore;
use grmpl_session::{serve, Server};
use grmpl_ent::EntStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:4000".to_string());
    let dir = args.next();

    let store: Arc<dyn TraceStore> = match dir {
        Some(path) => Arc::new(EntStore::open(path)?),
        None => {
            let tmp = std::env::temp_dir().join(format!("grmpld-{}", std::process::id()));
            Arc::new(EntStore::open(tmp)?)
        }
    };

    let server = Server::new(store);
    server.init()?;

    let listener = TcpListener::bind(&addr)?;
    println!("grmpld listening on {addr}");
    serve(server, listener);
    Ok(())
}
