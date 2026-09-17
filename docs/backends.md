# Swapping the inference backend

Golem assumes one contract from the backend:

- an **OpenAI-compatible** `POST /v1/chat/completions` at `LOCAL_AI_BASE_URL` (used by remote
  gateways, OpenCode, Aider, `bench`), and ideally also
- the **Anthropic** `POST /v1/messages` (used by Claude Code directly).

llama.cpp (default) honors both. Most other backends speak OpenAI only — if you swap, point
Claude Code at a translation gateway again, or at whatever Anthropic endpoint the backend offers.

| Backend | Base URL | Notes |
|---|---|---|
| **llama.cpp** (default) | `http://127.0.0.1:8000/v1` | Anthropic + OpenAI native, `--jinja` tool calls |
| **Ollama** | `http://127.0.0.1:11434/v1` | OpenAI only; no native `/v1/messages` |
| **vLLM** | `http://127.0.0.1:8000/v1` | OpenAI only; strong if you ever get a bigger GPU |
| **LM Studio** | `http://127.0.0.1:1234/v1` | OpenAI only; GUI-driven |

To swap: set `LOCAL_AI_BASE_URL` + `MODEL_ALIAS` in `.env` and start that backend yourself —
`docker/mcp-search.Dockerfile`, the `search` service and the MCP tooling are backend-agnostic
and keep working.

`health-check-ai.sh` and `benchmark-ai.sh` only use `LOCAL_AI_HOST/PORT` + `MODEL_ALIAS`, so they
work against any OpenAI-compatible backend. `download-model.sh` / `install-llama-cpp.sh` /
`start-local-ai.sh` are llama.cpp-specific and simply go unused with another backend.

## Legacy: the in-repo gateway

Golem previously shipped a claude-code-router gateway to translate Anthropic → OpenAI. It was
removed once llama.cpp gained native Anthropic support. If you still need one (e.g. for an
OpenAI-only backend), run claude-code-router on another machine and add Golem as its provider:

```json
{ "name": "golem", "api_base_url": "http://<golem-host>:8000/v1/chat/completions",
  "api_key": "<LOCAL_AI_API_KEY>", "models": ["local-model"] }
```
