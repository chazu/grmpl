//! The line-based TCP session layer (P3; websocket is a later transport).
//!
//! One connection = one player. The first line is the login name (minimal
//! auth); each subsequent line is a command, whose resulting `TELL` text is
//! written straight back. Each connection runs on its own thread; the session
//! engine resolves concurrent commits through the runtime's guarded optimistic
//! protocol.
//!
//! **No client can exhaust the server** ([`NetLimits`]): a line is read at
//! most so long, a connection idle past its timeout is closed, and past a cap
//! on connections served at once a new one is told the server is full and
//! closed before a thread is spent on it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::session::Server;

/// **What one client may cost the server.**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetLimits {
    /// The longest line, login or command, in bytes with its newline. A longer
    /// one is refused and its connection closed, so a client that never sends
    /// a newline cannot grow the server's buffer without bound.
    pub max_line: u64,
    /// How long a connection may wait on its client, reading a line or writing
    /// a reply, before it is closed. `None` waits forever.
    pub idle: Option<Duration>,
    /// Connections served at once. One more is told `server full` and closed.
    pub max_connections: usize,
}

impl Default for NetLimits {
    /// 4 KiB lines, ten idle minutes, 256 connections.
    fn default() -> NetLimits {
        NetLimits { max_line: 4096, idle: Some(Duration::from_secs(600)), max_connections: 256 }
    }
}

/// Accept connections on `listener` until it closes, handling each on its own
/// thread, under the default [`NetLimits`]. Blocks the calling thread.
pub fn serve(server: Arc<Server>, listener: TcpListener) {
    serve_with(server, listener, NetLimits::default());
}

/// [`serve`] under `limits`.
pub fn serve_with(server: Arc<Server>, listener: TcpListener, limits: NetLimits) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        // Counted here, before any thread exists, so the cap is exact.
        let Some(slot) = Slot::take(&open, limits.max_connections) else {
            // No lingering here: the accept loop must not wait on a client.
            let _ = stream.write_all(b"server full\n");
            let _ = stream.shutdown(Shutdown::Write);
            continue;
        };
        let server = Arc::clone(&server);
        thread::spawn(move || {
            let _slot = slot;
            let _ = handle(server, stream, limits);
        });
    }
}

/// One of the [`NetLimits::max_connections`] connections being served, given
/// back when its thread drops it.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(open: &Arc<AtomicUsize>, max: usize) -> Option<Slot> {
        open.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1)).ok()?;
        Some(Slot(Arc::clone(open)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Read one line into `line`, at most `max` bytes with its newline: an
/// over-long line is an error, with `line` grown no more than one byte past
/// the limit.
fn read_bounded(r: &mut BufReader<TcpStream>, line: &mut String, max: u64) -> io::Result<usize> {
    let n = r.by_ref().take(max + 1).read_line(line)?;
    if n as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(n)
}

/// The next line from the client into `line`, or `false` when the connection
/// should close: the client hung up, or (told why) sent a line too long or
/// nothing for too long.
fn next_line(r: &mut BufReader<TcpStream>, w: &mut TcpStream, line: &mut String, max: u64) -> io::Result<bool> {
    line.clear();
    let why = match read_bounded(r, line, max) {
        Ok(0) => return Ok(false),
        Ok(_) => return Ok(true),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => e.to_string(),
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
            "idle too long".to_string()
        }
        Err(e) => return Err(e),
    };
    refuse(w, &format!("error: {why}; closing"));
    Ok(false)
}

/// Tell the client why it is being closed, then close gently. Closing a
/// socket with input still unread resets the connection, which can discard
/// the message before the client reads it; so the message goes in one write,
/// then end-of-stream, then a little of what the client is still sending is
/// read and dropped first: a lingering close, bounded in bytes and time.
fn refuse(w: &mut TcpStream, msg: &str) {
    const LINGER_BYTES: usize = 64 * 1024;
    const LINGER: Duration = Duration::from_millis(500);
    let _ = w.write_all(format!("{msg}\n").as_bytes());
    let _ = w.shutdown(Shutdown::Write);
    let _ = w.set_read_timeout(Some(Duration::from_millis(100)));
    let until = Instant::now() + LINGER;
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    while drained < LINGER_BYTES && Instant::now() < until {
        match w.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn handle(server: Arc<Server>, stream: TcpStream, limits: NetLimits) -> io::Result<()> {
    stream.set_read_timeout(limits.idle)?;
    stream.set_write_timeout(limits.idle)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Provisioning: the first line names the player (minimal auth).
    let mut name_line = String::new();
    if !next_line(&mut reader, &mut writer, &mut name_line, limits.max_line)? {
        return Ok(());
    }
    let name = name_line.trim();
    if name.is_empty() {
        return Ok(());
    }
    let mut session = match server.login(name) {
        Ok(s) => s,
        Err(e) => {
            writeln!(writer, "login failed: {e}")?;
            return Ok(());
        }
    };
    writeln!(
        writer,
        "Welcome, {}. You are entity {}.",
        name,
        session.player().0
    )?;
    writer.flush()?;

    // The command loop: one line in, its told-text out.
    let mut line = String::new();
    loop {
        if !next_line(&mut reader, &mut writer, &mut line, limits.max_line)? {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        if cmd == "quit" {
            break;
        }
        // `watch` turns on reactive push for this connection: subsequent world
        // changes stream back as activation lines after each command's output.
        if cmd == "watch" {
            match session.subscribe() {
                Ok(()) => writeln!(writer, "Watching.")?,
                Err(e) => writeln!(writer, "error: {e}")?,
            }
            writer.flush()?;
            continue;
        }
        match session.submit(cmd) {
            Ok(msgs) => {
                for m in msgs {
                    writeln!(writer, "{m}")?;
                }
            }
            Err(e) => writeln!(writer, "error: {e}")?,
        }
        // Reactive push: drain any activations this player's subscription has
        // materialized (its own command may have changed a watched view, or a
        // peer's did) and stream them, like `TELL` output, to the socket.
        if let Err(e) = session.push_activations(&mut writer) {
            writeln!(writer, "error: {e}")?;
        }
        writer.flush()?;
    }
    Ok(())
}
