# Swapping the inference backend

Golem assumes one contract from the backend:

- an **OpenAI-compatible** `POST /v1/chat/completions` at `LOCAL_AI_BASE_URL`, and ideally also
- the **Anthropic** `POST /v1/messages` (used by Claude Code directly).

llama.cpp (default) honors both. Most other backends speak OpenAI only — swapping to one of
them keeps every OpenAI client working, but Claude Code will not connect to them directly
(it speaks the Anthropic protocol), which is exactly why the default is llama.cpp.

| Backend | Base URL | Notes |
|---|---|---|
| **llama.cpp** (default) | `http://127.0.0.1:8000/v1` | Anthropic + OpenAI native, `--jinja` tool calls |
| **Ollama** | `http://127.0.0.1:11434/v1` | OpenAI only; no native `/v1/messages` |
| **vLLM** | `http://127.0.0.1:8000/v1` | OpenAI only; strong if you ever get a bigger GPU |
| **LM Studio** | `http://127.0.0.1:1234/v1` | OpenAI only; GUI-driven |

To swap: set `LOCAL_AI_BASE_URL` + `MODEL_ALIAS` in `golem.toml` and start that backend yourself.

`golem status` and `golem eval` (sanity, tool calls, needle) only use
`LOCAL_AI_PORT` + `LOCAL_AI_API_KEY` + `MODEL_ALIAS`, so they work against any
OpenAI-compatible backend. The tok/s figures in `golem bench` and `golem eval` read
llama.cpp's per-request `timings` object and come out empty on backends that don't send it. `download-model.sh` / `install-llama-cpp.sh` /
`start-local-ai.sh` are llama.cpp-specific and simply go unused with another backend.
