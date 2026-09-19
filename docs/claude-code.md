# Pointing Claude Code at Golem

Prerequisite: `./golem up --docker` and `./golem status` green.

## Option A — `./golem claude` (on the Golem machine)

```bash
./golem claude            # exec claude with ANTHROPIC_* set from .env
```

## Option B — environment variables (any machine on the LAN)

Linux / macOS / WSL2:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"   # llama-server, Anthropic-native
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from .env>"
export ANTHROPIC_MODEL="local-model"                   # = MODEL_ALIAS
export ANTHROPIC_SMALL_FAST_MODEL="local-model"
claude
```

Windows PowerShell:

```powershell
$env:ANTHROPIC_BASE_URL = "http://<golem-host>:8000"
$env:ANTHROPIC_AUTH_TOKEN = "<LOCAL_AI_API_KEY from .env>"
$env:ANTHROPIC_MODEL = "local-model"
$env:ANTHROPIC_SMALL_FAST_MODEL = "local-model"
claude
```

Notes:

- Use **`ANTHROPIC_AUTH_TOKEN`**, not `ANTHROPIC_API_KEY`. With a custom base URL Claude Code
  sends it as a bearer token; `ANTHROPIC_API_KEY` is for real Anthropic auth.
- Settings can also live in `~/.claude/settings.json` under `env`.
- No translation layer is involved — llama.cpp implements `/v1/messages` natively, including
  streaming, `thinking` and `tool_use` blocks (verified against this stack; see
  [architecture.md](architecture.md)).

## Web search for Claude Code

Claude Code's built-in `WebFetch` works with the local model out of the box. Its built-in
`WebSearch` does not (it executes server-side on Anthropic's API). With Golem's MCP:

```bash
claude mcp add --transport http golem-search http://<golem-host>:8811/mcp \
  --header "Authorization: Bearer <MCP_SEARCH_API_KEY>"
```

or copy the repo's `.mcp.json.example` to `.mcp.json` in the project (fill in `<golem-host>`
and the key). The model then gets `searxng_web_search`, `web_url_read` and suggestions as
regular tools.

## Sanity check

```bash
ANTHROPIC_BASE_URL="http://<golem-host>:8000" \
ANTHROPIC_AUTH_TOKEN="<key>" \
claude -p "reply with the single word: pong"
```

A `pong` (watch `nvidia-smi` on the Golem machine) means the whole chain works.

## Reverting to real Anthropic

```bash
unset ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN ANTHROPIC_MODEL ANTHROPIC_SMALL_FAST_MODEL
```
