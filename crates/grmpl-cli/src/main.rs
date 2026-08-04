//! `grmpl` — the command-line entry point to the substrate.
//!
//! * `grmpl run [WORLD.grmpl] [STORE_DIR]` — stand up a language-defined world
//!   and play it from an interactive REPL.
//! * `grmpl serve [WORLD.grmpl] [STORE_DIR] [ADDR]` — expose that same world
//!   through the TCP session adapter.
//! * `grmpl shotengai [STORE_DIR]` — play the durable Kasumi Shotengai RPG.
//! * `grmpl showcase` — a narrated tour of the substrate's distinctive features.

mod run;
mod serve;
mod shotengai;
mod showcase;

const USAGE: &str = "\
grmpl — a differential, relational substrate you can play

USAGE:
    grmpl run [WORLD.grmpl] [STORE_DIR]   Stand up a language-defined world and
                                          open an interactive REPL. With no WORLD
                                          the built-in MOO is used; with no
                                          STORE_DIR a fresh temporary store.
    grmpl serve [WORLD.grmpl] [STORE_DIR] [ADDR]
                                          Serve the same runtime over TCP.
                                          Defaults to the built-in MOO,
                                          .grmpl/moo, and 127.0.0.1:7777.
    grmpl shotengai [STORE_DIR]           Enter the durable Kasumi Shotengai RPG.
                                          Defaults to .grmpl/shotengai.
    grmpl showcase                        Run a narrated tour of the substrate's
                                          distinctive features.
    grmpl help                            Show this help.

Inside `grmpl run`, type `help` for the world commands.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let result = match cmd {
        "run" => run::run(args.get(1).cloned(), args.get(2).cloned()),
        "serve" => serve::run(
            args.get(1).cloned(),
            args.get(2).cloned(),
            args.get(3).cloned(),
        ),
        "shotengai" => shotengai::run(args.get(1).cloned()),
        "showcase" => showcase::run(),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("unknown command `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
