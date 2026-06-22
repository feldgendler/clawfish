//! Search trait and value types.
//!
//! Defined at M2.C so M2.D's random-mover and M3+'s alpha-beta plug into the
//! orchestrator without trait churn. `SearchContext` carries the cancellation
//! flag, `TimeCaps`, the `VirtualClock` choice, and parsed `SearchLimits`;
//! `Search::go` is polled by the orchestrator's worker thread and must obey
//! the worker-local `SearchClock::should_abort` (per ADR-0011 and ADR-0019).
//!
//! M3.C ships the [`AlphaBetaMover`] implementation: fail-soft negamax +
//! alpha-beta with triangular PV recovery, MVV-LVA move ordering, and
//! repetition/50-move draw detection. See ADR-0016 and plan docs/plans/m3.c.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::history::{HistoryTable, MAX_HISTORY};
use crate::tt::{TranspositionTable, TtBound, TtData, score_from_tt, score_to_tt};
use crate::{Color, Move, MoveList, PieceKind, Position};

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
    /// `start_for` debug-assert.
    fn same_variant(&self) -> bool {
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

/// Parsed `go` parameters routed into search. Constructed by `handle_go` from
/// `GoParams`; `searchmoves` is already validated against the current
/// position (bad entries silently dropped — plan §6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchLimits {
    /// `go depth <N>`: cap the search at N plies. `None` = no depth cap.
    pub depth: Option<u32>,
    /// `go nodes <N>`: stop after evaluating N nodes. `None` = no node cap.
    pub nodes: Option<u64>,
    /// `go movetime <ms>`. Signed to mirror `wtime`/`btime` (the UCI clock
    /// can go negative under time-trouble overshoot); negative `movetime`
    /// in practice means "search briefly" — the engine treats `Some(<= 0)`
    /// the same as `Some(0)`.
    pub movetime: Option<i64>,
    /// `go mate <N>`: search for a mate in N moves. `None` = no mate constraint.
    pub mate: Option<u32>,
    /// `go infinite`: search until a `stop` command is received.
    pub infinite: bool,
    /// `go ponder`: search in pondering mode; wait for `ponderhit` or `stop`.
    pub ponder: bool,
    /// White's remaining clock time in milliseconds.
    pub wtime: Option<i64>,
    /// Black's remaining clock time in milliseconds.
    pub btime: Option<i64>,
    /// White's per-move increment in milliseconds.
    pub winc: Option<u64>,
    /// Black's per-move increment in milliseconds.
    pub binc: Option<u64>,
    /// Moves remaining until the next time control. `None` = sudden death.
    pub movestogo: Option<u32>,
    /// Restrict candidate moves to this set. `None` = no restriction.
    /// `Some(empty)` is a degenerate case — search should emit `bestmove
    /// 0000` since no candidate exists.
    pub searchmoves: Option<Vec<Move>>,
}

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
    /// Flipped by the orchestrator on `stop` / time expiry. Polled via
    /// `SearchClock::should_abort`. Cleared by the orchestrator at the
    /// start of each `go`.
    pub stop: Arc<AtomicBool>,
    /// `pub(crate)` because `TimeCaps` itself is `pub(crate)` — keeping the
    /// field's visibility no wider than its type. Other fields stay `pub`
    /// because their types are crate-public. The harness binary doesn't
    /// construct `SearchContext` (it talks UCI), so `pub(crate)` is
    /// sufficient.
    pub(crate) caps: TimeCaps,
    /// `VirtualClock` UCI option (ELOH.C). When `true`, the worker thread's
    /// `SearchClock` uses thread-CPU-time; when `false`, wallclock.
    pub virtual_clock: bool,
    /// Parsed `go` parameters for this search invocation.
    pub limits: SearchLimits,
    /// Zobrist trajectory from the start of the game through the current
    /// position. Cloned from `Engine::game_history` at `go`-start.
    /// M3.C negamax will push/pop entries on make/unmake during recursion.
    pub history: Vec<u64>,
    /// M4.A: shared transposition table. `None` for tests / search impls
    /// without TT (e.g. `MockSearch`); `Some` in production via
    /// `Engine::handle_go`/`handle_bench`. Visibility scoped to the crate
    /// because `TranspositionTable` is itself `pub(crate)`.
    pub(crate) tt: Option<Arc<TranspositionTable>>,
}

/// Result of one `go` invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// `None` ⇒ the orchestrator emits `bestmove 0000` (spec line 49).
    /// `Some(mv)` ⇒ the orchestrator emits `bestmove <uci>`.
    pub bestmove: Option<Move>,
    /// Move to ponder on, if any. `None` when the implementation does not suggest a ponder move.
    pub ponder: Option<Move>,
    /// Depth of the completed search (plies). 0 when no moves were evaluated.
    pub depth: u32,
    /// Score from the side-to-move's perspective. May carry a mate-relative
    /// score (`MATE - ply` for winning, `-(MATE - ply)` for losing) rather
    /// than a true centipawn value when the search returned a mate. M3.C-era
    /// contract gap; planned cleanup splits this into `score_cp` (true
    /// centipawns; `None` when score is mate) plus `score_mate_plies` (signed
    /// plies-to-mate; `None` when score is cp). Consumers wanting cp-vs-mate
    /// discrimination should call `score_to_uci`. Deferred fix: M3.E or a
    /// separate M3.C+1 cleanup commit.
    pub score_cp: Option<i32>,
    /// Total nodes evaluated during this search invocation.
    pub nodes: u64,
}

/// Search interface every implementation honors. `Send` is required because
/// implementations are moved into per-`go` worker threads via
/// `thread::spawn`. See ADR-0011 §"`Search` trait — committed at M2".
pub trait Search: Send {
    /// Run a search. Must obey `ctx`: poll cancellation, respect deadline,
    /// emit `info` lines via `info_sink`, return cleanly on cancellation.
    /// Must not write stdout directly. Must not read stdin.
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult;

    /// Notify the search of a new `Random_Seed` value. Default: no-op
    /// (most search impls don't have configurable seeds). M3+ alpha-beta
    /// will not implement this; it stays a no-op.
    fn set_seed(&mut self, _seed: u64) {}

    /// Update the adaptive aspiration parameters. Called by the engine under
    /// the search lock (after joining any in-flight worker) whenever one of
    /// the four `Aspiration_*` UCI options changes. Default: no-op.
    fn set_aspiration_params(&mut self, _params: AspirationParams) {}

    /// Notify the search that a new game has started (`ucinewgame`). Default:
    /// no-op. M4 will clear killer/history/TT here.
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// M3.C — AlphaBetaMover: fail-soft negamax + alpha-beta + triangular PV.
// ---------------------------------------------------------------------------

/// Maximum search depth in plies. PV table is sized to this constant.
pub(crate) const MAX_PLY: usize = 64;

/// Mate score: returned when a side is delivering mate. Ply-adjusted so faster
/// mates compare higher.
const MATE: i32 = 30_000;

/// Sentinel infinity value: wider than any MATE score; used for the initial
/// alpha/beta window.
const INF: i32 = 30_001;

/// Minimum mate score magnitude; used to distinguish mate scores from
/// centipawn scores in `score_to_uci`. A score with `|score| >= MATE_IN_MAX_PLY`
/// is a mate score.
pub(crate) const MATE_IN_MAX_PLY: i32 = MATE - MAX_PLY as i32; // 29_936

/// Minimum depth at which aspiration narrows the window. Below this, the
/// outer loop passes `(-INF, INF)` to negamax — same as M3.E behavior.
/// Depths 1–5 have prior-iteration scores that are too volatile to seed a
/// tight window (research §4 cites threshold values from 4 through 6 as
/// common; this project uses 6 because empirical tc=10+0.1 SPRT showed
/// threshold=4 regressed by ~22 Elo while threshold=6 gained ~+66 Elo
/// against the same baseline — fast-TC-only games reach ~depth 7, so
/// threshold=4 exposed too many shallow iterations to aspiration's
/// re-search overhead).
pub(crate) const ASPIRATION_MIN_DEPTH: u32 = 6;

/// First-try aspiration half-width in centipawns. Window is
/// `(prior - HALF_WIDTH, prior + HALF_WIDTH)`. CPW workhorse default;
/// roadmap §M4.D pins ±50 with a documented post-merge width-tune campaign
/// over ±25 / ±75 / ±100. Also the OFF-path fallback returned by
/// `aspiration_half_width` when `adaptive == false`.
const ASPIRATION_HALF_WIDTH: i32 = 50;

/// Default centi-K multiplier for the adaptive aspiration half-width formula
/// `half = clamp((k_centi * |d1 - d2| + 50) / 100, min, max)`. K=2.00 centers
/// the window at the proven fixed ±50 for the median ID score-delta (~25 cp).
const ASPIRATION_K_CENTI_DEFAULT: i32 = 200;

/// Default minimum adaptive aspiration half-width in centipawns. Prevents the
/// window from narrowing so tightly on stable positions that a single-cp
/// fluctuation causes a fail. Mirrors the item-5 hand-pick `MIN=25`.
const ASPIRATION_MIN_DEFAULT: i32 = 25;

/// Default maximum adaptive aspiration half-width in centipawns. Caps the
/// window on volatile positions, preventing a first-try window wider than ±50
/// on quiet positions (limit is ±250 ≈ 5 pawns). Mirrors the item-5
/// hand-pick `MAX=250`.
const ASPIRATION_MAX_DEFAULT: i32 = 250;

/// Default for the `Aspiration_Adaptive` UCI option. `true` enables the
/// SPRT-confirmed (+13.03 Elo) adaptive half-width path. Set to `false`
/// explicitly (via `setoption name Aspiration_Adaptive value false`) to
/// restore the fixed-±50 OFF path for comparison or diagnostics.
const ASPIRATION_ADAPTIVE_DEFAULT: bool = true;

/// Default lower bound of the adaptive-aspiration depth band. The adaptive
/// half-width formula is applied only when
/// `adaptive_min_depth ≤ depth ≤ adaptive_max_depth`. Defaults to
/// `ASPIRATION_MIN_DEPTH` (6) — the shallowest depth at which aspiration is
/// active at all — so the default band covers the entire aspiration domain.
/// UCI `Aspiration_AdaptiveMinDepth default 6`.
const ASPIRATION_ADAPTIVE_MIN_DEPTH_DEFAULT: u32 = ASPIRATION_MIN_DEPTH;

/// Default upper bound of the adaptive-aspiration depth band. Set to
/// `MAX_PLY as u32` (64) — tied to the symbol so a future `MAX_PLY` bump keeps
/// the default band a no-gate by construction. The ID loop hard-clamps to
/// `MAX_PLY − 1 = 63` on every path, so `[6, 64]` covers the entire reachable
/// depth domain and is a structural no-op gate. UCI
/// `Aspiration_AdaptiveMaxDepth default 64`.
const ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT: u32 = MAX_PLY as u32;

/// Parameters for the adaptive aspiration half-width feature. Stored in
/// `AlphaBetaMover` and set by `Engine::handle_setoption` via the
/// `set_aspiration_params` method (same worker-join discipline as `set_seed`).
///
/// **Production default: `adaptive == true`** (SPRT-confirmed +13.03 Elo).
/// When `adaptive == false` (explicit `setoption name Aspiration_Adaptive
/// value false`), `aspiration_half_width` returns the fixed
/// `ASPIRATION_HALF_WIDTH` constant for every input — byte-identical to the
/// pre-Unit-1 baseline.
///
/// The `adaptive_min_depth`/`adaptive_max_depth` band gate (Unit 2) is only
/// consulted when `adaptive == true` and `score_d2` is `Some` — the `!adaptive`
/// and `score_d2.is_none()` early-returns precede the band check, preserving
/// the OFF-path byte-identity invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AspirationParams {
    /// When `false`, `aspiration_half_width` returns the fixed fallback for
    /// every input, preserving byte-identical bench behavior.
    pub adaptive: bool,
    /// Centi-K multiplier (K × 100). UCI option `Aspiration_K default 200`.
    pub k_centi: i32,
    /// Minimum adaptive half-width in centipawns. UCI `Aspiration_Min default 25`.
    pub min: i32,
    /// Maximum adaptive half-width in centipawns. UCI `Aspiration_Max default 250`.
    pub max: i32,
    /// Lower bound of the adaptive-width depth band (inclusive). The formula is
    /// only applied at `depth ≥ adaptive_min_depth`. UCI
    /// `Aspiration_AdaptiveMinDepth default 6`.
    pub adaptive_min_depth: u32,
    /// Upper bound of the adaptive-width depth band (inclusive). The formula is
    /// only applied at `depth ≤ adaptive_max_depth`. UCI
    /// `Aspiration_AdaptiveMaxDepth default 64`.
    ///
    /// When `adaptive_min_depth > adaptive_max_depth` (inverted band), the
    /// band check is always true → fixed-50 everywhere. Accepted degenerate;
    /// falls back to baseline behavior.
    pub adaptive_max_depth: u32,
}

impl Default for AspirationParams {
    fn default() -> Self {
        Self {
            adaptive: ASPIRATION_ADAPTIVE_DEFAULT,
            k_centi: ASPIRATION_K_CENTI_DEFAULT,
            min: ASPIRATION_MIN_DEFAULT,
            max: ASPIRATION_MAX_DEFAULT,
            adaptive_min_depth: ASPIRATION_ADAPTIVE_MIN_DEPTH_DEFAULT,
            adaptive_max_depth: ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT,
        }
    }
}

/// Minimum depth at which NMP is attempted. Below this, the null-search's
/// `depth - 1 - R` would be ≤ 0, dispatching to qsearch — defeating the
/// cost/benefit calculation. ADR-0023 §2.
pub(crate) const NMP_MIN_DEPTH: u32 = 3;

/// NMP base reduction: `R = NMP_BASE_R + depth / NMP_DEPTH_DIVISOR`.
/// CPW workhorse default. ADR-0023 §1.
pub(crate) const NMP_BASE_R: u32 = 2;

/// NMP depth divisor in the reduction formula. ADR-0023 §1.
pub(crate) const NMP_DEPTH_DIVISOR: u32 = 6;

/// Upper depth bound for reverse-futility pruning. At depths above this,
/// `static_eval - margin*depth >= beta` is rarely true (the margin grows
/// faster than realistic eval surplus), and the tactical-blindness risk
/// from a depth-7+ refutation grows. Stockfish DD historical: `depth < 7`
/// (i.e., depth ≤ 6); ADR-0024 §1.
pub(crate) const RFP_MAX_DEPTH: u32 = 6;

/// Linear coefficient for reverse-futility pruning's depth-scaled margin.
/// `margin = RFP_MARGIN_PER_DEPTH * depth`. At depth=1, 100 cp (≈ one
/// pawn); at depth=6, 600 cp (≈ a rook). Conservative v1 starting value;
/// CPW workhorse alternative is 150 (post-landing SPRT-tune candidate).
/// ADR-0024 §1.
pub(crate) const RFP_MARGIN_PER_DEPTH: i32 = 100;

/// Minimum depth at which LMR is considered. Below this, reduced searches are
/// too shallow to justify the complexity and tend to degenerate into qsearch.
/// M5.C plan / forthcoming ADR-0025.
pub(crate) const LMR_MIN_DEPTH: u32 = 3;

/// Quiets are indexed 1-based within the node's quiet-only ordering. The
/// first quiet searched at a node (index 1) is never reduced; reductions may
/// begin at the second quiet (index 2).
pub(crate) const LMR_MIN_QUIET_INDEX: u32 = 2;

/// Additive base term for the M5.C log-log LMR formula.
pub(crate) const LMR_BASE_OFFSET: f64 = 0.99;

/// Divisor in the M5.C log-log LMR formula.
#[allow(clippy::approx_constant)] // conservative-band placeholder; SPRT-tunable
pub(crate) const LMR_LOG_DIVISOR: f64 = 3.14;

/// Quiets with history scores at or above this threshold are trusted and are
/// exempt from LMR in M5.C v1.
pub(crate) const LMR_HIGH_HISTORY_THRESHOLD: i16 = 4_096;

/// FFP node-level depth ceiling (M5.D — ADR-0026 §4). At depth > FFP_MAX_DEPTH,
/// FFP does not fire.
///
/// **v2: 1** (frontier-only, Heinz 1998 original formulation). The v1 setting
/// (`FFP_MAX_DEPTH = 2`) layered "extended futility" at depth 2 on top, but
/// the v1 mixed-TC SPRT (M5.D landing) showed strong slow-TC regression
/// implicating depth-2 FFP as the cause: per-TC bimodal pattern with positive
/// fast TC (10+0.1: 56.5%, 20+0.2: 68.3%) and negative slow TC (40+0.4:
/// 30.2%, 60+0.6: 40.6%). Restricting to depth 1 (Heinz's classical scope)
/// keeps the cheap frontier prune but drops the deeper-search tactical-
/// blindness risk. See [`bench/sprt/2026-05-06-m5.d-vs-m5c-mixed-tc.md`] and
/// the M5.D retrospective for the v1 → v2 reasoning.
///
/// `FFP_MARGIN_D2 = 150` and `FFP_MARGIN_D3 = 250` are kept as named
/// constants (forward compat) but inactive at v2.
pub(crate) const FFP_MAX_DEPTH: u32 = 1;

/// FFP margin at depth 1 (frontier nodes). 100 cp ≈ one pawn. Conservative v1
/// per ADR-0026 §4. TalkChess t=74403 successful at 100 cp / d=1 in `{100, 150}`.
pub(crate) const FFP_MARGIN_D1: i32 = 100;

/// FFP margin at depth 2 (pre-frontier / Heinz "extended futility"). 150 cp.
/// **Inactive at v2** (FFP_MAX_DEPTH = 1 keeps depth 2 from firing). Defined
/// here so a post-tune `FFP_MAX_DEPTH = 2` revival can re-activate it without
/// churn. ADR-0026 §4.
pub(crate) const FFP_MARGIN_D2: i32 = 150;

/// FFP margin at depth 3 (pre-pre-frontier / "limited razoring"). **Inactive
/// at v2** (FFP_MAX_DEPTH = 1 keeps depth 3 from firing). Defined here so a
/// post-tune `FFP_MAX_DEPTH = 3` SPRT can activate it without churn.
/// ADR-0026 §4.
pub(crate) const FFP_MARGIN_D3: i32 = 250;

/// Compile-time invariant: at v1, FFP fires at `depth ≤ 2` and LMR fires at
/// `depth ≥ 3`, so the two pruning paths cannot co-fire at any node. This
/// assertion is load-bearing as a tripwire — a future tuning that raises
/// `FFP_MAX_DEPTH` to overlap with `LMR_MIN_DEPTH` MUST update ADR-0026 §6
/// (the per-quiet ordinal semantics question) and remove this assertion in
/// the same patch. Mirrors ADR-0025 §3's `LMR_HIGH_HISTORY_THRESHOLD <=
/// MAX_HISTORY` invariant pattern.
const _: () = assert!(FFP_MAX_DEPTH < LMR_MIN_DEPTH);

/// M5.G singular-extension minimum remaining depth. Below this, SE is too
/// shallow to justify the verification-search cost, and the literature
/// majority sets the threshold here. ADR-0029 §1.
pub(crate) const SE_MIN_DEPTH: u32 = 6;

/// M5.G singular-extension margin per ply. `singular_beta = tt_score - depth *
/// SE_MARGIN_PER_DEPTH`. Xiphos / Ethereal defaults; conservative starting
/// value. Post-landing SPRT-tune candidate (tuning backlog: try 2). ADR-0029 §2.
pub(crate) const SE_MARGIN_PER_DEPTH: i32 = 1;

