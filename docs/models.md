# Choosing and swapping the model

## The two presets

Golem serves two models from one router ([config/models.ini](../config/models.ini)), and
both hold Claude Code's 200K window on a 12 GB card with 32 GB of RAM:

| Preset | Role | File | Served | Decode | Prefill (8K) | 200K gate | At depth |
|---|---|---|---|---|---|---|---|
| **`ornith-1.5-35b`** | coder, the default | Q4_K_M, 21.7 GB, MTP head inside | n-cpu-moe 35, ctx 248832, MTP | 63.5 t/s | 846 | **PASS**, 666 t/s | 34.7 t/s |
| **`ornith-1.5-9b`** | everyday | Q4_K_M, 5.6 GB, all on the card | ctx 262144, MTP off | 54.3 t/s | 1647 | **PASS**, 937 t/s | 22.3 t/s |

Both read images: the vision projector runs on the CPU (`no-mmproj-offload`), so it costs
RAM rather than the VRAM the context is sized from, and the eval's picture step passes on
both ("what colour is the circle" - "Red"). Measured 2026-10-28 on llama.cpp b11379,
quiet desktop; the coder's split and context are its 2026-10-25 tune, re-gated with the
projector.

**The coder** is Ornith-1.5-35B-A3B (MIT): a Qwen3.5-MoE skeleton (256 experts, 8 per
token, 40 layers, 30 linear-attention + 10 full), continued pretraining and
self-improvement RL. Vendor scores: SWE-bench Verified 79, SWE-bench Pro 59.6,
Terminal-Bench 2.1 67.8. Most experts stay in system RAM and the card holds attention, the
remaining experts and the KV cache, which is what leaves room for 248832 tokens.

**The everyday model** is Ornith-1.5-9B (MIT, dense, Qwen3.5-based), vendor SWE-bench
Verified 70.6 against Qwen3.5-9B's 53.2 - the model that held this slot until 2026-10-28,
and which it also beats on speed (54.3 against 52.1 t/s) at the same 262144 context. Its
GGUF carries an MTP head, and MTP is faster still, but on this card it cannot hold the
200K gate, so the tuner passes it over ([tuning.md](tuning.md)):

| Ornith-1.5-9B Q4_K_M | Context | Decode | VRAM | |
|---|---|---|---|---|
| MTP on | 209920 (largest under the wall) | 76.4 t/s | 11263 MiB | 189K gate only |
| MTP on, q4_0 draft cache | 223232 | 75.4 t/s | 11916 MiB | over the 11264 MiB wall |
| **MTP off** | **262144** | **54.3 t/s** | 11217 MiB | **200K PASS** |

MTP's draft keeps a KV cache of its own, about 5 KB per token beside the model's ~21.6,
plus ~0.4 GB of buffers; a q4_0 draft cache halves the per-token part and still does not
fit.

### How the coder was chosen

Adoption gate, 2026-10-19, b11379 Windows native, quiet desktop, paired same session:

| Preset | decode best / median | MTP accept | prefill 28.6K | VRAM | eval (tools + 211.8K needle) | decode @ 211.8K |
|---|---|---|---|---|---|---|
| `qwen3.6-35b-a3b` (baseline) | 59.8 / 59.1 t/s | 73% | 785 t/s | 11.8 GB | PASS (2026-10-03) | 30.4 t/s |
| **`ornith-1.5-35b`** (official Q4_K_M, 21.7 GB) | **59.5 / 59.3 t/s** | 66% | **832 t/s** | **11.0 GB** | **PASS**, 626 t/s prefill | 30.6 t/s |
| `ornith-1.5-35b-mtp` (mudler APEX-MTP-Compact, 17.4 GB) | 39.4 / 39.2 t/s | 32% | 1135 t/s | 11.0 GB | PASS, 735 t/s prefill | 21.3 t/s |

Ornith matches Qwen3.6 on speed with 0.8 GB more VRAM headroom and better vendor coding
scores. The APEX quant passes the gate but its bundled MTP head is weak (32% acceptance)
and costs a third of decode. The tuner then moved the split from the hand-set
`n-cpu-moe 36` to 35 and the context to 248832.

### Every preset, re-tuned 2026-10-25

`golem tune <preset>` for each, one at a time, on llama.cpp b11379, quiet desktop, wall
11264 MiB. Decode and prefill are the tuner's (median of three, 8K prompt); the gate is
the needle at 90% of the tuned context, capped at 200000 tokens. The rows are in
`bench/results.jsonl`, tagged with the round. All but the two presets above (and the
0.5B smoke-test model) have since left `config/models.ini`; `golem pull <name>` brings
back one the library lists.

