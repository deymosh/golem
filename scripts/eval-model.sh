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
# Needs only bash, curl and awk - the same three tools benchmark-ai.sh
# uses, so it runs identically on Windows (Git Bash), Linux and inside a
# container. It deliberately does NOT need python3 or WSL: the earlier
# python version had to be handed to WSL on Windows, and that hop is what
# silently made the tuner's gate measure the .env model instead of the
# candidate under test (the GOLEM_* override never crossed the boundary).
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
# GOLEM_MODEL_ALIAS overrides for one run (the auto-tuner gates with it).
MODEL_ALIAS="${GOLEM_MODEL_ALIAS:-${MODEL_ALIAS:-local-model}}"
TEMPERATURE="${TEMPERATURE:-0.15}"
TOP_P="${TOP_P:-0.95}"

# A server on 0.0.0.0 is reachable on the loopback; a server bound to the
# LAN needs its own address. LOCAL_AI_BASE_URL wins when set.
BASE_URL="${LOCAL_AI_BASE_URL:-http://127.0.0.1:${PORT}}"
BASE_URL="${BASE_URL%/}"
BASE_URL="${BASE_URL%/v1}"

# Scratch bodies live under data/tmp (git-ignored, and a real path in every
# environment - /tmp does not survive MSYS path translation to Windows curl).
TMP_DIR="${ROOT_DIR}/data/tmp"
mkdir -p "${TMP_DIR}"

LONG_TOKENS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --long) LONG_TOKENS="${2:-32768}"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

section() {
    echo
    echo "------------------------------------------------------------"
    echo " $*"
    echo "------------------------------------------------------------"
}

# ------------------------------------------------------------
# JSON helpers: awk, no dependency. The server's JSON is either
# compact or pretty-printed (llama.cpp prints `timings` indented),
# so every helper first squeezes the text onto one line.
# ------------------------------------------------------------

# json_str <json> <key> -> the string value, or empty
json_str() {
    printf '%s' "$1" | tr -d '\n\r' | awk -v key="$2" '
        {
            if (match($0, "\"" key "\":[ ]*\"[^\"]*\"")) {
                s = substr($0, RSTART, RLENGTH)
                sub("^\"" key "\":[ ]*\"", "", s); sub("\"$", "", s)
                print s
            }
        }'
}

# json_num <json> <key> -> the number as written, or empty
json_num() {
    printf '%s' "$1" | tr -d '\n\r' | awk -v key="$2" '
        {
            if (match($0, "\"" key "\":[ ]*-?[0-9]+([.][0-9]+)?([eE][-+]?[0-9]+)?")) {
                s = substr($0, RSTART, RLENGTH)
                sub("^\"" key "\":[ ]*", "", s)
                print s
            }
        }'
}

# json_raw <json> <key> -> the value as written (object, array, string, number)
json_raw() {
    printf '%s' "$1" | tr -d '\n\r' | awk -v key="$2" '
        {
            i = index($0, "\"" key "\":")
            if (!i) next
            rest = substr($0, i + length(key) + 3)
            sub(/^[ ]*/, "", rest)
            if (substr(rest, 1, 1) == "{") {
                depth = 0; out = ""
                for (j = 1; j <= length(rest); j++) {
                    c = substr(rest, j, 1); out = out c
                    if (c == "{") depth++
                    else if (c == "}") { depth--; if (depth == 0) break }
                }
                print out
            } else {
                print rest
            }
        }'
}

# The assistant's text: content, or reasoning_content for thinking models.
msg_content() {
    local json="$1" body
    body="$(json_raw "${json}" "message")"
    [[ -n "${body}" ]] || body="${json}"
    local c; c="$(json_str "${body}" "content")"
    [[ -n "${c}" ]] || c="$(json_str "${body}" "reasoning_content")"
    printf '%s' "${c}"
}

# ------------------------------------------------------------
# 1. HEALTH + IDENTITY
# ------------------------------------------------------------

section "1. HEALTH + IDENTITY"

curl --silent --show-error --fail --max-time 5 "${BASE_URL}/health" >/dev/null \
    || { echo "[FAIL] server not responding"; exit 1; }
echo "[ OK ] /health"

PROPS="$(curl --silent --max-time 10 -H "Authorization: Bearer ${API_KEY}" \
    "${BASE_URL}/props?model=${MODEL_ALIAS}" 2>/dev/null || true)"
