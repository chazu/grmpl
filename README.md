# grmpl

A differential, relational substrate for *deriving, watching, and patching a
versioned world*: persistent shared spaces in the vein of
[MOO](https://en.wikipedia.org/wiki/MOO), where a program never mutates
objects. It queries an immutable edition of the world and produces a guarded
patch that creates the next one. The store underneath is an Ent, after
Udanax Gold's: a family of measured, versioned, structurally shared trees.

## Try it

```sh
cargo run -p grmpl-cli -- moo                         # play the built-in MOO
cargo run -p grmpl-cli -- run worlds/shotengai.grmpl  # any world, generic REPL
cargo run -p grmpl-cli -- showcase                    # a narrated tour
cargo run -p grmpl-cli -- store info .grmpl/moo       # inspect a store
```

`grmpl help` lists every command. Inside `grmpl run`, `help` lists the REPL's
(`:rels`, `:views`, `? view args`, `:send`, `:watch`, `:at`).

## Read

* [`DESIGN.md`](DESIGN.md) — the design.
* [`CLAUDE.md`](CLAUDE.md) — the load-bearing invariants, short.
* [`docs/LANGUAGE.md`](docs/LANGUAGE.md) — the `.grmpl` language.
* [`docs/ROADMAP.md`](docs/ROADMAP.md) — what is built and what is next.
* The book: `mise run docs:serve` (sources in `docs/book/src`).

## Verify

```sh
cargo build && cargo test && cargo clippy --all-targets
mise run fuzz    # the decoders, briefly (nightly + cargo-fuzz)
```
