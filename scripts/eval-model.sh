#!/usr/bin/env bash

set -Eeuo pipefail

# ============================================================
# MODEL EVALUATION HARNESS
# ============================================================
#
# Assumes the inference server is already running (./golem up).
# Runs the same battery for every candidate model:
#
#   1. health + model identity
#   2. arithmetic sanity (detects quant damage / CUDA gibberish)
#   3. tool calling (OpenAI tools API round-trip)
#   4. throughput: 3 timed runs, prompt tok/s + decode tok/s via /metrics
#   5. optional needle-in-haystack: --long TOKENS
#
# Usage: ./scripts/eval-model.sh [--long tokens]
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi

HOST="${LOCAL_AI_HOST:-127.0.0.1}"
PORT="${LOCAL_AI_PORT:-8000}"
API_KEY="${LOCAL_AI_API_KEY:-local-key}"
MODEL_ALIAS="${MODEL_ALIAS:-local-model}"
TEMPERATURE="${TEMPERATURE:-0.15}"
TOP_P="${TOP_P:-0.95}"

BASE_URL="http://127.0.0.1:${PORT}"

LONG_TOKENS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --long) LONG_TOKENS="${2:-32768}"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

if ! command -v python3 >/dev/null 2>&1; then
    echo "[ERROR] python3 is required" >&2
    exit 1
fi

METRICS_URL="${BASE_URL}/metrics"

metric() {
    # metric <name> -> value from the Prometheus endpoint (exact field match)
    # /metrics is protected by the API key like every other endpoint.
    curl --silent --max-time 5 \
        -H "Authorization: Bearer ${API_KEY}" \
        "${METRICS_URL}" \
        | awk -v n="$1" '$1 == n {v=$2} END {print v+0}'
}

section() {
    echo
    echo "------------------------------------------------------------"
    echo " $*"
    echo "------------------------------------------------------------"
}

# ============================================================
# 1. HEALTH + IDENTITY
# ============================================================

section "1. HEALTH + IDENTITY"

curl --silent --show-error --fail --max-time 5 "${BASE_URL}/health" >/dev/null \
    || { echo "[FAIL] server not responding"; exit 1; }
echo "[ OK ] /health"

curl --silent -H "Authorization: Bearer ${API_KEY}" "${BASE_URL}/props" \
    | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
    if isinstance(d, dict):
        print("model_path :", d.get("model_path","?"))
        print("n_ctx      :", d.get("default_generation_settings",{}).get("n_ctx","?"))
    else:
        print("props      : (unexpected shape)")
except Exception as e:
    print("props      : (unavailable)")' || true

# ============================================================
# 2. WARMUP + ARITHMETIC SANITY
# ============================================================

section "2. WARMUP + ARITHMETIC SANITY"

echo "warmup request (first hit may compile CUDA graphs, be patient)..."
curl --silent --show-error --fail --max-time 900 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d '{"model": "'"${MODEL_ALIAS}"'", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8}' \
    > /dev/null || echo "[WARN] warmup request failed (continuing)"
echo "[ OK ] warmup done"

# Prefer thinking disabled (Qwen3.x hybrid models accept the kwarg);
# fall back to a plain body for models whose template rejects it.
SANITY_PLAIN='{
  "model": "'"${MODEL_ALIAS}"'",
  "messages": [{"role": "user", "content": "What is 17*23? Reply with the number only."}],
  "temperature": 0,
  "max_tokens": 512
}'

SANITY_NOTHINK='{
  "model": "'"${MODEL_ALIAS}"'",
  "messages": [{"role": "user", "content": "What is 17*23? Reply with the number only."}],
  "chat_template_kwargs": {"enable_thinking": false},
  "temperature": 0,
  "max_tokens": 512
}'

parse_msg() { python3 -c 'import json,sys; m=json.load(sys.stdin)["choices"][0]["message"]; print(m.get("content") or m.get("reasoning_content") or "")'; }

