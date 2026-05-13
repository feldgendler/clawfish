# ELOH.C — Hardware-invariant TC: `VirtualClock` UCI option

The engine-side hardware-invariance phase. Adds a `VirtualClock` UCI option that swaps the search's wallclock time source (`Instant::now()`) for thread CPU time (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`), and adds harness-side handshake-driven negotiation that turns the option on for advertising opponents (clawfish-vs-clawfish self-play). Closes the ELOH milestone.

Spec source: `docs/tooling/elo-iteration-harness.md` ELOH.C section. Research: `docs/research/tooling-cpu-cycle-counters.md` (initial survey + Apple-Silicon-specific follow-up + Linux-VM follow-up + M4 empirical CLOCK_THREAD_CPUTIME_ID probe). Validation precedent: ELOH.A's `MatchTimeMode` seam at `src/match_clock.rs`; M3.E's `compute_caps` time-management.

**ADR-0021 lands with this phase.** Title: `VirtualClock UCI option — thread CPU time as search time-source for hardware-invariant TC`. Captures: time-source decision (POSIX `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`); cycle-counter alternatives explicitly *rejected* (privileged on Apple Silicon — research); `--go-nodes N` explicitly *rejected* (implementation-coupled even within one binary across runtime settings, per user 2026-04-30); `MoveOverhead` reinterpretation (still ms, but of CPU time when VC=true); compute_caps wallclock-ms-as-CPU-ms drift bound (cpu/wall = 0.9993 on M4); the architecturally load-bearing **per-thread time-source ownership** decision (worker-local `SearchClock` struct, not orchestrator-pre-computed deadlines on `SearchContext`).

## 0. Sizing note

Estimated total: ~190 prod LOC + ~140 test LOC = ~330, well within the workflow's 300-800 typical band. Larger than v1's ~250 estimate because the plan-review must-fix on per-thread CPU-time semantics requires moving `start`/`deadline`/`soft_deadline` off `SearchContext` into a worker-local `SearchClock` struct, with corresponding test-site updates (~16 SearchContext-construction sites across `src/search.rs::tests` and `src/engine.rs::tests`). Slice A's coder-agent prompt accounts for the test-site fanout.

## 1. Goals

