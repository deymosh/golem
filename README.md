# Golem

**A local AI construct for coding agents.**
One GPU, one llama.cpp server, zero tokens leaving your LAN. It speaks both the
**Anthropic Messages API** and the **OpenAI API**, so Claude Code — or any OpenAI client —
plugs straight in. The model can search the web: golem runs the search itself, from your
machine, on DuckDuckGo with no key and no account, and labels every search it runs. The
search query and the pages fetched are all that leave your machine.

```
        ▟█▙   forge  ->  animate (load a model)  ->  it works
        ▜█▛
```

```
  Claude Code / OpenCode / Aider
        │
        │ Anthropic :8000
        │ OpenAI    :8000/v1
        ▼
  ┌───────────────────┐
  │ golem :8000       │──── web search and fetch (DuckDuckGo, from this machine)
  ├───────────────────┤
  │ llama-server :8001│
  │ local GGUF model  │
  └───────────────────┘
```

Two models out of the box, both holding Claude Code's **200K window** on a 12 GB GPU with
32 GB of RAM, both reading images:

- **The coder, `ornith-1.5-35b`** — a 35B mixture of experts (3B active) served at 248K
  context, most experts in system RAM, MTP speculative decoding: ~63 tok/s.
- **The everyday model, `ornith-1.5-9b`** — a dense 9B entirely on the card, served at
  262K context: ~54 tok/s, and the desktop stays usable.

Any other GGUF on Hugging Face is one `golem pull` away, and `golem tune` fits it to the
machine. Measured numbers: [docs/models.md](docs/models.md).

---

## Requirements

| | |
|---|---|
| GPU | NVIDIA (CUDA) or AMD (ROCm or Vulkan) with 12 GB VRAM — detected automatically; dense models up to ~9B fit fully in VRAM; MoE models of the 30B class run with expert weights offloaded to system RAM |
| RAM | 32 GB recommended for MoE offload (16 GB is enough for dense models) |
| OS | Linux, or Windows natively (no Docker, no WSL, no Git Bash). macOS on Apple Silicon is experimental: it builds, passes the tests and serves on Metal, and reads the share of memory Metal may use as the card, but has not been run on a real Mac |
| Docker | optional: Engine or Desktop + Compose v2 + GPU passthrough, for the container |
| Disk | ~17 GB for the default model (less for smaller models) |

## Quickstart

One binary. It creates `golem.toml` beside itself on first run, fetches the pinned engine
build and the model it needs, and starts serving.

```bash
golem up                     # install the engine if missing, fetch the model, start inference
golem status                 # health check
golem claude                 # launch Claude Code pointed at the local model
golem pull owner/repo        # any GGUF repo on Hugging Face: picks the quant, writes the preset
golem bench                  # speed: decode + prefill tok/s
golem eval --long 200000     # quality gate: tool calls + 200K needle
golem tune                   # auto-tune the offload split for this machine
golem                        # double-clicked: the console in the browser; in a terminal: the screen
golem down                   # stop the engine (the service stays; `golem service stop` ends it)
```

From a checkout the binary is `./golem.exe` (Windows) or `golem` (Linux/macOS) at the
repository root, and a clone builds it with `./scripts/cargo.sh build`. Edit `golem.toml` to
change anything — the model, the port, the key, the context; the defaults work as-is.
`golem pull` fetches the model explicitly; `up` does it automatically when a file is missing.

**Auto-tune** (`golem tune [preset]`) answers "what is the best way to run this model on
this machine?" with no AI and no hand-editing, in as few model loads as it can: it bisects
the expert offload split from llama.cpp's own placement tool, spends one probe on each knob
that can still matter, predicts the largest context the card holds from two measured
points and confirms it, holds every candidate to a **speed floor** (decode 25 / prefill
350 / at-depth 15 t/s by default), and gates the winner with the 200K eval. Nothing that
serves changes until the gate passes; `--adopt` then points `golem.toml` at the result.
`golem tune --auto` picks the best installed model first. How it decides:
[docs/tuning.md](docs/tuning.md).

**Console.** `golem service` hosts a page on `127.0.0.1:8787` (`CONSOLE_HOST`,
`CONSOLE_PORT`) for as long as it runs, whether or not an engine is up: live results from
`bench/results.jsonl`, every preset with its file and state, start/stop/restart/bench/eval/
tune buttons, the running engine's log, and pulling a model from Hugging Face. It stays on
localhost unless `CONSOLE_HOST` says otherwise; it has no login, so only open it to a
network you trust. Double-clicking `golem` on a desktop starts the service if needed and opens the
console in the browser; typed in a terminal, a bare `golem` opens the full-screen screen
instead, and on a machine with no screen at all it prints the usage.

