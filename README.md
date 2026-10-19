# Golem

**A local AI construct for coding agents.**
One GPU, one llama.cpp container, zero tokens leaving your LAN. It speaks both the
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

Default configuration: a **MoE model (35B total / 3B active)** served at **231K context** with
most expert weights offloaded to system RAM and MTP speculative decoding — quality of a
30B-class model on a 12 GB GPU at ~63 tok/s, with headroom for Claude Code's 200K window.
Dense models that fit entirely in VRAM are a one-line swap. See
[docs/models.md](docs/models.md) for measured numbers per setup.

---

## Requirements

| | |
|---|---|
| GPU | NVIDIA (CUDA) or AMD (ROCm) with 12 GB VRAM — detected automatically; dense models up to ~9B fit fully in VRAM; MoE models of the 30B class run with expert weights offloaded to system RAM |
| RAM | 32 GB recommended for MoE offload (16 GB is enough for dense models) |
| OS | Linux, or Windows — natively (Git Bash, no Docker) or via Docker Desktop/WSL2 |
| Docker | Docker Engine or Desktop + Compose v2 + NVIDIA GPU passthrough (docker mode only) |
| Disk | ~17 GB for the default model + ~4 GB image (less for smaller models) |

## Quickstart

```bash
./golem up               # bootstrap: creates .env from .env.example if absent,
                         # downloads any missing model files, starts inference
./golem status           # health check
./golem claude           # launch Claude Code pointed at the local model
./golem bench            # speed: decode + prefill tok/s
./golem eval --long 200000   # quality gate: tool calls + 200K needle
./golem tune             # auto-tune the offload split for this machine
./golem web              # console: results, models, start/stop/bench/eval/tune
./golem down             # stop everything
```

`cp .env.example .env && $EDITOR .env` before the first `up` if you want to tune anything
(model, keys, `DEPLOY_MODE`) — the defaults work as-is. `./golem pull` re-downloads /
pre-fetches `MODEL_FILE` explicitly; `up` does it automatically when a file is missing.

**Auto-tune** (`./golem tune [preset]`) answers "what is the best way to run this model on
this machine?" with no AI and no hand-editing. It tries every installed engine that can serve
the preset — sweeping the expert offload for each, seeding from llama.cpp's own placement tool
— keeps the fastest engine + split that is not spilling VRAM into system memory, gates the
winner with the 200K eval, and writes the verdict (engine, flags, every measured point) to
`data/tuned/`. Re-run with `--adopt` to put the winner into `.env`; `--engine NAME` tunes one
engine only, `--dry-run` prints the plan. It refuses to measure while a download, build or game
is loading the machine, because the numbers would be meaningless.

**Console** (`./golem web`, or `golem.cmd web`) serves a local page on `127.0.0.1:8787`
that stays up whether or not an engine is running: live results from `bench/results.jsonl`,
every preset with its file and state, start/stop/restart/bench/eval/tune buttons, the
running engine's log, and a form to add a model from a Hugging Face repo. Nothing is
exposed beyond localhost.

On Windows (`windows` mode) `golem.cmd` does the same without Git Bash: double-click it (or a
desktop shortcut to it) for a start / stop / restart / status / logs / report / web / tune
menu, or run `golem.cmd start|stop|restart|status|logs|report|web|tune`. A fresh clone
bootstraps itself from the menu's Start: `.env` is created, the pinned llama.cpp build and
every missing model download before the server starts — no Git Bash, no Docker, no manual
steps.

Three deploy modes (`DEPLOY_MODE` in `.env`, or `--docker` / `--native` / `--windows`):

| Mode | Runs | Where |
|---|---|---|
| `windows` | official `llama-server.exe`, no Docker/WSL — **~+50% decode vs docker, recommended on Windows** | Git Bash on Windows ([docs/windows-native.md](docs/windows-native.md)) |
| `docker` | llama.cpp container pinned by digest | Linux, or Windows via Docker Desktop (run `./golem` inside WSL2) |
| `native` | llama.cpp built on the host | Linux / WSL2 |

### `./golem` commands

| Command | Does |
|---|---|
| `setup` | machine report, build deps, directory scaffold (native mode) |
| `install` | build `llama.cpp` with CUDA/ROCm (native only) |
| `pull` | download `MODEL_FILE` from `HF_REPO` into `data/models/` (`up` does this automatically for anything missing) |
| `up` / `down` / `restart` | start / stop / cycle the stack in the configured mode |
| `status` | health of inference |
| `logs` | follow the inference logs |
| `bench [--runs N] [--prefill TOKENS]` | speed: decode best/median + MTP acceptance, prefill on a ~30K prompt (curl + awk only) |
| `eval [--long TOKENS]` | quality gate: arithmetic, tool calls, throughput, needle + decode at depth (python3; via WSL on Windows) |
| `report [--no-open]` | build `bench/report.html` (self-contained) from the recorded runs |
| `web [--port N]` | console on `127.0.0.1:8787`: live results, models, controls, engine log |
| `tune [preset] [--adopt] [--engine NAME] [--dry-run]` | best engine + flags for this model on this machine |
| `claude [args…]` | `exec claude` with `ANTHROPIC_*` already set |
| `env` | print those exports |

