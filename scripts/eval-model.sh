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
#   4. throughput: 3 runs, prompt + decode tok/s from server timings
#   5. optional needle-in-haystack at --long TOKENS, then decode speed
#      at that depth (follow-up request reusing the cached haystack)
#
# Needs python3; on Windows `./golem eval` runs it through WSL.
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
# GOLEM_MODEL_ALIAS overrides for one run (the auto-tuner gates with it).
MODEL_ALIAS="${GOLEM_MODEL_ALIAS:-${MODEL_ALIAS:-local-model}}"
TEMPERATURE="${TEMPERATURE:-0.15}"
TOP_P="${TOP_P:-0.95}"

# EVAL_HOST: reach a server on another host. Inside WSL (default NAT
# networking) the Windows loopback is invisible, so a server running
# natively on Windows is tried at WSL's default gateway = the Windows
# host; that needs the server bound to 0.0.0.0 (LOCAL_AI_HOST).
EVAL_HOST="${EVAL_HOST:-127.0.0.1}"
if ! curl --silent --fail --max-time 3 "http://${EVAL_HOST}:${PORT}/health" >/dev/null 2>&1 \
    && grep -qi microsoft /proc/version 2>/dev/null; then
    WIN_HOST="$(ip route show default 2>/dev/null | awk '{print $3; exit}')"
    [[ -n "${WIN_HOST}" ]] && EVAL_HOST="${WIN_HOST}"
fi
BASE_URL="http://${EVAL_HOST}:${PORT}"

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

curl --silent -H "Authorization: Bearer ${API_KEY}" "${BASE_URL}/props?model=${MODEL_ALIAS}" \
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
    # The heredoc is python's stdin, so the response travels via env.
    TOOL_OUT="${TOOL_OUT}" python3 - <<'PYEOF' || true
import json, os
d = json.loads(os.environ["TOOL_OUT"])
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

section "4. THROUGHPUT (3 runs, code-review prompt, 192 gen tokens)"

# Per-request server timings (llama.cpp `timings`): decode and prompt
# tok/s exclude HTTP and each other. Prompt caching is off so every run
# pays its prefill. ./golem bench is the fuller speed benchmark.
timings() {
    python3 -c '
import json, sys
t = json.load(sys.stdin).get("timings", {})
acc = " draft_accept=%.0f%%" % (100.0 * t["draft_n_accepted"] / t["draft_n"]) if t.get("draft_n") else ""
print("prompt=%d tok @ %.0f t/s, gen=%d tok @ %.1f t/s%s" % (
    t.get("prompt_n", 0), t.get("prompt_per_second", 0),
    t.get("predicted_n", 0), t.get("predicted_per_second", 0), acc))'
}

GEN_SPEEDS=()
for i in $(seq 1 3); do
    OUT="$(curl --silent --show-error --fail --max-time 900 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        -d @- <<EOF
{
  "model": "${MODEL_ALIAS}",
  "messages": [{"role": "user", "content": "Review this Go snippet for correctness and idiomatic issues. Be concise but complete.\n\nfunc LoadConfig(paths []string) (*Config, error) {\n\tdata, _ := os.ReadFile(paths[0])\n\tvar cfg Config\n\tjson.Unmarshal(data, &cfg)\n\tif cfg.Timeout == 0 { cfg.Timeout = 30 }\n\treturn &cfg, nil\n}\n\nExplain each problem, rank by severity, then show the fixed version."}],
  "chat_template_kwargs": {"enable_thinking": false},
  "cache_prompt": false,
  "temperature": ${TEMPERATURE},
  "top_p": ${TOP_P},
  "max_tokens": 192,
  "stream": false
}
EOF
)"
    LINE="$(timings <<<"${OUT}")"
    echo "run $i: ${LINE}"
    GEN_SPEEDS+=("$(sed -n 's/.*gen=[0-9]* tok @ \([0-9.]*\) t\/s.*/\1/p' <<<"${LINE}")")
