//! The TCP adapter's limits ([`NetLimits`]): an over-long line closes its
//! connection having read no more than the limit, an idle connection is
//! closed, and past the connection cap a client is turned away until a slot
//! frees — while a session inside the limits plays as ever.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod common;

use grmpl::{serve_with, NetLimits, Server};

fn start(limits: NetLimits) -> (Arc<Server>, String) {
    let case = grmpl_conformance::each_store().into_iter().next().unwrap();
    let server = common::server(&case);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let srv = Arc::clone(&server);
    // The case owns the store's directory; it lives as long as the server.
    thread::spawn(move || {
        let _case = case;
        serve_with(srv, listener, limits)
    });
    (server, addr)
}

fn limits() -> NetLimits {
    NetLimits { max_line: 64, idle: Some(Duration::from_secs(30)), max_connections: 2 }
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: &str) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Client { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream }
    }
    fn login(addr: &str, name: &str) -> Client {
        let mut c = Client::connect(addr);
        c.send(name);
        assert!(c.read_line().starts_with("Welcome"), "{name} was not welcomed");
        c
    }
    fn send(&mut self, line: &str) {
        self.writer.write_all(format!("{line}\n").as_bytes()).unwrap();
    }
    /// The next line, or `""` once the server has closed the connection.
    fn read_line(&mut self) -> String {
        let mut s = String::new();
        let _ = self.reader.read_line(&mut s);
        s.trim_end().to_string()
    }
    fn closed(&mut self) -> bool {
        let mut s = String::new();
        matches!(self.reader.read_line(&mut s), Ok(0) | Err(_))
    }
}

#[test]
fn a_session_inside_the_limits_plays() {
    let (_server, addr) = start(limits());
    let mut c = Client::login(&addr, "builder");
    c.send("create orb");
    assert_eq!(c.read_line(), "Created orb.");
    c.send("take orb");
    assert_eq!(c.read_line(), "Taken.");
}

#[test]
fn an_over_long_line_closes_the_connection() {
    // Room for every client below at once: a closed connection's slot frees
    // only once the server's lingering close is done.
    let (_server, addr) = start(NetLimits { max_connections: 8, ..limits() });
    // As a login.
    let mut c = Client::connect(&addr);
    c.send(&"x".repeat(100));
    assert_eq!(c.read_line(), "error: line too long; closing");
    assert!(c.closed());
    // As a command.
    let mut c = Client::login(&addr, "builder");
    c.send(&format!("say {}", "y".repeat(100)));
    assert_eq!(c.read_line(), "error: line too long; closing");
    assert!(c.closed());
    // The server still serves.
    let mut c = Client::login(&addr, "builder");
    c.send("create orb");
    assert_eq!(c.read_line(), "Created orb.");
}

/// A client streaming a line that never ends is cut off: the server stops
/// reading at the limit, so the client's writes fail long before 64 MiB, where
/// an unbounded read would have taken it all into memory.
#[test]
fn a_line_that_never_ends_is_cut_off() {
    let (_server, addr) = start(limits());
    let mut stream = TcpStream::connect(&addr).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let chunk = vec![b'z'; 64 * 1024];
    let mut sent = 0usize;
    let err = loop {
        match stream.write_all(&chunk) {
            Ok(()) => sent += chunk.len(),
            Err(e) => break Some(e),
        }
        if sent >= 64 << 20 {
            break None;
        }
    };
    let err = err.expect("the server read 64 MiB of one line");
    assert!(
        !matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
        "the server stopped reading but held the connection open: {err}"
    );
}

#[test]
fn an_idle_connection_is_closed() {
    let (_server, addr) = start(NetLimits { idle: Some(Duration::from_millis(200)), ..limits() });
    let mut c = Client::login(&addr, "builder");
    let begun = Instant::now();
    assert_eq!(c.read_line(), "error: idle too long; closing");
    assert!(c.closed());
    assert!(begun.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_connection_cap_turns_the_next_client_away() {
    let (_server, addr) = start(limits());
    let a = Client::login(&addr, "alice");
    let _b = Client::login(&addr, "bob");
    let mut c = Client::connect(&addr);
    assert_eq!(c.read_line(), "server full");
    assert!(c.closed());

    // A slot frees when its client leaves.
    drop(a);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut c = Client::connect(&addr);
        c.send("carol");
        let reply = c.read_line();
        if reply.starts_with("Welcome") {
            break;
        }
        assert!(reply == "server full" || reply.is_empty(), "unexpected reply {reply:?}");
        assert!(Instant::now() < deadline, "the departed client's slot never freed");
        thread::sleep(Duration::from_millis(20));
    }
}
