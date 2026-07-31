//! The line-based TCP session layer, exercised over real loopback sockets: a
//! client connects, logs in by name, and drives verbs by line — and two clients
//! racing the same lamp across threads still resolve to exactly one winner.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use grmpl_core::{Entity, TraceStore, Value};
use grmpl_session::world::HELD;
use grmpl_session::{serve, Server};

fn start(case: &grmpl_conformance::Case) -> (Arc<Server>, String) {
    let store: Arc<dyn TraceStore> = case.trace();
    let server = Server::new(store);
    server.init().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let srv = Arc::clone(&server);
    thread::spawn(move || serve(srv, listener));
    (server, addr)
}

/// A tiny line client: send a line, read exactly `expect` reply lines.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: &str, name: &str) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        let mut c = Client { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream };
        c.send(name);
        c.read_line(); // the welcome line
        c
    }
    fn send(&mut self, line: &str) {
        writeln!(self.writer, "{line}").unwrap();
        self.writer.flush().unwrap();
    }
    fn read_line(&mut self) -> String {
        let mut s = String::new();
        self.reader.read_line(&mut s).unwrap();
        s.trim_end().to_string()
    }
    /// Send a command and read exactly `n` reply lines.
    fn cmd(&mut self, line: &str, n: usize) -> Vec<String> {
        self.send(line);
        (0..n).map(|_| self.read_line()).collect()
    }
}

#[test]
fn a_client_builds_and_takes_over_tcp() {
    grmpl_conformance::for_each_store(|c| {
        let (server, addr) = start(c);

        let mut c = Client::connect(&addr, "builder");
        assert_eq!(c.cmd("create lamp", 1), vec!["Created lamp."]);
        assert_eq!(c.cmd("take lamp", 1), vec!["Taken."]);

        // The world really changed under the socket layer.
        let store = server.store();
        let held = store.read_at(HELD, store.current()).unwrap();
        assert_eq!(held.len(), 1, "the builder holds exactly one thing over TCP");
    });
}

#[test]
fn two_tcp_clients_racing_the_lamp_yield_one_winner() {
    grmpl_conformance::for_each_store(|c| {
        let (server, addr) = start(c);

        // A builder creates a lamp in the shared root room.
        let mut builder = Client::connect(&addr, "builder");
        assert_eq!(builder.cmd("create lamp", 1), vec!["Created lamp."]);

        // Two players connect (spawned into the same room) and both grab it.
        let a1 = addr.clone();
        let a2 = addr.clone();
        let h1 = thread::spawn(move || {
            let mut c = Client::connect(&a1, "alice");
            c.cmd("take lamp", 1)
        });
        let h2 = thread::spawn(move || {
            let mut c = Client::connect(&a2, "bob");
            c.cmd("take lamp", 1)
        });
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();

        let wins = [&r1, &r2].iter().filter(|r| r.iter().any(|l| l == "Taken.")).count();
        assert_eq!(wins, 1, "exactly one client took the lamp: {r1:?} {r2:?}");

        // And the store agrees: exactly one owner holds a lamp.
        let store = server.store();
        let held: Vec<Entity> = store
            .read_at(HELD, store.current())
            .unwrap()
            .into_iter()
            .filter(|(_, d)| *d > 0)
            .filter_map(|(t, _)| match t.as_slice().first() {
                Some(Value::Ent(e)) => Some(*e),
                _ => None,
            })
            .collect();
        assert_eq!(held.len(), 1);
    });
}
