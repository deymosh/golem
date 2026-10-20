#!/usr/bin/env bash

set -Eeuo pipefail
export LC_ALL=C

# ============================================================
# AUTO-TUNE ACROSS MODELS
# ============================================================
#
# "Set me up with the smartest model this machine can run, at the biggest
# context it can truly serve" - the question a new machine asks, and the
# one `tune <preset>` cannot answer because the user had to name the model
# first.
#
# How the best model is chosen:
#   1. every preset section whose GGUF is already in data/models gets a
#      cheap sweep (no gate, no refine) with scripts/tune.sh, which is the
#      same worker the single-preset path uses;
#   2. the presets are ranked by their "; Quality: N" comment - a vendor
#      number (SWE-bench Verified and friends) that is only ever a RANKING
#      input, never an adoption reason; presets without one rank by
#      parameter count, and the table says which is which;
#   3. the best-ranked preset is tuned fully (refine + 200K-style gate) and
#      adopted; if it fails the gate, the next ranked preset is tried.
#
# A model that is not downloaded is skipped, not fetched: that is
# `./golem pull <preset>`'s job, and a download would invalidate every
# measurement taken next to it.
#
# Usage: ./golem tune --auto [--runs N] [--prefill N] [--adopt] [--engine NAME]
#                        [--min-decode N] [--min-prefill N]
#                        [--min-depth-decode N] [--ctx N] [--no-refine]
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

PRESET="${GOLEM_MODELS_PRESET:-${MODELS_PRESET:-config/models.ini}}"
PRESET_PATH="${ROOT_DIR}/${PRESET}"
RUNS="${TUNE_RUNS:-3}"
PREFILL="${TUNE_PREFILL:-8000}"
ADOPT=0
REFINE=1
MIN_DECODE="${TUNE_MIN_DECODE:-25}"
MIN_PREFILL="${TUNE_MIN_PREFILL:-350}"
MIN_DEPTH_DECODE="${TUNE_MIN_DEPTH_DECODE:-15}"
CTX_POLICY="${TUNE_CTX_POLICY:-max}"
ENGINE_ONE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --prefill) PREFILL="$2"; shift 2 ;;
        --adopt) ADOPT=1; shift ;;
        --refine) REFINE=1; shift ;;
        --no-refine) REFINE=0; shift ;;
        --min-decode) MIN_DECODE="$2"; shift 2 ;;
        --min-prefill) MIN_PREFILL="$2"; shift 2 ;;
        --min-depth-decode) MIN_DEPTH_DECODE="$2"; shift 2 ;;
        --ctx) CTX_POLICY="$2"; shift 2 ;;
        --engine) ENGINE_ONE="$2"; shift 2 ;;
        -*) echo "unknown arg: $1" >&2; exit 1 ;;
        *) echo "tune-auto takes no preset argument: $1" >&2; exit 1 ;;
    esac
done

say() { echo "[ .. ] $*"; }
ok()  { echo "[ OK ] $*"; }
die() { echo "[FAIL] $*" >&2; exit 1; }

[[ -f "${PRESET_PATH}" ]] || die "preset file not found: ${PRESET_PATH}"

# ------------------------------------------------------------
# 1. Installed presets
# ------------------------------------------------------------

# Section -> model file, in file order. A section without a local model file
# is reported and skipped.
SECTIONS=(); MODELS=()
while IFS=$'\t' read -r sec mfile; do
    [[ -n "${sec}" && -n "${mfile}" ]] || continue
    if [[ ! -f "${ROOT_DIR}/data/models/${mfile}" ]]; then
        echo "  ${sec}: skipped - ${mfile} is not downloaded (./golem pull ${sec})"
        continue
    fi
    SECTIONS+=("${sec}"); MODELS+=("${mfile}")
