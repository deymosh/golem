#!/usr/bin/env bash

set -Eeuo pipefail

# ============================================================
# LOCAL AI MODEL DOWNLOAD
# ============================================================
# Fetches GGUFs straight from Hugging Face's resolve endpoint with
# curl — no python3, no venv, no hf CLI — so the same script runs on
# Linux, macOS and Git Bash on Windows. data/models is a plain host
# directory shared by every deploy mode (docker bind-mounts it), so a
# download from any shell serves them all.
#
# Usage:
#   download-model.sh                 # HF_REPO/MODEL_FILE (+ DRAFT_FILE) from .env
#   download-model.sh <repo> <file>   # one specific file (router auto-pull)

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

if [[ -f "${PROJECT_ROOT}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${PROJECT_ROOT}/.env"
    set +a
fi

if [[ $# -ge 2 ]]; then
    # Explicit file: router mode's auto-pull walks the preset's
    # "; Download: repo -> file" comments and calls this per file.
    HF_REPO="$1"
    MODEL_FILE="$2"
    DRAFT_FILE=""
elif [[ $# -eq 1 ]]; then
    echo "[ERROR] usage: download-model.sh [repo file]" >&2
    exit 1
fi

HF_REPO="${HF_REPO:-unsloth/Qwen3.6-35B-A3B-GGUF}"
MODEL_FILE="${MODEL_FILE:-Qwen3.6-35B-A3B-UD-Q3_K_M.gguf}"

MODELS_DIR="${PROJECT_ROOT}/data/models"
LOG_DIR="${PROJECT_ROOT}/logs"

TIMESTAMP="$(date '+%Y%m%d-%H%M%S')"
LOG_FILE="${LOG_DIR}/download-model-${TIMESTAMP}.log"

mkdir -p "${MODELS_DIR}" "${LOG_DIR}"

# ============================================================
# LOGGING
# ============================================================

exec > >(tee -a "${LOG_FILE}") 2>&1

info()    { echo "[INFO] $*"; }
ok()      { echo "[ OK ] $*"; }
warn()    { echo "[WARN] $*"; }
error()   { echo "[ERROR] $*" >&2; }
section() { echo; echo "============================================================"; echo " $*"; echo "============================================================"; }

trap 'error "Model download failed. Log: ${LOG_FILE}"' ERR

# ============================================================
# REQUIREMENTS + STORAGE
# ============================================================

section "LOCAL AI MODEL DOWNLOAD"

command -v curl >/dev/null 2>&1 || { error "curl is required (present on every supported platform)"; exit 1; }
ok "curl: $(curl --version | head -n 1)"

# Keep a generous safety margin: the biggest preset model plus a .part file.
AVAILABLE_KB="$(df -Pk "${PROJECT_ROOT}" | awk 'NR==2 {print $4}')"
AVAILABLE_GB="$((AVAILABLE_KB / 1024 / 1024))"
MIN_FREE_GB=25
if (( AVAILABLE_GB < MIN_FREE_GB )); then
    error "Insufficient disk space: ${AVAILABLE_GB} GB free, ${MIN_FREE_GB} GB recommended."
    exit 1
fi
ok "Available disk space: ${AVAILABLE_GB} GB"

# ============================================================
# DOWNLOAD
# ============================================================

# hf_get <repo> <file>: fetch one GGUF into MODELS_DIR, resuming a
# leftover .part from an interrupted run. Any real GGUF is at least
# ~1 GB; a smaller file means a truncated download, not a model.
hf_get() {
    local repo="$1" file="$2"
    local path="${MODELS_DIR}/${file}"

    if [[ -f "${path}" ]]; then
        local size
        size="$(stat -c '%s' "${path}" 2>/dev/null || stat -f '%z' "${path}")"
        if (( size > 1000000000 )); then
            ok "${file} already present."
            return 0
        fi
        warn "${file} is smaller than 1 GB — partial download, retrying."
        rm -f "${path}"
    fi

    info "Downloading ${file} from ${repo}..."

    local auth=()
    [[ -n "${HF_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer ${HF_TOKEN}")

    # -L follows the CDN redirect; -C - resumes the .part if one is
    # lying around; --retry rides out flaky links.
    curl -fL -C - --retry 5 --retry-delay 3 "${auth[@]+"${auth[@]}"}" \
        -o "${path}.part" \
        "https://huggingface.co/${repo}/resolve/main/${file}"

    local size
    size="$(stat -c '%s' "${path}.part" 2>/dev/null || stat -f '%z' "${path}.part")"
    if (( size < 1000000000 )); then
        error "${file} downloaded but smaller than 1 GB — not a real model. Delete data/models/${file}.part and retry."
        exit 1
    fi

    mv -f "${path}.part" "${path}"
    ok "${file} downloaded ($(( size / 1024 / 1024 / 1024 )) GB)."

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${path}" | tee "${path}.sha256"
    fi
}

if [[ -n "${DRAFT_FILE:-}" ]]; then
    hf_get "${DRAFT_HF_REPO:-${HF_REPO}}" "${DRAFT_FILE}"
fi
hf_get "${HF_REPO}" "${MODEL_FILE}"

# ============================================================
# SUMMARY
# ============================================================

section "DOWNLOAD COMPLETE"

echo "Models dir: ${MODELS_DIR}"
echo "Log       : ${LOG_FILE}"
echo
ok "Model download completed successfully."
