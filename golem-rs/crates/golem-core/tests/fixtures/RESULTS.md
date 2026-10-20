# Benchmark results

Generated from [`bench/results.jsonl`](results.jsonl) by `golem report --md`.

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

## rtx-4090_24g_64g_i9-13900k

NVIDIA GeForce RTX 4090 · windows/docker

### moe-35b-a3b — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 | **59.8** | 840 | 17.0 | 10.9 | 19.5 | 68% | PASS | 2026-10-19 |

### Rejected, with the measured reason

| model | engine | why |
|---|---|---|
| moe-35b-a3b | ik_llama.cpp `main-b5483-3d9321e` | rejected: 3.1x slower decode than llama.cpp on this box |

## test-box

Test GPU · linux/native

### dense-0.5b — 0.49B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 8K | llama.cpp `b11379-1537a0a8b` | threads=20 | **317.6** | 19704 | 648.2 | 1.6 | - | - | FAIL | 2026-10-19 |

### moe-35b-a3b — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11379-1537a0a8b` | threads=8 n-cpu-moe=36 | **58.5** | 823 | 16.6 | 6.2 | 19.0 | 66% | PASS | 2026-10-19 |
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 | **59.3** | 832 | 16.8 | 10.7 | 19.4 | 66% | PASS | 2026-10-19 |

### Rejected, with the measured reason

| model | engine | why |
|---|---|---|
| moe-35b-a3b | ik_llama.cpp `main-b5483-3d9321e` | rejected: 3.4x slower decode than llama.cpp with the same split |

## unknown host

Test GPU · linux/native

### unknown-size

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 4K | llama.cpp `b11379-1537a0a8b` | - | **12.0** |  | - | - | - | - | - | 2026-10-19 |

Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).
