#!/usr/bin/env bash

set -Eeuo pipefail

# ============================================================
# BENCHMARK REPORT
# ============================================================
#
# Builds bench/report.html from bench/results.jsonl: the records are
# inlined into bench/report.template.html, so the page is one self-
# contained file that opens from disk (no server, no CDN). Same output
# as scripts/report.ps1 (used on Windows).
#
# Usage: ./scripts/report.sh [--no-open] [--md]
#   --md writes bench/RESULTS.md (GitHub renders it) instead of the page.
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="${ROOT_DIR}/bench/results.jsonl"
TEMPLATE="${ROOT_DIR}/bench/report.template.html"
OUT="${ROOT_DIR}/bench/report.html"
MD_OUT="${ROOT_DIR}/bench/RESULTS.md"

if [[ "${1:-}" == "--md" ]]; then
    # GitHub strips HTML and JavaScript from Markdown, so the interactive
    # page cannot live in the README: this is the same data as a table.
    [[ -f "${RESULTS}" ]] || { echo "[FAIL] no ${RESULTS}"; exit 1; }
    awk -v out="${MD_OUT}" '
        function jstr(line, key,   s) {
            if (match(line, "\"" key "\": *\"[^\"]*\"")) {
                s = substr(line, RSTART, RLENGTH); sub("^\"" key "\": *\"", "", s); sub("\"$", "", s); return s
            }
            return ""
        }
        function jnum(line, key,   s) {
            if (match(line, "\"" key "\": *-?[0-9.]+")) {
                s = substr(line, RSTART, RLENGTH); sub("^\"" key "\": *", "", s); return s
            }
            return ""
        }
        function clsname(p) {
            if (p == 0) return "9 unknown"
            if (p < 2) return "1 <= 2B active"
            if (p < 8) return "2 2-8B active"
            if (p < 20) return "3 8-20B active"
            return "4 20B+ active"
        }
        {
            kind = jstr($0, "kind")
            if (kind == "reject") { rej[++nr] = "| " jstr($0, "model") " | " jstr($0, "engine") " `" jstr($0, "engine_build") "` | " jstr($0, "note") " |"; next }
            if (kind == "eval") {
                nd = jstr($0, "needle")
                if (nd != "") needle[jstr($0, "model")] = nd
                next
            }
            if (kind != "bench") next
            model = jstr($0, "model"); eng = jstr($0, "engine"); build = jstr($0, "engine_build")
            ctx = jnum($0, "ctx"); dec = jnum($0, "decode_median")
            if (dec == "") next
            key = model "|" eng "|" build "|" ctx
            if (!(key in best) || dec + 0 > best[key] + 0) {
                best[key] = dec; pre[key] = jnum($0, "prefill"); vram[key] = jnum($0, "vram_mib")
                ram[key] = jnum($0, "ram_used_gib"); pact[key] = jnum($0, "params_active_b")
                ptot[key] = jnum($0, "params_b"); nnexp[key] = jstr($0, "experts")
                acc[key] = jnum($0, "draft_accept"); day[key] = substr(jstr($0, "ts"), 1, 10)
                cls[key] = clsname((pact[key] == "" ? ptot[key] : pact[key]) + 0)
                modelof[key] = model; engof[key] = eng; buildof[key] = build; ctxof[key] = ctx
                if (!(key in seen)) { seen[key] = 1; order[++n] = key }
            }
        }
        END {
            print "# Benchmark results"
            print ""
            print "Generated from [`bench/results.jsonl`](results.jsonl) by `./golem report --md`."
            print "One row per model x engine build x context, at the best decode measured for that"
            print "combination. Compare inside a size class only: a 0.5B at 269 t/s says nothing"
            print "about a 35B at 59. `t/s / active B` is the cross-size ratio (active parameters are"
            print "what the GPU pushes per token: 3.5B for a 35B-A3B)."
            print ""
            # sort by class, then decode desc
            for (i = 2; i <= n; i++) {
                v = order[i]; j = i - 1
                while (j >= 1 && (cls[order[j]] > cls[v] || (cls[order[j]] == cls[v] && best[order[j]] + 0 < best[v] + 0))) {
                    order[j + 1] = order[j]; j--
                }
                order[j + 1] = v
            }
            cur = ""
            for (i = 1; i <= n; i++) {
                k = order[i]
                name = cls[k]; sub(/^[0-9] /, "", name)
                if (name != cur) {
                    cur = name
                    print ""
                    print "## " cur
                    print ""
                    print "| model | engine | ctx | decode t/s | prefill t/s | t/s / active B | params | VRAM GB | RAM GB | MTP acc. | needle | measured |"
                    print "|---|---|---:|---:|---:|---:|---|---:|---:|---:|---|---|"
                }
                p = (pact[k] == "" ? ptot[k] : pact[k])
                ratio = (p > 0 ? sprintf("%.1f", best[k] / p) : "-")
                pstr = (ptot[k] == "") ? "-" : sprintf("%.2fB / %.2fB active", ptot[k], pact[k])
                if (nexp[k] != "") pstr = pstr " \\*" nexp[k] "\\*"
                v = (vram[k] == "") ? "-" : sprintf("%.1f", vram[k] / 1024)
                r = (ram[k] == "") ? "-" : sprintf("%.1f", ram[k])
                a = (acc[k] == "") ? "-" : sprintf("%.0f%%", acc[k] * 100)
                nd = ((modelof[k] in needle) ? needle[modelof[k]] : "-")
                ctxs = (ctxof[k] == "") ? "-" : sprintf("%.0fK", ctxof[k] / 1024)
                printf "| %s | %s `%s` | %s | **%s** | %s | %s | %s | %s | %s | %s | %s | %s |\n", \
                    modelof[k], engof[k], buildof[k], ctxs, best[k], pre[k], ratio, pstr, v, r, a, nd, day[k]
            }
            if (nr > 0) {
                print ""
                print "## Rejected, with the measured reason"
                print ""
                print "| model | engine | why |"
                print "|---|---|---|"
                for (i = 1; i <= nr; i++) print rej[i]
            }
            print ""
            print "Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl)."
        }
    ' "${RESULTS}" > "${MD_OUT}"
    echo "[ OK ] ${MD_OUT} ($(grep -c '^| ' "${MD_OUT}" 2>/dev/null || echo 0) table lines)"
    exit 0
