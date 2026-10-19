#!/usr/bin/env bash

set -Eeuo pipefail

# Locale-proof: bash printf %f and awk reject "." as a decimal point under
# e.g. a Spanish locale, which would corrupt every measured number.
export LC_ALL=C

# ============================================================
# SPEED BENCHMARK
# ============================================================
#
# Assumes the inference server is already running (./golem up).
# Measures the two speeds that matter for an agentic coding client:
#
#   decode  - code generation tok/s, best + median of N runs, with the
#             MTP draft acceptance rate when speculative decoding is on
#   prefill - prompt processing tok/s on a ~30K-token prompt, the size
#             of Claude Code's first request (system prompt + tools)
#
# Numbers come from the server's own per-request `timings` (llama.cpp),
# so HTTP and queueing are excluded. Prompt caching is disabled per
# request. Needs only curl + awk: runs from Git Bash, WSL or Linux.
# Quality (tool calls, long-context needle) lives in eval-model.sh.
#
# Usage: ./scripts/benchmark-ai.sh [--runs N] [--prefill TOKENS]
#        --prefill 0 skips the prefill test.
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
MODEL_ALIAS="${MODEL_ALIAS:-local-model}"

RUNS=3
PREFILL_TOKENS=30000
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --prefill) PREFILL_TOKENS="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

# Always localhost, even when the server binds 0.0.0.0 for the LAN.
URL="http://127.0.0.1:${PORT}/v1/chat/completions"
BODY="$(mktemp)"
trap 'rm -f "${BODY}"' EXIT

request() {
    # request -> response JSON for the body in $BODY
    curl --silent --show-error --fail --max-time 1800 --noproxy '*' \
        "${URL}" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary "@${BODY}"
}

timing() {
    # timing <field> < response -> numeric value from "timings"
    # " *" tolerates pretty-printed JSON (Strata writes `"key": 12.3`,
    # llama.cpp writes `"key":12.3`).
    grep -o "\"$1\": *[0-9.eE+-]*" | head -n1 | cut -d: -f2 | tr -d ' '
}

echo
echo "============================================================"
echo " SPEED BENCHMARK  (${URL})"
echo "============================================================"

curl --silent --fail --max-time 5 --noproxy '*' "http://127.0.0.1:${PORT}/health" >/dev/null \
    || { echo "[FAIL] server not responding on :${PORT}"; exit 1; }

cat > "${BODY}" <<EOF
{"model": "${MODEL_ALIAS}", "max_tokens": 16,
 "messages": [{"role": "user", "content": "hi"}],
 "chat_template_kwargs": {"enable_thinking": false}}
EOF
request >/dev/null || { echo "[FAIL] warmup request failed (API key?)"; exit 1; }

# ------------------------------------------------------------
# Decode
# ------------------------------------------------------------

echo
echo "decode: ${RUNS} runs x 600 tokens of code generation"

SPEEDS=()
DRAFT_N=0; DRAFT_OK=0
for i in $(seq 1 "${RUNS}"); do
    cat > "${BODY}" <<EOF
{"model": "${MODEL_ALIAS}", "max_tokens": 600, "seed": $((41 + i)), "cache_prompt": false,
 "messages": [{"role": "user", "content": "Write a complete Python module implementing an LRU cache with TTL expiry, thread safety, a decorator interface, type hints and docstrings, followed by pytest unit tests covering eviction, expiry and concurrency. Output only code."}],
 "chat_template_kwargs": {"enable_thinking": false}}
EOF
    OUT="$(request)"
    TPS="$(timing predicted_per_second <<<"${OUT}")"
    DN="$(timing draft_n <<<"${OUT}")"; DN="${DN:-0}"
    DA="$(timing draft_n_accepted <<<"${OUT}")"; DA="${DA:-0}"
    DRAFT_N=$((DRAFT_N + DN)); DRAFT_OK=$((DRAFT_OK + DA))
    printf "  run %d: %6.1f tok/s  (%s tokens)\n" "${i}" "${TPS}" "$(timing predicted_n <<<"${OUT}")"
    SPEEDS+=("${TPS}")
done

read -r BEST MEDIAN < <(printf '%s\n' "${SPEEDS[@]}" | sort -g \
    | awk '{v[NR]=$1} END {m = (NR%2) ? v[(NR+1)/2] : (v[NR/2]+v[NR/2+1])/2; printf "%.1f %.1f\n", v[NR], m}')
ACCEPT="-"
[[ "${DRAFT_N}" -gt 0 ]] && ACCEPT="$(awk -v a="${DRAFT_OK}" -v n="${DRAFT_N}" 'BEGIN {printf "%.0f%%", 100*a/n}')"
echo "  best ${BEST} / median ${MEDIAN} tok/s, draft acceptance ${ACCEPT}"

# ------------------------------------------------------------
# Prefill
# ------------------------------------------------------------

PREFILL="-"
if [[ "${PREFILL_TOKENS}" -gt 0 ]]; then
    echo
    echo "prefill: ~${PREFILL_TOKENS}-token code prompt"
    # Synthetic, JSON-safe Python (no quotes/backslashes), ~2.75 chars/token.
    awk -v chars="$((PREFILL_TOKENS * 11 / 4))" -v model="${MODEL_ALIAS}" 'BEGIN {
        printf "{\"model\": \"%s\", \"max_tokens\": 1, \"cache_prompt\": false, ", model
        printf "\"chat_template_kwargs\": {\"enable_thinking\": false}, "
        printf "\"messages\": [{\"role\": \"user\", \"content\": \"Summarize this module:\\n"
        for (n = 0; len < chars; n++) {
            s = sprintf("def handler_%d(request, ctx):\\n    value = ctx.get(key_%d, %d) * %d\\n    if value > %d:\\n        return process(request, value, retries=%d)\\n    return None\\n\\n", n, n, n, (n*7)%13, (n*3)%97, n%5)
            printf "%s", s; len += length(s)
        }
        printf "\"}]}"
    }' > "${BODY}"
    OUT="$(request)"
    PREFILL="$(awk -v v="$(timing prompt_per_second <<<"${OUT}")" 'BEGIN {printf "%.0f", v}')"
    echo "  ${PREFILL} tok/s  ($(timing prompt_n <<<"${OUT}") tokens)"
fi

VRAM="-"
command -v nvidia-smi >/dev/null 2>&1 \
    && VRAM="$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -n1 | tr -d ' \r')MiB"

echo
echo "RESULT decode_best=${BEST} decode_median=${MEDIAN} draft_accept=${ACCEPT} prefill=${PREFILL} vram=${VRAM}"
