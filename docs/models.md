# Choosing and swapping the model

## Current model

**Qwen 3.5 9B Q4_K_M** — `lmstudio-community/Qwen3.5-9B-GGUF`

Why this one on a 12 GB RTX 3060:

- Top of willitrunai.com's RTX 3060 12GB tier list for coding (score 92), 40 tok/s measured there,
  **53 tok/s measured here**.
- Native tool calling in llama.cpp (`--jinja`) — the hard requirement for agent loops.
- Hybrid reasoning (`reasoning_content` / thinking blocks).
- Trained at 262K context; 32K served here because VRAM, not the model, is the limit.

## How the scripts pick a model

`scripts/download-model.sh` reads two variables from `.env`:

```bash
HF_REPO="lmstudio-community/Qwen3.5-9B-GGUF"
MODEL_FILE="Qwen3.5-9B-Q4_K_M.gguf"
```

It downloads `MODEL_FILE` from `HF_REPO` into `data/models/`. The compose `command` serves
`/models/$MODEL_FILE` and advertises it under `MODEL_ALIAS`.

**Keep `MODEL_ALIAS` constant** (`local-model`) across swaps: clients keep working untouched.

## Swapping

1. Pick a GGUF on Hugging Face (`bartowski`, `unsloth`, `lmstudio-community` publish per-quant files).
2. Set `HF_REPO` and `MODEL_FILE` in `.env` (add `HF_TOKEN` if gated).
3. `./golem pull`
4. `./golem restart --docker`

If the model is not multimodal-aware, ignore the mmproj file; if you *want* vision on a model
that has it, add `--mmproj /models/<mmproj>.gguf` in `compose.yml` and budget ~1 GB extra VRAM.

## VRAM math (the 12 GB budget)

Windows/display takes ~1.5 GB → **~10.5 GB usable**.

| Component | Size | Notes |
|---|---|---|
| Weights (Q4_K_M, 9B) | ~5.2 GiB | resident from boot |
| Compute buffers + CUDA | ~0.6 GiB | |
| KV cache @ q8_0 | **~75 KB/token** | virtual: grows as the slot fills |

| `CONTEXT_SIZE` | KV at full slot | Worst-case total | Verdict on 12 GB |
|---|---|---|---|
| 16384 | ~1.2 GiB | ~7.3 GiB | very safe |
| **32768** | ~2.3 GiB | ~9.2 GiB | **current setting — safe, use this** |
| 49152 | ~3.5 GiB | ~10.4 GiB | risky with display running |
| 65536+ | ~4.6 GiB+ | 11.5 GiB+ | will OOM mid-session |

Quant guide (9B): Q4_K_M 5.6 GB · Q6_K 7.4 GB · Q8_0 9.5 GB. Above Q4_K_M the KV budget
disappears — not worth it on 12 GB.

## Candidates that fit (S/A tier on willitrunai, 3060 12GB)

| Model | VRAM | tok/s | Ctx | Notes |
|---|---|---|---|---|
| **Qwen 3.5 9B** | 9.8 GB | 40 (53 here) | 32K | current; best tool-calling support |
| Ornith 1.0 9B | 8.3 GB | 34 | 137K | agentic specialist (SWE-bench 69.4); tool-template support in llama.cpp unverified |
| Ling 3.0 Tiny (MoE) | 9.8 GB | 103 | 28K | fastest; generalist |
| LFM2.5 8B A1B (MoE) | 7.5 GB | 101 | 128K | fast + long ctx; generalist |

**Do not** try ≥20B models (GPT-OSS 20B, Qwen3-Coder 30B, Devstral Small 24B…): they do not fit
and fall off a CPU-offload cliff.
