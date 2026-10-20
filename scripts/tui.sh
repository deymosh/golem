#!/usr/bin/env bash

set -Eeu
export LC_ALL=C

# ============================================================
# GOLEM TUI
# ============================================================
#
# The default face of Golem in a terminal: live state, the actions you take
# every day, the runs you just measured, and the engine's log.
#
#   ./golem            (no arguments, in a terminal: this)
#   ./golem tui
#   golem.cmd          (no arguments: this; menu item T as well)
#
# Works with no server and no model loaded. The terminal is put in raw mode
# and restored on every exit path (q, Ctrl+C, error, resize). When stdin is
# not a terminal it prints one frame and exits, so it can be piped.
#
# Keys:  up/down or j/k  move    PgUp/PgDn  scroll the runs/log
#        enter  run    mouse wheel  scroll    r  refresh    q  quit
#
# Implementation notes:
#  - raw mode (stty -icanon -echo) is what stops arrow keys from being
#    echoed as ^[[A: without it the terminal driver prints the bytes it
#    cannot interpret, which is exactly the artefact that shows up at the
#    prompt after quitting;
#  - the frame is ASCII: bash pads and cuts by bytes, so box-drawing
#    characters would be the wrong width in some locales;
#  - every fact (health, VRAM, RAM) is cached for a couple of seconds: each
#    one is a subprocess, and running three per frame made the menu lag;
#  - scrolls and resizes are handled without redrawing more than once.
# ============================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

