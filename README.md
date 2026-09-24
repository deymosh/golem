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
| GPU | NVIDIA with 12 GB VRAM — dense models up to ~9B fit fully in VRAM; MoE models of the 30B class run with expert weights offloaded to system RAM |
| RAM | 32 GB recommended for MoE offload (16 GB is enough for dense models) |
| OS | Linux, or Windows — natively (Git Bash, no Docker) or via Docker Desktop/WSL2 |
| Docker | Docker Engine or Desktop + Compose v2 + NVIDIA GPU passthrough (docker mode only) |
| Disk | ~17 GB for the default model + ~4 GB image (less for smaller models) |

## Quickstart

```bash
cp .env.example .env && $EDITOR .env     # set keys and DEPLOY_MODE

./golem pull             # download the model into data/models/
./golem up               # start inference (mode from DEPLOY_MODE)
./golem status           # health check
./golem claude           # launch Claude Code pointed at the local model
./golem bench            # speed: decode + prefill tok/s
./golem eval --long 200000   # quality gate: tool calls + 200K needle
./golem down             # stop everything
```

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
| `install` | build `llama.cpp` with CUDA (native only) |
| `pull` | download `MODEL_FILE` from `HF_REPO` into `data/models/` |
| `up` / `down` / `restart` | start / stop / cycle the stack in the configured mode |
| `status` | health of inference |
| `logs` | follow the inference logs |
| `bench [--runs N] [--prefill TOKENS]` | speed: decode best/median + MTP acceptance, prefill on a ~30K prompt (curl + awk only) |
| `eval [--long TOKENS]` | quality gate: arithmetic, tool calls, throughput, needle + decode at depth (python3; via WSL on Windows) |
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

---

## Repository layout

```
.
├── golem                     Control script — the front door
├── compose.yml               inference
├── .env.example              Canonical config; copy to .env
├── docs/
│   ├── architecture.md       Request flow, ports, auth
│   ├── claude-code.md        Claude Code → Golem, in detail
│   ├── backends.md           Swapping llama.cpp for Ollama/vLLM/…
│   ├── models.md             Model choice, memory regimes, benchmarks per setup
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
| Prefill suddenly 40% slower, VRAM ~full | Windows spills VRAM to shared system memory silently: raise `--n-cpu-moe` by 1-2 or close GPU-heavy apps (see [docs/windows-native.md](docs/windows-native.md)) |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
