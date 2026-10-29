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

## rtx-3060_12g_32g_i7-12700kf

NVIDIA GeForce RTX 3060 · windows

### qwen2.5-0.5b-instruct — 0.49B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 8K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 temp=0.6 min-p=0 | **348.7** |  | 711.6 | 1.1 | 0.6 | - | - | 2026-10-20 |

### qwen3.6-35b-a3b — 34.66B total, 3.45B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 spec-type=draft-mtp temp=0.6 min-p=0 | **62.4** | 843 | 18.1 | 11.1 | 19.8 | 73% | - | 2026-10-20 |

### ornith-1.5-35b — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 244K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp temp=0.6 min-p=0 | **62.1** | 870 | 17.6 | 11.0 | 19.9 | 68% | - | 2026-10-20 |

### ornith-1.5-35b-mtp — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 spec-type=draft-mtp temp=0.6 min-p=0 | **40.9** | 1201 | 11.6 | 10.2 | 16.2 | 32% | - | 2026-10-20 |

### nemotron-3.5-lightning-30b — 32.91B total, 3.70B active, 128x6 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=30 temp=0.6 min-p=0.01 top-p=0.95 | **25.3** | 149 | 6.8 | 11.7 | 16.3 | - | - | 2026-10-20 |

### qwen3.5-9b — 8.95B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 64K | llama.cpp `b11379-1537a0a8b` | threads=16 temp=0.7 min-p=0 top-p=0.8 | **52.3** | 1556 | 5.8 | 7.0 | 6.5 | - | - | 2026-10-20 |

Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).
