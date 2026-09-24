# Running the inference server natively on Windows

The docker mode runs llama.cpp inside Docker Desktop's WSL2 VM. On a
Windows host you can also run `llama-server.exe` directly — same model,
same `.env`, same API on the same port.

## Measured on the reference machine (2026-09-24, Qwen3.6 Q4_K_M + MTP, 262K ctx)

| | docker (WSL2) | native Windows |
|---|---|---|
| decode, empty ctx | 37-47 t/s | **57-61 t/s** |
| decode @ ~205K ctx | ~15-20 t/s | **36.7 t/s** |
| model load | ~90 s (9p bind mount) | **0.3 s** (NTFS + mmap) |
| 200K needle / tool calls | PASS | PASS |

The +50% decode is direct CUDA without WSL's paravirtualized GPU: batch-1 decode
is kernel-launch-bound, exactly where the VM boundary costs. Note the Windows
binary was the *newest* build (b11149) while docker had to be rolled back to
b10991 for the 200K needle — the regression is Linux-build-specific, and each
platform still gets gated by `eval-model.sh --long 200000` before adoption.

## When to prefer native

- **No vmmem monolith.** The WSL VM holds every byte Linux touched (locked
  model + page cache, ~22 GB on the current config) and only returns memory
  under pressure *inside the VM*, so Windows can starve while the VM looks
  fine. Natively, page cache is ordinary Windows standby memory that yields
  instantly; only the locked weights stay pinned. Expect a few GB more
  effective free RAM.
- **No VM to crash.** The failure mode observed on this machine (host memory
  pressure killing the whole Docker VM, every container down) disappears;
  worst case is one process to restart.
- **No Docker Desktop** (~1 GB overhead, one less autostart).
- Direct CUDA instead of the WSL paravirtualized GPU: a few percent more
  speed. Stability, not raw tok/s, is the argument.

Docker stays the default; native is a drop-in for the same `.env`.

## Usage

```powershell
powershell -ExecutionPolicy Bypass -File scripts\start-windows.ps1
# ... later:
powershell -ExecutionPolicy Bypass -File scripts\stop-windows.ps1
```

The start script downloads the official llama.cpp CUDA build on first run
(into `.local\bin-windows\`), then launches with the exact flags compose
uses — `SPEC_TYPE` / `DRAFT_FILE` (MTP), `EXTRA_ARGS` (`--n-cpu-moe`, ...),
KV quant, load mode — and waits for `/health`. Only one server can hold
`LOCAL_AI_PORT`: stop the docker inference (`./golem down`) first, or the
script refuses to start.

## LAN exposure

Bind `0.0.0.0` via `LOCAL_AI_HOST` (docker mode publishes the port for
you; native does not). Open it once, in an admin PowerShell:

```powershell
New-NetFirewallRule -DisplayName "Golem inference" -Direction Inbound `
  -LocalPort 8000 -Protocol TCP -Action Allow
```

Clients use `http://<pc-LAN-IP>:8000` with the same `LOCAL_AI_API_KEY`.

## Auto-start (optional)

`schtasks /create /tn "Golem inference" /tr "powershell -ExecutionPolicy Bypass -File C:\path\to\slave\scripts\start-windows.ps1" /sc onlogon /rl highest`

or drop a shortcut to the script in `shell:startup`.

## Notes

- **mlock on Windows** maps to `VirtualLock`, which is subject to working
  set limits: with the whole model pinned you may see lock warnings in the
  log. `LOAD_MODE="auto"` in `.env` degrades gracefully (pages trim and
  re-read from disk) if that bothers you.
- The model file is read from `data\models\` on NTFS directly — faster
  boots than the 9p bind mount, and the official release needs no NVIDIA
  runtime install (the CUDA runtime ships with it).
- Switching back: `scripts\stop-windows.ps1`, then `./golem up --docker`.