/// M5.G singular-extension TT-entry depth tolerance. SE fires only when the
/// TT entry's stored depth is at least `depth - SE_TT_DEPTH_DELTA`, i.e. the
/// TT score is "fresh enough" to be evidence at the current depth. Xiphos
/// default. ADR-0029 §3.
pub(crate) const SE_TT_DEPTH_DELTA: u32 = 3;

// Depth-disjointness invariants. These are load-bearing tripwires: a future
// tuning that moves FFP_MAX_DEPTH or SE_MIN_DEPTH into overlap MUST update
// the corresponding ADRs and remove the violated assertion.
//
// FFP ≤ LMR boundary (ADR-0026 §6 + ADR-0025 §3):
const _: () = assert!(FFP_MAX_DEPTH < SE_MIN_DEPTH);

/// Construct the "iteration-1 aborted before any root improvement" fallback
/// result for `Search::go` (M3.E).
///
/// Returns `(depth=0, bestmove, score)` where bestmove comes from `pv[0][0]`
/// if the in-progress aborted iteration populated it, else `None`; score
/// comes from `root_score.unwrap_or(0)` (the lockstep root-score field set
/// by negamax when alpha was improved at ply 0).
///
/// **Why an extracted helper.** The fallback path is structurally unreachable
/// at M3.E scope: iteration 1 from any chess position visits fewer than 4096
/// nodes (startpos depth-1: ~20; Kiwipete depth-1: ~100; max board branching
/// 218 « 4096), so the in-iteration cancellation cadence (`nodes & 4095 ==
/// 0`) never fires during iteration 1. Iteration 1 therefore always completes
/// and sets `last_complete = Some(...)`, so the inline call site never
/// executes. Mutations on the inline `pv.lengths[0] > 0` expression cannot be
/// killed by any chess fixture. Extracting into a named helper makes the
/// helper's body directly unit-testable with synthetic `PvTable` fixtures —
/// same precedent as M3.D's `negate_window` extraction.
fn aborted_fallback_result(pv: &PvTable, root_score: Option<i32>) -> (u32, Option<Move>, i32) {
    let bm = (pv.lengths[0] > 0).then(|| pv.moves[0][0]);
    let sc = root_score.unwrap_or(0);
    (0, bm, sc)
}

/// Negates and swaps the alpha/beta window for a recursive negamax/qsearch call.
///
/// Negamax's recursive contract: child sees `(child_alpha, child_beta) =
/// (-parent_beta, -parent_alpha)`. Extracting this into a named helper means
/// the bound-negation is unit-testable in isolation and the call sites become
/// `self.search(pos, negate_window(alpha, beta), ply+1, ctx)` instead of
/// open-coded `(-beta, -alpha)` — which is bug-prone (deleting either `-`
/// silently corrupts the search and is hard to catch via end-to-end tests at
/// shallow recursion depth).
fn negate_window(alpha: i32, beta: i32) -> (i32, i32) {
    (-beta, -alpha)
}

/// Volatility-responsive first-try aspiration half-width (Unit 1 + Unit 2).
///
/// Early-return order is load-bearing — each guard preserves a byte-identical
/// sub-path from prior milestones:
/// 1. `score_d2.is_none()` → fixed-50 (no completed prior-prior iteration to
///    delta against; first adaptive-eligible ID iteration always falls here).
/// 2. `!params.adaptive` → fixed-50 (OFF-path byte-identical to pre-Unit-1;
///    this guard precedes the band check so an explicit `adaptive=false` bench
///    never touches the band fields).
/// 3. Band gate (`depth < adaptive_min_depth || depth > adaptive_max_depth`) →
///    fixed-50 (Unit 2: restrict the adaptive formula to the `[min_depth,
///    max_depth]` closed interval). An inverted band (`min > max`) makes this
///    predicate permanently true, falling back to fixed-50 everywhere — the
///    accepted degenerate documented in plan §2.3.
/// 4. Adaptive formula:
///    `clamp((k_centi * |score_d1 - d2| + 50) / 100, min, max)`.
///    The `+ 50` rounds half-away-from-zero before the integer division by 100,
///    giving deterministic platform-independent arithmetic.
fn aspiration_half_width(
    score_d1: i32,
    score_d2: Option<i32>,
    params: &AspirationParams,
    depth: u32,
) -> i32 {
    let Some(d2) = score_d2 else {
        return ASPIRATION_HALF_WIDTH;
    };
    if !params.adaptive {
        return ASPIRATION_HALF_WIDTH;
    }
    if depth < params.adaptive_min_depth || depth > params.adaptive_max_depth {
        return ASPIRATION_HALF_WIDTH;
    }
    ((params.k_centi * (score_d1 - d2).abs() + 50) / 100).clamp(params.min, params.max)
}

/// First-try aspiration window. Returns `(-INF, INF)` (equivalent to no
/// aspiration) when:
///
/// 1. `depth < ASPIRATION_MIN_DEPTH` — too-shallow iteration; prior
///    score is unstable.
/// 2. `prior_score == None` — no prior iteration to seed from (the first
///    ID iteration of the current `go`).
///
/// Otherwise uses `aspiration_half_width` to compute the first-try half-width
/// from `params` and the two most recent completed ID scores, then returns
/// `(prior - half, prior + half)`. Mate-score `prior_score` values produce a
/// window straddling the mate boundary; `widen_after_fail` handles the
/// resulting first-try fail via the asymmetric full-window re-search
/// (research §7.2).
///
/// When `params.adaptive == false` (explicit OFF), `aspiration_half_width`
/// returns exactly `ASPIRATION_HALF_WIDTH` for any input, so this function is
/// byte-identical to the pre-Unit-1 signature for every (prior, depth) pair.
///
/// Pure function. Pinned by AS1–AS5b.
fn aspiration_window(
    prior_score: Option<i32>,
    prior_prior_score: Option<i32>,
    params: &AspirationParams,
    depth: u32,
) -> (i32, i32) {
    if depth < ASPIRATION_MIN_DEPTH {
        return (-INF, INF);
    }
    let Some(prior) = prior_score else {
        return (-INF, INF);
    };
    let half = aspiration_half_width(prior, prior_prior_score, params, depth);
    (prior - half, prior + half)
}

/// Two-tier asymmetric widening on aspiration failure. Computes the
/// re-search window from the failed-try's returned score and the failed
/// try's `(prev_alpha, prev_beta)` window.
///
/// **Fail-high** (`returned >= prev_beta`): re-search `(returned, +INF)` —
/// keep the proved lower bound as the new alpha; widen the upper side.
///
/// **Fail-low** (`returned <= prev_alpha`): re-search `(-INF, returned)` —
/// keep the proved upper bound as the new beta; widen the lower side.
///
/// **Caller contract**: only called when `(returned >= prev_beta) ||
/// (returned <= prev_alpha)`. The window-contained case is short-circuited
/// by the caller. Pinned by AS9b (debug panic if invariant violated).
///
/// Pure function. Pinned by AS6–AS9b.
fn widen_after_fail(returned: i32, prev_alpha: i32, prev_beta: i32) -> (i32, i32) {
    debug_assert!(
        returned >= prev_beta || returned <= prev_alpha,
        "widen_after_fail called with window-contained score: \
         returned={returned} prev_alpha={prev_alpha} prev_beta={prev_beta}"
    );
    if returned >= prev_beta {
        (returned, INF)
    } else {
        // returned <= prev_alpha by the debug_assert
        (-INF, returned)
    }
}

/// Return the root bestmove for the iteration's `last_complete` snapshot.
/// Prefers `pv[0][0]` when populated; falls back to the root TT entry's
/// `best_move` field when PV[0] is empty (rare empty-PV-after-aspiration-
/// re-search edge case — see §3.2 + §3.7 of the M4.D plan).
///
/// The sentinel `best_move == 0` case returns `None` rather than decoding
/// to a useless `a1-a1-Quiet` move.
///
/// Pure function (modulo TT probe). Pinned by AS24a–AS24d.
fn extract_bestmove_or_tt_fallback(
    pv: &PvTable,
    tt: Option<&TranspositionTable>,
    root_key: u64,
) -> Option<Move> {
    if pv.lengths[0] > 0 {
        return Some(pv.moves[0][0]);
    }
    let entry = tt?.probe(root_key)?;
    if entry.best_move == 0 {
        return None;
    }
    Some(Move::from_bits(entry.best_move))
}

/// NMP depth reduction. Returns `NMP_BASE_R + depth / NMP_DEPTH_DIVISOR`
/// (= `2 + depth/6`). Pure function. Extracted as a named helper so
/// mutations on the formula constants are directly unit-testable
/// (M3.D `negate_window` precedent).
pub(crate) fn null_move_reduction(depth: u32) -> u32 {
    NMP_BASE_R + depth / NMP_DEPTH_DIVISOR
}

/// RFP depth-scaled margin. Returns `RFP_MARGIN_PER_DEPTH * depth as i32`.
/// Pure function. Extracted as a named helper so mutations on the formula
/// constants are directly unit-testable (M3.D `negate_window` /
/// M3.E `aborted_fallback_result` / M5.A `null_move_reduction` precedent).
///
/// `RFP_MARGIN_PER_DEPTH * depth as i32` cannot overflow `i32` at any depth
/// below ~21M; the `depth <= RFP_MAX_DEPTH = 6` gate makes this trivially safe.
pub(crate) fn reverse_futility_margin(depth: u32) -> i32 {
    RFP_MARGIN_PER_DEPTH * depth as i32
}

/// LMR base reduction. Inputs are `(depth, quiet_index)` where `quiet_index`
/// is 1-based within the quiet-only ordering at the current node. Returns
/// `0` for `depth < LMR_MIN_DEPTH` or `quiet_index < LMR_MIN_QUIET_INDEX`
/// (in-domain guard pinned by tests; do not rely on caller-side gates
/// alone). Otherwise computes `floor(LMR_BASE_OFFSET + ln(depth) *
/// ln(quiet_index) / LMR_LOG_DIVISOR)` and clamps to `0..=(depth - 2)` so
/// the reduced child is always at least depth 1. Extracted as a named
/// helper so formula mutations are directly unit-testable (M3.D
/// `negate_window` precedent). ADR-0025 §4.
pub(crate) fn late_move_reduction(depth: u32, quiet_index: u32) -> u32 {
    if depth < LMR_MIN_DEPTH || quiet_index < LMR_MIN_QUIET_INDEX {
        return 0;
    }

    let raw = LMR_BASE_OFFSET + (depth as f64).ln() * (quiet_index as f64).ln() / LMR_LOG_DIVISOR;
    let reduction = raw.floor() as u32;
    reduction.clamp(0, depth.saturating_sub(2))
}

/// Per-depth FFP margin (M5.D — ADR-0026 §4 + §5). Returns 0 outside
/// `[1, FFP_MAX_DEPTH]`.
///
/// Defining the depth-3 entry here (even though `FFP_MAX_DEPTH = 2` disables
/// it at v1) keeps the constant inventory consistent with the roadmap's CPW
/// reference and lets a future SPRT raise `FFP_MAX_DEPTH` without revisiting
/// the table.
pub(crate) fn frontier_futility_margin(depth: u32) -> i32 {
    match depth {
        1 => FFP_MARGIN_D1,
        2 => FFP_MARGIN_D2,
        3 => FFP_MARGIN_D3, // inactive until FFP_MAX_DEPTH raised
        _ => 0,
    }
}

/// FFP gate test combined with proved fail-soft upper bound (M5.D — ADR-0026
/// §5). `static_eval` and `alpha` are STM-relative centipawn scores at the
/// **parent** node (pre-move).
///
/// Returns `Some(static_eval + margin)` iff `depth ∈ [1, FFP_MAX_DEPTH]` AND
/// `static_eval + margin <= alpha` (saturating addition); `None` otherwise.
/// The `Some` payload is the FFP-proved fail-soft upper bound on the move's
/// true score, guaranteed `<= alpha` by the gate. The call site uses it to
/// floor `best` (ADR-0026 §7) without a separate recompute.
///
/// **One helper, two responsibilities by design.** Splitting into a bool
/// gate + a separate i32 bound calc would create a saturation-overflow
/// asymmetry: the gate must use `saturating_add` (else `+`-overflow could
/// let the bound test pass when arithmetic says no), and the call-site
/// contribution must reuse the same saturated value (else the `+`-overflow
/// could re-emerge at the call site). The single helper closes the
/// asymmetry by computing the bound once.
///
/// **Domain guard.** The depth-range check is helper-level defense-in-depth
/// (M5.C `late_move_reduction` precedent): the helper returns `None` at
/// `d == 0` or `d > FFP_MAX_DEPTH` even with mathematically passing
/// `margin`/`alpha` — guards against a refactor that drops the call-site
/// `depth <= FFP_MAX_DEPTH` gate.
///
/// **Overflow defense.** `saturating_add` defends against `i32` overflow
/// when `static_eval` is near `MATE`. The node-level gate's
/// `alpha.abs() < MATE_IN_MAX_PLY` makes that case unreachable in
/// production, but the helper is `pub(crate)` and unit-tested independently
/// — overflow on a unit-test edge would be a confusing failure.
///
/// **Inequality.** `<=` not `<`: a move whose true score could exactly
/// equal alpha does not improve alpha (fail-soft requires strict
/// improvement); pruning at equality is the standard CPW form.
pub(crate) fn ffp_pruned_bound(static_eval: i32, depth: u32, alpha: i32) -> Option<i32> {
    if depth == 0 || depth > FFP_MAX_DEPTH {
        return None;
    }
    let bound = static_eval.saturating_add(frontier_futility_margin(depth));
    if bound <= alpha { Some(bound) } else { None }
}

/// Per-quiet LMR eligibility, applied after the caller has already cleared
/// the node-level gates (`ply > 0`, `!is_pv`, `depth >= LMR_MIN_DEPTH`,
/// `!in_check`). Returns `false` for non-quiets, for quiets at index
/// `< LMR_MIN_QUIET_INDEX`, for either killer slot, and for quiets whose
/// history score has reached `LMR_HIGH_HISTORY_THRESHOLD`. The TT move is
/// implicitly exempt: after the step-12 reorder it is the first searched
/// move (ADR-0018 §12), so a quiet TT move receives `quiet_index = 1` and
/// the floor at `LMR_MIN_QUIET_INDEX = 2` rules it out without an explicit
/// TT-move parameter. ADR-0025 §3.
fn is_lmr_eligible_quiet(
    mv: Move,
    pos: &Position,
    quiet_index: u32,
    killer0: Move,
    killer1: Move,
    history_table: &HistoryTable,
) -> bool {
    if !is_quiet(mv) || quiet_index < LMR_MIN_QUIET_INDEX {
        return false;
    }
    if mv == killer0 || mv == killer1 {
        return false;
    }
    history_table.score(pos.side_to_move(), mv.from_square(), mv.to_square())
        < LMR_HIGH_HISTORY_THRESHOLD
}

/// Classical M5.C re-search policy: re-search at full depth iff the reduced
/// first pass returns above alpha.
fn lmr_needs_full_research(reduced_score: i32, alpha: i32) -> bool {
    reduced_score > alpha
}

/// TT bound classification for a completed negamax node, including the M5.C
/// soundness rule that a node whose best score is only reduced-depth-proven
/// must not advertise a full-depth TT bound.
fn tt_bound_for_completed_node(
    best: i32,
    beta: i32,
    original_alpha: i32,
    best_is_full_depth: bool,
) -> Option<TtBound> {
    if !best_is_full_depth {
        return None;
    }
    Some(if best >= beta {
        TtBound::Lower
    } else if best > original_alpha {
        TtBound::Exact
    } else {
        TtBound::Upper
    })
}

/// Classify the bound for a completed qsearch result (three-way, M5.F.1).
///
/// `alpha_entry` is the node's window alpha at entry (after mate-distance
/// pruning, BEFORE the stand-pat raise) — i.e. the caller-facing lower bound.
///
/// - `best >= beta`: `Lower` (fail-high cutoff).
/// - `alpha_entry < best < beta`: `Exact` (improved the entry alpha without a
///   cutoff — a fully-searched fail-soft value).
/// - otherwise (`best <= alpha_entry`): `Upper` (fail-low).
///
/// **Why `Exact` is sound here, despite Stockfish 45e5e65.** That commit keeps
/// qsearch to Lower/Upper because a qsearch `Exact` (restricted to captures /
/// evasions, not all legal moves) could short-circuit a future *negamax* probe
/// with a value that isn't a true all-moves minimax. In *this* engine that
/// path does not exist: `negamax` delegates to `qsearch` at `depth == 0`
/// (`search.rs` step ~1480) **before** its TT-cutoff probe, and that probe only
/// cuts when `entry.depth >= depth` — unreachable for a depth-0 qsearch entry
/// at any `depth >= 1`. So a qsearch entry never triggers a negamax cutoff
/// (negamax only reads its `best_move` for ordering). The only consumer of a
/// qsearch `Exact` is qsearch's own re-probe, which returns it unconditionally;
/// that is correct because a completed fail-soft loop with `alpha_entry < best
/// < beta` searched every capture full-window (no PVS null-window) and the
/// move that set `best` returned its exact value — window-independent within
/// qsearch's own (captures + stand-pat) value model. M5.F.1 tuning campaign vs
/// M6.J (tuning-backlog M5.F item 1).
///
/// Terminal cases (handled at their own call sites with a forced
/// `Exact` bound, NOT this helper): true stalemate (returns 0) and mate
/// at horizon (returns `-(MATE - ply)`).
///
/// Precondition: `best != -INF`. The `-INF` seed at the in-check arm
/// either recurses (in which case any executed evasion's negated child
/// score satisfies `score > -INF`, so `best > -INF` after the first
/// completed iteration) or hits the empty-evasion terminal which
/// returns `-(MATE - ply)` directly via path D — that path stores
/// before reaching the post-loop classifier and never invokes this
/// helper. Pinned by debug_assert.
pub(crate) fn qsearch_tt_bound_for_completed_node(
    best: i32,
    alpha_entry: i32,
    beta: i32,
) -> TtBound {
    debug_assert!(
        best != -INF,
        "qsearch_tt_bound_for_completed_node: best == -INF unreachable at production sites"
    );
    if best >= beta {
        TtBound::Lower
    } else if best > alpha_entry {
        TtBound::Exact
    } else {
        TtBound::Upper
    }
}

/// Singular-extension β (M5.G — ADR-0029 §2). `tt_score - depth * SE_MARGIN_PER_DEPTH`,
/// floored at `-(MATE - 1)` to avoid mate-score wrap when `tt_score` is near `-MATE`.
/// Pure function. The caller-side `tt_score.abs() < MATE_IN_MAX_PLY` gate
/// (§5 of `singular_extension_eligible`) makes the floor a defense-in-depth
/// invariant rather than a hot-path concern, but the floor is unit-tested
/// independently.
pub(crate) fn singular_beta(tt_score: i32, depth: u32) -> i32 {
    let raw = tt_score.saturating_sub(SE_MARGIN_PER_DEPTH * depth as i32);
    raw.max(-(MATE - 1))
}

/// Singular-extension verification-search remaining depth (M5.G — ADR-0029 §3).
/// `(depth - 1) / 2` (integer division), with a debug-assert that
/// `depth >= SE_MIN_DEPTH`. At SE_MIN_DEPTH = 8 this yields verif_depth = 3.
/// Pure function.
///
/// **Caller obligation.** `depth >= SE_MIN_DEPTH` is the contract.
/// `(0 - 1)` underflows in `u32` (panicking in debug, wrapping in release —
/// `(0_u32.wrapping_sub(1)) / 2 = u32::MAX / 2`, which would then propagate
/// into a deep verification recursion). The eligibility predicate's clause 5
/// (`depth >= SE_MIN_DEPTH`) is the only call-site protection; the helper's
/// `debug_assert!` pins this in debug builds.
pub(crate) fn verification_depth(depth: u32) -> u32 {
    debug_assert!(
        depth >= SE_MIN_DEPTH,
        "verification_depth: caller must have passed the SE_MIN_DEPTH gate; depth={depth}"
    );
    (depth - 1) / 2
}

