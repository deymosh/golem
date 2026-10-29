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

# The workspace manifest, the cargo config and the rustup override are all at
# the repository root, and cargo finds them from the working directory: it
# runs from the root whatever directory called this, so relative paths in the
# arguments resolve against the root too.
cd "${ROOT_DIR}"
[[ $# -ge 1 ]] || { exec cargo; }
sub="$1"; shift
cargo "${sub}" "$@"

# A build leaves the binary at the repository root (git-ignored), which is the
# same shape a release has: a golem executable sitting next to the results/
# and data/ it uses. paths::home is the directory holding the binary, so
# ./golem.exe works on the checkout's own files - and the same code makes its
# folders in a fresh release directory.
#
# A test run builds without features, so it only puts a binary there when
# there is none: replacing one would swap a desktop build for one with no
# app window, and golemw.exe would open the browser instead.
root_bin="${ROOT_DIR}/golem-bin"
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) root_bin="${ROOT_DIR}/golem.exe" ;; esac
if [[ "${sub}" == "build" || ( "${sub}" == "test" && ! -f "${root_bin}" ) ]]; then
    profile="debug"
    case " $* " in *" --release "*) profile="release" ;; esac
    # The binary inside target/ is always `golem` (`golem.exe` on Windows);
    # only the copy at the root is renamed on Unix, where `golem` is taken.
    bin="golem"; copy="golem-bin"
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) bin="golem.exe"; copy="golem.exe" ;;
    esac
    # target/<triple>/<profile>/ when a target triple is configured (the
    # Windows GNU setup), else target/<profile>/ (Linux, macOS).
    built="$(ls -1 "${ROOT_DIR}/target"/*/"${profile}/${bin}" 2>/dev/null | head -n1 || true)"
    if [[ -z "${built}" && -f "${ROOT_DIR}/target/${profile}/${bin}" ]]; then
        built="${ROOT_DIR}/target/${profile}/${bin}"
    fi
    if [[ -n "${built}" ]]; then
        cp -f "${built}" "${ROOT_DIR}/${copy}"
        echo "[ OK ] ${copy} at the repository root (git-ignored) from ${built#"${ROOT_DIR}/"}"
    fi
    # The windowed launcher, when the desktop feature built it.
    launcher="${built%/*}/golemw.exe"
    if [[ -n "${built}" && -f "${launcher}" ]]; then
        cp -f "${launcher}" "${ROOT_DIR}/golemw.exe"
    fi
    # A GNU build with the desktop feature loads WebView2Loader.dll at run
    # time (an MSVC build, like CI's release, links it in); it goes beside
    # the binary, where Windows looks first.
    loader="$(ls -1t "${ROOT_DIR}/target"/*/"${profile}"/build/webview2-com-sys-*/out/x64/WebView2Loader.dll 2>/dev/null | head -n1 || true)"
    if [[ -n "${loader}" ]]; then
        cp -f "${loader}" "${ROOT_DIR}/WebView2Loader.dll"
    fi
fi
