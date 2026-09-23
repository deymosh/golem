#!/usr/bin/env bash

set -Eeuo pipefail

# ============================================================
# LOCAL AI MODEL DOWNLOAD
# ============================================================

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

if [[ -f "${PROJECT_ROOT}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${PROJECT_ROOT}/.env"
    set +a
fi

MODELS_DIR="${PROJECT_ROOT}/data/models"
CACHE_DIR="${PROJECT_ROOT}/data/cache"
LOG_DIR="${PROJECT_ROOT}/logs"

HF_REPO="${HF_REPO:-unsloth/Qwen3.6-35B-A3B-GGUF}"
MODEL_FILE="${MODEL_FILE:-Qwen3.6-35B-A3B-UD-Q3_K_M.gguf}"

HF_VENV="${CACHE_DIR}/huggingface-venv"
MODEL_PATH="${MODELS_DIR}/${MODEL_FILE}"

TIMESTAMP="$(date '+%Y%m%d-%H%M%S')"
LOG_FILE="${LOG_DIR}/download-model-${TIMESTAMP}.log"

mkdir -p "${MODELS_DIR}" "${CACHE_DIR}" "${LOG_DIR}"

# ============================================================
# LOGGING
# ============================================================

exec > >(tee -a "${LOG_FILE}") 2>&1

# ============================================================
# HELPERS
# ============================================================

info() {
    echo "[INFO] $*"
}

ok() {
    echo "[ OK ] $*"
}

warn() {
    echo "[WARN] $*"
}

error() {
    echo "[ERROR] $*" >&2
}

section() {
    echo
    echo "============================================================"
    echo " $*"
    echo "============================================================"
}

cleanup_on_error() {
    error "Model download failed."
    error "Log file: ${LOG_FILE}"
}

trap cleanup_on_error ERR

# ============================================================
# HEADER
# ============================================================

section "LOCAL AI MODEL DOWNLOAD"

echo
echo "Project root : ${PROJECT_ROOT}"
echo "Models dir   : ${MODELS_DIR}"
echo "Repository   : ${HF_REPO}"
echo "Model        : ${MODEL_FILE}"
echo "Log          : ${LOG_FILE}"
echo

# ============================================================
# 1. PLATFORM
# ============================================================

section "1. PLATFORM"

if [[ "$(uname -s)" != "Linux" ]]; then
    error "This script requires a Linux environment (native Linux or WSL2 on Windows)."
    exit 1
fi

ARCH="$(uname -m)"

case "${ARCH}" in
    x86_64|amd64)
        ok "Linux architecture: x86_64"
        ;;
    aarch64|arm64)
        ok "Linux architecture: ARM64"
        ;;
    *)
        warn "Unsupported architecture: ${ARCH}"
        ;;
esac

# ============================================================
# 2. REQUIREMENTS
# ============================================================

section "2. REQUIREMENTS"

if ! command -v python3 >/dev/null 2>&1; then
    error "python3 is not installed."
    exit 1
fi

PYTHON_VERSION="$(python3 --version)"
ok "Python: ${PYTHON_VERSION}"

if ! command -v curl >/dev/null 2>&1; then
    error "curl is not installed."

    if command -v apt-get >/dev/null 2>&1; then
        info "Installing curl..."
        sudo apt-get update
        sudo apt-get install -y curl
    else
        error "Please install curl manually."
        exit 1
    fi
fi

ok "curl: $(curl --version | head -n 1)"

# ============================================================
# 3. STORAGE
# ============================================================

section "3. STORAGE"

AVAILABLE_KB="$(df -Pk "${PROJECT_ROOT}" | awk 'NR==2 {print $4}')"
AVAILABLE_GB="$((AVAILABLE_KB / 1024 / 1024))"

info "Available disk space: approximately ${AVAILABLE_GB} GB"

# Keep a generous safety margin for the model plus temp files.
MIN_FREE_GB=25

if (( AVAILABLE_GB < MIN_FREE_GB )); then
    error "Insufficient disk space."
    error "At least ${MIN_FREE_GB} GB of free space is recommended."
    exit 1
fi

ok "Sufficient disk space."

# ============================================================
# 4. PYTHON VIRTUAL ENVIRONMENT SUPPORT
# ============================================================

section "4. PYTHON VIRTUAL ENVIRONMENT"

VENV_TEST_DIR="${CACHE_DIR}/venv-test"

rm -rf "${VENV_TEST_DIR}"

if python3 -m venv "${VENV_TEST_DIR}" >/dev/null 2>&1; then
    rm -rf "${VENV_TEST_DIR}"
    ok "Python venv support is available."
