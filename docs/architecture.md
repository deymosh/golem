# Architecture

## Services

| Service | Container | Listens | Protocol | Config |
|---|---|---|---|---|
| inference | `golem-inference` | `0.0.0.0:8000` | **Anthropic** Messages API (`/v1/messages`) **and** OpenAI API (`/v1/chat/completions`, `/v1/models`) | `.env` runtime vars → compose `command` |

There is **no translation layer**: llama.cpp serves the Anthropic Messages API natively, so
agents connect straight to `:8000`. Both APIs are served by the same process, model and KV
cache — a request through either protocol hits the same weights and the same prompt cache.

## Request flows

**Inference** — Claude Code builds an Anthropic `POST /v1/messages` to `ANTHROPIC_BASE_URL`
(`:8000`), auth `x-api-key` (or Bearer) = `LOCAL_AI_API_KEY`. llama-server runs the GGUF and
streams Anthropic SSE events back (`message_start` → `content_block_delta` → `message_stop`),
including `thinking` and `tool_use` blocks. OpenAI clients hit `/v1/chat/completions` with
`Authorization: Bearer` = the same key. Tool calling needs `--jinja` (enabled in compose).

Verified empirically against this stack (2026-09-20): non-streamed and streamed Anthropic
responses, Bearer auth, and a tool-use round trip (`stop_reason: "tool_use"`) all behave as the
Anthropic spec describes. Upstream llama.cpp documents the endpoint under
"Anthropic-compatible API Endpoints" with the caveat that "no strong claims of compatibility
with the Anthropic API spec are made" — it is a native implementation, not a certified one.

## Ports

| Port | Purpose | Override |
|---|---|---|
| 8000 | inference (Anthropic + OpenAI) | `LOCAL_AI_PORT` |

It binds `0.0.0.0` (LAN use). Anyone who can reach the port can use your GPU — keep the real
secret in `LOCAL_AI_API_KEY` and firewall if the LAN is not trusted.

## Memory timeline

Two regimes, chosen by `EXTRA_ARGS` in `.env` (details and measured numbers:
[models.md](models.md)):

- **Dense model, all weights in VRAM** — weights are resident from boot; the KV cache is
  virtual and grows as the conversation fills (~2.3 GiB per 32K slot at q8_0). What fits is
  `weights + KV + OS/display overhead` inside VRAM, which is why a 12 GB card with a ~9B Q4
  model caps around 100K context.
- **MoE model with `--cpu-moe`** — attention + dense parts + KV stay in VRAM; all expert FFN
  weights live in system RAM. VRAM stays nearly flat as context grows (KV still grows), which
  is how a 30B-class model serves a 262K window on a 12 GB card. System RAM needs headroom for
  the experts — under WSL2 raise the VM limit via `%USERPROFILE%\.wslconfig` (`memory=…`).
  `--n-cpu-moe N` is the tuned variant: only the first N layers' experts go to RAM and the
  leftover VRAM holds the rest.
- **Speculative decoding (MTP)** — Qwen3.5+ models ship a Multi-Token Prediction head that
  drafts ~3 tokens per step; the main model verifies them in one pass, so output is identical
  and decode roughly doubles. Enabled by `SPEC_TYPE=draft-mtp`, with `DRAFT_FILE` pointing at
  a standalone MTP GGUF when the main file lacks the layers.
