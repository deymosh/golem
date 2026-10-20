#!/usr/bin/env bash

set -Eeuo pipefail
export LC_ALL=C

# ============================================================
# RUST DEV TOOLCHAIN (local, git-ignored, no admin)
# ============================================================
#
# Gives this checkout a working `cargo test` / `cargo build` for golem-rs
# without installing anything system-wide:
#
#   Windows - a standalone mingw-w64 (GCC) toolchain in .local/toolchain/,
#             used through the x86_64-pc-windows-gnu target. No Visual
#             Studio Build Tools (~2 GB, admin) and no 7-Zip: the archive
#             is a plain .zip, extracted with what Windows already has.
#   Linux / macOS - the system compiler is already there; only the Rust
#             side is checked.
#
# What it writes:
#   .local/toolchain/            the extracted toolchain (git-ignored)
#   golem-rs/.cargo/config.toml  linker + target for this machine, absolute
#                                paths, git-ignored - never committed
#
# The CI workflow does not read any of it: CI builds the MSVC target, which
# is what a released Windows binary should be. This is for local testing.
#
# Usage: ./scripts/setup-rust-toolchain.sh [--force]
#   --force  reinstall even when the toolchain is already there
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN_DIR="${ROOT_DIR}/.local/toolchain"
DOWNLOAD_DIR="${ROOT_DIR}/data/tmp"
# At the repository root, not inside golem-rs: cargo reads .cargo/config.toml
# from the working directory upwards, so this way `cargo test` works from the
# root and from golem-rs alike.
CARGO_CONFIG_DIR="${ROOT_DIR}/.cargo"

# Pinned, like the engine build (LLAMA_WIN_BUILD) and the docker digest: a
# toolchain that changes underfoot makes a test failure mean nothing. Bump
# deliberately, with the new sha256 from the release's own .sha256 asset.
WINLIBS_TAG="${GOLEM_MINGW_TAG:-16.2.0posix-23.1.3-14.0.0-ucrt-r3}"
WINLIBS_ASSET="${GOLEM_MINGW_ASSET:-winlibs-x86_64-posix-seh-gcc-16.2.0-mingw-w64ucrt-14.0.0-r3.zip}"
WINLIBS_SHA256="${GOLEM_MINGW_SHA256:-3c69305081e4a647144339a936244fcf7d3d9e558319e1ad7be2cd2886d4b6a1}"
WINLIBS_URL="https://github.com/brechtsanders/winlibs_mingw/releases/download/${WINLIBS_TAG}/${WINLIBS_ASSET}"
TARGET="x86_64-pc-windows-gnu"

FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

say() { echo "[ .. ] $*"; }
ok()  { echo "[ OK ] $*"; }
die() { echo "[FAIL] $*" >&2; exit 1; }

# ------------------------------------------------------------
# Rust itself
# ------------------------------------------------------------

command -v cargo >/dev/null 2>&1 || die "cargo not found - install Rust from https://rustup.rs first"
command -v rustup >/dev/null 2>&1 || die "rustup not found - the target management below needs it"
ok "cargo $(cargo --version | awk '{print $2}')"

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) PLATFORM=windows ;;
    Darwin)               PLATFORM=macos ;;
    *)                    PLATFORM=linux ;;
esac

# ------------------------------------------------------------
# Linux / macOS: the system compiler is the toolchain
# ------------------------------------------------------------

if [[ "${PLATFORM}" != windows ]]; then
    if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
        ok "system compiler present ($( (command -v cc || command -v gcc || command -v clang) ))"
    else
        die "no C compiler found - install build-essential (Debian/Ubuntu) or xcode-select --install (macOS)"
    fi
    say "nothing else to install: cargo test builds for this host"
    echo
    ok "run: cd golem-rs && cargo test"
    exit 0
fi

# ------------------------------------------------------------
# Windows: mingw-w64 into .local/toolchain
# ------------------------------------------------------------

GCC=""
if [[ "${FORCE}" != 1 ]]; then
    # Already installed? Find the compiler wherever the archive put it.
    GCC="$(find "${TOOLCHAIN_DIR}" -name 'x86_64-w64-mingw32-gcc.exe' -print -quit 2>/dev/null || true)"
fi

if [[ -n "${GCC}" ]]; then
    ok "mingw-w64 already installed: ${GCC#"${ROOT_DIR}/"}"
