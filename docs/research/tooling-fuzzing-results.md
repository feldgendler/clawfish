# Fuzzing campaign results

Empirical campaign records per ADR-0013 §10. Per-campaign rows are appended chronologically; each row pins the date, host, target, parameters, and outcome.

## Format

| date | host | target | seed corpus | runs | exec/s peak | cov:final | ft:final | RSS peak | crashes | notes |
|---|---|---|---|---|---|---|---|---|---|---|

## Campaigns

(none yet — initial slice landed the harness setup; first campaigns are deferred to a follow-up session.)

## Crashes triaged

(none yet.)

## Notes on metric provenance

- libFuzzer's `cov:` is libfuzzer-internal (basic blocks instrumented), not the same metric as `cargo llvm-cov`. For unit-test coverage, `cargo llvm-cov --summary-only --lib` remains authoritative.
- `ft:` is libFuzzer's "features" count — a finer-grained signal than `cov:` that includes coverage edges plus comparison signals.
- `runs` is libFuzzer's total executed inputs; throughput (exec/s) declines as the corpus grows because each new input is replayed against more saved inputs.
- Per-target host context matters: Apple M4 numbers won't match Apple M1 numbers won't match x86-64 Linux. Record host with each row.
