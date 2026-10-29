# Golem

One Rust binary that installs, starts, supervises, measures and tunes a pinned inference
engine (llama.cpp) serving one local model over the Anthropic (`/v1/messages`) and OpenAI
APIs. The engine speaks both APIs natively.

Read before changing anything: `docs/architecture.md` (design), `docs/tuning.md` (the
tuner), `docs/roadmap.md` (the work, in order), `golem-rs/README.md` (code conventions).

## Layout

- `golem-rs/crates/golem-core/`: pure, tested decisions (JSON, `golem.toml`, INI
  presets and overlays, GGUF, results rows, the report, every tuner decision).
- `golem-rs/crates/golem/`: the binary. A library (`src/lib.rs` declares every module)
  and a thin `main.rs` that only parses arguments. Integration tests in `tests/` import
  `golem::<module>`.
- `config/models.ini`: the shipped presets (`; Download: repo -> file` comments feed
  `up` and `pull`). `engines/<name>/engine.ini`: engine adapters.
- Per-machine and git-ignored: `golem.toml` (template: `golem.toml.example`), `.local/`,
  `data/tuned/`, `data/models/`, `logs/`.

## Build and test

- Only `./scripts/cargo.sh test` and `./scripts/cargo.sh build`, never plain `cargo`: the
  wrapper puts the pinned toolchain on PATH. First-time setup: `./scripts/setup-rust-toolchain.sh`.
- The build drops the binary at the repo root (`./golem.exe` / `./golem`). It runs on the
  checkout's own `bench/`, `data/`, `config/`.
- `rust-version` is 1.81: no std APIs newer than that. `unsafe_code` is forbidden. The
  dependency set is deliberate (tokio and axum, ratatui and crossterm). JSON, TOML, INI,
  GGUF and HTTP are hand-written; don't add crates casually.
- After rebuilding, a running `golem service` still runs the old code: `golem service stop`
  it after the build. With the app window (`golem app`) open, the window starts a new
  service itself within seconds, from whatever binary is there then - a stop before the
  build brings the old code straight back. Check the service's start time against the
  binary's.

## Code conventions

- Decide in `golem-core` as pure functions with tests; measure, spawn and print in `golem`.
- Configuration goes through `config::get`/`config::set`, and per-run overrides through
  `config::Overrides` (the four `GOLEM_*` keys). Never `std::env::set_var`.
- Files go through `paths`, hardware probes through `machine`, child processes through
  `proc`, and every engine verb through `service::request`. The service owns the engine.
- Verbs write to a sink (`&mut dyn Write`) rather than printing, so the CLI, the service
  stream and the console share one implementation.
- Comments and docs state current behaviour in the repo's own voice, checked against the
  code. History lives in git, not in comments ("ported from", "used to", "now").

## Hard constraints

- No MCP server and no API translation, ever. Golem may answer the API in front of the
  engine (`api.rs`) for its own read-only web tools, search and fetch; every other
  request passes through unchanged, and an Anthropic request still goes to the engine's
  Anthropic endpoint.
- Backend-agnostic: model, engine and flags come from `golem.toml`, presets and adapters,
  never hard-coded to one machine.
- Public repo: no personal data, LAN IPs or hostnames in code, docs, fixtures or commits.
  A host is an anonymous hardware slug.
- Nothing (model, engine build, flags) becomes a default without
  `golem eval --long 200000` passing on real hardware.

## Measuring

The `measure` skill (`.claude/skills/measure/`) is the routine: preflight, background
runs, alternating pairs, build trials, adoption.

- The GPU usually belongs to someone's desktop: ask before loading a model or running
  `bench`/`eval`/`tune`. Cheap checks use a tiny model or no model at all.
- Never build while a bench or tune is measuring. Run one engine-touching command at a
  time. Long runs go to a log file in the background.
- Comparisons: best-of-3, quiet desktop, pairs measured in the same session, same size
  class only. A rate identifies its model: sanity-check speeds before trusting a PASS.
- A background script must not be the one that starts `golem service`: the service
  inherits the script's pipe and the script never sees EOF. Start the service
  interactively first.
- Unexplained VRAM or RAM use: look for an orphan `llama-server` before blaming anything,
  then at Docker Desktop's WSL VM (page cache).
- llama.cpp b11879 and b11936 fail `load-mode = mlock` on load (b11755 does not);
  `mmap+mlock` pins the same way and loads. A failed CUDA load blanks a Windows screen
  for a moment.
- The console page (`golem-rs/crates/golem/console/`: index.html, console.css, console.js)
  is compiled in: change it, rebuild,
  restart the service. The presets, library and default adapter are compiled in too, for
  a release folder; a checkout's own files win.

## Git

- Conventional Commits, one concern per commit. The body says what was measured or
  checked, and why.
- No pushes or tags without an explicit go-ahead.
