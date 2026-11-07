# Running on Windows

Golem runs the official `llama-server.exe` straight on Windows: no Docker, no WSL, no Git
Bash, no PowerShell on the path. `golem up` resolves the command line, installs the pinned
build if it is missing, fetches the model the preset needs, spawns the server, writes the pid
and waits for `/health` — all of it in the binary, which is also why a piped
`golem up | cat` returns in a second instead of holding the pipe for the engine's lifetime.

The container is still there for a Linux host or a machine you want to hand the whole stack
to ([../Dockerfile](../Dockerfile), [../compose.yml](../compose.yml)); on a Windows desktop
it is the slower path, and this page is why.

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
tuning (`-ub 2048`, see [models.md](models.md)), which applies inside a container too.

The other reasons to run natively:

- **No vmmem monolith.** The WSL VM holds every byte Linux touched (locked model + page
  cache, ~22 GB on this config) and only returns memory under pressure *inside the VM*, so
  Windows can starve while the VM looks fine. Natively, page cache is ordinary Windows
  standby memory; only the locked weights stay pinned.
- **No VM to crash.** Host memory pressure killing the whole Docker VM (observed on this
  machine) disappears; worst case is one process to restart.
- **The engine is a child of the service**, so `golem down` and `golem service stop` take it
  with them, and a killed run cannot leave a model holding the card.

## First run bootstraps itself

Nothing to set up on a fresh machine. `golem up` writes `golem.toml` beside the binary from
the compiled-in template if it is not there, installs the pinned llama.cpp build, and then
fetches the files the default model needs and `data/models/` lacks — the ones listed in
the `; Download: <repo> -> <file>` comments above the default section (`MODEL_ALIAS`; see
[config/models.ini](../config/models.ini)). Other presets only print a warning when missing —
fetch one with `golem pull <preset>`. Downloads go straight to Hugging Face's resolve
endpoint with `curl`, resume across interrupted runs, and the default coder with its
vision projector is ~22.6 GB. After that, `up` only fetches what is missing.

## Engines and pinned builds

`ENGINE` in `golem.toml` picks the inference engine (default `llama.cpp`); its
[config/engines/&lt;name&gt;.ini](../config/engines/llama.cpp.ini) names the GitHub release
source, the pinned build and the engine's capabilities. `golem up` (or `golem install`)
installs that build into `.local\engines\<name>\<build>-<backend>\` — a `BUILD` marker is
written only once every archive extracted, so an interrupted install reinstalls — and keeps
older builds side by side, so switching back is instant.

Builds are not interchangeable ([engines-and-models.md](engines-and-models.md#builds)), so
to upgrade: set `ENGINE_BUILD = "b<new>"`, `golem restart`, and keep it only if
`golem eval --long 200000` passes; then bump `build` in the adapter. `"latest"` follows
upstream ungated. `golem up --dry-run` prints the server command line without starting
anything, and `golem install` puts the engine on disk without starting it at all.

llama.cpp b11379 is the default engine; ik_llama.cpp and Strata were measured against it
on this box and lost, with the numbers in [engines-and-models.md](engines-and-models.md).

## The VRAM cliff

When a CUDA allocation exceeds dedicated VRAM, the Windows driver does not fail: it spills
into shared system memory over PCIe, silently. Symptom: prefill drops ~40% (820 → ~500 t/s)
with VRAM reading ~11.8-11.9 of 12 GB. The desktop's own VRAM use drifts (0.5-1.6 GB idle
measured the same day; browsers and video players add more), which is why the default keeps
headroom — the shipped preset lets the engine's fit place the experts with a 1 GiB margin
until the machine is tuned, and the router's tuned default
(`data/tuned/models.effective.ini`) runs `n-cpu-moe 35` at 11.2 GB. If you see the symptom,
raise `--n-cpu-moe` by 1-2, or re-run `golem tune` on a quiet desktop: the sweep measures the
cliff as a prefill collapse and will not pick a split that crosses it.

To make the cliff an error instead of a slowdown: NVIDIA Control Panel → Manage 3D settings →
Program settings → `llama-server.exe` → **CUDA - Sysmem Fallback Policy: Prefer No Sysmem
Fallback**. Startup then fails loudly when the config does not fit.

## LAN exposure

Set `LOCAL_AI_HOST = "0.0.0.0"` in `golem.toml` to bind every interface; the default
`127.0.0.1` keeps it local. A container publishes its port instead, which is why
`compose.yml` sets `GOLEM_LOCAL_AI_HOST=0.0.0.0` for it rather than changing the file. If LAN
clients can't connect, open the port once, in an admin PowerShell:

```powershell
New-NetFirewallRule -DisplayName "Golem inference" -Direction Inbound `
  -LocalPort 8000 -Protocol TCP -Action Allow
```

Clients use `http://<pc-LAN-IP>:8000` with the same `LOCAL_AI_API_KEY`.

## Auto-start (optional)

The service is what owns the engine and starts it on the way in, so it is what a logon task
should run:

```
schtasks /create /tn "Golem inference" /tr "\"C:\path\to\golem.exe\" service --up" /sc onlogon /rl highest
```

or drop a shortcut to `golem.exe service --up` in `shell:startup`.

## Notes

- **mlock on Windows** maps to `VirtualLock`, subject to working set limits: with the whole
  model pinned you may see lock warnings in the log, which the console reads as "mlock could
  not pin N GiB". `load-mode = auto` in a preset section degrades gracefully (pages trim and
  re-read from disk) if that bothers you. llama.cpp b11879 and b11936 cannot load under
  `mlock` at all; `mmap+mlock` pins the same way and loads
  ([engines-and-models.md](engines-and-models.md#builds)).
- **mlock and a host buffer over 4 GiB.** On a backend whose host weight buffer is split
  into chunks - Vulkan, on b11379 - a model with more than 4 GiB of weights in RAM (the 35B
  with `n-cpu-moe 36`) aborts on load under `load-mode = mlock`:
  `llama-mmap.cpp:804: GGML_ASSERT(addr) failed`, exit code `-1073740791` (0xC0000409). The
  engine locks the buffer by its base address, and a split buffer has none; upstream locks
  each chunk after b11379 (PR #30926). The abort is the same on the reference box's 3060
  under the Vulkan build as on an RX 7800 XT. `load-mode = mmap+mlock` loads and pins the
  whole mapping (21.7 GB for the 35B), so it needs the RAM to hold it, and the adapter
  serves it in `mlock`'s place on Vulkan (`served.vulkan.load-mode.mlock = mmap+mlock`):
  the engine reads `data/tuned/models.served.ini`, a tune's gate included. The CUDA build
  loads the same section under `mlock` on the reference box, which is why the shipped
  preset keeps it. Should a load abort anyway, the service names the assert when it stops
  the engine, and the console shows the line as "engine aborted (assert)".
- The server writes its own log (`--log-file logs\llama-server.log`), and the binary starts it
  with its stdout and stderr closed rather than inherited — a child that inherits the caller's
  pipes hangs anything reading `golem up`'s output.
- The official release needs no NVIDIA runtime install (the CUDA runtime ships with it). The
  container image installs `libgomp1`, which is the one library neither vendor base carries.
- `golem eval` and `golem bench` speak HTTP to the socket themselves: no python3, no shell,
  no WSL hop. `LOCAL_AI_HOST` / `LOCAL_AI_BASE_URL` decide where they look, and an `https://`
  base URL is refused rather than half-honoured, because the client has no TLS.