fi

DATA="$(mktemp)"
trap 'rm -f "${DATA}"' EXIT
# "</" inside a <script> block would end it early; "<\/" is the same JSON.
{ [[ -f "${RESULTS}" ]] && grep '^[[:space:]]*{' "${RESULTS}" || true; } \
    | awk 'BEGIN {printf "["} {gsub(/<\//, "<\\/"); printf "%s%s", (NR > 1 ? ",\n" : ""), $0} END {printf "]"}' > "${DATA}"

awk -v data="${DATA}" '
    index($0, "/*GOLEM_DATA*/") {
        i = index($0, "/*GOLEM_DATA*/")
        printf "%s", substr($0, 1, i - 1)
        while ((getline line < data) > 0) printf "%s%s", (n++ ? "\n" : ""), line
        print substr($0, i + 14)
        next
    }
    { print }
' "${TEMPLATE}" > "${OUT}"

echo "[ OK ] ${OUT} ($(grep -c '^[[:space:]]*{' "${RESULTS}" 2>/dev/null || echo 0) runs)"
if [[ "${1:-}" != "--no-open" ]]; then
    if command -v xdg-open >/dev/null 2>&1; then xdg-open "${OUT}" >/dev/null 2>&1 &
    elif command -v open >/dev/null 2>&1; then open "${OUT}"
    elif command -v cmd.exe >/dev/null 2>&1; then cmd.exe /c start "" "$(cygpath -w "${OUT}" 2>/dev/null || echo "${OUT}")"
    fi
fi
