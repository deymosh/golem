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

# cuda|rocm, auto-detected from the installed toolchain (nvcc/hipcc) when
# unset. GPU_BACKEND in .env overrides; AMDGPU_TARGETS (e.g. gfx1100) pins
# the ROCm target — empty lets the toolkit build for the detected GPU.
GPU_BACKEND="${GPU_BACKEND:-auto}"
case "${GPU_BACKEND}" in
    auto)
        if command -v nvcc >/dev/null 2>&1; then
            GPU_BACKEND="cuda"
        elif command -v hipcc >/dev/null 2>&1; then
            GPU_BACKEND="rocm"
        else
            fail "no CUDA (nvcc) or ROCm (hipcc) toolchain found; install one or set GPU_BACKEND in .env"
        fi
        ;;
    cuda|rocm) ;;
    *) fail "GPU_BACKEND must be auto, cuda or rocm (got: ${GPU_BACKEND})" ;;
esac

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

if [[ "${GPU_BACKEND}" == "cuda" ]] && ! command -v nvcc >/dev/null 2>&1; then
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

log "Configuring ${GPU_BACKEND} build..."

GGML_FLAG="-DGGML_CUDA=ON"
if [[ "${GPU_BACKEND}" == "rocm" ]]; then
    GGML_FLAG="-DGGML_HIP=ON"
    [[ -n "${AMDGPU_TARGETS:-}" ]] && GGML_FLAG="${GGML_FLAG} -DAMDGPU_TARGETS=${AMDGPU_TARGETS}"
fi

cmake -S . -B build \
    ${GGML_FLAG} \
    -DCMAKE_BUILD_TYPE=Release \
    -DLLAMA_CURL=ON

log "Building llama.cpp (${GPU_BACKEND})..."

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

log "Checking ${GPU_BACKEND} support..."

# CUDA builds mention CUDA in --help; HIP builds say HIP, not ROCm.
backend_grep="cuda"
[[ "${GPU_BACKEND}" == "rocm" ]] && backend_grep="hip"
if "${BIN_DIR}/llama-server" --help 2>&1 | grep -qi "${backend_grep}"; then
    ok "${GPU_BACKEND}-related support detected in llama-server"
else
    log "Could not verify ${GPU_BACKEND} support from --help."
    log "The binary was built with the ${GPU_BACKEND} backend; runtime validation comes next."
fi

echo
echo "Installation complete."
echo
