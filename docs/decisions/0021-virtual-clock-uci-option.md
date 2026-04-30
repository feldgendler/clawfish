# ADR-0021 — VirtualClock UCI option: thread CPU time as search time-source for hardware-invariant TC

**Status:** Accepted (lands with ELOH.C, 2026-04-30).

## Context

SPRT and rating-estimation runs on a wallclock-based time control are sensitive to thermal throttling, background scheduler activity, and core-type scheduling (P-core vs E-core on Apple Silicon's heterogeneous core design). M3.F observed the problem empirically: results couple to hardware thermal state and ambient load rather than engine strength alone.

The ELOH milestone exists to build a custom in-process harness (`src/bin/elo-iterate.rs`) capable of hardware-invariant rating estimation. ELOH.A and ELOH.B laid the harness foundation (spawn-once UCI subprocess driver, Robbins-Monro K-update, σ-stopping, N-parallel-pair concurrency). ELOH.C is the hardware-invariance phase: it adds a clawfish-private `VirtualClock` UCI option that swaps the search's wallclock time source for thread CPU time, and adds harness-side handshake-driven negotiation that activates the option for clawfish-vs-clawfish self-play.

ELOH.A's `MatchTimeMode { Wallclock, Nodes(u64) }` seam (`src/match_clock.rs`) was already in place as unconstructible-from-CLI dead code for the `Nodes` variant; ELOH.C bypasses that seam entirely — the `VirtualClock` approach operates at the engine's internal time-source level, not at the harness's `go`-command-format level.

## Decision

### 1. Time source: `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`

Under `VirtualClock=true`, the search worker uses POSIX `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` as its time source. This measures wall-time accumulated while the thread is scheduled on a CPU — it excludes time spent waiting for I/O, sleeping, or preempted by other processes. For a CPU-bound search thread it closely approximates "time spent doing search work," making it substantially more stable than wallclock under background load.

The implementation calls `libc::clock_gettime` directly (via the `libc = "0.2"` crate dependency) with a stack-allocated `timespec`, and returns nanoseconds as `u64`. Gated `#[cfg(unix)]` — Windows builds neither advertise the option nor accept `setoption VirtualClock value true`.

The `SearchInstant` enum carries the variant choice (`Wall(Instant)` / `Cpu(u64)`) so the type system enforces that all instants within a single `SearchClock` share the same variant. Cross-variant comparisons or subtractions reach `unreachable!("…cross-variant…")` — structurally impossible at production call sites but loudly documented.

### 2. Time-source ownership in the worker thread, not the orchestrator

`CLOCK_THREAD_CPUTIME_ID` is a per-thread counter: values from different threads are not comparable, and a read on Thread A means nothing for Thread B's CPU budget. The orchestrator thread (`Engine::handle_go`) must NOT read the CPU clock, because the search runs on a separate worker thread.

The fix is structural: `SearchContext` carries `caps: TimeCaps` (pure-function output of `compute_caps` — durations only, no clock reads) and `virtual_clock: bool`. The worker thread constructs a `SearchClock` at the entry of `Search::go`, reading the clock once on the correct thread:

```
let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
```

`start_for` reads `SearchInstant::now(virtual_clock)` once, derives `deadline` and `soft_deadline` as `Some(start.add(cap))` for finite caps and `None` for `Duration::MAX` caps, and returns a fully self-consistent `SearchClock`. All subsequent clock reads (`should_abort`, `is_soft_reached_at`, `elapsed_at`) call `SearchInstant::now(virtual_clock)` on the same worker thread.

This replaces M3.E's pattern where `handle_go` computed `Instant::now()` on the orchestrator thread and stored absolute deadlines in `SearchContext`. Under wallclock that was correct (all threads share the same wall clock); under CPU time it was categorically wrong.

The `bench` path is a degenerate single-thread case: `Engine::handle_bench` calls `Search::go` synchronously on the orchestrator thread, so the constructing thread and the consuming thread are the same. The per-thread invariant is trivially satisfied.

### 3. `SearchClock` and `SearchInstant` types

`SearchInstant` is a new enum in `src/search.rs`:

```rust
pub enum SearchInstant {
    Wall(Instant),
    Cpu(u64),  // nanoseconds from CLOCK_THREAD_CPUTIME_ID origin
}
```

All arithmetic and comparison methods require same-variant operands; cross-variant calls reach `unreachable!`. The `Cpu(u64).add(dur)` operation saturates on overflow (no panic for large durations).

`SearchClock` is a new worker-local struct:

```rust
pub struct SearchClock {
    pub start: SearchInstant,
    pub deadline: Option<SearchInstant>,
    pub soft_deadline: Option<SearchInstant>,
}
```

The ID outer loop reads `SearchInstant::now(ctx.virtual_clock)` once per iteration and passes that single value to both `clock.elapsed_at(now)` (for the `info … time` ms emission) and `clock.is_soft_reached_at(now)` (for the inter-iteration soft-cap check). This preserves a single-syscall-per-iteration invariant — no second clock read inside the same iteration boundary.

### 4. Rejected alternatives

**PMU cycle / instruction counters.** The research in `docs/research/tooling-cpu-cycle-counters.md` (initial survey + Apple-Silicon-specific follow-up + Linux-VM follow-up) confirms:

- On Apple Silicon macOS, `PMCCNTR_EL0` (ARM architectural cycle counter) is blocked: XNU does not set `PMUSERENR_EL0` to permit EL0 reads. Reading it generates an exception. The `kpc_*` / `kperf.framework` path requires root or the private entitlement `com.apple.private.kernel.kpc`, which is not grantable to third-party binaries.
- Apple-proprietary `SYS_APL_PMC*` registers are EL1-only by design.
- No new macOS 14/15/26 API exists to expose per-thread cycle or instruction counts unprivileged.
- Inside any common Linux ARM64 VM on Apple Silicon (UTM, Lima, Tart, OrbStack, VMware Fusion on ARM): no hardware performance counters are exposed to the guest — Apple does not pass the proprietary PMU through any hypervisor interface. The Linux ARM64 `perf_event_open` path requires hardware PMU support that the hypervisor cannot provide.
- The only path that would work — bare-metal Asahi Linux with `apple_m1_cpu_pmu` driver — is not the project's primary development environment.

PMU counters are therefore out of bounds for this project on its primary target.

**`--go-nodes N` harness flag (nodes-per-move budget).** User decision 2026-04-30: `go nodes N` ties the budget to the engine's internal node count, which is implementation-coupled. A future change — smarter-but-slower eval, more aggressive pruning, larger TT size, different history weights — shifts what "N nodes" means even within a single binary at different runtime settings. It is suitable for a single rating snapshot but not for cross-version SPRT, which is the project's primary use case from M4 onward. The `MatchTimeMode::Nodes(u64)` seam remains in the codebase as already-tested dead code (removing it would be more churn than retention); future work may revive it for diagnostic use if dual-boot Linux becomes the primary dev environment.

### 5. `MoveOverhead` reinterpretation under `VirtualClock=true`

The `MoveOverhead` UCI option (default 50 ms, valid `[0, 5000]`) is not changed. Under `VirtualClock=true` it still expresses milliseconds, but of CPU time rather than wallclock. The wallclock-jitter hedge that originally motivated the 50 ms default is partially meaningless under CPU-time TC (preemption does not consume CPU time). The option becomes a small fixed-cost conservatism. At 50 ms default this is harmless. Acknowledged degenerate case, left as-is.

### 6. `compute_caps` wall-ms-as-CPU-ms drift

`compute_caps` is a pure function (no clock reads) that divides UCI-protocol `wtime` / `btime` / `winc` / `binc` (wallclock fields per UCI spec) into a soft + hard cap expressed as `Duration` values. Under `VirtualClock=true`, the worker thread treats those output `Duration` values as CPU-time budgets — there is an implicit unit mismatch.

The empirical bound on this drift: the M4 probe (`docs/research/tooling-cpu-cycle-counters.md` §"Follow-up — CLOCK_THREAD_CPUTIME_ID empirical probe") shows `cpu/wall = 0.9993` on M4 / macOS 26.4.1 for a CPU-bound single thread — 0.07% drift. This is bounded by the degree to which the search thread is preempted during its wallclock time slot, which is near zero for a CPU-bound thread on an unloaded machine. Under heavy load, drift grows; VirtualClock is intended to be used with P-core pinning, which further reduces preemption. Acknowledged and documented; not corrected (the correction would require harness-side coordination to transmit a CPU-ms calibration factor, which is over-engineering for a 0.07% effect).

### 7. `#[cfg(unix)]` gating

The `VirtualClock` option is gated `#[cfg(unix)]`:

- `Engine::handle_uci` emits the `option name VirtualClock type check default false` line only on unix.
- `Engine::handle_setoption` accepts `VirtualClock` only on unix. On non-unix, the name-match arm returns an `info string` rejection message.
- `read_thread_cpu_ns()` (the libc shim calling `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`) is `#[cfg(unix)]` only.
- `SearchInstant::Cpu(u64)` as an enum variant exists on all platforms (to keep pattern-matching ergonomic), but `SearchInstant::now(true)` on non-unix reaches `unreachable!("VirtualClock not supported on non-unix platforms")`. In practice this path is unreachable: the option is not advertised on non-unix and `handle_setoption` rejects it, so `Engine::virtual_clock` cannot become `true` on non-unix.

Windows is not currently a build target, but the codebase compiles on Linux and macOS, and the gating keeps it clean.

## Consequences

- Clawfish-vs-clawfish self-play under `--virtual-clock` in the harness is substantially less sensitive to background load and scheduler noise than wallclock TC, improving the signal-to-noise ratio of SPRT and rating-estimation runs.
- The `SearchContext` field shape changed from M3.E: `start: Instant`, `deadline: Option<Instant>`, `soft_deadline: Option<Instant>` removed; `caps: TimeCaps`, `virtual_clock: bool` added. Existing test sites required mechanical migration.
- `SearchClock::should_abort` replaces `SearchContext::should_abort`. All production call sites are inside `Search::go` on the worker thread.
- The feature is opt-in (`default false`); wallclock behavior is unchanged when `VirtualClock` is false.
- Cross-engine matches (clawfish vs Stockfish) still use wallclock on the Stockfish side; Stockfish does not support the option. The harness silently skips the `setoption` for engines that do not advertise `VirtualClock`.
- `libc = "0.2"` added as a direct dependency.

## Operator's checklist — verifying `CLOCK_THREAD_CPUTIME_ID` on a new machine

Before relying on `--virtual-clock` for rating estimates on a new machine, run the following probe to confirm the M1-era underreporting bug is absent:

```c
// Save as probe.c; compile with: cc -O2 -o probe probe.c
#include <stdio.h>
#include <time.h>
#include <stdint.h>

static uint64_t clock_ns(clockid_t id) {
    struct timespec ts;
    clock_gettime(id, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}

int main(void) {
    for (int t = 0; t < 3; t++) {
        uint64_t w0 = clock_ns(CLOCK_MONOTONIC);
        uint64_t c0 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
        volatile uint64_t x = 1;
        for (uint64_t i = 0; i < 2000000000ULL; i++) x = x * 6364136223846793005ULL + 1442695040888963407ULL;
        (void)x;
        uint64_t w1 = clock_ns(CLOCK_MONOTONIC);
        uint64_t c1 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
        double wall_ms = (w1 - w0) / 1e6;
        double cpu_ms  = (c1 - c0) / 1e6;
        printf("trial %d: wall=%.3f ms  cpu=%.3f ms  cpu/wall=%.4f\n",
               t, wall_ms, cpu_ms, cpu_ms / wall_ms);
    }
    return 0;
}
```

**Expected output on a healthy machine:** `cpu/wall ≈ 1.0` (within ~1%) across all three trials.

**M1-bug symptom:** `cpu/wall ≈ 0.025` (approximately 1/40 of unity). On a machine exhibiting this ratio, `VirtualClock=true` will drastically undercount CPU time — the engine will use far less of its budget than intended per move, playing effectively at much lower depth. Do not use `--virtual-clock` on such a machine.

The M4 / macOS 26.4.1 probe result: `cpu/wall = 0.9993` across three trials — the M1 bug is absent on this generation.

## See also

- ADR-0020 (`docs/decisions/0020-eloh-harness-driving-model.md`) — ELOH foundation: wallclock TC, spawn-once lifecycle, `MatchTimeMode` seam.
- Plan: `docs/plans/eloh.c.md` — full scope, type definitions, test coverage strategy, risk register.
- Research: `docs/research/tooling-cpu-cycle-counters.md` — survey + Apple Silicon follow-up + Linux VM follow-up + M4 empirical probe.
- Spec: `docs/tooling/elo-iteration-harness.md` — ELOH milestone overview and sub-phase scope detail.
