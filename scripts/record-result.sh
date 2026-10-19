#!/usr/bin/env bash

# Best-effort: a missing field becomes null, never an aborted record.
set -Eeu
export LC_ALL=C

# ============================================================
# BENCHMARK RECORD
# ============================================================
#
# Appends one JSON line to bench/results.jsonl from the RESULT lines of a
# bench or eval run, plus what was measured on: host hardware, engine
# build, model file and the served preset's effective flags. Runs on the
# host (Git Bash, Linux, WSL-less), never inside WSL, so the hardware is
# the real one. Needs only curl + awk.
#
# Usage: ./scripts/record-result.sh <bench|eval> <captured-output-file> [note]
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

KIND="$1"
OUT_FILE="$2"
NOTE="${3:-}"
PORT="${LOCAL_AI_PORT:-8000}"
API_KEY="${LOCAL_AI_API_KEY:-local-key}"
# RECORD_MODEL: record for another preset than MODEL_ALIAS (backfills).
MODEL_ALIAS="${RECORD_MODEL:-${MODEL_ALIAS:-local-model}}"
RESULTS="${ROOT_DIR}/bench/results.jsonl"

json_str() {
    # json_str <text> -> quoted, escaped JSON string
    printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr -d '\r\n')"
}

json_num() {
    # json_num <value> -> the number, or null when not numeric
    [[ "$1" =~ ^-?[0-9]+(\.[0-9]+)?$ ]] && printf '%s' "$1" || printf 'null'
}

result_field() {
    # result_field <key> -> value of key=value on the RESULT lines
    grep '^RESULT' "${OUT_FILE}" | tr ' ' '\n' | grep "^$1=" | tail -n1 | cut -d= -f2- | tr -d '\r'
}

# ------------------------------------------------------------
# Host: anonymous hardware slug (public repo: no hostname, no IP)
# ------------------------------------------------------------

GPU="unknown"; VRAM_TOTAL=0; VRAM_USED=""
if command -v nvidia-smi >/dev/null 2>&1; then
    IFS=, read -r GPU VRAM_TOTAL VRAM_USED < <(nvidia-smi --query-gpu=name,memory.total,memory.used \
        --format=csv,noheader,nounits | head -n1 | tr -d '\r')
    GPU="$(echo "${GPU}" | sed 's/^ *//; s/ *$//')"
    VRAM_TOTAL="${VRAM_TOTAL// /}"; VRAM_USED="${VRAM_USED// /}"
fi
RAM_GB="$(awk '/^MemTotal/ {printf "%.0f", $2/1048576}' /proc/meminfo 2>/dev/null || echo 0)"
RAM_FREE_GB="$(awk '/^MemAvailable|^MemFree/ {v=$2} END {printf "%.1f", v/1048576}' /proc/meminfo 2>/dev/null || echo 0)"
CPU="$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null || echo unknown)"

slug() { tr '[:upper:]' '[:lower:]' | sed 's/(r)\|(tm)//g; s/nvidia\|geforce\|intel\|core\|amd\|radeon\|[0-9]*th gen\|processor//g; s/[^a-z0-9]\+/-/g; s/^-*//; s/-*$//'; }
HOST_SLUG="$(echo "${GPU}" | slug)_$((VRAM_TOTAL / 1024))g_${RAM_GB}g_$(echo "${CPU}" | slug)"

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) OS=windows ;;
    Darwin) OS=macos ;;
    *) OS=linux ;;
esac

# Quiet = no download/build running (they starve RAM and cost 5-10 t/s).
QUIET=true
if [[ "${OS}" == windows ]]; then
    tasklist 2>/dev/null | grep -qiE '^(curl|wget|hf|huggingface-cli|cmake|msbuild|ninja)\.exe' && QUIET=false
else
    pgrep -x 'curl|wget|hf|huggingface-cli|cmake|ninja|make' >/dev/null 2>&1 && QUIET=false
fi

# ------------------------------------------------------------
# Engine build + model file, straight from the server
# ------------------------------------------------------------

