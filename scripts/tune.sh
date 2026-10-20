#!/usr/bin/env bash

set -Eeuo pipefail
export LC_ALL=C

# ============================================================
# AUTO-TUNE
# ============================================================
#
# Answers "what is the best way to run model X on this machine?" with no AI
# and no hand-editing: for every installed engine that can serve the preset,
# sweep the offload split with real requests, then keep the fastest
# engine + split that is not spilling VRAM, and gate the winner with the
# 200K eval.
#
# Why the split matters: on a card smaller than the model the experts live
# in system RAM (--n-cpu-moe), and every layer left on the CPU costs decode
# speed while every layer moved to the GPU costs VRAM. Past ~97% of the
# card's memory the Windows driver spills to system memory silently and
# prefill collapses, so the optimum is the lowest n-cpu-moe that still fits.
#
# Usage: ./scripts/tune.sh [preset] [--runs N] [--prefill N] [--gate|--no-gate]
#                          [--candidates a,b,c] [--engines a,b] [--engine NAME]
#                          [--adopt] [--dry-run]
#   preset         preset section to tune (default: MODEL_ALIAS)
#   --engines      engines to compare (default: every installed one whose
#                  engine.ini says tune = true)
#   --engine NAME  tune just this engine (the quick path)
#   --gate         run the needle eval on the winner (default on; the needle
#                  is 200K tokens, or 90% of the model's context when that is
#                  smaller - a 64K model cannot answer a 200K needle)
#   --auto         tune every installed preset, keep the best that gates
#                  (scripts/tune-auto.sh)
#   --adopt        write ENGINE= and MODELS_PRESET= into .env for the winner
#   --min-decode N / --min-prefill N / --min-depth-decode N
#                  move the speed floor for this run (defaults from TUNE_MIN_*
#                  in .env; see docs/plan-tune-model-context.md for why 25/350/15)
#   --ctx N        pin the context instead of maximizing it; also TUNE_CTX_POLICY
#   --no-refine    only sweep the offload split (skip batch/threads/spec)
#   --dry-run      probe, print the plan, change nothing
#
# Output: data/tuned/<host>/<engine>/<preset>.ini   winning flags, per engine
#         data/tuned/<host>/winner.txt              the verdict + all numbers
#         data/tuned/models.effective.ini           config/models.ini + winner
# Every measured point goes to bench/results.jsonl like any other bench.
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

PORT="${LOCAL_AI_PORT:-8000}"
API_KEY="${LOCAL_AI_API_KEY:-local-key}"
# GOLEM_* overrides survive the .env re-source above (same convention as ./golem):
# without them there is no way to point the tuner at another preset file.
MODEL_ALIAS="${GOLEM_MODEL_ALIAS:-${MODEL_ALIAS:-local-model}}"
PRESET="${GOLEM_MODELS_PRESET:-${MODELS_PRESET:-config/models.ini}}"
ENGINE="${GOLEM_ENGINE:-${ENGINE:-llama.cpp}}"
DEPLOY_MODE="${DEPLOY_MODE:-native}"

RUNS=3
PREFILL=8000
GATE=1
DRY_RUN=0
REFINE=1
CANDIDATES=""
TARGET=""
TARGET_GIVEN=0
ENGINES_ARG=""
ADOPT=0
AUTO=0
# Speed floor (docs/plan-tune-model-context.md): a candidate under it is
# excluded from the ranking however fast its decode looks relative to worse
# candidates. .env can move the defaults (TUNE_MIN_*); CLI flags move them
# per run.
MIN_DECODE="${TUNE_MIN_DECODE:-25}"
MIN_PREFILL="${TUNE_MIN_PREFILL:-350}"
MIN_DEPTH_DECODE="${TUNE_MIN_DEPTH_DECODE:-15}"
CTX_POLICY="${TUNE_CTX_POLICY:-max}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --prefill) PREFILL="$2"; shift 2 ;;
        --gate) GATE=1; shift ;;
        --no-gate) GATE=0; shift ;;
        --refine) REFINE=1; shift ;;
        --no-refine) REFINE=0; shift ;;
        --candidates) CANDIDATES="$2"; shift 2 ;;
        --engines) ENGINES_ARG="${2//,/ }"; shift 2 ;;
        --all-engines) ENGINES_ARG="__all__"; shift ;;
        --engine) ENGINES_ARG="$2"; shift 2 ;;
        --adopt) ADOPT=1; shift ;;
        --auto) AUTO=1; shift ;;
        --min-decode) MIN_DECODE="$2"; shift 2 ;;
        --min-prefill) MIN_PREFILL="$2"; shift 2 ;;
        --min-depth-decode) MIN_DEPTH_DECODE="$2"; shift 2 ;;
        --ctx) CTX_POLICY="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -*) echo "unknown arg: $1" >&2; exit 1 ;;
        *) TARGET="$1"; TARGET_GIVEN=1; shift ;;
    esac
done
TARGET="${TARGET:-${MODEL_ALIAS}}"

say() { echo "[ .. ] $*"; }
ok()  { echo "[ OK ] $*"; }
die() { echo "[FAIL] $*" >&2; exit 1; }

