#!/usr/bin/env bash

set -Eeuo pipefail

# ============================================================
# CARGO, WITH THE LOCAL TOOLCHAIN
# ============================================================
#
# Runs cargo with the mingw-w64 that scripts/setup-rust-toolchain.sh
# installed first on PATH. The `linker` setting in .cargo/config.toml is
# enough to build a GNU binary, but not enough for everything the
# dependency tree does: the windows-sys chain runs `dlltool` to generate
# import libraries, and it looks the tool up by name on PATH. cargo's own
# `[env]` section cannot help there - it does not override PATH for build
# scripts - so PATH is set here, in one place.
#
# Usage: ./scripts/cargo.sh test
#        ./scripts/cargo.sh build --release
#        ./scripts/cargo.sh run -- report --md
#
# Plain `cargo test` works too, as long as the toolchain's bin is on your
# PATH. This is the version that always does.
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MINGW_BIN="${ROOT_DIR}/.local/toolchain/mingw64/bin"

if [[ ! -d "${MINGW_BIN}" ]]; then
    echo "[FAIL] no toolchain at .local/toolchain - run ./scripts/setup-rust-toolchain.sh first" >&2
    exit 1
fi

export PATH="${MINGW_BIN}:${PATH}"

# The manifest lives in golem-rs/, the cargo config at the repository root;
# running from here finds both, and the caller's working directory (which
# relative paths in arguments resolve against) is left alone. --manifest-path
# is a flag of each subcommand, not of cargo itself, so it goes after the
# subcommand.
[[ $# -ge 1 ]] || { exec cargo; }
sub="$1"; shift
cargo "${sub}" --manifest-path "${ROOT_DIR}/golem-rs/Cargo.toml" "$@"

# A build leaves the binary at the repository root (git-ignored), which is the
# same shape a release has: a golem executable sitting next to the bench/ and
# data/ it uses. paths::home picks the directory holding the binary when it
# has a bench/ beside it, so ./golem.exe works on the checkout's own files -
# and the same code finds its folders in a fresh release directory.
if [[ "${sub}" == "build" || "${sub}" == "test" ]]; then
    profile="debug"
    case " $* " in *" --release "*) profile="release" ;; esac
    # The binary inside target/ is always `golem` (`golem.exe` on Windows);
    # only the copy at the root is renamed on Unix, where `golem` is taken.
    bin="golem"; copy="golem-bin"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) bin="golem.exe"; copy="golem.exe" ;;
    esac
    built="$(ls -1 "${ROOT_DIR}/golem-rs/target"/*/"${profile}/${bin}" 2>/dev/null | head -n1 || true)"
    if [[ -n "${built}" ]]; then
        cp -f "${built}" "${ROOT_DIR}/${copy}"
        echo "[ OK ] ${copy} at the repository root (git-ignored) from ${built#"${ROOT_DIR}/"}"
    fi
fi
