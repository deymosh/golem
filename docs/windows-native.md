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
fetches every model file the configuration references and `data/models/` lacks —
`MODEL_FILE` / `DRAFT_FILE`, or in router mode the files listed in the `; Download: <repo> ->
<file>` comments above the default section (`MODEL_ALIAS`; see
[config/models.ini](../config/models.ini)). Other presets only print a warning when missing —
fetch one with `golem pull <preset>`. Downloads go straight to Hugging Face's resolve
endpoint with `curl`, resume across interrupted runs, and the default coder plus its draft
head are ~22 GB. After that, `up` only fetches what is missing.

## Engines and pinned builds

`ENGINE` in `golem.toml` picks the inference engine (default `llama.cpp`); its
[engines/&lt;name&gt;/engine.ini](../engines/llama.cpp/engine.ini) names the GitHub release
source, the pinned build and the engine's capabilities. `golem up` (or `golem install`)
installs that build into `.local\engines\<name>\<build>-<backend>\` — a `BUILD` marker is
written only once every archive extracted, so an interrupted install reinstalls — and keeps
older builds side by side, so switching back is instant.

Builds are not interchangeable (b11149's *Linux* build failed the 200K needle), so to
upgrade: set `ENGINE_BUILD = "b<new>"`, `golem restart`, and keep it only if
`golem eval --long 200000` passes; then bump `build` in the engine.ini. `"latest"` follows
upstream ungated. `golem up --dry-run` prints the server command line without starting
anything, and `golem install` puts the engine on disk without starting it at all.

| Engine | Status | Notes |
|---|---|---|
| `llama.cpp` (b11379) | **default, adopted** | router mode, `/v1/messages`, MTP |
| `ik_llama.cpp` (Thireus main-b5483) | **rejected** 2026-10-19 | 16.7-16.9 t/s decode vs llama.cpp 58.1 on the same model and flags (3.4x slower): its `--fit` fills to 12.0 GB (sysmem cliff) and the default split is worse; `-rtr` repack crashes at startup; `mlock` of the 17.7 GB buffer fails; no `/v1/messages`, no router, no idle unload |
| `strata` (v0.1.45) | **rejected, and unsupported** 2026-10-19 | cannot serve 200K on 12 GB: its VRAM cost per context token is ~57 KB (measured: 32K→64K costs 1.80 GiB) against llama.cpp's ~12.7 KB q8 KV, so 200K needs ~11.5 GB for context alone and the engine reports "0 MiB free" at 236K. Where it runs (32768 ctx) it gives 49.8 t/s vs llama.cpp 58.5 at the same context. Its adapter says it installs itself with its own shell launcher, and the shell is gone, so `golem` refuses it by name |

Engine gate, 2026-10-19, quiet desktop, all rows in `bench/results.jsonl`:

| Engine | Model | ctx | decode best/median | prefill | VRAM |
|---|---|---|---|---|---|
| llama.cpp b11379 | Ornith-1.5-35B Q4_K_M | 236544 | 59.6 / 58.1 | 836 | 11.1 GB |
| llama.cpp b11379 | Ornith-1.5-35B Q4_K_M | 32768 | 59.4 / 58.5 | 823 | 6.6 GB |
| strata v0.1.45 | Qwen3.6-35B UD-Q4_K_XL | 32768 | 50.6 / 49.8 | 657 | 11.7 GB |
| strata v0.1.45 | Qwen3.6-35B UD-Q4_K_XL | 236544 | impossible | – | – |
| ik_llama.cpp main-b5483 | Ornith-1.5-35B Q4_K_M | 236544 | 18.8 / 16.7 | 479 | 12.0 GB |

A future build of either rejected engine is one `ENGINE_BUILD` change plus a paired bench and
the 200K eval away from being reconsidered — for `ik_llama.cpp`. Strata would need its
installer ported first.

## The VRAM cliff

When a CUDA allocation exceeds dedicated VRAM, the Windows driver does not fail: it spills
into shared system memory over PCIe, silently. Symptom: prefill drops ~40% (820 → ~500 t/s)
with VRAM reading ~11.8-11.9 of 12 GB. The desktop's own VRAM use drifts (0.5-1.6 GB idle
measured the same day; browsers and video players add more), which is why the default keeps
headroom — the single-model path asks for `--n-cpu-moe 36`, and the router's tuned default
(`data/tuned/models.effective.ini`, 2026-10-20) runs 35 at 11.2 GB. If you see the symptom,
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
  model pinned you may see lock warnings in the log. `LOAD_MODE = "auto"` in `golem.toml`
  degrades gracefully (pages trim and re-read from disk) if that bothers you.
- The server writes its own log (`--log-file logs\llama-server.log`), and the binary starts it
  with its stdout and stderr closed rather than inherited — a child that inherits the caller's
  pipes hangs anything reading `golem up`'s output.
- The official release needs no NVIDIA runtime install (the CUDA runtime ships with it). The
  container image installs `libgomp1`, which is the one library neither vendor base carries.
- `golem eval` and `golem bench` speak HTTP to the socket themselves: no python3, no shell,
  no WSL hop. `LOCAL_AI_HOST` / `LOCAL_AI_BASE_URL` decide where they look, and an `https://`
  base URL is refused rather than half-honoured, because the client has no TLS.
