# Benchmark records

`results.jsonl` holds one JSON object per line, appended by `./golem bench` and
`./golem eval` (via [scripts/record-result.sh](../scripts/record-result.sh); `RECORD=0`
skips it, `RECORD_NOTE="..."` adds a note). Append-only: a correction is a new line with a
`note`, never an edit.

Fields: `ts` (UTC), `kind` (`bench` | `eval`), `host` (anonymous hardware slug: GPU, VRAM GB,
RAM GB, CPU), `gpu`, `os`, `mode` (DEPLOY_MODE), `engine`, `engine_build` (from the server's
`/props`), `model` (preset / alias), `model_file`, `flags` (effective preset flags: `[*]` +
the model's section), then the metrics — bench: `decode_best`, `decode_median`,
`draft_accept` (0-1), `prefill` (t/s on ~28.6K tokens), `vram_mib` (GPU), `ram_used_gib` (working set of the server process tree); eval: `eval.tools`,
`eval.needle`, `eval.needle_tokens`, `eval.needle_prefill`, `eval.depth_decode` — plus
`ram_free_gib`, `quiet` (false when a download/build was running: the number is suspect)
and `note`.
