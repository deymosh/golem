# Architecture

Golem serves one local model to Claude Code and to any OpenAI client. It is one Rust
binary (`golem-rs/`) and one configuration file (`golem.toml`); the inference engine is a
pinned upstream release that the binary installs, starts and supervises.

Three rules shape everything below:

- **The engine speaks the APIs itself.** llama.cpp serves the Anthropic Messages API
  (`/v1/messages`) and the OpenAI API (`/v1/chat/completions`) natively, so there is no
  gateway, no translation proxy and no MCP server, and never will be. An engine that
  cannot serve `/v1/messages` can be measured but never adopted.
- **Nothing is adopted on faith.** A model, an engine build or a set of flags becomes a
  default only after `golem eval --long 200000` passes on real hardware.
- **No script on any path.** Every command is the binary's, on Windows, Linux and macOS
  alike; a release is the executable plus the `golem.toml` it writes on first run.

## Requests

```
  Claude Code / OpenCode / Aider / any OpenAI client
        │  Anthropic  POST :8000/v1/messages         (x-api-key or Bearer)
        │  OpenAI     POST :8000/v1/chat/completions (Bearer)
        ▼
  llama-server  (router: one model resident, loaded on first request)
```

Both APIs hit the same process, the same weights and the same prompt cache. Tool calling
needs `--jinja`, which every preset and the single-model command line set. Verified
against this stack: streamed and non-streamed Anthropic responses, `thinking` and
`tool_use` blocks, a full tool-use round trip. Upstream documents the endpoint with the
caveat that it is a native implementation rather than a certified one.

The engine binds `LOCAL_AI_HOST` (default `127.0.0.1`) on `LOCAL_AI_PORT` (8000) and
requires `LOCAL_AI_API_KEY`. Anyone who can reach the port can use the GPU: change the key
before binding `0.0.0.0`, and firewall the port if the LAN is not trusted.

## The binary

```
golem-rs/crates/
  golem-core/   pure and tested: JSON, golem.toml, INI presets and overlays, GGUF header,
                results.jsonl, RESULT lines, the report, and every tuner decision
  golem/        the binary: commands, engine, service, measurement, console, screen
```

`golem` is a library (`lib.rs`, every module below) and a thin `main.rs` that parses the
command line. The binary's layers, top to bottom:

| Layer | Modules | Owns |
|---|---|---|
| Front ends | `main`, `desktop`, `app`, `tui`, `web`, `runner` | the CLI, which front end a bare `golem` opens, the window and tray (`--features desktop`), the full-screen screen, the console; `runner` is the one path a menu entry or a page button takes |
| Commands | `bench`, `eval`, `measure`, `tune`, `auto_tune`, `pull`, `install`, `setup`, `logs`, `client` | one command each; `measure` records a row after `bench`/`eval` |
| Engine | `service`, `launch`, `release`, `engine` | who owns the engine, how it is started and stopped, how its build is installed, how it is spoken to |
| Foundation | `config`, `paths`, `machine`, `proc`, `state`, `facts`, `models` | the configuration, the home's files, the hardware probes, child processes, what the console shows, what a row says |

`golem-core` decides; `golem` measures and acts. A tuner decision, an INI merge or a
results reduction is a pure function in `golem-core` with its own tests, so the parts that
need a GPU are thin.

## The service owns the engine

`golem service` is the only process that starts the engine, and it hosts the console
(`CONSOLE_HOST`:`CONSOLE_PORT`, `127.0.0.1:8787` by default) for as long as it runs. The
console has no login; listening beyond loopback is a configuration choice the service
warns about at every start. `up`, `down`,
`restart` and `status` from the CLI, the screen's menu and the console's buttons are all
clients of it: they send a verb, the service runs it (`launch.rs`), and the output streams
back.

A bare `golem` picks its front end from how it was started (`desktop.rs`): double-clicked
on a desktop (Explorer as the parent on Windows; a graphical session with no terminal on
Linux), it starts the service if needed and opens the console - in the Golem window in a
build with the desktop feature, in the browser otherwise; in a terminal, the full-screen
screen; with neither, the usage. golem.exe is a console program, so on Windows a
double-click still opens a console window for a moment; `golemw.exe`, a windowed
launcher built beside it, starts the app with none.

Why a long-lived owner rather than an `up` that spawns and exits:

- **A killed command must not orphan the engine.** The engine is the service's child, so
  it goes when the service goes; a model left resident with nothing pointing at it once
  cost a session's testing (11.7 GB of VRAM that `down` could not see).
- **A container needs a PID 1** that owns the engine and stays up. `golem service --up`
  is that process; `up` returns as soon as the engine is healthy and would end the
  container with it.
