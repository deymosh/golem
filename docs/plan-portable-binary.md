# Plan: one portable `golem` binary (Rust)

Executable by any agent model: each phase lists files, steps and a "done when" check. Work
top to bottom; commit after each step that passes its check.

## Status and next steps (update this block every session)

As of 2026-10-20 (`experiment/token-machine`):

- **Phase 0 done**: workspace, `--version`/help, three-OS CI matrix.
- **Phase 1 done**: `json.rs` (reader, plus the `quote` escaper the console writes
  with), `results.rs` (rows, size classes, best-per-configuration,
  last-needle-wins), `evalparse.rs` (the RESULT-line contract), `report.rs`,
  `gguf.rs` (golden-tested against `scripts/gguf-info.sh`, 8/8 models) and
  `ini.rs` - the reader and the `write_candidate` / `write_overlay` merge, both
  verified byte-identical to `scripts/tune.sh`'s awk on the real
  `config/models.ini`. `state.rs` reads the preset file through it now.
- **Local toolchain**: `scripts/setup-rust-toolchain.sh` (mingw-w64 into the
  git-ignored `.local/toolchain/` + the GNU-hosted Rust toolchain, pinned with a rustup
  override) and `scripts/cargo.sh` (the build wrapper that puts the toolchain on PATH —
  cargo's `[env]` cannot do that for build scripts). A build drops `golem.exe` at the
  repository root (git-ignored). **239 tests run locally, and the binary builds with no
  warnings at all**; CI keeps building MSVC.
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
  process. Of the interactive checklist, **the mouse wheel and tiny windows were real
  gaps, not things to verify** (2026-10-20): the screen never enabled mouse capture, so
  the terminal did not even send wheel events, and a three-row window drew a header and
  two rules with no menu on it. Both are fixed - one notch scrolls one row, and the
  header gives way below eight rows while the hint gives way below three. A resize
  already worked (the loop redraws every turn and ratatui re-reads the size). Still to
  walk by hand: **mintty and raw conhost**.
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
- **`scripts/report.sh` is gone** (deleted 2026-10-20). It emitted the old flat
  table and would have overwritten the redesigned `bench/RESULTS.md`, so it was
  retired in two steps: phase 6 routed `./golem report` and `golem.cmd report`
  to the binary, and the script was then removed under the migration gate -
  `report`'s gate is the binary's now. `./golem report` without a binary beside
  it says so instead of falling back to a stale table. `scripts/report.ps1`
  stays: it builds the *page* (not `RESULTS.md`), it is not stale, and it is
  what `golem.cmd report` uses on a checkout with no binary built yet.
- **The console's Models tab is done** (2026-10-20): `api/models` and the three edits
  behind it (`api/addmodel`, `api/delmodel`, `api/delpreset`), with the page's own
  capitalised field names, the preset file's `; Download:` comments and `gguf::read` for
  the parameters. Verified against the shell console running on the same machine: the
  two `api/models` answers agree on every field of all six real presets. The page's
  three dead buttons are wired too - `alias` and `engine` rewrite `.env` (checked
  against what is installed), `pull` is a launcher verb, and `golem.cmd` learned it.
  The console's Eval button now carries the `--long 200000` it promises.
- **`golem up` on Windows was broken** and is fixed by the same work: the binary asked
  `golem.cmd` for `up`, which that launcher has never had - it says `start`. The verb
  translation now lives in `Driver::verb`, which knows both spellings.
- **Phase 4 done (2026-10-20)**: `golem tune <preset>` sweeps, decides and gates. The
  measurement runs through the same launchers and bench script the shell tuner uses; the
  decisions are `golem_core::tune`'s. Verified against the script it ports - same preset,
  same flags, same candidates, one after the other: both verdicts are
  `engine=llama.cpp n_cpu_moe=0 ctx=8192` with decode within the few percent two
  single-run benches differ by, the overlay is the shell's file line for line, and
  `models.effective.ini` differs from the preset in exactly the keys the tune moved. A
  full run with the gate passes the 7372-token needle at 277.9 t/s at depth.
  **`--auto` is `auto_tune.rs` now**, driving this binary's own sweep per installed preset.
- **Phase 6 done (2026-10-20)**: `./golem` and `golem.cmd` hand `report`, `status`,
  `tui`, `web` and `tune` to the binary when it sits alongside; `up`/`down`/`restart`
  are deliberately not handed over (the binary drives those *through* these launchers,
  so it would be a loop on Linux), nor is `tune --auto`, nor `web status|stop`.
  `GOLEM_NO_BINARY=1` runs the shell everywhere.
- **Three bugs the work above shook out**, all worth remembering because each was
  silent: the `RESULT`-line parser rejected `vram=1209MiB` (the bench's own unit), so a
  good measurement read as "no usable result"; `Command::new("bash")` on Windows
  resolves to the **WSL launcher** in `System32`, so `scripts/*.sh` ran inside the Linux
  VM where the engine is not on localhost - the tell was `uname -s` saying `Linux` and a
  health check returning `code=000`; and `ram_free_gb` could not read this machine at
  all (MSYS `/proc/meminfo` has no `MemAvailable`, and PowerShell prints `19,2` in this
  locale), so the screen said 0.0 GB free on a box with 19 GB.
- **The shell's remaining commands are ported (2026-10-20, later)**: `logs`, `pull`, `bench`,
  `eval`, `claude`, `env`, `menu`, `tune --auto`, `setup` and `install`, and `./golem` /
  `golem.cmd` hand all of them over. What is left of the shell is `up` / `down` / `restart`
  (deliberately: the binary starts engines *by running these launchers*), `web` and
  `web status|stop` (they read the console pid file), and the scripts those launchers run.
  `scripts/report.sh` was deleted under the migration gate, and on 2026-10-20 the whole
  measurement cluster followed it - `scripts/benchmark-ai.sh`, `scripts/eval-model.sh`,
  `scripts/record-result.sh`, `scripts/tune.sh` and `scripts/tune-auto.sh` - because every
  one of their gates is the binary's now and no launcher runs any of them. `./golem
  bench|eval|tune` say "build the binary" when there is none, the way `report` already did.
  What stays on disk is the launchers' helpers (`download-model.sh`, `gguf-info.sh`,
  `install-llama-cpp.sh`, `health-check-ai.sh`), the launchers themselves, and the
  no-binary fallbacks (`tui.sh`, `web.ps1`, `report.ps1`, `setup-ai.sh`).
- **The recorder is the binary's** (`golem-core::record` + `golem/src/facts.rs`), and it was
  verified the way a port has to be: `scripts/record-result.sh` and
  `golem.exe bench|eval --replay <capture>` were run back to back against the same live
  engine on four real captures, and the two rows are byte-identical apart from `ts` and the
  machine's own live VRAM/RAM figures. `--replay` exists for exactly this check.
- **Two tuner bugs fell out of that work**, both silent, both fixed:
  `llama-fit-params` prints its `-ot` pattern with escaped dots (`blk\.3\.…`), and the port
  split the pattern on non-alphanumeric characters - which the backslash is - so it found no
  layers, fell through to the preset's own `n-cpu-moe`, and swept a different range than the
  shell (seed 32 against 35 on the reference box). The adopted winner sits in both ranges, so
  the verdict stands, but the rows around it do not describe the sweep the shell would run -
  which is a reason to re-measure, not to argue. And the plan's text was missing the `model`
  line, the `(windows mode)` tag and the per-engine seed/candidate list, which the shell
  echoes even in a dry run. `./golem.exe tune <preset> --dry-run` and
  `./scripts/tune.sh <preset> --dry-run` now agree line for line.
- **The measurement is the binary's too (2026-10-20, late)**: `bench` and `eval` are ports of
  `scripts/benchmark-ai.sh` and `scripts/eval-model.sh`, so the sweep measures itself in
  process and nothing on the path "clone → one command → tuned, running model" reaches a
  shell any more. The transport is a new shared one (`engine::{Base, post_json, get_json}`):
  the status client's socket with the deadline and the address the caller asks for, bodies
  de-framed (`Content-Length` truncates, chunked is unwrapped), and an `https://` base
  refused rather than half-honoured because the client has no TLS. Verified against both
  scripts on `qwen2.5-0.5b-instruct`, back to back on the same engine: `bench` prints the
  same shape, the same 600 and 3610 token counts and the same `draft_accept=-`, with
  346.7 vs 347.2 t/s decode and 20941 vs 20947 t/s prefill; `eval` is identical apart from
  one deliberate line, `tool_call :`, where the script's `json_str` cannot cross an escaped
  quote and prints `{\)` for the arguments while this prints them.
- **The scripts whose gates the binary replaced are gone** (2026-10-20): `report.sh`,
  `benchmark-ai.sh`, `eval-model.sh`, `record-result.sh`, `tune.sh`, `tune-auto.sh`. The
  measurement cluster went together because `tune.sh` called the other three. What stays is
  the launchers' helpers (`download-model.sh` is `./golem up`'s, `gguf-info.sh` is
  web.ps1's, `install-llama-cpp.sh` is start-local-ai.sh's), the launchers themselves, and
  the no-binary fallbacks (`tui.sh`, `web.ps1`, `report.ps1`, `setup-ai.sh`). Deleting those
  means porting `up`/`down`/`restart`, which phase 6 deliberately does not do.
- **`golem.toml` exists** and the binary reads it: nine keys whose only reader it is
  (`TEMPERATURE`, `TOP_P`, the five `TUNE_*`, `GATE_TOKENS`). See the Configuration section.
- **The two probes ran.** Strata refuses 236544 (recorded as a reject row, written up in
  docs/strata-models.md); llama.cpp b11755 passes the 200K gate but is **not adopted** - it
  buys prefill (1143 vs 871 t/s at 30K, 787 vs 672 at 200K) with decode (31.0 vs 35.2 t/s at
  depth, acceptance 57% vs 72%), and decode at depth is what a long session pays on every
  token. b11379 stays the pin; b11755 stays installed for a future paired retest.
- **The clean-machine walk ran**: a scratch clone with the binary shipped and the models
  already present starts and drives an engine with no shell at all. **The missing piece is a
  release asset** - see the walk section.
- **The launcher port has started** (2026-10-20, later): `golem down` kills the process
  tree and waits for the card itself, `up --dry-run` resolves the whole command line and
  agrees with `start-windows.ps1 -DryRun` flag for flag, and `golem_core::adapter` is the
  flag translation `flag.*` / `map.*` describes. `docs/plan-launchers.md` has the rest:
  install, spawn, restart, then the Linux and docker drivers.
- **The direction after this page (2026-10-20, from the user)**: *the binary is the app* -
  download, configure, tune and serve with no script behind any of it, because every script
  is a place that needs Git Bash, or WSL, or a second implementation for the other OS. The
  remaining work is one page each: [plan-launchers.md](plan-launchers.md) (`up` / `down` /
  `restart` in the binary, which also takes PowerShell off the launch path and lets the last
  `.env` keys move) and [plan-smart-tune.md](plan-smart-tune.md) (the tuner stops sweeping a
  grid: the frontier is bisected, engines that cannot serve `/v1/messages` are not measured,
  and refine spends one probe per knob). A **native UI** is a direction the user raised, not
  a task: the page stays until something better exists, and `tui.rs` is the proof that the
  console's data is a plain struct with actions as callbacks.
- **Next**: the re-bench on a quiet desktop from an emptied database, then `./golem report`.
- Worth borrowing when the console grows a LAN face: a `Host`-header check, since the
  current console binds 127.0.0.1 only and that is the reason it needs none today.

### The clean-machine walk, as far as it goes without the GPU (2026-10-20)

Cloned to a scratch directory (`git clone --local`) and inspected, without starting
anything. What a fresh clone has and has not:

- **no `golem.exe`.** The binary is git-ignored, so `clone → one command` cannot reach any
  of the commands ported above; the clone has only `./golem` and `golem.cmd`. Getting the
  binary onto the machine is either a release download (`.github/workflows/golem-bin.yml`
  builds one per OS on a tag) or `./scripts/cargo.sh build`, which needs Git Bash *and*
  `scripts/setup-rust-toolchain.sh` first (a ~264 MiB mingw-w64 into the git-ignored
  `.local/toolchain/`). **A release asset is the missing piece**, not another port.
- **no `.env`** - bootstrapped from `.env.example` by both front doors, which is fine.
- **no models** (`data/models/*` is git-ignored) and **no `.local/`** (no engine build, no
  toolchain), so the first `up` is a ~20 GB download and the first `install` a full build.
- **the measurement harness was bash, and is not any more.** When this was first walked,
  `bench`, `eval` and therefore `tune` ran `scripts/benchmark-ai.sh` and
  `scripts/eval-model.sh` through `driver::bash()` - Git Bash, by name, on Windows. Both
  are the binary's now (see the status block), so the *running* half of the walk no longer
  needs a shell: `up` drove PowerShell and `curl.exe` already, and the measurement no
  longer needs bash at all.

That leaves one thing: **ship the binary.**

### The walk, run (2026-10-20)

Cloned to `C:\workspaces\golem-walk`, put the built `golem.exe` and the checkout's `.env`
in it, and junctioned `data/models`, `data/tuned` and `.local` at the real ones - which is
what a machine that already has the models and the engine looks like, and the only way to
test the launch path without a 20 GB download. Then, with no shell typed:

- **`./golem.exe up` started the engine.** It drives `golem.cmd` → `cmd.exe` →
  `powershell.exe -File start-windows.ps1`, and there is no bash anywhere on that path
  (`golem.cmd`'s `golem-bash.cmd` bridge is only on the bench/eval/tune/pull labels, and
  those are the binary's now). PowerShell **5.1** is what Windows 11 ships; 7 is not needed.
- `status` reported the engine up and all six presets listed; `env` printed the exports;
  `logs --no-follow` tailed the engine log; `pull ornith-1.5-35b` found the files already
  present and finished; `down` stopped it. None of them reached a shell.

So the answer to phase 6's question is: **the only thing missing is a release asset.** The
walk's "tuned" half - `tune --auto` from the clone - is the re-bench the status block names
as next: the same code path `up` just exercised, plus the sweep.

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
  *Half met 2026-10-20: the 0.5B half is verified (identical verdict, overlay and
  effective preset against `scripts/tune.sh` run on the same candidates). The 35B half
  waits for the final measurement pass on an idle machine, which is also where the
  `--adopt` path gets its real exercise.*

### Phase 5 — TUI

- ratatui mirror of `scripts/tui.sh`: status, start/stop, tail, menu parity, Windows
  Terminal + mintty + raw conhost.
- **Done when:** the checklist from the interactive TUI pass (mouse wheel, arrows,
  resize, tiny windows) behaves in the binary.
  *Met for what a test can hold 2026-10-20: the wheel scrolls, a resize redraws, a tiny
  window keeps the menu, and the screen is asserted at 20x6, 12x5, 40x3 and 1x1. mintty
  and raw conhost still need a person at a terminal.*

### Phase 6 — switchover

- `golem.cmd` / `./golem` exec the binary when present; scripts deleted only per the
  migration gate. `.env` stays the config surface (GOLEM_* overrides included).
- **Done when:** clean-machine walk (clone → one command → tuned, running model) uses the
  binary everywhere and needs neither Git Bash nor PowerShell 7.
  *Partly met 2026-10-20: every command is the binary's and the handover is in place and
  per command, and each side was run to check which one answers. The clean-machine walk
  still does not pass, and the reason has moved: it is no longer a missing port but a
  missing artefact and a missing harness. A fresh clone has no `golem.exe` at all (the
  binary is git-ignored), so no ported command is reachable from it; and `bench`, `eval`
  and `tune` run `scripts/benchmark-ai.sh` / `scripts/eval-model.sh` through Git Bash, so
  "a tuned, running model" needs a shell until those two are ported or rewritten. `up`
  itself does not need one on Windows. The full reading is in the status block at the top.*

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
- **A key moves when the binary owns every reader of it** (2026-10-20, the answer to
  "will we change `.env` for a config file or not"). Almost every key today has two
  readers - the binary *and* a shell driver that sources `.env`: the launchers read
  `ENGINE`, `MODELS_PRESET`, `MODEL_ALIAS`, `LOCAL_AI_PORT` and `LOCAL_AI_API_KEY`,
  and `tune.sh` reads the `TUNE_*` floors and `GATE_TOKENS`. Moving a key while a
  driver still reads it means two sources of truth for one setting, which is the
  failure this section exists to avoid. So the order falls out of the porting order:
  the tuner's knobs move when `tune --auto` is the binary's, `ENGINE` /
  `MODELS_PRESET` / `MODEL_ALIAS` move when `up` is, and `golem.toml` is written in
  one deliberate pass at the tail of the switchover rather than alongside the ports.
  Until then: **no new keys in `.env`.** A setting only the binary owns goes straight
  into `golem.toml` from its first day, so the surface that has to migrate only ever
  shrinks.
- **Where this stands after the ports (2026-10-20).** The migration has started, and the
  rule above is what decided the first nine keys. `TEMPERATURE`, `TOP_P`,
  `TUNE_MIN_DECODE`, `TUNE_MIN_PREFILL`, `TUNE_MIN_DEPTH_DECODE`, `TUNE_CTX_POLICY`,
  `TUNE_RUNS`, `TUNE_PREFILL` and `GATE_TOKENS` are in `golem.toml` now: the shell drivers
  never read any of them, and the scripts that did are the measurement harnesses the binary
  is. `state::env_value` reads `golem.toml` first and `.env` second - one mechanism, so a
  key is in one file and never both - and the writers route a TOML-owned key to the TOML
  file, because writing `.env` for a key the reader takes from elsewhere looks like it
  worked and changes nothing.
  The rest still cannot move: the launchers source `.env` for `ENGINE`, `MODELS_PRESET`,
  `MODEL_ALIAS`, `LOCAL_AI_PORT` and `LOCAL_AI_API_KEY`, and they are still the shell's
  because the binary starts engines by running them. So the order is unchanged - `up` (or
  the launchers' `.env` reads) first, then the rest of the file.
- **Nothing about this is urgent.** It is written down so the shell is not extended
  with configuration work the binary will own (2026-10-20: "we don't want to add
  features with the scripting way if we will have to rebuild in rust").

## Non-goals

- No gateway, no MCP, no web search (unchanged).
- No engine binaries bundled — drivers still install/fetch engines as today.
- CI never runs models or adopts anything.