done < <(awk '
    /^\[/ { s = $0; gsub(/^\[|\].*$/, "", s); next }
    /^[[:space:]]*[;#]/ || !/=/ { next }
    s != "" && s != "*" {
        k = $0; sub(/[[:space:]]*=.*/, "", k); gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
        v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); gsub(/[[:space:]]+$/, "", v)
        if (k == "model") print s "\t" v
    }' "${PRESET_PATH}")

[[ ${#SECTIONS[@]} -gt 0 ]] || die "no installed preset to tune (data/models has none of ${PRESET}'s models)"

# "; Quality: 79" - the vendor's SWE-bench Verified score or equivalent.
# Optional; absent means the preset ranks by size instead. The comment may
# sit right above its section header (where this repo puts "; Download:")
# or inside the section: both are read, which an off-by-one section taught
# the hard way.
quality_of() {
    awk -v want="$1" '
        /^\[/ {
            s = $0; gsub(/^\[|\].*$/, "", s)
            if (s == want) { if (pending != "") { print pending; exit }; inwant = 1 }
            else { inwant = 0 }
            pending = ""
            next
        }
        /^[[:space:]]*;[[:space:]]*[Qq]uality:/ {
            v = $0; sub(/^[^:]*:[[:space:]]*/, "", v); gsub(/[^0-9.].*$/, "", v)
            if (v == "") next
            if (inwant) { print v; exit }
            pending = v
            next
        }
    ' "${PRESET_PATH}"
}

params_of() {
    "${ROOT_DIR}/scripts/gguf-info.sh" "${ROOT_DIR}/data/models/$1" 2>/dev/null \
        | awk -F= '/^params_total/ {printf "%.2f", $2/1e9}'
}

echo
echo "============================================================"
echo " AUTO-TUNE  [every installed preset]"
echo "============================================================"
echo "  preset file ${PRESET}   ${#SECTIONS[@]} installed section(s)"
echo "  floors      decode >= ${MIN_DECODE} t/s, prefill >= ${MIN_PREFILL} t/s, at-depth >= ${MIN_DEPTH_DECODE} t/s"
echo "  ranking     ; Quality: <score> when the preset has one, else parameters"
echo

# ------------------------------------------------------------
# 2. Cheap pass: each preset's frontier, no gate
# ------------------------------------------------------------

declare -A A_DEC A_PRE A_CTX A_VRAM A_Q A_PB A_OK
CANDIDATES=()
WORK_DIR="${ROOT_DIR}/data/tuned"
mkdir -p "${WORK_DIR}"

for i in "${!SECTIONS[@]}"; do
    p="${SECTIONS[$i]}"; m="${MODELS[$i]}"
    q="$(quality_of "${p}")"
    pb="$(params_of "${m}")"
    A_Q["${p}"]="${q}"; A_PB["${p}"]="${pb}"
    echo "  --- ${p} (${m}, ${pb}B${q:+, quality ${q}}): sweep, no gate"
    fwd=(--no-gate --runs "${RUNS}" --prefill "${PREFILL}"
         --min-decode "${MIN_DECODE}" --min-prefill "${MIN_PREFILL}"
         --min-depth-decode "${MIN_DEPTH_DECODE}")
    if [[ -n "${ENGINE_ONE}" ]]; then fwd+=(--engine "${ENGINE_ONE}"); fi
    if [[ "${REFINE}" == 0 ]]; then fwd+=(--no-refine); fi
    if [[ "${CTX_POLICY}" != "max" ]]; then fwd+=(--ctx "${CTX_POLICY}"); fi
    log="${WORK_DIR}/auto-${p}.log"
    if ! "${ROOT_DIR}/scripts/tune.sh" "${p}" "${fwd[@]}" > "${log}" 2>&1; then
        echo "  [WARN] ${p}: no usable point (see ${log#"${ROOT_DIR}/"}) - not a candidate"
        continue
    fi
    line="$(grep '^RESULT tune_preset=' "${log}" | tail -n1)"
    [[ -n "${line}" ]] || { echo "  [WARN] ${p}: no RESULT line (see ${log#"${ROOT_DIR}/"})"; continue; }
    get() { grep -o "$1=[^ ]*" <<<"${line}" | cut -d= -f2; }
    A_DEC["${p}"]="$(get decode)"; A_PRE["${p}"]="$(get prefill)"
    A_CTX["${p}"]="$(get ctx)";   A_VRAM["${p}"]="$(get vram)"
    A_OK["${p}"]=1
    CANDIDATES+=("${p}")
    printf "      %6s t/s decode  %6s t/s prefill  ctx %s  vram %s MiB\n" \
        "${A_DEC[${p}]}" "${A_PRE[${p}]}" "${A_CTX[${p}]}" "${A_VRAM[${p}]}"
done

[[ ${#CANDIDATES[@]} -gt 0 ]] || die "no preset produced a measurement above the floors"

# ------------------------------------------------------------
# 3. Rank: quality first (vendor number), then size, then context
# ------------------------------------------------------------

# Sort key: quality when the preset has one, else 0 (a preset without a score
# never outranks one with a score - it has no claim to "smartest"), then
# parameters, then the context it reached, then speed: two presets of the
# same model and quant reach the same quality, and the faster one is the
# better answer.
mapfile -t RANKED < <(
    for p in "${CANDIDATES[@]}"; do
        printf '%s\t%s\t%s\t%s\t%s\n' "${A_Q[${p}]:-0}" "${A_PB[${p}]:-0}" \
            "${A_CTX[${p}]:-0}" "${A_DEC[${p}]:-0}" "${p}"
    done | sort -t$'\t' -k1,1gr -k2,2gr -k3,3gr -k4,4gr | cut -f5
)

echo
echo "  ranked (quality, then parameters, then context, then speed):"
for p in "${RANKED[@]}"; do
    printf "    %-28s quality %-6s %6sB  %6s t/s decode  ctx %s\n" \
        "${p}" "$( [[ -n "${A_Q[${p}]}" ]] && echo "${A_Q[${p}]}" || echo '-' )" \
        "${A_PB[${p}]}" "${A_DEC[${p}]}" "${A_CTX[${p}]}"
done

# ------------------------------------------------------------
# 4. Full tune of the best preset, down the ranking until one gates
# ------------------------------------------------------------

WINNER=""
for p in "${RANKED[@]}"; do
    echo
    ok "best-ranked preset: ${p} - tuning it fully (refine + gate)"
    fwd=(--runs "${RUNS}" --prefill "${PREFILL}"
         --min-decode "${MIN_DECODE}" --min-prefill "${MIN_PREFILL}"
         --min-depth-decode "${MIN_DEPTH_DECODE}")
    if [[ -n "${ENGINE_ONE}" ]]; then fwd+=(--engine "${ENGINE_ONE}"); fi
    if [[ "${REFINE}" == 0 ]]; then fwd+=(--no-refine); fi
    if [[ "${CTX_POLICY}" != "max" ]]; then fwd+=(--ctx "${CTX_POLICY}"); fi
    if [[ "${ADOPT}" == 1 ]]; then fwd+=(--adopt); fi
    if "${ROOT_DIR}/scripts/tune.sh" "${p}" "${fwd[@]}"; then
        WINNER="${p}"
        break
    fi
    echo "  [WARN] ${p} did not pass the gate - trying the next ranked preset"
done

[[ -n "${WINNER}" ]] || die "no installed preset passed the gate; nothing adopted"

echo
ok "auto-tune winner: ${WINNER} (engine + flags in data/tuned/models.effective.ini)"
if [[ "${ADOPT}" == 1 ]]; then
    ok "adopted in .env; restart to serve it (./golem restart)"
else
    echo "  to adopt, re-run with --adopt, or set MODELS_PRESET=\"data/tuned/models.effective.ini\" in .env"
fi
