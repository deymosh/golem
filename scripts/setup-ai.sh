#!/usr/bin/env bash

# ============================================================
# Local AI - Linux Environment Setup
#
# Purpose:
#   Prepare and validate a Linux machine for local AI inference,
#   CUDA workloads and llama.cpp.
#
# Supported environments:
#   - Native Linux
#   - WSL2 Linux
#   - Linux VMs with GPU passthrough
#   - Ubuntu / Debian based distributions
#
# Target hardware:
#   - NVIDIA GPU
#   - 12GB+ VRAM recommended
#   - 16GB+ system RAM recommended
#
# This script:
#   - Detects the Linux distribution
#   - Detects CPU and RAM
#   - Detects NVIDIA GPUs
#   - Detects CUDA
#   - Installs common build dependencies
#   - Detects Docker
#   - Tests Docker GPU access when available
#   - Creates the local AI directory structure
#
# This script does NOT:
#   - Install NVIDIA drivers
#   - Modify kernel configuration
#   - Install CUDA drivers
#   - Download LLM models
#   - Build llama.cpp
#   - Install Claude Code
#
# Usage:
#
#   chmod +x scripts/setup-ai.sh
#   ./scripts/setup-ai.sh
#
# ============================================================

set -Eeuo pipefail

SCRIPT_NAME="$(basename "$0")"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

LOG_ROOT="${PROJECT_ROOT}/logs"
LOG_FILE="${LOG_ROOT}/setup-$(date +%Y%m%d-%H%M%S).log"

mkdir -p "$LOG_ROOT"

# ------------------------------------------------------------
# Colors
# ------------------------------------------------------------

if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    CYAN=''
    NC=''
fi

# ------------------------------------------------------------
# Logging
# ------------------------------------------------------------

exec > >(tee -a "$LOG_FILE") 2>&1

# ------------------------------------------------------------
# Helpers
# ------------------------------------------------------------

info() {
    echo -e "${BLUE}[INFO]${NC} $*"
}

ok() {
    echo -e "${GREEN}[ OK ]${NC} $*"
}

warn() {
    echo -e "${YELLOW}[WARN]${NC} $*"
}

error() {
    echo -e "${RED}[ERROR]${NC} $*"
}