SANITY_OUT="$(echo "${SANITY_NOTHINK}" | curl --silent --show-error --max-time 600 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d @- | parse_msg 2>/dev/null || true)"

if [[ -z "${SANITY_OUT}" ]]; then
    SANITY_OUT="$(echo "${SANITY_PLAIN}" | curl --silent --show-error --max-time 600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        -d @- | parse_msg || true)"
fi

[[ -z "${SANITY_OUT}" ]] && SANITY_OUT="(request failed)"

echo "answer: ${SANITY_OUT}"
if echo "${SANITY_OUT}" | grep -q "391"; then
    echo "[ OK ] arithmetic sanity"
else
    echo "[FAIL] arithmetic sanity (expected 391) - quant damage / bad CUDA?"
fi

# ============================================================
# 3. TOOL CALLING
# ============================================================

section "3. TOOL CALLING"

TOOL_BODY='{
  "model": "'"${MODEL_ALIAS}"'",
  "messages": [{"role": "user", "content": "What is the weather in Munich right now? Use the provided tool."}],
  "tools": [{
    "type": "function",
    "function": {
      "name": "get_weather",
      "description": "Get current weather for a city",
      "parameters": {
        "type": "object",
        "properties": {
          "city": {"type": "string", "description": "City name"},
          "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
        },
        "required": ["city"]
      }
    }
  }],
  "tool_choice": "auto",
  "temperature": 0,
  "max_tokens": 1024
}'

TOOL_OUT="$(echo "${TOOL_BODY}" | curl --silent --show-error --max-time 600 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d @- || true)"

if [[ -z "${TOOL_OUT}" ]]; then
    # retry once appending the kwarg (some templates only emit tool calls
    # when thinking is disabled)
    TOOL_OUT="$(echo "${TOOL_BODY}" | sed 's/"tool_choice": "auto",/"tool_choice": "auto", "chat_template_kwargs": {"enable_thinking": false},/' \
        | curl --silent --show-error --max-time 600 \
            "${BASE_URL}/v1/chat/completions" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer ${API_KEY}" \
            -d @- || true)"
fi

if [[ -z "${TOOL_OUT}" ]]; then
    echo "[FAIL] tool request returned no response"
else
    echo "${TOOL_OUT}" | python3 - <<'PYEOF' || true
import json, sys
d = json.load(sys.stdin)
msg = d["choices"][0]["message"]
tc = msg.get("tool_calls") or []
if tc and tc[0]["function"]["name"] == "get_weather":
    try:
        args = json.loads(tc[0]["function"]["arguments"])
        ok = args.get("city", "").lower() == "munich"
        print(f"tool_call : get_weather({args})")
        print("[ OK ] tool calling" if ok else "[WARN] tool called with wrong city")
    except json.JSONDecodeError as e:
        print(f"[FAIL] tool arguments are not valid JSON: {e}")
else:
    print(f"[FAIL] no tool call emitted; content={msg.get('content','')[:200]!r}")
PYEOF
fi

# ============================================================
# 4. THROUGHPUT (3 timed runs)
# ============================================================

section "4. THROUGHPUT (3 runs, ~600-token prompt, 192 gen tokens)"

RUNS=3
TOTAL_P=0; TOTAL_G=0; WALL=0

