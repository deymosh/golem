# Choosing and swapping the model

## Default model

**Qwen3.6-35B-A3B (UD-Q3_K_M)** — `unsloth/Qwen3.6-35B-A3B-GGUF`
**+ MTP draft head** — `bartowski/Qwen_Qwen3.6-35B-A3B-GGUF` / `mtp-Qwen_Qwen3.6-35B-A3B-Q4_0.gguf`

Served at **262144 context**, `--n-cpu-moe 32` (experts of the first 32 of 40 layers in
system RAM, the rest + attention + KV on GPU), `--spec-type draft-mtp` with the standalone
MTP head, `--load-mode mlock`, PARALLEL=1, KV q8_0. The unsloth GGUF has no MTP layers;
bartowski's separate 1.2 GB head loads next to it via `DRAFT_FILE`.

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
   For MTP: `SPEC_TYPE="draft-mtp"`, plus `DRAFT_HF_REPO` / `DRAFT_FILE` if the MTP head is a
   separate file (empty `DRAFT_FILE` when the model GGUF embeds it); `SPEC_TYPE=""` disables.
3. `./golem pull`
4. `./golem restart`

## Memory math, two regimes

**Dense models (all in VRAM)** — OS/display overhead (~1.5 GB on a desktop GPU) leaves
~10.5 GB usable on a 12 GB card. Weights + KV (q8, virtual growth) must fit; a 9B Q4 caps
near ~100K context.

**MoE models (the default setup)** — `--cpu-moe` keeps expert FFN weights in system RAM.
On Windows the WSL2 VM needs headroom for them: set `memory=24GB` in
`%USERPROFILE%\.wslconfig`. VRAM then holds only attention + dense parts + KV, so 25-35B
models fit and 256K context fits too. `--n-cpu-moe N` keeps only the first N layers' experts
in RAM and puts the remaining layers' experts on the GPU — lower N = faster, more VRAM. `--load-mode mlock` reads the whole model into
locked RAM at boot (best over bind mounts, notably Docker Desktop/WSL2 on Windows;
`--no-mmap` no longer exists — it is `--load-mode` now: auto | mmap | mlock | mmap+mlock).

---

# Benchmarks by setup

One section per machine. To add yours: configure the candidate in `.env`, then run
`./scripts/eval-model.sh` (and `./scripts/eval-model.sh --long 200000` for long-context
tests), and record decode/prefill/needle results in a new subsection.

## Setup A — 12 GB VRAM GPU + 32 GB RAM, Windows host, Docker (WSL2)

### Speculative decoding + expert placement (measured 2026-09-23)

Qwen3.6-35B-A3B UD-Q3_K_M, ctx 262144, 600-token code generations, best-of-3.
VRAM includes ~2 GB taken by the Windows desktop.

| Config | VRAM total | decode | prefill |
|---|---|---|---|
| `--cpu-moe`, no MTP (previous default) | 6.5 GB | 23 t/s | — |
| `--cpu-moe` + MTP | 8.3 GB | 35 t/s | — |
| `--n-cpu-moe 34` + MTP | 10.5 GB | 35-39 t/s | — |
| **`--n-cpu-moe 32` + MTP (default)** | 11.2 GB | **37-42 t/s** | ~530 t/s @200K |
| `--n-cpu-moe 31` + MTP | 11.5 GB | 38-50 t/s | 660-720 t/s @31K |

MTP draft acceptance ~70-80% (mean 3.1-3.4 tokens per step). Tool calls (OpenAI and
Anthropic endpoints) and the 200K needle pass with MTP on. N=31 is the fastest measured
but leaves ~0.8 GB VRAM; 32 keeps ~1 GB for desktop apps.

### Qwen3.8-27B (dense, 2026-08) — does not fit 262K on 12 GB

Qwen3.8 ships as 27B dense, 125B-A6B "Flash-Next" (72+ GB at 1-bit) and 2.4T-A95B — only
the 27B is runnable here. Its GGUFs embed MTP layers (`SPEC_TYPE=draft-mtp`, no
`DRAFT_FILE`). UD-IQ3_XXS (10.9 GB) is already ≥ free VRAM, so FFN weights spill to RAM
via `--n-cpu-ffn N`, and every dense layer on the CPU is paid on every token:

| Config (UD-IQ3_XXS) | decode |
|---|---|
| ctx 65536, KV q8_0, `--n-cpu-ffn 6`, no MTP | 10 t/s |
| ctx 32768, KV q8_0, `--n-cpu-ffn 8`, MTP | 22 t/s |
| ctx 262144, KV q4_0, `--fit`, MTP | 4.7 t/s |

KV is ~35 KB/token at q8_0 (16 full-attention layers of 64), so 262K alone needs ~9 GB.
Usable only as a short-context profile (see `.env.example`); on a 24 GB card it would
be the stronger choice.

### Model comparison

All candidates at ctx 262144, `--cpu-moe`, no MTP. Measured 2026-09-19.

| Model | VRAM | decode empty / full | prefill | needle | notes |
|---|---|---|---|---|---|
| **Qwen3.6-35B-A3B UD-Q3_K_M** | 7.7 GB | 18 / 11.7 t/s @200K | 517 t/s | PASS 189K | **default** (now + MTP, see above); best all-round |
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