# --auto tunes every installed preset and keeps the best one that passes the
# gate: a different job, so it lives in its own driver (tune-auto.sh) with
# this script as the per-preset worker. The flags are re-passed explicitly
# (the originals are consumed by now) so the driver tunes with the same
# settings the user asked for.
if [[ "${AUTO}" == 1 ]]; then
    if [[ "${TARGET_GIVEN}" == 1 ]]; then
        die "--auto tunes every installed preset; drop the preset argument"
    fi
    FWD=(--runs "${RUNS}" --prefill "${PREFILL}"
         --min-decode "${MIN_DECODE}" --min-prefill "${MIN_PREFILL}"
         --min-depth-decode "${MIN_DEPTH_DECODE}")
    if [[ "${ADOPT}" == 1 ]]; then FWD+=(--adopt); fi
    if [[ "${REFINE}" == 0 ]]; then FWD+=(--no-refine); fi
    if [[ "${CTX_POLICY}" != "max" ]]; then FWD+=(--ctx "${CTX_POLICY}"); fi
    # A single named engine can be forwarded; a list or __all__ cannot (the
    # driver sweeps whatever is installed, which is the point of --auto).
    if [[ -n "${ENGINES_ARG}" && "${ENGINES_ARG}" != "__all__" \
          && "${ENGINES_ARG}" != *,* && "${ENGINES_ARG}" != *" "* ]]; then
        FWD+=(--engine "${ENGINES_ARG}")
    fi
    exec "${ROOT_DIR}/scripts/tune-auto.sh" "${FWD[@]}"
fi

# engine_ini <name> <key>: one key from engines/<name>/engine.ini
engine_ini() {
    awk -v k="$2" '
        /^\[/ { next }
        /^[[:space:]]*[;#]/ || !/=/ { next }
        { key = $0; sub(/[[:space:]]*=.*/, "", key); gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
          if (key == k) { v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]+$/, "", v); print v; exit } }
    ' "${ROOT_DIR}/engines/$1/engine.ini" 2>/dev/null
}

preset_value() {
    # preset_value <section> <key> -> value, or empty
    awk -v want="$1" -v key="$2" '
        /^\[/ { s = $0; gsub(/^\[|\].*$/, "", s); next }
        /^[[:space:]]*[;#]/ || !/=/ { next }
        s == want {
            k = $0; sub(/[[:space:]]*=.*/, "", k); gsub(/^[[:space:]]+/, "", k)
            if (k == key) { v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]+$/, "", v); print v }
        }' "${PRESET_PATH}" | tail -n1
}

# ------------------------------------------------------------
# 1. Probe
# ------------------------------------------------------------

if ! command -v nvidia-smi >/dev/null 2>&1; then
    die "auto-tune needs nvidia-smi (NVIDIA card); for AMD set the split by hand for now"
fi
GPU="$(nvidia-smi --query-gpu=name --format=csv,noheader | head -n1 | tr -d '\r')"
read -r VRAM_TOTAL VRAM_USED_NOW < <(nvidia-smi --query-gpu=memory.total,memory.used \
    --format=csv,noheader,nounits | head -n1 | tr -d '\r' | awk -F, '{gsub(/ /,"");print $1" "$2}')

# /proc/meminfo is empty under some MSYS/Git Bash setups, so fall back to
# Windows' own counters there (and force a dot decimal separator).
ram_free_gb() {
    local v
    v="$(awk '/^MemAvailable/ {printf "%.1f", $2/1048576}' /proc/meminfo 2>/dev/null)"
    [[ -n "${v}" ]] && { echo "${v}"; return; }
    powershell.exe -NoProfile -Command \
        "[math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB,1)" 2>/dev/null \
        | tr -d '\r' | tr ',' '.'
}
RAM_FREE_GB="$(ram_free_gb)"
CORES="$(nproc 2>/dev/null || echo 8)"
# Physical cores (per socket). MSYS's /proc/cpuinfo lies about the count
# (reports 10 on a 12-core hybrid), so only trust it on real Linux and ask
# Windows otherwise. Topology tells the thread sweep where to probe: a hybrid
# P/E CPU has logical < 2 x physical (i7-12700KF: 8P with HT + 4E = 20
# logical / 12 physical), a plain SMT box has logical == 2 x physical.
PHYS_CORES=""
if ! uname -s | grep -qiE "mingw|msys|cygwin"; then
    PHYS_CORES="$(awk '/^cpu cores/ {c=$4} END {print c+0}' /proc/cpuinfo 2>/dev/null)"
fi
[[ -n "${PHYS_CORES}" && "${PHYS_CORES}" -gt 0 ]] || PHYS_CORES="$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).NumberOfCores' 2>/dev/null | tr -d '\r' | head -n1)"

# The cliff is a measured thing, not a number to guess at: when the driver
# spills to system memory the prefill rate collapses (~40% on the reference
# box). This estimate only seeds the sweep; selection below uses the
# measured collapse.
MARGIN_MIB=500
WALL_MIB=$(( VRAM_TOTAL - VRAM_USED_NOW - MARGIN_MIB ))

PRESET_PATH="${ROOT_DIR}/${PRESET}"
[[ -f "${PRESET_PATH}" ]] || die "preset file not found: ${PRESET_PATH}"

MODEL_FILE="$(preset_value "${TARGET}" model)"
[[ -n "${MODEL_FILE}" ]] || die "preset [${TARGET}] not found in ${PRESET}"
MODEL_PATH="${ROOT_DIR}/data/models/${MODEL_FILE}"
[[ -f "${MODEL_PATH}" ]] || die "model file not found: ${MODEL_PATH} (run ./golem pull ${TARGET})"

# The trained context (GGUF header, cached): the default ceiling for context
# maximization - extrapolating past it worked in testing (the 0.5B passed a
# 211K needle on a 32K-train model) but is per-model luck, so it must be
# asked for with an explicit --ctx, never defaulted to.
MODEL_CTX_TRAIN="$("${ROOT_DIR}/scripts/gguf-info.sh" "${MODEL_PATH}" 2>/dev/null | awk -F= '/^n_ctx_train/ {print $2}')"