for i in $(seq 1 "${RUNS}"); do
    P0=$(metric 'llamacpp:prompt_tokens_total')
    G0=$(metric 'llamacpp:tokens_predicted_total')
    T0=$(date +%s.%N)

    curl --silent --show-error --fail --max-time 900 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        -d @- <<EOF > /dev/null
{
  "model": "${MODEL_ALIAS}",
  "messages": [{"role": "user", "content": "Review this Go snippet for correctness and idiomatic issues. Be concise but complete.\n\nfunc LoadConfig(paths []string) (*Config, error) {\n\tdata, _ := os.ReadFile(paths[0])\n\tvar cfg Config\n\tjson.Unmarshal(data, &cfg)\n\tif cfg.Timeout == 0 { cfg.Timeout = 30 }\n\treturn &cfg, nil\n}\n\nExplain each problem, rank by severity, then show the fixed version."}],
  "temperature": ${TEMPERATURE},
  "top_p": ${TOP_P},
  "max_tokens": 192,
  "stream": false
}
EOF

    T1=$(date +%s.%N)
    P1=$(metric 'llamacpp:prompt_tokens_total')
    G1=$(metric 'llamacpp:tokens_predicted_total')

    DP=$((P1 - P0)); DG=$((G1 - G0))
    DT=$(python3 -c "print(f'{$T1 - $T0:.2f}')")

    echo "run $i: prompt=${DP} tok, gen=${DG} tok, wall=${DT}s"
    TOTAL_P=$((TOTAL_P + DP)); TOTAL_G=$((TOTAL_G + DG)); WALL=$(python3 -c "print($WALL + $DT)")
done

python3 - "$TOTAL_P" "$TOTAL_G" "$WALL" <<'PYEOF'
import sys
p, g, w = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
if w > 0:
    print(f"RESULT prompt_tok_s={p/w:.0f} gen_tok_s={g/w:.1f} (avg of 3 runs, incl. HTTP)")
PYEOF

# ============================================================
# 5. LONG CONTEXT NEEDLE (optional)
# ============================================================

if [[ -n "${LONG_TOKENS}" ]]; then
    section "5. NEEDLE-IN-HAYSTACK (~${LONG_TOKENS} tokens)"

    P0=$(metric 'llamacpp:prompt_tokens_total')
    G0=$(metric 'llamacpp:tokens_predicted_total')
    T0=$(date +%s.%N)

    python3 - "$LONG_TOKENS" <<'PYEOF' > /tmp/golem_needle_body.json
import json, sys
n = int(sys.argv[1])
line = "The warehouse holds crates of silicon wafers and the manifests are filed by quarter.\n"
filler = line * (n * 5 // len(line))          # ~4 chars/token for this text
needle = "IMPORTANT: the release code for Q3 is stored under the passphrase 8127.\n"
half = len(filler) // 2
haystack = filler[:half] + needle + filler[half:]
body = {
    "model": "local-model",
    "messages": [{"role": "user", "content":
        "Below is a pile of warehouse log lines. Somewhere inside there is one IMPORTANT line. "
        "What is the passphrase in it? Reply with the number only.\n\n" + haystack}],
    "temperature": 0,
    "max_tokens": 16,
    "stream": False,
}
json.dump(body, sys.stdout)
PYEOF

    NEEDLE_OUT="$(curl --silent --show-error --fail --max-time 3600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary @/tmp/golem_needle_body.json \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["choices"][0]["message"]["content"])')"

    T1=$(date +%s.%N)
    P1=$(metric 'llamacpp:prompt_tokens_total')
    G1=$(metric 'llamacpp:tokens_predicted_total')

    DP=$((P1 - P0)); DG=$((G1 - G0))
    DT=$(python3 -c "print(f'{$T1 - $T0:.2f}')")

    echo "prompt=${DP} tok in ${DT}s -> $(python3 -c "print(f'{$DP/$DT:.0f}')") prompt tok/s; answer: ${NEEDLE_OUT}"
    if echo "${NEEDLE_OUT}" | grep -q "8127"; then
        echo "[ OK ] needle found"
        echo "RESULT long_ctx_${LONG_TOKENS}=PASS prompt_tok_s=$(python3 -c "print(f'{$DP/$DT:.0f}')")"
    else
        echo "[FAIL] needle missed"
        echo "RESULT long_ctx_${LONG_TOKENS}=FAIL prompt_tok_s=$(python3 -c "print(f'{$DP/$DT:.0f}')")"
    fi
    rm -f /tmp/golem_needle_body.json
fi

# ============================================================
# DONE
# ============================================================

echo
echo "============================================================"
echo " EVAL COMPLETE - ${MODEL_ALIAS}"
echo "============================================================"
