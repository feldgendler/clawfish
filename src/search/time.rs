use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Color, SearchLimits};

// ---------------------------------------------------------------------------
// ELOH.C — VirtualClock UCI option: SearchInstant + SearchClock + libc shim.
// ---------------------------------------------------------------------------

/// Search-time instant. Either wallclock-based (`std::time::Instant`) or
/// thread-CPU-time-based (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`-derived
/// nanoseconds). Selected once per `Search::go` invocation by the engine's
/// `VirtualClock` UCI option (ELOH.C / ADR-0019).
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
    /// Wallclock instant, the M3.E default. `Instant::now()` semantics.
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
    pub fn now(virtual_clock: bool) -> Self {
        if !virtual_clock {
            return SearchInstant::Wall(Instant::now());
        }
        #[cfg(unix)]
        {
            SearchInstant::Cpu(read_thread_cpu_ns())
        }
        #[cfg(not(unix))]
        {
            unreachable!("VirtualClock not supported on non-unix platforms")
        }
    }

    /// `self + Duration` in the same variant. Used by `SearchClock::start_for`
    /// to construct deadlines from caps. `Wall + Duration` uses `Instant::add`;
    /// `Cpu + Duration` adds the duration's nanoseconds (saturating to avoid
    /// overflow at `u64::MAX` near the integer ceiling).
    ///
    /// Not implementing `std::ops::Add<Duration>` because that would force a
    /// uniform output type and obscure the per-variant contract; the
    /// `cross-variant Wall vs Cpu` `unreachable!()` story relies on
    /// always-explicit construction via `SearchInstant::add`.
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, dur: Duration) -> Self {
        match self {
            SearchInstant::Wall(t) => SearchInstant::Wall(t + dur),
            SearchInstant::Cpu(ns) => SearchInstant::Cpu(ns.saturating_add(dur.as_nanos() as u64)),
        }
    }

    /// `self - other`, returning a `Duration`. Cross-variant ⇒
    /// `unreachable!("SearchInstant::duration_since: cross-variant Wall vs Cpu")`.
    pub fn duration_since(self, other: SearchInstant) -> Duration {
        match (self, other) {
            (SearchInstant::Wall(a), SearchInstant::Wall(b)) => a.duration_since(b),
            (SearchInstant::Cpu(a), SearchInstant::Cpu(b)) => {
                Duration::from_nanos(a.saturating_sub(b))
            }
            _ => unreachable!("SearchInstant::duration_since: cross-variant Wall vs Cpu"),
        }
    }

    /// `self >= deadline`. Cross-variant ⇒
    /// `unreachable!("SearchInstant::is_at_or_past: cross-variant Wall vs Cpu")`.
    /// Boundary semantic: `>=` (matches M3.E's existing `Instant >= deadline`).
    pub fn is_at_or_past(self, deadline: SearchInstant) -> bool {
        match (self, deadline) {
            (SearchInstant::Wall(a), SearchInstant::Wall(b)) => a >= b,
            (SearchInstant::Cpu(a), SearchInstant::Cpu(b)) => a >= b,
            _ => unreachable!("SearchInstant::is_at_or_past: cross-variant Wall vs Cpu"),
        }
    }
}