/// Singular-extension full eligibility predicate (M5.G — ADR-0029 §1).
/// All nine conditions must hold. Pure function.
///
/// Conjunction (evaluation order; cheapest predicates first):
/// 1. `excluded_move.is_none()` — re-entrancy guard: the verification call
///    passes `excluded_move = Some(parent_tt_move)`; this clause prevents the
///    verification frame's own SE block from firing (would yield one wasted
///    nested verification at parent_depth ≥ 17).
/// 2. `ply > 0` — root must search every move at full depth.
/// 3. `!is_pv` — start conservative; PV-SE deferred per ADR-0029 §8.
/// 4. `!in_check_flag` — check evasions already a hot spot; SE adds overhead.
/// 5. `depth >= SE_MIN_DEPTH` — minimum-depth guard. Via the compile-time
///    invariant `FFP_MAX_DEPTH < SE_MIN_DEPTH`, SE and FFP are depth-disjoint.
/// 6. `tt_bound == Lower` — Lower bound is the only sound starting case
///    (Exact deferred to a follow-up campaign per ADR-0029 §1).
/// 7. `tt_depth >= depth - SE_TT_DEPTH_DELTA` — TT entry fresh enough;
///    saturating subtraction avoids underflow at `depth = SE_MIN_DEPTH`.
/// 8. `tt_score.abs() < MATE_IN_MAX_PLY` — mate-score guard; outside this
///    band, `singular_beta` arithmetic is meaningless.
/// 9. `tt_move != 0` — non-sentinel TT move; `Move::default().bits() == 0` is
///    the no-move sentinel and is never produced by movegen.
///
/// Pure predicate — no `pos` access, no side effects. Each per-clause flip-mutant
/// is independently testable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn singular_extension_eligible(
    excluded_move: Option<Move>,
    ply: u32,
    is_pv: bool,
    in_check_flag: bool,
    depth: u32,
    tt_bound: TtBound,
    tt_depth: u8,
    tt_score: i32,
    tt_move: u16,
) -> bool {
    excluded_move.is_none()
        && ply > 0
        && !is_pv
        && !in_check_flag
        && depth >= SE_MIN_DEPTH
        && tt_bound == TtBound::Lower
        && (tt_depth as u32) >= depth.saturating_sub(SE_TT_DEPTH_DELTA)
        && tt_score.abs() < MATE_IN_MAX_PLY
        && tt_move != 0
}

/// True iff `side` has at least one non-pawn, non-king piece on the board.
/// NMP zugzwang guard (ADR-0023 §4): zugzwang is dominantly a K+P-ending
/// phenomenon, so the presence of any minor or major piece reduces
/// zugzwang risk to near-zero.
pub(crate) fn has_non_pawn_material(pos: &Position, side: Color) -> bool {
    let pieces = pos.pieces_colored(side, PieceKind::Knight)
        | pos.pieces_colored(side, PieceKind::Bishop)
        | pos.pieces_colored(side, PieceKind::Rook)
        | pos.pieces_colored(side, PieceKind::Queen);
    !pieces.is_empty()
}

/// Triangular PV table. Holds the best line found at each ply.
///
/// `moves[ply]` is a slot-array; `lengths[ply]` is how many moves are populated.
/// `update(ply, mv)` copies the child PV (`moves[ply+1][..lengths[ply+1]]`) into
/// `moves[ply][1..]` and prepends `mv`, giving the full PV down to the leaf.
/// `clear_ply(ply)` resets `lengths[ply] = 0` at the start of each negamax frame
/// so a no-improving-move return leaves the slot empty rather than stale.
struct PvTable {
    moves: [[Move; MAX_PLY]; MAX_PLY],
    lengths: [usize; MAX_PLY],
}

impl PvTable {
    fn new() -> Self {
        Self {
            moves: [[Move::default(); MAX_PLY]; MAX_PLY],
            lengths: [0; MAX_PLY],
        }
    }

    fn clear_ply(&mut self, ply: usize) {
        self.lengths[ply] = 0;
    }

    fn update(&mut self, ply: usize, mv: Move) {
        let child_len = self.lengths[ply + 1];
        self.moves[ply][0] = mv;
        for i in 0..child_len {
            self.moves[ply][i + 1] = self.moves[ply + 1][i];
        }
        self.lengths[ply] = 1 + child_len;
    }
}

/// Update whether the current node's best score has at least one full-depth
/// witness after considering a newly searched move.
fn best_is_full_depth_after_score(
    best: i32,
    best_is_full_depth: bool,
    score: i32,
    move_is_full_depth: bool,
) -> bool {
    if score > best {
        move_is_full_depth
    } else {
        best_is_full_depth || (score == best && move_is_full_depth)
    }
}

/// Snapshot of a TT probe result captured at negamax step 7 (M5.G — ADR-0029).
/// Used by the SE block at step 12.5 which needs the probe data post-step-12
/// (after move ordering). The `score` field is already ply-adjusted via
/// `score_from_tt` — `singular_beta(snapshot.score, depth)` is correct without
/// further adjustment.
struct TtProbeSnapshot {
    tt_move: u16,
    bound: TtBound,
    depth: u8,
    /// Ply-adjusted to the current frame via `score_from_tt(entry.score, ply)`.
    score: i32,
}

/// Fail-soft negamax alpha-beta search with triangular PV recovery.
///
/// Replaces depth-1 GreedyMover as the production search in M3.C.
/// Move ordering: MVV-LVA on captures + queen-promotion bonus; quiets in
/// movegen order. Repetition and 50-move draw detection via M3.B helpers
/// (at `ply > 0` only — root always picks a move). PV recovered via a
/// triangular table. Mate scores are ply-adjusted (see ADR-0016 §3).
pub(crate) struct AlphaBetaMover {
    pv: PvTable,
    /// Search-owned Zobrist trajectory. Cloned from `ctx.history` at go-start;
    /// push/popped around each recursive make/unmake.
    history: Vec<u64>,
    /// Running node counter for this search invocation.
    nodes: u64,
    /// M7.B.2: current iterative-deepening *root* iteration depth. Set at the
    /// top of each ID iteration (`for depth in 1..=max_depth`). Read by qsearch
    /// to compute the depth-conditioned SEE-prune threshold
    /// (`qs_see_prune_threshold`): aggressive (0) at shallow iterations, relaxed
    /// toward a negative margin at deep iterations. Defaults to `0` for any path
    /// that does not run the ID loop (`qsearch_for_test`, the eval `stm_score`
    /// helper) ⇒ threshold `0` ⇒ byte-identical to the flat-0 M7.B behavior.
    root_depth: u32,
    /// Set when the cancellation check fires; once set, every recursive return
    /// propagates 0 without committing the score.
    aborted: bool,
    /// Score of the last root move whose subtree completed fully (i.e. whose
    /// PV was committed). Updated in lockstep with `pv` at ply 0 so that if
    /// the search aborts mid-root the reported score reflects the best fully
    /// explored subtree rather than the aborted return value of 0.
    root_score: Option<i32>,
    /// M4.A: per-`go` TT handle. Cloned from `ctx.tt` at top of `Search::go`;
    /// `None` between searches and for searches that don't use a TT (tests
    /// exercising `negamax_for_test` directly without a TT context).
    tt: Option<Arc<TranspositionTable>>,
    /// M4.B: per-ply killer slots. `killers[ply][0]` is the most-recent
    /// quiet-move beta-cutoff at this ply; `killers[ply][1]` is the
    /// previous one. Sentinel = `Move::default()` (bits == 0). Cleared
    /// per-go and per-iteration; not persisted across iterations.
    killers: [[Move; 2]; MAX_PLY],
    /// M4.C: butterfly history table. Persists across ID iterations and across
    /// `go` invocations within a game. Cleared by `Search::reset()` (called
    /// from `Engine::reset_for_new_game()` on `ucinewgame` and per bench position).
    history_table: HistoryTable,
    /// M6.B: search-owned pawn hash (ADR-0032 §2). Fixed 4 MiB, always-
    /// replace, keyed by `Position::pawn_zobrist`. Cleared by
    /// `Search::reset()` (same `ucinewgame`/per-bench-position discipline as
    /// `history_table`). Read by `evaluate_cached` from qsearch (Slice E).
    pawn_hash: crate::eval::pawns::PawnHashTable,
    /// SPSA Unit 1: runtime-tunable adaptive aspiration parameters. Set by the
    /// engine via `set_aspiration_params` under the search lock (same
    /// `set_seed` worker-join discipline). Production default: `adaptive ==
    /// true`; set `adaptive = false` explicitly to restore the fixed-±50
    /// OFF path (byte-identical to the pre-Unit-1 baseline).
    aspiration_params: AspirationParams,
    /// M5.A: per-`go` count of NMP firings (number of times the null sub-search
    /// was attempted, regardless of cutoff). Test-only instrumentation for the
    /// stacked-null `negamax_passes_allow_null_false_in_null_subsearch` test;
    /// gated by `#[cfg(test)]` so production builds don't carry the field.
    #[cfg(test)]
    nmp_firings: u32,
    /// M5.B: per-`go` count of RFP cutoff firings (number of times
    /// `static_eval - margin >= beta` caused an early return). Test-only
    /// instrumentation for gate-skip / ordering tests; gated by `#[cfg(test)]`
    /// so production builds don't carry the field.
    #[cfg(test)]
    rfp_firings: u32,
    /// M5.C: test-only count of reduced-depth first-pass searches performed by
    /// LMR at the outermost `negamax_for_test` frame only.
    #[cfg(test)]
    lmr_reduced_searches: u32,
    /// M5.C: test-only count of full-depth re-searches triggered at the
    /// outermost `negamax_for_test` frame only.
    #[cfg(test)]
    lmr_full_researches: u32,
    /// M5.C: exact quiet moves that took the reduced-depth first pass at the
    /// outermost `negamax_for_test` frame only.
    #[cfg(test)]
    lmr_reduced_moves: Vec<Move>,
    /// M5.C: exact quiet moves that triggered a full-depth re-search at the
    /// outermost `negamax_for_test` frame only.
    #[cfg(test)]
    lmr_researched_moves: Vec<Move>,
    /// M5.C: quiet moves that entered `quiets_searched` at the outermost
    /// `negamax_for_test` frame only. Used to pin that reduced-only quiets do
    /// not get treated as fully searched priors for later history malus.
    #[cfg(test)]
    lmr_history_candidates: Vec<Move>,
    /// M5.C: root-ply marker for test-only LMR instrumentation. `Some(ply)`
    /// means "record only events that happen at this negamax frame".
    /// **Reused by M5.D**: gates `ffp_firings` / `ffp_skipped_moves` recording
    /// to the same `Some(ply)` frame so FFP and LMR instrumentation share
    /// one trace root.
    #[cfg(test)]
    lmr_trace_root_ply: Option<u32>,
    /// M5.D: per-`go` count of FFP per-move skips (number of times the
    /// FFP gate fired and a quiet was skipped without recursion). Test-only
    /// instrumentation; gated by `#[cfg(test)]` so production builds don't
    /// carry the field. Naming follows the M5.A `nmp_firings` /
    /// M5.B `rfp_firings` convention.
    #[cfg(test)]
    ffp_firings: u32,
    /// M5.D: exact quiet moves that took the FFP-skip path at the
    /// `lmr_trace_root_ply` frame.
    #[cfg(test)]
    ffp_skipped_moves: Vec<Move>,
    /// M5.E: per-`go` count of qsearch single-reply extensions (M5.E #1) —
    /// number of times qsearch recursed on the unique legal quiet move
    /// instead of returning stand-pat. Test-only instrumentation; gated by
    /// `#[cfg(test)]` so production builds don't carry the field. Naming
    /// follows the M5.A `nmp_firings` / M5.B `rfp_firings` / M5.D
    /// `ffp_firings` convention.
    #[cfg(test)]
    qsearch_single_reply_firings: u32,
    /// M5.E: per-`go` count of qsearch under-promo extensions (M5.E #3) —
    /// number of synthesized rook/bishop promo recursions fired when a
    /// queen-promo's post-make child position is stalemate.
    #[cfg(test)]
    qsearch_under_promo_firings: u32,
    /// M5.F: per-`go` count of TT probes attempted at qsearch entry (step
    /// 2.5). Incremented once per qsearch frame that reaches the probe block,
    /// regardless of whether the probe hits or misses. Test-only; gated by
    /// `#[cfg(test)]` so production builds don't carry the field. Naming
    /// follows the M5.A `nmp_firings` / M5.B `rfp_firings` / M5.D
    /// `ffp_firings` / M5.E `qsearch_*_firings` convention.
    #[cfg(test)]
    qsearch_tt_probes: u32,
    /// M5.F: per-`go` count of TT stores performed at qsearch return points
    /// (step 11 via `qsearch_store_and_return`). Incremented once per real-
    /// result return from qsearch that commits to the TT (not on MAX_PLY
    /// ceiling guard returns, not on abort). Test-only instrumentation.
    #[cfg(test)]
    qsearch_tt_stores: u32,
    /// M5.G: per-`go` count of confirmed singular extensions (verification
    /// searches that returned fail-low below `singular_beta`). Test-only
    /// instrumentation; gated by `#[cfg(test)]` so production builds don't
    /// carry the field. Naming follows the M5.A `nmp_firings` /
    /// M5.B `rfp_firings` / M5.D `ffp_firings` convention.
    #[cfg(test)]
    se_extensions: u32,
    /// M5.G: the effective search depth used for the TT move (i == 0) at the
    /// non-LMR `search_child` call site. Set to `Some(depth - 1 + move_extension)`
    /// the first time the TT move is dispatched through the non-LMR path.
    /// `None` if the TT move went through LMR or was never reached.
    /// Used by `negamax_se_extension_increments_child_depth_by_one` to confirm
    /// that `move_extension == 1` caused the child to receive `depth` instead of
    /// `depth - 1`. Reset to `None` at each `negamax_for_test` invocation.
    #[cfg(test)]
    se_tt_move_search_depth: Option<u32>,
    /// M7.B: per-`go` count of qsearch captures skipped by the SEE-pruning gate.
    /// Incremented each time the `!in_chk && is_capture && !is_promotion &&
    /// qsearch_see_pruneable` guard fires a `continue`. Test-only; gated by
    /// `#[cfg(test)]` so production builds don't carry the field. Naming follows
    /// the M5.A `nmp_firings` / M5.D `ffp_firings` / M5.E `qsearch_*_firings`
    /// convention.
    #[cfg(test)]
    qsearch_see_prune_firings: u32,
}

impl AlphaBetaMover {
    /// Create a new `AlphaBetaMover` with empty history and zeroed counters.
    pub(crate) fn new() -> Self {
        Self {
            pv: PvTable::new(),
            history: Vec::new(),
            nodes: 0,
            root_depth: 0,
            aborted: false,
            root_score: None,
            tt: None,
            killers: [[Move::default(); 2]; MAX_PLY],
            history_table: HistoryTable::new(),
            pawn_hash: crate::eval::pawns::PawnHashTable::new(),
            aspiration_params: AspirationParams::default(),
            #[cfg(test)]
            nmp_firings: 0,
            #[cfg(test)]
            rfp_firings: 0,
            #[cfg(test)]
            lmr_reduced_searches: 0,
            #[cfg(test)]
            lmr_full_researches: 0,
            #[cfg(test)]
            lmr_reduced_moves: Vec::new(),
            #[cfg(test)]
            lmr_researched_moves: Vec::new(),
            #[cfg(test)]
            lmr_history_candidates: Vec::new(),
            #[cfg(test)]
            lmr_trace_root_ply: None,
            #[cfg(test)]
            ffp_firings: 0,
            #[cfg(test)]
            ffp_skipped_moves: Vec::new(),
            #[cfg(test)]
            qsearch_single_reply_firings: 0,
            #[cfg(test)]
            qsearch_under_promo_firings: 0,
            #[cfg(test)]
            qsearch_tt_probes: 0,
            #[cfg(test)]
            qsearch_tt_stores: 0,
            #[cfg(test)]
            se_extensions: 0,
            #[cfg(test)]
            se_tt_move_search_depth: None,
            #[cfg(test)]
            qsearch_see_prune_firings: 0,
        }
    }
}

impl Search for AlphaBetaMover {
    fn set_aspiration_params(&mut self, params: AspirationParams) {
        self.aspiration_params = params;
    }

    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult {
        // ELOH.C: construct the worker-local SearchClock at the top of the
        // worker thread. CLOCK_THREAD_CPUTIME_ID is a per-thread counter, so
        // this read MUST happen on the worker. The orchestrator never reads
        // any clock — `ctx.caps` carries durations only.
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);

        // Per-go reset.
        self.history = ctx.history.clone();
        self.nodes = 0;
        self.aborted = false;
        self.root_score = None;
        #[cfg(test)]
        {
            self.nmp_firings = 0;
            self.rfp_firings = 0;
            self.lmr_reduced_searches = 0;
            self.lmr_full_researches = 0;
            self.lmr_reduced_moves.clear();
            self.lmr_researched_moves.clear();
            self.lmr_history_candidates.clear();
            self.lmr_trace_root_ply = None;
            self.ffp_firings = 0;
            self.ffp_skipped_moves.clear();
            self.qsearch_tt_probes = 0;
            self.qsearch_tt_stores = 0;
            self.se_extensions = 0;
        }
        // M4.A: install the TT and advance its generation once per `go` (ADR-0018 §9).
        self.tt = ctx.tt.clone();
        if let Some(tt) = &self.tt {
            tt.new_search();
        }
        for i in 0..MAX_PLY {
            self.pv.lengths[i] = 0;
        }
        clear_killers(&mut self.killers);

        let max_depth = max_depth_from_limits(&ctx.limits);
        let mut pos_clone = *position;

        // Last fully completed iteration's snapshot. None until iteration 1
        // finishes; preserved across mid-iteration aborts.
        let mut last_complete: Option<(u32, Option<Move>, i32)> = None;
        // score(d-2): the iteration-before-last completed score, for the
        // adaptive aspiration half-width. Shifted in lockstep with
        // `last_complete`, ONLY on iteration completion (preserved across
        // mid-iteration aborts). Harmless when `aspiration_params.adaptive ==
        // false` since the helper ignores it.
        let mut prev_prev_score: Option<i32> = None;