echo "model_path : $( [[ -n "$(json_str "${PROPS}" "model_path")" ]] && json_str "${PROPS}" "model_path" || echo "?")"
echo "n_ctx      : $( [[ -n "$(json_num "${PROPS}" "n_ctx")" ]] && json_num "${PROPS}" "n_ctx" || echo "?")"

# ------------------------------------------------------------
# 2. WARMUP + ARITHMETIC SANITY
# ------------------------------------------------------------

section "2. WARMUP + ARITHMETIC SANITY"

echo "warmup request (first hit may compile CUDA graphs, be patient)..."
curl --silent --show-error --fail --max-time 900 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d '{"model": "'"${MODEL_ALIAS}"'", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 8}' \
    > /dev/null || echo "[WARN] warmup request failed (continuing)"
echo "[ OK ] warmup done"

# Thinking disabled where the template accepts the kwarg (Qwen3.x hybrids);
# plain body otherwise.
SANITY_NOTHINK='{
  "model": "'"${MODEL_ALIAS}"'",
  "messages": [{"role": "user", "content": "What is 17*23? Reply with the number only."}],
  "chat_template_kwargs": {"enable_thinking": false},
  "temperature": 0,
  "max_tokens": 512
}'

SANITY_PLAIN='{
  "model": "'"${MODEL_ALIAS}"'",
  "messages": [{"role": "user", "content": "What is 17*23? Reply with the number only."}],
  "temperature": 0,
  "max_tokens": 512
}'

SANITY_RAW="$(printf '%s' "${SANITY_NOTHINK}" | curl --silent --show-error --max-time 600 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d @- || true)"

SANITY_OUT="$(msg_content "${SANITY_RAW}")"
if [[ -z "${SANITY_OUT}" ]]; then
    SANITY_RAW="$(printf '%s' "${SANITY_PLAIN}" | curl --silent --show-error --max-time 600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        -d @- || true)"
    SANITY_OUT="$(msg_content "${SANITY_RAW}")"
fi
[[ -z "${SANITY_OUT}" ]] && SANITY_OUT="(request failed)"

echo "answer: ${SANITY_OUT}"
if grep -q "391" <<<"${SANITY_OUT}"; then
    echo "[ OK ] arithmetic sanity"
else
    echo "[FAIL] arithmetic sanity (expected 391) - quant damage / bad CUDA?"
fi

# ------------------------------------------------------------
# 3. TOOL CALLING
# ------------------------------------------------------------

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

TOOL_RAW="$(printf '%s' "${TOOL_BODY}" | curl --silent --show-error --max-time 600 \
    "${BASE_URL}/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${API_KEY}" \
    -d @- || true)"

# Some templates only emit tool calls with thinking disabled.
if [[ -z "$(json_raw "${TOOL_RAW}" "tool_calls")" ]]; then
    TOOL_RAW="$(printf '%s' "${TOOL_BODY}" \
        | sed 's/"tool_choice": "auto",/"tool_choice": "auto", "chat_template_kwargs": {"enable_thinking": false},/' \
        | curl --silent --show-error --max-time 600 \
            "${BASE_URL}/v1/chat/completions" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer ${API_KEY}" \
            -d @- || true)"
fi

TOOL_CALLS="$(json_raw "${TOOL_RAW}" "tool_calls")"
if [[ -z "${TOOL_RAW}" ]]; then
    echo "[FAIL] tool request returned no response"
elif [[ -z "${TOOL_CALLS}" ]]; then
    echo "[FAIL] no tool call emitted; content=$(msg_content "${TOOL_RAW}" | head -c 200)"
elif grep -q 'get_weather' <<<"${TOOL_CALLS}" && grep -qi 'munich' <<<"${TOOL_CALLS}"; then
    echo "tool_call : $(grep -o 'get_weather' <<<"${TOOL_CALLS}" | head -n1)($(json_str "${TOOL_CALLS}" "arguments"))"
    echo "[ OK ] tool calling"
elif grep -q 'get_weather' <<<"${TOOL_CALLS}"; then
    echo "tool_call : get_weather($(json_str "${TOOL_CALLS}" "arguments"))"
    echo "[WARN] tool called with wrong city"
else
    echo "[FAIL] no tool call emitted; content=$(msg_content "${TOOL_RAW}" | head -c 200)"
fi