| Preset | Split | Context | Decode | Prefill | VRAM | Gate | Needle prefill | At depth |
|---|---|---|---|---|---|---|---|---|
| **`ornith-1.5-35b`** | n-cpu-moe 35 | 248832 | 63.5 t/s | 846 | 11249 MiB | 200K PASS | 671 t/s | 35.9 t/s |
| `qwen3.6-35b-a3b` | n-cpu-moe 37 | 253952 | 60.8 t/s | 493 | 11246 MiB | 200K PASS | 655 t/s | 36.8 t/s |
| `nemotron-3.5-lightning-30b` | n-cpu-moe 41 | 344064 | 44.7 t/s | 930 | 11262 MiB | 200K PASS | 1134 t/s | 22.6 t/s |
| `qwen3.5-9b` | all on the card | 258048 | 52.1 t/s | 1659 | 11250 MiB | 200K PASS | 941 t/s | 21.9 t/s |
| `ornith-1.5-9b` (Q6_K) | all on the card | 156672 | 42.3 t/s | 1540 | 11257 MiB | 141K PASS | 1039 t/s | 23.8 t/s |
| `gemma-4-12b-it-qat-q4_0` | all on the card | 262144 | 40.0 t/s | 1244 | 10674 MiB | 200K PASS | 476 t/s | 19.5 t/s |
| `qwen2.5-0.5b-instruct` | all on the card | 32768 | 340.1 t/s | 19452 | 1286 MiB | 29K PASS | 11395 t/s | 179.3 t/s |

Ornith-1.5-9B at Q6_K fills the card at 156672 tokens; at Q4_K_M (above) it holds 262144.
Nemotron's hand-set preset (`n-cpu-moe 30`) spilled, which is where its old 25 t/s came
from; at the tuned split it is the only model here past 262144.

## How golem picks a model

The engine is a router over a preset file - `MODELS_PRESET`, the shipped
`config/models.ini` unless a tune adopted `data/tuned/models.effective.ini` - and each
section is one model with its own flags. A request's `model` field picks the section;
`MODEL_ALIAS` in `golem.toml` is the section clients get by default, and the name
`golem claude` and `golem env` hand them. `golem up` fetches the default section's files
(its `; Download:` lines) when they are missing.

## Swapping

1. `golem pull owner/repo[:quant]` (or a name from `config/library.ini`): golem asks the
   Hub, picks the quant this machine can hold, switches on the MTP head when the file has
   one, and writes the preset section.
2. `golem tune <preset> --adopt`: the split, the context and the gate, written as this
   machine's overlay.
3. `MODEL_ALIAS = "<preset>"` in `golem.toml` to make it the default; clients that name a
   model pick any section without it.

## Memory math, two regimes

**Dense models (all in VRAM)** - OS/display overhead (~0.5-1.5 GB on a desktop GPU) leaves
~10.5 GB usable on a 12 GB card. Weights + KV (q8) must fit, and how far the context goes
depends on the attention as much as the weights: Qwen3.5-9B and Ornith-1.5-9B at Q4
(5.6 GB, hybrid attention) hold 262144 tokens, Ornith-1.5-9B at Q6_K (7.0 GB) 156672.

**MoE models** - `n-cpu-moe N` keeps the first N layers' experts in system RAM and puts
the rest on the GPU - lower N is faster and takes more VRAM. VRAM then holds attention,
the dense parts, the remaining experts and the KV cache, so 25-35B models fit with a 256K
context. `load-mode = mlock` pins the whole model into RAM at start (on llama.cpp b11879
and later, `mmap+mlock`); a sweep loads with mmap so a 32 GB box survives the restarts.

---

# Benchmarks by setup

One section per machine. To add yours: configure the candidate in `golem.toml`, then run
`golem bench` (decode best/median + MTP acceptance, prefill on a ~30K prompt) and
`golem eval --long 200000` (tool calls, 200K needle, decode at 200K depth), and record the
`RESULT` lines in a new subsection. Measure best-of-3 on a quiet desktop.

## Setup A — 12 GB VRAM GPU + 32 GB RAM, Windows host (native, no container)

RTX 3060 12 GB, i7-12700KF (8P+4E, 20 threads), 32 GB DDR. llama.cpp b11149 Windows/CUDA 13.
Qwen3.6-35B-A3B UD-Q4_K_M + MTP, ctx 236544. Decode = 600-token code generation, best-of-3;
prefill = 35K-token code prompt, prompt cache off. Measured 2026-09-24.

### Tuning sweep (current default in bold)

