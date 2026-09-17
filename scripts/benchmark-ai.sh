#!/usr/bin/env bash

set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

HOST="${LOCAL_AI_HOST:-127.0.0.1}"
PORT="${LOCAL_AI_PORT:-8000}"

MODEL_ALIAS="${MODEL_ALIAS:-local-model}"
TEMPERATURE="${TEMPERATURE:-0.15}"
TOP_P="${TOP_P:-0.95}"

URL="http://${HOST}:${PORT}/v1/chat/completions"

# JSON-escape the prompt: prefer python3, fall back to jq.
if command -v python3 >/dev/null 2>&1; then
    json_string() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
elif command -v jq >/dev/null 2>&1; then
    json_string() { jq -Rs .; }
else
    echo "[ERROR] need python3 or jq to build the request" >&2
    exit 1
fi

PROMPT='You are an expert software engineer.

Analyze this task carefully.

We have a TypeScript application with a function:

function parseConfig(input: unknown): Config {
    if (!input || typeof input !== "object") {
        throw new Error("Invalid config");
    }

    const value = input as Record<string, unknown>;

    return {
        port: Number(value.port ?? 3000),
        debug: Boolean(value.debug ?? false),
    };
}

Identify the important correctness problems in this implementation.
Explain how you would improve it for production.
Do not modify the code. Reason step by step and be precise.'

echo
echo "============================================================"
echo " LOCAL AI BENCHMARK"
echo "============================================================"
echo

echo "Endpoint:"
echo "  ${URL}"
echo

curl \
    --silent \
    --show-error \
    --fail \
    "${URL}" \
    -H "Content-Type: application/json" \
    -d @- <<EOF
{
  "model": "${MODEL_ALIAS}",
  "messages": [
    {
      "role": "user",
      "content": $(printf '%s' "${PROMPT}" | json_string)
    }
  ],
  "temperature": ${TEMPERATURE},
  "top_p": ${TOP_P},
  "max_tokens": 1500,
  "stream": false
}
EOF

echo
echo