**The engine belongs to `golem service`.** `up`, `down`, `restart` and `status` are executed
by that process and the commands are clients of it, so a killed run cannot leave a model
resident and the engine goes when the service does. `golem up` starts a service when there is
none; `golem service` runs one in the foreground (which is what a container's PID 1 needs, and
what `docker stop` stops cleanly); `golem service stop` ends the service and the engine.

### Updating

Golem offers a new release of itself in the console (a banner over the tabs), and in a
terminal before `up` and `tui`, at most once a day; `golem update` does it on demand
(`--check` only looks, `--yes` skips the question). It downloads this platform's asset,
verifies it against the release's `.sha256`, and replaces its own binary — close the app
window or run `golem service stop` and start Golem again to run the new one.
`UPDATE_CHECK = "off"` in `golem.toml` stops the automatic checks. A checkout never updates
this way: `git pull` and `./scripts/cargo.sh build`.

### The same stack in a container

`Dockerfile` builds one image per GPU vendor — `docker build --build-arg GPU_BACKEND=cuda -t
golem:cuda .` — with the pinned engine baked in by `golem install` and `golem service --up`
as PID 1. `compose.yml` wires the GPU through (`--profile cuda` or `--profile rocm`) and
mounts `data/models`, `bench/`, `logs/` and `data/tuned`; the image is the engine's home.
Docker is packaging here, not a mode: the same binary, the same configuration, the same
commands.

### `golem` commands