section() {
    echo
    echo -e "${CYAN}============================================================${NC}"
    echo -e "${CYAN} $*${NC}"
    echo -e "${CYAN}============================================================${NC}"
    echo
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# ------------------------------------------------------------
# Error handler
# ------------------------------------------------------------

trap 'error "Script failed at line $LINENO. Check: $LOG_FILE"' ERR

# ------------------------------------------------------------
# Header
# ------------------------------------------------------------

section "LOCAL AI / LINUX SETUP"

echo "Script       : $SCRIPT_NAME"
echo "Project root : $PROJECT_ROOT"
echo "Log          : $LOG_FILE"
echo "Date         : $(date)"
echo "User         : $(whoami)"
echo

# ------------------------------------------------------------
# Root check
# ------------------------------------------------------------

section "1. PRIVILEGES"

if [[ "${EUID}" -eq 0 ]]; then
    error "Do not run this script as root."
    echo
    echo "Run it as your normal Linux user:"
    echo
    echo "  ./scripts/setup-ai.sh"
    exit 1
fi

ok "Running as user: $(whoami)"

if command_exists sudo; then
    ok "sudo is available"
else
    warn "sudo is not available"
    warn "Package installation may require manual setup"
fi

# ------------------------------------------------------------
# Operating system
# ------------------------------------------------------------

section "2. OPERATING SYSTEM"

if [[ ! -f /etc/os-release ]]; then
    error "Cannot identify Linux distribution."
    exit 1
fi

source /etc/os-release

echo "Distribution : ${PRETTY_NAME:-unknown}"
echo "ID           : ${ID:-unknown}"
echo "Version      : ${VERSION_ID:-unknown}"
echo "Kernel       : $(uname -r)"
echo "Architecture : $(uname -m)"

if [[ "${OSTYPE:-}" == "linux-gnu"* ]]; then
    ok "Linux detected"
else
    warn "Unexpected OSTYPE: ${OSTYPE:-unknown}"
fi

# ------------------------------------------------------------
# CPU
# ------------------------------------------------------------

section "3. CPU"

if command_exists lscpu; then

    CPU_MODEL="$(lscpu | awk -F: '/Model name/ {gsub(/^[ \t]+/, "", $2); print $2; exit}')"
    CPU_CORES="$(nproc)"

    echo "CPU model : ${CPU_MODEL:-unknown}"
    echo "Threads   : ${CPU_CORES:-unknown}"

else

    warn "lscpu is not available."

fi

# ------------------------------------------------------------
# Memory
# ------------------------------------------------------------

section "4. MEMORY"

free -h

TOTAL_MEM_KB="$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
TOTAL_MEM_GB="$((TOTAL_MEM_KB / 1024 / 1024))"

echo
echo "System memory: approximately ${TOTAL_MEM_GB} GB"

if (( TOTAL_MEM_GB >= 32 )); then
    ok "32GB+ RAM detected."
elif (( TOTAL_MEM_GB >= 16 )); then
    ok "16GB+ RAM detected."
else
    warn "Less than 16GB RAM detected."
fi

# ------------------------------------------------------------
# NVIDIA GPU
# ------------------------------------------------------------

section "5. NVIDIA GPU"

NVIDIA_AVAILABLE=false

if command_exists nvidia-smi; then

    NVIDIA_AVAILABLE=true

    ok "nvidia-smi detected"

    echo
    nvidia-smi

    echo
    info "GPU summary:"

    nvidia-smi \
        --query-gpu=name,memory.total,memory.used,memory.free,driver_version \
        --format=csv,noheader \
        || true

else

    warn "nvidia-smi is not available."
    warn "No NVIDIA GPU was detected through the standard NVIDIA interface."

fi

# ------------------------------------------------------------
# GPU devices
# ------------------------------------------------------------

section "6. GPU DEVICES"

if compgen -G "/dev/nvidia*" >/dev/null; then

    ok "NVIDIA device nodes detected"

    ls -lah /dev/nvidia*

else

    info "No /dev/nvidia* device nodes detected."

    if [[ "$NVIDIA_AVAILABLE" == true ]]; then
        info "nvidia-smi still reports a working GPU."
    fi

fi

# ------------------------------------------------------------
# CUDA
# ------------------------------------------------------------

section "7. CUDA"

if command_exists nvcc; then

    ok "CUDA compiler detected"

    nvcc --version

else

    info "nvcc is not installed."

fi

echo

if [[ -n "${CUDA_HOME:-}" ]]; then
    echo "CUDA_HOME: $CUDA_HOME"
fi

if [[ -d /usr/local/cuda ]]; then
    ok "/usr/local/cuda exists"
    ls -ld /usr/local/cuda
fi

echo
info "CUDA-related environment variables:"

env | grep -E '^(CUDA|NVIDIA)_' | sort || true

# ------------------------------------------------------------
# NVIDIA libraries
# ------------------------------------------------------------

section "8. NVIDIA LIBRARIES"

LIBCUDA_FOUND=false

SEARCH_PATHS=(
    "/usr/lib"
    "/usr/lib64"
    "/usr/local/cuda/lib64"
    "/usr/local/cuda/lib"
    "/usr/lib/x86_64-linux-gnu"
    "/usr/lib/wsl/lib"
)

for path in "${SEARCH_PATHS[@]}"; do

    if [[ -d "$path" ]] && compgen -G "$path/libcuda.so*" >/dev/null; then

        LIBCUDA_FOUND=true

        ok "CUDA driver library found in: $path"

        ls -lah "$path"/libcuda.so* 2>/dev/null || true

    fi

done

if [[ "$LIBCUDA_FOUND" == false ]]; then
    warn "libcuda.so was not found in standard locations."
fi

# ------------------------------------------------------------
# Build dependencies
# ------------------------------------------------------------

section "9. BUILD DEPENDENCIES"

PACKAGES=(
    build-essential
    cmake
    git
    pkg-config
    curl
    wget
    ca-certificates
    unzip
    jq
)

if command_exists apt-get; then

    info "Debian/Ubuntu package manager detected."

    sudo apt-get update

    sudo DEBIAN_FRONTEND=noninteractive \
        apt-get install -y "${PACKAGES[@]}"

elif command_exists dnf; then

    info "DNF package manager detected."

    sudo dnf install -y \
        gcc \
        gcc-c++ \
        make \
        cmake \
        git \
        pkgconfig \
        curl \
        wget \
        ca-certificates \
        unzip \
        jq

elif command_exists pacman; then

    info "Pacman package manager detected."

    sudo pacman -Sy --needed --noconfirm \
        base-devel \
        cmake \
        git \
        pkgconf \
        curl \
        wget \
        ca-certificates \
        unzip \
        jq

else

    warn "No supported package manager detected."
    warn "Install the build dependencies manually."

fi

# ------------------------------------------------------------
# Toolchain
# ------------------------------------------------------------

section "10. TOOLCHAIN"

for tool in gcc g++ make cmake git curl wget; do

    if command_exists "$tool"; then
        printf "%-8s " "$tool"

        case "$tool" in
            gcc|g++)
                "$tool" --version | head -n1
                ;;
            make)
                "$tool" --version | head -n1
                ;;
            cmake)
                "$tool" --version | head -n1
                ;;
            git)
                "$tool" --version
                ;;
            curl|wget)
                "$tool" --version | head -n1
                ;;
        esac

    else

        warn "$tool is not installed."

    fi

