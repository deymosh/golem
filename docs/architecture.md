# Architecture

## Services

| Service | Container | Listens | Protocol | Config |
|---|---|---|---|---|
| inference | `golem-inference` | `0.0.0.0:8000` | **Anthropic** Messages API (`/v1/messages`) **and** OpenAI API (`/v1/chat/completions`, `/v1/models`) | `.env` runtime vars → compose `command` |
| mcp-search | `golem-mcp-search` | `0.0.0.0:8811` | MCP over streamable HTTP (`/mcp`), Bearer auth | `MCP_*` env in compose |
| search | `golem-search` | compose network only | SearXNG HTTP API (JSON) | `config/searxng/settings.yml` |

There is **no gateway layer**: llama.cpp serves the Anthropic Messages API natively, so agents
connect straight to `:8000`. A remote gateway (claude-code-router, LiteLLM, …) on another machine
can still sit in front of it as a plain OpenAI-compatible upstream.

## Request flows

**Inference** — Claude Code builds an Anthropic `POST /v1/messages` to `ANTHROPIC_BASE_URL`
(`:8000`), auth `x-api-key` = `LOCAL_AI_API_KEY`. llama-server runs the GGUF (all layers on GPU,
FlashAttention, q8_0 KV cache) and streams Anthropic events back. OpenAI clients hit
`/v1/chat/completions` with `Authorization: Bearer` = the same key. Tool calling needs `--jinja`
(enabled in compose).

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

## VRAM timeline

Weights (5.24 GiB) are resident from boot; the KV cache is virtual and grows as the conversation
fills: a full 32K slot costs ~2.3 GiB. Worst case ≈ 9.2 GiB + Windows/display (~1.5 GiB) — this
is why `CONTEXT_SIZE=32768` is the ceiling on a 12 GB card, not the model's 262K native limit.
