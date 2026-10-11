#!/usr/bin/env bash

set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

PID_FILE="${ROOT_DIR}/.local/llama-server.pid"

if [[ ! -f "${PID_FILE}" ]]; then
    echo "[INFO] No PID file found"
    exit 0
fi

PID="$(cat "${PID_FILE}")"

if kill -0 "${PID}" 2>/dev/null; then
    echo "[INFO] Stopping llama-server (${PID})..."
    # Router mode runs each model in a child llama-server: stop those too.
    pkill -TERM -P "${PID}" 2>/dev/null || true
    kill "${PID}"

    for _ in $(seq 1 20); do
        if ! kill -0 "${PID}" 2>/dev/null; then
            break
        fi

        sleep 1
    done
fi

rm -f "${PID_FILE}"

echo "[ OK ] Local AI stopped"