done

# ------------------------------------------------------------
# Docker
# ------------------------------------------------------------

section "11. DOCKER"

DOCKER_AVAILABLE=false

if command_exists docker; then

    DOCKER_AVAILABLE=true

    ok "Docker detected"

    docker --version || true

    if docker compose version >/dev/null 2>&1; then
        docker compose version
        ok "Docker Compose available"
    else
        warn "Docker Compose is not available."
    fi

    if docker info >/dev/null 2>&1; then
        ok "Docker daemon is accessible"
    else
        warn "Docker daemon is not accessible."
    fi

else

    info "Docker is not installed."

fi

# ------------------------------------------------------------
# Docker GPU test
# ------------------------------------------------------------

section "12. DOCKER -> NVIDIA GPU"

if [[ "$DOCKER_AVAILABLE" == true ]] &&
   docker info >/dev/null 2>&1 &&
   [[ "$NVIDIA_AVAILABLE" == true ]]; then

    CUDA_IMAGE="nvidia/cuda:13.0.0-base-ubuntu24.04"

    info "Testing Docker GPU passthrough..."
    info "Image: $CUDA_IMAGE"

    if ! docker image inspect "$CUDA_IMAGE" >/dev/null 2>&1; then
        info "Pulling CUDA test image..."
        docker pull "$CUDA_IMAGE"
    fi

    if docker run --rm --gpus all "$CUDA_IMAGE" nvidia-smi; then
        ok "Docker -> NVIDIA GPU access works."
    else
        warn "Docker GPU access failed."
    fi

else

    info "Docker GPU test skipped."

fi

# ------------------------------------------------------------
# Storage
# ------------------------------------------------------------

section "13. STORAGE"

df -h "$PROJECT_ROOT"

echo

PROJECT_DISK_AVAILABLE_KB="$(
    df -Pk "$PROJECT_ROOT" | awk 'NR==2 {print $4}'
)"

PROJECT_DISK_AVAILABLE_GB="$(
    echo "$PROJECT_DISK_AVAILABLE_KB / 1024 / 1024" | bc 2>/dev/null || echo "unknown"
)"

if command_exists bc; then
    echo "Available project storage: approximately ${PROJECT_DISK_AVAILABLE_GB} GB"
fi

# ------------------------------------------------------------
# Local AI directories
# ------------------------------------------------------------

section "14. LOCAL AI DIRECTORIES"

AI_ROOT="${PROJECT_ROOT}/data"
MODEL_DIR="${AI_ROOT}/models"
CACHE_DIR="${AI_ROOT}/cache"
CONFIG_DIR="${PROJECT_ROOT}/config"
LOCAL_DIR="${PROJECT_ROOT}/.local"
LOG_DIR="${PROJECT_ROOT}/logs"

