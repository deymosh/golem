# Benchmark results

Generated from [`results/results.jsonl`](results.jsonl) by `golem report --md`.

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

### qwen2.5-0.5b-instruct Q4_K_M — 0.49B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 8K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **349.1** | 19432 | 712.4 | 1.0 | 1.1 | - | PASS | 2026-10-25 |
| 16K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **341.9** | 19454 | 697.8 | 1.1 | 1.1 | - | PASS | 2026-10-25 |
| 32K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **340.1** | 19452 | 694.1 | 1.3 | 1.2 | - | PASS | 2026-10-25 |

### ornith-1.5-35b Q4_K_M — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=0.6 top-k=20 min-p=0 | **64.1** | 561 | 18.2 | 9.6 | 22.6 | 68% | PASS | 2026-10-25 |
| 231K | llama.cpp `b11879-c49edf096` | threads=16 n-cpu-moe=33 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **62.1** | 820 | 17.6 | 11.5 | 23.7 | 65% | PASS | 2026-10-25 |
| 232K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 mmproj=mmproj-Ornith-1.5-35B-BF16.gguf | **54.1** | 562 | 15.4 | 11.0 | 18.3 | 68% | PASS | 2026-10-29 |
| 237K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 mmproj=mmproj-Ornith-1.5-35B-BF16.gguf | **52.2** | 519 | 14.8 | 11.2 | 19.0 | 68% | PASS | 2026-10-29 |
| 240K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **63.3** | 847 | 18.0 | 11.0 | 23.7 | 68% | PASS | 2026-10-25 |
| 240K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mlock ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **64.2** | 844 | 18.2 | 11.3 | 17.4 | 68% | PASS | 2026-10-25 |
| 241K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **63.7** | 830 | 18.1 | 11.0 | 23.7 | 68% | PASS | 2026-10-25 |
| 241K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **63.7** | 831 | 18.1 | 11.0 | 23.7 | 67% | PASS | 2026-10-25 |
| 241K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **63.6** | 845 | 18.1 | 11.0 | 23.7 | 68% | PASS | 2026-10-25 |
| 241K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **62.6** | 875 | 17.8 | 11.0 | 23.7 | 68% | PASS | 2026-10-25 |
| 242K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 mmproj=mmproj-Ornith-1.5-35B-BF16.gguf | **52.2** | 760 | 14.8 | 11.0 | 21.9 | 58% | PASS | 2026-10-29 |
| 243K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **63.5** | 846 | 18.0 | 11.0 | 23.7 | 68% | PASS | 2026-10-25 |
| 243K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mmap+mlock ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **58.5** | 925 | 16.6 | 11.4 | 23.4 | 63% | PASS | 2026-10-27 |
| 244K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mlock ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **62.1** | 870 | 17.6 | 11.0 | 19.9 | 68% | PASS | 2026-10-20 |
| 244K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 mmproj=mmproj-Ornith-1.5-35B-BF16.gguf | **53.7** | 750 | 15.3 | 11.0 | 21.3 | 60% | PASS | 2026-10-29 |
| 244K | llama.cpp `b11879-c49edf096` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **56.0** | 893 | 15.9 | 11.0 | 23.7 | 63% | PASS | 2026-10-25 |
| 245K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 mmproj=mmproj-Ornith-1.5-35B-BF16.gguf | **56.3** | 678 | 16.0 | 11.1 | 21.3 | 61% | PASS | 2026-10-29 |
| 245K | llama.cpp `b11879-c49edf096` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **57.1** | 882 | 16.2 | 11.0 | 23.7 | 63% | PASS | 2026-10-25 |
| 256K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mlock ubatch-size=1024 temp=0.6 top-k=20 min-p=0 | **63.8** | 559 | 18.1 | 10.1 | 18.9 | 68% | PASS | 2026-10-24 |
| 256K | llama.cpp `b11879-c49edf096` | threads=16 n-cpu-moe=35 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=0.6 top-k=20 min-p=0 | **56.3** | 543 | 16.0 | 10.1 | 22.7 | 63% | PASS | 2026-10-25 |

