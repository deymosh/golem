#!/usr/bin/env bash

set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${ROOT_DIR}/.local/bin"
BUILD_DIR="${ROOT_DIR}/.local/src/llama.cpp"

log() {
    printf '[INFO] %s\n' "$*"
}

ok() {
    printf '[ OK ] %s\n' "$*"
}

fail() {
    printf '[ERROR] %s\n' "$*" >&2
    exit 1
}

echo
echo "============================================================"
echo " LOCAL AI - LLAMA.CPP INSTALLATION"
echo "============================================================"
echo

mkdir -p "${BIN_DIR}"
mkdir -p "$(dirname "${BUILD_DIR}")"

if ! command -v git >/dev/null 2>&1; then
    fail "git is required"
fi

if ! command -v cmake >/dev/null 2>&1; then
    fail "cmake is required. Run setup-ai.sh first."
fi

if ! command -v gcc >/dev/null 2>&1; then
    fail "gcc is required. Run setup-ai.sh first."
fi

if ! command -v nvcc >/dev/null 2>&1; then
    log "nvcc is not installed."
    log "This is acceptable if the CUDA toolkit is exposed through the build environment."
fi

if [[ ! -d "${BUILD_DIR}/.git" ]]; then
    log "Cloning llama.cpp..."
    git clone \
        --depth 1 \
        https://github.com/ggml-org/llama.cpp.git \
        "${BUILD_DIR}"
else
    log "Updating existing llama.cpp checkout..."
    git -C "${BUILD_DIR}" pull --ff-only
fi

cd "${BUILD_DIR}"

rm -rf build

log "Configuring CUDA build..."

cmake -S . -B build \
    -DGGML_CUDA=ON \
    -DCMAKE_BUILD_TYPE=Release \
    -DLLAMA_CURL=ON

log "Building llama.cpp..."

cmake --build build \
    --config Release \
    --parallel "$(nproc)"

SERVER_BIN="${BUILD_DIR}/build/bin/llama-server"

if [[ ! -x "${SERVER_BIN}" ]]; then
    fail "llama-server was not built successfully"
fi

CLI_BIN="${BUILD_DIR}/build/bin/llama-cli"

if [[ -x "${CLI_BIN}" ]]; then
    cp "${CLI_BIN}" "${BIN_DIR}/llama-cli"
    chmod +x "${BIN_DIR}/llama-cli"
fi

cp "${SERVER_BIN}" "${BIN_DIR}/llama-server"
chmod +x "${BIN_DIR}/llama-server"

ok "llama-server installed"
ok "Binary: ${BIN_DIR}/llama-server"

echo
"${BIN_DIR}/llama-server" --version || true
echo

log "Checking CUDA support..."

if "${BIN_DIR}/llama-server" --help 2>&1 | grep -qi cuda; then
    ok "CUDA-related support detected in llama-server"
else
    log "Could not verify CUDA support from --help."
    log "The binary was built with GGML_CUDA=ON; runtime validation comes next."
fi

echo
echo "Installation complete."
echo