MODEL_GB="$(awk -v b="$(stat -c %s "${MODEL_PATH}" 2>/dev/null || echo 0)" 'BEGIN {printf "%.1f", b/1073741824}')"
CTX="$(preset_value "${TARGET}" ctx-size)"
UB="$(preset_value "${TARGET}" ubatch-size)"

# ------------------------------------------------------------
# 2. Engines
# ------------------------------------------------------------

ENGINE_LIST=""
case "${ENGINES_ARG}" in
    "")        ENGINE_LIST="$(ls -1 "${ROOT_DIR}/engines" 2>/dev/null)" ;;   # all of them
    "__all__") ENGINE_LIST="$(ls -1 "${ROOT_DIR}/engines" 2>/dev/null)" ;;
    *)         ENGINE_LIST="${ENGINES_ARG}" ;;
esac
[[ -n "${ENGINE_LIST}" ]] || die "no engines found under engines/"

SWEEPABLE=""
for e in ${ENGINE_LIST}; do
    [[ -f "${ROOT_DIR}/engines/${e}/engine.ini" ]] || die "unknown engine '${e}'"
    # An engine installed but not built yet cannot be measured; the launcher
    # installs it on first start, so just note it and move on.
    if [[ "$(engine_ini "${e}" tune)" == "false" ]]; then
        echo "  ${e}: skipped - $(engine_ini "${e}" tune_reason)"
        continue
    fi
    SWEEPABLE+="${e} "
done
SWEEPABLE="${SWEEPABLE% }"
[[ -n "${SWEEPABLE}" ]] || die "no engine can be tuned for [${TARGET}]"