| Config | VRAM total | decode | prefill |
|---|---|---|---|
| n-cpu-moe 34, 20 threads, ub 512 (previous default) | 11.8 GB | 60.6 t/s | 340 t/s |
| n-cpu-moe 34, 8 threads | 11.8 GB | 62.4 t/s | — |
| n-cpu-moe 34, 12 threads | 11.8 GB | 64.6 t/s | — |
| n-cpu-moe 34, 16 threads | 11.8 GB | 65.7 t/s | — |
| n-cpu-moe 34, 16 threads, ub 1024 | 11.8 GB | 64.6 t/s | 563 t/s |
| n-cpu-moe 34, 16 threads, ub 2048 | 11.8-11.9 GB | 63-65 t/s | 839 / 505 t/s ⚠ |
| n-cpu-moe 34, 16 threads, ub 4096 | 11.9 GB | 63.3 t/s | 506 t/s ⚠ |
| n-cpu-moe 33, 16 threads, ub 2048 | 11.9 GB | 66.2 t/s | 489 t/s ⚠ |
| n-cpu-moe 35, 16 threads, ub 2048 | 11.8 GB | 64.9 t/s | 800 t/s |
| n-cpu-moe 35, 16 threads, ub 3072 | 11.9 GB | 65.1 t/s | 920 t/s |
| **n-cpu-moe 36, 16 threads, ub 2048** | **11.6 GB** | **63.5 t/s** | **823 t/s** |
| n-cpu-moe 36, 16 threads, ub 3072 | 11.8 GB | 63.2 t/s | 966 t/s |
| MTP draft max 2 / 3 (default) / 4 | — | 67.3 / 65.7 / 65.4 t/s | — |

Reading:

- **ubatch is the prefill lever.** Prefill streams the RAM-resident experts to the GPU once
  per ubatch; 512 → 2048 cuts that traffic 4x: 340 → ~820 t/s, decode unchanged. Claude
  Code's ~27K-token first request drops from ~80 s to ~35 s.
- **⚠ = the VRAM cliff.** Above ~11.8 GB total the Windows driver silently spills into shared
  system memory, and prefill falls ~40% (505 t/s). The same config (n34 + ub2048) landed on
  either side of it across runs because the desktop's own VRAM use drifts (0.5-1.6 GB idle
  measured the same day). n36 + ub2048 keeps ~0.7 GB headroom at ~1 decode t/s cost.
  ub 3072 prefills faster still but sits back at the edge.
- **Threads:** 16 of 20 beats all 20 (~+8%) and 8 P-cores only (~+5%). Likely the RAM-expert
  decode is memory-bound, so the last SMT/E-core threads add sync overhead, not bandwidth.
- **MTP draft length** is within noise at 2-4 (acceptance ~73% at 3, ~83% at 2); the default
  3 stays.
- **Context 231K vs 262K**: Qwen3.6's hybrid attention (10 of 40 layers carry a KV cache)
  makes the KV small, so the cut is about not over-allocating, not speed.

### Second sweep (2026-10-03): the quant ceiling is RAM, not VRAM

Same machine, paired same-hour runs (ambient day swings decode ±3 t/s — always
compare configs measured back-to-back, never across days).

| Config (all mlock unless noted, n-cpu-moe 36, ub 2048) | VRAM | decode | prefill |
|---|---|---|---|
| Q4_K_M + MTP draft 3 (default) — b11149 | 11.8 GB | 60.2 / 59.8 t/s | 816 t/s |
| Q4_K_M + `--spec-draft-n-max 2` | 11.8 GB | 60.6 / 58.0 t/s | 829 t/s |
| Q4_K_M — **b11379** | 11.8 GB | 60.6 / 60.4 t/s | 822 t/s |
| Q4_K_XL (22.36 GB) | 11.8 GB | 57.0 / 56.9 t/s | 768 t/s |
| Q5_K_S (24.94 GB), n-cpu-moe 37, `--load-mode auto`, warm | 11.5 GB | 54.5 / 54.0 t/s | 773 t/s |
| Q5_K_S (24.94 GB), n-cpu-moe 37, `--load-mode mlock` | 11.6 GB | 56.0 / 54.9 t/s | — |

Reading:

- **Q5_K_S is the 64 GB-day model, not a today model.** Confirmed in a clean
  retest (quiet desktop, fully warmed) in *both* load modes: `auto` leaves
  0.8 GB available with decode −10%, `mlock` locks 21.1 GB and leaves 1.5 GB —
  either way there is no headroom left for the client, a browser, or a build,
  and decode sits ~10% under Q4_K_M. The first pass after an `auto` load also
  page-faults at 13.8 t/s until the experts fault in. The Q4→Q5 "decode is
  step-overhead-bound, quality is free" logic holds only while the weights
  stay resident — RAM capacity breaks that assumption before VRAM does. The
  file stays on disk ready for a RAM upgrade.
- **Q4_K_XL is dominated**: −6% decode, −7% prefill vs Q4_K_M for +0.23 GB of
  importance-matrix polish on tensors that are mostly GPU-side anyway.
- **draft-max 2 vs 3 is a wash** (confirmed paired, as the first sweep hinted):
  60.6/58.0 vs 60.2/59.8. Acceptance 80% vs 73% doesn't convert to speed here.
  Default 3 stays.