- **A client that spawns nothing cannot leak handles.** On Windows `std` spawns with
  `bInheritHandles = TRUE`, so an engine started by `golem up | tail` held the caller's
  pipe for the engine's lifetime. The one spawn a client makes - the service, on the first
  verb after a boot - goes through PowerShell's `Start-Process`, whose `ShellExecuteEx`
  hands the service no handles; PowerShell holds the pipe for the half-second it runs.

**Protocol.** One JSON object per line over loopback TCP. The service binds an ephemeral
port and writes it, with a per-run token, to `.local/golem-service.json`; loopback is
machine-wide on Windows, and the token is what stops a stray local process from stopping
the engine.

```text
-> {"token":"...","verb":"restart","env":{"GOLEM_MODELS_PRESET":"data/tuned/candidates/x.ini"}}
<- {"event":"line","text":"[ .. ] installing llama.cpp b11379 (cuda)..."}
<- {"event":"done","ok":true,"code":0}
```

Lines stream as they happen - a first `up` installs an engine and downloads 20 GB, and
silence for ten minutes reads as a hang - and `done` is the acknowledgement a client waits
for before it reports anything.

**Progress.** A long step says how far it has got in its own output, as lines a reader
draws rather than shows (`golem_core::progress`):

```text
PROGRESS 4194304/20971520 bytes pull: Ornith-1.5-35B-Q4_K_M.gguf
PROGRESS 6/10 steps tune: context, load 7
PROGRESS -
```