else
    mkdir -p "${TOOLCHAIN_DIR}" "${DOWNLOAD_DIR}"
    ZIP="${DOWNLOAD_DIR}/${WINLIBS_ASSET}"

    if [[ ! -f "${ZIP}" ]]; then
        say "downloading ${WINLIBS_ASSET} (~264 MiB, standalone mingw-w64; no installer, no admin)"
        curl -L --noproxy '*' --fail --max-time 1800 --progress-bar \
            -o "${ZIP}.part" "${WINLIBS_URL}" || die "download failed: ${WINLIBS_URL}"
        mv "${ZIP}.part" "${ZIP}"
    else
        say "using the archive already in data/tmp"
    fi

    say "verifying sha256"
    echo "${WINLIBS_SHA256}  ${ZIP}" | sha256sum -c - >/dev/null 2>&1 \
        || die "checksum mismatch for ${ZIP} - delete it and re-run"

    say "extracting into .local/toolchain"
    if command -v unzip >/dev/null 2>&1; then
        unzip -q -o "${ZIP}" -d "${TOOLCHAIN_DIR}"
    else
        # Windows' own PowerShell handles plain .zip; nothing else to install.
        powershell.exe -NoProfile -Command \
            "Expand-Archive -LiteralPath '$(cygpath -w "${ZIP}")' -DestinationPath '$(cygpath -w "${TOOLCHAIN_DIR}")' -Force" \
            || die "extraction failed"
    fi
    rm -f "${ZIP}"

    GCC="$(find "${TOOLCHAIN_DIR}" -name 'x86_64-w64-mingw32-gcc.exe' -print -quit 2>/dev/null || true)"
    [[ -n "${GCC}" ]] || die "no x86_64-w64-mingw32-gcc.exe under ${TOOLCHAIN_DIR#"${ROOT_DIR}/"} - unexpected archive layout"
    ok "mingw-w64 installed: ${GCC#"${ROOT_DIR}/"}"
fi

# ------------------------------------------------------------
# Rust toolchain: the GNU host, not the MSVC one
# ------------------------------------------------------------
#
# rustup's default Windows toolchain is MSVC-hosted, which needs the MSVC
# linker for two different things: the final link, and - the one that bites
# later - *build scripts*, which always compile for the host. Setting
# build.target to the GNU target fixes only the first: add a dependency with
# a build script (tokio, axum and their trees have several) and the host
# link fails even though the target is GNU.
#
# A GNU *host* toolchain removes both problems: build scripts and the final
# binary all link with the mingw-w64 installed above.
GNUPKG="stable-${TARGET}"
if rustup toolchain list | grep -q "^${GNUPKG}"; then
    ok "rustup toolchain ${GNUPKG} already installed"
else
    say "installing the ${GNUPKG} rustup toolchain (~200 MiB)"
    rustup toolchain install "${GNUPKG}" --profile minimal \
        || die "could not install ${GNUPKG}"
    ok "rustup toolchain ${GNUPKG} installed"
fi

# A per-directory override, so plain `cargo test` in this checkout uses it.
# rustup keeps this outside the repository, so CI - which has MSVC and builds
# it - is unaffected.
rustup override set "${GNUPKG}" --path "${ROOT_DIR}" >/dev/null 2>&1 \
    || say "could not set the rustup override; use: cargo +${GNUPKG} test"
ok "this checkout uses ${GNUPKG} for cargo (rustup override)"

# ------------------------------------------------------------
# cargo config
# ------------------------------------------------------------

mkdir -p "${CARGO_CONFIG_DIR}"
GCC_WIN="$(cygpath -w "${GCC}" 2>/dev/null || echo "${GCC}")"
MINGW_BIN="$(dirname "${GCC}")"
cat > "${CARGO_CONFIG_DIR}/config.toml" <<EOF
# Written by scripts/setup-rust-toolchain.sh - NOT committed (git-ignored):
# it holds this machine's absolute toolchain path.
#
# Local Rust builds use the GNU target with the mingw-w64 in
# .local/toolchain, so \`cargo test\` and \`cargo build\` work on a box
# without Visual Studio. CI ignores all of this and builds MSVC, which is
# what a released Windows binary should be.
#
# Build with ./scripts/cargo.sh, not plain cargo: the dependency tree runs
# dlltool (from this toolchain's bin) by name, and cargo's [env] section
# cannot put PATH in front of a build script. The wrapper does.
[build]
target = "${TARGET}"

[target.${TARGET}]
linker = '${GCC_WIN}'
# Static CRT: the test and dev binaries run without any mingw DLLs next to
# them, the same property the MSVC build has.
rustflags = ["-C", "target-feature=+crt-static"]
EOF
ok "wrote .cargo/config.toml at the repository root (git-ignored)"

echo
ok "run: cd golem-rs && cargo test"
echo "  (a rustup override pins this checkout to the GNU toolchain, so no"
echo "   --target flag is needed; undo with: rustup override unset --path .)"
echo "  CI still builds MSVC; this is the local test toolchain."
