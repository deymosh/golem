# Benchmark results

Generated from [`bench/results.jsonl`](results.jsonl) by `./golem report --md`.
One row per model x engine build x context, at the best decode measured for that
combination. Compare inside a size class only: a 0.5B at 269 t/s says nothing
about a 35B at 59. `t/s / active B` is the cross-size ratio (active parameters are
what the GPU pushes per token: 3.5B for a 35B-A3B).


## <= 2B active

| model | engine | ctx | decode t/s | prefill t/s | t/s / active B | params | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---|---|---:|---:|---:|---:|---|---:|---:|---:|---|---|
| qwen2.5-0.5b-instruct | llama.cpp `b11379-1537a0a8b` | 8K | **269.3** | 17548 | 549.6 | 0.49B / 0.49B active | 2.0 | - | - | - | 2026-10-19 |
| qwen2.5-0.5b-instruct | ik_llama.cpp `main-b5483-3d9321e` | 8K | **66.7** | 7812 | 136.1 | 0.49B / 0.49B active | 4.7 | 0.6 | - | - | 2026-10-19 |

## 2-8B active

| model | engine | ctx | decode t/s | prefill t/s | t/s / active B | params | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---|---|---:|---:|---:|---:|---|---:|---:|---:|---|---|
| ornith-1.5-35b | llama.cpp `b11379-1537a0a8b` | 231K | **59.3** | 832 | 16.8 | 35.51B / 3.52B active | 10.8 | - | 66% | PASS | 2026-10-19 |
| qwen3.6-35b-a3b | llama.cpp `b11379-1537a0a8b` | 231K | **59.1** | 785 | 17.1 | 34.66B / 3.45B active | 11.5 | - | 73% | - | 2026-10-19 |
| ornith-1.5-35b | llama.cpp `b11379-1537a0a8b` | 32K | **58.5** | 823 | 16.6 | 35.51B / 3.52B active | 6.4 | - | 66% | PASS | 2026-10-19 |
| qwen3.6-35b-a3b-ud-q4_k_xl | strata `v0.1.45` | - | **49.8** | 657 | 14.4 | 34.66B / 3.45B active | 11.4 | - | - | - | 2026-10-19 |
| ornith-1.5-35b-mtp | llama.cpp `b11379-1537a0a8b` | 231K | **39.2** | 1135 | 11.4 | 34.66B / 3.45B active | 10.8 | - | 32% | PASS | 2026-10-19 |
| ornith-1.5-35b | ik_llama.cpp `main-b5483-3d9321e` | 231K | **16.9** | 437 | 4.8 | 35.51B / 3.52B active | 11.8 | - | 85% | PASS | 2026-10-19 |

## Rejected, with the measured reason

| model | engine | why |
|---|---|---|
| ornith-1.5-35b | ik_llama.cpp `main-b5483-3d9321e` | rejected: 3.4x slower decode than llama.cpp (16.7-16.9 vs 58.1 t/s) with n-cpu-moe 36 AND its own --fit; --fit-margin fill hits 12.0GB VRAM (sysmem cliff); -rtr repack crashes at startup; mlock of the 17.7GB buffer fails; no /v1/messages; no router; no idle unload |
| qwen3.6-35b-a3b-ud-q4_k_xl | strata `v0.1.45` | rejected: cannot serve 200K context on 12GB. Its VRAM cost per context token is ~57 KB (measured 32K->64K = 1.80 GiB) vs llama.cpp q8 KV ~12.7 KB, so 200K needs ~11.5 GB for context alone; at 236K the engine reports 0 MiB free and refuses. Speed where it does run (32768 ctx): 49.8 t/s vs llama.cpp 58.5 at the same ctx (-15%), prefill 657 vs 823. Also: its setup mis-sizes --expert-vram (assumes 3 GB of weights+context, real ~7 GB). |

Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).
