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
API_KEY="${LOCAL_AI_API_KEY:-}"

# Health checks always run against localhost, even when the server
# binds to 0.0.0.0 for the LAN.
BASE_URL="http://127.0.0.1:${PORT}"

echo
echo "============================================================"
echo " LOCAL AI HEALTH CHECK"
echo "============================================================"
echo

echo "Endpoint:"
echo "  ${BASE_URL}"
echo

if ! command -v curl >/dev/null 2>&1; then
    echo "[ERROR] curl is required"
    exit 1
fi

if ! curl \
    --silent \
    --show-error \
    --fail \
    --max-time 5 \
    "${BASE_URL}/health" >/dev/null; then

    echo "[FAIL] Local AI server is not responding"
    exit 1
fi

echo "[ OK ] Local AI server is healthy"

echo
echo "Models:"
AUTH=()
[[ -n "${API_KEY}" ]] && AUTH=(-H "Authorization: Bearer ${API_KEY}")
curl \
    --silent \
    --show-error \
    "${AUTH[@]+"${AUTH[@]}"}" \
    "${BASE_URL}/v1/models"

echo
echo
