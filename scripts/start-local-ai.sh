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

MODEL_DIR="${ROOT_DIR}/data/models"
MODEL_FILE="${MODEL_FILE:-Qwen3.6-35B-A3B-UD-Q3_K_M.gguf}"
MODEL_ALIAS="${MODEL_ALIAS:-local-model}"

CONTEXT_SIZE="${CONTEXT_SIZE:-32768}"
CPU_THREADS="${CPU_THREADS:-20}"
GPU_LAYERS="${GPU_LAYERS:-auto}"

SERVER="${ROOT_DIR}/.local/bin/llama-server"
MODEL="${MODEL_DIR}/${MODEL_FILE}"

LOG_DIR="${ROOT_DIR}/logs"
LOG_FILE="${LOG_DIR}/local-ai.log"

mkdir -p "${LOG_DIR}"

if [[ ! -x "${SERVER}" ]]; then
    echo "[ERROR] llama-server is not installed"
    echo "Run: ./scripts/install-llama-cpp.sh"
    exit 1
fi

if [[ -n "${MODELS_PRESET:-}" ]]; then
    if [[ ! -f "${ROOT_DIR}/${MODELS_PRESET}" ]]; then
        echo "[ERROR] Preset not found: ${ROOT_DIR}/${MODELS_PRESET} (MODELS_PRESET in .env)"
        exit 1
    fi
elif [[ ! -f "${MODEL}" ]]; then
    echo "[ERROR] Model not found:"
    echo "  ${MODEL}"
    echo
    echo "Run: ./scripts/download-model.sh"
    exit 1
fi

if curl --silent --fail --max-time 2 "http://${HOST}:${PORT}/health" >/dev/null 2>&1; then
    echo "[ OK ] Local AI server is already running"
    exit 0
fi

echo
echo "============================================================"
echo " STARTING LOCAL AI"
echo "============================================================"
echo

echo "Model:"
if [[ -n "${MODELS_PRESET:-}" ]]; then
    echo "  router, presets from ${MODELS_PRESET} (default: ${MODEL_ALIAS})"
else
    echo "  ${MODEL_FILE}"
    echo "  (served as: ${MODEL_ALIAS})"
fi

echo
echo "Endpoint:"
echo "  http://${HOST}:${PORT}"

echo
echo "Context:"
echo "  ${CONTEXT_SIZE}"

echo
echo "Log:"
echo "  ${LOG_FILE}"

echo

mkdir -p "${ROOT_DIR}/.local"

# Keep these flags in sync with the docker entrypoint in compose.yml:
# both must honor PARALLEL, LOAD_MODE, CACHE_TYPE_K/V, SPEC_TYPE,
# DRAFT_FILE and EXTRA_ARGS so a model configured in .env behaves
# identically in either mode.
MODEL_ARGS=()
if [[ -n "${MODELS_PRESET:-}" ]]; then
    # Router: command-line flags override every preset, so only the shared
    # ones go on the command line; one model resident at a time.
    MODEL_ARGS+=(--models-preset "${ROOT_DIR}/${MODELS_PRESET}" --models-max 1)
else
    MODEL_ARGS+=(
        --model "${MODEL}"
        --alias "${MODEL_ALIAS}"
        --ctx-size "${CONTEXT_SIZE}"
        --n-gpu-layers "${GPU_LAYERS}"
        --jinja
        -fa on
        --load-mode "${LOAD_MODE:-auto}"
        --cache-type-k "${CACHE_TYPE_K:-q8_0}"
        --cache-type-v "${CACHE_TYPE_V:-q8_0}"
    )
    [[ -n "${SPEC_TYPE:-}" ]] && MODEL_ARGS+=(--spec-type "${SPEC_TYPE}")
    [[ -n "${DRAFT_FILE:-}" ]] && MODEL_ARGS+=(--spec-draft-model "${MODEL_DIR}/${DRAFT_FILE}")
    # shellcheck disable=SC2206
    MODEL_ARGS+=(${EXTRA_ARGS:-})
fi
[[ -n "${SLEEP_IDLE_SECONDS:-}" ]] && MODEL_ARGS+=(--sleep-idle-seconds "${SLEEP_IDLE_SECONDS}")

# Started from data/models so preset model paths resolve as bare filenames.
cd "${MODEL_DIR}"
nohup "${SERVER}" \
    --host "${HOST}" \
    --port "${PORT}" \
    --threads "${CPU_THREADS}" \
    --parallel "${PARALLEL:-1}" \
    --metrics \
    --api-key "${LOCAL_AI_API_KEY:-local-key}" \
    "${MODEL_ARGS[@]}" \
    > "${LOG_FILE}" 2>&1 &

PID=$!

echo "${PID}" > "${ROOT_DIR}/.local/llama-server.pid"

echo "PID: ${PID}"

echo
echo "Waiting for server..."

for _ in $(seq 1 60); do
    if curl --silent --fail --max-time 2 \
        "http://${HOST}:${PORT}/health" >/dev/null 2>&1; then

        echo
        echo "[ OK ] Local AI server is ready"
        echo
        exit 0
    fi

    sleep 2
done

echo
echo "[ERROR] Server did not become ready"
echo
echo "Last log output:"
tail -100 "${LOG_FILE}" || true

exit 1
