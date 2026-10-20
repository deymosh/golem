# Engine and model landscape

What a research pass on 2026-10-20 found, and what it means for Golem on the reference
box (RTX 3060 12 GB, 32 GB RAM, Windows native). Everything here is *found*, not
*measured* — nothing becomes a default without the paired bench and the 200K eval gate.

## Engines

### The filter that decides everything

Golem has no gateway and never will: the engine itself must speak the **Anthropic
`/v1/messages` API** for Claude Code to connect directly, plus OpenAI for everything
else. That single requirement eliminates most of the field:

| Engine | Anthropic API | GGUF | Verdict for us |
|---|---|---|---|
| **llama.cpp** (default, b11379) | yes | yes | keeps the default |
| **Strata** v0.1.45 | yes | its own | rejected (measured): no 200K on 12 GB, −15% speed; 0.2.x pending the probe |
| **ik_llama.cpp** | no | yes | rejected (measured): 3.4× slower |
| vLLM, SGLang, TensorRT-LLM, LMDeploy, TGI | no | partial/no | datacenter engines, Linux-first, and the docker path costs ~50% decode here |
| Ollama, LM Studio, TabbyAPI | no | yes | front-ends/wrappers; Anthropic clients cannot use them directly |

### New entrants found (none ready)

- **Ferrox** — from-scratch hobby GGUF engine (Llama/Qwen/Gemma/Phi-3), r/OpenSourceAI.
  Young, no server API surface we can gate. Watch.
- **Frink** (github.com/antonellof/frink) — pure-Rust GGUF engine, MoE support, CUDA,
  OpenAI-compatible server, benchmarked against llama.cpp. No Anthropic API. Watch; also
  a useful reference for the golem-rs GGUF reader.
- **Prism** — llama.cpp fork adding ternary **PTQ1_0** models (Bonsai 2). Interesting
  mostly for its model format (below).
- **Windows ML** added experimental llama.cpp hosting (Microsoft, Oct 2026) — task APIs,
  not an agent-serving server. Not for us.

### The actionable engine item

llama.cpp upstream is at **b11687+** vs our pinned b11379, with work directly relevant to
this box: Qwen4 MTP rollback support and DSpark speculative decoding (the draft family
the Nemotron preset expects). A build bump is the cheapest engine win available — one
install, paired bench, 200K eval gate, adopt or record.

## Models

### Our size class (≈30–35B total, ≈3B active, MoE, 200K ctx)

The 2026 "best local coder" lists still point at dense-or-smaller picks (Gemma 4 12B,
Qwen3.5-9B, Qwen2.5-Coder-14B) for people without expert-offload tuning; nothing measured
beats the current default `ornith-1.5-35b` (SWE-V 79) in this class.

- **Qwen3-Coder-30B-A3B** — the community favorite in many 2026 threads, but it is the
  2025 generation; Ornith-1.5 is its stronger descendant-class successor. Skip.
- **Nemotron-3.5-Lightning-30B-A3B** — already presetted here, unmeasured; long-context
  summarizer candidate (SWE-V 52, cheap KV). Roadmap item, unchanged.
- Flagships (**GLM-5.2** 755 GB, **Kimi K3** 1.56 TB, **DeepSeek V4 Flash** 156 GB) —
  not for a 12 GB card. DeepSeek V4 Pro is the best open-weights coder overall
  (vals.ai) and none of it runs here.

### Candidates worth gating when the GPU is free

- **Ornith 1.5 9B** (dense, Qwen3.5-based) — SWE-V 70.6 and a striking GPQA 86.4;
  Q6_K is ~7 GB, fully in VRAM, ~92 t/s on a 3080 Ti-class card. Candidate to replace
  the `qwen3.5-9b` everyday-assistant preset with a much stronger coder at the same
  footprint. (atomic.chat 12 GB guide, Sep 2026)
- **Gemma 4 12B QAT Q4_0** — 256K native context, 6.5 GB, solid agentic scores; a
  general-assistant alternative in the same slot. Same guide.
- **Bonsai 2 27B ternary (PTQ1_0)** — LiveCodeBench 90 at 5.5 GB, ~58 t/s — but it needs
  the Prism engine fork and its VRAM at 64K ctx is already 10.2 GB: **200K does not fit
  on 12 GB**, so it fails our context requirement. Recorded, not pursued.

### Watch

- **Qwen3.8-Flash-Next pruned "Coder"** via Strata (125B → 256/512 experts, ~1.89 bpw,
  91.3% of the full model's SWE-V, ~50 t/s on 12 GB + 32 GB RAM) — the one model that
  would justify Strata on this box. Tracked in [strata-models.md](strata-models.md).

## Reading rules (unchanged)

Compare only inside a size class, only same-session pairs, best-of-3 on a quiet desktop;
nothing is adopted without `./golem eval --long 200000` PASS. Vendor and guide numbers
above are reasons to *measure*, never reasons to adopt.
