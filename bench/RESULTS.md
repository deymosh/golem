# Benchmark results

Generated from [`bench/results.jsonl`](results.jsonl) by `./golem report --md`.

**One section per machine.** The heading is the `host` slug the recorder wrote - GPU, VRAM,
RAM and CPU in one string, so `rtx-3060_12g_32g_i7-12700kf` is a 12 GB RTX 3060 with 32 GB
of RAM and an i7-12700KF - and machines never share a table: the same flags on another box
are that box's numbers, not a comparison.

Inside a machine, one block per model, and inside that one line per context: the best setup
measured at that context, with the flags that won named on the row. The tuning that lost is
still in [`results.jsonl`](results.jsonl) and in the console. Compare inside a size class
only - a 0.5B at 269 t/s says nothing about a 35B at 59 - and `t/s / active B` is the one
honest cross-size ratio (active parameters are what the GPU pushes per token: 3.5B for a
35B-A3B).

## rtx-3060_12g_32g_i7-12700kf

NVIDIA GeForce RTX 3060 · windows

### qwen2.5-0.5b-instruct — 0.49B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 8K | ik_llama.cpp `main-b5483-3d9321e` | n-cpu-moe=0 n-gpu-layers=99 | **66.7** | 7812 | 136.1 | 4.7 | 0.6 | - | PASS | 2026-10-19 |
| 8K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=0 n-gpu-layers=99 | **317.6** | 19704 | 648.2 | 1.5 | 0.5 | - | PASS | 2026-10-20 |
| 128K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=0 n-gpu-layers=99 | **326.2** | 21097 | 665.7 | 3.0 | 1.0 | - | PASS | 2026-10-19 |
| 231K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=0 n-gpu-layers=99 | **326.7** | 19793 | 666.7 | 1.7 | 0.6 | - | PASS | 2026-10-20 |

### ornith-1.5-35b — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=36 spec-type=draft-mtp n-gpu-layers=99 | **58.5** | 823 | 16.6 | 6.4 | - | 66% | PASS | 2026-10-19 |
| 231K | ik_llama.cpp `main-b5483-3d9321e` | spec-type=draft-mtp fit=on n-gpu-layers=auto fit-target=1500 | **16.9** | 437 | 4.8 | 11.8 | - | 85% | PASS | 2026-10-19 |
| 231K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=36 spec-type=draft-mtp n-gpu-layers=99 | **59.3** | 832 | 16.8 | 10.8 | - | 66% | PASS | 2026-10-19 |

### qwen3.6-35b-a3b — 34.66B total, 3.45B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=36 spec-type=draft-mtp n-gpu-layers=99 | **59.1** | 785 | 17.1 | 11.5 | - | 73% | - | 2026-10-19 |

### qwen3.6-35b-a3b-ud-q4_k_xl — 34.66B total, 3.45B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| - | strata `v0.1.45` | n-gpu-layers=99 | **49.8** | 657 | 14.4 | 11.4 | - | - | - | 2026-10-19 |

### ornith-1.5-35b-mtp — 34.66B total, 3.45B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | n-cpu-moe=36 spec-type=draft-mtp n-gpu-layers=99 | **39.2** | 1135 | 11.4 | 10.8 | - | 32% | PASS | 2026-10-19 |

### Rejected, with the measured reason

| model | engine | why |
|---|---|---|
| ornith-1.5-35b | ik_llama.cpp `main-b5483-3d9321e` | rejected: 3.4x slower decode than llama.cpp (16.7-16.9 vs 58.1 t/s) with n-cpu-moe 36 AND its own --fit; --fit-margin fill hits 12.0GB VRAM (sysmem cliff); -rtr repack crashes at startup; mlock of the 17.7GB buffer fails; no /v1/messages; no router; no idle unload |
| qwen3.6-35b-a3b-ud-q4_k_xl | strata `v0.1.45` | rejected: cannot serve 200K context on 12GB. Its VRAM cost per context token is ~57 KB (measured 32K->64K = 1.80 GiB) vs llama.cpp q8 KV ~12.7 KB, so 200K needs ~11.5 GB for context alone; at 236K the engine reports 0 MiB free and refuses. Speed where it does run (32768 ctx): 49.8 t/s vs llama.cpp 58.5 at the same ctx (-15%), prefill 657 vs 823. Also: its setup mis-sizes --expert-vram (assumes 3 GB of weights+context, real ~7 GB). |
| qwen2.5-0.5b-instruct | llama.cpp `b11379-1537a0a8b` | correction: the eval rows recorded for this model before 2026-10-20T02:00Z are invalid. The gate ran ./golem eval through WSL (for python3), environment variables do not cross that boundary, so GOLEM_MODEL_ALIAS was dropped and the harness evaluated MODEL_ALIAS from .env - the Ornith-35B - while the row was written under the 0.5B name. Their 35B rates give them away: needle_prefill 867-883 t/s and depth_decode 43-49 t/s, where the 0.5B measures 18346 and 266.6. Nothing was adopted from them. Fixed by rewriting scripts/eval-model.sh in bash+curl+awk (no python3, no WSL). |

Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).
