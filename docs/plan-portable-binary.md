# Plan: one portable `golem` binary (Rust)

Executable by any agent model: each phase lists files, steps and a "done when" check. Work
top to bottom; commit after each step that passes its check.

## Status and next steps (update this block every session)

As of 2026-10-20 (`experiment/token-machine`): **proposed, not started.** Decision context
in `pending-work` memory and below; this plan is the agreed shape, phases are sized to be
mergeable one at a time.

## Why

The shell stack (4.8k lines across `golem`/`golem.cmd` + 18 scripts, two shells kept in
sync) works, but its costs are structural, not fixable in place:

- startup cost per subprocess (every status check re-reads .env, re-execs bash/MSYS bash);
- locale/quoting traps (comma decimals, `set -e` + `(( ))`, MSYS path translation);
- no real TTY primitives — the TUI is a rendered string pushed through `printf`;
- PowerShell 5.1 and bash must implement every feature twice (`web.ps1` vs `web.sh` …).

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

## Non-goals

- No gateway, no MCP, no web search (unchanged).
- No engine binaries bundled — drivers still install/fetch engines as today.
- CI never runs models or adopts anything.
