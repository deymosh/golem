# Engines and models

What has been measured on the reference box (RTX 3060 12 GB, 32 GB RAM, i7-12700KF,
Windows native), what has only been found, and what that means. Vendor and guide numbers
are reasons to *measure*, never reasons to adopt: compare only inside a size class, only
same-session pairs, best-of-3 on a quiet desktop, and nothing becomes a default without
`golem eval --long 200000` PASS.

## Engines

### The filter that decides everything

Golem translates no API - its front only adds its own web tools - so the engine itself
must serve the **Anthropic `/v1/messages` API** for Claude Code to connect, plus OpenAI
for everything else. That one requirement
eliminates most of the field:

| Engine | `/v1/messages` | GGUF | Verdict |
|---|---|---|---|
| **llama.cpp** b11379 | yes | yes | **the default**: router mode, MTP, CPU expert offload |
| ik_llama.cpp main-b5483 | no | yes | **rejected, measured**: 16.7 t/s against 58.1 on the same model and flags; its adapter stays for paired measurements and the tuner never adopts it |
| Strata 0.2.6 | yes | its own packs | **passes the 200K gate, measured** with the Qwen3.8-Flash-Next Coder (below); not adopted: half llama.cpp's decode, and golem does not install or start it (v0.1.45 refused 200K; 0.2.7 not yet measured) |
| vLLM, SGLang, TensorRT-LLM, LMDeploy, TGI | no | partial | datacenter engines, Linux-first, no single-GPU expert offload |
| Ollama, LM Studio, TabbyAPI, llamafile | no | yes | wrappers or front-ends; OpenAI clients only |

Engine gate, 2026-10-19, quiet desktop, rows in `results/results.jsonl`:

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

**Strata 0.2.x** (0.2.0-beta 2026-10-19 to 0.2.6 2026-10-25) changes the numbers that
refused: its documentation now gives ~13 KB of KV per context token, KV streaming that
keeps part of it in RAM (~13.7 KB per token there) from 64K up, and a measured 262K prompt
on a 12 GB RTX 5070 with the Coder. It serves `/v1/messages`, installs per user with a
checksummed, signed release feed, and starts headless (`strata launch --model iq1_m
--family coder --context 200k --no-tui --yes`). The Coder pack is IQ1_M: 58 GB on disk
plus a 5.2 GB MTP draft layer, with a low-RAM mode for a 32 GB PC with a 12-16 GB card.
Golem still does not install or start it; it is measured as another server.

Measured 2026-10-25 on the reference box (Strata 0.2.6, engine 0.1.50, `--context 200k
--vision off`, golem's `eval --long 200000` against port 8080; the row is in
`results/results.jsonl`), beside llama.cpp b11379 with Ornith-1.5-35B the same afternoon:

| | Strata, Coder IQ1_M | llama.cpp, Ornith-1.5-35B |
|---|---|---|
| 200K needle | **PASS**, 829 t/s prefill | **PASS**, 667 t/s prefill |
| decode at depth | 30.7 t/s (MTP 79%) | 35.3 t/s |
| decode, short prompt (best of 3) | 34.9 t/s (MTP 63-68%) | ~64 t/s |
| tool call | PASS | PASS |

The structural refusal is gone: the Coder serves 200K on this card, prefills faster at
depth and clears every floor. What it costs is decode - about half llama.cpp's on short
turns - for a pruned 125B-class model at 1.6 bits whose quality is the vendor's claim (91%
of the full model's SWE-bench Verified), not a measurement here. Two further limits: the
haystack was sized by the characters-per-token estimate (the engine has no `/tokenize`),
so the needle was ~201.5K tokens; and it leaves ~90 GB on disk with its packed expert
file. The packs were removed after the run; the installed terminal and engine stay.

**Strata 0.2.7** (2026-10-27, engine 0.1.52) adds a routing prior for cards of 13 GiB or
less - its notes claim +15% decode on a story prompt and +37% on code for a 3060 - and a fixed API
port. Not measured here: it needs the Coder pack again (~64.5 GB), so it waits for the
disk and a free desktop, and is measured the same way as 0.2.6.

