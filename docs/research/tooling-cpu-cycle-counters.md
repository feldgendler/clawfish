# CPU Time / Cycle Counter Survey for Chess Engine Time Budgeting

**Research goal:** identify which hardware-invariant "compute consumed" counters are accessible from unprivileged user-space code on Apple Silicon macOS (primary target) and Linux x86-64 / ARM64 (secondary targets), for use in a per-thread time-budget primitive ("VirtualClock").

**Constraint that drives the survey:** the engine binary runs under `cargo run` and in tournament harnesses. No root, no entitlements, no kernel extensions, no `sudo`.

---

## Framing: what "invariant" means here

There are two distinct invariance properties, and confusing them is the core gotcha:

- **Frequency-invariant:** the counter ticks at a fixed hardware rate regardless of the CPU's current execution frequency (thermal throttling, DVFS power states). `CNTVCT_EL0` (ARM) and `RDTSC` (x86 invariant TSC) are frequency-invariant. They are wall-time-equivalent counters running at a fixed reference frequency.
- **Work-invariant (thermal-invariant):** the counter accumulates proportionally to *work done* regardless of clock speed. No publicly accessible counter on any of these platforms provides this property. CPU-time clocks (POSIX `CLOCK_THREAD_CPUTIME_ID`) measure time-slice-on-CPU in wall-time units, not cycles-of-work.

A chess engine that wants "did I spend my compute budget" needs to understand which property each API actually delivers.

---

## Candidate-by-candidate findings

### Apple Silicon macOS

#### `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`

- **What it measures:** wall-time accumulated while the thread is scheduled on a CPU. Implemented via the Mach scheduler's time accounting.
- **Thermal throttling:** NOT thermal-invariant. When the CPU frequency is reduced by thermal throttling, the same code path takes more wall-clock seconds and therefore accumulates more CLOCK_THREAD_CPUTIME_ID seconds. A thread that does fixed work will show higher CPU-time when throttled.
- **Severe bug on Apple Silicon:** A confirmed report on Apple Developer Forums (tested on macOS 12.5 / M1) shows the API returns drastically underreported values — approximately 0.024 s for 1 s of actual CPU work. Intel Macs and iPhones return correct values. The bug affects native M1 code and Rosetta 2.
- **Fix status:** No official fix confirmation found as of April 2026. Whether M3/M4 or macOS 14/15 resolve this is unknown; the issue does not appear in public release notes.
- **Resolution:** ~1 ms on macOS (Darwin documentation says accuracy to ~1 ms).
- **Verdict: do not use on Apple Silicon.** Bug alone disqualifies it; thermal non-invariance is a secondary concern.

#### `mach_thread_info(THREAD_BASIC_INFO)` — the Mach-native equivalent

