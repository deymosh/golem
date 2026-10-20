# Pointing Claude Code at Golem

Prerequisite: `golem up` and `golem status` green.

Context: the router default serves 249344 tokens (244K), tuned on 2026-10-20 - the preset
asks for 236544 and `golem tune --adopt` grew it to what the card actually holds. Claude
Code assumes a 200K window and auto-compacts before it; the extra ~44K absorbs tokenizer
differences (the server counts with Qwen's tokenizer) and the final turn's output, so
requests never overflow.

## Option A — `golem claude` (on the Golem machine)

```bash
golem claude            # exec claude with ANTHROPIC_* set from golem.toml
```

## Option B — environment variables (any machine on the LAN)

Linux / macOS / WSL2:

```bash
export ANTHROPIC_BASE_URL="http://<golem-host>:8000"   # llama-server, Anthropic-native
export ANTHROPIC_AUTH_TOKEN="<LOCAL_AI_API_KEY from golem.toml>"
export ANTHROPIC_MODEL="local-model"                   # = MODEL_ALIAS
export ANTHROPIC_SMALL_FAST_MODEL="local-model"
claude
```

Windows PowerShell:

```powershell
$env:ANTHROPIC_BASE_URL = "http://<golem-host>:8000"
$env:ANTHROPIC_AUTH_TOKEN = "<LOCAL_AI_API_KEY from golem.toml>"
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
