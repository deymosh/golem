# Golem

**A local AI construct for coding agents.**
One GPU, one llama.cpp server, zero tokens leaving your LAN. It speaks both the
**Anthropic Messages API** and the **OpenAI API**, so Claude Code — or any OpenAI client —
plugs straight in.

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
  │ llama-server :8000│
  │ local GGUF model  │
  └───────────────────┘
```

Default configuration: a **MoE model (35B total / 3B active)** served at **244K context** with
most expert weights offloaded to system RAM and MTP speculative decoding — quality of a
30B-class model on a 12 GB GPU at ~62 tok/s, with headroom for Claude Code's 200K window.
Dense models that fit entirely in VRAM are a one-line swap. See
[docs/models.md](docs/models.md) for measured numbers per setup.

---

## Requirements

| | |
|---|---|
| GPU | NVIDIA (CUDA) or AMD (ROCm) with 12 GB VRAM — detected automatically; dense models up to ~9B fit fully in VRAM; MoE models of the 30B class run with expert weights offloaded to system RAM |
| RAM | 32 GB recommended for MoE offload (16 GB is enough for dense models) |
| OS | Linux, or Windows natively (no Docker, no WSL, no Git Bash) |
| Docker | optional: Engine or Desktop + Compose v2 + GPU passthrough, for the container |
| Disk | ~17 GB for the default model (less for smaller models) |

## Quickstart

One binary. It creates `golem.toml` beside itself on first run, fetches the pinned engine
build and the model it needs, and starts serving.

```bash
golem up                     # install the engine if missing, fetch the model, start inference
golem status                 # health check
golem claude                 # launch Claude Code pointed at the local model
golem bench                  # speed: decode + prefill tok/s
golem eval --long 200000     # quality gate: tool calls + 200K needle
golem tune                   # auto-tune the offload split for this machine
golem web                    # console: results, models, start/stop/bench/eval/tune
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

**Console** (`golem web`) serves a local page on `127.0.0.1:8787` that stays up whether or
not an engine is running: live results from `bench/results.jsonl`, every preset with its
file and state, start/stop/restart/bench/eval/tune buttons, the running engine's log, and a
form to add a model from a Hugging Face repo. Nothing is exposed beyond localhost.

