# Architecture

## Services

| Service | Container | Listens | Protocol | Config |
|---|---|---|---|---|
| inference | `golem-inference` | `0.0.0.0:8000` | **Anthropic** Messages API (`/v1/messages`) **and** OpenAI API (`/v1/chat/completions`, `/v1/models`) | `.env` runtime vars → compose `command` |
| mcp-search | `golem-mcp-search` | `0.0.0.0:8811` | MCP over streamable HTTP (`/mcp`), Bearer auth | `MCP_*` env in compose |
| search | `golem-search` | compose network only | SearXNG HTTP API (JSON) | `config/searxng/settings.yml` |

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

**Web search** — the agent's MCP client POSTs JSON-RPC to `http://<host>:8811/mcp`
(`initialize` → `tools/call`). `mcp-searxng` queries SearXNG at `http://search:8080` over the
compose network and returns merged results from Google/Brave/DuckDuckGo/&c. `web_url_read`
fetches and converts a URL to markdown.

Hardening on `:8811`: bearer token (`MCP_HTTP_AUTH_TOKEN`), host allowlist
(`MCP_HTTP_ALLOWED_HOSTS`) and CORS allowlist (`MCP_HTTP_ALLOWED_ORIGINS`). If a client gets
`Invalid Host header`, add its host there.

## Ports

| Port | Purpose | Override |
|---|---|---|
| 8000 | inference (Anthropic + OpenAI) | `LOCAL_AI_PORT` |
| 8811 | MCP web search | `MCP_SEARCH_PORT` |

Both bind `0.0.0.0` (LAN use). Anyone who can reach the ports can use your GPU / your network
exit for searches — keep real secrets in `LOCAL_AI_API_KEY` / `MCP_SEARCH_API_KEY` and firewall
if the LAN is not trusted.

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
