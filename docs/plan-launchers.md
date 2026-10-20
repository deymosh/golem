# Plan: the launchers move into the binary

Status: **decided 2026-10-20**, and most of it is built - see "Where this stands" at the
bottom. This is the last piece of [plan-portable-binary.md](plan-portable-binary.md)'s
configuration migration, and the reason it exists is one sentence from that plan: *a key
moves when the binary owns every reader*.

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
  that never comes. **The port does this and the pipe still stays open on Windows** -
  `Stdio::null()` cannot stop the child inheriting the caller's pipe as an extra handle, and
  neither can the alternatives that were measured. It is the one known gap in the port, it
  costs a piped `up` (not a typed one, and not one whose output goes to a file), and
  [plan-service.md](plan-service.md) sets out the three ways to close it.
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
   **Done**: `plan` resolves both shapes (router passes `--models-preset`, a single-model
   engine gets the section's flags translated through the adapter), and `up` installs,
   spawns, writes the pid and polls health. What is left is the live paired start below.
3. The installer (the zip path), verified against a fresh `.local/engines/`. **Done** - the
   zip path ran against the real release in a scratch home, and the tree it produced is the
   one the script produces.
4. `restart`, **done**; the Linux and docker drivers are not started.

## How it gets verified

- `golem down` and `scripts/stop-windows.ps1` on the same running engine: same pid file, same
  VRAM after, same refusal when nothing is running.
- `golem up` and `start-windows.ps1`: same pid file, same served model, **the same
  `/props` args** - the command line is the thing worth diffing, not the banner.
- A paired bench on each, because the launch flags decide the speed.
- The clean-machine walk again, this time with no PowerShell in the path at all.

### Where this stands (2026-10-20, later)

**Done and measured.** `down` kills the tree and waits for the card. `up` resolves the whole
command line, installs the pinned build when it is absent, spawns the server with its own
`--log-file` and its stdout/stderr closed, writes the pid, and waits for `/health` with a
deadline *and* an early exit. `restart` is the two. Windows and native mode both take this
path; docker, an engine that installs itself (`launcher = strata`) and single-model mode
with no preset still hand the work to the shell launcher, which is why neither script is
deleted yet. (The last of those three is ported now - see below.)

- **The command line agrees.** `./golem.exe up --dry-run` and
  `scripts/start-windows.ps1 -DryRun` print the same line flag for flag; the only difference
  is the separator in two paths (`data/tuned/...` against `data\tuned\...`).
- **The installer agrees.** `release::ensure` run against the real b11379 release in a
  scratch home downloaded `llama-b11379-bin-win-cuda-13.4-x64.zip` (146 MB) and
  `cudart-llama-bin-win-cuda-13.4-x64.zip` (404 MB), extracted both with
  `%SystemRoot%\System32\tar.exe`, landed a working `llama-server.exe`, wrote the `BUILD`
  marker, and did nothing at all on a second call. The 56 files it left are the same 56 the
  script's own install of that build holds, name for name. It needed **no new crate**: the
  transport is curl, which `pull.rs` already uses, and the extractor is System32's bsdtar -
  Git's GNU tar on PATH cannot read a zip at all.

Two things the port found on the way, both silent if left:

- **The tuner's per-run overrides.** `plan` read `ENGINE`, `MODELS_PRESET` and `MODEL_ALIAS`
  straight from the configuration, while the launcher reads all three through
  `$env:GOLEM_*` - which is how `tune.rs` points one run at one candidate. A sweep would
  have restarted the same preset for every candidate and the gate would have scored
  whatever `.env` names. Fixed, and tested in `tests/launch_plan.rs`, which runs the real
  binary in its own process because these are process-environment variables.
- **Two shapes `up` resolved wrongly rather than refusing**: a single-model engine whose
  preset has no section for `MODEL_ALIAS` (the launcher refuses; the plan built a line with
  no `--model` on it), and no preset at all. Both now say so and hand the work back, the
  same way Strata does.

**The live paired start has run** (2026-10-20). `golem up` brought the engine up in router
mode on b11379, `down` stopped it and returned the card to the desktop, and
`start-windows.ps1` then started the same engine: **the two `/props` answers are identical**
(`role: router`, `max_instances: 1`, `models_autoload: true`, the same `build_info`), the
same pid file is written, and the banner and the `$env:` block match the launcher's line for
line. The paired bench is not run: the two command lines are already identical, so it would
measure the same configuration twice. What the run did find is the pipe above, which is why
`up` is not handed over yet.

**The model fetch is in too.** `up` reads the preset's `; Download:` comments before it
starts anything, exactly as `cmd_up`'s `ensure_models` and `start-windows.ps1` do: the
default section's missing files are fetched, another section's are a warning naming
`golem pull <section>`, a preset whose alias names no section is fetched whole (a new
preset), and a section that names a model no `; Download:` comment covers is fatal for the
section being served and a warning for the rest. The download itself is `pull.rs`'s - one
downloader, one completeness rule, so a model fetched by `up` and one fetched by `pull` are
the same bytes.

Measured on a scratch home with the engine junctioned in (no 550 MB install) and a
deliberately empty `data/models`: `up` warned about the other section, fetched the 0.5B
(379 MB, size-checked, `.sha256` written), and then started the engine - a fresh machine's
first `up` is a download and then a working server, which is what the launchers promise.
`up --dry-run` reports what a real one would fetch, since it installs and downloads nothing.

**Single-model mode with no preset is ported too**, which is the shape a fresh clone has
(`.env.example` ships `MODELS_PRESET` commented out). `up` builds that command line from
`.env` the way the launcher does - `MODEL_FILE`, `CONTEXT_SIZE`, `GPU_LAYERS`, `jinja`,
`flash-attn on`, the cache types, `LOAD_MODE`, `SPEC_TYPE`, `DRAFT_FILE`, then `EXTRA_ARGS`
verbatim and last - and the closing line names the model *file* there and the alias when a
preset chose the model, as the launcher's does.

Measured in a scratch clone with `data/models` and `.local` junctioned in: `up --dry-run`
and `start-windows.ps1 -DryRun` print the same line flag for flag, `EXTRA_ARGS` included and
both absolute paths included, with the path separator the only difference. That is the
verification this item was waiting on, and it needed the clone because the script reads
`.env` from the checkout it sits in.

**Still to do, in order.**

1. **The piped `up`**, above: the one place the port does not behave like the launcher, and
   the reason `./golem` and `golem.cmd` keep their `up`/`restart` arms and both scripts stay.
   [plan-service.md](plan-service.md) sets out the three ways to close it - and records that
   the service alone does *not*, because the inheritance happens at whichever process calls
   `CreateProcess`, not at whichever process owns the engine.
2. **The Linux and docker drivers.** Linux is the same code as Windows with different asset
   keys and no zip to work around. Docker is a larger question than a driver:
   [plan-deploy-modes.md](plan-deploy-modes.md) argues it stops being a deploy mode at all and becomes an
   image whose entrypoint is golem - which is another reason the service comes first.

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