/// Read `CLOCK_THREAD_CPUTIME_ID` for the calling thread via libc.
/// Returns nanoseconds. Panics with the libc return code on error
/// (`unreachable!` — the panic is structurally unreachable for valid
/// clk_id + stack-allocated timespec). The clk_id is documented
/// infallible on Linux and macOS for valid usage; the only failure
/// modes are EINVAL (bad clk_id — caught at compile) or EFAULT
/// (bad pointer — impossible with stack-allocated timespec).
#[cfg(unix)]
fn read_thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        unreachable!("clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed: rc={rc}");
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64
}

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
    /// Reference instant at the start of the search; same variant as
    /// `deadline`/`soft_deadline` by construction.
    pub start: SearchInstant,
    /// Hard cap (cancel mid-iteration when reached). `None` = no hard cap.
    pub deadline: Option<SearchInstant>,
    /// Soft cap (don't start a new ID iteration past this point). `None` =
    /// no soft cap.
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
    ///
    /// `pub(crate)` because `TimeCaps` is `pub(crate)` (visibility no wider
    /// than the parameter type). External crates have no need for this
    /// constructor — `Search::go` is the sole caller.
    pub(crate) fn start_for(virtual_clock: bool, caps: TimeCaps) -> Self {
        let start = SearchInstant::now(virtual_clock);
        let deadline = (caps.hard != Duration::MAX).then(|| start.add(caps.hard));
        let soft_deadline = (caps.soft != Duration::MAX).then(|| start.add(caps.soft));
        let clock = SearchClock {
            start,
            deadline,
            soft_deadline,
        };
        debug_assert!(
            clock.same_variant(),
            "SearchClock::start_for must produce same-variant SearchInstants",
        );
        clock
    }

    /// Cancellation-cadence check. Reads the worker's clock fresh.
    /// `nodes_searched`-cap path stays here for unified call site at
    /// negamax / qsearch.
    #[inline]
    pub fn should_abort(
        &self,
        stop: &AtomicBool,
        nodes_limit: Option<u64>,
        nodes_searched: u64,
    ) -> bool {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        if let Some(d) = self.deadline {
            // Variant matches `self.start`'s variant (established by
            // `start_for`'s single clock-read). Reading the time-source
            // choice from `start`'s variant preserves the per-thread
            // invariant — every clock read inside this struct uses the
            // same domain that constructed it.
            let now = SearchInstant::now(matches!(self.start, SearchInstant::Cpu(_)));
            if now.is_at_or_past(d) {
                return true;
            }
        }
        if let Some(cap) = nodes_limit
            && nodes_searched >= cap
        {
            return true;
        }
        false
    }

    /// ID-loop tail soft-deadline check. Caller passes the `now`
    /// already read for elapsed-ms emission so the two share one syscall.
    #[inline]
    pub fn is_soft_reached_at(&self, now: SearchInstant) -> bool {
        match self.soft_deadline {
            Some(d) => now.is_at_or_past(d),
            None => false,
        }
    }

    /// `now - self.start`. Caller passes `now` (same source as
    /// `is_soft_reached_at` to share the syscall).
    #[inline]
    pub fn elapsed_at(&self, now: SearchInstant) -> Duration {
        now.duration_since(self.start)
    }

    /// Verify all three SearchInstants share variant. Used by the
    /// `start_for` debug-assert and by tests (§6.x) that construct
    /// deliberately mismatched clocks to pin the invariant.
    pub(crate) fn same_variant(&self) -> bool {
        let start_is_wall = matches!(self.start, SearchInstant::Wall(_));
        let deadline_ok = match self.deadline {
            None => true,
            Some(SearchInstant::Wall(_)) => start_is_wall,
            Some(SearchInstant::Cpu(_)) => !start_is_wall,
        };
        let soft_ok = match self.soft_deadline {
            None => true,
            Some(SearchInstant::Wall(_)) => start_is_wall,
            Some(SearchInstant::Cpu(_)) => !start_is_wall,
        };
        deadline_ok && soft_ok
    }

    /// Test-only accessor: returns `Some(ns)` when `start` is `Cpu(ns)`,
    /// `None` for `Wall(_)`. Used by §6.3's
    /// `search_clock_start_for_reads_calling_thread_cpu` test.
    #[cfg(test)]
    pub fn start_cpu_ns(&self) -> Option<u64> {
        match self.start {
            SearchInstant::Wall(_) => None,
            SearchInstant::Cpu(ns) => Some(ns),
        }
    }
}

/// Soft + hard time caps for a single `go` invocation. Returned by
/// [`compute_caps`].
///
/// - `soft` — "Don't start the next iterative-deepening iteration past this
///   point." Polled by `Search::go` BETWEEN iterations only.
/// - `hard` — "Abort the current iteration immediately past this point."
///   Polled by `SearchContext::should_abort` via the deadline path.
/// - `Duration::MAX` is the "no cap" sentinel for either field. Wiring code
///   that converts caps to `Instant`s must guard with
///   `(cap != Duration::MAX).then(|| now + cap)` — `now + Duration::MAX`
///   panics on overflow.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeCaps {
    pub soft: Duration,
    pub hard: Duration,
}