### qwen3.6-35b-a3b UD-Q4_K_M — 34.66B total, 3.45B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 spec-type=draft-mtp cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mlock ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **62.4** | 843 | 18.1 | 11.1 | 19.8 | 73% | PASS | 2026-10-20 |
| 247K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=37 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **58.7** | 488 | 17.0 | 11.0 | 24.0 | 75% | PASS | 2026-10-25 |
| 248K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=37 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **60.8** | 493 | 17.6 | 11.0 | 24.3 | 75% | PASS | 2026-10-25 |
| 256K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=37 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **60.6** | 492 | 17.6 | 11.2 | 23.8 | 75% | PASS | 2026-10-25 |

### flint UD-Q3_K_XL — 39.79B total, 2.82B active, 256x6 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=24 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=1.0 top-k=20 top-p=0.95 | **52.9** | 913 | 18.8 | 10.8 | 18.0 | - | FAIL | 2026-10-28 |
| 44K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=24 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=1.0 top-k=20 top-p=0.95 | **52.1** | 900 | 18.5 | 11.0 | 18.0 | - | FAIL | 2026-10-28 |
| 46K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=24 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=1.0 top-k=20 top-p=0.95 | **52.3** | 946 | 18.5 | 11.0 | 18.0 | - | FAIL | 2026-10-28 |
| 47K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=24 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=1.0 top-k=20 top-p=0.95 | **51.7** | 930 | 18.3 | 11.0 | 18.0 | - | FAIL | 2026-10-28 |
| 52K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=25 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **51.6** | 1250 | 18.3 | 11.0 | 18.2 | - | FAIL | 2026-10-28 |
| 64K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=24 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=1024 temp=1.0 top-k=20 top-p=0.95 | **52.0** | 916 | 18.4 | 11.3 | 18.1 | - | FAIL | 2026-10-28 |
| 86K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=27 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **49.7** | 1199 | 17.6 | 11.0 | 18.3 | - | FAIL | 2026-10-28 |
| 88K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=27 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **49.9** | 1178 | 17.7 | 11.0 | 18.3 | - | FAIL | 2026-10-28 |
| 132K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=30 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **49.2** | 1149 | 17.4 | 11.0 | 18.5 | - | FAIL | 2026-10-28 |
| 133K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=30 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **49.3** | 1152 | 17.5 | 11.0 | 18.5 | - | FAIL | 2026-10-28 |
| 140K | llama.cpp `b11936-27b7a8a00` | threads=16 n-cpu-moe=30 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=1.0 top-k=20 top-p=0.95 | **48.5** | 1110 | 17.2 | 11.1 | 18.5 | - | FAIL | 2026-10-28 |

### nemotron-3.5-lightning-30b UD-Q4_K_M — 32.91B total, 3.70B active, 128x6 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=38 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 min-p=0.01 top-p=0.95 | **47.1** | 1024 | 12.7 | 11.6 | 23.9 | - | PASS | 2026-10-25 |
| 335K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=41 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 min-p=0.01 top-p=0.95 | **44.8** | 915 | 12.1 | 11.0 | 23.9 | - | PASS | 2026-10-25 |
| 336K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=41 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 min-p=0.01 top-p=0.95 | **44.7** | 930 | 12.1 | 11.0 | 23.8 | - | PASS | 2026-10-25 |
| 373K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=41 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 min-p=0.01 top-p=0.95 | **44.7** | 927 | 12.1 | 11.3 | 24.5 | - | PASS | 2026-10-25 |
| 462K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=41 fit=off cache-type-k=q8_0 cache-type-v=q8_0 load-mode=auto ubatch-size=2048 temp=0.6 min-p=0.01 top-p=0.95 | **44.6** | 943 | 12.1 | 11.4 | 23.3 | - | PASS | 2026-10-25 |

### ornith-1.5-35b-mtp — 35.51B total, 3.52B active, 256x8 experts

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 spec-type=draft-mtp cache-type-k=q8_0 cache-type-v=q8_0 load-mode=mlock ubatch-size=2048 temp=0.6 top-k=20 min-p=0 | **40.9** | 1201 | 11.6 | 10.2 | 16.2 | 32% | - | 2026-10-20 |