if [[ -f "${ROOT_DIR}/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "${ROOT_DIR}/.env"
    set +a
fi
PORT="${LOCAL_AI_PORT:-8000}"
ALIAS="${MODEL_ALIAS:-local-model}"
ENGINE="${ENGINE:-llama.cpp}"

B=$'\033[1m'; DIM=$'\033[2m'; GREEN=$'\033[32m'; RED=$'\033[31m'; ACC=$'\033[33m'; R=$'\033[0m'

TTY=0
[[ -t 0 && -t 1 ]] && TTY=1
TEST="${GOLEM_TUI_TEST:-0}"        # read keys from stdin even when piped
INTERACTIVE=0
if (( TTY )) || (( TEST == 1 )); then INTERACTIVE=1; fi

# ------------------------------------------------------------
# Terminal: raw mode, alternate screen, mouse
# ------------------------------------------------------------

STTY_SAVED=""
enter_screen() {
    if (( TTY )); then
        STTY_SAVED="$(stty -g 2>/dev/null || true)"
        stty -icanon -echo 2>/dev/null || true
        printf '\033[?1049h\033[?25l\033[?1000h\033[?1006h'   # alt screen, no cursor, wheel
    fi
}
leave_screen() {
    if (( TTY )); then
        printf '\033[?1006l\033[?1000l\033[?25h\033[?1049l'
        [[ -n "${STTY_SAVED}" ]] && stty "${STTY_SAVED}" 2>/dev/null || true
        stty sane 2>/dev/null || true
    fi
}
cleanup() { leave_screen; }
trap cleanup EXIT INT TERM

term_size() {
    local rows="${GOLEM_TUI_ROWS:-}" cols="${GOLEM_TUI_COLS:-}"
    if [[ -z "${rows}" || -z "${cols}" ]]; then
        local s
        s="$(stty size 2>/dev/null || echo "")"
        [[ -n "${s}" ]] && read -r rows cols <<<"${s}"
    fi
    ROWS="${rows:-24}"; COLS="${cols:-80}"
    if (( ROWS < 10 )); then ROWS=10; fi
    if (( COLS < 40 )); then COLS=40; fi
    W=$(( COLS - 1 ))
    if (( W > 110 )); then W=110; fi
}

# ------------------------------------------------------------
# Facts (cached: each is a subprocess)
# ------------------------------------------------------------

CACHE_AT=0; RAM_AT=0; CACHE_UP=""; CACHE_VRAM=""; CACHE_RAM=""; CACHE_WEB=""
WEB_PID_FILE="${ROOT_DIR}/.local/golem-web.pid"

# The console is a separate process; the TUI shows whether it is up and can
# start or stop it. web.ps1 writes its pid so this can find it again, and so
# it can be matched in Task Manager (Details tab, PID column).
web_up() { curl -sf --noproxy "*" --connect-timeout 0.2 -m 0.4 http://127.0.0.1:8787/api/state >/dev/null 2>&1; }
web_start() {
    if web_up; then return 0; fi
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${ROOT_DIR}/scripts/web.ps1" -Port 8787 >/dev/null 2>&1 &
    for _ in $(seq 1 20); do
        curl -sf --noproxy "*" --connect-timeout 0.3 -m 1 http://127.0.0.1:8787/api/state >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    return 1
}
web_stop() {
    local pid
    [[ -f "${WEB_PID_FILE}" ]] || return 0
    pid="$(tr -d ' \r\n' < "${WEB_PID_FILE}")"
    taskkill.exe //T //F //PID "${pid}" >/dev/null 2>&1 || kill "${pid}" 2>/dev/null || true
    rm -f "${WEB_PID_FILE}"
    return 0
}
refresh_facts() {
    local now; now=$(date +%s)
    if (( now - CACHE_AT >= 2 )); then
        CACHE_AT=${now}
        # Short connect timeout: with no server the default waits a full
        # second on every refresh, and the menu then feels broken.
        if curl -sf --noproxy '*' --connect-timeout 0.2 -m 0.4 \
            "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
            CACHE_UP="up"
        else
            CACHE_UP="down"
        fi
        CACHE_VRAM="$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits 2>/dev/null | head -n1 | tr -d ' \r')"
        if curl -sf --noproxy "*" --connect-timeout 0.2 -m 0.4 http://127.0.0.1:8787/api/state >/dev/null 2>&1; then CACHE_WEB="up"; else CACHE_WEB="down"; fi
    fi
    if (( now - RAM_AT >= 10 )); then
        RAM_AT=${now}
        CACHE_RAM="$(awk '/^MemAvailable|^MemFree/ {v=$2} END {printf "%.0f", v/1048576}' /proc/meminfo 2>/dev/null)"
        if [[ -z "${CACHE_RAM}" ]]; then
            # PowerShell costs ~350 ms: once every ten seconds is fine.
            CACHE_RAM="$(powershell.exe -NoProfile -Command \
                '[math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB,0)' 2>/dev/null | tr -d '\r' | tr ',' '.')"
        fi
    fi
}

# ------------------------------------------------------------
# Panes
# ------------------------------------------------------------

json_val() { grep -o "\"$1\": *\"[^\"]*\"" <<<"$2" | head -n1 | cut -d'"' -f4; }
json_num() { grep -o "\"$1\": *[0-9.]*" <<<"$2" | head -n1 | awk -F': *' '{print $2}'; }

# One awk pass for every row: four greps per row meant ~50 processes per
# refresh, and the menu visibly lagged on Windows.
load_runs() {
    RUN_LINES=()
    local l
    while IFS= read -r l; do RUN_LINES+=("${l}"); done < <(
        tail -n 30 bench/results.jsonl 2>/dev/null | grep '^{' | tail -n 12 |
        awk '
            function jstr(line, key,   s) {
                if (match(line, "\"" key "\": *\"[^\"]*\"")) {
                    s = substr(line, RSTART, RLENGTH)
                    sub("^\"" key "\": *\"", "", s); sub("\"$", "", s)
                    return s
                }
                return ""
            }
            function jnum(line, key,   s) {
                if (match(line, "\"" key "\": *-?[0-9.]+")) {
                    s = substr(line, RSTART, RLENGTH)
                    sub("^\"" key "\": *", "", s)
                    return s
                }
                return ""
            }
            {
                kind = jstr($0, "kind"); model = jstr($0, "model"); eng = jstr($0, "engine")
                dec = jnum($0, "decode_median"); pact = jnum($0, "params_active_b")
                model = (length(model) > 26) ? substr(model, 1, 26) : model
                if (kind == "reject")
                    printf "  reject  %-26s %s\n", model, eng
                else
                    printf "%7s t/s  %-26s %5sB act  %s\n", (dec == "" ? "-" : dec), model, (pact == "" ? "-" : pact), eng
            }'
    )
}

LOG_LINES=()
load_log() {
    LOG_LINES=()
    local f
    f="$(ls -t logs/*.log 2>/dev/null | head -n1)"
    [[ -n "${f}" ]] || { LOG_LINES=("  (no log yet)"); return; }
    while IFS= read -r l; do
        LOG_LINES+=("${l}")
    done < <(tail -n 40 "${f}" 2>/dev/null | sed 's/^\[[0-9]*\] //; s/^[0-9.]* [A-Z] [a-zA-Z_]* *//' \
             | tr -cd '[:print:]\n')
}

ACTIONS=(
    "Start / stop the engine|toggle"
    "Bench (speed)|bench"
    "Eval (quality gate)|eval --long 200000"
    "Tune (engine + flags)|tune"
    "Models|models"
    "Console (web UI)|web"
    "Results page|report"
    "Logs|logs"
)
SEL=0
RUN_OFF=0
LOG_OFF=0

# ------------------------------------------------------------
# Drawing
# ------------------------------------------------------------

# No forks per line: ${s:0:W} truncates in-process (cut would be a process
# per row, and a frame has ~30 rows).
bar() { printf '%*s' "${W}" '' | tr ' ' "$1"; }
fit() { printf '%s' "${1:0:${W}}"; }
BAR_EQ=""; BAR_DASH=""; BAR_W=-1
build_bars() {
    (( W == BAR_W )) && return 0
    BAR_W=${W}
    BAR_EQ="$(bar '=')"
    BAR_DASH="$(bar '-')"
    return 0
}

draw() {
    term_size
    build_bars
    refresh_facts

    # The header is composed from segments and dropped from the right when
    # the window is narrow: truncating a string that already carries colour
    # escapes would cut at the wrong place (cut counts the escapes).
    local head_plain="" head_col="" seg_p seg_c
    seg_p="▟█▙ GOLEM"; seg_c="${B}${seg_p}${R}"
    head_plain+="${seg_p}"; head_col+="${seg_c}"
    if [[ "${CACHE_UP}" == "up" ]]; then seg_p="  ● up"; seg_c="  ${GREEN}● up${R}${R}"
    else seg_p="  ○ down"; seg_c="  ${RED}○ down${R}"; fi
    head_plain+="${seg_p}"; head_col+="${seg_c}"
    local cand_p cand_c
    cand_p="   ${CACHE_VRAM:-?} MiB VRAM"; cand_c="   ${cand_p}"
    if (( ${#head_plain} + ${#cand_p} <= W )); then head_plain+="${cand_p}"; head_col+="${cand_c}"; fi
    cand_p="   ${ALIAS}"; cand_c="   ${B}${ALIAS}${R}"
    if (( ${#head_plain} + ${#cand_p} <= W )); then head_plain+="${cand_p}"; head_col+="${cand_c}"; fi
    cand_p="  [${ENGINE}]"; cand_c="  ${DIM}[${ENGINE}]${R}"
    if (( ${#head_plain} + ${#cand_p} <= W )); then head_plain+="${cand_p}"; head_col+="${cand_c}"; fi
    if [[ "${CACHE_WEB}" == "up" ]]; then seg_p="  web up"; seg_c="  ${GREEN}web up${R}"
    else seg_p="  web down"; seg_c="  ${DIM}web down${R}"; fi
    if (( ${#head_plain} + ${#seg_p} <= W )); then head_plain+="${seg_p}"; head_col+="${seg_c}"; fi
    cand_p="   ${CACHE_RAM:-?} GB free"; cand_c="   ${cand_p}"
    if (( ${#head_plain} + ${#cand_p} <= W )); then head_plain+="${cand_p}"; head_col+="${cand_c}"; fi

    local out=""
    out+=$'\033[H'
    out+="${BAR_EQ}"$'\n'
    out+="  ${head_col}"$'\n'
    out+="${BAR_DASH}"$'\n'

    # The action list is windowed when the screen is short: the frame must
    # never be taller than the terminal, or the top scrolls away.
    local nact=${#ACTIONS[@]}
    local fixed=$(( 3 + 2 + 1 ))                 # bars, hint, header chrome
    local room=$(( ROWS - fixed ))
    (( room < 3 )) && room=3
    local astart=0 ashow=${nact}
    if (( nact > room )); then
        ashow=$(( room - 1 ))                    # one row for the "more" marker
        astart=$(( SEL - ashow / 2 ))
        if (( astart < 0 )); then astart=0; fi
        if (( astart > nact - ashow )); then astart=$(( nact - ashow )); fi
    fi
    local i
    for (( i = astart; i < astart + ashow; i++ )); do
        local label="${ACTIONS[$i]%%|*}"
        if (( i == SEL )); then out+="$(fit "  ${ACC}▸ ${label}${R}")"$'\n'
        else out+="$(fit "    ${label}")"$'\n'; fi
    done
    if (( ashow < nact )); then out+="$(fit "    ${DIM}… $( (( astart > 0 )) && echo "↑" )$( (( astart + ashow < nact )) && echo "↓" )${R}")"$'\n'; fi
    out+="${BAR_DASH}"$'\n'

    # Panes shrink with the window: actions first, then the log, then runs.
    local used=$(( 3 + ${#ACTIONS[@]} + 2 ))
    local avail=$(( ROWS - used - 2 ))          # minus hint line and padding
    local show_log=0 show_runs=0
    if (( avail >= 4 )); then show_runs=1; fi
    if (( avail >= 9 )); then
        show_log=1
        if (( avail - 4 < 3 )); then show_log=0; fi
    fi
    local run_h=0 log_h=0
    if (( show_runs )); then
        if (( show_log )); then run_h=$(( (avail - 4) / 2 )); else run_h=$(( avail - 1 )); fi
    fi
    if (( run_h > 4 )); then run_h=4; fi
    if (( run_h < 0 )); then run_h=0; fi
    if (( show_log )); then log_h=$(( avail - 2 - run_h )); fi
    if (( log_h > 6 )); then log_h=6; fi
    if (( log_h < 0 )); then log_h=0; fi

    if (( show_runs )); then
        out+="$(fit "  ${DIM}recent runs${R}")"$'\n'
        local n=${#RUN_LINES[@]}
        if (( RUN_OFF > n - run_h )); then RUN_OFF=$(( n > run_h ? n - run_h : 0 )); fi
        if (( RUN_OFF < 0 )); then RUN_OFF=0; fi
        for (( j = RUN_OFF; j < n && j < RUN_OFF + run_h; j++ )); do
            out+="$(fit "${RUN_LINES[$j]}")"$'\n'
        done
        for (( j = n; j < RUN_OFF + run_h; j++ )); do out+="$(fit "")"$'\n'; done
    fi
    if (( show_log )); then
        out+="$(fit "  ${DIM}engine log${R}")"$'\n'
        local n=${#LOG_LINES[@]}
        if (( LOG_OFF > n - log_h )); then LOG_OFF=$(( n > log_h ? n - log_h : 0 )); fi
        if (( LOG_OFF < 0 )); then LOG_OFF=0; fi
        for (( j = LOG_OFF; j < n && j < LOG_OFF + log_h; j++ )); do
            out+="$(fit "  ${LOG_LINES[$j]}")"$'\n'
        done
        for (( j = n; j < LOG_OFF + log_h; j++ )); do out+="$(fit "")"$'\n'; done
    fi

    out+="${BAR_DASH}"$'\n'
    out+="$(fit "  ${DIM}up/down move   enter run   wheel/PgUp scroll   r refresh   q quit${R}")"
    printf '%s' "${out}"
}

# ------------------------------------------------------------
# Actions
# ------------------------------------------------------------

show_models() {
    echo "Presets in config/models.ini:"
    awk '/^\[/ { s=$0; gsub(/^\[|\].*/,"",s); if (s != "*") printf "  %-28s ", s }
         /^model *=/ { sub(/^model *= */,""); print }' config/models.ini
    echo
    echo "Files in data/models:"
    ls -1sh data/models/*.gguf 2>/dev/null | sed 's/^/  /' || echo "  (none)"
}

run_action() {
    local what="$1"
    leave_screen
    case "${what}" in
        toggle)
            if [[ "${CACHE_UP}" == "up" ]]; then ./golem down; else ./golem up; fi ;;
        models) show_models ;;
        logs)
            local f; f="$(ls -t logs/*.log 2>/dev/null | head -n1)"
            if [[ -n "${f}" ]]; then
                echo "Following ${f} - Ctrl+C returns to the menu"; echo
                tail -n 25 -f "${f}" 2>/dev/null || true
            else
                echo "no log yet"
            fi ;;
        web)
            if web_up; then
                echo "Console is up on http://127.0.0.1:8787/ (pid $(tr -d ' \r\n' < "${WEB_PID_FILE}" 2>/dev/null))"
                printf 'stop it? [y/N] '
                read -rs answer || answer=""
                [[ "${answer}" == "y" || "${answer}" == "Y" ]] && { web_stop; echo "stopped"; } || echo "left running"
            else
                echo "Starting the console..."
                if web_start; then echo "Console: http://127.0.0.1:8787/"; else echo "could not start it (see logs/)"; fi
            fi ;;
        *)  # shellcheck disable=SC2086
            ./golem ${what} ;;
    esac
    echo
    if (( INTERACTIVE )); then
        printf 'press enter to return '
        read -rs _ || true
    fi
    CACHE_AT=0          # the action changed things: refresh on return
    load_runs; load_log
    enter_screen
}

# ------------------------------------------------------------
# Main loop
# ------------------------------------------------------------

load_runs
load_log
enter_screen
CLAMP() {
    if (( SEL < 0 )); then SEL=0; fi
    if (( SEL > ${#ACTIONS[@]} - 1 )); then SEL=$(( ${#ACTIONS[@]} - 1 )); fi
}
DRAWN=0

while true; do
    draw
    DRAWN=1
    (( INTERACTIVE )) || break

    key=""
    IFS= read -rsn1 key || break

    case "${key}" in
        "")
            run_action "${ACTIONS[$SEL]#*|}" ;;
        $'\e')
            # ESC [ ... : arrows, page keys, and SGR mouse reports.
            seq=""
            IFS= read -rsn1 -t 0.2 seq || true
            if [[ "${seq}" == "[" ]]; then
                rest=""
                IFS= read -rsn1 -t 0.2 rest || true
                if [[ "${rest}" == "<" ]]; then
                    # SGR mouse: <b;x;yM with b=64 wheel up, 65 wheel down.
                    m=""
                    while IFS= read -rsn1 -t 0.05 c && [[ "${c}" != [Mm] ]]; do m+="${c}"; done
                    btn="${m%%;*}"
                    case "${btn}" in
                        # "|| true": (( )) reports failure when its result is
                        # 0, and under set -e that would quit the TUI on the
                        # scroll that lands on offset 0.
                        64) RUN_OFF=$(( RUN_OFF - 1 )); LOG_OFF=$(( LOG_OFF - 1 )) ;;
                        65) RUN_OFF=$(( RUN_OFF + 1 )); LOG_OFF=$(( LOG_OFF + 1 )) ;;
                    esac
                    if (( RUN_OFF < 0 )); then RUN_OFF=0; fi
                    if (( LOG_OFF < 0 )); then LOG_OFF=0; fi
                else
                    case "${rest}" in
                        A) SEL=$(( SEL - 1 )); CLAMP ;;
                        B) SEL=$(( SEL + 1 )); CLAMP ;;
                        # PgUp/PgDn arrive as "5~" / "6~"
                        5) RUN_OFF=$(( RUN_OFF > 3 ? RUN_OFF - 3 : 0 )); LOG_OFF=$(( LOG_OFF > 3 ? LOG_OFF - 3 : 0 )) ;;
                        6) RUN_OFF=$(( RUN_OFF + 3 )); LOG_OFF=$(( LOG_OFF + 3 )) ;;
                        H) SEL=0 ;;
                        F) SEL=$(( ${#ACTIONS[@]} - 1 )) ;;
                    esac
                fi
            fi ;;
        q|Q) break ;;
        k) SEL=$(( SEL - 1 )); CLAMP ;;
        j) SEL=$(( SEL + 1 )); CLAMP ;;
        K) RUN_OFF=$(( RUN_OFF > 2 ? RUN_OFF - 2 : 0 )); LOG_OFF=$(( LOG_OFF > 2 ? LOG_OFF - 2 : 0 )) ;;
        J) RUN_OFF=$(( RUN_OFF + 2 )); LOG_OFF=$(( LOG_OFF + 2 )) ;;
        r|R) CACHE_AT=0; load_runs; load_log ;;
        h|H) : ;;
    esac
done
