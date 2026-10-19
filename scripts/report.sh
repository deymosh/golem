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
# Usage: ./scripts/report.sh [--no-open]
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="${ROOT_DIR}/bench/results.jsonl"
TEMPLATE="${ROOT_DIR}/bench/report.template.html"
OUT="${ROOT_DIR}/bench/report.html"

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