        for depth in 1..=max_depth {
            // M7.B.2: publish the current ID iteration depth so qsearch can
            // depth-condition its SEE-prune threshold (`qs_see_prune_threshold`).
            // Set before any negamax/qsearch call in this iteration.
            self.root_depth = depth;
            // Per-iteration reset (M4.B inter-iteration policy): clear the
            // killer table once at the top of each ID iteration. NOT cleared
            // between aspiration tries — research §12.7 + plan AS23: a failed
            // try's killer slots are valuable ordering hints for the re-search.
            // Other per-iteration state (aborted / root_score / pv.lengths)
            // moves down to the per-try reset inside the aspiration loop.
            clear_killers(&mut self.killers);

            // M4.D: aspiration loop. Up to two negamax calls per iteration
            // (first try + at most one re-search). The two-tier cap is
            // enforced by an explicit `tries >= 2` break AFTER the
            // window-contained check — see §3.3 of the plan.
            let prior_score = last_complete.map(|(_, _, s)| s);
            let (mut alpha, mut beta) =
                aspiration_window(prior_score, prev_prev_score, &self.aspiration_params, depth);

            // Score eventually accepted as this iteration's result. Assigned
            // on every loop pass before any read; mid-iteration abort breaks
            // out via the outer `if self.aborted` below without consulting it.
            let mut returned: i32;
            let mut tries: u32 = 0;
            loop {
                // Per-aspiration-try reset. `aborted` cleared so a prior
                // try's abort doesn't bleed; `root_score` cleared so the
                // lockstep is correct for this try; `pv.lengths[..]` cleared
                // so a prior try's stale state on plies > 0 doesn't make the
                // parent's pv.update copy stale (AS12). Killers NOT cleared
                // here — preserved across tries for ordering quality.
                self.aborted = false;
                self.root_score = None;
                for i in 0..MAX_PLY {
                    self.pv.lengths[i] = 0;
                }

                returned = self.negamax(
                    &mut pos_clone,
                    depth,
                    0,
                    alpha,
                    beta,
                    true,
                    true,
                    None,
                    ctx,
                    &clock,
                );

                if self.aborted {
                    break;
                }

                tries += 1;

                // Window-contained: first-try success (common case at good
                // ordering, ~70% per literature). No re-search.
                if returned > alpha && returned < beta {
                    break;
                }

                // Two-tier cap: at most one re-search. Lands AFTER the
                // window-contained check so a re-search whose new wider
                // window contains the score returns through the success path.
                if tries >= 2 {
                    break;
                }

                let (na, nb) = widen_after_fail(returned, alpha, beta);
                alpha = na;
                beta = nb;

                // Test-instrumentation hook (plan §3.8). Emitted only when a
                // re-search will actually run — i.e., after the widen, before
                // the next negamax call. AS19 / AS20 / AS22 parse this line.
                info_sink(&format!(
                    "info string aspiration_re_search depth={depth} alpha={alpha} beta={beta}"
                ));
            }

            if self.aborted {
                // Mid-iteration abort (in either try): discard partial
                // PV/score; preserve prior last_complete snapshot.
                break;
            }

            // Iteration completed (window-contained on first try OR re-search
            // accepted). M4.D: TT-fallback on empty PV handles the rare
            // edge case where a re-search at `(L, +INF)` finds true_value
            // exactly L and fails to update PV in fail-soft.
            let bestmove =
                extract_bestmove_or_tt_fallback(&self.pv, self.tt.as_deref(), pos_clone.zobrist());
            // Shift the two-iteration window: old last_complete → prev_prev_score,
            // then record this iteration as the new last_complete.
            prev_prev_score = last_complete.map(|(_, _, s)| s);
            last_complete = Some((depth, bestmove, returned));

            // Single SearchInstant::now() reused for both the elapsed-ms
            // field and the soft-cap check below — avoids a duplicate syscall
            // and keeps the two reads coherent.
            let now = SearchInstant::now(ctx.virtual_clock);
            let elapsed_ms = clock.elapsed_at(now).as_millis();
            let pv_str = if self.pv.lengths[0] == 0 {
                "0000".to_string()
            } else {
                self.pv.moves[0][..self.pv.lengths[0]]
                    .iter()
                    .map(|m| m.to_uci())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            info_sink(&format!(
                "info depth {depth} score {} nodes {} time {elapsed_ms} pv {pv_str}",
                score_to_uci(returned),
                self.nodes,
            ));

            if depth >= max_depth {
                break;
            }
            // Stop check between iterations is load-bearing: per-iteration
            // `aborted` is reset at the top of the loop, so a `stop` flipped
            // between iterations would otherwise be missed until the next
            // 4096-cadence poll inside negamax.
            if ctx.stop.load(Ordering::Relaxed) {
                break;
            }
            if clock.is_soft_reached_at(now) {
                break;
            }
        }

        debug_assert_eq!(
            pos_clone, *position,
            "negamax must restore position via balanced make/unmake"
        );

        // Pick final result. Prefer the last completed iteration's snapshot;
        // fall back via `aborted_fallback_result` to whatever the aborted
        // iteration left in `pv` (covers the pathological "iteration 1
        // aborted before any root move improved alpha" case where
        // `last_complete` is None). The fallback is extracted into a named
        // helper so its `pv.lengths[0] > 0` mutations are directly
        // unit-testable — the structural-undetectability gap that motivated
        // M3.D's `negate_window` extraction applies here too: the fallback
        // is unreachable at M3.E scope (iter-1 from any position visits <
        // 4096 nodes, below the cadence poll), so mutations on the inline
        // expression can't be killed by any chess fixture.
        let (final_depth, final_bestmove, final_score) = match last_complete {
            Some((d, bm, sc)) => (d, bm, sc),
            None => aborted_fallback_result(&self.pv, self.root_score),
        };

        // Honor infinite/movetime/ponder wait loop (unchanged from M3.D).
        let wait = ctx.limits.infinite || ctx.limits.movetime.is_some() || ctx.limits.ponder;
        if wait {
            while !clock.should_abort(&ctx.stop, ctx.limits.nodes, self.nodes) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        SearchResult {
            bestmove: final_bestmove,
            depth: final_depth,
            score_cp: Some(final_score),
            nodes: self.nodes,
            ponder: None,
        }
    }

    fn reset(&mut self) {
        self.history.clear(); // M3.B carry-forward: game-history Zobrist Vec.
        clear_killers(&mut self.killers); // M4.B: killer table.
        self.history_table.clear(); // M4.C: butterfly history table.
        self.pawn_hash.clear(); // M6.B: search-owned pawn hash (ADR-0032).
        // pv and nodes are reset per-go; TT lives in engine.
    }
}

impl AlphaBetaMover {
    /// Fail-soft negamax with alpha-beta pruning and triangular PV recovery.
    ///
    /// `is_pv` is the synthetic ordering predicate that gates TT cutoffs
    /// (ADR-0018 §11). `true` at the root and at the first child of a PV
    /// parent (recursion-order index 0); `false` everywhere else. A future PVS
    /// milestone (M8.A — *not* M4.D, which shipped aspiration windows) would
    /// replace it with the window-based `beta - alpha == 1` check.
    ///
    /// `allow_null` (M5.A) gates the NMP block at step 8. `true` from the
    /// top-level `Search::go` call and from the move-loop recursive call;
    /// `false` only in the NMP null-search recursive call (stacked-null
    /// prevention — ADR-0023 §5).
    ///
    /// Make `mv`, recurse `negamax` on the resulting child at `child_depth`,
    /// undo, and return the negated child score. Centralises the
    /// `make_move` / `history.push` / `negate_window` / `history.pop` /
    /// `unmake_move` plumbing so the move loop has one call site per
    /// recursion variant (M5.C reduced first pass, M5.C full re-search,
    /// non-LMR child) instead of three near-identical inline blocks.
    /// `allow_null = true` for every move-loop child (the
    /// stacked-NMP guard is the NMP block's responsibility, not the
    /// move-loop's). The returned score is `0` if the search aborted;
    /// callers MUST inspect `self.aborted` immediately after the call and
    /// propagate the abort up to their own caller (`if self.aborted {
    /// return 0; }`) before reading the returned score.
    #[allow(clippy::too_many_arguments)]
    fn search_child(
        &mut self,
        pos: &mut Position,
        mv: Move,
        child_depth: u32,
        ply: u32,
        alpha: i32,
        beta: i32,
        child_is_pv: bool,
        ctx: &SearchContext,
        clock: &SearchClock,
    ) -> i32 {
        let undo = pos.make_move(mv);
        self.history.push(pos.zobrist());
        let (child_alpha, child_beta) = negate_window(alpha, beta);
        let score = -self.negamax(
            pos,
            child_depth,
            ply + 1,
            child_alpha,
            child_beta,
            child_is_pv,
            true, // move-loop children always permit NMP at their own gate
            None, // excluded_move: children never inherit a parent's excluded move
            ctx,
            clock,
        );
        self.history.pop();
        pos.unmake_move(mv, undo);
        score
    }