A download counts its bytes against the size the Hub reports, the tuner its model loads
against what it still expects to spend (the budget in [tuning.md](tuning.md#the-search)),
and eval its requests. They are written only when stdout is not a terminal, where curl's
meter and the narration already say it. The runner opens and closes every job with the
clear; the console's state carries the newest line as `job_progress` and leaves them all
out of the job tail; the screen draws it on its bottom line; and `golem up`/`restart` on
a terminal draws the lines the service streams as one bar redrawn in place.

**Concurrency.** Each connection is served on its own thread. One *start* (`up`,
`restart`) runs at a time and a second is refused; `down` and `status` are answered while
a start runs, so a start that hangs can be stopped (its health wait sees the child go and
reports it). Ctrl+C or SIGTERM stops the engine and then the service - a Linux PID 1 with
no SIGTERM handler would ignore `docker stop`.

**Overrides.** A request may carry `GOLEM_<KEY>` values for ENGINE, MODELS_PRESET,
MODEL_ALIAS and LOCAL_AI_HOST. They are data handed to the plan for that one verb, laid
over the service's own `GOLEM_*` environment, and never written into the process
environment. This is how the tuner restarts the engine on a candidate preset, and how a
container keeps `0.0.0.0` on every restart. A service that a client starts on demand
inherits none of the four: the client's overrides belong to its request, not to every
later one.

## Starting an engine

`launch::plan` resolves the command line from `golem.toml`, the overrides, the engine's
adapter and the preset, and spawns nothing - `golem up --dry-run` prints it.

- **The adapter** (`engines/<name>/engine.ini`) names the engine's pinned `build`, the
  GitHub release archives per platform and GPU backend (`windows_asset_cuda`,
  `linux_asset_rocm`, ...), the server's name, its capabilities (`api_anthropic`,
  `router`) and the flag translation (`flag.*`, `map.*`) from the canonical llama.cpp
  names. The platform is detected, never configured; `GPU_BACKEND = "auto"` reads the card.
- **Install** (`release.rs`) downloads the matching archives with `curl` and extracts them
  into `.local/engines/<name>/<build>-<backend>/`, writing a `BUILD` marker only when every
  archive extracted. On Windows the extractor is `%SystemRoot%\System32\tar.exe` (bsdtar,
  which reads zip) - the `tar` on PATH under Git for Windows is GNU tar and cannot.
  Releases only, on purpose: the pin is the artifact the 200K gate adopted, and a source
  build of today's upstream is a different binary.
- **Router mode** (`MODELS_PRESET` set, adapter `router = true`): `--models-preset <file>
  --models-max 1`, and the preset carries every per-model flag. A named preset that is not
  on disk is refused - serving the single-model keys instead would load a different model.
  **Single-model mode** builds the line from `MODEL_FILE`, `CONTEXT_SIZE`, `GPU_LAYERS`,
  the cache types, `LOAD_MODE`, `SPEC_TYPE`, `DRAFT_FILE`, then `EXTRA_ARGS` verbatim.
- **Models** the default section names (`; Download: <repo> -> <file>` comments) are
  fetched before the start; other sections' missing files are a warning naming
  `golem pull <section>`.
- **Spawn and wait.** The server writes its own `logs/llama-server.log` and its stdout and
  stderr are closed. `up` polls `/health` for up to ten minutes (a loading engine answers
  503) and fails at once, with the log's tail and the exit code, if the child dies first.
- **Stop.** `down` kills the process tree (in router mode each model is a child process)
  and waits for the card's memory to come back: a start that begins before the driver
  reclaims the VRAM fails with `cudaMalloc failed: out of memory`.

## Configuration and the home

`golem.toml` is the only configuration, read through `config::get` and written through
`config::set`, which replaces one line and keeps every comment. It is git-ignored - it
holds the port, the API key and the model this machine serves - and
`golem.toml.example` is the tracked template, compiled into the binary and written out on
first run.

The **home** is `GOLEM_HOME` if set, otherwise the directory the binary lives in. A
release folder populates itself; the dev build that `./scripts/cargo.sh` drops at the
repository root uses the checkout's own files.

| Path | What |
|---|---|
| `golem.toml` | this machine's configuration |
| `config/models.ini` | the shipped presets (router mode) |
| `engines/<name>/engine.ini` | engine adapters |
| `data/models/` | GGUF files |
| `data/tuned/<host>/<engine>/<preset>.ini` | each gated tune's overlay |
| `data/tuned/models.effective.ini` | the base presets with this machine's overlays merged in |
| `bench/results.jsonl` | every measurement, append-only ([bench/README.md](../bench/README.md)) |
| `logs/llama-server.log`, `logs/web-job.log` | the engine's log; the console's job log |
| `.local/` | installed engine builds, the pid file, the service endpoint, the dev toolchain |

## The container

One `Dockerfile`, `--build-arg GPU_BACKEND=cuda|rocm`: a builder stage runs `cargo build
--release`, and the runtime stage (`nvidia/cuda:13.4.1-runtime-ubuntu24.04` or
`rocm/dev-ubuntu-24.04`) gets the binary, the adapter and the presets, runs
`golem install` to bake in the pinned engine, and starts `golem service --up` as PID 1.
`compose.yml` picks the vendor by profile, mounts `golem.toml`, `data/models`, `bench/`,
`logs/` and `data/tuned`, and sets `GOLEM_LOCAL_AI_HOST=0.0.0.0` because a container's
port is published. Docker is packaging, not a mode: the same binary, the same commands,
`tune` and `eval` included.

Two things the container needed: `libgomp1`, which the llama.cpp release requires and
neither base image carries (without it the server exits 127 and logs nothing), and the
release's `.so` files linked into `/usr/local/lib` with `ldconfig`. On a Windows desktop
the container is the slow path - WSL2's paravirtualized GPU costs about a third of decode
([windows-native.md](windows-native.md)).

## Platform facts that cost time

- **VRAM spills silently on Windows.** Past the card's size the driver moves allocations
  to shared system memory: no error, prefill falls ~40%. The tuner treats the card's size
  less a headroom as a hard wall ([tuning.md](tuning.md)).
- **A closed loopback port drops the SYN** on the reference box instead of refusing it, so
  a connect waits its whole timeout (2.07s measured). Every loopback client uses a 500ms
  connect timeout.
- **`bash` on Windows resolves to the WSL launcher** in `System32`, not Git Bash - and the
  environment does not cross into WSL. That is how a 0.5B candidate was once "gated" with
  the 35B's numbers. Nothing in golem runs a shell.
- **PowerShell prints numbers in the machine's locale** (`19,2`), and MSYS's
  `/proc/meminfo` exists on Windows with no `MemAvailable`. `machine.rs` handles both once.
- **`mlock` on a 20 GB model leaves a 32 GB box ~400 MB.** Reloading it per candidate ran
  Windows out of pages (`STATUS_IN_PAGE_ERROR` on `cmd.exe`), so the tuner's sweep points
  load with mmap and only the gate locks.

## Memory regimes

- **Dense model, all in VRAM.** Weights are resident from boot and the KV cache grows with
  the conversation (~2.3 GiB per 32K at q8_0); a 9B Q4 on 12 GB caps near 100K context.
- **MoE with experts in RAM.** Attention, the dense parts and the KV cache stay on the
  card; expert weights live in system RAM. `--n-cpu-moe N` keeps only the first N layers'
  experts in RAM and the rest on the card - lower is faster and costs VRAM. Decode runs the
  RAM experts on the CPU; prefill streams them to the card once per ubatch, which is why
  `-ub 2048` more than doubles prompt throughput over the default 512.
- **MTP speculative decoding.** Qwen3.5-family models carry a Multi-Token Prediction head
  that drafts tokens the model verifies in one pass: identical output, much faster decode
  when acceptance is high. `spec-type = draft-mtp`, with `spec-draft-model` when the head
  is a separate file.