| Command | Does |
|---|---|
| `setup` | Linux: machine report, build deps, directory scaffold, and on a desktop the app's libraries and applications-menu entry; Windows: the app's Start-menu and desktop shortcuts |
| `install` | put the pinned engine build on this machine (~600 MB, no compiler needed) |
| `pull [preset \| name \| owner/repo[:quant]]` | fetch a preset's files, a library model, or any GGUF repo on Hugging Face (picks the quant, writes the preset); with no argument the default model's files (`up` fetches anything missing itself) |
| `up` / `down` / `restart` | start / stop / cycle the engine, through the service that owns it |
| `status` | health of inference, and whether a service owns it |
| `service [--up\|stop]` | run the engine owner, and the console it hosts, in the foreground (a container's PID 1), or stop it |
| `logs` | follow the inference log |
| `bench [--runs N] [--prefill TOKENS]` | speed: decode best/median + MTP acceptance, prefill on a ~30K prompt |
| `eval [--long TOKENS]` | quality gate: arithmetic, tool calls, a picture when the model has vision, throughput, needle + decode at depth |
| `report [--no-open] [--md]` | build `bench/report.html` (self-contained) from the recorded runs; `--md` writes `bench/RESULTS.md` for GitHub instead |
| `app` | the Golem window and tray, showing the console (a build with `--features desktop`; the release for Windows and macOS has it, and `golem-linux-x86_64-desktop` on Linux, which needs WebKitGTK). On Windows `golemw.exe` beside `golem.exe` starts it without a console window - point shortcuts at it |
| `tui` | the full-screen screen in the terminal (what a bare `golem` opens there): the actions, the model list (Tab; Enter makes the default or downloads, `t` tunes, `x` deletes the file, Del removes the preset), `p` pull, `e` engine, `w` console |
| `tune [preset] [--adopt] [--dry-run]` / `tune --auto` | the fastest gated setup for this model (or the best installed model) on this machine |
| `claude [args…]` | `exec claude` with `ANTHROPIC_*` already set |
| `env` | print those exports |

---

## Connecting clients

### Claude Code (direct)

`llama-server` exposes the **Anthropic Messages API natively** (`POST /v1/messages`), so Claude
Code talks to it without any translation layer, through golem's port:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from golem.toml>"
export ANTHROPIC_MODEL="ornith-1.5-35b"             # = MODEL_ALIAS
export ANTHROPIC_SMALL_FAST_MODEL="ornith-1.5-35b"
claude
```

On the Golem machine itself: `golem claude` does all of this. See
[docs/claude-code.md](docs/claude-code.md).

### Anything OpenAI (OpenCode, Aider, scripts)

Base URL `http://<golem-host>:8000/v1`, model `ornith-1.5-35b` (or `ornith-1.5-9b`),
Bearer = `LOCAL_AI_API_KEY`.
OpenCode and Aider have much smaller per-turn overheads than Claude Code (~6-10K tokens vs
15-20K) — on a constrained context window that meaningfully increases the usable working
context. See [docs/models.md](docs/models.md).

### Web search

Every chat request through golem's port gets two tools the model may call,
`golem_web_search` and `golem_web_fetch`, and Claude Code's own **WebSearch** works
unchanged: golem answers it. A search goes to DuckDuckGo from your machine (no key, no
account) and comes back to the model as a short numbered list; a fetch comes back as the
page's main text in Markdown, never HTML, and only public pages are fetched - never this
machine or your network. Each one shows in the answer as `🔎 Golem web search: ...` or
`🌐 Golem web fetch: ...`, and is a line in `logs/web.log`. `WEB_SEARCH = "off"` in
`golem.toml` turns it off. The details are in
[docs/architecture.md](docs/architecture.md#web-search).

---

## The models

The engine always runs as a **router**: one endpoint, every model in the preset file, each
loaded on its first request and swapped out when a client asks for another (one resident
at a time). The request's `model` field picks the [config/models.ini](config/models.ini)
section, which carries that model's own flags; `MODEL_ALIAS` in `golem.toml` is the one
clients get when they name none.

| `model` | Use | On an RTX 3060 12 GB + 32 GB RAM |
|---|---|---|
| `ornith-1.5-35b` | Coding (Claude Code, OpenCode) — the default | ~11.2 GB VRAM + ~14 GB RAM, 248K ctx, ~63 tok/s, images |
| `ornith-1.5-9b` | Everyday: summaries, shell one-liners, quick questions and code | ~11.2 GB VRAM, 262K ctx, ~54 tok/s, images |

To add one: `golem pull owner/repo[:quant]` writes its section, `golem tune <preset>
--adopt` fits it to the machine and gates it at 200K. `SLEEP_IDLE_SECONDS` unloads the
model after that long without requests, so the GPU and RAM are free between sessions; the
next request reloads it (~27 s for the coder). VRAM math, memory regimes and measured
benchmarks: [docs/models.md](docs/models.md).

---

## Repository layout

```
.
├── golem-rs/                 The Rust source of the binary (see its README)
├── Dockerfile                The container image: binary + pinned engine, service as PID 1
├── compose.yml               CUDA / ROCm profiles for that image
├── golem.toml.example        Configuration template; `golem.toml` is written from it
├── config/models.ini         Model presets for router mode (MODELS_PRESET)
├── docs/
│   ├── architecture.md       The binary, the service, the engine, the config, the container
│   ├── tuning.md             How `golem tune` searches, decides and gates
│   ├── roadmap.md            What is left, in order
│   ├── claude-code.md        Claude Code → Golem, in detail
│   ├── models.md             Model choice, memory regimes, benchmarks per setup
│   ├── engines-and-models.md What was measured, what was only found, other servers
│   ├── windows-native.md     Windows: the native engine, the VRAM cliff, LAN, auto-start
│   └── amd-rocm.md           AMD GPUs under ROCm
├── scripts/                  Building this binary: cargo.sh and the toolchain setup
├── data/models/              GGUF files (git-ignored)
├── data/cache/               Download / HF cache (git-ignored)
├── data/tuned/               Tuner output per machine (git-ignored)
└── logs/                     Run logs (git-ignored)
```

**One binary, no shell.** Every command — the measurement, the tuner, the console, the
screen, the engine and the service that owns it — is `golem-rs/`, which builds one
executable with no bash, no PowerShell and no WSL behind it. A release is that executable
plus the `golem.toml` it writes beside itself; a checkout builds it with
`./scripts/cargo.sh build` and gets the same thing at the repository root.

## Troubleshooting

| Symptom | Fix |
|---|---|
| The engine exits / OOM on load | `golem tune <preset>` fits the split and context to the card; by hand, lower the section's `ctx-size` or raise `n-cpu-moe` |
| MoE model slow or OOM under WSL2 | Give the WSL2 VM more RAM: `memory=24GB` in `%USERPROFILE%\.wslconfig` |
| Claude Code still hits api.anthropic.com | Export the four `ANTHROPIC_*` vars (`golem env`); use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` |
| Slow first answer | Prompt cache is cold; prefill runs ~800 tok/s on a 12 GB GPU (Claude Code's ~27K first prompt ≈ 35 s) and caches after |
| First answer after a pause takes ~30 s | `SLEEP_IDLE_SECONDS` unloaded the idle model; raise it or leave it empty to stay loaded |
| Router mode: `model not found` | The client's model name must match a section in `config/models.ini` (`golem env` shows `MODEL_ALIAS`) |
| Prefill suddenly 40% slower, VRAM ~full | Windows spills VRAM to shared system memory silently: raise `--n-cpu-moe` by 1-2 or close GPU-heavy apps (see [docs/windows-native.md](docs/windows-native.md)) |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