- **What it measures:** same scheduler-accounting accumulator, exposed as `user_time` + `system_time` `time_value_t` fields.
- **Thermal throttling:** same wall-time-on-CPU semantics as above; not thermal-invariant.
- **Resolution:** explicitly documented as "only accurate up to 1 ms" on Darwin.
- **Known issues:** The `user_time` / `system_time` fields on M1 had unit bugs (reported in osquery PR #7473); the `time_value_t` tick rate changed on Apple Silicon relative to Intel.
- **Verdict: do not use.** Same underlying mechanism as CLOCK_THREAD_CPUTIME_ID with same or worse fidelity.

#### ARM `CNTVCT_EL0` (virtual counter, `mrs` instruction)

- **What it measures:** ARM virtual counter — a 64-bit monotonic wall-time counter.
- **Frequency:** fixed 24 MHz on Apple Silicon M1 (confirmed: `cntfrq_el0` reads 24_000_000; one tick every ~41.67 ns). This is a fixed hardware reference clock, not the core execution clock.
- **Frequency-invariant:** yes. The 24 MHz reference clock does not change with core frequency, DVFS states, or thermal throttling.
- **Thermal-invariant (work-based):** no. It is wall-time; throttled CPUs do the same work in more ticks.
- **Accessible from EL0 (user space) on macOS:** yes. The kernel sets `CNTKCTL_EL1.EL0VCTEN = 1` on Apple Silicon; `mrs x0, cntvct_el0` executes without trapping. Confirmed by cpufun.substack.com benchmarking on M1 darwin (14 ns per read vs 24 ns for nanotime).
- **Not a cycle counter:** 24 MHz is ~100x-500x slower than the core clock (which runs 3–4 GHz). Cannot be used to count cycles.
- **Verdict: usable for wall-time measurement at 24 MHz resolution.** Better overhead than `mach_absolute_time` (fewer ns per read). Not useful as a compute-consumed measure.

#### `mach_absolute_time` / `clock_gettime(CLOCK_MONOTONIC)`

- **What it measures:** wall-time (monotonic). On Apple Silicon, `mach_absolute_time` ticks at 24 MHz (numer=125, denom=3 to convert to nanoseconds). This is the same underlying counter as `CNTVCT_EL0`.
- **Thermal throttling:** not affected (fixed reference clock). But it is wall-time, not compute time.
- **Verdict: wall-time only.** The engine's time management already uses this (via `Instant::now()` in Rust, which maps to CLOCK_MONOTONIC). Not a compute-budget primitive.

#### `mach_continuous_time`

- **What it measures:** monotonic wall-time that continues to advance during sleep. Even less suitable than `mach_absolute_time` for compute budgeting.
- **Verdict: not relevant.**

#### Apple PMU via `kpc_*` / `kperf.framework`

- **What it provides:** true hardware performance counters — cycles retired, instructions retired, cache misses, etc.
- **Privileges required:** root (`sudo`) or the private entitlement `com.apple.private.kernel.kpc`. This entitlement is not requestable by third-party developers; it requires Apple to grant it.
- **Practical accessibility:** fully inaccessible to unprivileged processes (confirmed by the ibireme gist: the code explicitly prints "Permission denied, xnu/kpc requires root privileges" for ordinary users). Reading `kpc_cpu_string()` and `kpc_get_config_count()` is documented as non-root-requiring, but these are informational only — no counter values.
- **Verdict: out of bounds.** Requires root or a non-grantable private entitlement. Disqualified by the constraint.

---

### Linux x86-64

#### `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`

- **What it measures:** wall-time accumulated while the thread is scheduled on a CPU, maintained by the kernel's CFS scheduler in nanoseconds. Since Linux 2.6.12 + glibc 2.4, implemented as a proper kernel syscall (not a hardware timer register emulation).
- **Thermal throttling:** NOT thermal-invariant. The scheduler counts time slices in nanoseconds using the system wall clock. A throttled CPU that does the same work in 2x the wall-time accumulates 2x more CPU-time. No mechanism exists to remove the throttling component.
- **Privilege:** no privilege required. Any process can call it on its own thread.
- **Resolution:** nanosecond units; actual granularity is HZ-limited historically but modern kernels use perf-counter-assisted accounting for sub-HZ accuracy.
- **Verdict: usable but thermally non-invariant.** Correct for "how long was I scheduled" but not for "how much work did I do."

#### `perf_event_open` with `PERF_COUNT_HW_INSTRUCTIONS` or `PERF_COUNT_HW_CPU_CYCLES`

- **What it provides:** true hardware performance counters — instructions retired (closest to work-invariant), CPU cycles elapsed (core-clock-dependent, not frequency-invariant if clocks scale).
- **Privilege requirements:**
  - The relevant control is `/proc/sys/kernel/perf_event_paranoid`.
  - Paranoid level **2** (default since Linux 4.6 on many distros, including modern Ubuntu/Debian): "per-process performance monitoring only; CPU and system events in user space only."
  - At paranoid=2, unprivileged users **can** open per-thread hardware counters (pid=0 / own thread, cpu=-1 / any CPU) with `exclude_kernel=1, exclude_hv=1`. The `perf_event_open(2)` man page example explicitly shows this configuration for unprivileged use.
  - Per-CPU (system-wide, cpu>=0, pid=-1) monitoring requires paranoid < 1 or `CAP_SYS_ADMIN` / `CAP_PERFMON`.
- **Paranoid=3 (some hardened distros, Android):** blocks hardware counters entirely for unprivileged users. Not the typical desktop/server Linux default, but cannot be assumed away in a tournament harness.
- **Hardware availability:** `PERF_COUNT_HW_INSTRUCTIONS` is not guaranteed on all microarchitectures. ARM64 servers and embedded systems may lack or expose fewer PMU counters. On x86 post-Nehalem, it is reliably present.
- **`PERF_COUNT_HW_INSTRUCTIONS` is work-invariant:** instructions retired does not scale with clock frequency. 1M instructions retired is 1M instructions regardless of whether they ran at 1 GHz or 4 GHz. This is the closest available proxy to true "compute consumed."
- **`PERF_COUNT_HW_CPU_CYCLES` is not work-invariant:** counts core-clock cycles; if thermal throttling reduces frequency, the same work produces fewer cycles per second but the cycle counter moves slower — cycle count per fixed computation is *stable* but clock-elapsed-time is longer.
- **Verdict: useful on typical Linux x86, fragile across environments.** `PERF_COUNT_HW_INSTRUCTIONS` is the best available work-proxy. Requires fallback when paranoid level blocks access or hardware lacks the counter.

#### `rdtsc` (x86 Time Stamp Counter)

- **What it measures:** 64-bit hardware timestamp. On modern CPUs with invariant TSC (CPUID.80000007H:EDX[8] set — universal on Intel Nehalem+ and AMD K10+): runs at a fixed nominal frequency regardless of core frequency, DVFS P/C/T-states, and thermal throttling.
- **Frequency:** fixed at the CPU's nominal base frequency (e.g. 3.2 GHz for a chip running at 3.2–4.5 GHz turbo). Typical values: 2–4 GHz.
- **Frequency-invariant:** yes (invariant TSC). Thermal throttling does not affect the TSC increment rate.
- **Thermal-invariant (work-based):** no. Like CNTVCT_EL0, it is a wall-time counter at fixed rate. Throttled CPUs do the same work in more ticks.
- **User-space accessible:** yes by default on Linux. The `RDTSC` instruction executes at any privilege level unless `CR4.TSD` is set; the Linux kernel does not set TSD. No special setup required.
- **Not a true cycle counter:** the invariant TSC runs at a fixed reference rate, not at the actual execution core frequency. When turbo is active, actual cycles can exceed TSC ticks; when throttled, actual cycles lag behind TSC.
- **Verdict: very low overhead wall-time equivalent.** ~3 ns overhead. No privilege requirements. Usable as a cheap monotonic timer on x86, but not as a compute budget.

---

### Linux ARM64

#### `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` — same as x86 above

- Same kernel scheduler accounting; same thermal non-invariance.
- On ARM64, the kernel's vDSO implements CLOCK_MONOTONIC using CNTVCT_EL0 (no syscall), but CLOCK_THREAD_CPUTIME_ID requires a syscall.
- Verdict: same as x86 Linux — usable but thermally non-invariant.

#### `CNTVCT_EL0` — same register as on Apple Silicon

- **Linux ARM64 kernel enables user-space access by default:** the ARM64 Linux port sets `CNTKCTL_EL1.EL0VCTEN = 1` during boot for the vDSO implementation of CLOCK_MONOTONIC. Direct `mrs` reads therefore succeed in user space on standard Linux ARM64 kernels (confirmed by Go runtime issue #67937 benchmarks on Raspberry Pi 5, Linux).
- **Frequency:** varies by SoC. Raspberry Pi 5: typically 54 MHz (one tick every ~18.5 ns). The frequency is board-specific; read `cntfrq_el0` to determine it.
- **Frequency-invariant:** yes (fixed hardware reference clock, independent of core frequency).
- **Overhead:** ~28 ns on Raspberry Pi 5 (vs 43 ns for syscall to get CLOCK_MONOTONIC).
- **Verdict: portable wall-time primitive on Linux ARM64.** Same caveats as on macOS: it is wall-time, not compute.

#### `perf_event_open` on Linux ARM64

- Same paranoid-level rules as x86; same privilege model.
- `PERF_COUNT_HW_INSTRUCTIONS` availability depends on the specific ARM PMU implementation. Most server-grade ARM64 chips (Neoverse, Apple A/M series on Linux) expose it; embedded/IoT SoCs may not.
- Verdict: same conditional recommendation as x86 — best work-proxy available, but requires fallback.

---

## Comparison table

| Candidate | Platform | Unprivileged? | Thermal-invariant? | Frequency-invariant? | Counter type |
|---|---|---|---|---|---|
| `CLOCK_THREAD_CPUTIME_ID` | Apple Silicon macOS | Yes | No | No | Wall-time-on-CPU (bug: ~40x underreport on M1) |
| `mach_thread_info(THREAD_BASIC_INFO)` | Apple Silicon macOS | Yes | No | No | Wall-time-on-CPU (1 ms resolution; same underlying accumulator) |
| `CNTVCT_EL0` (`mrs` instruction) | Apple Silicon macOS | Yes | No | Yes | Wall-time, 24 MHz fixed reference |
| `mach_absolute_time` / `CLOCK_MONOTONIC` | Apple Silicon macOS | Yes | No | Yes | Wall-time, 24 MHz fixed reference (same as CNTVCT_EL0) |
| `kpc_*` / `kperf.framework` | Apple Silicon macOS | **No** (root / private entitlement) | Yes | Yes | True hardware PMU (cycles, instructions) |
| `CLOCK_THREAD_CPUTIME_ID` | Linux x86-64 / ARM64 | Yes | No | No | Wall-time-on-CPU (scheduler accounting) |
| `perf_event_open` `PERF_COUNT_HW_INSTRUCTIONS` | Linux x86-64 | Conditional (paranoid ≤ 2) | **Yes** | Yes | Instructions retired (true work proxy) |
| `perf_event_open` `PERF_COUNT_HW_CPU_CYCLES` | Linux x86-64 | Conditional (paranoid ≤ 2) | No | No | Core-clock cycles (not fixed-rate) |
| `perf_event_open` `PERF_COUNT_HW_INSTRUCTIONS` | Linux ARM64 | Conditional (paranoid ≤ 2) | **Yes** | Yes | Instructions retired (PMU availability varies) |
| `rdtsc` | Linux x86-64 | Yes | No | Yes (invariant TSC) | Wall-time, fixed reference rate |
| `CNTVCT_EL0` (`mrs`) | Linux ARM64 | Yes | No | Yes | Wall-time, SoC-specific fixed rate |
| Node count | All | Yes | **Yes** | **Yes** | Self-counted; engine-internal |

---

## Thermal-invariance analysis: the critical point

No publicly accessible user-space counter on these platforms directly measures "work done" independent of clock speed. The distinction:

- **`CLOCK_THREAD_CPUTIME_ID` / `THREAD_BASIC_INFO`:** counts wall-time the thread was scheduled. When the CPU throttles to 50% frequency, the same computation takes 2x wall seconds and accumulates 2x thread-CPU-time. The budget empties 2x faster for the same work. Not thermal-invariant.

- **`CNTVCT_EL0` / `RDTSC`:** count fixed-rate wall-time ticks regardless of core frequency. When the CPU throttles, ticks per unit-of-work increase (the work takes more wall time). Also not thermal-invariant for the purpose of work-budgeting.

- **`perf_event_open` `PERF_COUNT_HW_INSTRUCTIONS`:** counts instructions retired. This IS work-invariant — 1 M instructions executed is 1 M regardless of whether the core ran at 1 GHz or 4 GHz. This is the only accessible counter that isolates compute from scheduling noise. It is not a time counter; it is a proxy for work done. Accessible on Linux at paranoid ≤ 2 per-thread.

- **Node count:** the engine's own node counter. Strictly monotone per unit of search work. Completely immune to all hardware-level noise. The cost is that "time" must be converted to nodes by the caller (GUI, testing harness) using a known NPS rate — the `nodestime` UCI extension is the established protocol mechanism for this (discussed in TalkChess thread on fixed-node testing and the "UCI extension: nodestime" thread).

---

## Gotchas and corner cases

### Apple Silicon `CLOCK_THREAD_CPUTIME_ID` bug

- Reported on macOS 12.5 / Xcode 13.2.1. Returns ~0.024 s for ~1 s of CPU work.
- The bug persists under Rosetta 2.
- No public fix confirmation found. Testing on the project's M3 Mac would confirm or refute whether it still affects current hardware/OS.
- **Action required before using:** verify empirically on the actual target machine with a known-duration busy loop.

### `mach_absolute_time` timebase changed on Apple Silicon

- Intel Macs: numer=1, denom=1 (already nanoseconds).
- M1 Macs: numer=125, denom=3 (one tick = 41.67 ns). Must call `mach_timebase_info` and apply conversion.
- Code that assumed Intel behavior (numer=denom=1) silently produces wrong values on Apple Silicon.
- Rust's `std::time::Instant` handles this correctly by going through the OS conversion.

### `perf_event_open` paranoid variance

- paranoid=2: per-thread hardware counters accessible unprivileged (exclude_kernel=1 required).
- paranoid=3: hardware counters entirely blocked (some hardened distros, CI environments).
- paranoid=1: also allows per-thread hardware counters without the exclude_kernel restriction.
- Tournament harnesses run on diverse hosts; paranoid=3 is plausible. Engine must fall back gracefully.

### `perf_event_open` PMU hardware availability

- `PERF_COUNT_HW_INSTRUCTIONS` not guaranteed on all ARM64 targets.
- On `perf_event_open` returning ENOENT or EOPNOTSUPP: fall back to wall-time budget.

### Fixed-node testing and time-skew

- TalkChess discussion (Don, Komodo, circa 2018) identifies a "time-skew" artifact: engines that dramatically change NPS by game phase (e.g. 4–5x faster in the endgame) produce systematically different decisions under fixed-node versus wall-time budgets.
- The `nodestime` UCI extension partially addresses this at the GUI layer by allowing NPS calibration between moves.
- For SPRT testing, fixed-depth or node-limited games eliminate thermal noise but alter the effective time-control structure.

### `CNTVCT_EL0` on Linux ARM64: hypervisor traps

- Direct `mrs` from user space works on bare-metal Linux ARM64 because the kernel enables `EL0VCTEN`.
- Inside some hypervisors (particularly Hyper-V guests), the bit may not be set, causing an EL1 trap. The vDSO implements a fallback; direct `mrs` without vDSO may fail.

### `rdtsc` on x86: cross-socket skew

- On NUMA systems with multiple physical processors, different sockets may have unsynchronized TSCs.
- For a per-thread time budget where the thread runs on a single physical core, this is rarely an issue in practice. Thread affinity would eliminate it entirely.

---

## Recommendations

### Apple Silicon macOS (primary target)

The engine should **use wall-clock time (`Instant::now()` / `CLOCK_MONOTONIC`) for its time budget**, which is already the current implementation. None of the alternatives offer a better privilege-free compute budget:

- `CLOCK_THREAD_CPUTIME_ID`: disqualified by the Apple Silicon bug.
- `CNTVCT_EL0` / `mach_absolute_time`: equivalent to CLOCK_MONOTONIC at 24 MHz; no additional benefit over what Rust's `Instant` already provides.
- `kpc_*`: requires root; disqualified.

For the engine to be insulated from thermal throttling noise on Apple Silicon, the viable alternative is **node-count budgeting** (see below).

### Linux x86-64 / ARM64

For user-space tournament use, the practical options rank as:

1. **`perf_event_open` with `PERF_COUNT_HW_INSTRUCTIONS`** — best work-proxy available; thermal-invariant; accessible per-thread without root at paranoid ≤ 2 on typical distros. Requires a graceful fallback for paranoid=3 or missing PMU.
2. **`CLOCK_THREAD_CPUTIME_ID`** — always accessible; no fallback needed; thermally non-invariant but much better than wall-time at filtering preemption noise.
3. **`rdtsc`** — lowest overhead (~3 ns); no privilege needed; wall-time equivalent; useful only as a cheap monotonic timer, not as a compute budget.

### Node count as the universal VirtualClock

The most robust approach is to **use the engine's internal node count as the primary compute budget, with wall-time as a deadline backstop**:

- Node count is **strictly thermal-invariant, frequency-invariant, and privilege-free** by construction.
- The engine already tracks node counts for UCI output and SPRT testing.
- The `nodestime` UCI extension (TalkChess thread, adopted in XBoard) provides the protocol scaffolding: the GUI sends `nps NNN` with the `go` command, and the engine converts node budgets to time budgets using that rate.
- Gotcha: requires the calling GUI to support nodestime, or the engine to use a fixed assumed NPS for internal conversion. For SPRT with fastchess, the current wall-time approach remains appropriate since fastchess manages both sides equally.

**Recommendation for M4.D and beyond:** continue using wall-clock time for external protocol compliance; add a node-count soft limit as the internal search-interruption gate. This sidesteps all hardware-counter complexity while naturally achieving thermal invariance for internal decisions.

---

## Open questions

- **Is the `CLOCK_THREAD_CPUTIME_ID` bug fixed on M3 macOS?** The Apple Developer Forums post is dated 2022 on macOS 12. The project runs on M3. An empirical 5-line test (busy-spin 1 s, measure `CLOCK_THREAD_CPUTIME_ID`) would confirm current behavior before any future use.

- **Paranoid level in CI and tournament environments.** If the project ever runs SPRT in a cloud CI environment, the paranoid level of those hosts determines whether `perf_event_open` instructions-retired would be available. This should be probed when the CI setup is chosen, not assumed.

- **Whether Apple will ever expose per-thread instruction counts to unprivileged code.** Instruments and the Xcode profiler can access the PMU, but via a privileged daemon. There is no sign of Apple opening unprivileged PMU access to ordinary binaries.

---

## Sources consulted

- ARM Architecture Reference Manual, CNTVCT_EL0 register specification — [fooptrvoid.github.io](https://fooptrvoid.github.io/arm-mra-2024-12-sysreg/AArch64-cntvct_el0.html)
- Jim Cownie, "Fun with Timers and cpuid," cpufun.substack.com — [cpufun.substack.com](https://cpufun.substack.com/p/fun-with-timers-and-cpuid)
- Go runtime issue #67937 "consider CNTVCT_EL0 to implement cputicks on ARM64" — [github.com/golang/go](https://github.com/golang/go/issues/67937)
- cpucycles.cr.yp.to counters reference (DJB et al.) — [cpucycles.cr.yp.to](https://cpucycles.cr.yp.to/counters.html)
- Apple Developer Forums, "clock_gettime can't get the exact value" (CLOCK_THREAD_CPUTIME_ID M1 bug) — [developer.apple.com](https://developer.apple.com/forums/thread/711929)
- ibireme gist, reading PMU counters on Intel/M1 via kperf — [gist.github.com](https://gist.github.com/ibireme/173517c208c7dc333ba962c1f0d67d12)
- Linux kernel perf-security documentation — [kernel.org](https://www.kernel.org/doc/html/latest/admin-guide/perf-security.html)
- `perf_event_open(2)` man page — [man7.org](https://man7.org/linux/man-pages/man2/perf_event_open.2.html)
- perf_event_open detailed man page — [web.eece.maine.edu](https://web.eece.maine.edu/~vweaver/projects/perf_events/perf_event_open.html)
- Wikipedia, Time Stamp Counter — [en.wikipedia.org](https://en.wikipedia.org/wiki/Time_Stamp_Counter)
- aakinshin.net, TSC vignette — [aakinshin.net](https://aakinshin.net/vignettes/tsc/)
- The Eclectic Light Company, "Inside M1 Macs: Time and logs" — [eclecticlight.co](https://eclecticlight.co/2020/11/27/inside-m1-macs-time-and-logs/)
- osquery PR #7473 (mach user_time/system_time unit bug on M1) — [github.com/osquery](https://github.com/osquery/osquery/pull/7473)
- TalkChess, "fixed nodes testing" — [talkchess.com](https://talkchess.com/viewtopic.php?p=415982)
- TalkChess, "UCI extension: nodestime" — [talkchess.com](https://www.talkchess.com/forum3/viewtopic.php?t=55742)
- Chess Programming Wiki, Engine Testing — [chessprogramming.org](https://www.chessprogramming.org/Engine_Testing)
- Linux LWN, "Full dynticks task/cputime accounting" — [lwn.net](https://lwn.net/Articles/534698/)
- ARM kernel mailing list RFC, enabling CNTVCT userspace on Hyper-V ARM64 — [spinics.net](https://www.spinics.net/lists/arm-kernel/msg774720.html)

---

## Follow-up: direct cycle-counter access

**Scope of this section:** verifies or refutes each specific candidate the original survey left open, using deeper research into ARM architecture docs, Apple Silicon reverse-engineering notes, Linux kernel patches, and Asahi Linux documentation. Conducted April 2026.

---

### Apple Silicon macOS: specific candidates

#### `PMCCNTR_EL0` — the ARM64 architectural cycle counter

**What it is.**

- The ARM64 architectural Performance Monitors Cycle Count Register.
- A true 64-bit cycle counter incrementing at the CPU core clock rate.
- Not a fixed-frequency timer. Counts actual core-clock cycles elapsed.

**Access control mechanism (ARM Architecture Reference Manual).**

- Controlled by `PMUSERENR_EL0` (Performance Monitors User Enable Register), which is itself only writable from EL1 or higher — EL0 (user space) cannot modify it.
- `PMUSERENR_EL0.EN` (bit 0): when 0, all PMU register accesses from EL0 trap to EL1 unless explicitly unlocked by other bits.
- `PMUSERENR_EL0.CR` (bit 2): "Cycle counter Read enable" — when `EN=0` and `CR=1`, EL0 reads of `PMCCNTR_EL0` are allowed. But setting CR requires the kernel to write `PMUSERENR_EL0` from EL1.
- Reset value: architecturally UNKNOWN (implementation-defined). The kernel controls the startup state.

**Apple Silicon macOS kernel behavior.**

- Apple's XNU kernel does NOT set `PMUSERENR_EL0.CR` or `PMUSERENR_EL0.EN` to permit EL0 access.
- Confirmed across multiple sources: FFTW PR #267 discussion (rdolbeau, stevengj, 2021) explicitly states "CNTVCT_EL0 support is enabled by default on macOS running on the M1 (aarch64), but not PMCCNTR_EL0" and "PMCCNTR_EL0 requires being enabled somehow (privileged by default)."
- Daniel Lemire's 2021 blog post on Apple M1 performance counters documents that reading hardware PMU counters requires `wheel` group membership (administrative access / `sudo`), with no unprivileged path described.
- The `mperf` tool (March 2026, lambdafoo.com) and `macos-perf` (siedentop) both explicitly require `sudo`. No unprivileged access path exists in any published tool.
- Attempting `mrs x0, pmccntr_el0` from EL0 without kernel enablement generates an exception (SIGILL or SIGSEGV depending on how the trap is handled). This is not documented by Apple but is consistent with ARM architecture behavior when the trap bits are set.

**Verdict: blocked on Apple Silicon macOS.** No unprivileged path confirmed. The EL1 kernel would need to write `PMUSERENR_EL0` to permit EL0 access, and Apple's XNU does not do this.

---

#### `CNTPCT_EL0` vs `CNTVCT_EL0` — physical vs virtual timer

**`CNTPCT_EL0` (physical counter).**

- Counts at the fixed system reference frequency (same hardware oscillator as `CNTVCT_EL0`).
- Access from EL0 is controlled by `CNTKCTL_EL1.EL0PCTEN`. Apple Silicon XNU does NOT set this bit — physical counter access from EL0 is trapped.
- Attempting `mrs x0, cntpct_el0` from user space on macOS: traps to EL1 (SIGILL in practice).
- Not a cycle counter in any case: same 24 MHz reference clock as `CNTVCT_EL0`.

**`CNTVCT_EL0` (virtual counter).**

- Access controlled by `CNTKCTL_EL1.EL0VCTEN`. Apple Silicon XNU DOES set this bit.
- Confirmed accessible from EL0 via `mrs` without trapping. See original survey above.
- Frequency: 24 MHz on Apple Silicon. This is a timer (wall-time reference), not a cycle counter.
- The 24 MHz rate is ~125x-167x slower than the M1's 3-3.2 GHz execution frequency. There is no way to derive CPU cycles from it.

**Verdict: `CNTPCT_EL0` is blocked; `CNTVCT_EL0` is accessible but is a 24 MHz timer, not a cycle counter.**

---

#### Apple-proprietary system registers (e.g. `SYS_APL_PMC*`)

**What exists.**

- Apple Silicon has a proprietary PMU, distinct from the standard ARM PMU. The control registers are named in reverse-engineered documentation as `SYS_APL_PMCR0_EL1` (encoded `s3_1_c15_c0_0`), `SYS_APL_PMC0` through `SYS_APL_PMC9`, etc.
- The PMU provides 2 fixed counters (cycles, instructions) plus 8 configurable event counters, for up to 10 simultaneous events. Event databases live at `/usr/share/kpep/a14.plist` (M1), `a15.plist` (M2), `as4.plist` (M4), etc.
- These are EL1 registers (`_EL1` suffix = only accessible from EL1 or higher). EL0 reads of these registers trap unconditionally regardless of any enable bit.

**Can they be enabled for EL0 access?**

- The blog post at blog.clf3.org (2024/2025, "Utilizing PMU Event Counters on Apple M3 and M4") found that on a patched kernel, writes to `SYS_APL_PMCR0_EL1` from user space are possible. However:
  - It requires a kernel patch (not stock XNU).
  - Even with patching, writes to the control register are overwritten by the kernel within ~100 microseconds, making sustained use impractical without further kernel modification.
- No stock macOS path exists. No entitlement grants this. No WWDC API was released in macOS 14 or 15 exposing this to ordinary processes.

**Verdict: Apple-proprietary PMU registers are EL1-only and inaccessible from unprivileged EL0 code.** Kernel patching can temporarily expose writes but is not a production-viable approach and does not constitute unprivileged access.

---

#### Any new macOS 14/15 API for per-thread cycles or instructions?

- Searched WWDC 2024 and 2025 releases for new `os_*`, `dispatch_*`, or similar framework APIs exposing hardware performance counters to unprivileged apps.
- No such API found. The `os` framework additions in macOS 14/15 (Sonoma/Sequoia) are focused on concurrency, memory pressure, and workgroup coordination — none expose PMU counters.
- Apple's public performance instrumentation for developers remains Instruments (which requires a privileged daemon) and the `xctrace` command-line tool (also privileged). No unprivileged equivalent exists.

**Verdict: no new macOS API. The situation is unchanged from M1 (2020) to M4 (2025).**

---

### Apple Silicon macOS: summary table (follow-up candidates)

| Register / API | Accessible from EL0 (macOS)? | What it measures | Cycle counter? | Confirmed by |
|---|---|---|---|---|
| `PMCCNTR_EL0` (`mrs`) | No — trapped, kernel does not enable | Core-clock cycles | Yes | FFTW PR #267 discussion; Lemire 2021; multiple tool READMEs |
| `CNTPCT_EL0` (`mrs`) | No — `EL0PCTEN` not set by XNU | 24 MHz wall-time | No | ARM arch ref; expected from XNU behavior |
| `CNTVCT_EL0` (`mrs`) | Yes — `EL0VCTEN` set by XNU | 24 MHz wall-time | No | Original survey; FFTW PR #267 |
| `SYS_APL_PMC*` (`mrs`) | No — EL1 register, traps unconditionally | Cycles, instructions, events | Yes (cycles) | blog.clf3.org; ibireme gist; kperf tools |
| `kpc_get_thread_counters` | No — requires root or private entitlement | Cycles, instructions | Yes | ibireme gist; mperf README; macos-perf README |
| New macOS 14/15 API | None exists | N/A | N/A | WWDC 2024/2025 search |

**Overall verdict for Apple Silicon macOS: the original survey's conclusion stands and is confirmed. No privilege-free cycle counter (or instruction counter) is accessible from ordinary unprivileged user-space code. The access-control mechanisms are layered — the ARM architectural trap bits in `PMUSERENR_EL0`, the Apple-proprietary EL1-only register design, and the `kpc` kernel subsystem's root requirement all independently block access.**

---

### Linux ARM64: specific candidates

#### `PMCCNTR_EL0` via direct `mrs` — is it accessible?

**Standard Linux ARM64 behavior.**

- On standard Linux ARM64 kernels, `PMUSERENR_EL0` is NOT configured to allow EL0 reads of `PMCCNTR_EL0` by default.
- From the ARM64 Linux kernel docs: "For general ARM64 systems, access to the PMU cycle counter from user space is not enabled by default in the arm64 Linux kernel. It is possible to enable cycle counter for user space access by configuring the PMU from the privileged mode (kernel space)." (Confirmed by arm-arm-kernel list RFC and zhiyisun.github.io 2016 write-up.)
- The historical approach (Linux kernel modules like `armv8_pmu_cycle_counter_el0` by jerinjacobk) required a kernel module to set `PMUSERENR_EL0.CR` from EL1, then user space could do `mrs x0, pmccntr_el0`. This requires loading a kernel module (root).

**Linux 5.16+ perf_user_access path (indirect enablement).**

- A kernel patch series (Rob Herring, v12, merged ~Linux 5.16) added `arm64: perf: Enable PMU counter userspace access for perf event`.
- Mechanism: open a `perf_event_open()` fd with `config1 = 3` (user access enabled, 64-bit). The kernel then sets `PMUSERENR_EL0` appropriately for that task and exposes the counter index via the mmap'd perf page, allowing `mrs` reads of the specific counter without a syscall per read.
- Also requires `kernel.perf_user_access` sysctl to be enabled (a per-system knob an admin must set; default state varies by distro).
- Privilege for `perf_event_open()` itself: same paranoid-level rules as any perf event. At `perf_event_paranoid ≤ 2`, unprivileged per-thread events are allowed.
- **This path does give unprivileged direct `mrs` access to a PMU cycle counter on Linux ARM64**, subject to: (a) kernel ≥ 5.16, (b) `kernel.perf_user_access = 1`, (c) `perf_event_paranoid ≤ 2`, (d) perf fd opened with config1=3. All four conditions must hold simultaneously.

**Asahi Linux (Apple Silicon running Linux).**

- The Asahi Linux kernel uses Apple's proprietary PMU driver (`apple_m1_cpu_pmu`), not the standard ARM PMU driver. This driver exposes cycles and instructions via `perf stat -e apple_firestorm_pmu/cycles/` and `apple_firestorm_pmu/instructions/`.
- The `perf_user_access` path (direct `mrs` to `PMCCNTR_EL0`) is specific to the standard ARM PMU driver. On Asahi Linux, the Apple proprietary PMU driver does not implement this feature (as of April 2026 — no documentation found confirming it does).
- The Apple PMU driver does not expose `PMCCNTR_EL0` (a standard ARM register); it uses Apple's proprietary `SYS_APL_PMC*` registers. Whether the `config1` userspace-access path is wired up for the Apple PMU driver is an open question — no documentation found confirming it.
- `perf stat` itself works on Asahi Linux bare-metal for cycles and instructions, using the core-type-qualified syntax. Privilege for this depends on the system's paranoid level.

**Verdict: `PMCCNTR_EL0` direct access on standard Linux ARM64 is possible but requires four simultaneous conditions (kernel version, sysctl, paranoid level, and perf fd setup). On Asahi Linux (Apple Silicon), the Apple proprietary PMU driver replaces the standard path, and whether it supports the userspace-direct `mrs` path is unconfirmed.**

---

#### `perf_event_open` with `PERF_COUNT_HW_CPU_CYCLES` — paranoid=2 on ARM64

**The privilege model.**

- `perf_event_paranoid` is an integer sysctl at `/proc/sys/kernel/perf_event_paranoid`.
- At paranoid=2 (the Linux default since 4.6): "per-process performance monitoring only; CPU and system events happening when executing in user space only can be monitored."
- This means an unprivileged process can call `perf_event_open(pid=0, cpu=-1, exclude_kernel=1)` and successfully get a file descriptor for its own thread's user-space-only hardware counters.
- Confirmed by the Linux kernel's `docs/admin-guide/perf-security.rst`: paranoid=2 allows per-process user-space-only monitoring for unprivileged users.
- `PERF_COUNT_HW_CPU_CYCLES` at paranoid=2: counts user-space CPU cycles only (kernel cycles excluded by `exclude_kernel=1`). This IS accessible unprivileged at paranoid=2 per the documentation.

**ARM64-specific considerations.**

- The `perf_event_open` privilege model is architecture-neutral — it is implemented in the kernel's `perf_event.c` core, not per-architecture.
- On ARM64, the backend that satisfies `PERF_COUNT_HW_CPU_CYCLES` is the platform's PMU driver. On standard ARM64 (Neoverse, Cortex-A series), this is the ARM PMU driver and `PMCCNTR_EL0` is the hardware register used.
- On Asahi Linux, the Apple proprietary PMU driver satisfies it via `SYS_APL_PMC*` registers.
- In both cases, the privilege check is the same paranoid-level gate.
- PMU hardware availability: `PERF_COUNT_HW_CPU_CYCLES` is reliably supported on server-grade ARM64 (AWS Graviton, Ampere Altra, Apple M-series on Linux). It may fail on embedded ARM64 SoCs that implement a minimal PMU.

**What "CPU cycles" measures on ARM64 (not work-invariant clarification).**

- `PERF_COUNT_HW_CPU_CYCLES` on ARM64 counts core-clock cycles. When the CPU is thermally throttled (lower frequency), the same computation takes more wall-time but the cycle count per instruction is stable. The cycle counter moves slower during throttling, so the absolute cycle budget is consumed at the same rate per unit of work — but wall-time stretches.
- This means CPU cycles ARE more stable than wall-time for work-budgeting during frequency scaling, but they are not strictly frequency-invariant across different throttle states. If the engine uses a fixed cycle budget, it gets less wall-time when running at full speed (turbo) and more wall-time when throttled — the inverse of wall-time budgeting.
- For chess tournament purposes (fixed time per move), wall-time is what the clock measures, so wall-time budgeting is correct. CPU cycles do not help for time-control compliance.

**Verdict: `PERF_COUNT_HW_CPU_CYCLES` via `perf_event_open` IS accessible unprivileged at the default `perf_event_paranoid=2` on Linux ARM64, including Asahi Linux. This confirms what the original survey said for x86 also applies to ARM64. The underlying counts are core-clock cycles (not frequency-invariant), so they are not a better compute-budget primitive than the existing node counter.**

---

### Linux ARM64: `mrs x0, cntvct_el0` vs true cycle counter

- `mrs x0, cntvct_el0` gives the fixed-frequency system counter (same register as on Apple Silicon, just at a different SoC-defined frequency). This is what the Linux ARM64 vDSO uses for `CLOCK_MONOTONIC`. It is accessible unprivileged (kernel sets `EL0VCTEN`). It is a timer, not a cycle counter.
- For a true cycle counter on Linux ARM64 without `perf_event_open`, the only path is the `perf_user_access` sysctl + `config1=3` approach described above (Linux 5.16+), which still requires `perf_event_open()` as setup and only works if the sysctl is enabled.
- There is no ARM64 equivalent of x86's `rdtsc` that gives a universally accessible fixed-rate counter without kernel cooperation. `CNTVCT_EL0` is the closest analogue but runs at 24–54 MHz depending on SoC (vs x86 invariant TSC at 2–4 GHz).

---

### Linux ARM64: summary table (follow-up candidates)

| Candidate | Accessible unprivileged at paranoid=2? | What it measures | Notes |
|---|---|---|---|
| `perf_event_open` `PERF_COUNT_HW_CPU_CYCLES` | Yes (pid=0, cpu=-1, exclude_kernel=1) | Core-clock cycles (user-space only) | Confirmed at paranoid=2; not work-invariant |
| `perf_event_open` `PERF_COUNT_HW_INSTRUCTIONS` | Yes (same conditions) | Instructions retired | Work-invariant; best available proxy |
| `mrs x0, pmccntr_el0` (direct) | No by default; yes if `perf_user_access=1` + Linux 5.16+ + config1=3 perf fd | Core-clock cycles | Four conditions must hold; not a general baseline |
| `mrs x0, cntvct_el0` (direct) | Yes (EL0VCTEN set) | Fixed-rate system timer (24–54 MHz SoC-dependent) | Not a cycle counter; same as CLOCK_MONOTONIC |

---

### Revised overall verdict

**Apple Silicon macOS:**

The original survey conclusion is confirmed and strengthened. No privilege-free cycle counter (or instruction counter) is accessible from ordinary unprivileged user-space code. Every candidate is blocked by at least one of:

- ARM architectural trap bit (`PMUSERENR_EL0`) controlled by EL1 (kernel), not set by Apple's XNU for EL0 access.
- Apple proprietary register namespace requiring EL1 privilege by design.
- `kpc` kernel subsystem requiring root or non-grantable private entitlement.
- No new public API from Apple (WWDC 2024/2025 checked).

The only accessible counters are wall-time references (`CNTVCT_EL0` at 24 MHz, equivalent to what `Instant::now()` already gives). No cycle counter or instruction counter is reachable.

**Linux ARM64:**

The original survey's claim that `perf_event_open` with `PERF_COUNT_HW_INSTRUCTIONS` is accessible at paranoid=2 is confirmed, and extended to include `PERF_COUNT_HW_CPU_CYCLES` under the same conditions. Both work for the unprivileged process's own thread. Hardware availability (PMU driver) is the practical risk on embedded targets; on server-grade ARM64 and on Asahi Linux, it is reliably available.

The Linux 5.16+ `perf_user_access` sysctl path for direct `mrs` access to `PMCCNTR_EL0` exists but requires four simultaneous conditions that cannot be assumed in a tournament harness. It is a research/profiling path, not a portable production path.

**Implication for this project:** the recommendation from the original survey stands. Wall-clock time (`Instant::now()`) for Apple Silicon; node count as the portable work-budget primitive; `perf_event_open` instructions-retired as an optional enhancement on Linux for operators who want thermal invariance. No redesign of ELOH.C is warranted based on this follow-up.

---

### Additional sources (follow-up section)

- FFTW PR #267, "default to CNTVCT_EL0 cycle counter on Apple M1" — [github.com/FFTW/fftw3](https://github.com/FFTW/fftw3/pull/267)
- Daniel Lemire, "Counting cycles and instructions on the Apple M1 processor" (2021) — [lemire.me](https://lemire.me/blog/2021/03/24/counting-cycles-and-instructions-on-the-apple-m1-processor/)
- lambdafoo.com, "Quick Hardware Performance Counters on macOS ARM64" (March 2026) — [lambdafoo.com](https://lambdafoo.com/posts/2026-03-25-mperf-hardware-counters-macos.html)
- blog.bugsiki.dev, "PMU Counters on Apple Silicon" — [blog.bugsiki.dev](https://blog.bugsiki.dev/posts/apple-pmu/)
- blog.clf3.org, "Utilizing PMU Event Counters on Apple M3 and M4" — [blog.clf3.org](https://blog.clf3.org/post/pmu-event-counters/)
- Jon's Arm Reference, PMUSERENR_EL0 — [arm.jonpalmisc.com](https://arm.jonpalmisc.com/latest_sysreg/AArch64-pmuserenr_el0)
- ARM Developer docs, PMCCNTR_EL0 — [developer.arm.com](https://developer.arm.com/documentation/ddi0595/2021-03/AArch64-Registers/PMCCNTR-EL0--Performance-Monitors-Cycle-Count-Register)
- Linux kernel LKML, "arm64: perf: Enable PMU counter userspace access for perf event" (patch v11, Rob Herring, 2021) — [lkml.iu.edu](https://lkml.iu.edu/hypermail/linux/kernel/2110.2/03899.html)
- LWN.net, "arm64 userspace counter support" (2021) — [lwn.net](https://lwn.net/Articles/878150/)
- Linux kernel docs, ARM64 perf — [docs.kernel.org](https://docs.kernel.org/arch/arm64/perf.html)
- Linux kernel docs, perf-security — [docs.kernel.org](https://docs.kernel.org/admin-guide/perf-security.html)
- Asahi Linux wiki, "perf on M1 systems" — [leo3418.github.io](https://leo3418.github.io/asahi-wiki-build/perf-on-m1-systems/)
- zhiyisun.github.io, "How to Use PMU of 64-bit ARMv8-A in Linux" (2016) — [zhiyisun.github.io](http://zhiyisun.github.io/2016/03/02/How-to-Use-Performance-Monitor-Unit-(PMU)-of-64-bit-ARMv8-A-in-Linux.html)

---

## Follow-up — Linux ARM64 VM on Apple Silicon

**Research date:** April 2026.

**Question:** can an unprivileged user-space process inside a Linux ARM64 guest VM running on Apple Silicon macOS get a working `perf_event_open` counter for `PERF_COUNT_HW_INSTRUCTIONS`?

---

### Verdict

**No. No widely-available macOS-host VM stack exposes working hardware performance counters (`PERF_COUNT_HW_INSTRUCTIONS` or any other `PERF_TYPE_HARDWARE` event) to an unprivileged Linux ARM64 guest process as of April 2026.**

This is not a paranoid-level problem or a distro configuration problem. It is a structural hardware problem: Apple's PMU is proprietary, and Apple does not expose it through any hypervisor interface.

---

### The blocking mechanism

The chain of failures, layer by layer:

**Layer 1 — Apple's PMU is non-standard.**

- Apple Silicon does not implement the architectural ARMv8 PMU (`PMCCNTR_EL0`, `PMEVCNTR<n>_EL0`).
- Apple uses a proprietary PMU accessed via `SYS_APL_PMC*` registers at EL1.
- These registers have no standard ARM equivalent. Guest kernel PMU drivers written for standard ARM PMU hardware cannot use them.

**Layer 2 — Apple does not expose any PMU to VM guests.**

- The Asahi Linux wiki states explicitly: "This will never work in a VM, because Apple do not support the standard ARM performance counters (they use a custom PMU) and they do not expose proprietary features to VM guests."
- Apple's Hypervisor.framework (the API underlying UTM, Lima, Tart, OrbStack, and other macOS-hosted VMs) has no public API for PMU virtualization. The framework's documented vCPU capabilities cover general registers, system registers, and interrupt controller state — not performance monitoring.
- Apple Hypervisor.framework update notes for macOS 13, 14, and 15 contain no PMU-related API additions.

**Layer 3 — QEMU cannot work around this.**

- QEMU's HVF (Hypervisor.framework) backend for Apple Silicon ARM64 was added in 2021 and enables near-native guest execution. It does not implement vPMU.
- When a QEMU guest CPU type is set to "host" (passthrough), no PMU is exposed at all — dmesg shows no PMU driver loaded.
- When set to a specific CPU model (e.g. cortex-a72), QEMU exposes a virtual PMU via device model. The guest kernel loads the ARM PMU driver, which expects architectural PMU registers (`PMCCNTR_EL0`, etc.). But the virtual PMU cannot be backed by Apple's real hardware PMU (which is proprietary and inaccessible to the hypervisor). The result: dmesg shows "PMU detected" but `perf list` shows no hardware events, and `perf stat -e instructions` returns "not supported" or 0 counts. This is confirmed in the UTM GitHub issue #4200 (open, unresolved as of April 2026).
- The UTM issue itself explicitly lists the problem: "probably requires multiple moving parts: qemu support for PMU virtualization/passthrough, UTM setting the right options, guest Linux kernel knowledge of whatever PMU is exposed (is it Apple-specific?), and knowledge of the right counters in the perf userspace tool."

---

### Per-VM-stack summary

| VM stack | Hypervisor backend | PMU exposed to guest? | `perf stat -e instructions` result | Status as of April 2026 |
|---|---|---|---|---|
| UTM (QEMU mode) | QEMU + Apple HVF | No (cpu=host) / broken (cpu=cortex-a72) | Not supported / 0 counts | Open issue #4200; no resolution |
| UTM (Apple Virtualization.framework mode) | Apple Virtualization.framework | No | Not supported | No API exists |
| Lima | Apple Virtualization.framework (default) | No | Not supported | GitHub issue #2351 open; user reports empty `perf list hw` |
| Tart | Apple Virtualization.framework | No | Not supported | Tart is a thin CLI wrapper; inherits Virtualization.framework limits |
| OrbStack | Apple Virtualization.framework | No | Not supported | No PMU documentation; same structural constraint |
| Parallels Desktop | Proprietary (parallel.framework) | Unknown / unconfirmed for ARM64 | Not documented | No ARM64 PMU support reported in forums or docs |
| VMware Fusion | Apple Virtualization.framework (ARM mode) | No | Not supported | VMware Fusion on ARM uses Virtualization.framework; same constraint |
| Asahi Linux (bare-metal, not a VM) | None — native Linux on Apple Silicon | Yes (via apple_m1_cpu_pmu driver) | Works with explicit core-type syntax: `perf stat -e apple_firestorm_pmu/instructions/` | Functional on bare metal only |

---

### Paranoid-level note (not the gating issue)

- Ubuntu 22.04 / 24.04 default: `perf_event_paranoid = 4` (restrictive).
- Debian 12 default: `perf_event_paranoid = 2` (permissive for per-thread counters).
- Even at paranoid=2 or paranoid=-1, the above VM stacks still return no hardware events. The paranoid sysctl governs whether the kernel's perf subsystem permits access; it cannot grant access to hardware counters that the hypervisor never exposed. `ENOSYS` or `EOPNOTSUPP` is the error from `perf_event_open`, not `EPERM` (which would be the paranoid-level error).

---

### Per-thread vs per-VM counter accounting (moot)

- Per-thread `perf_event_open` accounting correctness across VM scheduling is a real concern on hypervisors that do expose hardware counters (e.g. KVM on x86 with Intel/AMD PMU virtualization).
- On Apple Silicon hosts, this question is moot: no counter values are produced at all. There is no counter time to account for incorrectly.

---

### Is there any path forward?

The only scenario where hardware performance counters work inside a Linux ARM64 guest on Apple Silicon is bare-metal Asahi Linux — which is not a guest VM scenario. It is Linux running natively on the Apple Silicon hardware with a custom kernel driver for Apple's proprietary PMU.

No VM path exists because:

- Apple does not expose the PMU through any hypervisor interface.
- The PMU is EL1-only and proprietary; a guest kernel cannot access it even if the hypervisor tried to pass it through.
- No hypervisor can synthesize accurate hardware instruction counts without hardware support.

---

### Sources (this section)

- Asahi Linux wiki, "perf on M1 systems" — [leo3418.github.io](https://leo3418.github.io/asahi-wiki-build/perf-on-m1-systems/) — explicit statement: "This will never work in a VM."
- UTM GitHub issue #4200, "Expose usable PMU (perf counters) to guest" — [github.com/utmapp/UTM](https://github.com/utmapp/UTM/issues/4200) — open, unresolved; user reports no hardware events in guest.
- Lima GitHub issue #2351, "Example for access to performance counters?" — [github.com/lima-vm/lima](https://github.com/lima-vm/lima/issues/2351) — open; user reports empty `perf list hw` inside Lima guest.
- Tart Hacker News discussion — [news.ycombinator.com](https://news.ycombinator.com/item?id=39059100) — confirms Tart is a thin wrapper around Apple Virtualization.framework.
- Apple Hypervisor.framework documentation — [developer.apple.com](https://developer.apple.com/documentation/hypervisor) — no PMU API present.
- Apple Hypervisor.framework update history — [developer.apple.com](https://developer.apple.com/documentation/updates/hypervisor) — no PMU-related additions in macOS 13/14/15.

---

## Follow-up — `CLOCK_THREAD_CPUTIME_ID` empirical probe (Apple M4 / macOS 26.4.1, 2026-04-30)

**Question.** The 2021 reports of `CLOCK_THREAD_CPUTIME_ID` ~40× underreporting on M1 are well-documented; status on later silicon (M2/M3/M4) and later macOS (14/15/26) is undocumented. Before committing the engine-side `VirtualClock` UCI option to use `CLOCK_THREAD_CPUTIME_ID`, verify it accumulates correctly on the development machine.

**Probe.** Compile-and-run a standalone C program that runs three trials of a tight CPU-bound integer-multiplication loop (~1 second each, output sunk to a `volatile` to defeat optimization), sampling `CLOCK_MONOTONIC` and `CLOCK_THREAD_CPUTIME_ID` before and after each trial, and printing the ratio. On healthy hardware the ratio should be ≈ 1.0 (single-threaded CPU-bound work spends nearly all wallclock on CPU). On the M1-bug symptom the ratio is ~ 0.025 (1/40th).

**Hardware.** Apple M4, 4 P-cores + 6 E-cores, 32 GB. macOS 26.4.1.

**Result.**

```
trial 0: wall=958.269 ms  cpu=957.595 ms  cpu/wall=0.9993
trial 1: wall=960.118 ms  cpu=959.432 ms  cpu/wall=0.9993
trial 2: wall=959.472 ms  cpu=958.836 ms  cpu/wall=0.9993
```

**Verdict.** `CLOCK_THREAD_CPUTIME_ID` is healthy on M4 / macOS 26. Three trials consistently within 0.07% of unity. The M1 bug is not present in this generation. ELOH.C may use `CLOCK_THREAD_CPUTIME_ID` as the engine-side `VirtualClock` time source on this machine without underreporting hazard.

**Caveats this probe does NOT verify.**
- Thermal-throttling behavior under load. Known-and-accepted limitation: when the P-core clocks down under heat, more CPU-time-units accumulate per unit of work, so CPU-time is *not* fully thermal-invariant. This is mitigated in the deployment plan (P-core pinning + external cooling), not by the probe.
- Behavior on E-cores. The work loop here ran on whichever core macOS scheduled it on; E-cores have lower IPC than P-cores, so the same wallclock spent on an E-core would represent less work. P-core pinning addresses this.
- Older Apple Silicon (M1/M2/M3) on macOS 26. The probe is single-machine. The M1 bug should be retested if ELOH.C is run on a different chip.
- Machines other than the developer's. Future contributors should re-run the probe on their own hardware before relying on the metric.