    #[allow(clippy::too_many_arguments)]
    fn negamax(
        &mut self,
        pos: &mut Position,
        depth: u32,
        ply: u32,
        mut alpha: i32,
        mut beta: i32,
        is_pv: bool,
        allow_null: bool,
        excluded_move: Option<Move>,
        ctx: &SearchContext,
        clock: &SearchClock,
    ) -> i32 {
        use crate::movegen::in_check;

        // 1. Clear this ply's PV slot. Stays BEFORE the depth==0 delegation so
        //    even leaves reset their slot — otherwise a prior subtree at the
        //    same ply could leave lengths[ply] > 0, which qsearch wouldn't
        //    touch, and the parent's pv.update would copy stale child PV moves.
        self.pv.clear_ply(ply as usize);

        // 2. Horizon: delegate to qsearch BEFORE incrementing self.nodes.
        //    qsearch's own per-frame increment + cancellation poll covers the leaf.
        //    Preserves M3.C's "1 leaf = 1 node" budget under `go nodes <N>`.
        //    Qsearch does not consult the TT in M4.A (ADR-0018 §6).
        if depth == 0 {
            return self.qsearch(pos, alpha, beta, ply, ctx, clock);
        }

        // 3. Per-frame nodes increment + cancellation poll (non-leaf only).
        self.nodes += 1;
        if self.nodes & 4095 == 0 && clock.should_abort(&ctx.stop, ctx.limits.nodes, self.nodes) {
            self.aborted = true;
            return 0;
        }

        // 4. Capture caller's pre-MDP alpha BEFORE any mutation (load-bearing
        //    for bound classification at step 12). MDP can tighten alpha
        //    upward; classifying Exact vs Upper against the MDP-tightened
        //    alpha would mis-label fail-lows as Exact. ADR-0018 §13.
        let original_alpha = alpha;

        // 5. Repetition + 50-move draw checks — only at ply > 0 (root must
        //    pick a move). Runs BEFORE the TT probe (ADR-0018 §10): a stale
        //    TT score for the same key would otherwise mis-score a draw.
        if ply > 0 {
            if is_fifty_move_draw(pos.halfmove_clock()) {
                return 0;
            }
            if is_repetition(&self.history, pos.halfmove_clock()) {
                return 0;
            }
        }

        // 6. Mate-distance pruning (may tighten alpha/beta).
        let mating_value = MATE - ply as i32;
        if mating_value < beta {
            beta = mating_value;
            if alpha >= mating_value {
                return mating_value;
            }
        }
        let mated_value = -(MATE - ply as i32);
        if mated_value > alpha {
            alpha = mated_value;
            if beta <= mated_value {
                return mated_value;
            }
        }

        // 7. TT probe. Compares post-MDP `(alpha, beta)`; never returns
        //    early at PV nodes (ADR-0018 §11).
        //
        //    M5.G: the cutoff branch is gated on `excluded_move.is_none()`.
        //    At the verification frame (`excluded_move = Some(tt_move)`), the same
        //    TT entry that triggered SE at the parent has `bound=Lower` and
        //    `tt_score >= singular_beta = tt_score - depth`. The cutoff guard
        //    `tt_score >= beta_verif` reduces to `depth >= 0` (always true), so
        //    the verification frame would self-cut and return `tt_score` — never
        //    running the actual move loop, making SE a 0-Elo no-op. Suppressing
        //    the cutoff (but NOT the probe — `tt_move` is captured unconditionally
        //    so step-12 ordering works, though the excluded-move skip removes it
        //    anyway) forces the verification frame to run the full move loop.
        //    ADR-0029 §7.
        let mut tt_move: u16 = 0;
        let mut tt_probe_snapshot: Option<TtProbeSnapshot> = None;
        if let Some(tt) = &self.tt
            && let Some(entry) = tt.probe(pos.zobrist())
        {
            let tt_score = score_from_tt(entry.score as i32, ply as i32);
            tt_move = entry.best_move;
            tt_probe_snapshot = Some(TtProbeSnapshot {
                tt_move: entry.best_move,
                bound: entry.bound(),
                depth: entry.depth,
                score: tt_score,
            });
            if excluded_move.is_none() && !is_pv && entry.depth as u32 >= depth {
                match entry.bound() {
                    TtBound::Exact => return tt_score,
                    TtBound::Lower if tt_score >= beta => return tt_score,
                    TtBound::Upper if tt_score <= alpha => return tt_score,
                    _ => {}
                }
            }
        }

        // 8. Reverse futility pruning (M5.B — ADR-0024). Independent of NMP: own
        //    cheap-gate, own lazy `static_eval` read, own cutoff. M5.A's NMP block
        //    at step 9 below is byte-identical to its M5.A form (it has its own
        //    static_eval read inside its own gate; the duplicated read is a
        //    deliberate plan choice over a shared eager hoist — see plan §1).
        //    Order: RFP fires before NMP because it's cheaper (no sub-search).
        //    On the d=3..6 overlap, an RFP cutoff skips NMP's sub-search entirely.
        if ply > 0
            && !is_pv
            && !in_check(pos)
            && depth <= RFP_MAX_DEPTH
            && beta.abs() < MATE_IN_MAX_PLY
        {
            let stm = pos.side_to_move();
            let static_eval = if stm == Color::White {
                pos.static_eval_white()
            } else {
                -pos.static_eval_white()
            };
            let margin = reverse_futility_margin(depth);
            if static_eval - margin >= beta {
                #[cfg(test)]
                {
                    self.rfp_firings += 1;
                }
                // Fail-soft proved lower bound: even after discounting `margin*depth`
                // cp from `static_eval`, we still beat beta. No TT store (research
                // §6/§7): the proof is depth-specific to the margin, not a
                // search-quality bound.
                return static_eval - margin;
            }
        }

        // 9. Null-move pruning (M5.A — ADR-0023, unchanged from M5.A). Seven-condition
        //    gate: `ply > 0` first as a structural-root guard (defense-in-depth
        //    against a future PVS refactor that would change `is_pv`'s
        //    semantics); cheap predicates next; the static-eval read pulled
        //    inside the gate as a lazy late predicate so it fires only
        //    when the cheaper gates have already passed. On `null_score >=
        //    beta`, return a fail-high (mate-capped) and store a Lower
        //    bound at the current depth in the TT. The step-8 RFP block above
        //    has its own independent `static_eval` read (lazy-dup by design —
        //    ADR-0024); this NMP block's read is preserved verbatim.
        // Test-only stacked-null reachability witness (same `#[cfg(test)]`
        // instrumentation class as `nmp_firings`/`rfp_firings`; compiled out
        // of release). Counts `allow_null == false` nodes (NMP children) where
        // every *other* NMP gate holds — provably 0 under the zero-window
        // null search; the load-bearing assertion in
        // `negamax_passes_allow_null_false_in_null_subsearch`. ADR-0023 §5.
        #[cfg(test)]
        {
            if ply > 0 && !allow_null && !is_pv && depth >= NMP_MIN_DEPTH && !in_check(pos) {
                let stm = pos.side_to_move();
                if has_non_pawn_material(pos, stm) {
                    let se = if stm == Color::White {
                        pos.static_eval_white()
                    } else {
                        -pos.static_eval_white()
                    };
                    if se >= beta {
                        STACKED_NULL_GATE_REACHABLE
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
        if ply > 0 && allow_null && !is_pv && depth >= NMP_MIN_DEPTH && !in_check(pos) {
            let stm = pos.side_to_move();
            if has_non_pawn_material(pos, stm) {
                let static_eval = if stm == Color::White {
                    pos.static_eval_white()
                } else {
                    -pos.static_eval_white()
                };
                if static_eval >= beta {
                    #[cfg(test)]
                    {
                        self.nmp_firings += 1;
                    }
                    let r = null_move_reduction(depth);
                    let null_undo = pos.make_null_move();
                    self.history.push(pos.zobrist());
                    let null_score = -self.negamax(
                        pos,
                        depth - 1 - r,
                        ply + 1,
                        -beta,
                        -beta + 1,
                        false, // is_pv
                        false, // allow_null (stacked-null prevention — ADR-0023 §5)
                        None,  // excluded_move: NMP children never inherit excluded move
                        ctx,
                        clock,
                    );
                    self.history.pop();
                    pos.unmake_null_move(null_undo);

                    // Abort discipline matches the move loop: unmake before
                    // checking aborted, so the balanced-make/unmake debug-
                    // assert at the top of `Search::go` holds even on the
                    // abort path. (M3.E abort discipline; ADR-0023 §6.)
                    if self.aborted {
                        return 0;
                    }

                    if null_score >= beta {
                        // Mate-cap (ADR-0023 §6): NMP doesn't prove mate.
                        // A `null_score >= MATE_IN_MAX_PLY` means the
                        // opponent mates after we pass — dangerous, but
                        // not a real mate proof. Returning the mate
                        // magnitude would mis-rank in the parent search
                        // and propagate unsoundness via the TT.
                        let cutoff_score = if null_score >= MATE_IN_MAX_PLY {
                            beta
                        } else {
                            null_score
                        };
                        // TT store as Lower at the CURRENT depth (the
                        // cutoff proves a lower bound at this node, not at
                        // the reduced child — ADR-0018 §3 + §1; ADR-0023 §7).
                        // Stored score is the mate-CAPPED value; best_move
                        // = 0 (NMP didn't pick a move) is preserved-against-
                        // overwrite by ADR-0018 §7's rule.
                        // M5.G: suppressed at the verification frame — the
                        // NMP cutoff is for the verification's modified game
                        // (TT-move excluded), not for pos.zobrist(). ADR-0029 §7.
                        if excluded_move.is_none()
                            && let Some(tt) = &self.tt
                        {
                            let adjusted = score_to_tt(cutoff_score, ply as i32);
                            debug_assert!(
                                adjusted == adjusted as i16 as i32,
                                "TT score overflow on NMP store: adjusted={adjusted}"
                            );
                            tt.store(
                                pos.zobrist(),
                                TtData {
                                    score: adjusted as i16,
                                    depth: depth as u8,
                                    bound: TtBound::Lower,
                                    best_move: 0,
                                },
                            );
                        }
                        return cutoff_score;
                    }
                }
            }
        }

        // 10-12. Construct the stager. Eagerly generates, partitions, and sorts.
        //        searchmoves filter folded into new() — root-only.
        let killer0 = self.killers[ply as usize][0];
        let killer1 = self.killers[ply as usize][1];
        let searchmoves_filter = if ply == 0 {
            ctx.limits.searchmoves.as_deref()
        } else {
            None
        };
        let mut stager = MoveStager::new(
            pos,
            killer0,
            killer1,
            &self.history_table,
            tt_move,
            searchmoves_filter,
        );

        // 11. Terminal: no legal moves (post-filter).
        //     At ply==0 with searchmoves filter active, an empty list is a degenerate
        //     user input (all-illegal or empty filter). Short-circuit BEFORE the
        //     in_check triage — otherwise a check position with a degenerate filter
        //     would falsely return -MATE.
        if stager.is_empty() {
            if ply == 0 && ctx.limits.searchmoves.is_some() {
                return 0;
            }
            if in_check(pos) {
                return -(MATE - ply as i32);
            } else {
                return 0; // stalemate
            }
        }

        // 12.5  Singular-extension verification (M5.G — ADR-0029).
        //
        // Fire only when:
        //   - step 7 captured a probe snapshot (tt_probe_snapshot.is_some());
        //   - stager.peek().bits() == snapshot.tt_move (stager has a non-zero,
        //     legal TT move at the front; without this match, snapshot.tt_move is
        //     either 0 or a stale-but-illegal value and we don't fire);
        //   - singular_extension_eligible passes all 9 clauses (clause 9 is
        //     `tt_move != 0` for defense-in-depth).
        //
        // On verification fail-low (verif_score < singular_beta), set tt_move_extension = 1.
        // On verification fail-high or non-fire, tt_move_extension stays 0.
        //
        // `in_check(pos)` is read lazily — the cheap predicates (snapshot present,
        // stager non-empty post-step-11, `stager.peek()` matches the TT move) gate
        // the read so that frames where SE could not possibly fire don't pay the
        // in_check cost. Per ADR-0024 §6 / ADR-0026 §3 lazy-dup convention. The
        // full eligibility predicate is then called once with the computed flag; SE
        // fires only at depth ≥ SE_MIN_DEPTH = 6 where the overhead of one extra
        // in_check call is negligible.

        let mut tt_move_extension: u32 = 0;
        if let Some(snapshot) = &tt_probe_snapshot
            && stager.peek().map_or(0, Move::bits) == snapshot.tt_move
            && singular_extension_eligible(
                excluded_move,
                ply,
                is_pv,
                in_check(pos),
                depth,
                snapshot.bound,
                snapshot.depth,
                snapshot.score,
                snapshot.tt_move,
            )
        {
            let s_beta = singular_beta(snapshot.score, depth);
            let v_depth = verification_depth(depth);

            let verif_score = self.negamax(
                pos,
                v_depth,
                ply,
                s_beta - 1,
                s_beta,
                false, // is_pv (verification is non-PV by design)
                true,  // allow_null (research §6.1: allowed inside verif)
                Some(
                    stager
                        .peek()
                        .expect("peek post-non-empty post-gate-pass cannot be None"),
                ), // exclude the TT move
                ctx,
                clock,
            );
            if self.aborted {
                return 0;
            }

            if verif_score < s_beta {
                tt_move_extension = 1;
                #[cfg(test)]
                {
                    self.se_extensions += 1;
                }
            }
        }

        // 13. Recurse fail-soft. `child_is_pv = is_pv && i == 0` per ADR-0018 §11
        //     where `i` is the recursion-order index (post-step-12 reorder).
        let mut best = -INF;
        let mut best_is_full_depth = false;
        let mut cutoff_move: Option<Move> = None;
        // M4.C: quiets that complete recursion without cutting; used for malus on cutoff.
        let mut quiets_searched: MoveList = MoveList::new();
        let lmr_node_eligible = ply > 0 && !is_pv && depth >= LMR_MIN_DEPTH && !in_check(pos);

        // M5.D — FFP node-level gate. `ffp_static_eval == Some(_)` IS the
        // eligibility predicate; the per-quiet branch matches on
        // `Some(static_eval)` and skips otherwise. No separate
        // `ffp_node_eligible: bool` to keep in sync.
        //
        // Lazy-dup `static_eval` (and lazy-dup `in_check(pos)`) per ADR-0024
        // §6 / ADR-0026 §3: this block reads its own `static_eval`
        // independently of NMP (step 9) and RFP (step 8), so the M5.A/B
        // reads remain byte-identical and the M5.D SPRT signal is
        // attributable to FFP alone. At v1 constants, FFP (depth ≤ 2) and
        // LMR (depth ≥ 3) cannot fire at the same node (compile-time
        // invariant `FFP_MAX_DEPTH < LMR_MIN_DEPTH`), so any duplicated
        // `in_check` evaluation is structural only — never per-frame.
        let ffp_static_eval: Option<i32> = if ply > 0
            && !is_pv
            && (1..=FFP_MAX_DEPTH).contains(&depth)
            && alpha.abs() < MATE_IN_MAX_PLY
            && !in_check(pos)
        {
            let stm = pos.side_to_move();
            Some(if stm == Color::White {
                pos.static_eval_white()
            } else {
                -pos.static_eval_white()
            })
        } else {
            None
        };

        let mut quiet_index: u32 = 0;
        let mut i: usize = 0;
        while let Some(mv) = stager.next() {
            let cur_i = i;
            i += 1;
            // M5.G: skip the excluded move at the verification frame. This skip
            // fires BEFORE `child_is_pv`, `quiet_move`, and `quiet_index` so that
            // `quiet_index` semantics (ADR-0025 §3) are preserved — skipped moves
            // are not counted as "considered for reduction."
            if Some(mv) == excluded_move {
                continue;
            }

            // M5.G: per-iteration extension. Only the TT move (cur_i == 0) is
            // eligible for singular extension; all other moves are searched at
            // depth - 1. The SE block above computes `tt_move_extension` (1 when
            // singular, 0 otherwise); it is zero for non-SE nodes and non-TT moves.
            let move_extension: u32 = if cur_i == 0 { tt_move_extension } else { 0 };

            let child_is_pv = is_pv && cur_i == 0;
            let quiet_move = is_quiet(mv);
            let mut move_is_full_depth = true;

            // M5.D — FFP per-move skip (lands BEFORE LMR; ADR-0026 §6).
            //   - per-quiet, non-capture, non-promo (is_quiet excludes those)
            //   - alpha is the node's caller-tightened alpha at this point
            //   - the pruned bound is the move's fail-soft proved upper
            //     bound, guaranteed `<= alpha` by the gate; never improves
            //     alpha; never causes a beta cutoff — only floors `best`
            //   - the contribution is routed through
            //     `best_is_full_depth_after_score` with `move_is_full_depth
            //     = false` so the flag is downgraded if the FFP bound
            //     overwrites a previously full-depth-witnessed `best`
            //     (load-bearing for TT-store correctness — ADR-0026 §7)
            //   - FFP-skipped quiets do NOT advance `quiet_index` (semantic:
            //     `quiet_index` is the considered-for-reduction ordinal,
            //     not seen-in-ordering — ADR-0025 §3 / ADR-0026 §6) and do
            //     NOT enter `quiets_searched` (no recursive evidence —
            //     ADR-0026 §8)
            if quiet_move
                && let Some(static_eval) = ffp_static_eval
                && let Some(pruned_bound) = ffp_pruned_bound(static_eval, depth, alpha)
            {
                #[cfg(test)]
                if self.lmr_trace_root_ply == Some(ply) {
                    self.ffp_firings += 1;
                    self.ffp_skipped_moves.push(mv);
                }

                best_is_full_depth = best_is_full_depth_after_score(
                    best,
                    best_is_full_depth,
                    pruned_bound,
                    /* move_is_full_depth = */ false,
                );
                // `pruned_bound` is `<= alpha` by FFP's gate; floors `best`
                // without ever improving alpha or causing a beta cutoff.
                // Use `i32::max` (not a `>` conditional) so the only
                // operator at this site is mutation-undetectable as
                // equivalent (the conditional form's `> → >=` mutant is
                // observationally identical when `pruned_bound == best`).
                best = best.max(pruned_bound);
                continue;
            }

            // Decide whether this move takes the LMR path (reduced first
            // pass + conditional full re-search) or the plain full-depth
            // path. Only quiets at LMR-eligible nodes that pass the per-quiet
            // skip policy reduce; everything else searches at `depth - 1`.
            let lmr_reduction = if quiet_move {
                quiet_index += 1;
                if lmr_node_eligible
                    && is_lmr_eligible_quiet(
                        mv,
                        pos,
                        quiet_index,
                        killer0,
                        killer1,
                        &self.history_table,
                    )
                {
                    let r = late_move_reduction(depth, quiet_index);
                    debug_assert!(
                        r <= depth.saturating_sub(2),
                        "LMR reduction must clamp to depth-2; depth={depth} reduction={r}"
                    );
                    // Guard against future tunings of LMR_BASE_OFFSET / LMR_LOG_DIVISOR
                    // (plan §8 row 1) that could push the formula's floor down to 0.
                    // A `Some(0)` reduction would search the child at full `depth - 1`
                    // first, then re-search at the same `depth - 1` on `> alpha` —
                    // doubled work for any alpha-improving move, no correctness gain.
                    // Skip the LMR path entirely when the formula yields 0.
                    if r == 0 { None } else { Some(r) }
                } else {
                    None
                }
            } else {
                None
            };

            let score = if let Some(reduction) = lmr_reduction {
                #[cfg(test)]
                if self.lmr_trace_root_ply == Some(ply) {
                    self.lmr_reduced_searches += 1;
                    self.lmr_reduced_moves.push(mv);
                }

                let reduced_score = self.search_child(
                    pos,
                    mv,
                    depth - 1 - reduction,
                    ply,
                    alpha,
                    beta,
                    child_is_pv,
                    ctx,
                    clock,
                );
                if self.aborted {
                    return 0;
                }

                if lmr_needs_full_research(reduced_score, alpha) {
                    #[cfg(test)]
                    if self.lmr_trace_root_ply == Some(ply) {
                        self.lmr_full_researches += 1;
                        self.lmr_researched_moves.push(mv);
                    }

                    let full_score = self.search_child(
                        pos,
                        mv,
                        depth - 1,
                        ply,
                        alpha,
                        beta,
                        child_is_pv,
                        ctx,
                        clock,
                    );
                    if self.aborted {
                        return 0;
                    }
                    full_score
                } else {
                    move_is_full_depth = false;
                    reduced_score
                }
            } else {
                // M5.G: the TT move (cur_i == 0) may be extended by 1 ply when
                // singular_extension_eligible passed and verification returned
                // fail-low. All other moves use `move_extension = 0` (plain
                // `depth - 1`). The compile-time invariant `FFP_MAX_DEPTH <
                // SE_MIN_DEPTH` guarantees SE and FFP never co-fire at the same
                // node; the `cur_i == 0` predicate guarantees SE and LMR (which
                // requires `quiet_index >= 2`) never co-fire on the same move.
                #[cfg(test)]
                if cur_i == 0 && move_extension > 0 && self.se_tt_move_search_depth.is_none() {
                    // Record the effective depth only when SE extension actually applies
                    // (move_extension > 0). This isolates the singular-extension depth
                    // increment from normal i==0 dispatches in recursive sub-calls.
                    self.se_tt_move_search_depth = Some(depth - 1 + move_extension);
                }
                self.search_child(
                    pos,
                    mv,
                    depth - 1 + move_extension,
                    ply,
                    alpha,
                    beta,
                    child_is_pv,
                    ctx,
                    clock,
                )
            };

            // Abort check: score from an aborted search is invalid.
            if self.aborted {
                return 0;
            }

            best_is_full_depth =
                best_is_full_depth_after_score(best, best_is_full_depth, score, move_is_full_depth);

            if score > best {
                best = score;
                if score > alpha {
                    alpha = score;
                    // PV update: this move improved alpha at this ply.
                    self.pv.update(ply as usize, mv);
                    // Track the root score in lockstep with the PV so that if
                    // the search aborts later, `go` can report the score of the
                    // last fully-explored root subtree instead of the 0 sentinel.
                    if ply == 0 {
                        self.root_score = Some(score);
                    }
                }
                if alpha >= beta {
                    cutoff_move = Some(mv);
                    // M4.B + M4.C: on quiet beta-cutoff, update killers + apply
                    // history bonus to cutter and malus to all priors.
                    if is_quiet(mv) {
                        update_killers(&mut self.killers, ply as usize, mv);
                        // SIDE-TO-MOVE INVARIANT: pos has been restored to the
                        // pre-move-loop state by `pos.unmake_move(mv, undo)` above,
                        // so `pos.side_to_move()` here is the **mover's color**.
                        // Read here, BEFORE any future make/unmake; do NOT recompute
                        // inside the malus loop. Pinned by HS8 (root White) + HS8b
                        // (non-root Black).
                        let side = pos.side_to_move();
                        update_history_on_quiet_cutoff(
                            &mut self.history_table,
                            side,
                            mv,
                            &quiets_searched,
                            depth,
                        );
                    }
                    break; // beta cutoff — fail-soft: return `best`, not `beta`
                }
            }

            // Did not cut. If quiet, record for potential malus by a later cutter.
            if is_quiet(mv) && move_is_full_depth {
                debug_assert!(
                    mv.from_square() != mv.to_square(),
                    "Move::default() sentinel must never enter quiets_searched"
                );
                #[cfg(test)]
                {
                    if self.lmr_trace_root_ply == Some(ply) {
                        self.lmr_history_candidates.push(mv);
                    }
                }
                quiets_searched.push(mv);
            }
        }

        // 14. Store on completion. Skip on abort (partial bounds are not real)
        //     and never mid-loop (the abort path returns above without storing).
        //     Together this guarantees aborted iterations never overwrite a
        //     prior iteration's entry. Bound classification compares against
        //     `original_alpha` — the caller's pre-MDP alpha (step 4).
        //
        //     M5.G: suppressed at the verification frame (`excluded_move.is_some()`).
        //     The verification ran with a candidate move excluded; the score it
        //     computes is for a sub-game that is not the position keyed by
        //     `pos.zobrist()`. ADR-0029 §7.
        if let Some(tt) = &self.tt
            && !self.aborted
            && excluded_move.is_none()
            && let Some(bound) =
                tt_bound_for_completed_node(best, beta, original_alpha, best_is_full_depth)
        {
            let best_move_packed: u16 = match bound {
                TtBound::Lower => cutoff_move.map(|m| m.bits()).unwrap_or(0),
                TtBound::Exact if self.pv.lengths[ply as usize] > 0 => {
                    self.pv.moves[ply as usize][0].bits()
                }
                _ => 0,
            };
            let adjusted = score_to_tt(best, ply as i32);
            debug_assert!(
                adjusted == adjusted as i16 as i32,
                "TT score overflow: adjusted={adjusted}"
            );
            tt.store(
                pos.zobrist(),
                TtData {
                    score: adjusted as i16,
                    depth: depth as u8,
                    bound,
                    best_move: best_move_packed,
                },
            );
        }

        best
    }

    /// Test-only entry point that forwards verbatim to `negamax`. Exists
    /// because tests cannot call private methods directly; production code
    /// never calls this.
    ///
    /// `allow_null` (M5.A) gates the NMP block. Tests that don't care about
    /// NMP behavior pass `true`; NMP-behavior tests in §5.3 control it
    /// explicitly to drive gate-pass / gate-skip sister fixtures.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn negamax_for_test(
        &mut self,
        pos: &mut Position,
        depth: u32,
        ply: u32,
        alpha: i32,
        beta: i32,
        is_pv: bool,
        allow_null: bool,
        excluded_move: Option<Move>,
        ctx: &SearchContext,
    ) -> i32 {
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
        self.lmr_reduced_searches = 0;
        self.lmr_full_researches = 0;
        self.lmr_reduced_moves.clear();
        self.lmr_researched_moves.clear();
        self.lmr_history_candidates.clear();
        self.ffp_firings = 0;
        self.ffp_skipped_moves.clear();
        // M5.E: clear qsearch counters here too — negamax delegates to
        // qsearch at depth 0, so a negamax-driven test that touches qsearch
        // transitively must start with cleared counters. Symmetric reset
        // across both test entry points keeps back-to-back invocations
        // independent regardless of which entry triggered the prior call.
        self.qsearch_single_reply_firings = 0;
        self.qsearch_under_promo_firings = 0;
        // M5.F: reset TT probe/store counters symmetrically.
        self.qsearch_tt_probes = 0;
        self.qsearch_tt_stores = 0;
        // M5.G: reset SE extensions counter and depth-recording field.
        self.se_extensions = 0;
        self.se_tt_move_search_depth = None;
        // M7.B: reset SEE-prune firing counter symmetrically.
        self.qsearch_see_prune_firings = 0;
        // M7.B.2: reset root_depth so the depth-0 qsearch delegation uses the
        // flat-0 threshold (≡ M7.B) on a reused mover instance.
        self.root_depth = 0;
        self.lmr_trace_root_ply = Some(ply);
        let score = self.negamax(
            pos,
            depth,
            ply,
            alpha,
            beta,
            is_pv,
            allow_null,
            excluded_move,
            ctx,
            &clock,
        );
        self.lmr_trace_root_ply = None;
        score
    }

    /// Test-only accessor for the per-`go` NMP firings counter (M5.A).
    /// Mirrors `killers_for_test`. Production code never reads the counter
    /// through this accessor.
    #[cfg(test)]
    pub(super) fn nmp_firings_for_test(&self) -> u32 {
        self.nmp_firings
    }

    /// Test-only accessor for the per-`go` RFP firings counter (M5.B).
    /// Mirrors `nmp_firings_for_test`. Production code never reads the counter
    /// through this accessor.
    #[cfg(test)]
    pub(super) fn rfp_firings_for_test(&self) -> u32 {
        self.rfp_firings
    }

    /// Test-only accessor for the M5.C reduced-first-pass counter.
    #[cfg(test)]
    pub(super) fn lmr_reduced_searches_for_test(&self) -> u32 {
        self.lmr_reduced_searches
    }

    /// Test-only accessor for the M5.C full-depth re-search counter.
    #[cfg(test)]
    pub(super) fn lmr_full_researches_for_test(&self) -> u32 {
        self.lmr_full_researches
    }

    /// Test-only accessor for the exact quiets that took the reduced-depth
    /// first pass in M5.C.
    #[cfg(test)]
    pub(super) fn lmr_reduced_moves_for_test(&self) -> &[Move] {
        &self.lmr_reduced_moves
    }

    /// Test-only accessor for the exact quiets that were re-searched at full
    /// depth in M5.C.
    #[cfg(test)]
    pub(super) fn lmr_researched_moves_for_test(&self) -> &[Move] {
        &self.lmr_researched_moves
    }

    /// Test-only accessor for quiets that entered `quiets_searched` at the
    /// traced negamax frame.
    #[cfg(test)]
    pub(super) fn lmr_history_candidates_for_test(&self) -> &[Move] {
        &self.lmr_history_candidates
    }

    /// Test-only accessor for the per-`go` FFP firings counter (M5.D).
    /// Mirrors `nmp_firings_for_test` / `rfp_firings_for_test`.
    #[cfg(test)]
    pub(super) fn ffp_firings_for_test(&self) -> u32 {
        self.ffp_firings
    }

    /// Test-only accessor for the exact quiets that took the FFP-skip path
    /// at the traced negamax frame (M5.D).
    #[cfg(test)]
    pub(super) fn ffp_skipped_moves_for_test(&self) -> &[Move] {
        &self.ffp_skipped_moves
    }

    /// Test-only accessor for the per-`go` SE extensions counter (M5.G).
    /// Mirrors `nmp_firings_for_test` / `rfp_firings_for_test` /
    /// `ffp_firings_for_test`. Production code never reads the counter through
    /// this accessor.
    #[cfg(test)]
    pub(super) fn se_extensions_for_test(&self) -> u32 {
        self.se_extensions
    }

    /// Test-only accessor for the effective depth used when searching the TT
    /// move through the non-LMR path (M5.G). `Some(d)` means the TT move was
    /// dispatched at depth `d`; `None` means either LMR consumed i==0 or the
    /// TT move was never dispatched (e.g., excluded, or the position had no
    /// legal moves). Reset to `None` at each `negamax_for_test` invocation.
    #[cfg(test)]
    pub(super) fn se_tt_move_search_depth_for_test(&self) -> Option<u32> {
        self.se_tt_move_search_depth
    }

    /// Test-only setter to install a TT directly without going through
    /// `Search::go`. Production code never sets this — the per-`go` install
    /// happens at the top of `Search::go`.
    #[cfg(test)]
    pub(super) fn set_tt_for_test(&mut self, tt: Option<Arc<TranspositionTable>>) {
        self.tt = tt;
    }

    /// Test-only setter for the aborted flag. Pre-setting `aborted = true`
    /// before calling `negamax_for_test` exercises the abort-propagation path
    /// without relying on node-count boundaries: the first move-loop
    /// `if self.aborted { return 0; }` check fires immediately, simulating
    /// an abort that arrived from a sibling subtree. `negamax_for_test` does
    /// NOT reset `aborted`, so the pre-set value persists into the search.
    #[cfg(test)]
    pub(super) fn set_aborted_for_test(&mut self, aborted: bool) {
        self.aborted = aborted;
    }

    /// Test-only: return a reference to the killer table. Mirrors
    /// `pv_root_for_test` and `set_tt_for_test`. Production code never
    /// reads the killer table through this accessor.
    #[cfg(test)]
    pub(super) fn killers_for_test(&self) -> &[[Move; 2]; MAX_PLY] {
        &self.killers
    }

    /// Test-only: overwrite the killer table wholesale. Used by S26's
    /// run-(b) pre-population and S29's pre-populate step.
    #[cfg(test)]
    pub(super) fn set_killers_for_test(&mut self, killers: [[Move; 2]; MAX_PLY]) {
        self.killers = killers;
    }

    /// Test-only: return the root PV as a `Vec<Move>`.
    ///
    /// Returns `self.pv.moves[0][..self.pv.lengths[0]]` as an owned vector so
    /// proptest can iterate over consecutive pairs without holding a reference
    /// into the mover. Production code never calls this.
    #[cfg(test)]
    pub(super) fn pv_root_for_test(&self) -> Vec<Move> {
        self.pv.moves[0][..self.pv.lengths[0]].to_vec()
    }

    /// HS9-only test accessor: returns the comparator-sorted move list for
    /// `pos` against `self.history_table` and the *current* killer slots at
    /// ply 0 (typically empty in HS9's pre-search setup), **without** the
    /// post-sort TT-move-first bubble that production negamax step-10
    /// applies. To test the full step-10 pipeline (including TT-bubble), use
    /// `negamax_for_test` and read PV[0] post-search. Production code never
    /// calls this.
    #[cfg(test)]
    pub(super) fn ordered_moves_for_test(&self, pos: &Position) -> Vec<Move> {
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves_vec: Vec<Move> = ml.iter().collect();
        let killer0 = self.killers[0][0];
        let killer1 = self.killers[0][1];
        moves_vec.sort_by_cached_key(|&m| {
            -negamax_move_order_score(m, pos, killer0, killer1, &self.history_table)
        });
        moves_vec
    }

    /// Test-only accessor for the history table. Used by E_h
    /// (engine-level ucinewgame-clears-history-table test) to inspect
    /// the table after `Search::reset()` runs.
    #[cfg(test)]
    pub(crate) fn history_table_for_test(&self) -> &HistoryTable {
        &self.history_table
    }

    /// Test-only mutable accessor for the history table. Used by E_h to
    /// pre-seed entries before driving `reset_for_new_game()`. Production
    /// code never calls this — the search worker mutates the table only
    /// during `go`.
    #[cfg(test)]
    pub(crate) fn history_table_for_test_mut(&mut self) -> &mut HistoryTable {
        &mut self.history_table
    }

    /// Test-only accessor for the M6.B search-owned pawn hash. Used by the
    /// Slice-E wiring tests to observe `Search::reset`'s clear. Production
    /// reads the table only via `evaluate_cached` from qsearch.
    #[cfg(test)]
    pub(crate) fn pawn_hash_for_test_mut(&mut self) -> &mut crate::eval::pawns::PawnHashTable {
        &mut self.pawn_hash
    }

    /// Quiescence search: extends the leaf evaluation with captures and queen
    /// promotions until the position is quiet, restoring tactical correctness
    /// past the negamax horizon. Called by negamax at `depth == 0`.
    ///
    /// Stand-pat (static eval as lower bound) is used when not in check.
    /// In check, all legal evasions are searched; no stand-pat (unsound in check).
    /// Captures + queen promotions only outside of check; full move list in check.
    fn qsearch(
        &mut self,
        pos: &mut Position,
        mut alpha: i32,
        mut beta: i32,
        ply: u32,
        ctx: &SearchContext,
        clock: &SearchClock,
    ) -> i32 {
        use crate::eval::evaluate_cached;
        use crate::movegen::{MoveList, generate_moves, in_check};

        // 1. Per-frame nodes increment + cancellation poll. Negamax's depth==0
        //    branch delegates to qsearch BEFORE its own increment, so this is the
        //    sole counter for the depth==0 leaf — preserves M3.C's "1 leaf = 1
        //    node" budget interpretation under `go nodes <N>`.
        self.nodes += 1;
        if self.nodes & 4095 == 0 && clock.should_abort(&ctx.stop, ctx.limits.nodes, self.nodes) {
            self.aborted = true;
            return 0;
        }

        // M5.E — defense-in-depth against pathological forced-quiet chains
        // under the single-reply extension (M5.E #1). Pre-M5.E qsearch
        // terminated naturally as captures ran out; #1 introduces all-quiet
        // recursion. Only the !in_check arm is guarded (see helper docs).
        // Helper extraction (vs inline `ply >= MAX_PLY - 1 && !in_check`)
        // is per the M3.D `negate_window` precedent: structurally trivial
        // checks inside `qsearch` are difficult for cargo-mutants to
        // discriminate from the fall-through behavior on existing test
        // fixtures (the fall-through path also returns stand-pat at the
        // !in_check ply ceiling), so extracting the predicate into a named
        // helper gives `cargo mutants --in-diff` a unique name to mutate
        // and a unit test surface to discriminate.
        if qsearch_short_circuit_at_ply_ceiling(ply, pos) {
            return evaluate_cached(pos, &mut self.pawn_hash);
        }

        // 2. Mate-distance pruning — may return mate-bound before stand-pat is
        //    computed. Safe because mate-distance pruning narrows toward provable
        //    mate scores only; if the window is already collapsed by a known mate,
        //    no static eval or capture sequence at this depth can improve on it.
        let mating_value = MATE - ply as i32;
        if mating_value < beta {
            beta = mating_value;
            if alpha >= mating_value {
                return mating_value;
            }
        }
        let mated_value = -(MATE - ply as i32);
        if mated_value > alpha {
            alpha = mated_value;
            if beta <= mated_value {
                return mated_value;
            }
        }

        // M5.F.1: snapshot the entry-window alpha (post-mate-distance-pruning,
        // before the stand-pat raise below) for the three-way completed-node
        // bound classifier. A completed loop with `alpha_entry < best < beta`
        // is Exact (see `qsearch_tt_bound_for_completed_node`).
        let alpha_entry = alpha;

        // 2.5. TT probe (M5.F — ADR-0028). No depth comparison: qsearch's
        //      notional depth is 0; any stored entry is at least as deep
        //      (negamax entries: depth ≥ 1; qsearch entries: depth = 0).
        //      Apply the standard fail-soft cutoff. Retain tt_move for ordering;
        //      filter is enforced implicitly at step 7's membership scan.
        let mut tt_move: u16 = 0;
        if let Some(tt) = &self.tt
            && let Some(entry) = tt.probe(pos.zobrist())
        {
            #[cfg(test)]
            {
                self.qsearch_tt_probes += 1;
            }
            let tt_score = score_from_tt(entry.score as i32, ply as i32);
            match entry.bound() {
                TtBound::Exact => return tt_score,
                TtBound::Lower if tt_score >= beta => return tt_score,
                TtBound::Upper if tt_score <= alpha => return tt_score,
                _ => {}
            }
            tt_move = entry.best_move;
        }

        // 3. In-check triage decides whether stand-pat is permitted and which
        //    moves are searched. Stand-pat-in-check is unsound (engine would
        //    "stand pat" while the king is being mated next ply — CPW).
        let in_chk = in_check(pos);

        // 4. Stand-pat lower-bound (only when NOT in check). Stand-pat =
        //    side-to-move's static eval.
        //     - >= beta: fail-soft beta cutoff, return stand-pat.
        //     - > alpha: tighten alpha (we'll see if any capture beats it).
        //    `best_init` is the seed for the move-loop's fail-soft accumulator
        //    `best`. On the in-check branch, initialized to `-INF` so any
        //    evasion's negated child score beats the seed and propagates up.
        let best_init = if !in_chk {
            let sp = evaluate_cached(pos, &mut self.pawn_hash);
            if sp >= beta {
                // Path A: stand-pat fail-high → Lower bound (M5.F).
                return self.qsearch_store_and_return(pos, sp, TtBound::Lower, 0, ply);
            }
            if sp > alpha {
                alpha = sp;
            }
            sp
        } else {
            // In check: stand-pat forbidden; seed `best` to `-INF` so any
            // evasion's negated child score beats the seed and propagates up.
            -INF
        };

        // 5. Generate moves. In check: ALL legal moves (evasions). Otherwise:
        //    captures + queen-promos only (filter from full list).
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves_vec: Vec<Move> = if in_chk {
            ml.iter().collect()
        } else {
            ml.iter().filter(|m| qsearch_move_filter(*m)).collect()
        };

        // 6. Terminal:
        //    - In check + empty: mate (-(MATE - ply)).
        //    - Not in check + empty: distinguish three sub-cases via the full
        //      legal-move list `ml` (true stalemate, single-reply extension,
        //      or M3.D false-stalemate guard).
        if moves_vec.is_empty() {
            if in_chk {
                // Path D: Mate at horizon — empty filtered means empty legal in the
                // in-check arm (full legal moves are evasions, no separate
                // filter). M5.F stores Exact (FIDE-definite terminal).
                let mate_score = -(MATE - ply as i32);
                return self.qsearch_store_and_return(pos, mate_score, TtBound::Exact, 0, ply);
            }

            // M5.E #1 + #2: distinguish the three not-in-check empty-filter
            // cases. `ml` is already populated in step 5; use the O(1)
            // `is_empty()` / `len()` accessors rather than `iter().count()`.

            if ml.is_empty() {
                // Path B: True stalemate (M5.E #2): zero legal moves and not in check
                // is a FIDE-9.2 draw. M5.F stores Exact (FIDE-definite terminal).
                return self.qsearch_store_and_return(pos, 0, TtBound::Exact, 0, ply);
            }

            if ml.len() == 1 {
                // Single-reply extension (M5.E #1): recurse on the unique
                // legal move. The unique move is necessarily a non-promo
                // quiet by movegen invariant: legal-direct movegen always
                // emits all four promotion variants whenever a promotion is
                // legal, so an under-promo can never appear alone. With the
                // qsearch filter rejecting both quiets and under-promos, an
                // empty filter + ml.len() == 1 leaves only Quiet | DoublePush
                // | KingCastle | QueenCastle as reachable flags. Pinned by
                // `qsearch_single_reply_under_promo_uniqueness_is_structurally_unreachable`.
                let only_mv = ml
                    .iter()
                    .next()
                    .expect("ml.len() == 1 → at least one move in iter");

                #[cfg(test)]
                {
                    self.qsearch_single_reply_firings += 1;
                }

                let undo = pos.make_move(only_mv);
                self.history.push(pos.zobrist());
                let (child_alpha, child_beta) = negate_window(alpha, beta);
                let score = -self.qsearch(pos, child_alpha, child_beta, ply + 1, ctx, clock);
                self.history.pop();
                pos.unmake_move(only_mv, undo);

                if self.aborted {
                    return 0;
                }

                // Path C: single-reply extension result. M5.F stores with
                // best_move = only_mv.bits() and bound from helper.
                let bound = qsearch_tt_bound_for_completed_node(score, alpha_entry, beta);
                return self.qsearch_store_and_return(pos, score, bound, only_mv.bits(), ply);
            }

            // Path E: 2+ legal moves: M3.D false-stalemate guard preserved.
            // M5.F stores Upper (stand_pat with no move searched).
            return self.qsearch_store_and_return(pos, best_init, TtBound::Upper, 0, ply);
        }

        // 7. Order: MVV-LVA descending; then TT-move-first if present.
        //    The TT move is promoted to index 0 iff it appears in moves_vec.
        //    The membership scan implicitly filters quiet TT moves at !in_chk
        //    positions (qsearch_move_filter already excluded them from moves_vec).
        moves_vec.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));
        if tt_move != 0
            && let Some(idx) = moves_vec.iter().position(|m| m.bits() == tt_move)
            && idx != 0
        {
            let mv = moves_vec.remove(idx);
            moves_vec.insert(0, mv);
        }

        // 8. Recurse fail-soft. Track cutoff_move for path F Lower best_move.
        let mut best = best_init;
        let mut cutoff_move: Option<Move> = None;
        // M7.B.2: depth-conditioned prune threshold for this frame (constant
        // across the move loop — `root_depth` is fixed for the ID iteration).
        let see_threshold = qs_see_prune_threshold(self.root_depth);
        for mv in moves_vec {
            // M7.B: qsearch SEE-pruning — skip statically-losing captures when not
            // in check. Promotions and (when in check) evasions are exempt by guard.
            // M7.B.2: `see_threshold` relaxes the prune at deep ID iterations.
            if !in_chk
                && mv.is_capture()
                && !mv.is_promotion()
                && qsearch_see_pruneable(pos, mv, see_threshold)
            {
                #[cfg(test)]
                {
                    self.qsearch_see_prune_firings += 1;
                }
                continue;
            }
            let undo = pos.make_move(mv);
            self.history.push(pos.zobrist());

            // M5.E #3: detect queen-promo stalemate BEFORE the recursion. The
            // detection scans the post-make position (child's POV) — at this
            // point `pos` is in the post-make state. Cheap: the `matches!`
            // gate skips the movegen for non-queen-promo moves. Captured
            // into a local so the predicate's lifetime crosses the recursion
            // / unmake window cleanly without splitting `history.pop()` from
            // `unmake_move`.
            let queen_promo_stalemates = matches!(
                mv.flag(),
                crate::mov::MoveFlag::QueenPromo | crate::mov::MoveFlag::QueenPromoCapture
            ) && {
                let mut child_ml = MoveList::new();
                generate_moves(pos, &mut child_ml);
                // True stalemate at the child: zero legal moves AND not in check.
                child_ml.is_empty() && !in_check(pos)
            };

            let (child_alpha, child_beta) = negate_window(alpha, beta);
            let score = -self.qsearch(pos, child_alpha, child_beta, ply + 1, ctx, clock);
            self.history.pop();
            pos.unmake_move(mv, undo);

            // 9. Abort propagation: discard score; do not commit. The push/pop
            //    above is balanced even on abort because the abort check runs
            //    AFTER both `history.pop()` and `unmake_move`.
            if self.aborted {
                return 0;
            }

            // 10a. Update best/alpha from the queen-promo (or other) score.
            if score > best {
                best = score;
                if score > alpha {
                    alpha = score;
                }
            }

            // 10b. M5.E #3: under-promo extension — runs BEFORE the cutoff
            //      check (10c) so a queen-promo's stalemate score (0) cannot
            //      suppress the under-promo recursions via `alpha >= beta`.
            //      Knight-promo deliberately not searched (out of M5.E scope).
            if queen_promo_stalemates {
                for under_mv_opt in stalemate_avoiding_under_promos(mv) {
                    let Some(under_mv) = under_mv_opt else {
                        continue;
                    };

                    #[cfg(test)]
                    {
                        self.qsearch_under_promo_firings += 1;
                    }

                    let undo2 = pos.make_move(under_mv);
                    self.history.push(pos.zobrist());
                    let (ca, cb) = negate_window(alpha, beta);
                    let under_score = -self.qsearch(pos, ca, cb, ply + 1, ctx, clock);
                    self.history.pop();
                    pos.unmake_move(under_mv, undo2);

                    if self.aborted {
                        return 0;
                    }

                    if under_score > best {
                        best = under_score;
                        if under_score > alpha {
                            alpha = under_score;
                        }
                    }

                    // Mid-under-promo cutoff: skip remaining under-promo
                    // variants if a rook/bishop promo's score already cut
                    // the node. Record the specific under-promo cutter
                    // (more precise than the outer queen-promo that stalemates).
                    if alpha >= beta {
                        cutoff_move = Some(under_mv);
                        break;
                    }
                }
            }

            // 10c. Per-move beta cutoff (post-under-promo). If the
            //      under-promo inner loop already set cutoff_move, preserve
            //      it (rook/bishop promo was the actual cutter; the outer mv
            //      is the queen-promo that stalemates — less specific).
            if alpha >= beta {
                if cutoff_move.is_none() {
                    cutoff_move = Some(mv);
                }
                break; // beta cutoff, fail-soft
            }
        }

        let bound = qsearch_tt_bound_for_completed_node(best, alpha_entry, beta);
        let best_move_packed = match bound {
            TtBound::Lower => cutoff_move.map(|m| m.bits()).unwrap_or(0),
            // Exact/Upper: no cutoff move recorded. (Exact entries store no PV
            // move for now — the bound itself is the M5.F.1 lever; ordering via
            // a stored Exact PV move is a possible future refinement.)
            _ => 0,
        };
        self.qsearch_store_and_return(pos, best, bound, best_move_packed, ply)
    }

    /// Store a qsearch result in the TT (if TT is set and not aborted), then
    /// return `score`. Centralises the abort guard, mate-score adjustment,
    /// depth-0 convention, and counter increment at all real-result return
    /// points (paths A–F in the §4.5 diagram). Paths X (MAX_PLY ceiling) and
    /// Y (abort) bypass this and return directly.
    fn qsearch_store_and_return(
        &mut self,
        pos: &Position,
        score: i32,
        bound: TtBound,
        best_move: u16,
        ply: u32,
    ) -> i32 {
        if let Some(tt) = &self.tt
            && !self.aborted
        {
            #[cfg(test)]
            {
                self.qsearch_tt_stores += 1;
            }
            let adjusted = score_to_tt(score, ply as i32);
            debug_assert!(
                adjusted == adjusted as i16 as i32,
                "TT score overflow on qsearch store: adjusted={adjusted}"
            );
            tt.store(
                pos.zobrist(),
                TtData {
                    score: adjusted as i16,
                    depth: 0,
                    bound,
                    best_move,
                },
            );
        }
        score
    }

    /// Test-only entry point that forwards verbatim to `qsearch`. Mirrors
    /// `negamax_for_test`. Production code never calls this.
    #[cfg(test)]
    pub(super) fn qsearch_for_test(
        &mut self,
        pos: &mut Position,
        alpha: i32,
        beta: i32,
        ply: u32,
        ctx: &SearchContext,
    ) -> i32 {
        // M5.E: reset qsearch counters at each entry so back-to-back
        // qsearch_for_test invocations get independent counter values.
        // Symmetric with negamax_for_test (which also resets these because
        // negamax delegates to qsearch at depth 0).
        self.qsearch_single_reply_firings = 0;
        self.qsearch_under_promo_firings = 0;
        // M5.F: reset TT probe/store counters per-entry for the same reason.
        self.qsearch_tt_probes = 0;
        self.qsearch_tt_stores = 0;
        // M7.B: reset SEE-prune firing counter per-entry for the same reason.
        self.qsearch_see_prune_firings = 0;
        // M7.B.2: reset root_depth so qsearch_for_test uses the flat-0
        // threshold (≡ M7.B) regardless of any prior `go`/depth-driving on a
        // reused mover instance. Makes the "default 0 ⇒ flat threshold"
        // guarantee code-backed, not invariant-backed.
        self.root_depth = 0;
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
        self.qsearch(pos, alpha, beta, ply, ctx, &clock)
    }

    /// Test-only variant of `qsearch_for_test` that drives the M7.B.2
    /// depth-conditioned SEE-prune ramp by publishing `root_depth` *after* the
    /// per-entry resets (so it is not clobbered). Exercises the live qsearch
    /// path's read of `self.root_depth` (which the pure-function value-table
    /// test cannot).
    #[cfg(test)]
    pub(super) fn qsearch_at_root_depth_for_test(
        &mut self,
        pos: &mut Position,
        alpha: i32,
        beta: i32,
        ply: u32,
        ctx: &SearchContext,
        root_depth: u32,
    ) -> i32 {
        self.qsearch_single_reply_firings = 0;
        self.qsearch_under_promo_firings = 0;
        self.qsearch_tt_probes = 0;
        self.qsearch_tt_stores = 0;
        self.qsearch_see_prune_firings = 0;
        self.root_depth = root_depth;
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
        self.qsearch(pos, alpha, beta, ply, ctx, &clock)
    }

    /// Test-only accessor for the per-`go` qsearch single-reply firings
    /// counter (M5.E #1). Mirrors `nmp_firings_for_test` /
    /// `rfp_firings_for_test` / `ffp_firings_for_test`.
    #[cfg(test)]
    pub(super) fn qsearch_single_reply_firings_for_test(&self) -> u32 {
        self.qsearch_single_reply_firings
    }

    /// Test-only accessor for the per-`go` qsearch under-promo firings
    /// counter (M5.E #3).
    #[cfg(test)]
    pub(super) fn qsearch_under_promo_firings_for_test(&self) -> u32 {
        self.qsearch_under_promo_firings
    }

    /// Test-only accessor for the per-`go` qsearch SEE-prune firing counter
    /// (M7.B). Incremented each time the `!in_chk && is_capture &&
    /// !is_promotion && qsearch_see_pruneable` gate fires a `continue`.
    /// Production code never reads this through the accessor.
    #[cfg(test)]
    pub(super) fn qsearch_see_prune_firings_for_test(&self) -> u32 {
        self.qsearch_see_prune_firings
    }

    /// Test-only accessor for the per-`go` qsearch TT probe counter (M5.F).
    /// Mirrors `nmp_firings_for_test` / `rfp_firings_for_test` /
    /// `ffp_firings_for_test` / `qsearch_single_reply_firings_for_test`.
    /// Production code never reads the counter through this accessor.
    #[cfg(test)]
    pub(super) fn qsearch_tt_probes_for_test(&self) -> u32 {
        self.qsearch_tt_probes
    }

    /// Test-only accessor for the per-`go` qsearch TT store counter (M5.F).
    /// Incremented at each real-result return point (not at MAX_PLY ceiling
    /// guard or abort returns). Production code never reads this.
    #[cfg(test)]
    pub(super) fn qsearch_tt_stores_for_test(&self) -> u32 {
        self.qsearch_tt_stores
    }
}

// ---------------------------------------------------------------------------
// M4.B + M4.C — Killer- and history-aware move-ordering helpers + score tiers.
// ---------------------------------------------------------------------------

/// Score offset added to every non-quiet move's `mvv_lva_score` in the
/// unified `negamax_move_order_score` comparator. Chosen so every
/// capture/promo/EP sorts strictly above both killer slots and every
/// history-rated quiet, regardless of how high the captures' raw MVV-LVA
/// values run. Pinned by `score_tier_invariants_compile` (compile-time
/// const-assert at the bottom of this section).
// Used by order_moves (test-only post-M5.H1) and the compile-time invariant.
#[allow(dead_code)]
const CAPTURE_OFFSET: i32 = 1_000_000;

/// Bonus score for the most-recent quiet beta-cutoff at this ply (slot 0).
/// Strictly above `KILLER1_SCORE` and `MAX_HISTORY`, and strictly below
/// `CAPTURE_OFFSET` so captures still sort above killers under the
/// post-M4.C unified scoring scheme. The previous M4.B value (`200`) was
/// chosen to fit between `0` and the smallest capture (QxP=287); restoring
/// `MAX_HISTORY = 16384` (literature standard for the history table; see
/// `src/history.rs`) required bumping the killer constants above
/// `MAX_HISTORY` and shifting captures above the killer band via
/// `CAPTURE_OFFSET`. The relative ordering invariants are unchanged from
/// M4.B; only the absolute-score scale shifted.
// Used by order_moves (test-only post-M5.H1) and the compile-time invariant.
#[allow(dead_code)]
const KILLER0_SCORE: i32 = 100_001;

/// Bonus score for the prior quiet beta-cutoff at this ply (slot 1).
/// Must satisfy `KILLER1_SCORE < KILLER0_SCORE` and
/// `KILLER1_SCORE > MAX_HISTORY` (so killers always rank above the best
/// history-rated quiet).
// Used by order_moves (test-only post-M5.H1) and the compile-time invariant.
#[allow(dead_code)]
const KILLER1_SCORE: i32 = 100_000;

/// Compile-time check that the score tiers are strictly ordered:
///
/// ```text
/// CAPTURE_OFFSET > KILLER0_SCORE > KILLER1_SCORE > MAX_HISTORY > -MAX_HISTORY
/// ```
///
/// This fixes the relative discipline between the four tunables in one
/// place. If any of them is changed in a way that violates the ordering,
/// the crate fails to compile rather than silently producing wrong
/// move-ordering decisions at runtime.
const _SCORE_TIER_INVARIANTS: () = {
    assert!(CAPTURE_OFFSET > KILLER0_SCORE);
    assert!(KILLER0_SCORE > KILLER1_SCORE);
    assert!(KILLER1_SCORE > MAX_HISTORY as i32);
    assert!((MAX_HISTORY as i32) > -(MAX_HISTORY as i32));
};

/// Returns true iff `mv` is a non-capture, non-promotion, non-en-passant
/// move (the moves eligible to populate the killer slots).
///
/// Direct mirror of the existing MVV-LVA "quiets sort below all captures"
/// arm in `mvv_lva_score`. Free function so `cargo mutants` reports any
/// flag-bit mutation directly against this function's name.
fn is_quiet(mv: Move) -> bool {
    use crate::mov::MoveFlag::*;
    matches!(mv.flag(), Quiet | DoublePush | KingCastle | QueenCastle)
}

/// Shift-on-distinct killer update.
///
/// `mv == killers[ply][0]` → no-op. Otherwise:
///     killers[ply][1] = killers[ply][0];
///     killers[ply][0] = mv;
///
/// Caller is responsible for the quiet-gate — `update_killers` doesn't
/// re-check `is_quiet(mv)`; it trusts the caller. Free function so the
/// shift is unit-testable in isolation (M3.D `negate_window` precedent).
fn update_killers(killers: &mut [[Move; 2]; MAX_PLY], ply: usize, mv: Move) {
    if killers[ply][0] != mv {
        killers[ply][1] = killers[ply][0];
        killers[ply][0] = mv;
    }
}

/// Apply the M4.C butterfly-history updates for a quiet beta-cutoff:
/// `+depth^2` to the cutter and `-depth^2` to each prior quiet searched at
/// the same node.
fn update_history_on_quiet_cutoff(
    history_table: &mut HistoryTable,
    side: Color,
    cutoff_move: Move,
    quiets_searched: &MoveList,
    depth: u32,
) {
    let bonus = (depth as i32) * (depth as i32);
    history_table.update(
        side,
        cutoff_move.from_square(),
        cutoff_move.to_square(),
        bonus,
    );
    for prior in quiets_searched.iter() {
        history_table.update(side, prior.from_square(), prior.to_square(), -bonus);
    }
}

/// Killer- and history-aware move-ordering score for negamax (NOT qsearch).
/// Wraps `mvv_lva_score`:
///   - non-quiet move → `mvv_lva_score(mv, pos) + CAPTURE_OFFSET`
///     (captures/promos sort above all killers and all history-rated quiets).
///   - quiet move matching `killer0` → `KILLER0_SCORE`.
///   - quiet move matching `killer1` (and not `killer0`) → `KILLER1_SCORE`.
///   - other quiet → `history_table.score(side, from, to) as i32`, in
///     `[-MAX_HISTORY, MAX_HISTORY]`.
///
/// Boundary discipline (post-M4.C):
///
/// ```text
/// captures > KILLER0_SCORE > KILLER1_SCORE > MAX_HISTORY > -MAX_HISTORY
/// ```
///
/// The four tunables are pinned by the `_SCORE_TIER_INVARIANTS`
/// compile-time const-assert above; runtime tests S23
/// (smallest-capture-above-killer0) and HS12 (capture above killer above
/// history-quiet at MAX_HISTORY) re-pin the discipline against drift.
///
/// Called by `MoveStager::new` (production), `order_moves` (test-only
/// post-M5.H1), and `ordered_moves_for_test`.
fn negamax_move_order_score(
    mv: Move,
    pos: &Position,
    killer0: Move,
    killer1: Move,
    history_table: &HistoryTable,
) -> i32 {
    if !is_quiet(mv) {
        return mvv_lva_score(mv, pos) + CAPTURE_OFFSET;
    }
    if mv == killer0 {
        KILLER0_SCORE
    } else if mv == killer1 {
        KILLER1_SCORE
    } else {
        history_table.score(pos.side_to_move(), mv.from_square(), mv.to_square()) as i32
    }
}

/// Apply M4.B's full move-ordering pass. Pure function; no side effects;
/// directly unit-testable on synthetic move lists.
///
/// Two-step:
///   1. Sort `moves_vec` descending by `negamax_move_order_score`.
///      Captures/promos rank above killers; killers above other quiets.
///   2. If `tt_move != 0` AND `tt_move` is found in the (now-sorted) list
///      AND it is not already at index 0, swap it to index 0.
///
/// **Killer legality discipline:** stale killers whose bits do not match
/// any legal move at this ply have no effect on ordering — the only path
/// by which a killer can move to the front is via `negamax_move_order_score`
/// returning `KILLER0_SCORE`, which requires `mv == killer0`. If no `mv`
/// matches the killer's bits, the score for every entry is its capture
/// value or 0, and the post-sort order is identical to the empty-killer
/// baseline. Pinned by S24f. Mirrors M4.A's TT-move legality scan
/// (ADR-0018 §12).
///
/// Demoted to `#[cfg(test)]` at M5.H1: the production call site now uses
/// `MoveStager::new` (plan §1.1). The 14+ test call sites in the H1-P1
/// proptest and S24/S24* fixtures still use this function as a reference.
#[cfg(test)]
#[allow(clippy::ptr_arg)]
fn order_moves(
    moves_vec: &mut Vec<Move>,
    pos: &Position,
    killer0: Move,
    killer1: Move,
    history_table: &HistoryTable,
    tt_move: u16,
) {
    moves_vec.sort_by_cached_key(|&m| {
        -negamax_move_order_score(m, pos, killer0, killer1, history_table)
    });
    if tt_move != 0
        && let Some(idx) = moves_vec.iter().position(|m| m.bits() == tt_move)
        && idx != 0
    {
        let mv = moves_vec.remove(idx);
        moves_vec.insert(0, mv);
    }
}

/// Zero the killer table. Single-statement body, but extracted as a
/// named free helper so all three call sites (per-go reset,
/// per-iteration reset, `Search::reset`) lower through the same line —
/// mutation testing on `[[Move::default(); 2]; MAX_PLY]` then targets
/// one helper instead of three indistinguishable inline literals.
fn clear_killers(killers: &mut [[Move; 2]; MAX_PLY]) {
    *killers = [[Move::default(); 2]; MAX_PLY];
}

/// M5.E #4 — qsearch ply-ceiling short-circuit predicate. Returns `true`
/// when qsearch should return `evaluate(pos)` immediately instead of
/// recursing further, as a defense-in-depth against pathological forced-
/// quiet chains under the M5.E #1 single-reply extension.
///
/// Only the `!in_check` arm is guarded. At in-check + ply ceiling the
/// existing evasion arm runs as before. Returning a fabricated mate-in-ply
/// score (`-(MATE - ply)`) would propagate a false mate through
/// `score_to_uci`, `score_to_tt`, and mate-distance pruning, and the
/// in-check ceiling case is structurally near-impossible (a chain of
/// forced check evasions tall enough to hit MAX_PLY does not occur in
/// real positions). ADR-0027 §4.
///
/// Extracted from the inline qsearch guard for mutation-coverage
/// discrimination per the M3.D `negate_window` precedent: at the only
/// callable ply value (`MAX_PLY - 1`) the qsearch fall-through path also
/// returns stand-pat on typical fixtures, so cargo-mutants cannot
/// distinguish the inline arithmetic. The helper's named function and
/// unit tests give mutation testing a discriminating surface.
#[inline]
pub(crate) fn qsearch_short_circuit_at_ply_ceiling(ply: u32, pos: &Position) -> bool {
    ply >= MAX_PLY as u32 - 1 && !crate::movegen::in_check(pos)
}

/// Returns true iff the move should be searched in qsearch (when not in check).
/// Captures, EnPassant, and queen-promo variants (with or without capture).
/// Excludes under-promotions and under-promo-captures (M3.D scope) and
/// non-capture checks (M4+).
fn qsearch_move_filter(mv: Move) -> bool {
    use crate::mov::MoveFlag::*;
    matches!(
        mv.flag(),
        Capture | EnPassant | QueenPromo | QueenPromoCapture
    )
}

// ---------------------------------------------------------------------------
// M7.B — qsearch SEE-pruning
// ---------------------------------------------------------------------------

/// M7.B.2 — depth-conditioned qsearch SEE-prune ramp parameters
/// (`docs/plans/m7.b.2.md`). The threshold is flat `0` (prune every `see < 0`
/// capture, ≡ flat-0 M7.B) at/below `D0`, then ramps linearly more-negative by
/// `SLOPE` cp per ply, clamped at `FLOOR`. Rationale: a `see < 0` capture can be
/// a real tactical sacrifice the engine *would* refute given depth; at shallow
/// (fast-TC) iterations the node saving wins, but at deep (slow-TC) iterations
/// pruning it costs accuracy — so relax the prune only at depth.
const QS_SEE_RAMP_D0: u32 = 12;
const QS_SEE_RAMP_SLOPE: i32 = 16;
const QS_SEE_RAMP_FLOOR: i32 = -64;
/// Fast-out validity (see `qsearch_see_pruneable`): the ramp must be
/// non-positive at every depth so that `victim >= attacker ⇒ see >= 0 ⇒ not
/// pruneable` holds. `-SLOPE * max(0, ·)` clamped to `[FLOOR, 0]` is `<= 0` iff
/// `FLOOR <= 0 && SLOPE >= 0`. Compile-time-pinned (strongest form).
const _: () = assert!(QS_SEE_RAMP_FLOOR <= 0 && QS_SEE_RAMP_SLOPE >= 0);

/// Depth-conditioned SEE-prune threshold for the current ID root iteration.
/// `threshold(d) = clamp(-SLOPE * max(0, d - D0), FLOOR, 0)`: `0` for `d <= D0`
/// (flat-0 ≡ M7.B), ramping to `FLOOR` at `d >= D0 + FLOOR.abs()/SLOPE`.
/// Monotonically non-increasing in `d`; always in `[FLOOR, 0]`.
fn qs_see_prune_threshold(root_depth: u32) -> i32 {
    (-QS_SEE_RAMP_SLOPE * (root_depth.saturating_sub(QS_SEE_RAMP_D0) as i32))
        .clamp(QS_SEE_RAMP_FLOOR, 0)
}

/// Returns `true` iff `mv` (a non-promotion capture: `Capture` | `EnPassant`)
/// should be pruned from quiescence as statically losing relative to
/// `threshold` (the depth-conditioned `qs_see_prune_threshold` value). The
/// caller guarantees `!in_check` and `mv.is_capture() && !mv.is_promotion()`.
///
/// Fast-out: a capture whose victim is worth >= the attacker can never be
/// SEE-negative (worst case the attacker is traded off, netting >= 0), so
/// the full `see()` resolver is skipped for the common winning/equal captures
/// (PxN, RxQ, equal trades, all EP PxP). Only `attacker > victim` captures
/// (QxP-class) pay for the resolver. The fast-out is only valid while
/// `threshold <= 0` (victim>=attacker ⇒ see>=0 ⇒ not < threshold); the ramp is
/// `<= 0` by construction (clamped to `[FLOOR, 0]`, FLOOR<=0), pinned at the
/// const site above and re-asserted here as a debug belt-and-suspenders.
fn qsearch_see_pruneable(pos: &Position, mv: Move, threshold: i32) -> bool {
    use crate::mov::MoveFlag;
    use crate::piece::PieceKind;
    use crate::see::{SEE_VALUE, see};
    debug_assert!(threshold <= 0, "fast-out validity requires threshold <= 0");

    let attacker = SEE_VALUE[pos
        .piece_at(mv.from_square())
        .expect("qsearch_see_pruneable: capture from-square occupied")
        .kind as usize];
    // EP victim is a pawn off-square (the to-square is empty for EP); reading
    // piece_at(to) would be a bug. All other captures take the piece on `to`.
    let victim = match mv.flag() {
        MoveFlag::EnPassant => SEE_VALUE[PieceKind::Pawn as usize],
        _ => {
            SEE_VALUE[pos
                .piece_at(mv.to_square())
                .expect("qsearch_see_pruneable: capture to-square occupied")
                .kind as usize]
        }
    };
    if victim >= attacker {
        return false; // SEE >= 0; never prune (valid while threshold <= 0)
    }
    see(pos, mv) < threshold
}

/// M5.E #3 — given a queen-promo move `mv`, returns the rook-promo and
/// bishop-promo variants at the same `(from, to)`. Capture-aware: if `mv`
/// is `QueenPromoCapture`, returns the `*PromoCapture` variants; otherwise
/// the non-capture `*Promo` variants. Knight-promo deliberately omitted —
/// fork-tactic motivation is independent of stalemate avoidance and out
/// of scope per ADR-0027 §1 (M5.E open-questions row 1).
///
/// Caller contract: `mv.flag()` SHOULD be `QueenPromo | QueenPromoCapture`.
/// On any other input, the helper returns `[None, None]` — defense-in-depth
/// against a future call-site refactor that loses the queen-promo gate.
/// The empty-array fallback mirrors the M5.D `ffp_pruned_bound -> Option<i32>`
/// helper-level domain guard precedent: misuse returns the empty value of
/// the contribution type rather than panicking.
///
/// The returned moves are legal-by-construction at the same `(from, to)`
/// as the input queen-promo: legal-direct movegen would have emitted all
/// four promo variants from the same square pair, and the legality of the
/// move (geometric path, check resolution, pin) is identical across the
/// four promotion choices. The caller may pass the synthesized moves to
/// `make_move` without re-validation.
pub(crate) fn stalemate_avoiding_under_promos(mv: Move) -> [Option<Move>; 2] {
    use crate::mov::MoveFlag::*;
    let (rook_flag, bishop_flag) = match mv.flag() {
        QueenPromo => (RookPromo, BishopPromo),
        QueenPromoCapture => (RookPromoCapture, BishopPromoCapture),
        _ => return [None, None],
    };
    let from = mv.from_square();
    let to = mv.to_square();
    [
        Some(Move::new(from, to, rook_flag)),
        Some(Move::new(from, to, bishop_flag)),
    ]
}

/// MVV-LVA move ordering score. Captures and promotions score > 0; quiets score 0.
///
/// Victim value × 16 − attacker value ensures victim kind dominates across the
/// entire MVV-LVA matrix (PeSTO MG: P=82, N=337, B=365, R=477, Q=1025, K=0).
/// Non-capture promotions score `promo_value` (between large captures and quiets).
/// Promo-captures score `victim×16 − attacker + promo_value`.
fn mvv_lva_score(mv: Move, pos: &Position) -> i32 {
    use crate::eval::MATERIAL;
    use crate::mov::MoveFlag::*;

    let flag = mv.flag();
    let attacker_kind = pos.piece_at(mv.from_square()).unwrap().kind;
    let attacker_value = MATERIAL[attacker_kind as usize];

    match flag {
        // Quiets sort below all captures/promotions.
        Quiet | DoublePush | KingCastle | QueenCastle => 0,
        Capture => {
            let victim_kind = pos.piece_at(mv.to_square()).unwrap().kind;
            let victim_value = MATERIAL[victim_kind as usize];
            victim_value * 16 - attacker_value
        }
        EnPassant => {
            // EP victim is always a pawn regardless of capture-square arithmetic.
            MATERIAL[crate::piece::PieceKind::Pawn as usize] * 16 - attacker_value
        }
        QueenPromo | RookPromo | BishopPromo | KnightPromo => {
            MATERIAL[mv.promotion_kind().unwrap() as usize] // no victim; no attacker subtraction
        }
        QueenPromoCapture | RookPromoCapture | BishopPromoCapture | KnightPromoCapture => {
            let victim_kind = pos.piece_at(mv.to_square()).unwrap().kind;
            let victim_value = MATERIAL[victim_kind as usize];
            let promo_value = MATERIAL[mv.promotion_kind().unwrap() as usize];
            victim_value * 16 - attacker_value + promo_value
        }
    }
}

/// Convert an internal score to a UCI score token: `"cp <N>"` or `"mate <N>"`.
///
/// Scores with `|score| >= MATE_IN_MAX_PLY` are treated as mate scores.
/// The returned move count is `(MATE - |score| + 1) / 2` (plies-to-mate rounded
/// up to full moves), negative when the side to move is being mated.
fn score_to_uci(score: i32) -> String {
    debug_assert!(
        score > i32::MIN,
        "score must be > i32::MIN to avoid abs() overflow"
    );
    let abs_score = score.unsigned_abs() as i32;
    if abs_score >= MATE_IN_MAX_PLY {
        let plies_to_mate = MATE - abs_score;
        let moves = (plies_to_mate + 1) / 2;
        let signed_moves = if score > 0 { moves } else { -moves };
        format!("mate {signed_moves}")
    } else {
        format!("cp {score}")
    }
}

// ---------------------------------------------------------------------------
// Draw-detection helpers (M3.B). Consumed by M3.C alpha-beta.
// ---------------------------------------------------------------------------

/// Single-occurrence-in-search repetition test.
///
/// `history` is the full Zobrist trajectory ending with the current
/// position (i.e. `history.last()` is the position to test). Returns
/// `true` if any earlier entry equals `history.last()`, walking back at
/// most `min(halfmove_clock, history.len() - 1)` plies in 2-ply steps.
///
/// Per CPW "Repetitions": a single match inside the search counts as a
/// draw. The FIDE 9.2 three-fold-claim rule is enforced by the GUI /
/// tournament adjudicator, not the engine. The 2-ply step exploits the
/// fact that only same-side-to-move positions can repeat (the Zobrist
/// turn key flips on every ply). The `halfmove_clock` cap is the
/// irreversible-move stop: any pawn move or capture zeros the clock and
/// severs the repetition chain. The `history.len() - 1` cap is the
/// safety bound — `halfmove_clock` is loaded from FEN and may exceed
/// the history depth the engine has actually observed.
pub fn is_repetition(history: &[u64], halfmove_clock: u8) -> bool {
    let Some((&current, prior)) = history.split_last() else {
        return false;
    };
    let max_back = (halfmove_clock as usize).min(prior.len());
    for i in (2..=max_back).step_by(2) {
        if prior[prior.len() - i] == current {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// M3.E — Time management: TimeCaps + compute_caps pure function.
// ---------------------------------------------------------------------------

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

/// 50-move rule: returns true at `halfmove_clock >= 100`.
///
/// 100 plies = 50 full moves without a pawn move or capture; this is the
/// FIDE 9.3 "claimable" threshold. The engine treats a claimable draw as
/// drawn for search reasoning — strictly speaking, a tournament arbiter
/// only adjudicates after a claim, but a side that wants the draw can
/// always claim it, so the position is effectively a draw at the search
/// horizon. The 75-move auto-draw (FIDE 9.6.2, 150 plies) is not
/// separately handled; we cross the claim threshold first.
pub fn is_fifty_move_draw(halfmove_clock: u8) -> bool {
    halfmove_clock >= 100
}

// ---------------------------------------------------------------------------
// M6.G corpus seam — quiescence eval accessor (additive, behavior-neutral).
//
// Reads the *existing* `AlphaBetaMover::qsearch` with a full window at ply 0.
// Introduces no new search/eval logic and touches no existing code path, so
// `evaluate`/negamax/qsearch and the deterministic `bench` signature are
// byte-identical (the M6.G "no engine build / no bench" clause targets
// strength/bench-affecting change). White-POV (Texel needs White-positive);
// `qsearch` returns a side-to-move-relative score, negated for Black here.
//
// Consumed by `corpus::quiet` (M6.G) and downstream by M6.I, the
// tuning-backlog "PST co-tuning" Arm B, future SPSA campaigns, and M11
// NNUE data-prep — a deliberately public data-infra seam.
// ---------------------------------------------------------------------------

/// Opaque per-worker quiescence-eval handle. Wraps the crate-private
/// searcher so the M6.G seam does not leak engine internals (the
/// `evaluate_cached` `pub(crate)`-type-leak precedent). Construct once per
/// worker thread and reuse across positions — do **not** allocate per
/// position (R6/R7: avoids per-position searcher/TT/pawn-hash allocation).
pub struct QSearcher {
    inner: AlphaBetaMover,
    /// Reused per-call so each `eval_white` invocation does not allocate
    /// a fresh `Arc<AtomicBool>` (≈6M allocations elided on a 3M-record
    /// corpus build). The flag stays `false` for the lifetime of this
    /// `QSearcher` — `eval_white` never sets `stop`, and there is no
    /// other concurrent owner.
    stop: Arc<AtomicBool>,
}

impl Default for QSearcher {
    fn default() -> Self {
        Self::new()
    }
}

impl QSearcher {
    /// New reusable handle.
    pub fn new() -> Self {
        QSearcher {
            inner: AlphaBetaMover::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// White-POV quiescence-search score (centipawns) of `pos`.
    /// `evaluate`/qsearch logic is unchanged; this only *reads* it. White-POV
    /// because Texel needs White-positive (`qsearch` is side-to-move-relative,
    /// negated for Black here).
    pub fn eval_white(&mut self, pos: &Position) -> i32 {
        let caps = TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        };
        let ctx = SearchContext {
            stop: Arc::clone(&self.stop),
            caps,
            virtual_clock: false,
            limits: SearchLimits::default(),
            history: Vec::new(),
            tt: None,
        };
        let clock = SearchClock::start_for(false, caps);
        let mut work = *pos;
        let stm_score = self.inner.qsearch(&mut work, -INF, INF, 0, &ctx, &clock);
        if pos.side_to_move() == crate::Color::White {
            stm_score
        } else {
            -stm_score
        }
    }
}

/// White-POV quiescence-search score (centipawns) of `pos`. Convenience
/// wrapper that allocates a throwaway [`QSearcher`]; for bulk corpus use
/// prefer a per-worker-reused `QSearcher`.
pub fn quiescence_eval_white(pos: &Position) -> i32 {
    QSearcher::new().eval_white(pos)
}

// ---------------------------------------------------------------------------
// M5.H1 — MoveStager: iterator over the negamax move sequence.
//
// H1 v2 implementation: thin wrapper around a single eager-sorted Vec, matching
// M5.G's `order_moves` per-node cost exactly. The earlier H1 v1 design used a
// stage-state machine with per-stage Vecs (captures, quiets), which was
// bench-equivalent at depth 7 but introduced ~3× per-node allocation pressure
// and a second `sort_by_cached_key` call. That cost surfaced under sustained
// game-load as a ~3.5% NPS regression at deep search and ~50 Elo regression
// in the defensive SPRT confirmation match — both invisible to the bench
// snapshot. The thin-wrapper restores per-node cost parity with M5.G while
// preserving the API surface (`new` / `next` / `peek` / `len` / `is_empty` /
// `yield_sequence`) that the M5.G SE block, the position-counter pattern, and
// future M5.H2 lazy generation will consume.
//
// M5.H2's contract is then: keep the same public API, swap the internal Vec
// for per-stage lazy generation. The five logical stages (TT → captures →
// killer 0 → killer 1 → quiets) are still produced by `negamax_move_order_score`
// — they are encoded in the comparator's score tiers, not in a separate enum.
// Plan: docs/plans/m5.h1.md §3.  ADR-0030.
// ---------------------------------------------------------------------------

/// M5.H1 — iterator over the negamax move sequence.
///
/// Yields the same byte-equivalent move sequence as today's `order_moves` +
/// `for mv in moves_vec.iter()` pattern. Internally backed by a single
/// `Vec<Move>` sorted by `negamax_move_order_score` with TT promotion to
/// index 0 — identical algorithm and per-node cost to M5.G.
///
/// **H1 generation discipline.** All moves are generated by a single
/// `generate_moves` call at construction. M5.H2 will switch to lazy
/// per-stage generation; the public API stays the same.
///
/// `total_len` is the pre-iteration count computed at construction and is NOT
/// decremented by `next()` (see §3.1 of the plan). Do NOT call `is_empty()`
/// mid-iteration; the negamax caller honours this by checking once at step 11.
pub(crate) struct MoveStager {
    /// Pre-ordered move sequence (single sort by `negamax_move_order_score`
    /// + TT promotion to index 0). Iterated by index.
    moves: Vec<Move>,
    /// Index of the next move `next()` will yield.
    idx: usize,
    /// Pre-iteration total. `len()` returns this regardless of how far
    /// `next()` has advanced (temporal-contract pin: H1-S17).
    total_len: usize,
}

impl MoveStager {
    /// Eagerly generate, optionally apply searchmoves filter, sort by
    /// `negamax_move_order_score`, and promote TT to index 0.
    ///
    /// `tt_move == 0` → no TT promotion. `killer{0,1}.bits() == 0` (the
    /// `Move::default()` sentinel) → that slot is unset (the comparator's
    /// `mv == killer{0,1}` test never matches). `searchmoves_filter == None`
    /// → no filter (typical at ply > 0).
    pub(crate) fn new(
        pos: &Position,
        killer0: Move,
        killer1: Move,
        history: &HistoryTable,
        tt_move: u16,
        searchmoves_filter: Option<&[Move]>,
    ) -> Self {
        use crate::movegen::generate_moves;
        // Eager full generation.
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves: Vec<Move> = ml.iter().collect();

        // Searchmoves filter (root-only; None at non-root). Mirrors today's
        // `moves_vec.retain(|m| filter.contains(m))` BEFORE order_moves.
        if let Some(filter) = searchmoves_filter {
            moves.retain(|m| filter.contains(m));
        }

        // Single sort by the M5.G score-tier comparator: captures (with
        // CAPTURE_OFFSET) > killer0 (KILLER0_SCORE) > killer1 (KILLER1_SCORE)
        // > history-quiets. Stable; movegen-emit order preserved within ties.
        moves.sort_by_cached_key(|&m| -negamax_move_order_score(m, pos, killer0, killer1, history));

        // TT promotion (mirrors M5.G's `order_moves` post-sort step).
        // `tt_move == 0` (Move::default sentinel) short-circuits without
        // scanning. `Vec::remove(idx)` + `insert(0, mv)` is order-preserving
        // for all non-moved elements.
        if tt_move != 0
            && let Some(idx) = moves.iter().position(|m| m.bits() == tt_move)
            && idx != 0
        {
            let mv = moves.remove(idx);
            moves.insert(0, mv);
        }

        let total_len = moves.len();
        Self {
            moves,
            idx: 0,
            total_len,
        }
    }

    /// Advance one move. Returns `None` once the sequence is exhausted.
    #[inline]
    pub(crate) fn next(&mut self) -> Option<Move> {
        let mv = self.moves.get(self.idx).copied()?;
        self.idx += 1;
        Some(mv)
    }

    /// Return what `next()` would yield without advancing.
    ///
    /// **Idempotency invariant.** `peek()` takes `&self` (not `&mut self`):
    /// consecutive calls return identical `Option<Move>` values. Load-bearing
    /// for the M5.G SE block, which calls `peek()` twice.
    #[inline]
    pub(crate) fn peek(&self) -> Option<Move> {
        self.moves.get(self.idx).copied()
    }

    /// Pre-iteration total move count. Does NOT decrement as `next()` advances.
    // Not called from production paths (is_empty covers the negamax gate); used by tests.
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.total_len
    }

    /// `true` iff `len() == 0`. Same temporal contract as `len()`.
    pub(crate) fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// Test-only: materialise the full yield sequence (clone-and-drain).
    /// Used by the H1-P1 equivalence proptest and the H1-S* yield-order tests.
    #[cfg(test)]
    pub(crate) fn yield_sequence(&self) -> Vec<Move> {
        // From the current `idx` onwards. `plain_stager(&pos)` constructs a
        // fresh stager with `idx = 0`, so this returns the full ordered list
        // for tests that consume `MoveStager::new(...).yield_sequence()`.
        self.moves[self.idx..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// M5.H1 — partition / sort / extract helpers.
//
// Originally introduced in H1 v1 (per-stage stager) and used by
// `MoveStager::new` to partition into captures/quiets and sort each tier.
// H1 v2 (this file's current `MoveStager`) collapses the per-stage sorts back
// into a single `negamax_move_order_score` sort to match M5.G's per-node cost
// (the v1 design's ~3× allocation pressure caused a sustained-load NPS
// regression invisible to bench but visible in SPRT — see `MoveStager`'s
// header comment for context). The helpers below remain `#[cfg(test)]`-only
// as the H1-H1..H10 mutation-discrimination surface; M5.H2's per-stage lazy
// generation may resurrect them as production code paths, in which case they
// can be un-cfg-tested without API change.
// ---------------------------------------------------------------------------

/// Remove and return the first move in `v` whose `bits()` equal `target`.
/// `Vec::remove` (O(N), order-preserving) — chosen over `swap_remove` so
/// post-removal element order matches today's `order_moves` semantics.
/// `target == 0` returns `None` without scanning.
#[cfg(test)]
fn extract_move_by_bits(v: &mut Vec<Move>, target: u16) -> Option<Move> {
    if target == 0 {
        return None;
    }
    let idx = v.iter().position(|m| m.bits() == target)?;
    Some(v.remove(idx))
}

/// Same as `extract_move_by_bits` but compares against a full `Move`
/// (flag bits included). Used for killer-slot extraction.
#[cfg(test)]
fn extract_move_by_eq(v: &mut Vec<Move>, target: Move) -> Option<Move> {
    let idx = v.iter().position(|&m| m == target)?;
    Some(v.remove(idx))
}

/// Partition `all` into `(captures, quiets)` by `is_quiet`. Manual
/// implementation rather than `Iterator::partition` to make the
/// order-preservation guarantee explicit. Within each output Vec, original
/// movegen-emit order is preserved.
#[cfg(test)]
fn partition_captures_quiets(all: Vec<Move>) -> (Vec<Move>, Vec<Move>) {
    let mut captures: Vec<Move> = Vec::with_capacity(all.len());
    let mut quiets: Vec<Move> = Vec::with_capacity(all.len());
    for m in all {
        if is_quiet(m) {
            quiets.push(m);
        } else {
            captures.push(m);
        }
    }
    (captures, quiets)
}

/// MVV-LVA-desc stable sort. Stable within ties (movegen-order preserved).
#[cfg(test)]
fn mvv_lva_sort_in_place(captures: &mut [Move], pos: &Position) {
    captures.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));
}

/// History-score-desc stable sort over a slice of quiet moves.
#[cfg(test)]
fn history_sort_in_place(quiets: &mut [Move], pos: &Position, history: &HistoryTable) {
    let stm = pos.side_to_move();
    quiets.sort_by_cached_key(|&m| -(history.score(stm, m.from_square(), m.to_square()) as i32));
}

// ---------------------------------------------------------------------------

/// Drift anchor (NOT the discriminator) for
/// `negamax_passes_allow_null_false_in_null_subsearch`. The total NMP firing
/// count across the search subtree on the test fixture
/// (`r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9` at
/// depth 8, ply 1, beta = `static_eval - 100`). Re-pinned on every search
/// reshaping (M5.B, M5.C) — the absolute count is sensitive to pruning/eval
/// changes and carries no anti-regression power on its own.
///
/// **M6.D lands at the score-neutral floor → anchor stays 2 (== M6.C).**
/// M6.D ships piece-mobility infrastructure with every `*_MOBILITY_*` weight
/// zeroed in `eval::data` (the §11.4 score-neutral floor — a per-kind SPRT
/// screen ladder proved the literature-default weights have a scale-invariant
/// structural mismatch; M6.F joint Texel reshapes the whole set). With all
/// mobility weights zeroed the term is behaviorally inert and `evaluate` is
/// byte-identical to `M6.C`, so the search tree is byte-identical to M6.C and
/// this firing count reverts to M6.C's value `2`. The pin is a pure
/// regression *anchor* (catches gross unintended tree-shape changes), **not**
/// the stacked-null-prevention discriminator (that is the eval-independent
/// `STACKED_NULL_GATE_REACHABLE == 0` reachability invariant below).
///
/// **Why the count is not the discriminator (M6.D finding).** Flipping the
/// NMP null-search's `allow_null = false` → `true` is *provably and
/// empirically inert* w.r.t. this count: the null search uses the zero-width
/// window `(-beta, -beta + 1)` and `make_null_move` leaves the board (hence
/// `static_eval_white`) unchanged, so a node reached with `allow_null ==
/// false` has `child_static_eval = -parent_static_eval` and `child_beta =
/// -parent_beta + 1`; the parent fired NMP (`parent_static_eval >=
/// parent_beta`), making the child's `static_eval >= child_beta` gate
/// `parent_static_eval <= parent_beta - 1` — a contradiction. The immediate
/// stacked null is mathematically unreachable; verified empirically as
/// `STACKED_NULL_GATE_REACHABLE == 0` across 27 fixture/depth combinations
/// (incl. depth-12, ~15k firings) — the mutant firing count equals the
/// correct count on every one. The load-bearing anti-regression assertion is
/// therefore the `STACKED_NULL_GATE_REACHABLE == 0` *reachability invariant*
/// (see that static's doc), which is the exact provable statement of
/// stacked-null prevention and would flip non-zero under a real regression
/// (e.g. a non-zero-window null search combined with the flag flip).
#[cfg(test)]
const NMP_FIRINGS_PINNED: u32 = 2;

/// Test-only instrumentation for `negamax_passes_allow_null_false_in_null_subsearch`.
///
/// Counts negamax nodes that are reached with `allow_null == false` (i.e., the
/// immediate child of an NMP null-move search — the ONLY place line ~1628
/// passes `allow_null = false`) AND that simultaneously satisfy *every other*
/// NMP gate (`ply > 0`, `!is_pv`, `depth >= NMP_MIN_DEPTH`, `!in_check`,
/// `has_non_pawn_material`, `static_eval >= beta`). At such a node the
/// `allow_null` flag is the SOLE predicate suppressing a (stacked) NMP fire —
/// exactly what flipping line ~1628 `false → true` would unblock.
///
/// **Provably always 0 under the current zero-window NMP design.** The NMP
/// null-search recurses with the zero-width window `(-beta, -beta + 1)`, so a
/// node reached with `allow_null == false` has `child_beta = -parent_beta + 1`
/// and (because `make_null_move` only flips the side — the board, hence
/// `static_eval_white`, is unchanged) `child_static_eval = -parent_static_eval`.
/// The parent fired NMP, so `parent_static_eval >= parent_beta`; the child's
/// `static_eval >= child_beta` gate is `-parent_static_eval >= -parent_beta + 1`
/// ⟺ `parent_static_eval <= parent_beta - 1` — contradicting the parent's
/// firing condition. Hence the immediate stacked null is mathematically
/// unreachable and this counter is identically 0 (empirically confirmed across
/// 27 fixture/depth combinations incl. depth-12 with ~15k firings). The
/// `allow_null = false` at line ~1628 is therefore *defense-in-depth*
/// (ADR-0023 §5) for a hypothetical future non-zero-window null search, not a
/// currently-firing-count-observable guard — which is precisely why the
/// firing-count pin alone CANNOT discriminate the mutation, and this
/// reachability invariant is the load-bearing anti-regression assertion.
#[cfg(test)]
static STACKED_NULL_GATE_REACHABLE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
mod tests;