# ------------------------------------------------------------
# 4. THROUGHPUT (3 timed runs)
# ------------------------------------------------------------

section "4. THROUGHPUT (3 runs, code-review prompt, 192 gen tokens)"

# Per-request server timings (llama.cpp `timings`): decode and prompt
# tok/s exclude HTTP and each other. Prompt caching is off so every run
# pays its prefill. ./golem bench is the fuller speed benchmark.
timings_line() {
    local json="$1" prompt_n prompt_tps gen_n gen_tps draft_n draft_acc out
    prompt_n="$(json_num "${json}" "prompt_n")"
    prompt_tps="$(json_num "${json}" "prompt_per_second")"
    gen_n="$(json_num "${json}" "predicted_n")"
    gen_tps="$(json_num "${json}" "predicted_per_second")"
    draft_n="$(json_num "${json}" "draft_n")"
    draft_acc="$(json_num "${json}" "draft_n_accepted")"
    out="prompt=${prompt_n:-0} tok @ $(printf '%.0f' "${prompt_tps:-0}") t/s, gen=${gen_n:-0} tok @ $(printf '%.1f' "${gen_tps:-0}") t/s"
    if [[ -n "${draft_n}" && "${draft_n}" != "0" ]]; then
        out+=" draft_accept=$(awk -v a="${draft_acc:-0}" -v n="${draft_n}" 'BEGIN {printf "%.0f%%", 100 * a / n}')"
    fi
    printf '%s' "${out}"
}

GEN_SPEEDS=()
for i in $(seq 1 3); do
    OUT="$(curl --silent --show-error --max-time 900 \
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
    LINE="$(timings_line "${OUT}")"
    echo "run $i: ${LINE}"
    GEN_SPEEDS+=("$(json_num "${OUT}" "predicted_per_second")")
done
echo "RESULT gen_tok_s_best=$(printf '%s\n' "${GEN_SPEEDS[@]}" | sort -g | tail -n1)"

# ------------------------------------------------------------
# 5. LONG CONTEXT NEEDLE (optional)
# ------------------------------------------------------------