**ik_llama.cpp** loses on every axis here: its `--fit` fills the card to 12.0 GB (the
sysmem cliff), `-rtr` repacking crashes at start-up, `mlock` of its 17.7 GB buffer fails,
and it has no `/v1/messages`, no router and no idle unload.

### Builds

Builds are not interchangeable, so the pin is gated like a model. b11149's *Linux* build
answered the 200K needle wrong twice while every other test passed. **b11755** (measured
2026-10-20) passes the gate and prefills +31% (1143 vs 871 t/s at 30K, 787 vs 672 at
200K) but decodes slower at depth (31.0 vs 35.2 t/s, MTP acceptance 57% vs 72%) - and
decode at depth is what a long session pays on every token, so b11379 stays. A bump is `ENGINE_BUILD = "b<new>"`, `golem restart`, the
eval, and only then the adapter's `build`.

**b11879** (2026-10-25, the latest) is not adopted, on two counts measured the same night
against b11379 with Ornith-1.5-35B:

| | b11879 | b11379 |
|---|---|---|
| n-cpu-moe 35, ub2048 (mmap sweep point) | 56.6 t/s decode, 879-902 prefill | 63.4 t/s, 874 prefill |
| n-cpu-moe 35, ub1024 | 56.6-57.0 t/s | 64.1 t/s |
| largest context under the wall | 250880 | 248832 |
| gate load (`mlock`, the serving mode) | **CUDA out of memory** on every load | 200K **PASS**, 671 prefill, 35.9 at depth |

The decode gap is about 11% at every split both builds measured. The load failure is not
the card being full: the nospec point at 9057 MiB failed the same way, while every `mmap`
point loaded, so it is `mlock` on this build - the abort is in
`ggml_backend_cuda_buffer_set_tensor`. Each failed load reset the display driver
(nvlddmkm event 153) and blanked the screen for a moment, which is why the service now
stops the engine after the first failed load (see [architecture.md](architecture.md)) and
the tuner stops at a gate whose model does not load.

**b11936** (2026-10-27, measured 2026-10-28) fails the same way under `load-mode = mlock`
- the weights load in 25 s, then `ggml_backend_cuda_buffer_set_tensor` reports CUDA out of
memory - and **loads under `mmap+mlock`**, which maps the file and pins it, so the weights
stay in RAM exactly as `mlock` keeps them. What broke between b11755 and b11879 is `mlock`
without `mmap` (the weights read into allocated buffers), not the card. With `mmap+mlock`,
Ornith-1.5-35B on its b11379-tuned preset, quiet desktop, three alternating pairs the same
session:

| | decode median | decode best | prefill (30K) | MTP accept |
|---|---|---|---|---|
| b11379 | 59.5 / 61.4 / 61.0 | 62.3 / 62.4 / 61.3 | 869 / 856 / 857 | 67-68% |
| b11936 | 58.5 / 54.9 / 52.7 | 61.2 / 59.1 / 58.3 | 925 / 895 / 926 | 63% |

and its gate: 200K **PASS**, 707 t/s needle prefill (b11379: 671), 35.2 t/s at depth
(35.9). Prefill gains ~7%, decode loses 4-10% with a weaker MTP acceptance, so b11379 stays.
The next build to try is run under `mmap+mlock` (the `measure` skill's build trial); only
the pinned b11379 stays installed.

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

Measured here (numbers in [models.md](models.md); the tuned values are from the
2026-10-25 re-tune on b11379). Only the coder ships as a preset in `config/models.ini`;
`qwen3.6-35b-a3b` is a `golem pull` away through the library, and the rest were deleted
from disk once measured:

- **`ornith-1.5-35b`** - the default since 2026-10-19 (vendor SWE-V 79); tuned to
  n-cpu-moe 35 at 248832 tokens, 63.5 t/s decode; re-gated with its vision projector on
  2026-10-28, 200K needle at 666 t/s and 34.7 t/s at depth.
- `qwen3.6-35b-a3b` - the previous default; n-cpu-moe 37 at 253952, 60.8 t/s, 200K at
  36.8 at depth, but its prefill is about half Ornith's (493 against 846 on the tuner's 8K
  prompt).
