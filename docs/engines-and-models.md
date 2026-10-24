# Engines and models

What has been measured on the reference box (RTX 3060 12 GB, 32 GB RAM, i7-12700KF,
Windows native), what has only been found, and what that means. Vendor and guide numbers
are reasons to *measure*, never reasons to adopt: compare only inside a size class, only
same-session pairs, best-of-3 on a quiet desktop, and nothing becomes a default without
`golem eval --long 200000` PASS.

## Engines

### The filter that decides everything

Golem has no gateway, so the engine itself must serve the **Anthropic `/v1/messages`
API** for Claude Code to connect, plus OpenAI for everything else. That one requirement
eliminates most of the field:

| Engine | `/v1/messages` | GGUF | Verdict |
|---|---|---|---|
| **llama.cpp** b11379 | yes | yes | **the default**: router mode, MTP, CPU expert offload |
| ik_llama.cpp main-b5483 | no | yes | **rejected, measured**: 16.7 t/s against 58.1 on the same model and flags; its adapter stays for paired measurements and the tuner never adopts it |
| Strata v0.1.45 | yes | its own | **rejected, measured, unsupported**: no 200K on 12 GB (below); it installs itself with its own setup, which golem does not run |
| vLLM, SGLang, TensorRT-LLM, LMDeploy, TGI | no | partial | datacenter engines, Linux-first, no single-GPU expert offload |
| Ollama, LM Studio, TabbyAPI, llamafile | no | yes | wrappers or front-ends; OpenAI clients only |

Engine gate, 2026-10-19, quiet desktop, rows in `bench/results.jsonl`:

| Engine | Model | ctx | decode best/median | prefill | VRAM |
|---|---|---|---|---|---|
| llama.cpp b11379 | Ornith-1.5-35B Q4_K_M | 236544 | 59.6 / 58.1 | 836 | 11.1 GB |
| llama.cpp b11379 | Ornith-1.5-35B Q4_K_M | 32768 | 59.4 / 58.5 | 823 | 6.6 GB |
| strata v0.1.45 | Qwen3.6-35B UD-Q4_K_XL | 32768 | 50.6 / 49.8 | 657 | 11.7 GB |
| strata v0.1.45 | Qwen3.6-35B UD-Q4_K_XL | 236544 | refused | – | – |
| ik_llama.cpp main-b5483 | Ornith-1.5-35B Q4_K_M | 236544 | 18.8 / 16.7 | 479 | 12.0 GB |

**Strata's refusal** is structural, not a placement to tune around: its KV costs ~57 KB per
context token against llama.cpp's ~12.7 KB at q8_0 (measured: 32K → 64K costs 1.80 GiB),
so 236544 needs ~13.5 GB before any weights, and the engine reports "0.00 GiB of VRAM free
… 1256 MiB needed". Its layout (experts in RAM, a lookup table on SSD) is still the only
one that runs a pruned 125B-class model on this card - the Qwen3.8-Flash-Next "Coder" build
below - which is the reason to re-check its 0.2.x engine line, and nothing else is.

**ik_llama.cpp** loses on every axis here: its `--fit` fills the card to 12.0 GB (the
sysmem cliff), `-rtr` repacking crashes at start-up, `mlock` of its 17.7 GB buffer fails,
and it has no `/v1/messages`, no router and no idle unload.

### Builds

Builds are not interchangeable, so the pin is gated like a model. b11149's *Linux* build
answered the 200K needle wrong twice while every other test passed. **b11755** (measured
2026-10-20) passes the gate and prefills +31% (1143 vs 871 t/s at 30K, 787 vs 672 at
200K) but decodes slower at depth (31.0 vs 35.2 t/s, MTP acceptance 57% vs 72%) - and
decode at depth is what a long session pays on every token, so b11379 stays. b11755 stays
installed for a paired retest. A bump is `ENGINE_BUILD = "b<new>"`, `golem restart`, the
eval, and only then the adapter's `build`.

### Using another server

Golem's measuring commands only need an OpenAI-compatible endpoint: point
`LOCAL_AI_BASE_URL` (and `MODEL_ALIAS`) at Ollama (`:11434/v1`), vLLM (`:8000/v1`) or LM
Studio (`:1234/v1`) and `golem status`, `bench` and `eval` measure it (tok/s comes from
llama.cpp's `timings`, so other servers report empty speeds). Golem does not start or
install them, and Claude Code cannot use a server without `/v1/messages`. An `https://`
base URL is refused: the client has no TLS.

### Found, not ready

- **Ferrox** (from-scratch GGUF engine) and **Frink** (pure-Rust GGUF engine, MoE, CUDA,
  OpenAI server) - no Anthropic API; watch.
- **Prism** - a llama.cpp fork for ternary PTQ1_0 models (Bonsai 2); interesting for the
  format, not the server.
- llama.cpp's own **MoE expert cache RFC** (~60% reported) - re-bench when merged.

## Models

### The coder slot (≈30-35B total, ≈3B active, MoE, 200K context)

Measured and in `config/models.ini` (numbers in [models.md](models.md)):

- **`ornith-1.5-35b`** - the default since 2026-10-19 (vendor SWE-V 79); tuned to
  n-cpu-moe 35 at 249344, 62.8 t/s decode, 200K needle at 35.8 t/s.
- `qwen3.6-35b-a3b` - the previous default and the fallback; same speed, 0.8 GB more VRAM.
- `ornith-1.5-35b-mtp` (APEX-MTP-Compact) - passes the gate, but its bundled head accepts
  32% and decode falls to 39 t/s. Rejected.
- `nemotron-3.5-lightning-30b` - 25.3 t/s decode and 149 t/s prefill at the VRAM edge, no
  MTP; vendor SWE-V 52. A long-context fallback at most.

Not viable on 12 GB + 32 GB: GLM-5.2 (755 GB), Kimi K3 (1.56 TB), DeepSeek V4 Flash
(156 GB), Qwen3-Coder-Next 80B (IQ2 only), Kimi-Linear 48B (needs 64 GB), Muse Glimmer 30B
dense (too slow), Bonsai 2 27B ternary (10.2 GB at 64K, so 200K does not fit). Older
generation, skipped: Qwen3-Coder-30B-A3B, Devstral-Small-2-24B.

### The everyday slot (fits in VRAM, the desktop stays usable)

`qwen3.5-9b` with its vision mmproj is the current assistant preset. Two candidates to
gate when the GPU is free, each replacing it rather than the coder:

- **Ornith-1.5-9B** (dense, Qwen3.5-based) - vendor SWE-V 70.6, GPQA 86.4; Q6_K ~7 GB.
- **Gemma 4 12B QAT Q4_0** - 256K native context, 6.5 GB, solid agentic scores.

### Watch

- **Qwen3.8-Flash-Next pruned "Coder"** (125B → 256/512 experts, ~1.89 bpw, 91.3% of the
  full model's SWE-V, ~50 t/s claimed on 12 GB + 32 GB) - Strata-only. If a Strata engine
  ever serves 200K on 12 GB, this is the model to gate it with, at 2-bit quality that has
  to be measured rather than assumed.

Embeddings and a reranker were tried and removed (the reranker misordered a mixed
Spanish/English corpus); image generation (sd-server + Z-Image-Turbo, 22 s per 1024²) was
measured but is not part of golem.