if [[ -n "${LONG_TOKENS}" ]]; then
    section "5. NEEDLE-IN-HAYSTACK (~${LONG_TOKENS} tokens) + DECODE AT DEPTH"

    # Haystack first, question last: the follow-up request below shares
    # the whole haystack as a cached prefix, which is how an agent turn
    # looks (long history, short new tail).
    #
    # The text is built by awk into a file (it is ~1 MB at 200K tokens) and
    # sized by the server's own tokenizer, not by a guessed characters-per-
    # token ratio: that ratio belongs to the model's tokenizer, and a gate
    # that asks for 200K while sending 165K is testing something else.
    BODY_NEEDLE="${TMP_DIR}/eval-needle.json"
    BODY_DEPTH="${TMP_DIR}/eval-depth.json"
    BODY_TOKENS="${TMP_DIR}/eval-tokenize.json"

    build_bodies() {
        # $1 = target tokens, $2 = characters per token (the first guess)
        awk -v target="$1" -v cpt="$2" -v model="${MODEL_ALIAS}" \
            -v needle_out="${BODY_NEEDLE}" -v depth_out="${BODY_DEPTH}" \
            -v tokens_out="${BODY_TOKENS}" '
            BEGIN {
                line = "The warehouse holds crates of silicon wafers and the manifests are filed by quarter."
                needle = "IMPORTANT: the release code for Q3 is stored under the passphrase 8127."
                reps = int(target * cpt / length(line)) + 1
                if (reps < 2) reps = 2
                half = int(reps / 2)

                head = "Below is a pile of warehouse log lines. Somewhere inside there is one IMPORTANT line."
                hay = head
                for (i = 0; i < half; i++) hay = hay "\\n" line
                hay = hay "\\n" needle
                for (i = half; i < reps; i++) hay = hay "\\n" line

                q_needle = "\\n\\nWhat is the passphrase in the IMPORTANT line? Reply with the number only."
                q_depth = "\\n\\nNow write a Python function that parses one of these log lines into a dict, with a docstring."

                common = "\"temperature\": 0, \"chat_template_kwargs\": {\"enable_thinking\": false}, \"stream\": false"
                printf "{\"model\": \"%s\", \"messages\": [{\"role\": \"user\", \"content\": \"%s%s\"}], %s, \"cache_prompt\": false, \"max_tokens\": 16}",
                    model, hay, q_needle, common > needle_out
                printf "{\"model\": \"%s\", \"messages\": [{\"role\": \"user\", \"content\": \"%s%s\"}], %s, \"cache_prompt\": true, \"max_tokens\": 256}",
                    model, hay, q_depth, common > depth_out
                # /tokenize takes the model in the body and a bare `content`,
                # not a chat request: the same text, counted by the server.
                printf "{\"model\": \"%s\", \"content\": \"%s%s\"}", model, hay, q_needle > tokens_out
                close(needle_out); close(depth_out); close(tokens_out)
            }'
    }

    # How many tokens the server sees in a request body, or nothing when the
    # engine has no /tokenize (then the estimate stands, as before). Never
    # fails: `grep -c` exits 1 on a zero count, and under `set -e` that would
    # kill the run silently - which is exactly what it did first time round.
    tokenize_count() {
        local ids
        ids="$(curl --silent --max-time 120 "${BASE_URL}/tokenize" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer ${API_KEY}" \
            --data-binary @"${BODY_TOKENS}" 2>/dev/null \
            | tr -d '\n' \
            | sed -n 's/.*"tokens": *\[\([^]]*\)\].*/\1/p' || true)"
        [[ -n "${ids}" ]] || return 0
        printf '%s' "${ids}" | tr ',' '\n' | grep -c '[0-9]' || true
    }

    build_bodies "${LONG_TOKENS}" "4.7"
    HAYSTACK_TOKENS="$(tokenize_count)"
    if [[ -n "${HAYSTACK_TOKENS}" ]] && (( HAYSTACK_TOKENS > 0 )); then
        # One correction pass against the measured count; the ratio is
        # linear enough that a second pass is noise.
        if (( HAYSTACK_TOKENS != LONG_TOKENS )); then
            build_bodies "${LONG_TOKENS}" \
                "$(awk -v cpt=4.7 -v want="${LONG_TOKENS}" -v got="${HAYSTACK_TOKENS}" 'BEGIN {printf "%.3f", cpt * want / got}')"
            HAYSTACK_TOKENS="$(tokenize_count)"
        fi
        echo "haystack: ${HAYSTACK_TOKENS} tokens (asked for ${LONG_TOKENS})"
    else
        echo "haystack: sized by the 4.7 chars/token estimate (no /tokenize on this engine)"
    fi

    NEEDLE_RAW="$(curl --silent --show-error --fail --max-time 3600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary @- < "${BODY_NEEDLE}" || true)"
    NEEDLE_OUT="$(msg_content "${NEEDLE_RAW}")"
    [[ -z "${NEEDLE_OUT}" ]] && NEEDLE_OUT="(request failed)"
    PROMPT_TPS="$(json_num "${NEEDLE_RAW}" "prompt_per_second")"
    PROMPT_N="$(json_num "${NEEDLE_RAW}" "prompt_n")"
    echo "needle : prompt=${PROMPT_N:-0} tok @ $(printf '%.0f' "${PROMPT_TPS:-0}") t/s; answer: ${NEEDLE_OUT}"

    DEPTH_RAW="$(curl --silent --show-error --fail --max-time 3600 \
        "${BASE_URL}/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer ${API_KEY}" \
        --data-binary @- < "${BODY_DEPTH}" || true)"
    DEPTH_TPS="$(json_num "${DEPTH_RAW}" "predicted_per_second")"
    echo "depth  : $(timings_line "${DEPTH_RAW}") (prefix from cache)"
    rm -f "${BODY_NEEDLE}" "${BODY_DEPTH}" "${BODY_TOKENS}"

    if grep -q "8127" <<<"${NEEDLE_OUT}"; then
        echo "[ OK ] needle found"
        VERDICT=PASS
    else
        echo "[FAIL] needle missed"
        VERDICT=FAIL
    fi
    echo "RESULT long_ctx_${LONG_TOKENS}=${VERDICT} prompt_tok_s=$(printf '%.0f' "${PROMPT_TPS:-0}") gen_tok_s_at_depth=$(printf '%.1f' "${DEPTH_TPS:-0}")"
fi

# ------------------------------------------------------------
# DONE
# ------------------------------------------------------------

echo
echo "============================================================"
echo " EVAL COMPLETE - ${MODEL_ALIAS}"
echo "============================================================"