- `flint` (Recursive, 39.8B-A2.5B MoE, UD-Q3_K_XL; vendor SWE-V 74.2) - tuned on b11936
  on 2026-10-28: 52.9 t/s decode, but its context stops at 88064 tokens under the wall on
  this card and both gate attempts failed the needle. Rejected.
- `ornith-1.5-35b-mtp` (APEX-MTP-Compact) - passes the gate, but its bundled head accepts
  32% and decode falls to 39 t/s. Rejected.
- `nemotron-3.5-lightning-30b` - no MTP; vendor SWE-V 52. Tuned to n-cpu-moe 41 at 344064
  tokens (its trained context is past 262144): 44.7 t/s decode, 930 prefill, and the
  fastest 200K needle measured here (1134 t/s), but 22.6 t/s at depth. The earlier 25.3
  t/s and 149 prefill came from the preset's hand-set `n-cpu-moe 30`, which spills: the
  tune measured n38 already over the wall. Not kept: the coder holds 200K faster at depth.

Not viable on 12 GB + 32 GB: GLM-5.2 (755 GB), Kimi K3 (1.56 TB), DeepSeek V4 Flash
(156 GB), Qwen3-Coder-Next 80B (IQ2 only), Kimi-Linear 48B (needs 64 GB), Muse Glimmer 30B
dense (too slow), Bonsai 2 27B ternary (10.2 GB at 64K, so 200K does not fit). Older
generation, skipped: Qwen3-Coder-30B-A3B, Devstral-Small-2-24B.

### The everyday slot (fits in VRAM, the desktop stays usable)

**`ornith-1.5-9b`** at Q4_K_M holds the slot since 2026-10-28. The candidates, tuned and
gated on b11379 on a quiet desktop, one at a time:

| Preset | File | Context | Decode | Prefill (8K) | Gate | At depth |
|---|---|---|---|---|---|---|
| **`ornith-1.5-9b`** (2026-10-28) | Q4_K_M, 5.6 GB, MTP off | 262144 | **54.3 t/s** | 1647 | 200K **PASS**, 937 t/s | 22.3 t/s |
| `qwen3.5-9b` (2026-10-25) | UD-Q4_K_XL, 5.6 GB | 258048 | 52.1 t/s | 1659 | 200K **PASS**, 941 t/s | 21.9 t/s |
| `ornith-1.5-9b` Q6_K (2026-10-25) | Q6_K, 7.0 GB | 156672 | 42.3 t/s | 1540 | 141K PASS, 1039 t/s | 23.8 t/s |
| `gemma-4-12b-it-qat-q4_0` (2026-10-25) | QAT Q4_0, 6.5 GB | 262144 | 40.0 t/s | 1244 | 200K **PASS**, 476 t/s | 19.5 t/s |

- **Ornith-1.5-9B** (dense, Qwen3.5-based; vendor SWE-V 70.6 against Qwen3.5-9B's 53.2)
  wins on speed and on the vendor scores once its quant leaves room for the context: the
  Q6_K stops at 156672 tokens, the Q4_K_M holds the trained 262144. Its MTP head decodes
  76-78 t/s, but the draft's own KV cache caps the context at 209920 tokens, short of the
  200K gate's 223232, so the tuner serves it with MTP off ([models.md](models.md)).
- **Gemma 4 12B QAT** holds its whole 256K context and passes 200K, but decodes 23% and
  prefills 25% slower than Qwen3.5-9B, and its needle prefill is half.

### Watch

- **Qwen3.8-Flash-Next pruned "Coder"** (125B → 256/512 experts, IQ1_M, 91.3% of the
  full model's SWE-V) - Strata-only, and it passes the 200K gate here (above) at ~35 t/s.
  Whether its quality earns half the decode speed is a judgement golem's gate cannot make;
  a side-by-side on real coding tasks is the measurement that would.

Embeddings and a reranker were tried and removed (the reranker misordered a mixed
Spanish/English corpus); image generation (sd-server + Z-Image-Turbo, 22 s per 1024²) was
measured but is not part of golem.
