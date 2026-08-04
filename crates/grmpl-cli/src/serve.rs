//! `grmpl serve [WORLD.grmpl] [STORE_DIR] [ADDR]` — expose the same world
//! runtime as `grmpl run` through the line-oriented TCP adapter.

use std::net::TcpListener;
use std::sync::Arc;

use grmpl::{serve, MooRuntime, Server};
use grmpl_core::WorldStore;
use grmpl_ent::EntStore;

pub fn run(
    world_path: Option<String>,
    store_dir: Option<String>,
    address: Option<String>,
) -> Result<(), String> {
    let source = match world_path {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read world `{path}`: {error}"))?,
        None => grmpl::moo::SOURCE.to_string(),
    };
    let store_path = store_dir.unwrap_or_else(|| ".grmpl/moo".to_string());
    let concrete = Arc::new(
        EntStore::open(&store_path)
            .map_err(|error| format!("cannot open store at {store_path}: {error:?}"))?,
    );
    let store: Arc<dyn WorldStore> = concrete;
    let world = MooRuntime::compile(store, &source)?;
    let server = Server::new(world);
    let address = address.unwrap_or_else(|| "127.0.0.1:7777".to_string());
    let listener = TcpListener::bind(&address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    println!("grmpl — serving {address} from {store_path}");
    serve(server, listener);
    Ok(())
}
