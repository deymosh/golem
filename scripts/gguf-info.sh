#!/usr/bin/env bash

set -Eeu
export LC_ALL=C

# ============================================================
# GGUF INFO
# ============================================================
#
# Reads a GGUF file's header - metadata and the tensor table - and prints
# what the model is: architecture, layer/expert counts, trained context, and
# the parameter counts (total, and the active ones for a mixture of experts,
# where only the routed experts that fire on a token count). Nothing is
# loaded: only the header is read, so it is instant even for a 100 GB file,
# and it needs nothing but od and awk.
#
# The header is walked as a byte stream, because a tokenizer vocabulary
# makes the metadata several megabytes - far too much to hold in an awk
# array. Memory stays constant whatever the model ships.
#
# Two traps, both of which silently corrupt the parse:
#   - od collapses runs of identical lines into "*" (the token-type array is
#     nothing but runs of them), so it must run with -v;
#   - the header is far longer than any fixed guess (6 MB for a 150k-token
#     vocabulary), so it is streamed rather than read with -N.
#
# Usage: ./scripts/gguf-info.sh <file.gguf>
#        GGUF_INFO_DEBUG=1 ...    trace the walk on stderr
#
# Output (shell-friendly key=value; empty when the file does not say):
#   arch  name  n_layer  n_embd  n_ctx_train  n_expert  n_expert_used
#   moe  params_total  params_active  mtp_tensors  tensors
#
# No pipefail on purpose: awk exits as soon as it has parsed the header,
# which closes the pipe and kills od with SIGPIPE (141) - expected, not an
# error.
# ============================================================

FILE="${1:-}"
[[ -n "${FILE}" && -f "${FILE}" ]] || { echo "usage: gguf-info.sh <file.gguf>" >&2; exit 1; }

# Upper bound on what may be read: the header only (od stops early anyway).
MAXBYTES="${GGUF_INFO_BYTES:-134217728}"

# Parsing a 150k-token vocabulary takes a few seconds, and the answer never
# changes for a given file: cache it under data/cache, keyed by the file's
# name, size and mtime, so the recorder stays fast.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE_DIR="${ROOT_DIR}/data/cache/gguf-info"
SIZE="$(stat -c '%s' "${FILE}" 2>/dev/null || stat -f '%z' "${FILE}")"
MTIME="$(stat -c '%Y' "${FILE}" 2>/dev/null || stat -f '%m' "${FILE}")"
CACHE="${CACHE_DIR}/$(basename "${FILE}").${SIZE}.${MTIME}.txt"
if [[ -f "${CACHE}" ]]; then cat "${CACHE}"; exit 0; fi

INFO="$(od -An -v -tu1 -N "${MAXBYTES}" "${FILE}" | awk -v debug="${GGUF_INFO_DEBUG:-0}" '
    # ---- byte source: one od line at a time, no buffering of the file ----
    function refill(   i, k) {
        if (getline line <= 0) { eof = 1; return 0 }
        k = split(line, f, " ")
        for (i = 1; i <= k; i++) q[i] = f[i]
        qh = 1; qt = k
        return 1
    }
    function nb() { pos++; return (qh > qt && !refill()) ? -1 : q[qh++] }
    function ru(n,   i, x, m, byte) {
        x = 0; m = 1
        for (i = 0; i < n; i++) { byte = nb(); if (byte < 0) return -1; x += byte * m; m *= 256 }
        return x
    }
    function rstr(   len, i, s, c) {
        len = ru(8); if (len < 0) return ""
        s = ""
        for (i = 0; i < len; i++) { c = nb(); if (c < 0) return s; s = s sprintf("%c", c) }
        return s
    }
    # Sizes of the fixed-width GGUF value types.
    function vsize(t) {
        if (t == 2 || t == 3) return 2
        if (t == 4 || t == 5 || t == 6) return 4
        if (t == 10 || t == 11 || t == 12) return 8
        return 1                      # u8, i8, bool
    }
    # Consume a value without keeping it (arrays included).
    function skipval(t,   et, cnt, i, size) {
        if (t == 8) { skip_string(); return }
        if (t != 9) { if (vsize(t) == 1) { nb(); return } ; ru(vsize(t)); return }
        et = ru(4); cnt = ru(8)
        if (cnt < 0) return
        if (debug) printf "  array et=%d cnt=%d pos=%d\n", et, cnt, pos > "/dev/stderr"
        if (et == 8) {
            for (i = 0; i < cnt; i++) {
                skip_string()
                if (debug && i < 400) printf "  elem %d len=%d pos=%d\n", i, lastlen, pos > "/dev/stderr"
            }
            return
        }
        size = vsize(et) * cnt
        while (size-- > 0) if (nb() < 0) return
    }
    function skip_string(   len, i) {
        len = ru(8); if (len < 0) return
        for (i = 0; i < len; i++) if (nb() < 0) return
    }

    BEGIN {
        qh = 1; qt = 0                     # empty queue: qh > qt
        magic = ""
        for (i = 0; i < 4; i++) magic = magic sprintf("%c", nb())
        if (magic != "GGUF") { print "error=not a GGUF file"; exit 1 }
        version = ru(4)
        ntensors = ru(8)
        nkv = ru(8)

        for (k = 0; k < nkv; k++) {
            key = rstr()
            t = ru(4)
            if (t == 8)                       { val = rstr(); kv[key] = val }
            else if (t == 4 || t == 5)        { kv[key] = ru(4) }
            else if (t == 10 || t == 11)      { kv[key] = ru(8) }
            else if (t == 2 || t == 3)        { kv[key] = ru(2) }
            else if (t == 0 || t == 1 || t == 7) { kv[key] = nb() }
            else                              { skipval(t) }
            if (debug) printf "kv %d key=%s type=%d pos=%d\n", k, key, t, pos > "/dev/stderr"
        }

        total = 0; routed = 0; mtp = 0
        for (k = 0; k < ntensors; k++) {
            name = rstr()
            nd = ru(4)
            cnt = 1
            for (d = 0; d < nd; d++) cnt *= ru(8)
            ru(4)                                   # quant type
            ru(8)                                   # offset
            total += cnt
            # llama.cpp names routed experts blk.N.ffn_{gate,up,down}_exps;
            # a shared expert is "..._shexp" and runs on every token.
            if (name ~ /_exps\./) routed += cnt
            if (name ~ /\.nextn\./) mtp += cnt
        }

        arch = kv["general.architecture"]
        n_expert = kv[arch ".expert_count"] + 0
        n_expert_used = kv[arch ".expert_used_count"] + 0

        active = total
        if (n_expert > 1 && n_expert_used > 0)
            active = total - routed + routed * (n_expert_used / n_expert)

        printf "arch=%s\n", arch
        printf "name=%s\n", kv["general.name"]
        printf "n_layer=%d\n", kv[arch ".block_count"]
        printf "n_embd=%d\n", kv[arch ".embedding_length"]
        printf "n_ctx_train=%d\n", kv[arch ".context_length"]
        printf "n_expert=%d\n", n_expert
        printf "n_expert_used=%d\n", n_expert_used
        printf "moe=%s\n", (n_expert > 1 ? "yes" : "no")
        printf "params_total=%d\n", total
        printf "params_active=%d\n", active
        printf "mtp_tensors=%d\n", (mtp > 0 ? 1 : 0)
        printf "tensors=%d\n", ntensors
    }
'
)"


printf "%s\n" "${INFO}"

# Cache it: the answer never changes for a given file.
mkdir -p "${CACHE_DIR}"
printf "%s\n" "${INFO}" > "${CACHE}"
