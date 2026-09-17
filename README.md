# Golem

**A local AI construct for coding agents.**
One RTX 3060 12GB, one llama.cpp container, zero tokens leaving the LAN. It speaks both the
**Anthropic Messages API** and the **OpenAI API**, so Claude Code — or any OpenAI client, or a
remote gateway — plugs straight in. A bundled MCP server gives agents **web search + URL
reading** through a self-hosted SearXNG.

```
        ▟█▙   forge  ->  animate (load a model)  ->  it works
        ▜█▛
```

```
  Claude Code / OpenCode / Aider / remote gateway
        │                          │
        │ Anthropic :8000          │ MCP streamable HTTP :8811/mcp  (Bearer)
        │ OpenAI    :8000/v1       ▼
        │                    ┌──────────────┐      ┌──────────┐
        ▼                    │ mcp-search   │ ───▶ │ SearXNG  │ ──▶ web
  ┌───────────────────┐      └──────────────┘      └──────────┘
  │ llama-server :8000│       (docker, internal)
  │ Qwen 3.5 9B GGUF  │
  └───────────────────┘
```

Current deployment: **Qwen 3.5 9B Q4_K_M** — 53 tok/s decode, 32K context, native tool calling,
in Docker with `restart: unless-stopped` (built for 24/7).

---

## Requirements

| | |
|---|---|
| GPU | NVIDIA 12 GB VRAM (RTX 3060) — the model **must** fit fully in VRAM; 24B+ models do not |
| Docker | Docker Desktop / Engine + Compose v2 + NVIDIA GPU passthrough |
| Disk | ~6 GB model + ~4 GB image |

## Quickstart

```bash
cp .env.example .env && $EDITOR .env     # set keys; docker mode is the default here

./golem pull             # download the model into data/models/
./golem up --docker      # inference + SearXNG + MCP search
./golem status           # health of everything
./golem claude           # launch Claude Code pointed at the local model
./golem bench            # sanity-check generation
./golem down             # stop everything
```

### `./golem` commands

| Command | Does |
|---|---|
| `setup` | machine report, build deps, directory scaffold (native mode) |
| `install` | build `llama.cpp` with CUDA (native only) |
| `pull` | download `MODEL_FILE` from `HF_REPO` into `data/models/` |
| `up` / `down` / `restart` | start / stop / cycle the stack (docker default; `--native` skips the MCP services) |
| `status` | health of inference + MCP search |
| `logs [inference\|search]` | follow logs |
| `bench` | fixed code prompt against the OpenAI endpoint |
| `claude [args…]` | `exec claude` with `ANTHROPIC_*` already set |
| `env` | print those exports |

---

## Connecting clients

### Claude Code (direct — no gateway needed)

`llama-server` exposes the **Anthropic Messages API natively** (`POST /v1/messages`), so Claude
Code talks to it without any translation layer:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from .env>"
export ANTHROPIC_MODEL="local-model"
export ANTHROPIC_SMALL_FAST_MODEL="local-model"
claude
```

On the Golem machine itself: `./golem claude` does all of this. See
[docs/claude-code.md](docs/claude-code.md).

### A remote gateway (claude-code-router, LiteLLM, …)

Add Golem as an **OpenAI-compatible provider** on the machine that runs the gateway:

```json
{
  "name": "golem",
  "api_base_url": "http://<golem-host>:8000/v1/chat/completions",
  "api_key": "<LOCAL_AI_API_KEY from .env>",
  "models": ["local-model"]
}
```

### Anything OpenAI (OpenCode, Aider, scripts)

Base URL `http://<golem-host>:8000/v1`, model `local-model`, Bearer = `LOCAL_AI_API_KEY`.
OpenCode and Aider have much smaller per-turn overheads than Claude Code (~6-10K tokens vs
15-20K) — on a 32K window that doubles the usable working context. See [docs/models.md](docs/models.md).

### Web search + web read for agents (MCP)

`mcp-search` (docker) exposes [`mcp-searxng`](https://github.com/ihor-sokoliuk/mcp-searxng) over
streamable HTTP — tools: `searxng_web_search`, `web_url_read`, `searxng_search_suggestions`.
Point any MCP client at it (the repo ships a ready
[.mcp.json.example](.mcp.json.example) — copy it to `.mcp.json` and fill in host + key):

```json
{
  "mcpServers": {
    "golem-search": {
      "type": "http",
      "url": "http://<golem-host>:8811/mcp",
      "headers": { "Authorization": "Bearer <MCP_SEARCH_API_KEY from .env>" }
    }
  }
}
```

Notes:

- Claude Code's built-in `WebFetch` works out of the box with a local model; its built-in
  `WebSearch` does **not** (it is a server-side Anthropic tool). Use this MCP instead — keyless,
  self-hosted, no API quotas.
- SearXNG itself is not published to the host; only `mcp-search` talks to it.

---

## The model

Current: **Qwen 3.5 9B Q4_K_M** (`lmstudio-community/Qwen3.5-9B-GGUF`) — chosen as the best
12GB-VRAM agentic-coding fit (top of willitrunai.com's RTX 3060 tier list, native tool calling,
hybrid reasoning). Served with FlashAttention + q8_0 KV cache at 32K context.

To swap: edit `HF_REPO` / `MODEL_FILE` in `.env`, `./golem pull`, `./golem restart --docker`.
Keep `MODEL_ALIAS=local-model` so clients never change. VRAM math and candidates:
[docs/models.md](docs/models.md).

---

## Repository layout

```
.
├── golem                     Control script — the front door
├── compose.yml               inference + search + mcp-search
├── .env.example              Canonical config; copy to .env
├── .mcp.json.example         Ready MCP config template (copy → .mcp.json)
├── docker/
│   └── mcp-search.Dockerfile node22 + mcp-searxng (streamable HTTP)
├── config/searxng/           SearXNG settings (JSON format enabled)
├── docs/
│   ├── architecture.md       Request flow, ports, auth
│   ├── claude-code.md        Claude Code → Golem, in detail
│   ├── backends.md           Swapping llama.cpp for Ollama/vLLM/…
│   └── models.md             Model choice, VRAM math
├── scripts/                  Building blocks the control script calls
├── data/models/              GGUF files (git-ignored)
├── data/cache/               Download / HF cache (git-ignored)
└── logs/                     Run logs (git-ignored)
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `llama-server` exits / OOM | Lower `CONTEXT_SIZE` (KV ≈ 75 KB/token at q8_0) or use a smaller quant |
| Claude Code still hits api.anthropic.com | Export the four `ANTHROPIC_*` vars (`./golem env`); use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` |
| MCP search tool returns 404 "Search endpoint" | `SEARXNG_URL` must be the SearXNG **root** (`http://search:8080`), without `/search` |
| MCP `Invalid Host header` | Add your client's host to `MCP_HTTP_ALLOWED_HOSTS` in `compose.yml` |
| Slow first answer | Prompt cache is cold; prefill runs ~200-500 tok/s on a 3060 and caches after |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
- [ihor-sokoliuk/mcp-searxng](https://github.com/ihor-sokoliuk/mcp-searxng) — MCP web search
- [searxng/searxng](https://github.com/searxng/searxng) — metasearch
