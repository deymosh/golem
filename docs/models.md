# Choosing and swapping the model

## Default model

**Qwen3.6-35B-A3B (UD-Q3_K_M)** — `unsloth/Qwen3.6-35B-A3B-GGUF`

Served at **262144 context**, `--cpu-moe` (all MoE expert weights in system RAM,
attention + KV on GPU), `--load-mode mlock`, PARALLEL=1, KV q8_0.

Why a MoE of this class is the sensible default on a 12 GB VRAM / 32 GB RAM box:

- 256K native context — Claude Code (assumes 200K) never overflows and auto-compacts.
- MoE 35B total / 3B active: quality of a 30B-class model at offloaded speeds.
- Valid OpenAI tool-calls through `--jinja` (verified: name, args, finish_reason).
- VRAM stays nearly flat as the context grows → headroom for compute buffers.

Measured numbers: [Benchmarks by setup](#benchmarks-by-setup) below.

## How the scripts pick a model

`scripts/download-model.sh` reads two variables from `.env`:

```bash
HF_REPO="unsloth/Qwen3.6-35B-A3B-GGUF"
MODEL_FILE="Qwen3.6-35B-A3B-UD-Q3_K_M.gguf"
```

The compose `command` serves `/models/$MODEL_FILE` and advertises it under `MODEL_ALIAS`.
Extra llama-server flags go in `EXTRA_ARGS` (`.env`) — `compose.yml` appends them
via the `sh -c` entrypoint (e.g. `--cpu-moe`, `--n-cpu-moe N`, `--mmproj ...`).

**Keep `MODEL_ALIAS` constant** (`local-model`) across swaps: clients keep working untouched.

## Swapping

1. Pick a GGUF on Hugging Face (`unsloth`, `bartowski`, `lmstudio-community` publish per-quant files).
2. Set `HF_REPO`, `MODEL_FILE` and `EXTRA_ARGS` in `.env` (MoE → `--cpu-moe`; dense → empty).
3. `./golem pull`
4. `./golem restart`

## Memory math, two regimes

**Dense models (all in VRAM)** — OS/display overhead (~1.5 GB on a desktop GPU) leaves
~10.5 GB usable on a 12 GB card. Weights + KV (q8, virtual growth) must fit; a 9B Q4 caps
near ~100K context.

**MoE models (the default setup)** — `--cpu-moe` keeps expert FFN weights in system RAM.
On Windows the WSL2 VM needs headroom for them: set `memory=24GB` in
`%USERPROFILE%\.wslconfig`. VRAM then holds only attention + dense parts + KV, so 25-35B
models fit and 256K context fits too. `--n-cpu-moe N` moves the last N layers' experts back
to GPU for speed when VRAM headroom allows. `--load-mode mlock` reads the whole model into
locked RAM at boot (best over bind mounts, notably Docker Desktop/WSL2 on Windows;
`--no-mmap` no longer exists — it is `--load-mode` now: auto | mmap | mlock | mmap+mlock).

---

# Benchmarks by setup

One section per machine. To add yours: configure the candidate in `.env`, then run
`./scripts/eval-model.sh` (and `./scripts/eval-model.sh --long 200000` for long-context
tests), and record decode/prefill/needle results in a new subsection.

## Setup A — 12 GB VRAM GPU + 32 GB RAM, Windows host, Docker (WSL2)

All candidates at ctx 262144. Measured 2026-09-19.

| Model | VRAM | decode empty / full | prefill | needle | notes |
|---|---|---|---|---|---|
| **Qwen3.6-35B-A3B UD-Q3_K_M** | 7.7 GB | 18 / 11.7 t/s @200K | 517 t/s | PASS 189K | **default**; best all-round |
| Gemma 4 26B-A4B UD-IQ4_NL | 8.9 GB | 14 / 9.8 t/s @200K | 508 t/s | PASS 189K | better quant (Q4); deleted, re-downloadable |
| Gemma 4 12B IQ4_NL (dense) | 11.5 GB ⚠ | 37 / 25 t/s @131K | 644 t/s | PASS 131K | fast/OpenCode tier; ctx ≤196K recommended (0.8 GB headroom at 262K) |

Prompt-cache reuse (Qwen3.6): 189K tokens replayed from cache in 0.4 s
(`cache_n=188961, prompt_n=1`) — Claude Code turn deltas are cheap.

Benchmarking gotchas: desktop load (Defender, browser) swings decode 8× — trust
best-of-3 in quiet; the first request after boot includes CUDA warmup (>120 s);
`/metrics` and `/props` need the API key; Qwen3.x and Gemma 4 are hybrid-thinking —
give generous `max_tokens` (reasoning eats the budget first) or send
`"chat_template_kwargs": {"enable_thinking": false}` (Qwen).

## Client pairing

- **Claude Code** (200K assumed): needs a server ctx ≥ 200K — the default Qwen3.6 config.
  `./golem env` prints the exports.
- **OpenCode** (context declared per model in `opencode.json` via
  `limit: { context, output }`, auto-compact honours it): pair it with the Gemma 12B
  profile (`MODEL_FILE="gemma-4-12b-it-IQ4_NL.gguf"`, `EXTRA_ARGS=""`,
  `CONTEXT_SIZE="196608"`), declaring `limit.context: 196608`.