- **b11379 adopted**: identical speed to b11149, and it passes the full gate —
  needle found at 211.8K (629 t/s prefill), decode at depth 30.4 t/s, tools OK.
  Staying current is cheap while the eval keeps refereeing.
- Measurement hygiene: a 25 GB model download or a background disk scan
  starves RAM (mlock + file cache) and costs 5-10 t/s of decode — never adopt
  or reject a config from a bench taken while one runs.

### Current default, end to end

| Test | Result |
|---|---|
| `golem bench` (2026-09-24, b11149) | decode 63.4 t/s best / 63.2 median, MTP acceptance 73%, prefill 842 t/s (28.6K tokens), VRAM 11.8 GB |
| `golem bench` (2026-10-03, b11379) | decode 60.6 t/s best / 60.4 median, acceptance 73%, prefill 822 t/s, VRAM 11.8 GB (busier desktop day) |
| `golem eval --long 200000` (2026-10-03, b11379) | arithmetic + tool calls PASS; needle 211.8K **PASS**, prefill 629 t/s over the full 211.8K; decode at depth 30.4 t/s |
| Real Claude Code session | 27,251-token first prompt at 747 t/s; follow-up turns reprocess only the new tokens (56) |

Cache reuse note: Qwen3.6 is a hybrid recurrent model, so the server rolls back through
context checkpoints taken per ubatch. An agent turn that *appends* reuses everything; a
request that *replaces* the tail (as the eval's depth probe does) re-processes up to one
ubatch (~2K tokens).

## Setup A (docker) — 12 GB VRAM GPU + 32 GB RAM, Windows host, Docker (WSL2)

Superseded on this machine by the native Windows mode above (~+50% decode: the WSL
paravirtualized GPU costs batch-1 decode dearly). Kept for Linux/docker reference.

### Speculative decoding + expert placement (measured 2026-09-23)

Qwen3.6-35B-A3B UD-Q3_K_M, ctx 262144, 600-token code generations, best-of-3.
VRAM includes ~2 GB taken by the Windows desktop.

| Config | VRAM total | decode | prefill |
|---|---|---|---|
| `--cpu-moe`, no MTP (pre-MTP baseline) | 6.5 GB | 23 t/s | — |
| Q3_K_M, `--cpu-moe` + MTP | 8.3 GB | 35 t/s | — |
| Q3_K_M, `--n-cpu-moe 34` + MTP | 10.5 GB | 35-39 t/s | — |
| Q3_K_M, `--n-cpu-moe 32` + MTP | 11.2 GB | 37-42 t/s | 522 t/s @211K |
| Q3_K_M, `--n-cpu-moe 31` + MTP | 11.5 GB | 38-50 t/s | 660-720 t/s @31K |

MTP draft acceptance ~70-80% (mean 3.1-3.4 tokens per step), identical across quants.

### Quant sweep at `--n-cpu-moe 34` + MTP (measured 2026-09-23, same day, quiet desktop)

| Quant | bits/weight | VRAM | decode | prefill @211K |
|---|---|---|---|---|
| UD-Q3_K_M | ~3.8 | 11.5 GB | 37-42 t/s | 522 t/s |
| UD-IQ4_XS | ~4.1 | 11.0 GB | 26-32 t/s | not measured |
| **UD-Q4_K_M (default)** | ~5.1 | 11.5 GB | **37-47 t/s** | 336 t/s |

Reading: under MTP, decode is step-overhead-bound, not CPU-bandwidth-bound, so Q4_K_M
decodes as fast as Q3_K_M while carrying better weights — it wins on quality at equal
speed. IQ4_XS is dominated: slower than both with barely more precision than Q3_K_M
(its extra bits sit in experts that are mostly off-GPU). The Q4_K_M price is prefill:
reading ~30% more bytes per prompt token costs ~35% throughput on full-context fills,
paid once per session / auto-compact. Tool calls (OpenAI + Anthropic) and the 200K
needle pass on both Q3_K_M and Q4_K_M with MTP on. WSL RAM at Q4_K_M n34: ~18 of 23 GB.

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
Usable only as a short-context profile (see `golem.toml.example`); on a 24 GB card it would
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

**llama.cpp build gate:** builds are not interchangeable — b11149 (2026-09-24,
CUDA 13.4 runtime) answered the 200K needle wrong *twice, identically* (tool calls,
100K needle, throughput and prefill all fine) on Linux, while b10991 passes. So
the release is pinned - `build` in `engines/llama.cpp/engine.ini`, the one pin
the native install and the container image both use - and a bump is
`ENGINE_BUILD = "b<new>"`, `golem restart`, and adoption only if
`golem eval --long 200000` passes. Currently b11379, adopted 2026-10-03 (the
Windows build of b11149 also passes; that regression is Linux-build-specific).

