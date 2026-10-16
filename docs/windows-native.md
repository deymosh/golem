# Running the inference server natively on Windows

The docker mode runs llama.cpp inside Docker Desktop's WSL2 VM. On a Windows host,
`DEPLOY_MODE="windows"` runs the official `llama-server.exe` directly instead — same model,
same `.env`, same flags, same API on the same port. **It is the recommended mode on Windows.**

## Measured on the reference machine (2026-09-24, Qwen3.6 Q4_K_M + MTP)

| | docker (WSL2), 262K | native Windows, 231K tuned |
|---|---|---|
| decode, empty ctx | 37-47 t/s | **63 t/s** |
| decode @ ~210K ctx | ~15-20 t/s | **31-37 t/s** |
| prefill, ~30K prompt | not measured | **~820 t/s** |
| prefill, full 212K prompt | 336 t/s | **642 t/s** |
| model load | ~90 s (9p bind mount) | **~25 s** (NTFS + mlock) |
| 200K needle / tool calls | PASS | PASS |

Decode gains come from direct CUDA without WSL's paravirtualized GPU: batch-1 decode is
kernel-launch-bound, exactly where the VM boundary costs. Prefill gains are mostly the
tuning (`-ub 2048`, see [models.md](models.md#setup-a--12-gb-vram-gpu--32-gb-ram-windows-host-native-deploy_modewindows)),
which applies to docker too.

Other reasons to prefer it:

- **No vmmem monolith.** The WSL VM holds every byte Linux touched (locked model + page
  cache, ~22 GB on this config) and only returns memory under pressure *inside the VM*, so
  Windows can starve while the VM looks fine. Natively, page cache is ordinary Windows
  standby memory; only the locked weights stay pinned.
- **No VM to crash.** Host memory pressure killing the whole Docker VM (observed on this
  machine) disappears; worst case is one process to restart.
- **No Docker Desktop** needed for inference.

## Usage

From Git Bash:

```bash
# DEPLOY_MODE="windows" in .env, or pass --windows to each command
./golem up          # installs llama.cpp on first run, starts, waits for /health
./golem status
./golem logs        # tail logs/llama-server-windows.log
./golem bench       # speed
./golem eval --long 200000   # quality gate (python3 via WSL, see below)
./golem down
```

Or straight from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\start-windows.ps1
powershell -ExecutionPolicy Bypass -File scripts\stop-windows.ps1
```

The start script launches with the exact flags compose uses — `SPEC_TYPE` / `DRAFT_FILE`
(MTP), `EXTRA_ARGS` (`--n-cpu-moe`, `-ub`, ...), KV quant, load mode — and waits for
`/health`. Only one server can hold `LOCAL_AI_PORT`: stop the docker inference
(`./golem down --docker`) first, or the script refuses to start.

## First run bootstraps itself

Nothing to set up on a fresh clone: `golem.cmd` creates `.env` from `.env.example`
(flipped to `DEPLOY_MODE="windows"` — this launcher *is* windows mode), and the start
script then downloads the pinned llama.cpp build **and every model file the config
references but `data\models\` lacks** — `MODEL_FILE` / `DRAFT_FILE`, or in router mode
each file listed in the preset's `; Download: <repo> -> <file>` comments (see
[config/models.ini](../config/models.ini)). Downloads go straight to Hugging Face's
resolve endpoint with `curl.exe` (resume across interrupted runs included); expect the
default coder + draft to take a while (~22 GB). After that, `up` only fetches what's
missing. The bash `./golem up` does the same on Linux, macOS, WSL and Git Bash via
[download-model.sh](../scripts/download-model.sh) (curl-based, no python needed).

## Pinned llama.cpp build

The start script installs the official CUDA release named by `LLAMA_WIN_BUILD` (default
`b11149`, which passed the 200K needle and tool calls here) into `.local\bin-windows\`, and
reinstalls if the pin changes. llama.cpp builds are not interchangeable (b11149's *Linux*
build fails the 200K needle), so to upgrade: set `LLAMA_WIN_BUILD="b<new>"`,
`./golem restart`, and keep it only if `./golem eval --long 200000` passes. `"latest"`
follows upstream ungated.

## The VRAM cliff

When a CUDA allocation exceeds dedicated VRAM, the Windows driver does not fail: it spills
into shared system memory over PCIe, silently. Symptom: prefill drops ~40% (820 → ~500 t/s)
with VRAM reading ~11.8-11.9 of 12 GB. The desktop's own VRAM use drifts (0.5-1.6 GB idle
measured the same day; browsers and video players add more), which is why the default keeps
~0.7 GB headroom (`--n-cpu-moe 36`). If you see the symptom, raise `--n-cpu-moe` by 1-2.

To make the cliff an error instead of a slowdown: NVIDIA Control Panel → Manage 3D settings →
Program settings → `llama-server.exe` → **CUDA - Sysmem Fallback Policy: Prefer No Sysmem
Fallback**. Startup then fails loudly when the config does not fit.

## Evaluating from Windows

`eval-model.sh` needs python3; Git Bash usually has none, so `./golem eval` runs it inside
WSL. WSL's default NAT network cannot see the Windows loopback, so the script falls back to
WSL's default gateway (= the Windows host). That requires the server to listen beyond
loopback: `LOCAL_AI_HOST="0.0.0.0"`. `./golem bench` needs only curl + awk and runs from
Git Bash directly.

## LAN exposure

Bind `0.0.0.0` via `LOCAL_AI_HOST` (docker mode publishes the port for you; native does
not). If LAN clients can't connect, open the port once, in an admin PowerShell:

```powershell
New-NetFirewallRule -DisplayName "Golem inference" -Direction Inbound `
  -LocalPort 8000 -Protocol TCP -Action Allow
```

Clients use `http://<pc-LAN-IP>:8000` with the same `LOCAL_AI_API_KEY`.

## Auto-start (optional)

`schtasks /create /tn "Golem inference" /tr "powershell -ExecutionPolicy Bypass -File C:\path\to\slave\scripts\start-windows.ps1" /sc onlogon /rl highest`

or drop a shortcut to the script in `shell:startup`.

## Notes

- **mlock on Windows** maps to `VirtualLock`, subject to working set limits: with the whole
  model pinned you may see lock warnings in the log. `LOAD_MODE="auto"` in `.env` degrades
  gracefully (pages trim and re-read from disk) if that bothers you.
- The server writes its own log (`--log-file logs\llama-server-windows.log`) rather than
  having PowerShell redirect it: redirection makes the child inherit the caller's pipes, which
  hangs anything reading `./golem up`'s output.
- The official release needs no NVIDIA runtime install (the CUDA runtime ships with it).
- Switching back: `./golem down --windows`, then `./golem up --docker`.