- New UCI option `option name VirtualClock type check default false`. When `true`, search time-keeping uses thread CPU time; when `false` (default), wallclock — unchanged from M3.E.
- Engine-side time source: `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` via direct `libc` call. POSIX call works on macOS and Linux. Gated `#[cfg(unix)]`; Windows builds (not currently a target but the codebase compiles on Linux/macOS) get neither the UCI advertisement nor the `setoption` acceptance.
- **Time-source ownership lives in the worker thread, not the orchestrator.** `CLOCK_THREAD_CPUTIME_ID` is a *per-thread* counter; values are not comparable across threads. The orchestrator computes `caps: TimeCaps` (durations only, no clock reads) and passes them through `SearchContext`; the worker thread reads its own clock at entry of `Search::go` and constructs a worker-local `SearchClock` carrying `start`/`deadline`/`soft_deadline`.
- Harness-side: parse opponent's `option name VirtualClock ...` line from the `uci`-handshake response (case-insensitive name match per UCI spec); conditionally send `setoption name VirtualClock value true` if both (a) the opponent advertises the option and (b) the new harness flag `--virtual-clock` is set. Falls back silently when the opponent doesn't advertise (Stockfish's case).
- New CLI flag `--virtual-clock` on `src/bin/elo-iterate.rs`. Default off — match ELOH.A/B's behavior unchanged. Opt-in mode for clawfish-vs-clawfish hardware-invariant runs.
- `MoveOverhead` reinterpretation: under VirtualClock=true, the option still expresses milliseconds, but of CPU time. Documented in the option's UCI description and ADR-0021. Acknowledged degenerate case: the wallclock-jitter hedge that motivated `MoveOverhead=50` is partially meaningless under CPU-time TC; the option becomes a small fixed-cost conservatism. Left as-is; harmless at the 50ms default.
- `compute_caps`'s output is wallclock-ms by construction (it divides UCI-protocol `wtime`/`btime` etc. — wallclock fields). Under VC=true those ms are reinterpreted as CPU-ms; the wall ↔ CPU drift is bounded on M4 at cpu/wall = 0.9993 (search is CPU-bound). Documented in §10 risk register + ADR-0021.
- Mate-distance-pruning interaction: M3.E's MDP is algorithmically agnostic to the time source; confirm in tests (§6.3).
- Back-validation gate Part 1 (engine-side under simulated CPU load) — see §7.
- **`bench` path note.** `Engine::handle_bench` invokes `Search::go` synchronously on the orchestrator thread (per M3.F's deterministic-bench design). The "worker thread" framing in §4.2 is a degenerate single-thread case under `bench`: the orchestrator thread *is* the constructing thread, so `SearchClock::start_for(...)` reads the calling thread's CPU clock and the per-thread invariant is trivially satisfied. Bench under `VirtualClock=true` is a valid invocation path; node-count output is **byte-identical** to bench under `VirtualClock=false` because `bench` is fixed-depth, time-source-irrelevant (the time source affects only deadline-based abort decisions, and a fixed-depth search has `caps = (MAX, MAX)` ⇒ no deadlines ⇒ time source unused). The §7 step-6 bench captures this — VC=true vs VC=false bench measures syscall-overhead on the wallclock side, not search behavior.

## 2. Out of scope

- **`--go-nodes N` (the `MatchTimeMode::Nodes(u64)` seam variant) — DROPPED.** Per user decision 2026-04-30: nodes-per-work-unit is implementation-coupled and shifts even within a single binary across runtime settings (Hash size, eval weights, etc.). The seam itself stays in the codebase as already-tested dead code (ELOH.A landed it; removing it is more churn than retention; future work may revive it as a non-load-bearing diagnostic if dual-boot Linux setup happens). Spec doc ELOH.C row updated to record the drop with rationale; spec doc's §"In scope (harness-side, ~70 LOC)" item 6 amended to strike the `--go-nodes` line.
- PMU instruction-counting / cycle counter (the "even sharper" follow-up in `docs/tooling-backlog.md`). Confirmed inaccessible without root or non-grantable private entitlements on Apple Silicon (research §"Follow-up — direct cycle-counter access"). Confirmed inaccessible inside any common Linux ARM64 VM on Apple Silicon (research §"Follow-up — Linux ARM64 VM on Apple Silicon"). Defer indefinitely; revisit only if/when the user dual-boots Asahi Linux or moves primary development to a non-Apple ARM64 / Linux x86 target.
- Cross-engine `VirtualClock` enforcement. Stockfish doesn't support it; harness silently skips the setoption when the option isn't advertised.
- Adaptive `MoveOverhead` under VirtualClock. The wallclock-jitter hedge at default 50ms is left untouched even though it's semantically odd under CPU-time TC.
- M4-coupled forward-validation runs (M4.A's TT-vs-no-TT rating estimate under `--virtual-clock`). Optional follow-up after the phase lands; not a gate.
- Windows engine-side support. The option is `#[cfg(unix)]`-gated; on Windows the engine doesn't advertise it and rejects `setoption name VirtualClock value true`. The harness's silent-fallback path handles this uniformly.

## 3. Files modified

| File | Change | LOC est |
|---|---|---|
| `src/search.rs` | New `SearchInstant` enum (Wall/Cpu variants) + impl. New `SearchClock` struct (worker-local; owns `start`/`deadline`/`soft_deadline` + `should_abort` + `is_soft_reached_at` + `elapsed_at`). `SearchContext` loses `deadline`/`soft_deadline`/`start` fields; gains `caps: TimeCaps` + `virtual_clock: bool`. `SearchContext::should_abort` removed (moved to `SearchClock`). The ~16 test-site `SearchContext { start: Instant::now(), deadline: ..., ... }` constructions change to `SearchContext { caps: TimeCaps { ... }, virtual_clock: false, ... }` plus a paired `SearchClock::start_for(false, caps)` where the test calls a time-aware method (the test-site migrations net to roughly 0 LOC — field rename, not addition). New private `read_thread_cpu_ns()` libc shim; gated `#[cfg(unix)]` (Windows path: panic on construction of `SearchInstant::Cpu` — never reachable because option is unadvertised on Windows). | +95 / -25 |
| `src/engine.rs` | New field `Engine::virtual_clock: bool` (default `false`). `handle_uci` emits the new option line (under `#[cfg(unix)]` only). `handle_setoption` parses `VirtualClock` (under `#[cfg(unix)]`; on non-unix, the name match arm returns "rejected — option unsupported on this platform"). `handle_go` no longer reads any clock; it computes `caps = compute_caps(...)`, sets `SearchContext { caps, virtual_clock, ... }`, and the worker constructs the `SearchClock` at entry. | +35 / -8 |
| `src/bin/elo-iterate.rs` | `cli::Args::virtual_clock: bool` field + `--virtual-clock` flag (default `false`); `driver::EngineCapabilities::supports_virtual_clock: bool` field populated by parsing `option name VirtualClock` lines from the uci-handshake response (case-insensitive name match); `driver::wait_for_uciok` extended to return `EngineCapabilities` alongside; `controller::production_worker_fn` (and the ELOH.A pre-controller equivalent in `match_loop` if it has a separate handshake path) sends `setoption name VirtualClock value true` after `uciok` when both `args.virtual_clock` and `caps.supports_virtual_clock` are true. **No change to `MatchTimeMode`.** | +55 |
| `src/match_clock.rs` | Untouched (the `Nodes(u64)` variant remains as unconstructible-from-CLI dead code per ELOH.A). | 0 |
| `Cargo.toml` | Add `libc = "0.2"` dependency. (The existing crate has no direct libc dep, per a search through `src/`. The harness binary doesn't need libc — only `src/search.rs::read_thread_cpu_ns` does.) | +1 |
| `docs/decisions/0021-virtual-clock-uci-option.md` | New ADR. | +110 |
| `docs/tooling/elo-iteration-harness.md` | ELOH.C row → done; scope detail → "Done" prose with actual landing size; explicitly amend `§"In scope (harness-side, ~70 LOC)"` item 6 to strike `--go-nodes N` line and add a one-paragraph rationale block; cross-link Part 1 result. | re-state |
| `docs/tooling-backlog.md` | "Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension" entry → "Done" block with caveats (CPU time used; cycles/instructions privileged on Apple Silicon and inaccessible in any common macOS-host VM; `go nodes` dropped per implementation-coupling concern). | re-state |
| `docs/architecture.md` | Settled-commitments row + small note in the search-v1 subsection ("Time-source — wallclock by default; CPU time when `VirtualClock=true` per ADR-0021; ownership in worker thread"). | +6 |
| `docs/research/tooling-cpu-cycle-counters.md` | Already at v4 (survey + 3 follow-ups including M4 probe); no further additions in this phase unless Part 1's empirical data warrants. | 0 |
| `docs/research/tooling-virtual-clock-validation.md` | New file. Created post-Part-1 manual back-test, recording engine-side σ-comparison under load. Lands as follow-up commit, atomic with phase landing. | new ~60 |
| `bench/eloh-c.md` | New milestone bench file. Pre/post `bench` invocation under `VirtualClock=false` (default; verifies no regression on default path) and `VirtualClock=true` (characterizes VC-on path's overhead — expected to be syscall-cost dominated, ~µs per cancellation cadence). | new ~30 |
| `.cargo/mutants.toml` | Anticipated additions only if survivors emerge in pre-review. | +0..10 |

## 4. Type definitions and key signatures

### 4.1 `SearchInstant` (new, `src/search.rs`)

```rust
/// Search-time instant. Either wallclock-based (`std::time::Instant`) or
/// thread-CPU-time-based (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`-derived
/// nanoseconds). Selected once per `Search::go` invocation by the engine's
/// `VirtualClock` UCI option (ELOH.C / ADR-0021).
///
/// **Per-thread invariant (load-bearing):** `Cpu` variants are only valid
/// within the *single* thread that constructed them via `now(true)`. The
/// `Cpu` clock is a per-thread counter (POSIX `CLOCK_THREAD_CPUTIME_ID`);
/// comparing `Cpu` values across threads is meaningless. `SearchClock`
/// (the worker-local struct that owns the values) enforces this by
/// being constructed inside `Search::go` after the worker thread has
/// started.
///
/// **Same-variant invariant:** all `SearchInstant`s held by a single
/// `SearchClock` carry the same variant. Cross-variant comparison /
/// subtraction is `unreachable!()` — the contract is enforced via the
/// type system + unreachable.
#[derive(Debug, Clone, Copy)]
pub enum SearchInstant {
    Wall(Instant),
    /// Nanoseconds from the per-thread CLOCK_THREAD_CPUTIME_ID origin.
    /// Only meaningful within the constructing thread; deltas across
    /// threads are nonsense.
    ///
    /// Variant present on all platforms (the type's set of variants is
    /// not platform-conditional, to keep pattern-matching ergonomic);
    /// `now(true)` calls `read_thread_cpu_ns()` which is `#[cfg(unix)]`
    /// and on non-unix `now(true)` panics with
    /// `unreachable!("VirtualClock not supported on non-unix platforms")`.
    /// In practice this is unreachable in normal flows: `handle_uci`
    /// doesn't advertise the option on non-unix and `handle_setoption`
    /// rejects the value, so `Engine::virtual_clock` cannot become
    /// `true` on non-unix.
    Cpu(u64),
}

impl SearchInstant {
    /// Read the appropriate clock for `virtual_clock`'s value.
    /// **Must be called on the thread that will own the resulting
    /// instant** — see the per-thread invariant in the type doc.
    pub fn now(virtual_clock: bool) -> Self;

    /// `self + Duration` in the same variant. Used by `SearchClock::start_for`
    /// to construct deadlines from caps. Wall+Duration uses `Instant::add`;
    /// Cpu+Duration adds the duration's nanoseconds (saturating).
    pub fn add(self, dur: Duration) -> Self;

    /// `self - other`, returning a `Duration`. Cross-variant ⇒
    /// `unreachable!("SearchInstant::duration_since: cross-variant Wall vs Cpu")`.
    pub fn duration_since(self, other: SearchInstant) -> Duration;

    /// `self >= deadline`. Cross-variant ⇒
    /// `unreachable!("SearchInstant::is_at_or_past: cross-variant Wall vs Cpu")`.
    /// Boundary semantic: `>=` (matches M3.E's existing `Instant >= deadline`).
    pub fn is_at_or_past(self, deadline: SearchInstant) -> bool;
}

/// Read `CLOCK_THREAD_CPUTIME_ID` for the calling thread via libc.
/// Returns nanoseconds. `#[cfg(unix)]`. Panics with the libc return code
/// on error: `unreachable!("clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed: rc={rc}")`.
/// (The plan deliberately avoids reading `errno` to dodge the
/// `__errno_location` (Linux) vs `__error` (macOS) cross-platform shim;
/// the libc rc is -1 on error and that's enough signal for the panic
/// message — the panic is structurally unreachable for valid clk_id +
/// stack-allocated timespec.) The clk_id is documented infallible on
/// Linux and macOS for valid usage; the only failure modes are EINVAL
/// (bad clk_id — caught at compile) or EFAULT (bad pointer — impossible
/// with stack-allocated timespec).
#[cfg(unix)]
fn read_thread_cpu_ns() -> u64;
```

### 4.2 `SearchClock` (new, `src/search.rs`, worker-local)

```rust
/// Time-keeping state owned by the worker thread executing `Search::go`.
/// Constructed at entry; carries `start` / `deadline` / `soft_deadline` in
/// the variant chosen by `ctx.virtual_clock`. All clock reads happen on
/// the worker thread, satisfying the per-thread invariant of
/// `SearchInstant::Cpu`.
///
/// The orchestrator (`Engine::handle_go`) does NOT construct this — it
/// only computes `caps: TimeCaps` (durations) and `virtual_clock: bool`,
/// passes them through `SearchContext`, and lets the worker construct
/// `SearchClock::start_for(...)` at the top of `Search::go`.
#[derive(Debug, Clone, Copy)]
pub struct SearchClock {
    pub start: SearchInstant,
    pub deadline: Option<SearchInstant>,
    pub soft_deadline: Option<SearchInstant>,
}

impl SearchClock {
    /// Construct from caps and time-source choice. Reads the calling
    /// thread's clock once (via `SearchInstant::now(virtual_clock)`) and
    /// derives all three fields from that single read — same-variant by
    /// construction.
    ///
    /// `Duration::MAX` caps yield `None` deadlines (mirroring M3.E:
    /// "no cap"). `caps.hard != Duration::MAX` ⇒ `deadline = Some(start.add(caps.hard))`.
    /// Same for `soft`.
    pub fn start_for(virtual_clock: bool, caps: TimeCaps) -> Self;

    /// Cancellation-cadence check. Reads the worker's clock fresh.
    /// `nodes_searched`-cap path stays here for unified call site at
    /// negamax / qsearch.
    #[inline]
    pub fn should_abort(
        &self,
        stop: &AtomicBool,
        nodes_limit: Option<u64>,
        nodes_searched: u64,
    ) -> bool;

    /// ID-loop tail soft-deadline check. Caller passes the `now`
    /// already read for elapsed-ms emission so the two share one syscall.
    #[inline]
    pub fn is_soft_reached_at(&self, now: SearchInstant) -> bool;

    /// `now - self.start`. Caller passes `now` (same source as
    /// `is_soft_reached_at` to share the syscall).
    #[inline]
    pub fn elapsed_at(&self, now: SearchInstant) -> Duration;
}
```

**ID-loop tail invariant (load-bearing).** The current code at `src/search.rs:359` deliberately calls `Instant::now()` *once* and reuses the value for both elapsed-ms emission and the soft-cap check. Under ELOH.C this becomes:
```rust
let now = SearchInstant::now(ctx.virtual_clock);
let elapsed_ms = clock.elapsed_at(now).as_millis();
// ... info_sink emit ...
if depth >= max_depth { break; }
if ctx.stop.load(Ordering::Relaxed) { break; }
if clock.is_soft_reached_at(now) { break; }  // reuse `now`, no second clock read
```
Pinned in the §6.3 test `id_loop_tail_reads_clock_once_per_iteration`.

### 4.3 `SearchContext` revised shape (`src/search.rs`)

```rust
/// Per-`go` context. Cloned into the worker thread.
///
/// **Time-source field changes (ELOH.C):**
/// - Removed: `start: Instant`, `deadline: Option<Instant>`, `soft_deadline: Option<Instant>`.
///   These were orchestrator-thread-computed under M3.E. Under ELOH.C
///   `CLOCK_THREAD_CPUTIME_ID` is per-thread, so orchestrator-thread
///   reads are wrong values for the worker. `SearchClock` (worker-local,
///   constructed at `Search::go` entry) replaces these.
/// - Added: `caps: TimeCaps` (durations; pure-function output of
///   `compute_caps`; no clock reads), `virtual_clock: bool`.
#[derive(Clone)]
pub struct SearchContext {
    pub stop: Arc<AtomicBool>,
    /// `pub(crate)` because `TimeCaps` itself is `pub(crate)` — keeping the
    /// field's visibility no wider than its type. Other fields stay `pub`
    /// because their types are crate-public. The harness binary doesn't
    /// construct `SearchContext` (it talks UCI), so `pub(crate)` is
    /// sufficient.
    pub(crate) caps: TimeCaps,
    pub virtual_clock: bool,
    pub limits: SearchLimits,
    pub history: Vec<u64>,
}
```

The existing `SearchContext::should_abort` is **removed.** All call sites move to `clock.should_abort(&ctx.stop, ctx.limits.nodes, nodes_searched)` after the worker has constructed `clock`. Visibility:
- `SearchInstant`: `pub` (used by the alpha-beta search inside the same module; no extern consumer at present).
- `SearchClock`: `pub` (same).
- `TimeCaps`: stays `pub(crate)` (already so at `src/search.rs:870`).
- `SearchContext::caps`: `pub(crate)` (no wider than its type — closes the visibility-tension issue from plan-review pass 2).

**`should_abort` call sites that move to `SearchClock`:**
- Negamax/qsearch's `nodes_searched & 4095 == 0` cancellation cadence (the primary purpose of the refactor).
- The post-result wait loop at `src/search.rs:417` (executes inside `Search::go` on the worker thread, under `infinite`/`movetime`/`ponder`). Migrates to `clock.should_abort(...)`. Semantics preserved verbatim: `infinite`/`ponder` produce `caps = (MAX, MAX)` ⇒ `clock.deadline = None` ⇒ `should_abort` only fires on stop or nodes-cap; `movetime` produces a finite `caps.hard` ⇒ `clock.deadline = Some(start.add(caps.hard))` ⇒ wait loop exits when the deadline is reached, identical to M3.D/E.
- ID outer loop's between-iterations soft-cap check moves to `clock.is_soft_reached_at(now)` (separate method; not `should_abort`). The `now` is shared with the elapsed-ms read per the §4.2 single-read invariant.

### 4.4 `Engine::virtual_clock` field + UCI option plumbing (`src/engine.rs`)

```rust
pub struct Engine<W, S> {
    // existing fields...
    /// `VirtualClock` UCI option (ELOH.C). When `true`, `handle_go` sets
    /// `SearchContext::virtual_clock = true` so that the worker thread
    /// uses thread-CPU-time for search time-keeping.
    /// Always defaults to `false`. On non-unix platforms this field
    /// exists but cannot be set to `true` (the option is not advertised
    /// and `handle_setoption` rejects the value).
    virtual_clock: bool,
}
```

`handle_uci` emits one new line, after the existing `MoveOverhead` line, **only on `#[cfg(unix)]`**:
```rust
#[cfg(unix)]
self.write_line("option name VirtualClock type check default false");
```

`handle_setoption` accepts `VirtualClock` under `#[cfg(unix)]`. Value parsing is case-insensitive (`true`/`false`/`True`/`TRUE`/etc.), matching the `MoveOverhead` parsing pattern's tolerance. Malformed values rejected via `info_string_always`.
```rust
#[cfg(unix)]
"VirtualClock" => match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
    Some("true")  => self.virtual_clock = true,
    Some("false") => self.virtual_clock = false,
    other => self.info_string_always(&format!(
        "VirtualClock: rejected (expected `true` or `false`, got {:?})", other)),
}
```

`handle_go` no longer reads the clock:
```rust
// Was:
//   let now = Instant::now();
//   let caps = compute_caps(&limits, ...);
//   let deadline = (caps.hard != Duration::MAX).then(|| now + caps.hard);
//   let soft_deadline = (caps.soft != Duration::MAX).then(|| now + caps.soft);
//   ... SearchContext { ... start: now, deadline, soft_deadline, ... }
//
// Now:
let caps = compute_caps(&limits, self.position.side_to_move(), self.move_overhead);
let ctx = SearchContext {
    stop: Arc::clone(&self.stop),
    caps,
    virtual_clock: self.virtual_clock,
    limits,
    history: self.game_history.clone(),
};
```

The worker (in `Search::go`) does the clock-read after spawn:
```rust
fn go(&mut self, position: &Position, ctx: &SearchContext, info_sink: &dyn Fn(&str)) -> SearchResult {
    let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
    // ... use clock.should_abort / clock.is_soft_reached_at / clock.elapsed_at ...
}
```

Test-only access pattern (mirrors `move_overhead()`):
```rust
#[cfg(test)]
pub(crate) fn virtual_clock(&self) -> bool { self.virtual_clock }
```

### 4.5 Harness-side handshake parsing + setoption (`src/bin/elo-iterate.rs`)

Two new pieces in `mod driver`:

```rust
/// Capabilities advertised by an engine in its `uci` response.
/// Returned alongside `uciok` settling by the extended `wait_for_uciok`.
#[derive(Default)]
pub(crate) struct EngineCapabilities {
    pub supports_virtual_clock: bool,
}

/// Parse a single `option name <X> type ...` line and return `<X>`.
/// Returns `None` on malformed input. Per UCI spec, the option name
/// is the longest-prefix-up-to-but-not-including ` type`. Names are
/// case-insensitive in spec; the function returns the engine's
/// emitted casing (normalization to lowercase happens at the
/// capability-match site).
pub(crate) fn parse_option_advertisement(line: &str) -> Option<&str>;
```

Existing `wait_for_uciok` extended: after each `EngineLine::Other(s)`, run `parse_option_advertisement(&s)`; if `Some(name)`, lowercase-compare to known option names and update `EngineCapabilities`. Function signature changes from `wait_for_uciok(handle, timeout) -> Result<()>` to `wait_for_uciok(handle, timeout) -> Result<EngineCapabilities>`. All existing call sites updated; the harness's existing single call site uses the result.

`mod cli` adds one flag:
```rust
pub(crate) struct Args {
    // existing ELOH.A/B fields...
    pub virtual_clock: bool, // --virtual-clock; default false; takes no value.
}
```

CLI parsing adds the boolean-flag case (the existing parser is token-pair-based for value-bearing flags; `--virtual-clock` is the first no-value flag, so the parse loop's invariant gets a small extension). The `=`-bound form is not accepted (`--virtual-clock=true` rejects with the existing harness's no-equals convention preserved).

`mod controller::production_worker_fn` (and any analogous ELOH.A path): after `wait_for_uciok` returns `EngineCapabilities`:
```rust
if args.virtual_clock && caps.supports_virtual_clock {
    handle.send_line("setoption name VirtualClock value true")?;
    // No isready here — option ack is fire-and-forget per UCI; the
    // existing post-option-block `isready` already gates the handshake's
    // settling. Mirrors the existing UCI_LimitStrength + UCI_Elo flow.
}
```

`--virtual-clock`'s effect when only one engine supports the option: the harness sends the setoption only to advertising engines. The other engine plays under wallclock; this is documented in `--help` as expected behavior.

`--virtual-clock` semantics in `--help`:
> When set, the harness sends `setoption name VirtualClock value true` to engines advertising the `VirtualClock` option (clawfish does; Stockfish does not). Engines with the option will measure search time in thread CPU time instead of wallclock, making rating measurements more robust to thermal throttling and background-load noise. Default off — engines use wallclock TC. Note: CPU time is not fully thermal-invariant; combine with P-core pinning and external cooling for tighter results. See ADR-0021 / `docs/research/tooling-cpu-cycle-counters.md` for the reasoning.

## 5. Module boundaries

```
src/search.rs
    pub enum SearchInstant       (NEW)
    pub struct SearchClock       (NEW; worker-local)
    pub struct SearchContext     (field changes; should_abort removed; helper retained for limits.nodes)
    fn read_thread_cpu_ns()      (NEW; private; libc::clock_gettime; cfg(unix))
    impl SearchInstant { ... }   (NEW)
    impl SearchClock { ... }     (NEW)

src/engine.rs
    Engine::virtual_clock        (NEW field)
    handle_uci                   (one cfg(unix)-gated line added)
    handle_setoption             (cfg(unix)-gated VirtualClock arm)
    handle_go                    (clock read removed; constructs caps + virtual_clock)

src/bin/elo-iterate.rs
    mod cli                      (--virtual-clock added; first no-value flag)
    mod driver                   (EngineCapabilities, parse_option_advertisement,
                                  wait_for_uciok return-type extension)
    mod controller / mod match_loop  (post-uciok setoption send)

Cargo.toml
    [dependencies] libc = "0.2"  (NEW)

docs/decisions/0021-virtual-clock-uci-option.md  (NEW)
```

## 6. Test coverage strategy

### 6.1 `SearchInstant` unit tests (`mod search::tests`, ~35 LOC)

| Test | Asserts |
|---|---|
| `search_instant_now_wall_returns_wall` | `SearchInstant::now(false)` matches `Wall(_)`. |
| `search_instant_now_cpu_returns_cpu` | `SearchInstant::now(true)` matches `Cpu(_)`. (`#[cfg(unix)]`-gated test.) |
| `search_instant_wall_add_advances_by_duration` | Compare via `duration_since`: `Wall(t).add(Duration::from_millis(10)).duration_since(Wall(t)) == Duration::from_millis(10)`. |
| `search_instant_cpu_add_advances_by_duration` | `Cpu(ns).add(Duration::from_millis(10))` ⇒ `Cpu(ns + 10_000_000)`. (`#[cfg(unix)]`-gated.) |
| `search_instant_cpu_add_saturates` | `Cpu(u64::MAX).add(Duration::from_millis(1))` ⇒ `Cpu(u64::MAX)` (no overflow panic; saturating). |
| `search_instant_duration_since_wall` | `Wall(t1).duration_since(Wall(t0))` ⇒ `t1 - t0`. |
| `search_instant_duration_since_cpu` | `Cpu(t1_ns).duration_since(Cpu(t0_ns))` ⇒ `Duration::from_nanos(t1_ns - t0_ns)`. |
| `search_instant_is_at_or_past_wall_strict` | `Wall(now+1ms).is_at_or_past(Wall(now)) == true`; `Wall(now-1ms).is_at_or_past(Wall(now)) == false`. |
| `search_instant_is_at_or_past_cpu_strict` | Mirror for `Cpu`. |
| `search_instant_is_at_or_past_equal_fires` | `t.is_at_or_past(t) == true` (boundary; pins `>=`-not-`>` semantic, matching M3.E `Instant >= deadline`). |
| **`#[should_panic(expected = "cross-variant Wall vs Cpu")] search_instant_cross_variant_duration_unreachable`** | `Wall(_).duration_since(Cpu(_))` panics with the named message substring. Pins the contract; `expected = ...` ensures a future refactor that swaps the unreachable for a different unreachable (or silent default) is caught. |
| **`#[should_panic(expected = "cross-variant Wall vs Cpu")] search_instant_cross_variant_compare_unreachable`** | `Wall(_).is_at_or_past(Cpu(_))` panics with the named message substring. |
| `search_instant_cpu_now_non_decreasing_within_thread` | `let a = SearchInstant::now(true); let b = SearchInstant::now(true);` ⇒ `b.is_at_or_past(a) == true`. **Non-decreasing**, not strict-greater (CLOCK_THREAD_CPUTIME_ID can have coarse granularity; equal-monotone is valid and `is_at_or_past` defines `>=`). (`#[cfg(unix)]`-gated.) |

### 6.2 `SearchClock` unit tests (`mod search::tests`, ~35 LOC)

| Test | Asserts |
|---|---|
| `search_clock_start_for_wall_no_caps_yields_none_deadlines` | `start_for(false, TimeCaps { soft: Duration::MAX, hard: Duration::MAX })` ⇒ `clock.deadline.is_none() && clock.soft_deadline.is_none()`. |
| `search_clock_start_for_wall_with_caps_yields_wall_deadlines` | `start_for(false, TimeCaps { soft: 100ms, hard: 200ms })` ⇒ both deadlines `Some(Wall(_))`, `start` is `Wall(_)`. |
| `search_clock_start_for_cpu_with_caps_yields_cpu_deadlines` | `start_for(true, TimeCaps { soft: 100ms, hard: 200ms })` ⇒ both deadlines `Some(Cpu(_))`, `start` is `Cpu(_)`. (`#[cfg(unix)]`-gated.) |
| `search_clock_start_same_variant_invariant` | `debug_assert!` on `start_for` runs cleanly (no panic). The assertion verifies all three fields share variant. |
| `search_clock_should_abort_no_deadline_no_nodes_only_stop` | `clock` with no deadlines + `stop=false` + `nodes_limit=None` ⇒ `should_abort(stop, None, 1_000_000) == false`. With `stop=true` ⇒ `true`. |
| `search_clock_should_abort_node_cap_works_independent_of_clock` | `nodes_limit=Some(100)` + `nodes_searched=100` ⇒ `true` regardless of clock variant. |
| `search_clock_should_abort_wall_deadline_fires_after_sleep` | `start_for(false, TimeCaps { hard: 1ms, soft: ... })`, sleep 5ms, `should_abort(...) == true`. |
| **`search_clock_should_abort_cpu_deadline_does_not_fire_under_pure_sleep`** | `start_for(true, TimeCaps { hard: 1_000ms, soft: ... })` (1 second of CPU time deadline — 200× margin vs. wake-up jitter); `thread::sleep(200ms)` (no CPU work); `should_abort(...) == false`. **Pins the load-invariance contract**: under wallclock the deadline would fire, under CPU time it doesn't because the thread spent the wallclock sleeping, not on CPU. Critical correctness test for the option's value proposition. (`#[cfg(unix)]`-gated.) |
| `search_clock_should_abort_cpu_deadline_fires_under_cpu_burn` | `start_for(true, TimeCaps { hard: 50ms, soft: ... })`; tight CPU loop with `std::hint::black_box`-fenced multiplication consuming ~200ms of CPU; `should_abort(...) == true`. (`#[cfg(unix)]`-gated.) |
| `search_clock_is_soft_reached_at_uses_passed_now` | `start_for(false, TimeCaps { soft: 10ms, hard: ... })`; pass `now = Wall(start + 20ms)`; `is_soft_reached_at(now) == true`. Pins that the method does NOT internally read the clock — uses the parameter. |

### 6.3 Search time-source integration tests (`mod search::tests`, ~25 LOC)

The cancellation hot path and the ID-loop tail must respect the chosen time source.

| Test | Asserts |
|---|---|
| `search_clock_start_for_reads_calling_thread_cpu` | Single-thread test using a `#[cfg(test)] pub fn start_cpu_ns(&self) -> Option<u64>` accessor on `SearchClock` (returns `Some(ns)` when `start` is `Cpu(ns)`, `None` for `Wall(_)`). Spawn one thread that consumes ~500 ms of CPU via a `std::hint::black_box`-fenced multiplication loop, then constructs `SearchClock::start_for(true, TimeCaps { soft: 0, hard: 0 })` from inside that thread, then asserts `start_cpu_ns() >= 500_000_000`. Pins that `start_for` reads the *calling* thread's clock — not an inherited or main-thread value. Simpler shape than the v1 plan's "spawn A vs B" comparison: no inter-thread comparison, no false-positive zero-zero match, no false-negative spawn-cost spike. (`#[cfg(unix)]`-gated.) |
| `id_loop_tail_reads_clock_once_per_iteration` | Counter-instrumented test using a `Search` impl that wraps `SearchInstant::now` calls. Per ID-loop iteration: at most ONE call to `SearchInstant::now(virtual_clock)` for the elapsed-ms-and-soft-deadline pair. (Pins the §4.2 single-read invariant.) |
| `mate_distance_pruning_independent_of_time_source` | A constructed mate-in-2 position. Run `Search::go` once with `virtual_clock=false`, once with `virtual_clock=true`. Both return the same mate score and PV (modulo timing — neither hits the deadline at this depth). MDP is algorithmically agnostic; this confirms in code. |

### 6.4 `VirtualClock` UCI option tests (engine-side, `mod engine::tests`, ~30 LOC)

Mirrors M3.E's `MoveOverhead` test pattern (E33–E35).

| Test | Asserts |
|---|---|
| **`#[cfg(unix)] option_advertised_in_uci_response_on_unix`** | After `uci`, output contains `option name VirtualClock type check default false`. |
| **`#[cfg(not(unix))] option_not_advertised_on_non_unix`** | After `uci`, output does NOT contain a VirtualClock line. (Compile-and-pass on macOS/Linux; the test exists for completeness.) |
| `setoption_virtual_clock_true_sets_flag` (cfg(unix)) | Send `setoption name VirtualClock value true`; via `engine.virtual_clock()` test accessor, flag is `true`. |
| `setoption_virtual_clock_false_resets_flag` (cfg(unix)) | Set true, then false; flag ends at `false`. |
| `setoption_virtual_clock_default_is_false` | Fresh engine: flag is `false`. (Cross-platform.) |
| `setoption_virtual_clock_invalid_value_rejected` (cfg(unix)) | `value bogus` ⇒ flag unchanged + `info string` warning emitted. Mirrors `MoveOverhead`'s rejection path. |
| `setoption_virtual_clock_case_insensitive_value` (cfg(unix)) | `value TRUE` and `value True` and `value tRuE` all set the flag to `true`. (Spec says option names are case-insensitive; values are by convention; pin clawfish's choice for VirtualClock.) |

### 6.5 Harness handshake-parse / setoption tests (`mod driver::tests`, ~40 LOC)

| Test | Asserts |
|---|---|
| `parse_option_advertisement_well_formed` | `"option name VirtualClock type check default false"` ⇒ `Some("VirtualClock")`. |
| `parse_option_advertisement_with_extras` | `"option name MoveOverhead type spin default 50 min 0 max 5000"` ⇒ `Some("MoveOverhead")`. |
| `parse_option_advertisement_malformed_returns_none` | `"option foo bar"` ⇒ `None`. |
| `parse_option_advertisement_multiword_name` | `"option name UCI_Chess960 type check default false"` ⇒ `Some("UCI_Chess960")` (single-token name, but pins the parser handles names with underscore). |
| `wait_for_uciok_records_virtual_clock_capability` | Mock pipe emits `option name VirtualClock type check default false\nuciok\n`; `EngineCapabilities { supports_virtual_clock: true, .. }`. |
| `wait_for_uciok_records_no_virtual_clock_when_absent` | Mock emits unrelated options + `uciok`; `supports_virtual_clock: false`. |
| **`wait_for_uciok_handles_interleaved_info_string`** | Mock emits `info string warming up\noption name VirtualClock type check default false\ninfo string ready\nuciok\n`; capability is detected. Pins that interleaved `info string` (real engines emit them) doesn't break parsing. |
| **`wait_for_uciok_case_insensitive_option_name_match`** | Mock emits `option name virtualclock type check default false`; `supports_virtual_clock: true`. UCI spec says option names are case-insensitive; clawfish emits canonical case but a future engine version (or a different engine) might emit otherwise. |
| `wait_for_uciok_duplicate_advertisement_idempotent` | Mock emits `option name VirtualClock ...` twice + `uciok`; capability is `true` (no panic, no flap). |
| `production_worker_sends_setoption_when_advertised_and_flag_on` | `cli.virtual_clock=true` + caps says supported ⇒ `"setoption name VirtualClock value true"` appears in the recorded send sequence between `uciok` and any `ucinewgame`. |
| `production_worker_skips_setoption_when_unadvertised` | `cli.virtual_clock=true` + caps says NOT supported ⇒ no `setoption name VirtualClock` line in the send sequence. |
| `production_worker_skips_setoption_when_flag_off` | `cli.virtual_clock=false` + caps says supported ⇒ no setoption (default behavior unchanged). |

### 6.6 CLI parse (`mod cli::tests`, ~15 LOC)

| Test | Asserts |
|---|---|
| `parse_args_virtual_clock_default_false` | Omitted ⇒ `false`. |
| `parse_args_virtual_clock_flag_sets_true` | `--virtual-clock` on argv ⇒ `true`. |
| `parse_args_virtual_clock_flag_takes_no_value` | `--virtual-clock --max-games 4 ...` parses correctly (boolean flag; doesn't consume the next token as a value). |
| `parse_args_virtual_clock_equals_form_rejected` | `--virtual-clock=true` ⇒ `Err(InvalidValue)`. Pins the no-equals convention used by the rest of the parser. |

### 6.7 `#[ignore]`-gated end-to-end self-play (~25 LOC)

Extends ELOH.A/B's smoke tests:

| Test | Asserts |
|---|---|
| `end_to_end_self_play_virtual_clock_runs` | `--engine clawfish --opponent clawfish --virtual-clock --concurrency 1 --max-games 2 --tc 1+0.05 --target-sigma 0 --initial-elo 2000 --k0 0`. Both engines receive `setoption name VirtualClock value true`. 2 PGNs written, summary has 2 entries + final `converged:` line, exit 0. |
| `end_to_end_vs_stockfish_virtual_clock_falls_back_silently` | Stockfish doesn't advertise the option. `--virtual-clock` set; harness sends to clawfish only; the run completes normally; no error string in summary or stderr. (`#[ignore]` because it requires Stockfish on PATH.) |

## 7. Order of operations

1. **Slice A — engine-side `SearchInstant` + `SearchClock` + SearchContext refactor + UCI plumbing.** ~110 prod LOC + ~75 test LOC. New `SearchInstant` enum, new `SearchClock` struct, `read_thread_cpu_ns` libc shim, `SearchContext` field changes (drops `start`/`deadline`/`soft_deadline`, adds `caps`/`virtual_clock`), all production call sites updated (`should_abort` moves to SearchClock; `handle_go` simplified; `Search::go` constructs SearchClock at entry; ID-loop tail uses single SearchInstant::now per iteration), `Engine::virtual_clock` field + UCI plumbing, ~16 test-site SearchContext-construction migrations across `src/search.rs::tests` and `src/engine.rs::tests`. **Opus override flag** — load-bearing surface (SearchContext fields), novel domain type with cross-variant unreachable contracts, per-thread invariant on `SearchInstant::Cpu` that the coder must understand and apply correctly. Spawn-prompt callout: "the per-thread invariant is the single most important correctness obligation; the worker constructs SearchClock, the orchestrator does NOT."
2. **Slice B — harness-side handshake + setoption.** ~55 prod LOC + ~55 test LOC. `EngineCapabilities`, `parse_option_advertisement`, `wait_for_uciok` extension, `--virtual-clock` flag, post-uciok setoption send, §6.5 + §6.6 tests. Sonnet — prescriptive transcription, narrow surface. **Independent of Slice A in source** (no shared file modulo Cargo.toml's `libc` dep which is a one-line addition); only coupled at integration-test scope.
3. **Slice C — `#[ignore]`-gated integration tests + ADR-0021.** ~25 LOC tests + ~110 LOC ADR. Sonnet. Sequential after A+B (the e2e tests need both ends working).
4. **Pre-review mechanical checks** (workflow.md step 9).
5. **Final review loop** (workflow.md step 10).
6. **Benchmark.**
   - Pre-impl invocation (current `tooling/elo-harness` HEAD = ELOH.B's `dba6c10`): standard `bench` for the regression baseline.
   - Post-impl invocation:
     - Default path (`VirtualClock=false`): node count + NPS. Must match pre-impl bench bit-for-bit on node count (search behavior is unchanged; the only diff is the SearchContext-vs-SearchClock plumbing, which has no effect on the node visit order).
     - VC=true path: drive via `setoption name VirtualClock value true` followed by `bench`. **Expected node-count output is byte-identical** to VC=false (bench is fixed-depth-12 startpos; `caps = (MAX, MAX)` ⇒ `clock.deadline = None` ⇒ time source is unused; only `should_abort`'s clock-poll path costs change, and only via the `clock_gettime` syscall vs. `Instant::now`). NPS may differ slightly (syscall-cost delta — `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` is a vDSO call on Linux, a thread-info read on macOS; both fast but distinguishable from `mach_absolute_time`). The point of the VC=true bench is to characterize that overhead delta, NOT to compare nodes.
     - **Failure mode**: if VC=true bench's node count differs from VC=false's, that's a correctness bug. The time source must not affect search behavior at fixed depth.
   - Append to `bench/eloh-c.md` (a new file; the ELOH milestone is a tooling-track milestone, not a strength-track milestone, so its bench numbers go in their own file rather than mixing with `bench/m4.md`).
7. **Commit + push** — atomic doc-delta per §11.
8. **Manual back-test (Part 1, post-commit).**
   - Two clawfish-vs-clawfish SPRT runs under simulated CPU load (`stress-ng --cpu 4 &` in the background, or `yes > /dev/null &` ×4, or any equivalent producer):
     - Run 1: baseline. Both engines wallclock TC. `cargo run --release --bin elo-iterate -- --engine target/release/clawfish --opponent target/release/clawfish --tc 10+0.1 --max-games 200 --target-sigma 0 --initial-elo 2114 --concurrency 4 --k0 0`.
     - Run 2: VirtualClock=true on both. Same command + `--virtual-clock`. Should reduce per-game variance under the load.
   - Pass: σ of the per-game W/L/D distribution (or, more directly, σ of an estimated Elo from the fixed-anchor self-play match — both binaries are identical so true Elo is 0; deviation is pure noise). VirtualClock run's σ at least 30% lower than baseline.
   - Reasoning: a fixed-anchor self-play match between two identical binaries has expectation 50% W (with draws) and any deviation from that is noise. Wallclock noise should dominate when the system is loaded; CPU-time TC should remove most of it.
   - Quantitative target: `σ_VC / σ_wall ≤ 0.7` (i.e. 30% reduction). Wider tolerance acknowledged: 30% is the target for a "load-equivalent" run; if σ ratio is in `[0.7, 0.9]`, document as partial improvement and decide whether to widen the tolerance (if e.g. wallclock noise is small relative to other variance sources at this TC) or to investigate.
   - Diagnostic ladder on Part 1 failure:
     - If σ_VC ≈ σ_wall: VirtualClock isn't kicking in. Verify via the harness log for the setoption send + per-game `info string` echo from clawfish.
     - If σ_VC > σ_wall: hard error in the time-source path. Re-run with the `search_clock_should_abort_cpu_deadline_does_not_fire_under_pure_sleep` test passing locally; if test passes, run a single-game probe with `--virtual-clock` and inspect per-move CPU times in the PGN.
     - If σ ratio is in `[0.7, 0.9]`: partial improvement. Surface the result in chat with the σ numbers; don't auto-fail the gate without user judgment.
   - Archive verdict to `docs/research/tooling-virtual-clock-validation.md`.

## 8. Dependencies

- **ELOH.A** for driver/match_loop/handshake stack. Already landed.
- **ELOH.B** for controller (the per-pair setoption broadcast hooks into `production_worker_fn`'s post-uciok block). Already landed.
- **`libc = "0.2"`** added to `Cargo.toml`. Single new dependency; widely used in the Rust ecosystem; covered by `deny.toml`'s license allowlist (MIT/Apache-2.0).
- **No M4 dependency.** ELOH.C is independent of M4 phase progression.
- Crate API surface: `SearchInstant`, `SearchClock` become public types in `src/search.rs`. `SearchContext` field type changes are technically a public API change but the only consumer is inside the crate (the harness binary doesn't construct `SearchContext`).

## 9. Parallelization map

After this plan converges through review:
- **Slice A and Slice B in parallel** via two coder agents. They share **no source files** (Slice A: `src/search.rs` + `src/engine.rs` + ~16 test sites in those files; Slice B: `src/bin/elo-iterate.rs`). Both touch `Cargo.toml` (Slice A adds `libc`); ordered atomically — Slice A merges first; Slice B's `Cargo.toml` change is empty if Slice A already added the dep. (Realistically: Slice A adds the dep; Slice B uses no new dep; no conflict.) Slice A is **flagged for Opus override** (`SearchContext` refactor, per-thread invariant on `SearchInstant::Cpu`, cross-variant `unreachable!` contract, ~16-test-site fanout). Slice B is Sonnet (prescriptive plan body; narrow harness surface).
- **Slice C** (integration tests + ADR) sequential after A and B. Sonnet.

## 10. Risk register

- **POSIX `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` reliability on the dev machine.** Empirically verified clean on M4 / macOS 26.4.1 (probe: `cpu/wall = 0.9993` ratio). M1's known ~40× underreporting bug NOT present. If a future contributor's machine does exhibit the bug, the §6.1 `search_instant_cpu_now_non_decreasing_within_thread` test still passes (the bug is rate-of-progress, not non-monotonicity); the back-test gate Part 1 catches it (σ ratio would not improve, would stay at `~ 1.0`). Document the probe procedure in ADR-0021's "Operator's checklist" so future contributors run it before relying on the metric.

- **`compute_caps` outputs wallclock-ms but VirtualClock interprets them as CPU-ms.** `compute_caps` is a pure function (no clock reads) that divides UCI-protocol `wtime`/`btime`/`winc`/`binc` (wallclock fields per UCI spec) into a soft+hard cap. Under VC=true, the worker treats those output ms as *CPU* ms — i.e. the budget the engine spends on CPU is calibrated as if it were wallclock. Acknowledged drift bound: M4 probe shows cpu/wall = 0.9993 for a CPU-bound thread, so the unit-mismatch causes ≤0.1% drift. Documented in ADR-0021. The alternative — recomputing caps from a CPU-time-aware `compute_caps` — would require harness-side coordination (the harness sends UCI's wallclock fields, the engine couldn't infer how to scale them) and is rejected as over-engineering for a 0.1% effect. Single follow-up flag if empirical results show >5% drift in some scenario.

- **Per-thread `CLOCK_THREAD_CPUTIME_ID` semantics — orchestrator-vs-worker thread mismatch.** This was the v1 plan's central correctness bug (caught by plan-review). v2 fixes it structurally: `SearchClock` is constructed *inside* `Search::go` on the worker thread (the same thread that subsequently calls `should_abort` and reads `SearchInstant::now(true)` per iteration). Orchestrator-thread `Engine::handle_go` does NOT read any clock — it computes `caps: TimeCaps` (durations) and threads them through `SearchContext`. Pinned by §6.3 test `search_clock_start_for_reads_calling_thread_cpu`.

- **`SearchClock` cross-variant invariant.** All three fields share variant by construction (single `SearchInstant::now()` read in `start_for`). Invariant is enforced by the type system + `unreachable!` on cross-variant `SearchInstant::is_at_or_past` / `duration_since`, plus a `debug_assert!` in `start_for` that all three fields match. If a future change adds a SearchClock-mutating method that re-reads the clock (e.g. clock-skew correction), the variant must be preserved.

- **Stockfish doesn't support VirtualClock.** Expected; the harness's silent-fallback path is tested in §6.5. Documented in `--help`. `--virtual-clock` matches against Stockfish are wallclock on Stockfish's side; the clawfish side gets the benefit. Asymmetric noise; partial mitigation only. ELOH.B's existing concurrency-control + P-core-pin recipe still required.

- **Mate-distance pruning (M3.E).** Algorithmically agnostic to the time source. Confirmed by §6.3's `mate_distance_pruning_independent_of_time_source` test — same mate score and PV under both modes for a constructed mate-in-2.

- **`MoveOverhead` semantic mismatch under VirtualClock.** Documented in the option's UCI description and ADR-0021. Default 50ms is harmless; if empirically problematic, follow-up to make `MoveOverhead` mode-aware (separate flag or different default) — out of ELOH.C scope.

- **Test-site fanout in Slice A.** ~16 SearchContext-construction sites need the field migration. All mechanical; coder Opus-override on Slice A handles the volume; spawn prompt explicitly lists the field rename.

- **`unreachable!()` introduces panic paths in production code.** Acceptable per workflow.md "Code quality" — the panic message documents the invariant; a future change that breaks it fails loudly. Cross-variant cases are structurally impossible at the call sites in production (`SearchClock::should_abort` reads `self.deadline` and the worker's `SearchInstant::now(...)` — both same-variant by construction). The `#[should_panic(expected = ...)]` tests pin the messages.

- **Back-test gate's stress-loaded baseline.** σ ratio depends on system load level. Lightly-loaded baseline run might show σ_wall ≈ σ_VC because wallclock noise wasn't dominant at that moment. Mitigation: explicitly document the load-generation command in the gate-run record; re-run baseline if load level was low. Quantitative tolerance band `[0.7, 0.9]` already accommodates partial-improvement outcomes.

- **`libc` dependency new to the crate.** `libc = "0.2"` is one of the most-vetted crates on crates.io (~6 billion downloads). Single small additional dep; covered by existing `deny.toml` policy (MIT/Apache-2.0 license; crates.io source). No advisories. `cargo deny check` will pass.

## 11. Doc-delta — atomic with landing

- `docs/decisions/0021-virtual-clock-uci-option.md` — new ADR. Sections: Status; Context; Decision (1: time source = `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`; 2: time-source ownership in worker thread, not orchestrator; 3: rejected alternatives = cycle/instructions counters + `--go-nodes`; 4: `MoveOverhead` reinterpretation; 5: `compute_caps` wall-ms-as-CPU-ms drift; 6: cfg(unix) gating with rejection arm on Windows); Consequences; Operator's checklist (running the M4-style empirical probe before relying on the metric on a new machine); See also.
- `docs/tooling/elo-iteration-harness.md` — ELOH.C row → done; scope detail → "Done" prose with landing size. Two correlated edits to keep totals consistent:
   - **Strike item 6 of §"In scope (harness-side, ~70 LOC)"** (the `--go-nodes N` flag with its 30-LOC sub-budget) and add a one-paragraph rationale block citing the user's 2026-04-30 decision and pointing at ADR-0021. The section heading total (`~70 LOC`) gets revised to `~40 LOC` (subtract item 6's 30-LOC sub-budget).
   - **Update the size table at line 54** (the `~70 harness + ~80 engine + ~80 tests` row for ELOH.C) to reflect actual landing — pull from this plan's §0 sizing total (`~190 prod LOC + ~140 test LOC = ~330 LOC` — same framing as §0; ADR + bench + research files are tracked separately as docs, not folded into the prod+test total).
   - Cross-link Part 1+2 results.
- `docs/tooling-backlog.md` — "Custom in-process Elo-iteration harness" entry already in "Done" (ELOH.B). "Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension" entry → "Done" block with caveats: CPU time used for VirtualClock; cycles/instructions privileged on Apple Silicon; inaccessible in any common macOS-host VM; `go nodes` dropped per implementation-coupling concern.
- `docs/architecture.md` — settled-commitments row + ~3 lines in Search-v1 subsection ("Time-source: wallclock by default; CPU time when `VirtualClock=true` per ADR-0021; ownership in worker thread, not orchestrator").
- `docs/roadmap.md` — ELOH milestone closed; per the milestone's exit criteria all three sub-phases pass.
- `docs/research/tooling-virtual-clock-validation.md` — created post-Part-1 manual back-test (follow-up commit).
- `CLAUDE.md` Status table — ELOH milestone closed row added (or, if a separate ELOH status table exists, that one updated).
- `bench/eloh-c.md` — pre/post bench numbers under `VirtualClock=false` / `VirtualClock=true`. New file; tooling-track bench, separate from M4's strength-track bench file.

## 12. Verification checklist

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --release` (the full suite — ELOH.A's 51 + ELOH.B's 88 + ELOH.C's ~85 new tests)
- `cargo llvm-cov --summary-only --lib --release`
- `git add -N` on any new `.rs` files; `cargo mutants --in-diff` on the unit's diff
- `cargo deny check` (because `Cargo.toml` changed — `libc` added)
- The `search_instant_cpu_now_non_decreasing_within_thread` test passes (gates the dev-machine clock-source health on the CI's own machine, in addition to the documented M4 probe).

## Appendix — branches and worktrees

ELOH.C lands on the existing `tooling/elo-harness` branch in `/Users/alex/clawfish-elo-harness`, on top of ELOH.B's `dba6c10`. The spec's earlier preference for a separate `tooling/eloh-c-hardware-invariant-tc` branch was contingent on ELOH.B having merged to `main` before ELOH.C planning began; that hasn't happened (ELOH.B is still on the harness branch awaiting ELOH-milestone-close merge). Landing all three sub-phases on the same branch and merging the ELOH milestone as one set of commits is consistent with the user's directive ("Work in the ~/clawfish-elo-harness worktree").

## Appendix — review history

- **Plan v1 (2026-04-30)** — written; spawned blind plan-reviewer (Opus). Reviewer returned 4 must-fix + 8 should-fix + 4 nits + verdict "revisions required." Most consequential must-fix: per-thread `CLOCK_THREAD_CPUTIME_ID` semantics meant orchestrator-thread-pre-computed deadlines are wrong values for the worker thread to compare against. v2 fixes this structurally: `SearchClock` is worker-local, `SearchContext` carries durations not absolute deadlines.
- **Plan v2 (2026-04-30)** — addressed all v1 must-fix + should-fix + nits. Reviewer pass 2 returned 0 must-fix + 6 should-fix + 3 nits + verdict "revisions required." The 6 should-fix items: (a) `TimeCaps` visibility tension with public `SearchContext.caps`; (b) wait-loop callsite explicit treatment for the `infinite`/`movetime`/`ponder` block at search.rs:417; (c) `bench` path's single-thread degenerate case for the per-thread invariant; (d) `search_go_constructs_clock_in_worker_thread_under_vc` test redesign (simpler one-thread accessor-based shape); (e) `read_thread_cpu_ns` errno cross-platform shim avoidance (use libc rc instead of errno); (f) bench note clarifying byte-identical node count under VC=true vs VC=false.
- **Plan v3 (this revision)** — addresses all v2 should-fix + nits. SearchContext.caps is `pub(crate)` (no wider than `TimeCaps`'s visibility); wait-loop migration explicit in §4.3; bench-path single-thread case explicit in §1; thread test redesigned to use a `start_cpu_ns()` accessor; `read_thread_cpu_ns` panic uses libc rc not errno; bench section in §7 is explicit about node-count byte-identicality + the failure mode if it differs; §3 LOC est corrected to +95/-25; spec-doc size-table update added to §11. Re-review pending.