done
echo "RESULT gen_tok_s_best=$(printf '%s\n' "${GEN_SPEEDS[@]}" | sort -g | tail -n1)"

# ============================================================
# 5. LONG CONTEXT NEEDLE (optional)
# ============================================================

if [[ -n "${LONG_TOKENS}" ]]; then
    section "5. NEEDLE-IN-HAYSTACK (~${LONG_TOKENS} tokens) + DECODE AT DEPTH"

    # Haystack first, question last: the follow-up request below shares
    # the whole haystack as a cached prefix, which is how an agent turn
    # looks (long history, short new tail).
    python3 - "$LONG_TOKENS" "${MODEL_ALIAS}" <<'PYEOF'
import json, sys
n, model = int(sys.argv[1]), sys.argv[2]
line = "The warehouse holds crates of silicon wafers and the manifests are filed by quarter.\n"
filler = line * (n * 5 // len(line))          # ~4 chars/token for this text
needle = "IMPORTANT: the release code for Q3 is stored under the passphrase 8127.\n"
half = len(filler) // 2
haystack = ("Below is a pile of warehouse log lines. Somewhere inside there is one IMPORTANT line.\n\n"
            + filler[:half] + needle + filler[half:])
def body(question, max_tokens, cache):
    return {
        "model": model,
        "messages": [{"role": "user", "content": haystack + "\n" + question}],
        "temperature": 0,
        "max_tokens": max_tokens,
        # hybrid-thinking models would spend the budget reasoning
        "chat_template_kwargs": {"enable_thinking": False},
        "cache_prompt": cache,
        "stream": False,
    }
json.dump(body("What is the passphrase in the IMPORTANT line? Reply with the number only.", 16, False),
          open("/tmp/golem_needle_body.json", "w"))
json.dump(body("Now write a Python function that parses one of these log lines into a dict, with a docstring.", 256, True),
          open("/tmp/golem_depth_body.json", "w"))
PYEOF

    NEEDLE_RAW="$(curl --silent --show-error --fail --max-time 3600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary @/tmp/golem_needle_body.json || true)"
    NEEDLE_OUT="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["choices"][0]["message"]["content"])' <<<"${NEEDLE_RAW}" 2>/dev/null || echo "(request failed)")"
    PROMPT_TPS="$(python3 -c 'import json,sys; print("%.0f" % json.load(sys.stdin)["timings"]["prompt_per_second"])' <<<"${NEEDLE_RAW}" 2>/dev/null || echo 0)"
    echo "needle : $(timings <<<"${NEEDLE_RAW}" 2>/dev/null); answer: ${NEEDLE_OUT}"

    DEPTH_RAW="$(curl --silent --show-error --fail --max-time 3600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary @/tmp/golem_depth_body.json || true)"
    DEPTH_TPS="$(python3 -c 'import json,sys; print("%.1f" % json.load(sys.stdin)["timings"]["predicted_per_second"])' <<<"${DEPTH_RAW}" 2>/dev/null || echo 0)"
    echo "depth  : $(timings <<<"${DEPTH_RAW}" 2>/dev/null) (prefix from cache)"
    rm -f /tmp/golem_needle_body.json /tmp/golem_depth_body.json

    if echo "${NEEDLE_OUT}" | grep -q "8127"; then
        echo "[ OK ] needle found"
        VERDICT=PASS
    else
        echo "[FAIL] needle missed"
        VERDICT=FAIL
    fi
    echo "RESULT long_ctx_${LONG_TOKENS}=${VERDICT} prompt_tok_s=${PROMPT_TPS} gen_tok_s_at_depth=${DEPTH_TPS}"
fi

# ============================================================
# DONE
# ============================================================

echo
echo "============================================================"
echo " EVAL COMPLETE - ${MODEL_ALIAS}"
echo "============================================================"
