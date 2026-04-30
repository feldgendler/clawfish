# ELOH.C benchmark

Captured **2026-04-30** on **Apple M4** (4 P-cores + 6 E-cores), **macOS 26.4.1**, **32 GB**. Tooling-track milestone bench — records the `bench` UCI command's node count and NPS under both time-source modes. Separate from strength-track bench files (`bench/m3.md`, `bench/m4.md`).

Per ADR-0010 and the plan's §7 step 6 correctness gate: **node count must be byte-identical across all six runs** (VC=false and VC=true, three paired runs). `bench` is fixed-depth; `caps = (Duration::MAX, Duration::MAX)` ⇒ `clock.deadline = None` ⇒ the time source is never consulted for abort decisions during bench. The node count being identical is the correctness invariant: if `VirtualClock=true` altered node count, that would signal a search-behavior bug.

## Method

```
printf 'bench\nquit\n' | ./target/release/clawfish
printf 'setoption name VirtualClock value true\nbench\nquit\n' | ./target/release/clawfish
```

Three paired runs interleaved (VC=false run N followed immediately by VC=true run N, to control for thermal state within each pair).

## Results

| Run | VirtualClock | Nodes | NPS |
|---|---|---|---|
| 0 | false | 172,312,700 | 4,269,921 |
| 0 | true  | 172,312,700 | 4,554,681 |
| 1 | false | 172,312,700 | 4,370,867 |
| 1 | true  | 172,312,700 | 4,721,025 |
| 2 | false | 172,312,700 | 5,456,559 |
| 2 | true  | 172,312,700 | 5,822,948 |

## Correctness gate — passed

Node count is **172,312,700** in all six runs — byte-identical across both modes and all three repetitions. The plan's stated failure mode (VC=true node count differs from VC=false) does not fire. The `VirtualClock` option does not alter search behavior, only the time-source consulted for deadline checks.

## NPS observations

**NPS varies ~25% run-to-run** (4.27–5.82 Mnps) from thermal and scheduling noise: the M4's P-cores boost and throttle dynamically; without explicit P-core pinning (`taskpolicy -c utility`), each bench run lands on whichever core macOS selects. The NPS figures are not a regression baseline.

**Within each paired run, VC=true is 5–8% faster than VC=false** (run 0: +6.7%; run 1: +8.0%; run 2: +6.7%). The plan anticipated a small overhead from `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` vs `mach_absolute_time`; the observed direction is opposite. Likely `CLOCK_THREAD_CPUTIME_ID` is cheaper than `mach_absolute_time` on M4 hardware, but the magnitude is within the run-to-run noise band, so the direction cannot be distinguished from thermal variation without P-core pinning. Not a load-bearing finding.

**Node-count signature is M3.F's (172,312,700), not M4.A's (39,964,046).** This branch (`tooling/elo-harness`) is based on M3.F's `035c115` ancestor — ELOH.A+B+C are tooling-track and have not absorbed M4.A's TT, M4.B's killer moves, or M4.C's history heuristic. When the ELOH branch eventually merges to main alongside the M4 stack, future bench runs will pick up M4.A–C's reduced node counts. This file's signature is M3.F + tooling-only changes.

## See also

- ADR-0021 (`docs/decisions/0021-virtual-clock-uci-option.md`) — time-source decision.
- `docs/research/tooling-cpu-cycle-counters.md` §"Follow-up — CLOCK_THREAD_CPUTIME_ID empirical probe" — the M4 probe confirming `cpu/wall = 0.9993` (M1 bug absent).
- `bench/m3.md` — M3.F bench signature (`bench: 172312700 nodes 11489045 nps`) that the node count here matches.
