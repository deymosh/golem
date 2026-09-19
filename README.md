# Golem

**A local AI construct for coding agents.**
One GPU, one llama.cpp container, zero tokens leaving your LAN. It speaks both the
**Anthropic Messages API** and the **OpenAI API**, so Claude Code — or any OpenAI client —
plugs straight in. A bundled MCP server gives agents **web search + URL
reading** through a self-hosted SearXNG.

```
        ▟█▙   forge  ->  animate (load a model)  ->  it works
        ▜█▛
```

```
  Claude Code / OpenCode / Aider
        │                          │
        │ Anthropic :8000          │ MCP streamable HTTP :8811/mcp  (Bearer)
        │ OpenAI    :8000/v1       ▼
        │                    ┌──────────────┐      ┌──────────┐
        ▼                    │ mcp-search   │ ───▶ │ SearXNG  │ ──▶ web
  ┌───────────────────┐      └──────────────┘      └──────────┘
  │ llama-server :8000│       (docker, internal)
  │ local GGUF model  │
  └───────────────────┘
```

Default configuration: a **MoE model (35B total / 3B active)** served at **262K context** with
expert weights offloaded to system RAM — quality of a 30B-class model on a 12 GB GPU, with
context headroom for Claude Code's 200K window. Dense models that fit entirely in VRAM are a
one-line swap. See [docs/models.md](docs/models.md) for measured numbers per setup.

---

## Requirements

| | |
|---|---|
| GPU | NVIDIA with 12 GB VRAM — dense models up to ~9B fit fully in VRAM; MoE models of the 30B class run with expert weights offloaded to system RAM |
| RAM | 32 GB recommended for MoE offload (16 GB is enough for dense models) |
| OS | Linux, or Windows with WSL2 (Docker Desktop + a WSL2 distro for the scripts) |
| Docker | Docker Engine or Desktop + Compose v2 + NVIDIA GPU passthrough |
| Disk | ~17 GB for the default model + ~4 GB image (less for smaller models) |

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

On Windows, run these from inside your WSL2 distro (Docker Desktop wires it to the same
engine). The native mode (`./golem up --native`) builds llama.cpp on the host and is
Linux/WSL2-only.

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

### Claude Code (direct)

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

### Anything OpenAI (OpenCode, Aider, scripts)

Base URL `http://<golem-host>:8000/v1`, model `local-model`, Bearer = `LOCAL_AI_API_KEY`.
OpenCode and Aider have much smaller per-turn overheads than Claude Code (~6-10K tokens vs
15-20K) — on a constrained context window that meaningfully increases the usable working
context. See [docs/models.md](docs/models.md).

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

Any GGUF that [llama.cpp](https://github.com/ggml-org/llama.cpp) can serve works. The default
(`HF_REPO` / `MODEL_FILE` in `.env`) is a MoE 35B/3B-active quant at 262K context via
`--cpu-moe`; dense alternatives that live fully in VRAM are listed in the same file.

To swap: edit `HF_REPO` / `MODEL_FILE` in `.env`, `./golem pull`, `./golem restart --docker`.
Keep `MODEL_ALIAS=local-model` so clients never change. VRAM math, memory regimes and measured
benchmarks per setup: [docs/models.md](docs/models.md).

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
│   └── models.md             Model choice, memory regimes, benchmarks per setup
├── scripts/                  Building blocks the control script calls
├── data/models/              GGUF files (git-ignored)
├── data/cache/               Download / HF cache (git-ignored)
└── logs/                     Run logs (git-ignored)
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `llama-server` exits / OOM | Lower `CONTEXT_SIZE` (KV ≈ 75 KB/token at q8_0) or use a smaller quant |
| MoE model slow or OOM under WSL2 | Give the WSL2 VM more RAM: `memory=24GB` in `%USERPROFILE%\.wslconfig` |
| Claude Code still hits api.anthropic.com | Export the four `ANTHROPIC_*` vars (`./golem env`); use `ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY` |
| MCP search tool returns 404 "Search endpoint" | `SEARXNG_URL` must be the SearXNG **root** (`http://search:8080`), without `/search` |
| MCP `Invalid Host header` | Add your client's host to `MCP_HTTP_ALLOWED_HOSTS` in `compose.yml` |
| Slow first answer | Prompt cache is cold; prefill runs ~200-500 tok/s on a 12 GB GPU and caches after |

## Credits

- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — inference server (OpenAI + Anthropic native)
- [ihor-sokoliuk/mcp-searxng](https://github.com/ihor-sokoliuk/mcp-searxng) — MCP web search
- [searxng/searxng](https://github.com/searxng/searxng) — metasearch