PROPS="$(curl --silent --max-time 5 --noproxy '*' -H "Authorization: Bearer ${API_KEY}" \
    "http://127.0.0.1:${PORT}/props?model=${MODEL_ALIAS}" 2>/dev/null || true)"
BUILD="$(grep -o '"build_info":"[^"]*"' <<<"${PROPS}" | cut -d'"' -f4 || true)"
MODEL_FILE_SERVED="$(grep -o '"model_path":"[^"]*"' <<<"${PROPS}" | cut -d'"' -f4 || true)"
MODEL_FILE_SERVED="${MODEL_FILE_SERVED##*[/\\]}"

# Some servers (ik_llama.cpp) report no build in /props: take it from the
# running engine's install path instead (.local/engines/<engine>/<build>).
SRV_PID=""
if [[ -f "${ROOT_DIR}/.local/llama-server-windows.pid" ]]; then
    SRV_PID="$(tr -d ' \r\n' < "${ROOT_DIR}/.local/llama-server-windows.pid")"
fi
if [[ -z "${BUILD}" && -n "${SRV_PID}" ]]; then
    if [[ "${OS}" == windows ]]; then
        CMDLINE="$(powershell.exe -NoProfile -Command "(Get-CimInstance Win32_Process -Filter 'ProcessId=${SRV_PID}').CommandLine; (Get-CimInstance Win32_Process -Filter 'ProcessId=${SRV_PID}').ExecutablePath" 2>/dev/null | tr -d '\r')"
    else
        CMDLINE="$(tr '\0' ' ' < "/proc/${SRV_PID}/cmdline" 2>/dev/null) $(readlink -f "/proc/${SRV_PID}/exe" 2>/dev/null)"
    fi
    for tok in ${CMDLINE}; do
        case "${tok}" in
            *engines/*|*engines\\*)
                rel="${tok#*engines[/\\]}"; name="${rel%%[/\\]*}"
                if [[ "${name}" == "${ENGINE:-llama.cpp}" ]]; then
                    rest="${rel#*[/\\]}"
                    BUILD="${rest%%[/\\]*}"
                    BUILD="${BUILD%-cuda}"; BUILD="${BUILD%-rocm}"
                    break
                fi ;;
        esac
    done
fi

# System RAM the engine holds: working set of the server process tree (the
# llama.cpp router spawns one child per loaded model; Strata's server runs
# its engine as a child). Null when nothing is running.
RAM_USED_GIB=""
if [[ -n "${SRV_PID}" ]]; then
    if [[ "${OS}" == windows ]]; then
        RAM_USED_GIB="$(SRV_PID="${SRV_PID}" powershell.exe -NoProfile -Command '
            $ids = @(([int]$env:SRV_PID)); $all = @()
            while ($ids.Count) {
                $n = $ids[0]; $ids = @($ids | Select-Object -Skip 1)
                if (Get-Process -Id $n -ErrorAction SilentlyContinue) { $all += $n }
                $ids += @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$n" |
                          Select-Object -ExpandProperty ProcessId)
            }
            [math]::Round(($all | ForEach-Object { (Get-Process -Id $_).WorkingSet64 } |
                           Measure-Object -Sum).Sum / 1GB, 1)' 2>/dev/null | tr -d '\r')"
    else
        rss_tree() { ps -o rss= -p "$1" 2>/dev/null; local c; for c in $(pgrep -P "$1" 2>/dev/null); do rss_tree "$c"; done; }
        RAM_USED_GIB="$(awk -v b="$(rss_tree "${SRV_PID}" | awk '{s+=$1} END{print s}')" 'BEGIN {printf "%.1f", b/1048576}')"
    fi
fi

# ------------------------------------------------------------
# Effective flags: preset [*] + [MODEL_ALIAS], or the single-model .env
# ------------------------------------------------------------

if [[ -n "${MODELS_PRESET:-}" && -f "${ROOT_DIR}/${MODELS_PRESET}" ]]; then
    FLAGS="$(awk -v want="${MODEL_ALIAS}" '
        /^\[/ { s = $0; gsub(/^\[|\].*$/, "", s); next }
        /^[[:space:]]*[;#]/ || !/=/ { next }
        s == "*" || s == want {
            k = $0; sub(/[[:space:]]*=.*/, "", k); gsub(/^[[:space:]]+/, "", k)
            v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[[:space:]]+$/, "", v)
            if (k == "model") next
            f[k] = v; if (!(k in seen)) { seen[k] = 1; order[++n] = k }
        }
        END {
            printf "{"
            for (i = 1; i <= n; i++) {
                k = order[i]; v = f[k]
                if (v !~ /^-?[0-9]+(\.[0-9]+)?$/ && v != "true" && v != "false") {
                    gsub(/\\/, "\\\\", v); gsub(/"/, "\\\"", v); v = "\"" v "\""
                }
                printf "%s\"%s\":%s", (i > 1 ? "," : ""), k, v
            }
            printf "}"
        }' "${ROOT_DIR}/${MODELS_PRESET}")"
