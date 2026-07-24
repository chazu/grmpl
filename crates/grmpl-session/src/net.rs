//! The line-based TCP session layer (P3; websocket is a later transport).
//!
//! One connection = one player. The first line is the login name (minimal
//! auth); each subsequent line is a command, whose resulting `TELL` text is
//! written straight back. Each connection runs on its own thread; the session
//! engine serializes their commits behind its single writer.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use crate::session::Server;

/// Accept connections on `listener` until it closes, handling each on its own
/// thread. Blocks the calling thread.
pub fn serve(server: Arc<Server>, listener: TcpListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let server = Arc::clone(&server);
                thread::spawn(move || {
                    let _ = handle(server, s);
                });
            }
            Err(_) => break,
        }
    }
}

fn handle(server: Arc<Server>, stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Provisioning: the first line names the player (minimal auth).
    let mut name_line = String::new();
    if reader.read_line(&mut name_line)? == 0 {
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
    writeln!(writer, "Welcome, {}. You are entity {}.", name, session.player().0)?;
    writer.flush()?;

    // The command loop: one line in, its told-text out.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
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