else
    rm -rf "${VENV_TEST_DIR}"

    warn "Python venv support is missing."

    # --------------------------------------------------------
    # Ubuntu / Debian
    # --------------------------------------------------------

    if command -v apt-get >/dev/null 2>&1; then
        info "Debian/Ubuntu package manager detected."
        info "Installing Python venv support..."

        sudo apt-get update

        PYTHON_MINOR="$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')"

        if sudo apt-get install -y "python${PYTHON_MINOR}-venv"; then
            ok "Installed python${PYTHON_MINOR}-venv."
        else
            warn "python${PYTHON_MINOR}-venv package was not available."
            info "Trying generic python3-venv package..."

            sudo apt-get install -y python3-venv

            ok "Installed python3-venv."
        fi

    # --------------------------------------------------------
    # Fedora / RHEL
    # --------------------------------------------------------

    elif command -v dnf >/dev/null 2>&1; then
        info "DNF package manager detected."
        info "Installing Python venv support..."

        sudo dnf install -y python3-virtualenv

        ok "Installed Python virtual environment support."

    # --------------------------------------------------------
    # Arch
    # --------------------------------------------------------

    elif command -v pacman >/dev/null 2>&1; then
        info "Pacman package manager detected."

        sudo pacman -Sy --needed --noconfirm python

        ok "Python environment support installed."

    else
        error "Could not determine the Linux package manager."
        error "Please install Python venv support manually."
        exit 1
    fi

    # --------------------------------------------------------
    # Verify again
    # --------------------------------------------------------

    if ! python3 -m venv "${VENV_TEST_DIR}" >/dev/null 2>&1; then
        error "Python venv support is still unavailable."
        error "Try installing the appropriate python3-venv package."
        exit 1
    fi

    rm -rf "${VENV_TEST_DIR}"

    ok "Python venv support is now available."
fi

# ============================================================
# 5. HUGGING FACE CLI
# ============================================================

section "5. HUGGING FACE CLI"

HF_BIN=""

# ------------------------------------------------------------
# Prefer an already-installed hf command
# ------------------------------------------------------------

if command -v hf >/dev/null 2>&1; then
    HF_BIN="$(command -v hf)"
    ok "Hugging Face CLI found: ${HF_BIN}"

else
    info "Hugging Face CLI not found."
    info "Creating isolated Hugging Face environment..."

    mkdir -p "${CACHE_DIR}"

    # --------------------------------------------------------
    # Create virtual environment
    # --------------------------------------------------------

    if [[ -d "${HF_VENV}" ]]; then
        info "Existing Hugging Face virtual environment found."
    else
        info "Creating virtual environment: ${HF_VENV}"

        python3 -m venv "${HF_VENV}"

        ok "Virtual environment created."
    fi

    HF_PYTHON="${HF_VENV}/bin/python"

    if [[ ! -x "${HF_PYTHON}" ]]; then
        error "Hugging Face virtual environment is invalid."
        exit 1
    fi

    # --------------------------------------------------------
    # Bootstrap pip
    # --------------------------------------------------------

    info "Checking pip..."

    if ! "${HF_PYTHON}" -m pip --version >/dev/null 2>&1; then
        warn "pip is missing from the virtual environment."
        info "Bootstrapping pip..."

        "${HF_PYTHON}" -m ensurepip --upgrade

        ok "pip bootstrapped."
    fi

    # --------------------------------------------------------
    # Install huggingface_hub
    # --------------------------------------------------------

    info "Installing/updating huggingface_hub..."

    "${HF_PYTHON}" -m pip install \
        --upgrade \
        pip \
        huggingface_hub

    ok "huggingface_hub installed."

    # --------------------------------------------------------
    # Locate hf
    # --------------------------------------------------------

    if [[ -x "${HF_VENV}/bin/hf" ]]; then
        HF_BIN="${HF_VENV}/bin/hf"
    elif "${HF_PYTHON}" -m huggingface_hub.commands.huggingface_cli --help >/dev/null 2>&1; then
        HF_BIN="${HF_PYTHON} -m huggingface_hub.commands.huggingface_cli"
    else
        error "Could not locate Hugging Face CLI."
        exit 1
    fi

    ok "Using Hugging Face CLI: ${HF_BIN}"
fi

# ============================================================
# 6. HUGGING FACE AUTHENTICATION
# ============================================================

section "6. HUGGING FACE AUTHENTICATION"

if [[ -n "${HF_TOKEN:-}" ]]; then
    ok "HF_TOKEN is available in the environment."
else
    info "No HF_TOKEN detected."
    info "Attempting download without authentication."
fi

