# Runtime benchmark

This benchmark starts Agul through ARI and serves a deterministic
OpenAI-compatible stream from a fake local provider. Each fresh session first
runs a tool turn with an oversized result, then measures a second `ari.send`:

- time from `ari.send` to the first event (excludes process startup, which
  happens before the timed call);
- time from `ari.send` to the final result;
- peak resident memory observed for the Agul process while it runs;
- input, output, cache-hit, cache-miss, and reasoning tokens;
- the exact reusable prompt prefix between the final warmup request and the
  measured request, reported through the normal cache-token usage path.

Every sample uses a fresh Agul process and workspace. Latency excludes process
startup and the warmup turn; peak memory covers the complete sample. The KV
metric is warm-turn prefix reuse inside one session.

Run it after building Agul:

```bash
cargo build --release
python benchmarks/runtime_benchmark.py --samples 5 --output target/benchmark.json
```

Without budgets this only measures and exits 0 when measurement succeeds. The
report is printed to stdout and, with `--output`, written to a JSON file.

Every sample has a finite 30-second hard deadline by default, covering ARI
initialization, the response, and shutdown. Override it with
`--sample-timeout-seconds`. A timeout terminates the Agul child and writes a
machine-readable partial/error report before returning nonzero.

## Regression gate

Pass budgets to turn the benchmark into a pass/fail gate:

| Flag | Budget is a | Applies to |
| --- | --- | --- |
| `--first-event-budget-ms MS` | maximum | median time to the first event |
| `--total-response-budget-ms MS` | maximum | median time to the final result |
| `--peak-memory-budget-mib MIB` | maximum | median peak resident memory |
| `--kv-hit-budget-percent PERCENT` | minimum | warm-turn stable-prefix reuse |

```bash
python benchmarks/runtime_benchmark.py \
  --samples 5 --output target/benchmark.json \
  --sample-timeout-seconds 5 \
  --first-event-budget-ms 1000 \
  --total-response-budget-ms 1000 \
  --peak-memory-budget-mib 128 \
  --kv-hit-budget-percent 99
```

Budgets must be finite, non-negative numbers; the cache-hit budget must be at
most 100. The sample timeout must be finite and greater than zero. Invalid
values are rejected before any work starts, with exit code 2 and an error
naming the offending flag.

The gate compares the unrounded **median** across samples against each budget,
not p95 or max. With the handful of samples CI runs, p95 and max are just the
single worst sample and would make the gate flaky on shared runners; the median
is the least noisy summary of a tiny sample set. p95 and max stay in the report
for manual inspection.

When budgets are set, the JSON report gains a `gate` section with the budgets,
the statistic used, the overall pass/fail, and one violation entry per failed
metric. The report file is written **before** the gate result is acted on, so a
failed gate still leaves the complete measurement behind.

All completed reports include the effective `configuration` and raw
`sample_metrics`; aggregate values are rounded to three decimals for stable
artifact comparison while gate decisions use the raw per-sample values.

Exit codes:

- `0` – measurement succeeded (and the gate passed, if budgets were given);
- `1` – the gate failed; the JSON report was still written, violations are
  listed on stderr;
- `2` – invalid arguments (bad budget value, bad sample count).
- `3` – timeout or measurement failure; a partial/error JSON report was still
  written when `--output` was supplied.

## CI

The ubuntu-24.04 CI job runs the benchmark tests and then the gate with five
samples, a five-second hard sample timeout, and explicit budgets:

- first event: 1000 ms
- total response: 1000 ms
- peak memory: 128 MiB
- warm-turn stable prompt prefix: at least 99%

The JSON report is uploaded as a workflow artifact even when the gate fails.
Use the `benchmark-report` artifact from the latest successful run as the
current reference. Local latency and memory values are meaningful only when
compared on the same machine with the same build and sample configuration.

## Limitations

- **Latency is not wall-clock CI proof.** Shared GitHub runners are noisy. The
  budgets remain well above observed values, but now catch regressions crossing
  the one-second boundary and large memory growth. A separate hard deadline
  catches hangs; this gate will still not notice a 2x slowdown. For fine-grained
  latency or memory work, compare reports collected on the same machine.
- **Memory sampling is coarse.** RSS is polled every 10 ms, so short spikes can
  be missed. Only the Agul process is measured (VmRSS on Linux, peak working
  set on Windows); the fake provider and child processes are not counted.
- **The hard deadline owns the direct Agul process.** Reader cleanup is bounded,
  but the benchmark does not attempt platform-specific cleanup of arbitrary
  grandchildren. The CI job timeout remains the final backstop for that
  pathological case.
- **The KV percentage is a structural proxy, not a provider benchmark.** The
  fake provider derives hit and miss tokens from the exact common prefix of two
  rendered Agul requests. This catches reordered tools and rewritten history,
  while real providers may still miss because of routing, cache lifetime, or
  provider policy.
- **One workload only.** The benchmark exercises a warm multi-round tool session
  with one oversized result. Regressions specific to other ARI flows or much
  longer sessions remain out of scope.