---

## Connecting clients

### Claude Code (direct)

`llama-server` exposes the **Anthropic Messages API natively** (`POST /v1/messages`), so Claude
Code talks to it without any translation layer:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from .env>"
export ANTHROPIC_MODEL="local-model"
export ANTHROPIC_SMALL_FAST_MODEL="local-model"
claude
```

On the Golem machine itself: `./golem claude` does all of this. See
[docs/claude-code.md](docs/claude-code.md).

### Anything OpenAI (OpenCode, Aider, scripts)

Base URL `http://<golem-host>:8000/v1`, model `local-model`, Bearer = `LOCAL_AI_API_KEY`.
OpenCode and Aider have much smaller per-turn overheads than Claude Code (~6-10K tokens vs
15-20K) — on a constrained context window that meaningfully increases the usable working
context. See [docs/models.md](docs/models.md).

---

## The model

Any GGUF that [llama.cpp](https://github.com/ggml-org/llama.cpp) can serve works. The default
(`HF_REPO` / `MODEL_FILE` in `.env`) is a MoE 35B/3B-active quant at 231K context with most
experts in RAM (`--n-cpu-moe`), 2048-token ubatches for fast prefill and MTP speculative
decoding (`SPEC_TYPE` / `DRAFT_FILE`); dense alternatives are listed in the same file and in
[docs/models.md](docs/models.md).

To swap: edit `HF_REPO` / `MODEL_FILE` in `.env`, `./golem pull`, `./golem restart`, then
`./golem eval --long 200000` before trusting it.
Keep `MODEL_ALIAS=local-model` so clients never change. VRAM math, memory regimes and measured
benchmarks per setup: [docs/models.md](docs/models.md).

### Several models, loaded on demand

Set `MODELS_PRESET="config/models.ini"` and llama-server runs as a **router**: one endpoint,
several models, each loaded on its first request and swapped out when a client asks for another
(one resident at a time). The request's `model` field picks the
[config/models.ini](config/models.ini) section, which carries that model's own flags:

| `model` | Use | Footprint (RTX 3060 12 GB) |
|---|---|---|
| `ornith-1.5-35b` | Coding (Claude Code, OpenCode) — router default | 11.0 GB VRAM + ~20 GB RAM, 231K ctx, ~60 tok/s |
| `qwen3.6-35b-a3b` | Coding — previous default, fallback | 11.8 GB VRAM + ~20 GB RAM, 231K ctx, ~60 tok/s |
| `qwen3.5-9b` | Everyday: summaries, curl/shell one-liners, quick questions, tool calls | ~6.6 GB VRAM, 64K ctx, ~49 tok/s, thinking off |

Set `MODEL_ALIAS` to the section clients should use by default. Add `SLEEP_IDLE_SECONDS` to
unload the model after that long without requests, so the GPU and RAM are free between
sessions; the next request reloads it (~27 s for the coder, a few seconds for the 9B once its
file is cached). Router and sleep both work in all three deploy modes. Speed is unchanged
from single-model serving (measured: 62.6 tok/s decode, 840 tok/s prefill on the coder).

---

## Repository layout

```
.
├── golem                     Control script — the front door
├── compose.yml               inference
├── .env.example              Canonical config; copy to .env
├── config/models.ini         Model presets for router mode (MODELS_PRESET)
├── docs/
│   ├── architecture.md       Request flow, ports, auth
│   ├── claude-code.md        Claude Code → Golem, in detail
│   ├── backends.md           Swapping llama.cpp for Ollama/vLLM/…
│   ├── models.md             Model choice, memory regimes, benchmarks per setup
│   ├── amd-rocm.md           AMD GPUs: ROCm in all three deploy modes
│   └── windows-native.md     Windows mode: llama-server.exe without Docker
├── scripts/                  Building blocks the control script calls
├── data/models/              GGUF files (git-ignored)
├── data/cache/               Download / HF cache (git-ignored)
└── logs/                     Run logs (git-ignored)
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `llama-server` exits / OOM | Lower `CONTEXT_SIZE` (KV ≈ 75 KB/token at q8_0) or use a smaller quant |
| MoE model slow or OOM under WSL2 | Give the WSL2 VM more RAM: `memory=24GB` in `%USERPROFILE%\.wslconfig` |
| Claude Code still hits api.anthropic.com | Export the four `ANTHROPIC_*` vars (`./golem env`); use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` |
| Slow first answer | Prompt cache is cold; prefill runs ~800 tok/s on a 12 GB GPU (Claude Code's ~27K first prompt ≈ 35 s) and caches after |
| First answer after a pause takes ~30 s | `SLEEP_IDLE_SECONDS` unloaded the idle model; raise it or leave it empty to stay loaded |
| Router mode: `model not found` | The client's model name must match a section in `config/models.ini` (`./golem env` shows `MODEL_ALIAS`) |
| Prefill suddenly 40% slower, VRAM ~full | Windows spills VRAM to shared system memory silently: raise `--n-cpu-moe` by 1-2 or close GPU-heavy apps (see [docs/windows-native.md](docs/windows-native.md)) |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