else
    FLAGS="{\"ctx-size\":$(json_num "${CONTEXT_SIZE:-}"),\"extra\":$(json_str "${EXTRA_ARGS:-}")}"
fi

# ------------------------------------------------------------
# Metrics
# ------------------------------------------------------------

if [[ "${KIND}" == bench ]]; then
    ACCEPT="$(result_field draft_accept | tr -d '%')"
    [[ -n "${ACCEPT}" && "${ACCEPT}" != "-" ]] && ACCEPT="$(awk -v a="${ACCEPT}" 'BEGIN {printf "%.2f", a/100}')"
    METRICS="\"decode_best\":$(json_num "$(result_field decode_best)"),\"decode_median\":$(json_num "$(result_field decode_median)"),\"draft_accept\":$(json_num "${ACCEPT}"),\"prefill\":$(json_num "$(result_field prefill)")"
else
    TOOLS=FAIL
    grep -q '^\[ OK \] tool calling' "${OUT_FILE}" && TOOLS=PASS
    NEEDLE_KEY="$(grep '^RESULT' "${OUT_FILE}" | tr ' ' '\n' | grep -o '^long_ctx_[0-9]*' | tail -n1)"
    NEEDLE_TOKENS="$(grep -o 'needle : prompt=[0-9]*' "${OUT_FILE}" | grep -o '[0-9]*$' | tail -n1)"
    METRICS="\"eval\":{\"tools\":\"${TOOLS}\",\"needle\":$( [[ -n "${NEEDLE_KEY}" ]] && json_str "$(result_field "${NEEDLE_KEY}")" || echo null ),\"needle_tokens\":$(json_num "${NEEDLE_TOKENS}"),\"needle_prefill\":$(json_num "$(result_field prompt_tok_s)"),\"depth_decode\":$(json_num "$(result_field gen_tok_s_at_depth)"),\"decode_best\":$(json_num "$(result_field gen_tok_s_best)")}"
fi

mkdir -p "$(dirname "${RESULTS}")"
printf '{"ts":"%s","kind":"%s","host":%s,"gpu":%s,"os":"%s","mode":"%s","engine":%s,"engine_build":%s,"model":%s,"model_file":%s,"flags":%s,%s,"vram_mib":%s,"ram_used_gib":%s,"ram_free_gib":%s,"quiet":%s,"note":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${KIND}" "$(json_str "${HOST_SLUG}")" "$(json_str "${GPU}")" \
    "${OS}" "${DEPLOY_MODE:-native}" "$(json_str "${ENGINE:-llama.cpp}")" "$(json_str "${BUILD:-unknown}")" \
    "$(json_str "${MODEL_ALIAS}")" "$(json_str "${MODEL_FILE_SERVED:-${MODEL_FILE:-}}")" "${FLAGS}" "${METRICS}" \
    "$(json_num "${VRAM_USED}")" "$(json_num "${RAM_USED_GIB}")" "$(json_num "${RAM_FREE_GB}")" "${QUIET}" "$(json_str "${NOTE}")" >> "${RESULTS}"

echo "[ OK ] recorded in bench/results.jsonl (${KIND}, ${MODEL_ALIAS}, quiet=${QUIET})"
