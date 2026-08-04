use std::sync::Arc;

use grmpl::{MooRuntime, Server};

pub fn server(case: &grmpl_conformance::Case) -> Arc<Server> {
    let world = MooRuntime::builtin(case.shared()).expect("compile built-in MOO");
    Server::new(world)
}