### ornith-1.5-9b Q4_K_M — 9.20B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **78.2** | 1565 | 8.5 | 6.4 | 7.3 | 63% | PASS | 2026-10-28 |
| 64K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **77.4** | 1542 | 8.4 | 7.8 | 7.4 | 63% | PASS | 2026-10-28 |
| 183K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **77.2** | 1557 | 8.4 | 11.0 | 7.6 | 63% | PASS | 2026-10-28 |
| 184K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **77.6** | 1555 | 8.4 | 11.0 | 7.6 | 63% | PASS | 2026-10-28 |
| 190K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **76.5** | 1542 | 8.3 | 11.2 | 7.6 | 63% | PASS | 2026-10-28 |
| 205K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **76.4** | 1551 | 8.3 | 11.0 | 7.6 | 63% | PASS | 2026-10-28 |
| 208K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **76.7** | 1556 | 8.3 | 11.1 | 7.7 | 63% | PASS | 2026-10-28 |
| 215K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=none fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **54.4** | 1638 | 5.9 | 10.0 | 7.1 | - | PASS | 2026-10-28 |
| 218K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **75.4** | 1435 | 8.2 | 11.6 | 7.7 | 63% | PASS | 2026-10-28 |
| 224K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=draft-mtp fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **75.1** | 1427 | 8.2 | 11.6 | 7.8 | 63% | PASS | 2026-10-28 |
| 256K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 spec-type=none fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.6 top-k=20 min-p=0 top-p=0.95 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **54.3** | 1647 | 5.9 | 11.0 | 7.1 | - | PASS | 2026-10-28 |

### qwen3.5-9b UD-Q4_K_XL — 8.95B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 64K | llama.cpp `b11379-1537a0a8b` | threads=16 cache-type-k=q8_0 cache-type-v=q8_0 temp=0.7 top-k=20 min-p=0 top-p=0.8 | **52.3** | 1556 | 5.8 | 7.0 | 6.5 | - | PASS | 2026-10-20 |
| 128K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.7 top-k=20 min-p=0 top-p=0.8 | **52.1** | 1655 | 5.8 | 8.3 | 6.6 | - | PASS | 2026-10-25 |
| 252K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=0.7 top-k=20 min-p=0 top-p=0.8 | **52.1** | 1659 | 5.8 | 11.0 | 6.7 | - | PASS | 2026-10-25 |

### ornith-1.5-9b Q6_K — 9.20B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **42.3** | 1546 | 4.6 | 8.4 | 7.4 | - | PASS | 2026-10-25 |
| 64K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **42.3** | 1541 | 4.6 | 9.1 | 7.4 | - | PASS | 2026-10-25 |
| 153K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 mmproj=mmproj-Ornith-1.5-9B-BF16.gguf | **42.3** | 1540 | 4.6 | 11.0 | 7.5 | - | PASS | 2026-10-25 |

### gemma-4-12b-it-qat-q4_0 Q4_0 — 11.91B

| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 32K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=1.0 top-k=64 top-p=0.95 mmproj=mmproj-gemma-4-12b-it-qat-q4_0.gguf | **40.0** | 1248 | 3.4 | 7.9 | 8.0 | - | PASS | 2026-10-25 |
| 64K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=1.0 top-k=64 top-p=0.95 mmproj=mmproj-gemma-4-12b-it-qat-q4_0.gguf | **39.9** | 1244 | 3.4 | 8.3 | 8.0 | - | PASS | 2026-10-25 |
| 256K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=0 fit=off cache-type-k=q8_0 cache-type-v=q8_0 temp=1.0 top-k=64 top-p=0.95 mmproj=mmproj-gemma-4-12b-it-qat-q4_0.gguf | **40.0** | 1244 | 3.4 | 10.4 | 8.2 | - | PASS | 2026-10-25 |

## unknown_0g_0g_

unknown · windows

Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).