/// Compute soft + hard time caps from `go` parameters and a per-engine
/// `move_overhead` (latency hedge) value. **Pure function**; no clock reads,
/// no engine state — same inputs always give same outputs.
///
/// Implementation per `docs/plans/m3.e.md` §4 and `docs/research/m3-time-management.md`
/// §1, §3, §6, §9 test table. ADR-0017 binds the formulas.
///
/// Headline shapes:
/// - Non-time limits (`infinite` / `ponder` / `depth` / `nodes` / `mate`) → `(MAX, MAX)`.
/// - `movetime N`: `soft = hard = max(1, N - move_overhead)`.
/// - Clock-based: `soft = remaining/denom + increment/2 - move_overhead`,
///   `hard = min(3 × soft, remaining - move_overhead)`. `denom = movestogo`
///   when present and > 0; 20 (sudden death) otherwise.
/// - Increment-only TC (`rem == 0`, `inc > 0`): hard = `3 × soft`, no forfeit
///   clamp (research §5).
/// - Very low time (`rem == 0`, `inc == 0`): `(1ms, 1ms)`.
pub(crate) fn compute_caps(
    limits: &SearchLimits,
    side_to_move: Color,
    move_overhead: u64,
) -> TimeCaps {
    // Non-time limits → no cap.
    if limits.infinite
        || limits.ponder
        || limits.depth.is_some()
        || limits.nodes.is_some()
        || limits.mate.is_some()
    {
        return TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        };
    }

    // movetime overrides clock-based: soft = hard = movetime - move_overhead.
    if let Some(mt) = limits.movetime {
        let v = (mt.max(0) as u64).saturating_sub(move_overhead).max(1);
        let d = Duration::from_millis(v);
        return TimeCaps { soft: d, hard: d };
    }

    // Clock-based.
    let (rem_signed, inc_opt) = match side_to_move {
        Color::White => (limits.wtime, limits.winc),
        Color::Black => (limits.btime, limits.binc),
    };
    let Some(rem_signed) = rem_signed else {
        // No movetime, no clock — degenerate `go` with no time information.
        return TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        };
    };

    let rem = rem_signed.max(0) as u64;
    let inc = inc_opt.unwrap_or(0);

    // Very-low-time floor: both clock and increment zero → emit immediately.
    if rem == 0 && inc == 0 {
        let d = Duration::from_millis(1);
        return TimeCaps { soft: d, hard: d };
    }

    let denom: u64 = match limits.movestogo {
        None => 20,
        Some(0) => 1, // spec violation; defensive fallback
        Some(n) => n as u64,
    };

    let raw_soft = rem / denom + inc / 2;
    let soft_unclamped = raw_soft.saturating_sub(move_overhead);

    // Increment-only TC (rem == 0 with inc > 0): forfeit guard does NOT apply
    // because the increment refills the clock after the move (research §5).
    if rem == 0 {
        let soft = Duration::from_millis(soft_unclamped.max(1));
        let hard = Duration::from_millis(soft_unclamped.saturating_mul(3).max(1));
        return TimeCaps { soft, hard };
    }

    // Forfeit guard: soft and hard both clamped to (rem - move_overhead).max(1).
    let max_clamp = rem.saturating_sub(move_overhead).max(1);
    let soft_ms = soft_unclamped.min(max_clamp).max(1);
    let hard_ms = soft_ms.saturating_mul(3).min(max_clamp).max(1);
    TimeCaps {
        soft: Duration::from_millis(soft_ms),
        hard: Duration::from_millis(hard_ms),
    }
}

/// Resolve the iterative-deepening loop's max depth from `SearchLimits` (M3.E).
///
/// - `Some(d)`: clamp to `MAX_PLY - 1 = 63`.
/// - Time-bounded `go` (any of `infinite`, `ponder`, `movetime`, `nodes`, `mate`,
///   `wtime`, `btime`): `MAX_PLY - 1 = 63`.
/// - Bare `go` (no fields set): legacy fallback `4`.
pub(crate) fn max_depth_from_limits(limits: &SearchLimits) -> u32 {
    use super::MAX_PLY;
    if let Some(d) = limits.depth {
        return d.min(MAX_PLY as u32 - 1);
    }
    let any_other_limit = limits.infinite
        || limits.ponder
        || limits.movetime.is_some()
        || limits.nodes.is_some()
        || limits.mate.is_some()
        || limits.wtime.is_some()
        || limits.btime.is_some();
    if any_other_limit {
        (MAX_PLY as u32) - 1
    } else {
        4
    }
}