echo
echo "============================================================"
echo " AUTO-TUNE  [${TARGET}] on ${GPU}"
echo "============================================================"
echo "  model      ${MODEL_FILE} (${MODEL_GB} GB)"
echo "  engines    ${SWEEPABLE}  (${DEPLOY_MODE} mode)"
echo "  vram       ${VRAM_TOTAL} MiB total, ${VRAM_USED_NOW} MiB in use by the desktop"
echo "             wall ${WALL_MIB} MiB (seed heuristic; the real cliff shows up as a"
echo "             prefill collapse during the sweep)"
echo "  ram free   ${RAM_FREE_GB} GB   cpu ${CORES} threads   ctx ${CTX}   ubatch ${UB}"
echo "  floors     decode >= ${MIN_DECODE} t/s, prefill >= ${MIN_PREFILL} t/s, at-depth >= ${MIN_DEPTH_DECODE} t/s"
echo "  ctx        $( [[ "${CTX_POLICY}" == "max" ]] && echo "maximize (ceiling: the model's ${MODEL_CTX_TRAIN:-?}-token trained ctx)" || echo "pinned at ${CTX_POLICY}" )"

# The gate asks for a needle of up to 200K tokens (GATE_TOKENS caps it),
# always at 90% of the context the candidate actually serves: asking a 64K
# model for 200K would fail every candidate and adopt nothing. The final
# number is computed per candidate in the gate loop, after maximization.
GATE_TOKENS_MAX="${GATE_TOKENS:-200000}"
echo "  gate       needle at 90% of each candidate's tuned context, up to ${GATE_TOKENS_MAX} tokens"

# A game or a render job on the GPU makes every number noisy (decode varies
# by several times with desktop load, measured). Downloads and builds are
# refused outright below; heavy VRAM use only earns a warning, since the
# user may know better.
if (( VRAM_USED_NOW > 2000 )); then
    echo "  [WARN] ${VRAM_USED_NOW} MiB of VRAM is already in use (a game or another model?) -"
    echo "         the sweep will run, but the numbers will be noisy. Close it for a fair result."
fi

# Nothing may be downloading or building: both starve RAM and make the
# numbers meaningless (measured: 5-10 t/s). A dry run only prints the plan,
# so there it is a warning rather than a stop.
BUSY=0
if uname -s | grep -qiE "mingw|msys|cygwin"; then
    tasklist 2>/dev/null | grep -qiE '^(curl|wget|hf|huggingface-cli|cmake|msbuild|ninja)\.exe' && BUSY=1
else
    pgrep -x 'curl|wget|hf|cmake|ninja|make' >/dev/null 2>&1 && BUSY=1
fi
if [[ "${BUSY}" == 1 ]]; then
    [[ "${DRY_RUN}" == 1 ]] \
        && echo "  [WARN] a download or build is running - a real run would stop here" \
        || die "a download or build is running - re-run when the machine is quiet"
fi

# ------------------------------------------------------------
# 3. Seed (per engine: the engine's own placement tool when it ships one)
# ------------------------------------------------------------

seed_for() {
    local eng="$1" bin="" out="" seed=""
    if [[ "${eng}" == "llama.cpp" ]]; then
        for c in "${ROOT_DIR}/.local/engines/${eng}"/*/llama-fit-params \
                 "${ROOT_DIR}/.local/engines/${eng}"/*/llama-fit-params.exe \
                 "${ROOT_DIR}/.local/bin/llama-fit-params"; do
            [[ -f "${c}" ]] && { bin="${c}"; break; }
        done
    fi
    if [[ -n "${bin}" ]]; then
        out="$(cd "${ROOT_DIR}/data/models" && timeout 600 "${bin}" -m "${MODEL_FILE}" \
            -c "${CTX}" -ctk q8_0 -ctv q8_0 -fa on -b "${UB:-2048}" -ub "${UB:-2048}" 2>/dev/null \
            | grep -o '\-ot "[^"]*"' | head -n1 || true)"
        [[ -n "${out}" ]] && seed="$(grep -o 'blk\\\.[0-9]*' <<<"${out}" | sort -u | wc -l | tr -d ' ')"
    fi
    [[ -n "${seed}" ]] || seed="$(preset_value "${TARGET}" n-cpu-moe)"
    [[ -n "${seed}" ]] || seed=$(( 40 - 40 * WALL_MIB / ($(awk -v g="${MODEL_GB}" 'BEGIN{print (g<1?1:g)}') * 1024) ))
    echo "${seed}"
}

# ------------------------------------------------------------
# 4. Sweep: every engine, every candidate
# ------------------------------------------------------------

TUNE_DIR="${ROOT_DIR}/data/tuned"
CAND_DIR="${TUNE_DIR}/candidates"
# Same anonymous slug the recorder uses (GPU, VRAM, RAM, CPU): no hostname
# and no user name, since data/tuned can be shared with a benchmark report.
slug() { tr '[:upper:]' '[:lower:]' | sed 's/(r)\|(tm)//g; s/nvidia\|geforce\|intel\|core\|amd\|radeon\|[0-9]*th gen\|processor//g; s/[^a-z0-9]\+/-/g; s/^-*//; s/-*$//'; }
CPU_NAME="$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null)"
[[ -n "${CPU_NAME}" ]] || CPU_NAME="$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' 2>/dev/null | tr -d '\r' | head -n1)"
RAM_GB="$(awk '/^MemTotal/ {printf "%.0f", $2/1048576}' /proc/meminfo 2>/dev/null || echo 0)"
HOST_SLUG="$(echo "${GPU}" | slug)_$((VRAM_TOTAL / 1024))g_${RAM_GB}g_$(echo "${CPU_NAME}" | slug)"
OUT_DIR="${TUNE_DIR}/${HOST_SLUG}"
mkdir -p "${CAND_DIR}" "${OUT_DIR}"

# A candidate preset file: everything from the real preset, with the given
# "key = value" overrides applied to this section (they are printed right
# after its header, replacing the original lines for those keys).
write_candidate() {
    local out="$1"; shift
    awk -v want="${TARGET}" -v ov="$(printf '%s\n' "$@")" '
        BEGIN {
            n = split(ov, lines, "\n")
            for (i = 1; i <= n; i++) {
                if (lines[i] == "") continue
                k = lines[i]; sub(/[ \t]*=.*/, "", k); gsub(/^[ \t]+|[ \t]+$/, "", k)
                o[k] = lines[i]; keys[++nk] = k
            }
        }
        /^\[/ {
            s = $0; gsub(/^\[|\].*$/, "", s)
            print
            if (s == want) for (i = 1; i <= nk; i++) print o[keys[i]]
            next
        }
        s == want && /=/ {
            k = $0; sub(/[ \t]*=.*/, "", k); gsub(/^[ \t]+|[ \t]+$/, "", k)
            if (k in o) next
        }
        { print }
    ' "${PRESET_PATH}" > "${out}"
}

declare -A RESULT_DEC RESULT_PRE RESULT_VRAM RESULT_ENG RESULT_OV RESULT_N
POINTS=()          # "<engine>|<label>" in measured order

# One measurement point: write the candidate preset, restart the engine on
# it, bench it, and record it. Sets M_DEC / M_PRE / M_VRAM on success.
measure() {
    local eng="$1" label="$2"; shift 2
    local cfg="${CAND_DIR}/${TARGET}-${label}.ini"
    M_DEC=""; M_PRE=""; M_VRAM=""
    write_candidate "${cfg}" "$@"
    say "${eng} ${label}: restarting..."
    # GOLEM_* overrides, not plain env vars: ./golem re-sources .env, so
    # MODELS_PRESET/ENGINE/MODEL_ALIAS set here would be overwritten.
    ( cd "${ROOT_DIR}" && GOLEM_ENGINE="${eng}" GOLEM_MODEL_ALIAS="${TARGET}" \
        GOLEM_MODELS_PRESET="data/tuned/candidates/${TARGET}-${label}.ini" \
        ./golem restart >/dev/null 2>&1 ) \
        || { echo "  restart failed, skipping"; return 1; }

    for _ in $(seq 1 120); do
        curl -s --noproxy '*' -m 3 "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1 && break
        sleep 5
    done
    curl -s --noproxy '*' -m 3 "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1 \
        || { echo "  never became healthy, skipping"; return 1; }

    local out; out="$(cd "${ROOT_DIR}" && GOLEM_MODEL_ALIAS="${TARGET}" \
        RECORD_NOTE="tune ${TARGET} on ${eng}: ${label}" \
        ./scripts/benchmark-ai.sh --runs "${RUNS}" --prefill "${PREFILL}" 2>&1 || true)"
    M_DEC="$(grep -o 'decode_median=[0-9.]*' <<<"${out}" | cut -d= -f2)"
    M_PRE="$(grep -o 'prefill=[0-9.]*' <<<"${out}" | cut -d= -f2)"
    M_VRAM="$(grep -o 'vram=[0-9]*' <<<"${out}" | cut -d= -f2)"
    [[ -n "${M_DEC}" && -n "${M_VRAM}" ]] || { echo "  no usable result, skipping"; return 1; }

    local key="${eng}|${label}"
    POINTS+=("${key}")
    RESULT_DEC["${key}"]="${M_DEC}"; RESULT_PRE["${key}"]="${M_PRE}"
    RESULT_VRAM["${key}"]="${M_VRAM}"; RESULT_ENG["${key}"]="${eng}"
    RESULT_OV["${key}"]="$(printf '%s\n' "$@" | tr '\n' ';')"
    RESULT_N["${key}"]="$(grep -o 'n-cpu-moe = [0-9]*' <<<"$(printf '%s\n' "$@")" | awk '{print $3}')"
    printf "  %-14s %-12s decode %6s t/s  prefill %6s t/s  vram %5s MiB\n" \
        "${eng}" "${label}" "${M_DEC}" "${M_PRE}" "${M_VRAM}"
    # Every point is a measurement like any other: record it (the bench
    # script alone does not - recording lives in the ./golem wrapper).
    printf '%s\n' "${out}" > "${CAND_DIR}/out-${TARGET}-${label}.txt"
    GOLEM_ENGINE="${eng}" RECORD_MODEL="${TARGET}" "${ROOT_DIR}/scripts/record-result.sh" bench \
        "${CAND_DIR}/out-${TARGET}-${label}.txt" "tune ${TARGET} on ${eng}: ${label}" \
        >/dev/null 2>&1 || true
    return 0
}

# A model with a mixture of experts has an MTP head only if its GGUF says so;
# the spec on/off point is only worth spending time on when it might help.
MODEL_HAS_MTP=""
if [[ -n "${MODEL_PATH}" ]]; then
    MODEL_HAS_MTP="$("${ROOT_DIR}/scripts/gguf-info.sh" "${MODEL_PATH}" 2>/dev/null | awk -F= '/^mtp_tensors/ {print $2}')"
fi

for eng in ${SWEEPABLE}; do
    SEED="$(seed_for "${eng}")"
    if [[ -n "${CANDIDATES}" ]]; then
        LIST="${CANDIDATES//,/ }"
    else
        # Wider above the seed than below: the placement tool keeps ~1 GB
        # free, while a hand-tuned split on the reference box ran 4 layers
        # further (fitter = faster, up to the cliff).
        LIST=""
        for d in -2 -1 0 1 2 3 4 5 6; do
            n=$(( SEED + d ))
            (( n >= 0 && n <= 60 )) && LIST+="${n} "
        done
    fi
    LIST="${LIST% }"
    echo
    echo "  --- ${eng}: seed ${SEED}, offload candidates ${LIST// /, }"
    if [[ "${DRY_RUN}" == 1 ]]; then continue; fi

    BEST_LOCAL=""; BEST_LOCAL_DEC=""
    for n in ${LIST}; do
        measure "${eng}" "${eng}-n${n}" "n-cpu-moe = ${n}" "fit = off" || continue
        # Track this engine's best so far; the refine points build on it.
        if [[ -z "${BEST_LOCAL_DEC}" ]] || awk -v a="${M_DEC}" -v b="${BEST_LOCAL_DEC}" 'BEGIN {exit !(a > b)}'; then
            BEST_LOCAL="${n}"; BEST_LOCAL_DEC="${M_DEC}"
        fi
    done

    # Refine the engine's best offload with the knobs the plan promised:
    # prompt batch, thread count, and speculative decoding on/off - a handful
    # of extra points per engine; --no-refine skips them.
    if [[ "${REFINE}" == 1 && -n "${BEST_LOCAL}" ]]; then
        echo "  --- ${eng}: refining n-cpu-moe ${BEST_LOCAL} (batch, threads, spec)"
        base=("n-cpu-moe = ${BEST_LOCAL}" "fit = off")
        if [[ -n "${UB}" && "${UB}" != "1024" ]]; then
            measure "${eng}" "${eng}-ub1024" "${base[@]}" "ubatch-size = 1024" || true
        fi
        # Threads: probe the neighborhoods the CPU topology makes promising
        # (measured on the reference box, docs/models.md "Threads": 16 of 20
        # = P-cores with hyperthreads beats all 20 by ~8% - E-cores add sync
        # overhead when decode streams RAM-resident experts - and 8 P-only by
        # ~5%). Hybrid CPU: {P, 2P, all}; plain SMT: {physical, all-2, all}.
        # Deduplicated, the one equal to the server default skipped.
        if [[ -n "${CORES}" && "${CORES}" -gt 4 ]]; then
            if [[ -n "${PHYS_CORES}" && "${PHYS_CORES}" -gt 0 ]] && (( PHYS_CORES * 2 > CORES )); then
                P_N=$(( CORES - PHYS_CORES ))
                TSET="${P_N} $(( P_N * 2 )) ${CORES}"
            else
                TSET="${PHYS_CORES:-$(( CORES / 2 ))} $(( CORES - 2 )) ${CORES}"
            fi
            SEEN_T=" "
            for t in ${TSET}; do
                if [[ "${SEEN_T}" == *" ${t} "* ]]; then continue; fi
                SEEN_T="${SEEN_T}${t} "
                if (( t > 0 )) && [[ "${t}" != "${CPU_THREADS:-}" ]]; then
                    measure "${eng}" "${eng}-t${t}" "${base[@]}" "threads = ${t}" || true
                fi
            done
        fi
        if [[ "$(preset_value "${TARGET}" spec-type)" == "draft-mtp" && "${MODEL_HAS_MTP}" == "1" ]]; then
            measure "${eng}" "${eng}-nospec" "${base[@]}" "spec-type = none" || true
        fi
    fi
done

if [[ "${DRY_RUN}" == 1 ]]; then echo; ok "dry run: nothing started"; exit 0; fi
[[ ${#POINTS[@]} -gt 0 ]] || die "no candidate produced a measurement"

# ------------------------------------------------------------
# 5. Winner: fastest decode that is not spilling
# ------------------------------------------------------------
#
# The cliff is not a VRAM number to guess at: when the driver spills to
# system memory the prefill rate collapses. So a candidate whose prefill is
# far under the best prefill *that same engine* reached is excluded even if
# its decode looks good. Per engine, not across engines: engines differ in
# prefill by design (ik_llama.cpp runs ~480 t/s where llama.cpp runs ~830 on
# the reference box), and that is a property of the engine, not of spilling.
declare -A ENG_MAXPRE
for k in "${POINTS[@]}"; do
    eng="${RESULT_ENG[${k}]}"
    cur="${ENG_MAXPRE[${eng}]:-0}"
    if awk -v a="${RESULT_PRE[${k}]}" -v b="${cur}" 'BEGIN {exit !(a > b)}'; then ENG_MAXPRE["${eng}"]="${RESULT_PRE[${k}]}"; fi
done

RANKED=()
for k in "${POINTS[@]}"; do
    eng="${RESULT_ENG[${k}]}"
    if awk -v p="${RESULT_PRE[${k}]}" -v m="${ENG_MAXPRE[${eng}]}" 'BEGIN {exit !(m > 0 && p < 0.75 * m)}'; then
        echo "  ${eng} ${k##*|}: prefill ${RESULT_PRE[${k}]} far under ${eng}'s own ${ENG_MAXPRE[${eng}]} - spilling, excluded"
        continue
    fi
    # The speed floor is absolute, not relative: however good a point looks
    # next to worse points, an unusable setup must never win (docs/plan-
    # tune-model-context.md). Excluded points stay recorded, just not ranked.
    if awk -v d="${RESULT_DEC[${k}]}" -v m="${MIN_DECODE}" 'BEGIN {exit !(d+0 < m)}'; then
        echo "  ${eng} ${k##*|}: decode ${RESULT_DEC[${k}]} t/s under the ${MIN_DECODE} t/s floor - excluded"
        continue
    fi
    if awk -v p="${RESULT_PRE[${k}]}" -v m="${MIN_PREFILL}" 'BEGIN {exit !(p+0 < m)}'; then
        echo "  ${eng} ${k##*|}: prefill ${RESULT_PRE[${k}]} t/s under the ${MIN_PREFILL} t/s floor - excluded"
        continue
    fi
    RANKED+=("${k}")
done
[[ ${#RANKED[@]} -gt 0 ]] || die "every candidate was excluded (spilling, or under the speed floor decode >= ${MIN_DECODE} / prefill >= ${MIN_PREFILL} t/s) - lower the floors (--min-decode/--min-prefill, TUNE_MIN_* in .env) or pick a smaller context"

# sort by decode, best first
mapfile -t SORTED < <(for k in "${RANKED[@]}"; do printf '%s %s\n' "${RESULT_DEC[${k}]}" "${k}"; done | sort -gr | awk '{print $2}')

echo
echo "  ranked (best decode that is not spilling):"
for k in "${SORTED[@]}"; do
    printf "    %-14s %-12s %6s t/s decode  %6s t/s prefill  %5s MiB\n" \
        "${RESULT_ENG[${k}]}" "${k##*|}" "${RESULT_DEC[${k}]}" "${RESULT_PRE[${k}]}" "${RESULT_VRAM[${k}]}"
done

# ------------------------------------------------------------
# 6. Gate the winner (and fall back down the ranking if it fails)
# ------------------------------------------------------------

write_overlay() {
    local eng="$1" key="$2" dir="${OUT_DIR}/${eng}"
    mkdir -p "${dir}"
    {
        echo "; Auto-tuned by scripts/tune.sh on $(date -u +%Y-%m-%dT%H:%MZ) for this machine."
        echo "; Winner for engine ${eng} (${key##*|}); the launchers merge it over ${PRESET}."
        echo "; Sweep (label=decode t/s): $(for k in "${POINTS[@]}"; do [[ "${RESULT_ENG[${k}]}" == "${eng}" ]] && printf '%s=%s ' "${k##*|}" "${RESULT_DEC[${k}]}"; done)"
        echo "[${TARGET}]"
        # The winner's own overrides, minus the bookkeeping ones.
        printf '%s\n' "${RESULT_OV[${key}]}" | tr ';' '\n' | grep -vE '^[[:space:]]*$' | grep -v '^fit = off$'
        echo "fit = off"
    } > "${dir}/${TARGET}.ini"

    EFFECTIVE="${TUNE_DIR}/models.effective.ini"
    awk -v overlay="${dir}/${TARGET}.ini" '
        BEGIN {
            while ((getline line < overlay) > 0) {
                if (line ~ /^[[:space:]]*[;#]/ || line ~ /^[[:space:]]*$/) { }
                else if (line ~ /^\[/) { s = line; gsub(/^\[|\].*$/, "", s) }
                else {
                    k = line; sub(/[[:space:]]*=.*/, "", k); gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
                    v = line; sub(/^[^=]*=[[:space:]]*/, "", v); gsub(/[[:space:]]+$/, "", v)
                    ov[s, k] = v
                    if (!((s, k) in seen)) { seen[s, k] = 1; order[s] = order[s] " " k }
                    if (!(s in sorder)) { sorder[s] = 1; slist[++sn] = s }
                }
            }
            close(overlay)
        }
        /^\[/ { s = $0; gsub(/^\[|\].*$/, "", s); cur = s; inthefile[s] = 1; print; next }
        cur != "" && /=/ {
            k = $0; sub(/[[:space:]]*=.*/, "", k); gsub(/^[[:space:]]+|[[:space:]]+$/, "", k)
            if ((cur, k) in ov) { print k " = " ov[cur, k]; next }
        }
        { print }
        END {
            for (i = 1; i <= sn; i++) {
                s = slist[i]
                if (s in inthefile) continue
                print ""; print "[" s "]"
                n = split(order[s], ks, " ")
                for (j = 1; j <= n; j++) if (ks[j] != "") print ks[j] " = " ov[s, ks[j]]
            }
        }
    ' "${PRESET_PATH}" > "${EFFECTIVE}"
    echo "${EFFECTIVE}"
}

# The context a candidate point serves: its own ctx-size override, else the
# preset's, else the engine default (= the model's trained ctx, observed).
cand_ctx() {
    local v
    v="$(tr ';' '\n' <<<"${RESULT_OV[$1]}" | sed -n 's/^ctx-size = //p' | tail -n1)"
    [[ -n "${v}" ]] || v="${CTX:-${MODEL_CTX_TRAIN:-4096}}"
    echo "${v}"
}

# Context maximization (docs/plan-tune-model-context.md): the preset's ctx is
# a starting point, not a ceiling - a model that truly serves more should not
# sit low because a smoke-test preset said so. Grow the winner's context
# toward the model's trained ctx (or a pinned --ctx) while the card holds and
# the decode floor is met; every rung is a recorded point like any other.
# Sets MAX_KEY to the final measured point; empty = keep the candidate as is.
MAX_KEY=""
maximize_ctx() {
    local key="$1" eng="$2" cur next mid
    local -a ovs=()
    mapfile -t ovs < <(tr ';' '\n' <<<"${RESULT_OV[${key}]}" | grep -v '^$')
    if [[ "${CTX_POLICY}" != "max" ]]; then
        # Pinned: one rung straight to the pin, up or down.
        cur="${CTX_POLICY}"
        if measure "${eng}" "${eng}-ctx${cur}" "${ovs[@]}" "ctx-size = ${cur}"; then
            MAX_KEY="${eng}|${eng}-ctx${cur}"
        fi
        return
    fi
    cur="${CTX:-4096}"
    local ceiling="${MODEL_CTX_TRAIN:-${cur}}"
    while :; do
        next=$(( cur * 2 ))
        if (( next > ceiling )); then next="${ceiling}"; fi
        if (( next <= cur )); then break; fi
        if ! measure "${eng}" "${eng}-ctx${next}" "${ovs[@]}" "ctx-size = ${next}"; then break; fi
        if awk -v v="${M_VRAM}" -v w="${WALL_MIB}" 'BEGIN {exit !(v+0 > w)}' \
           || awk -v d="${M_DEC}" -v m="${MIN_DECODE}" 'BEGIN {exit !(d+0 < m)}'; then
            # Over the VRAM wall or under the floor: one midpoint probe, then
            # stop - finer steps are noise at this point in the sweep.
            mid=$(( (cur + next) / 2 ))
            if (( mid > cur )) \
               && measure "${eng}" "${eng}-ctx${mid}" "${ovs[@]}" "ctx-size = ${mid}" \
               && ! awk -v v="${M_VRAM}" -v w="${WALL_MIB}" 'BEGIN {exit !(v+0 > w)}' \
               && ! awk -v d="${M_DEC}" -v m="${MIN_DECODE}" 'BEGIN {exit !(d+0 < m)}'; then
                MAX_KEY="${eng}|${eng}-ctx${mid}"
            fi
            break
        fi
        cur="${next}"; MAX_KEY="${eng}|${eng}-ctx${next}"
        say "context grows to ${cur} (${M_DEC} t/s decode, ${M_VRAM} MiB VRAM)"
    done
}

WINNER=""; WINNER_ENG=""; WINNER_N=""
for k in "${SORTED[@]}"; do
    eng="${RESULT_ENG[${k}]}"; n="${RESULT_N[${k}]:-?}"
    ok "candidate winner: ${eng} ${k##*|} (${RESULT_DEC[${k}]} t/s decode, ${RESULT_PRE[${k}]} t/s prefill, ${RESULT_VRAM[${k}]} MiB VRAM)"
    MAX_KEY=""
    maximize_ctx "${k}" "${eng}"
    if [[ -n "${MAX_KEY}" ]]; then
        k="${MAX_KEY}"
        say "context maximized: ${eng} now serves $(cand_ctx "${k}") tokens"
    fi
    CAND_CTX="$(cand_ctx "${k}")"
    GATE_MAX=$(( CAND_CTX * 9 / 10 ))
    if (( GATE_MAX < GATE_TOKENS_MAX )); then GATE_TOKENS="${GATE_MAX}"; else GATE_TOKENS="${GATE_TOKENS_MAX}"; fi
    EFFECTIVE="$(write_overlay "${eng}" "${k}")"

    if [[ "${GATE}" != 1 ]]; then WINNER="${k}"; WINNER_ENG="${eng}"; WINNER_N="${n}"; break; fi

    say "gating ${eng} ${k##*|}: restarting and running the ${GATE_TOKENS}-token needle (this takes a while)..."
    ( cd "${ROOT_DIR}" && GOLEM_ENGINE="${eng}" GOLEM_MODEL_ALIAS="${TARGET}" \
        GOLEM_MODELS_PRESET="data/tuned/models.effective.ini" \
        ./golem restart >/dev/null 2>&1 ) || true
    for _ in $(seq 1 120); do
        curl -s --noproxy '*' -m 3 "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1 && break
        sleep 5
    done
    EVAL_OUT="$( cd "${ROOT_DIR}" && GOLEM_MODEL_ALIAS="${TARGET}" \
        ./golem eval --long "${GATE_TOKENS}" 2>&1 || true )"
    grep '^RESULT' <<<"${EVAL_OUT}" | tail -n1 | sed 's/^/    /'
    if grep -q "long_ctx_${GATE_TOKENS}=PASS" <<<"${EVAL_OUT}"; then
        # A context you cannot use at depth is a fake win: the eval measures
        # decode at full depth, and the floor applies to it too.
        DEPTH_DEC="$(grep -o 'gen_tok_s_at_depth=[0-9.]*' <<<"${EVAL_OUT}" | cut -d= -f2)"
        if [[ -n "${DEPTH_DEC}" ]] && awk -v d="${DEPTH_DEC}" -v m="${MIN_DEPTH_DECODE}" 'BEGIN {exit !(d > 0 && d < m)}'; then
            echo "  [WARN] decode at depth ${DEPTH_DEC} t/s under the ${MIN_DEPTH_DECODE} t/s floor for ${eng} + n-cpu-moe ${n} - trying the next candidate"
            continue
        fi
        ok "eval gate PASSED"
        WINNER="${k}"; WINNER_ENG="${eng}"; WINNER_N="${n}"
        break
    fi
    echo "  [WARN] eval gate FAILED for ${eng} + n-cpu-moe ${n} - trying the next candidate"
    # Say why. A gate can fail with no RESULT line at all (the eval died in an
    # earlier section, e.g. the server refused the request), and without this
    # the failure is indistinguishable from a missed needle.
    if ! grep -q '^RESULT' <<<"${EVAL_OUT}"; then
        echo "        (the eval produced no RESULT line; its last lines were:)"
    fi
    tail -n 4 <<<"${EVAL_OUT}" | sed 's/^/        | /'
done

[[ -n "${WINNER}" ]] || die "no candidate passed the eval gate; nothing adopted"

# ------------------------------------------------------------
# 7. Verdict
# ------------------------------------------------------------

VERDICT="${OUT_DIR}/winner.txt"
{
    echo "model      ${TARGET} (${MODEL_FILE})"
    echo "host       ${HOST_SLUG}"
    echo "date       $(date -u +%Y-%m-%dT%H:%MZ)"
    echo "engine     ${WINNER_ENG}"
    echo "n-cpu-moe  ${WINNER_N}"
    echo "ctx        $(cand_ctx "${WINNER}")"
    echo "decode     ${RESULT_DEC[${WINNER}]} t/s (median), prefill ${RESULT_PRE[${WINNER}]} t/s, vram ${RESULT_VRAM[${WINNER}]} MiB"
    echo "eval       $( [[ "${GATE}" == 1 ]] && echo "${GATE_TOKENS}-token needle PASS" || echo "not run" )"
    echo
    echo "all points:"
    for k in "${POINTS[@]}"; do
        printf '  %-14s %-12s %6s t/s decode  %6s t/s prefill  %5s MiB\n' \
            "${RESULT_ENG[${k}]}" "${k##*|}" "${RESULT_DEC[${k}]}" "${RESULT_PRE[${k}]}" "${RESULT_VRAM[${k}]}"
    done
} > "${VERDICT}"

echo
ok "winner: ${WINNER_ENG} with n-cpu-moe ${WINNER_N} - ${RESULT_DEC[${WINNER}]} t/s decode, ${RESULT_PRE[${WINNER}]} t/s prefill"
ok "overlay: ${OUT_DIR}/${WINNER_ENG}/${TARGET}.ini"
ok "verdict: ${VERDICT#"${ROOT_DIR}/"}"

# One machine-readable line for the drivers (tune-auto.sh ranks on it), in
# the same RESULT idiom the bench and eval scripts use.
echo "RESULT tune_preset=${TARGET} engine=${WINNER_ENG} n_cpu_moe=${WINNER_N} ctx=$(cand_ctx "${WINNER}") decode=${RESULT_DEC[${WINNER}]} prefill=${RESULT_PRE[${WINNER}]} vram=${RESULT_VRAM[${WINNER}]}"

if [[ "${ADOPT}" == 1 ]]; then
    cp -f "${ROOT_DIR}/.env" "${ROOT_DIR}/.env.bak"
    if grep -q '^ENGINE=' "${ROOT_DIR}/.env"; then
        sed -i "s|^ENGINE=.*|ENGINE=\"${WINNER_ENG}\"|" "${ROOT_DIR}/.env"
    else
        printf '\nENGINE="%s"\n' "${WINNER_ENG}" >> "${ROOT_DIR}/.env"
    fi
    if grep -q '^MODELS_PRESET=' "${ROOT_DIR}/.env"; then
        sed -i 's|^MODELS_PRESET=.*|MODELS_PRESET="data/tuned/models.effective.ini"|' "${ROOT_DIR}/.env"
    else
        printf 'MODELS_PRESET="data/tuned/models.effective.ini"\n' >> "${ROOT_DIR}/.env"
    fi
    ok "adopted in .env (backup: .env.bak): ENGINE=${WINNER_ENG}, MODELS_PRESET=data/tuned/models.effective.ini"
else
    echo
    echo "  to adopt, either re-run with --adopt or set in .env:"
    echo "    ENGINE=\"${WINNER_ENG}\""
    echo "    MODELS_PRESET=\"data/tuned/models.effective.ini\""
fi