mkdir -p \
    "$MODEL_DIR" \
    "$CACHE_DIR" \
    "$CONFIG_DIR" \
    "$LOCAL_DIR" \
    "$LOG_DIR"

ok "Project data directory : $AI_ROOT"
ok "Models directory       : $MODEL_DIR"
ok "Cache directory        : $CACHE_DIR"
ok "Config directory       : $CONFIG_DIR"
ok "Local build/state dir  : $LOCAL_DIR"
ok "Logs directory         : $LOG_DIR"

if [[ ! -f "${PROJECT_ROOT}/.env" && -f "${PROJECT_ROOT}/.env.example" ]]; then
    warn "No .env yet - copy it before running the other scripts:"
    echo "    cp .env.example .env"
fi

# ------------------------------------------------------------
# Git
# ------------------------------------------------------------

section "15. GIT"

if command_exists git; then

    echo "Git version:"
    git --version

    echo

    if git config --global user.name >/dev/null 2>&1; then
        echo "User name : $(git config --global user.name)"
    else
        info "Global git user.name is not configured."
    fi

    if git config --global user.email >/dev/null 2>&1; then
        echo "Email     : $(git config --global user.email)"
    else
        info "Global git user.email is not configured."
    fi

fi

# ------------------------------------------------------------
# Final GPU status
# ------------------------------------------------------------

section "16. GPU BASELINE"

if [[ "$NVIDIA_AVAILABLE" == true ]]; then

    nvidia-smi \
        --query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu \
        --format=csv,noheader \
        || true

else

    warn "No NVIDIA GPU information available."

fi

# ------------------------------------------------------------
# Summary
# ------------------------------------------------------------

section "SETUP SUMMARY"

echo "Operating system:"
echo "  ${PRETTY_NAME:-unknown}"
echo

echo "CPU:"
echo "  ${CPU_MODEL:-unknown}"
echo "  ${CPU_CORES:-unknown} threads"
echo

echo "Memory:"
echo "  ~${TOTAL_MEM_GB} GB"
echo

echo "GPU:"

if [[ "$NVIDIA_AVAILABLE" == true ]]; then

    GPU_NAME="$(
        nvidia-smi \
            --query-gpu=name \
            --format=csv,noheader \
            2>/dev/null | head -n1 || true
    )"

    GPU_VRAM="$(
        nvidia-smi \
            --query-gpu=memory.total \
            --format=csv,noheader \
            2>/dev/null | head -n1 || true
    )"

    GPU_DRIVER="$(
        nvidia-smi \
            --query-gpu=driver_version \
            --format=csv,noheader \
            2>/dev/null | head -n1 || true
    )"

    echo "  Model        : ${GPU_NAME:-unknown}"
    echo "  VRAM         : ${GPU_VRAM:-unknown}"
    echo "  Driver       : ${GPU_DRIVER:-unknown}"

else

    echo "  NVIDIA GPU   : not detected"

fi

echo
echo "CUDA:"
echo "  nvcc         : $(command -v nvcc 2>/dev/null || echo 'not installed')"
echo

echo "Docker:"
if [[ "$DOCKER_AVAILABLE" == true ]]; then
    echo "  Available    : yes"
else
    echo "  Available    : no"
fi

echo
echo "Project:"
echo "  Root         : $PROJECT_ROOT"
echo "  Models       : $MODEL_DIR"
echo "  Cache        : $CACHE_DIR"
echo "  Config       : $CONFIG_DIR"
echo "  Logs         : $LOG_DIR"

echo
echo "Log file:"
echo "  $LOG_FILE"

echo
echo "Next steps:"
echo "  1. cp .env.example .env   (and edit)"
echo "  2. ./scripts/install-llama-cpp.sh"
echo "  3. ./scripts/download-model.sh"
echo "  4. ./scripts/start-local-ai.sh"
echo "  See README.md and docs/ for pointing Claude Code at the stack."

echo
echo -e "${GREEN}============================================================${NC}"
echo -e "${GREEN} LINUX AI ENVIRONMENT CHECK COMPLETE${NC}"
echo -e "${GREEN}============================================================${NC}"