**The engine belongs to `golem service`.** `up`, `down`, `restart` and `status` are executed
by that process and the commands are clients of it, so a killed run cannot leave a model
resident and the engine goes when the service does. `golem up` starts a service when there is
none; `golem service` runs one in the foreground (which is what a container's PID 1 needs, and
what `docker stop` stops cleanly); `golem service stop` ends the service and the engine.

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
| `setup` | machine report, build deps, directory scaffold |
| `install` | put the pinned engine build on this machine (~600 MB, no compiler needed) |
| `pull` | download `MODEL_FILE` from `HF_REPO` into `data/models/` (`up` does this automatically for anything missing) |
| `up` / `down` / `restart` | start / stop / cycle the engine, through the service that owns it |
| `status` | health of inference, and whether a service owns it |
| `service [--up\|stop]` | run the engine owner in the foreground (a container's PID 1), or stop it |
| `logs` | follow the inference log |
| `bench [--runs N] [--prefill TOKENS]` | speed: decode best/median + MTP acceptance, prefill on a ~30K prompt |
| `eval [--long TOKENS]` | quality gate: arithmetic, tool calls, throughput, needle + decode at depth |
| `report [--no-open] [--md]` | build `bench/report.html` (self-contained) from the recorded runs; `--md` writes `bench/RESULTS.md` for GitHub instead |
| `web [--port N]` | console on `127.0.0.1:8787`: live results, models, controls, engine log |
| `tui` | the same console in the terminal |
| `tune [preset] [--adopt] [--dry-run]` / `tune --auto` | the fastest gated setup for this model (or the best installed model) on this machine |
| `claude [args…]` | `exec claude` with `ANTHROPIC_*` already set |
| `env` | print those exports |

---

## Connecting clients

### Claude Code (direct)

`llama-server` exposes the **Anthropic Messages API natively** (`POST /v1/messages`), so Claude
Code talks to it without any translation layer:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from golem.toml>"
export ANTHROPIC_MODEL="local-model"
export ANTHROPIC_SMALL_FAST_MODEL="local-model"
claude
```

On the Golem machine itself: `golem claude` does all of this. See
[docs/claude-code.md](docs/claude-code.md).

### Anything OpenAI (OpenCode, Aider, scripts)

Base URL `http://<golem-host>:8000/v1`, model `local-model`, Bearer = `LOCAL_AI_API_KEY`.
OpenCode and Aider have much smaller per-turn overheads than Claude Code (~6-10K tokens vs
15-20K) — on a constrained context window that meaningfully increases the usable working
context. See [docs/models.md](docs/models.md).

---

## The model

Any GGUF that [llama.cpp](https://github.com/ggml-org/llama.cpp) can serve works. The default
(`HF_REPO` / `MODEL_FILE` in `golem.toml`) is a MoE 35B/3B-active quant at 231K context with most
experts in RAM (`--n-cpu-moe`), 2048-token ubatches for fast prefill and MTP speculative
decoding (`SPEC_TYPE` / `DRAFT_FILE`); dense alternatives are listed in the same file and in
[docs/models.md](docs/models.md).

To swap: edit `HF_REPO` / `MODEL_FILE` in `golem.toml`, `golem pull`, `golem restart`, then
`golem eval --long 200000` before trusting it.
Keep `MODEL_ALIAS=local-model` so clients never change. VRAM math, memory regimes and measured
benchmarks per setup: [docs/models.md](docs/models.md).

### Several models, loaded on demand

Set `MODELS_PRESET = "config/models.ini"` (or the tuned `data/tuned/models.effective.ini`) in `golem.toml` and llama-server runs as a **router**: one endpoint,
several models, each loaded on its first request and swapped out when a client asks for another
(one resident at a time). The request's `model` field picks the
[config/models.ini](config/models.ini) section, which carries that model's own flags:

| `model` | Use | Footprint (RTX 3060 12 GB) |
|---|---|---|
| `ornith-1.5-35b` | Coding (Claude Code, OpenCode) — router default | ~11.3 GB VRAM + ~20 GB RAM, 244K ctx (tuned), ~62 tok/s |
| `qwen3.6-35b-a3b` | Coding — previous default, fallback | 11.8 GB VRAM + ~20 GB RAM, 231K ctx, ~60 tok/s |
| `qwen3.5-9b` | Everyday: summaries, curl/shell one-liners, quick questions, tool calls | ~6.6 GB VRAM, 64K ctx, ~49 tok/s, thinking off |

Set `MODEL_ALIAS` to the section clients should use by default. Add `SLEEP_IDLE_SECONDS` to
unload the model after that long without requests, so the GPU and RAM are free between
sessions; the next request reloads it (~27 s for the coder, a few seconds for the 9B once its
file is cached). Speed is unchanged
from single-model serving (measured: 62.6 tok/s decode, 840 tok/s prefill on the coder).

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
| `llama-server` exits / OOM | Lower `CONTEXT_SIZE` (KV ≈ 75 KB/token at q8_0) or use a smaller quant |
| MoE model slow or OOM under WSL2 | Give the WSL2 VM more RAM: `memory=24GB` in `%USERPROFILE%\.wslconfig` |
| Claude Code still hits api.anthropic.com | Export the four `ANTHROPIC_*` vars (`golem env`); use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` |
| Slow first answer | Prompt cache is cold; prefill runs ~800 tok/s on a 12 GB GPU (Claude Code's ~27K first prompt ≈ 35 s) and caches after |
| First answer after a pause takes ~30 s | `SLEEP_IDLE_SECONDS` unloaded the idle model; raise it or leave it empty to stay loaded |
| Router mode: `model not found` | The client's model name must match a section in `config/models.ini` (`golem env` shows `MODEL_ALIAS`) |
| Prefill suddenly 40% slower, VRAM ~full | Windows spills VRAM to shared system memory silently: raise `--n-cpu-moe` by 1-2 or close GPU-heavy apps (see [docs/windows-native.md](docs/windows-native.md)) |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
