# golem-rs — the binary

One `golem` executable owns everything: the commands, the engine and the service that
owns it, the measurement, the tuner, the console and the screen. How the pieces fit is
[docs/architecture.md](../docs/architecture.md); this page is for changing them.

```
crates/
  golem-core/   pure and tested: JSON, golem.toml, INI presets and overlays, GGUF,
                results.jsonl, RESULT lines, the report, the tuner's decisions
  golem/        the binary (see the layer table in docs/architecture.md)
```

## Build and test

```sh
./scripts/setup-rust-toolchain.sh   # once per machine
./scripts/cargo.sh test
./scripts/cargo.sh build            # drops golem.exe / golem at the repository root
```

`setup-rust-toolchain.sh` gives the checkout a compiler without installing anything
system-wide:

- **Windows** — a pinned mingw-w64 in `.local/toolchain/` (~264 MiB, no installer, no
  admin, checksum verified) plus the **GNU-hosted** Rust toolchain, pinned to this
  checkout with a `rustup override`. The usual MSVC route needs Visual Studio Build Tools
  (~2 GB, admin); this avoids it.
- **Linux / macOS** — the system compiler; only the Rust side is checked.

The GNU *host* matters, not just the target: build scripts compile for the host, so an
MSVC host still needs the MSVC linker for every dependency that has one.

**Build through `./scripts/cargo.sh`, never plain cargo.** Dependencies run `dlltool` from
the toolchain's `bin` by name, and cargo's `[env]` cannot put PATH in front of a build
script; the wrapper does and forwards every argument. Never build while a bench or a tune
is measuring.

The binary lands at the repository root (git-ignored) because a binary next to `bench/`,
`data/` and `config/` is exactly the shape a release has. **CI builds MSVC**
(`.github/workflows/golem-bin.yml`, Windows, Linux and macOS), which is what a released
Windows binary should be; the local GNU build is self-contained (`+crt-static`) but not
byte-identical to it.

## Conventions

- **Decide in `golem-core`, act in `golem`.** A rule that can be a pure function - a
  ranking, a merge, a prediction - goes in the core with its tests; the binary measures,
  spawns and prints.
- **Configuration goes through `config`.** `config::get` / `config::set` for `golem.toml`,
  `config::Overrides` for the four per-run keys. Never `std::env::set_var`: the service
  is multi-threaded, and overrides are values handed to whoever resolves a start.
- **Well-known files go through `paths`**, hardware probes through `machine`, child
  processes of this binary through `proc`, and every engine verb through
  `service::request`.
- **Verbs take a sink** (`&mut dyn Write`) instead of printing, so the CLI, the service's
  stream and the console's job log run one implementation.
- **Library plus a thin binary:** every module is declared in `src/lib.rs`, and `main.rs`
  only parses the command line. The integration tests in `tests/` import `golem::<module>`;
  `tests/launch_plan.rs` runs the built binary itself.
- `unsafe_code` is forbidden workspace-wide; the dependency set is deliberate (tokio and
  axum for the console, ratatui and crossterm for the screen, nothing else), and JSON,
  TOML, INI, GGUF and HTTP are hand-written for that reason.

## Where the binary keeps its files

**The home is `GOLEM_HOME` if set, otherwise the directory the binary lives in.**
Everything lives under it and is created on first run - `bench/`, `data/models`,
`data/cache`, `data/tuned`, `logs/`, and `golem.toml`, compiled in and written out only
when missing (an existing copy always wins). The page (`golem-rs/crates/golem/console/`:
the markup, its stylesheet and its script, put together at build) and the library (`config/library.ini`) are compiled in too; the page is never
written into the home, so an upgrade always serves its own. A release unpacked anywhere
populates its own folder; the dev build at the repository root uses the checkout's files.