# hf_download <repo> <file>: fetch one file into MODELS_DIR
hf_download() {
    local token_args=()
    [[ -n "${HF_TOKEN:-}" ]] && token_args=(--token "${HF_TOKEN}")

    if [[ "${HF_BIN}" == *" "* ]]; then
        # Command stored as a string, e.g. python -m ...
        eval "${HF_BIN} download \"\$1\" \"\$2\" --local-dir \"\${MODELS_DIR}\" \"\${token_args[@]}\""
    else
        "${HF_BIN}" download "$1" "$2" --local-dir "${MODELS_DIR}" "${token_args[@]}"
    fi
}

# ============================================================
# 6b. DRAFT MODEL (speculative decoding, optional)
# ============================================================

if [[ -n "${DRAFT_FILE:-}" ]]; then
    section "6b. DRAFT MODEL"

    if [[ -f "${MODELS_DIR}/${DRAFT_FILE}" ]]; then
        ok "Draft model present: ${DRAFT_FILE}"
    else
        info "Downloading draft ${DRAFT_FILE} from ${DRAFT_HF_REPO:-${HF_REPO}}..."
        hf_download "${DRAFT_HF_REPO:-${HF_REPO}}" "${DRAFT_FILE}"
        ok "Draft model downloaded."
    fi
fi

# ============================================================
# 7. MODEL STATUS
# ============================================================

section "7. MODEL STATUS"

if [[ -f "${MODEL_PATH}" ]]; then
    MODEL_SIZE_BYTES="$(stat -c '%s' "${MODEL_PATH}")"
    MODEL_SIZE_GB="$(awk "BEGIN {printf \"%.2f\", ${MODEL_SIZE_BYTES}/1024/1024/1024}")"

    info "Existing model found:"
    info "Path : ${MODEL_PATH}"
    info "Size : ${MODEL_SIZE_GB} GB"

    # Any real GGUF is at least ~1 GB; a smaller file here means a
    # truncated or partial download.
    if (( MODEL_SIZE_BYTES > 1000000000 )); then
        ok "Existing model appears to be complete."
        echo
        echo "Model already downloaded."
        echo "Nothing else to do."
        echo
        echo "Model : ${MODEL_PATH}"
        echo "Log   : ${LOG_FILE}"
        exit 0
    else
        warn "Existing model appears incomplete."
        warn "Removing partial file..."

        rm -f "${MODEL_PATH}"
    fi
else
    info "Model is not present."
fi

# ============================================================
# 8. DOWNLOAD MODEL
# ============================================================

section "8. DOWNLOAD MODEL"

info "Repository : ${HF_REPO}"
info "Filename   : ${MODEL_FILE}"
info "Destination: ${MODELS_DIR}"

mkdir -p "${MODELS_DIR}"

TEMP_MODEL="${MODEL_PATH}.part"

rm -f "${TEMP_MODEL}"

info "Downloading model..."
echo

hf_download "${HF_REPO}" "${MODEL_FILE}"

# ============================================================
# 9. VERIFY DOWNLOAD
# ============================================================

section "9. VERIFY DOWNLOAD"

if [[ ! -f "${MODEL_PATH}" ]]; then
    error "Expected model file was not found."
    error "Expected: ${MODEL_PATH}"
    exit 1
fi

MODEL_SIZE_BYTES="$(stat -c '%s' "${MODEL_PATH}")"
MODEL_SIZE_GB="$(awk "BEGIN {printf \"%.2f\", ${MODEL_SIZE_BYTES}/1024/1024/1024}")"

info "Downloaded model size: ${MODEL_SIZE_GB} GB"

if (( MODEL_SIZE_BYTES < 1000000000 )); then
    error "Downloaded file is suspiciously small."
    error "Expected a multi-GB GGUF model."
    exit 1
fi

ok "Model file exists and has a plausible size."

# ============================================================
# 10. CHECKSUM
# ============================================================

section "10. CHECKSUM"

SHA256_FILE="${MODEL_PATH}.sha256"

info "Calculating SHA256..."
sha256sum "${MODEL_PATH}" | tee "${SHA256_FILE}"

ok "SHA256 checksum saved."

# ============================================================
# 11. FINAL SUMMARY
# ============================================================

section "DOWNLOAD COMPLETE"

echo
echo "Model:"
echo "  ${MODEL_PATH}"
echo
echo "Repository:"
echo "  ${HF_REPO}"
echo
echo "Size:"
echo "  ${MODEL_SIZE_GB} GB"
echo
echo "SHA256:"
cat "${SHA256_FILE}"
echo
echo "Checksum:"
echo "  ${SHA256_FILE}"
echo
echo "Cache:"
echo "  ${CACHE_DIR}"
echo
echo "Log:"
echo "  ${LOG_FILE}"
echo

ok "Model download completed successfully."

echo
echo "Next step:"
echo "  Configure your local inference server/runtime."
echo
