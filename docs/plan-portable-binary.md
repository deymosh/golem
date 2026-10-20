# Plan: one portable `golem` binary (Rust)

Executable by any agent model: each phase lists files, steps and a "done when" check. Work
top to bottom; commit after each step that passes its check.

## Status and next steps (update this block every session)

As of 2026-10-20 (`experiment/token-machine`):

- **Phase 0 done**: workspace, `--version`/help, three-OS CI matrix.
- **Phase 1 done except `ini.rs`**: `json.rs`, `results.rs`
  (rows, size classes, best-per-configuration, last-needle-wins), `evalparse.rs`
  (the RESULT-line contract), `report.rs` and `gguf.rs` (golden-tested against
  `scripts/gguf-info.sh`, 8/8 models). **`golem-core/src/ini.rs` is still only an
  `Entry` struct**: the reader and the `write_candidate`/`write_overlay` merge are
  unwritten because nothing needed them until phase 4. `golem`'s `state.rs` reads the
  preset file with a small scanner in the meantime.
- **Local toolchain**: `scripts/setup-rust-toolchain.sh` (mingw-w64 into the
  git-ignored `.local/toolchain/` + the GNU-hosted Rust toolchain, pinned with a rustup
  override) and `scripts/cargo.sh` (the build wrapper that puts the toolchain on PATH —
  cargo's `[env]` cannot do that for build scripts). A build drops `golem.exe` at the
  repository root (git-ignored). 145 tests run locally; CI keeps building MSVC.
- **Phase 2 (read-only) done**: `golem web` serves the page and `/results.jsonl` from
  the binary, up on an empty database (a fresh machine is where the first result will
  appear), 404 elsewhere, 500 for a file that exists but cannot be read.
- **Phase 3 done**: `golem status`, `up`/`down`/`restart` and the console's
  `api/state`, `api/logs` and `api/run`, all driving the shell launchers through one
  `Driver`. The console's answer was diffed against `scripts/web.ps1`'s while both were
  up: the binary was missing `engines` and `job_tail`, which the page reads, so the
  engine dropdown was empty and the job log never appeared. Fixed; the integration test
  asserts the whole shape now.
- **Phase 5, the console half, done**: `golem tui` opens the screen and its menu entries
  run the launcher verbs. Verified against the running engine (the header renders live
  state) and in a real terminal. The `Web` entry starts this binary's own console in
  process. The checklist from the interactive pass (mouse wheel, resize, tiny windows,
  mintty/conhost) has not been walked yet.
- **The binary resolves a home, not a repository** (decided 2026-10-20, from the user's
  requirement that a release work alone): `GOLEM_HOME` if set, else the directory the
  binary lives in. Everything it needs lives under it and is created on first run -
  `bench/`, `data/{models,cache,tuned}`, `logs/` - and the page template is compiled in
  and written out when missing (an existing copy is never overwritten). The same rule
  serves a downloaded release and the dev binary at the repository root. Verified by
  dropping the binary into an empty folder: it created its directories, wrote the page,
  and served an empty console.
- **The report is grouped hardware first** (2026-10-20, the user's redesign): one `##`
  section per `host` slug, one `###` block per model inside it, and inside that one line
  per context at the **best setup** measured there - the tuning that lost stays in
  `results.jsonl` and the console. Two machines never share a table, and a rejected row
  sits in its own machine's section. The frozen fixture documents it.
- **`scripts/report.sh --md` is stale and must not be run**: it emits the old flat table
  and would overwrite the redesigned `bench/RESULTS.md`. Only `./golem.exe report --md`
  writes the current shape (`golem.cmd report` → `report.ps1` has no `--md` at all).
  Retiring that path is phase 6 work: `./golem` should hand `report` to the binary when
  it sits alongside.
- **Next, in order**: (1) the console's `api/models` plus `api/addmodel`,
  `api/delmodel`, `api/delpreset`, which is what the page's Models tab calls - it needs
  the preset file's `; Download:` comments, the file size, and `gguf::read` for the
  parameters; (2) phase 4, the tuner orchestration, with `golem-core/src/ini.rs`
  written for its overlay; (3) phase 6, the switchover - `./golem` handing a command to
  `golem.exe` when it sits alongside, which is also what retires the stale
  `scripts/report.sh --md` path.
- **Three of the page's buttons are dead in the binary's console**, found by diffing its
  `/api/run` against `scripts/web.ps1`'s: the shell accepts `alias` and `engine` (it
  rewrites `.env` and answers "applied") and hands anything else to the launcher, which
  knows `pull`. The binary's `driver::ACTIONS` has neither pair, so "Set default",
  the engine dropdown's Set and "Fetch" all come back 400. `pull` is also missing from
  `golem.cmd`'s command list, so that verb has no Windows launcher path at all.
- Worth borrowing when the console grows a LAN face: a `Host`-header check, since the
  current console binds 127.0.0.1 only and that is the reason it needs none today.

## Why

The shell stack (4.8k lines across `golem`/`golem.cmd` + 18 scripts, two shells kept in
sync) works, but its costs are structural, not fixable in place:

- startup cost per subprocess (every status check re-reads .env, re-execs bash/MSYS bash);
- locale/quoting traps (comma decimals, `set -e` + `(( ))`, MSYS path translation);
- no real TTY primitives — the TUI is a rendered string pushed through `printf`;
- PowerShell 5.1 and bash must implement every feature twice (`web.ps1` vs `web.sh` …);
- **dependencies the user should not need.** The eval harness used to require python3
  and, on Windows, WSL to run it — a hop that cost correctness, not just convenience:
  environment variables do not cross into WSL (so the tuner's `GOLEM_MODEL_ALIAS` was
  silently dropped and the gate measured `.env`'s model instead of the candidate), and
  MSYS path translation breaks body files handed to Windows `curl`. Rewritten in
  bash+curl+awk on 2026-10-20 (one code path on every platform), which is the interim
  fix; the binary is the real one — no bash, no WSL, no path translation, no
  environment that stops at a process boundary.

One static binary fixes all four and gives Windows/Linux/macOS from a single code base,
plus a unit-testable core (DB, ini merge, GGUF parsing, eval output parsing).

## Shape (decided 2026-10-20)

- **Language: Rust.** ratatui + crossterm (TUI), axum (web), a GGUF crate or a small own
  reader. (Go was the alternative — simpler cross-compilation — but the TUI story is
  weaker and we already depend on nothing Go-shaped.)
- **The binary orchestrates; the scripts stay the engine drivers.** The binary shells out
  to `start-windows.ps1` / `start-local-ai.sh` / `compose.yml` for launching, and to
  `./golem bench|eval` for measurement, exactly like a human would. Engine launching is
  the riskiest code in the repo and already works on three platforms.
- **Migration gate:** a script is deleted only when the binary replaces its gate — the
  eval-gate call, the bench recorder, or the tune sweep loop. Until then both exist and
  the binary is just a better front door.
- **Results view:** the server page is the way to look at results; a static page is an
  export only (`bench/report.template.html` stays the view source; `report --md` stays
  the GitHub view).
- **The 200K eval stays a manual gate on real hardware.** CI never adopts a model, build
  or engine; it only builds and unit-tests.

## Phases

### Phase 0 — scaffolding

**Files:** new `golem-rs/` workspace (bin + lib crates), `.github/workflows/release.yml`.

- `cargo` workspace: `golem` (bin), `golem-core` (lib: results DB, ini, gguf, evalparse).
- CI matrix: `windows-latest`, `ubuntu-latest`, `macos-latest` → `cargo test` + release
  build artifact per OS. Tag push publishes a release.
- **Done when:** three CI jobs green on an empty `golem --version`.

### Phase 1 — read-only parity (the honest core)

**Files:** `golem-core/src/{results,ini,gguf,evalparse}.rs` + unit tests.

- `results.jsonl` reader: parse, filter by model/engine/kind; append with the same schema
  (`ts, kind, host, engine, …, quiet, note`). Append-only, never rewrite.
- ini engine: read `config/models.ini` + `engines/<name>/engine.ini`; reimplement
  `write_candidate` / `write_overlay` (section override merge) exactly.
- GGUF header reader; golden-test against `scripts/gguf-info.sh` output for every GGUF in
  `data/models/`.
- eval output parser: `RESULT long_ctx_N=PASS …`, bench `decode_median=…` lines.
- CLI: `golem results [--model X] [--engine Y]`, `golem report --md` (same tables as
  `scripts/report.sh`).
- **Done when:** `golem report --md` output is byte-identical to `./golem report --md` on
  the current `results.jsonl`, and gguf golden tests pass for all local GGUFs.
  *Superseded 2026-10-20: the byte-identity was reached and then deliberately given up,
  because the report gained a shape the shell never had (hardware first, one line per
  model and context). The GGUF half still stands - 8/8 local models match
  `scripts/gguf-info.sh`.*

### Phase 2 — web console

**Files:** `golem/src/web/`, reuse `bench/report.template.html`.

- axum server on :8787 (same `web.port` config), serving the results page live from
  `results.jsonl`, model list, engine log tail; controls come in Phase 3.
- Same routes the PowerShell console exposes, so bookmarks keep working.
- **Done when:** the pages the console serves render identically to `./golem web` for the
  same data (headless-browser diff), with the script console still the default.

### Phase 3 — controls

- start/stop/restart/status by driving the existing drivers (spawn `golem.cmd` /
  `./golem`), pid file, health polling with short timeouts (the `--noproxy` lesson lives
  in the HTTP client now), engine log tail, `web status|stop`.
- **Done when:** a full tune run (Phase 4's loop) driven end-to-end through the binary's
  start/stop, with the scripts untouched.

### Phase 4 — tuner orchestration

- Port the tune loop: probe (nvidia-smi), seed (llama-fit-params), candidate sweep,
  cliff exclusion (per-engine prefill collapse), ranked fallback on gate failure,
  overlay + `models.effective.ini`, `--adopt` (.env rewrite with .env.bak). Measurement
  itself still shells out to `./golem restart` + bench.
- Unit tests for ranking/fallback/adopt with fixture sweep outputs.
- **Done when:** `golem tune <preset>` on the 0.5B reproduces the script tuner's verdict
  from the same candidates, and on the 35B it adopts the same winner.

### Phase 5 — TUI

- ratatui mirror of `scripts/tui.sh`: status, start/stop, tail, menu parity, Windows
  Terminal + mintty + raw conhost.
- **Done when:** the checklist from the interactive TUI pass (mouse wheel, arrows,
  resize, tiny windows) behaves in the binary.

### Phase 6 — switchover

- `golem.cmd` / `./golem` exec the binary when present; scripts deleted only per the
  migration gate. `.env` stays the config surface (GOLEM_* overrides included).
- **Done when:** clean-machine walk (clone → one command → tuned, running model) uses the
  binary everywhere and needs neither Git Bash nor PowerShell 7.

## Configuration: from `.env` to a file the binary owns (decided 2026-10-20)

`.env` is a shell idiom: it is sourced, so every value is a string and every
consumer re-implements parsing (`awk` for the preset, PowerShell's own loop for the
launcher). The binary should not inherit that.

- **The binary reads `golem.toml`.** TOML, not JSON, for a concrete reason: the
  current `.env` is heavily commented, and the comments are documentation the repo
  would lose. TOML carries comments and matches the INI style `config/models.ini`
  already uses. (Strata's JSON config is a fine choice for Strata; this file has
  different content.)
- **The migration is per phase, not big-bang.** `.env` stays the source of truth for
  every shell driver, exactly as today. When the binary drives a script, it reads the
  TOML and passes what that driver needs as environment for the child process, so a
  driver keeps working unchanged while its configuration source moves.
- **A phase retires the keys of the driver it replaces.** When nothing reads a key
  any more, it leaves `.env`; `.env.example` documents the remaining surface until
  `.env` itself is only a compatibility shim.
- **Nothing about this is urgent.** It is written down so the shell is not extended
  with configuration work the binary will own (2026-10-20: "we don't want to add
  features with the scripting way if we will have to rebuild in rust").

## Non-goals

- No gateway, no MCP, no web search (unchanged).
- No engine binaries bundled — drivers still install/fetch engines as today.
- CI never runs models or adopts anything.
