# Plan: the launchers move into the binary

Status: **decided 2026-10-20**, not started. This is the last piece of
[plan-portable-binary.md](plan-portable-binary.md)'s configuration migration, and the reason
it exists is one sentence from that plan: *a key moves when the binary owns every reader*.

## Why

Every command is the binary's, and so is the measurement. The only `.env` readers left are
the launchers - `./golem`, `golem.cmd` and `scripts/start-windows.ps1` - which read
`ENGINE`, `MODELS_PRESET`, `MODEL_ALIAS`, `LOCAL_AI_PORT` and `LOCAL_AI_API_KEY` (plus the
flags they pass). Those five keys cannot move to `golem.toml` while the launchers read
`.env`, and keeping a copy in both files is the two-sources-of-truth failure the
configuration section exists to avoid.

Porting `up` / `down` / `restart` into the binary removes the last readers:

- the remaining keys move to `golem.toml`, and `.env` becomes what its name says - a shim
  for the shell, then nothing at all;
- **Windows stops needing PowerShell to start an engine.** Today `up` goes
  `golem.cmd` → `cmd.exe` → `powershell.exe -File start-windows.ps1`; the binary can spawn
  `llama-server.exe` itself. That is what the clean-machine walk wanted and settles for
  because PowerShell 5.1 happens to be present.
- `up` stops being a place where the config can disagree with the binary about what is
  being served - the class of bug that gated the wrong model once already.

## What `up` has to do

The shell's behaviour, not its intent - `scripts/start-windows.ps1` is the contract:

1. **Resolve the engine**: `ENGINE` (→ `golem.toml`), then `engines/<name>/engine.ini`.
2. **Resolve the build**: `ENGINE_BUILD` if set, else `LLAMA_WIN_BUILD` for llama.cpp
   (which shadows the ini today - one of the two goes), else the ini's `build`.
   `latest` follows upstream.
3. **Install it when missing**: the release zip matching `windows_asset_<backend>` plus the
   `windows_runtime_<backend>` zip, extracted into
   `.local/engines/<name>/<build>-<backend>/`. This is the one piece that needs something
   new: **Rust's std cannot unzip.** Decision needed (below).
4. **Resolve what is served**: router mode passes `--models-preset <file>`; single-model
   mode turns the section's flags into the command line, which is the flag translation the
   adapter's `flag.*` / `map.*` keys describe.
5. **Spawn** with the log file, write `.local/llama-server-windows.pid`, poll `/health`, and
   print the same `$env:` block the launcher prints.
6. `down`: kill the process **tree**, then wait for the card (the settle the launcher has
   now). `restart` is the two.

Two details from the launcher that a port must keep, both of which bite:

- **The server logs to its own `--log-file`; its stdout/stderr must be closed.** The launcher
  redirects them only for Strata. Handing the child our pipes leaves the pipe open as long
  as the server runs, and anything reading our output (`./golem up | tail`) waits for EOF
  that never comes.
- **Health is a deadline plus an early-exit check**: ten minutes, `/health` every three
  seconds, and the moment the child has exited, print its log tail and fail with the exit
  code - instead of polling a dead process for ten minutes.

Linux and docker keep a driver: native spawns `.local/bin/llama-server` (what
`scripts/install-llama-cpp.sh` builds), docker runs `docker compose up -d`. Both are
already process-level, so neither needs a shell.

## The dependency question, answered

Unzipping, and getting the zips at all. The binary has **no TLS** (see `engine::request`),
and GitHub is HTTPS only, so the transport has to be something that already does: `curl`,
which `pull.rs` already uses for Hugging Face and which ships with Windows 10+ and every
Linux and macOS.

For extraction, `tar` does the job and ships everywhere too - `C:\Windows\System32\tar.exe`
is bsdtar, and bsdtar reads zip. **So no new crate.** Two details:

- **Pick the system tar by path, not by name on PATH.** In a shell that has Git for Windows,
  `tar` resolves to `Program Files\Git\usr\bin\tar.exe`, which is GNU tar and cannot read a
  zip at all ("This does not look like a tar archive"). This is the same trap as
  `Command::new("bash")` being the WSL launcher: use `%SystemRoot%\System32\tar.exe` when it
  exists, and fall back to `tar`.
- `curl -fL` for the asset URL, then `tar -xf <zip> -C <dir>`, then the wrapping-top-folder
  fixup the launcher already does, then the `BUILD` marker.

## Order of work

1. `down` first: it is small, it is the one with a correctness rule (kill the tree, wait for
   the card), and it can be verified against `stop-windows.ps1` on its own. **Done**:
   `golem down` kills the tree and waits for the VRAM, and `./golem` / `golem.cmd` hand
   `down` / `stop` to it.
2. `up` for **router mode only**, then single-model. Router mode is what this box serves.
   **Half done**: `up --dry-run` resolves everything and prints the command line, and it
   already agrees with `start-windows.ps1 -DryRun` flag for flag (only a path separator
   differs). What is missing is the two things it would then *do*: install the pinned build
   when it is absent, and spawn/poll/pid.
3. The installer (the zip path), verified against a fresh `.local/engines/`.
4. `restart`, then the Linux and docker drivers.

## How it gets verified

- `golem down` and `scripts/stop-windows.ps1` on the same running engine: same pid file, same
  VRAM after, same refusal when nothing is running.
- `golem up` and `start-windows.ps1`: same pid file, same served model, **the same
  `/props` args** - the command line is the thing worth diffing, not the banner.
- A paired bench on each, because the launch flags decide the speed.
- The clean-machine walk again, this time with no PowerShell in the path at all.

## What stays

`scripts/start-*/stop-*` stay until the port is verified on both engines and all three
modes; then they go under the migration gate like the measurement cluster did. `golem` and
`golem.cmd` keep their remaining arms (`web status`/`stop`, the no-binary fallbacks) and
their `up`/`down`/`restart` arms start saying "build the binary", as `report` does.

## Risk

This is the riskiest code in the repository - three deploy modes, a pinned build, a router
preset - and it is the one the plan deliberately deferred. Nothing here should be adopted on
a single run: the same engine must start from the binary and from the script, produce the
same `/props`, and bench within noise of each other, before either script is touched.
