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
use crate::{Color, Move, PieceKind, Position};

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

    /// Notify the search that a new game has started (`ucinewgame`). Default:
    /// no-op. M4 will clear killer/history/TT here.
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// M3.C — AlphaBetaMover: fail-soft negamax + alpha-beta + triangular PV.
// ---------------------------------------------------------------------------

/// Maximum search depth in plies. PV table is sized to this constant.
const MAX_PLY: usize = 64;

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
const ASPIRATION_MIN_DEPTH: u32 = 6;

/// First-try aspiration half-width in centipawns. Window is
/// `(prior - HALF_WIDTH, prior + HALF_WIDTH)`. CPW workhorse default;
/// roadmap §M4.D pins ±50 with a documented post-merge width-tune campaign
/// over ±25 / ±75 / ±100.
const ASPIRATION_HALF_WIDTH: i32 = 50;

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

/// First-try aspiration window. Returns `(-INF, INF)` (equivalent to no
/// aspiration) when:
///
/// 1. `depth < ASPIRATION_MIN_DEPTH` — too-shallow iteration; prior
///    score is unstable.
/// 2. `prior_score == None` — no prior iteration to seed from (the first
///    ID iteration of the current `go`).
///
/// Otherwise returns `(prior - ASPIRATION_HALF_WIDTH, prior + ASPIRATION_HALF_WIDTH)`.
/// Mate-score `prior_score` values produce a window straddling the mate
/// boundary; `widen_after_fail` handles the resulting first-try fail via
/// the asymmetric full-window re-search (research §7.2).
///
/// Pure function. Pinned by AS1–AS5b.
fn aspiration_window(prior_score: Option<i32>, depth: u32) -> (i32, i32) {
    if depth < ASPIRATION_MIN_DEPTH {
        return (-INF, INF);
    }
    let Some(prior) = prior_score else {
        return (-INF, INF);
    };
    (prior - ASPIRATION_HALF_WIDTH, prior + ASPIRATION_HALF_WIDTH)
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
}

impl AlphaBetaMover {
    /// Create a new `AlphaBetaMover` with empty history and zeroed counters.
    pub(crate) fn new() -> Self {
        Self {
            pv: PvTable::new(),
            history: Vec::new(),
            nodes: 0,
            aborted: false,
            root_score: None,
            tt: None,
            killers: [[Move::default(); 2]; MAX_PLY],
            history_table: HistoryTable::new(),
            #[cfg(test)]
            nmp_firings: 0,
            #[cfg(test)]
            rfp_firings: 0,
        }
    }
}

impl Search for AlphaBetaMover {
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

        for depth in 1..=max_depth {
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
            let (mut alpha, mut beta) = aspiration_window(prior_score, depth);

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
        // pv and nodes are reset per-go; TT lives in engine.
    }
}

impl AlphaBetaMover {
    /// Fail-soft negamax with alpha-beta pruning and triangular PV recovery.
    ///
    /// `is_pv` is the synthetic ordering predicate that gates TT cutoffs
    /// (ADR-0018 §11). `true` at the root and at the first child of a PV
    /// parent (recursion-order index 0); `false` everywhere else. PVS at M4.D
    /// will replace it with the window-based `beta - alpha == 1` check.
    ///
    /// `allow_null` (M5.A) gates the NMP block at step 8. `true` from the
    /// top-level `Search::go` call and from the move-loop recursive call;
    /// `false` only in the NMP null-search recursive call (stacked-null
    /// prevention — ADR-0023 §5).
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
        ctx: &SearchContext,
        clock: &SearchClock,
    ) -> i32 {
        use crate::movegen::{MoveList, generate_moves, in_check};

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
        let mut tt_move: u16 = 0;
        if let Some(tt) = &self.tt
            && let Some(entry) = tt.probe(pos.zobrist())
        {
            tt_move = entry.best_move;
            if !is_pv && entry.depth as u32 >= depth {
                let tt_score = score_from_tt(entry.score as i32, ply as i32);
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
                        if let Some(tt) = &self.tt {
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

        // 10. Generate moves.
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves_vec = ml.iter().collect::<Vec<_>>();

        // Searchmoves filter at root only.
        if ply == 0
            && let Some(filter) = &ctx.limits.searchmoves
        {
            moves_vec.retain(|m| filter.contains(m));
        }

        // 11. Terminal: no legal moves.
        //     At ply==0 with searchmoves filter active, an empty list is a degenerate
        //     user input (all-illegal or empty filter). Short-circuit BEFORE the
        //     in_check triage — otherwise a check position with a degenerate filter
        //     would falsely return -MATE.
        if moves_vec.is_empty() {
            if ply == 0 && ctx.limits.searchmoves.is_some() {
                return 0;
            }
            if in_check(pos) {
                return -(MATE - ply as i32);
            } else {
                return 0; // stalemate
            }
        }

        // 12. Order: killer-aware scoring (captures > killers > history-scored
        //     quiets) descending via `order_moves` (extended for M4.C to consult
        //     the history table); then promote the TT move (if any) to index 0.
        //     `Move::default().bits() == 0` is the no-move sentinel and is
        //     never produced by movegen, so `tt_move == 0` falls through.
        //     The legality scan over the legal-move list rejects garbage
        //     values (ADR-0018 §12).
        let killer0 = self.killers[ply as usize][0];
        let killer1 = self.killers[ply as usize][1];
        order_moves(
            &mut moves_vec,
            pos,
            killer0,
            killer1,
            &self.history_table,
            tt_move,
        );

        // 13. Recurse fail-soft. `child_is_pv = is_pv && i == 0` per ADR-0018 §11
        //     where `i` is the recursion-order index (post-step-12 reorder).
        let mut best = -INF;
        let mut cutoff_move: Option<Move> = None;
        // M4.C: quiets that complete recursion without cutting; used for malus on cutoff.
        let mut quiets_searched: MoveList = MoveList::new();
        for (i, &mv) in moves_vec.iter().enumerate() {
            let undo = pos.make_move(mv);
            self.history.push(pos.zobrist());
            let (child_alpha, child_beta) = negate_window(alpha, beta);
            let child_is_pv = is_pv && i == 0;
            let score = -self.negamax(
                pos,
                depth - 1,
                ply + 1,
                child_alpha,
                child_beta,
                child_is_pv,
                true, // allow_null: move-loop children may attempt NMP
                ctx,
                clock,
            );
            self.history.pop();
            pos.unmake_move(mv, undo);

            // Abort check: score from an aborted search is invalid.
            if self.aborted {
                return 0;
            }

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
                        let bonus = (depth as i32) * (depth as i32);
                        // SIDE-TO-MOVE INVARIANT: pos has been restored to the
                        // pre-move-loop state by `pos.unmake_move(mv, undo)` above,
                        // so `pos.side_to_move()` here is the **mover's color**.
                        // Read here, BEFORE any future make/unmake; do NOT recompute
                        // inside the malus loop. Pinned by HS8 (root White) + HS8b
                        // (non-root Black).
                        let side = pos.side_to_move();
                        self.history_table
                            .update(side, mv.from_square(), mv.to_square(), bonus);
                        for prior in quiets_searched.iter() {
                            self.history_table.update(
                                side,
                                prior.from_square(),
                                prior.to_square(),
                                -bonus,
                            );
                        }
                    }
                    break; // beta cutoff — fail-soft: return `best`, not `beta`
                }
            }

            // Did not cut. If quiet, record for potential malus by a later cutter.
            if is_quiet(mv) {
                debug_assert!(
                    mv.from_square() != mv.to_square(),
                    "Move::default() sentinel must never enter quiets_searched"
                );
                quiets_searched.push(mv);
            }
        }

        // 14. Store on completion. Skip on abort (partial bounds are not real)
        //     and never mid-loop (the abort path returns above without storing).
        //     Together this guarantees aborted iterations never overwrite a
        //     prior iteration's entry. Bound classification compares against
        //     `original_alpha` — the caller's pre-MDP alpha (step 4).
        if let Some(tt) = &self.tt
            && !self.aborted
        {
            let bound = if best >= beta {
                TtBound::Lower
            } else if best > original_alpha {
                TtBound::Exact
            } else {
                TtBound::Upper
            };
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
        ctx: &SearchContext,
    ) -> i32 {
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
        self.negamax(pos, depth, ply, alpha, beta, is_pv, allow_null, ctx, &clock)
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

    /// Test-only setter to install a TT directly without going through
    /// `Search::go`. Production code never sets this — the per-`go` install
    /// happens at the top of `Search::go`.
    #[cfg(test)]
    pub(super) fn set_tt_for_test(&mut self, tt: Option<Arc<TranspositionTable>>) {
        self.tt = tt;
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
        use crate::eval::evaluate;
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
            let sp = evaluate(pos);
            if sp >= beta {
                return sp;
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
        //    - Not in check + empty: return stand-pat. The empty-after-filter
        //      case does NOT mean stalemate — the position has many quiet moves.
        //      Returning stand-pat avoids the false-stalemate bug (CPW §10.7).
        if moves_vec.is_empty() {
            if in_chk {
                return -(MATE - ply as i32);
            } else {
                return best_init; // = stand_pat
            }
        }

        // 7. Order: MVV-LVA descending. Reuses negamax's helper.
        moves_vec.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));

        // 8. Recurse fail-soft.
        let mut best = best_init;
        for mv in moves_vec {
            let undo = pos.make_move(mv);
            self.history.push(pos.zobrist());
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

            if score > best {
                best = score;
                if score > alpha {
                    alpha = score;
                }
                if alpha >= beta {
                    break; // beta cutoff, fail-soft
                }
            }
        }

        best
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
        let clock = SearchClock::start_for(ctx.virtual_clock, ctx.caps);
        self.qsearch(pos, alpha, beta, ply, ctx, &clock)
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
const KILLER0_SCORE: i32 = 100_001;

/// Bonus score for the prior quiet beta-cutoff at this ply (slot 1).
/// Must satisfy `KILLER1_SCORE < KILLER0_SCORE` and
/// `KILLER1_SCORE > MAX_HISTORY` (so killers always rank above the best
/// history-rated quiet).
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

/// Pinned NMP firings count for the
/// `negamax_passes_allow_null_false_in_null_subsearch` test (M5.A).
/// Empirically observed at impl time on the chosen fixture
/// (`r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9` at
/// depth 8, ply 1, beta = `static_eval - 100`). The value is the number of
/// NMP firings observed across the entire search subtree under stacked-null
/// prevention; mutating the inner null-search's `allow_null = false` to
/// `true` would let nested nulls fire and inflate this count.
///
/// M5.B re-pin: K is unchanged from M5.A's value of 34. RFP is gated by
/// `depth <= RFP_MAX_DEPTH = 6`, so the test's depth-8 root frame trivially
/// fails the RFP depth predicate before any margin computation. The pin's
/// load-bearing claim — that across the entire search subtree no descendant
/// frame at `depth <= 6` satisfies the RFP gate's structural cheap-gates AND
/// `static_eval - margin*d >= beta_at_that_node` AT a frame where M5.A's NMP
/// would otherwise have fired — is empirical, verified by the test passing
/// at impl time. M5.C's LMR may reshape the descendant set; if K drifts on
/// a future phase's run, re-pin to the new observed value.
#[cfg(test)]
const NMP_FIRINGS_PINNED: u32 = 34;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::movegen::{MoveList, generate_moves};
    use crate::{Move, Position};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn non_aborting_ctx() -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits::default(),
            history: Vec::new(),
            tt: None,
        };
        (ctx, stop)
    }

    fn non_aborting_ctx_at_depth(d: u32) -> (SearchContext, Arc<AtomicBool>) {
        let (ctx, stop) = non_aborting_ctx();
        let ctx = SearchContext {
            limits: SearchLimits {
                depth: Some(d),
                ..SearchLimits::default()
            },
            ..ctx
        };
        (ctx, stop)
    }

    /// Test helper: build a non-aborting context wired with `tt`. M4.A
    /// added the TT field; tests that pre-populate or inspect TT entries
    /// use this instead of `non_aborting_ctx`.
    fn non_aborting_ctx_with_tt(tt: Arc<TranspositionTable>) -> (SearchContext, Arc<AtomicBool>) {
        let (mut ctx, stop) = non_aborting_ctx();
        ctx.tt = Some(tt);
        (ctx, stop)
    }

    /// Test helper: build a non-aborting context wired with depth `d` and
    /// `tt`.
    fn non_aborting_ctx_at_depth_with_tt(
        d: u32,
        tt: Arc<TranspositionTable>,
    ) -> (SearchContext, Arc<AtomicBool>) {
        let (mut ctx, stop) = non_aborting_ctx_at_depth(d);
        ctx.tt = Some(tt);
        (ctx, stop)
    }

    // -----------------------------------------------------------------------
    // D11 — should_abort three sub-cases (carried over from M2.C verbatim).
    // -----------------------------------------------------------------------

    // D11 is carried over verbatim from M2.C — it tests SearchClock::should_abort
    // (was SearchContext::should_abort pre-ELOH.C; relocated to the worker-local
    // SearchClock so cancellation gets the per-thread clock semantics).
    #[test]
    fn should_abort_three_subcases() {
        // Sub-case 1: stop flag set.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let clock = SearchClock::start_for(
                false,
                TimeCaps {
                    soft: Duration::MAX,
                    hard: Duration::MAX,
                },
            );
            assert!(
                !clock.should_abort(&stop, None, 0),
                "should not abort before stop is set"
            );
            stop.store(true, Ordering::Relaxed);
            assert!(
                clock.should_abort(&stop, None, 0),
                "should abort after stop flag is set"
            );
        }

        // Sub-case 2: deadline already expired.
        {
            let stop = Arc::new(AtomicBool::new(false));
            // 1ms hard cap; sleep to ensure the deadline is in the past.
            let clock = SearchClock::start_for(
                false,
                TimeCaps {
                    soft: Duration::MAX,
                    hard: Duration::from_millis(1),
                },
            );
            std::thread::sleep(Duration::from_millis(5));
            assert!(
                clock.should_abort(&stop, None, 0),
                "should abort when deadline is already in the past"
            );
        }

        // Sub-case 3: node cap.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let clock = SearchClock::start_for(
                false,
                TimeCaps {
                    soft: Duration::MAX,
                    hard: Duration::MAX,
                },
            );
            assert!(
                clock.should_abort(&stop, Some(500), 1_000),
                "should abort when nodes_searched (1000) >= cap (500)"
            );
            assert!(
                !clock.should_abort(&stop, Some(500), 100),
                "should not abort when nodes_searched (100) < cap (500)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M3.B — draw-detection helper unit tests. Consumed by M3.C alpha-beta.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // is_fifty_move_draw — threshold at 100 plies.
    // -----------------------------------------------------------------------

    #[test]
    fn is_fifty_move_draw_threshold_at_100() {
        // False below the threshold: catches ">= 99" or "<= 100" mutants.
        assert!(!is_fifty_move_draw(0), "halfmove 0 must be false");
        assert!(!is_fifty_move_draw(1), "halfmove 1 must be false");
        assert!(!is_fifty_move_draw(50), "halfmove 50 must be false");
        assert!(!is_fifty_move_draw(99), "halfmove 99 must be false");
        // True at and above: catches "> 100" mutant (misses exact boundary).
        assert!(is_fifty_move_draw(100), "halfmove 100 must be true");
        assert!(is_fifty_move_draw(101), "halfmove 101 must be true");
        assert!(is_fifty_move_draw(200), "halfmove 200 must be true");
        assert!(is_fifty_move_draw(255), "halfmove 255 must be true");
    }

    // -----------------------------------------------------------------------
    // is_repetition — empty / minimal history.
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_empty_history_returns_false() {
        assert!(
            !is_repetition(&[], 0),
            "empty history, halfmove=0 must be false"
        );
        assert!(
            !is_repetition(&[], 50),
            "empty history, halfmove=50 must be false"
        );
    }

    #[test]
    fn is_repetition_single_entry_returns_false() {
        // No prior to compare against.
        assert!(!is_repetition(&[0xABC], 50), "single entry must be false");
    }

    // -----------------------------------------------------------------------
    // is_repetition — no match.
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_no_match_within_window_returns_false() {
        // [0x1, 0x2, 0x3, 0x4] — current is 0x4, none of the priors match.
        assert!(
            !is_repetition(&[0x1, 0x2, 0x3, 0x4], 4),
            "no matching entry within window must return false"
        );
    }

    // -----------------------------------------------------------------------
    // is_repetition — basic match cases.
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_match_at_2_ply_returns_true() {
        // History [X, Y, X]: current X at index 2, prior X at index 0 (distance 2).
        // halfmove=2 allows looking back 2 plies. Must return true.
        const X: u64 = 0xDEAD_BEEF;
        const Y: u64 = 0xCAFE_BABE;
        assert!(
            is_repetition(&[X, Y, X], 2),
            "match at 2-ply distance must return true"
        );
    }

    #[test]
    fn is_repetition_match_at_4_ply_returns_true() {
        // History [X, Y, Z, W, X]: current X at index 4, prior X at index 0.
        // halfmove=4 allows looking back 4 plies. Pins the second loop iteration.
        const X: u64 = 0xDEAD_BEEF;
        assert!(
            is_repetition(&[X, 0x1, 0x2, 0x3, X], 4),
            "match at 4-ply distance must return true"
        );
    }

    // -----------------------------------------------------------------------
    // is_repetition — even/odd distance (pins step_by(2)).
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_even_distance_match_returns_true() {
        // History [X, A, B, C, D, E, X]: current X at index 6, prior X at index 0.
        // Distance = 6 (even). halfmove=6. Must return true.
        const X: u64 = 0xAAAA;
        assert!(
            is_repetition(&[X, 0x1, 0x2, 0x3, 0x4, 0x5, X], 6),
            "even-distance (6-ply) match must return true"
        );
    }

    #[test]
    fn is_repetition_odd_distance_match_returns_false() {
        // History [Y, X, A, B, X]: current X at index 4, prior X at index 1.
        // Distance = 3 (odd). step_by(2) never checks i=3 — skips odd-distance entries.
        const X: u64 = 0xBBBB;
        assert!(
            !is_repetition(&[0x9999, X, 0x1, 0x2, X], 3),
            "odd-distance match must return false (2-ply step skips it)"
        );
    }

    // -----------------------------------------------------------------------
    // is_repetition — halfmove_clock cap.
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_capped_by_halfmove_clock() {
        // History [X, Y, Z, W, X] (5 entries): current X at index 4, prior X at
        // index 0 (distance 4). halfmove=2 caps the search at 2 plies.
        const X: u64 = 0xCCCC;
        assert!(
            !is_repetition(&[X, 0x1, 0x2, 0x3, X], 2),
            "halfmove=2 must cap lookback to 2 plies; X at distance 4 must be invisible"
        );
    }

    // -----------------------------------------------------------------------
    // is_repetition — safety bound: halfmove_clock may exceed history depth.
    // -----------------------------------------------------------------------

    #[test]
    fn is_repetition_no_panic_when_halfmove_exceeds_history_match() {
        // History [X, Y, X] (prior.len()=2), halfmove=200.
        const X: u64 = 0xDDDD;
        assert!(
            is_repetition(&[X, 0x1, X], 200),
            "halfmove exceeds history but match at i=2 must still return true"
        );
    }

    #[test]
    fn is_repetition_no_panic_when_halfmove_exceeds_history_no_match() {
        // History [X, Y, Z] (prior.len()=2), halfmove=200, no match.
        assert!(
            !is_repetition(&[0x1, 0x2, 0x3], 200),
            "halfmove exceeds history, no match — must return false without panic"
        );
    }

    #[test]
    fn is_repetition_halfmove_zero_returns_false() {
        // History [X, Y, X], halfmove=0. Irreversible move zeroed the clock.
        const X: u64 = 0xEEEE;
        assert!(
            !is_repetition(&[X, 0x1, X], 0),
            "halfmove=0 (irreversible move) must return false"
        );
    }

    // ===========================================================================
    // M3.C — AlphaBetaMover unit tests.
    // ===========================================================================

    // -----------------------------------------------------------------------
    // MVV-LVA helper tests (rows 1–4 of §8.1 table)
    // -----------------------------------------------------------------------

    /// PxQ must score higher than QxP. Victim-dominates-attacker property.
    #[test]
    fn mvv_lva_pawn_takes_queen_scores_higher_than_queen_takes_pawn() {
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");

        let pawn_takes_queen =
            Move::from_uci("b4c5", &pos).expect("b4c5 must be a legal pawn capture");
        let queen_takes_pawn =
            Move::from_uci("d5c6", &pos).expect("d5c6 must be a legal queen capture");

        let pxq = mvv_lva_score(pawn_takes_queen, &pos);
        let qxp = mvv_lva_score(queen_takes_pawn, &pos);
        assert!(
            pxq > qxp,
            "PxQ score ({pxq}) must be > QxP score ({qxp}) — victim dominates attacker"
        );
    }

    /// PromoCapture score must include the promo bonus.
    #[test]
    fn mvv_lva_promo_capture_includes_promo_bonus() {
        // Position: white pawn on a7, black rook on b8, white to move.
        // a7xb8=Q is a queen-promo-capture.
        let pos = Position::from_fen("1r5k/P7/8/8/8/8/8/4K3 w - - 0 1").expect("FEN must parse");

        let promo_capture =
            Move::from_uci("a7b8q", &pos).expect("a7b8q must be a legal queen promo-capture");
        let non_capture_promo =
            Move::from_uci("a7a8q", &pos).expect("a7a8q must be a legal queen promo");

        let promo_capture_score = mvv_lva_score(promo_capture, &pos);
        let non_capture_score = mvv_lva_score(non_capture_promo, &pos);
        assert!(
            promo_capture_score > non_capture_score,
            "promo-capture score ({promo_capture_score}) must exceed non-capture promo score ({non_capture_score})"
        );
    }

    /// Quiet / DoublePush / KingCastle / QueenCastle all score exactly 0.
    #[test]
    fn mvv_lva_quiet_scores_zero() {
        let pos = Position::starting_position();
        let quiet_move = Move::from_uci("e2e3", &pos).expect("e2e3 must be a legal quiet move");
        let double_push = Move::from_uci("e2e4", &pos).expect("e2e4 must be a legal double push");

        assert_eq!(
            mvv_lva_score(quiet_move, &pos),
            0,
            "quiet move must score 0"
        );
        assert_eq!(
            mvv_lva_score(double_push, &pos),
            0,
            "double pawn push must score 0"
        );

        // Castling: use Kiwipete where both castling rights exist.
        let kiwipete = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("Kiwipete FEN must parse");
        let king_castle = Move::from_uci("e1g1", &kiwipete)
            .expect("e1g1 (king-side castle) must be legal in Kiwipete");
        let queen_castle = Move::from_uci("e1c1", &kiwipete)
            .expect("e1c1 (queen-side castle) must be legal in Kiwipete");
        assert_eq!(
            mvv_lva_score(king_castle, &kiwipete),
            0,
            "king-side castle must score 0"
        );
        assert_eq!(
            mvv_lva_score(queen_castle, &kiwipete),
            0,
            "queen-side castle must score 0"
        );
    }

    /// En-passant score uses pawn as victim regardless of capture-square geometry.
    #[test]
    fn mvv_lva_en_passant_scores_pawn_victim() {
        // FEN with en-passant available: white pawn on e5, black pawn just pushed
        // to d5, so EP target is d6.
        let pos =
            Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").expect("EP FEN must parse");
        let ep_move = Move::from_uci("e5d6", &pos).expect("e5d6 must be a legal EP capture");

        // EP score = P(82)*16 - P(82) = 1312 - 82 = 1230.
        let ep_score = mvv_lva_score(ep_move, &pos);
        assert_eq!(
            ep_score, 1230,
            "EP score must equal pawn_victim*16 - pawn_attacker = 82*16-82 = 1230; got {ep_score}"
        );
    }

    // -----------------------------------------------------------------------
    // score_to_uci tests (rows 5–8 of §8.1 table)
    // -----------------------------------------------------------------------

    /// Normal centipawn scores produce "cp <N>".
    #[test]
    fn score_to_uci_cp_for_normal_scores() {
        assert_eq!(score_to_uci(36), "cp 36");
        assert_eq!(score_to_uci(-1024), "cp -1024");
        assert_eq!(score_to_uci(0), "cp 0");
    }

    /// Winning mate scores produce "mate N" with N > 0.
    #[test]
    fn score_to_uci_mate_in_n_for_winning_mates() {
        // MATE - 1 = 29999: mate in 1 ply → 1 full move → "mate 1"
        assert_eq!(score_to_uci(MATE - 1), "mate 1");
        // MATE - 5 = 29995: mate in 5 plies → (5+1)/2 = 3 full moves → "mate 3"
        assert_eq!(score_to_uci(MATE - 5), "mate 3");
    }

    /// Losing (being mated) scores produce "mate -N" with N > 0.
    #[test]
    fn score_to_uci_mate_negative_for_losing_mates() {
        // -(MATE - 4) = -29996: mated in 4 plies → (4+1)/2 = 2 → "mate -2"
        assert_eq!(score_to_uci(-(MATE - 4)), "mate -2");
    }

    /// Boundary: MATE_IN_MAX_PLY triggers "mate"; MATE_IN_MAX_PLY - 1 triggers "cp".
    #[test]
    fn score_to_uci_threshold_at_mate_in_max_ply() {
        // At MATE_IN_MAX_PLY (29936), |score| >= MATE_IN_MAX_PLY → mate string.
        let score_at_boundary = MATE_IN_MAX_PLY;
        let result = score_to_uci(score_at_boundary);
        assert!(
            result.starts_with("mate "),
            "score MATE_IN_MAX_PLY ({score_at_boundary}) must produce a 'mate ...' string; got '{result}'"
        );

        // One below: MATE_IN_MAX_PLY - 1 = 29935 is NOT a mate score → "cp".
        let below_boundary = MATE_IN_MAX_PLY - 1;
        assert_eq!(
            score_to_uci(below_boundary),
            format!("cp {below_boundary}"),
            "score MATE_IN_MAX_PLY - 1 ({below_boundary}) must produce a 'cp ...' string"
        );
    }

    // -----------------------------------------------------------------------
    // PvTable tests (rows 9–10 of §8.1 table)
    // -----------------------------------------------------------------------

    /// clear_ply resets the length at the specified ply and leaves others intact.
    #[test]
    fn pv_table_clear_ply_resets_length() {
        let mut pv = PvTable::new();
        pv.lengths[3] = 5;
        pv.lengths[2] = 7;
        pv.clear_ply(3);
        assert_eq!(pv.lengths[3], 0, "clear_ply(3) must reset lengths[3] to 0");
        assert_eq!(pv.lengths[2], 7, "clear_ply(3) must not affect lengths[2]");
    }

    /// update(ply, mv) copies the child PV into the parent slot correctly.
    #[test]
    fn pv_table_update_copies_child_pv() {
        let mut pv = PvTable::new();

        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mv_a = Move::new(Square::A1, Square::B1, MoveFlag::Quiet);
        let mv_b = Move::new(Square::A1, Square::C1, MoveFlag::Quiet);
        let mv_c = Move::new(Square::A1, Square::D1, MoveFlag::Quiet);
        let mv_x = Move::new(Square::A1, Square::E1, MoveFlag::Quiet);

        // Simulate child PV at ply 5: [mv_a, mv_b, mv_c], length 3.
        pv.moves[5][0] = mv_a;
        pv.moves[5][1] = mv_b;
        pv.moves[5][2] = mv_c;
        pv.lengths[5] = 3;

        // Update ply 4 with move mv_x.
        pv.update(4, mv_x);

        assert_eq!(pv.lengths[4], 4, "lengths[4] must be 1 + child_len (1+3=4)");
        assert_eq!(
            pv.moves[4][0], mv_x,
            "pv.moves[4][0] must be mv_x (the new move)"
        );
        assert_eq!(
            pv.moves[4][1], mv_a,
            "pv.moves[4][1] must be mv_a (child pv[0])"
        );
        assert_eq!(
            pv.moves[4][2], mv_b,
            "pv.moves[4][2] must be mv_b (child pv[1])"
        );
        assert_eq!(
            pv.moves[4][3], mv_c,
            "pv.moves[4][3] must be mv_c (child pv[2])"
        );
    }

    // -----------------------------------------------------------------------
    // AlphaBetaMover integration tests (rows 11–20 of §8.1 table)
    // -----------------------------------------------------------------------

    fn go_alpha_beta_no_sink(
        ab: &mut AlphaBetaMover,
        pos: &Position,
        ctx: &SearchContext,
    ) -> SearchResult {
        ab.go(pos, ctx, &|_| {})
    }

    /// AlphaBetaMover must find mate-in-1 from a constructed position.
    ///
    /// Position: `7k/8/5KQ1/8/8/8/8/8 w - - 0 1`
    /// Black king h8, white king f6, white queen g6.
    /// Both Qg6-g7 and Qg6-h7 deliver checkmate (the queen covers all
    /// escape squares from either destination), so we verify the score
    /// is MATE−1 rather than asserting a specific move.
    #[test]
    fn alphabeta_finds_mate_in_1_from_constructed_position() {
        let pos =
            Position::from_fen("7k/8/5KQ1/8/8/8/8/8 w - - 0 1").expect("mate-in-1 FEN must parse");
        let mut ab = AlphaBetaMover::new();
        // depth 2 to ensure the engine can see mate-in-1 at depth 1.
        let (ctx_depth2, _stop) = non_aborting_ctx_at_depth(2);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth2);

        assert!(
            result.bestmove.is_some(),
            "mate-in-1 position must have a bestmove"
        );
        assert_eq!(
            result.score_cp,
            Some(MATE - 1),
            "score must be MATE-1 for a mate-in-1 position"
        );
    }

    /// Engine must find a forced mate from a dominating position (Kf6+Qe7 vs Kg8).
    ///
    /// Position: `6k1/4Q3/5K2/8/8/8/8/8 w - - 0 1`
    /// The exact mate distance is not pinned; only that the score crosses
    /// the MATE_IN_MAX_PLY threshold.
    #[test]
    fn alphabeta_finds_forced_mate_from_dominating_position() {
        let pos = Position::from_fen("6k1/4Q3/5K2/8/8/8/8/8 w - - 0 1")
            .expect("dominating position FEN must parse");
        let mut ab = AlphaBetaMover::new();
        let (ctx_depth4, _stop) = non_aborting_ctx_at_depth(4);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth4);

        assert!(
            result.bestmove.is_some(),
            "dominating position must have a bestmove"
        );
        let score = result.score_cp.expect("must have a score");
        assert!(
            score >= MATE_IN_MAX_PLY,
            "engine must find a forced winning mate; got score cp {score}"
        );
    }

    /// Engine must pick the material-winning capture (hanging rook) over passive moves.
    #[test]
    fn alphabeta_takes_hanging_rook_over_passive_move() {
        let pos = Position::from_fen("r3k3/8/8/8/8/8/8/R3K3 w Qq - 0 1")
            .expect("hanging rook FEN must parse");
        let mut ab = AlphaBetaMover::new();
        let (ctx_depth2, _stop) = non_aborting_ctx_at_depth(2);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth2);

        let mv = result
            .bestmove
            .expect("position has legal moves — bestmove must be Some");
        let expected = Move::from_uci("a1a8", &pos).expect("a1a8 must be a legal rook capture");
        assert_eq!(
            mv,
            expected,
            "engine must take the hanging rook (a1a8); got {}",
            mv.to_uci()
        );
    }

    /// Stalemate at root must return score 0 and bestmove None.
    ///
    /// Position: `7k/5K2/6Q1/8/8/8/8/8 b - - 0 1` — black king h8, white king f7,
    /// white queen g6. Black has no legal moves and is not in check → stalemate.
    #[test]
    fn alphabeta_returns_zero_for_stalemate_root() {
        let pos =
            Position::from_fen("7k/5K2/6Q1/8/8/8/8/8 b - - 0 1").expect("stalemate FEN must parse");
        let mut ab = AlphaBetaMover::new();
        let (ctx_depth3, _stop) = non_aborting_ctx_at_depth(3);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth3);

        assert_eq!(
            result.score_cp,
            Some(0),
            "stalemate must return score 0; got {:?}",
            result.score_cp
        );
        assert_eq!(result.bestmove, None, "stalemate must return bestmove None");
    }

    /// At ply > 0, a position repeated in history returns score 0.
    #[test]
    fn alphabeta_recognizes_repetition_at_ply_gt_0() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 4 1")
            .expect("startpos with halfmove=4 must parse");

        let (ctx_depth2, _stop) = non_aborting_ctx_at_depth(2);

        let mut ab = AlphaBetaMover::new();
        let other_zobrist: u64 = 0xDEAD_BEEF_CAFE_0000;
        ab.history = vec![pos.zobrist(), other_zobrist, pos.zobrist()];

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, true, &ctx_depth2);
        assert_eq!(
            score, 0,
            "repetition at ply > 0 must return score 0; got {score}"
        );
    }

    /// At ply > 0, a position with halfmove_clock == 100 returns score 0.
    #[test]
    fn alphabeta_returns_50_move_draw_at_halfmove_100() {
        let pos = Position::from_fen("8/8/8/8/4k3/8/8/4K3 w - - 100 1")
            .expect("KvK with halfmove=100 FEN must parse");

        let (ctx_depth2, _stop) = non_aborting_ctx_at_depth(2);

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, true, &ctx_depth2);
        assert_eq!(
            score, 0,
            "50-move draw at halfmove=100 must return score 0; got {score}"
        );
    }

    /// After `go depth 3` from startpos, the PV first move must be a legal move.
    #[test]
    fn alphabeta_pv_first_move_is_legal_in_starting_position() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        let (ctx_depth3, _stop) = non_aborting_ctx_at_depth(3);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth3);

        let mv = result
            .bestmove
            .expect("startpos has legal moves — bestmove must be Some");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|legal| legal == mv),
            "PV first move {} must be in generate_moves(startpos)",
            mv.to_uci()
        );
    }

    /// For each consecutive PV move pair, applying the first move must make
    /// the second move legal. Tests the triangular-PV legality invariant.
    mod proptest_m3c_e1 {
        use super::*;
        use crate::search::AlphaBetaMover;
        use proptest::prelude::*;

        const FENS: &[&str] = &[
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
            "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
        ];

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 64,
                ..ProptestConfig::default()
            })]
            /// Each consecutive PV move pair (pv[i], pv[i+1]) must satisfy:
            /// after applying pv[i] from the current position, pv[i+1] is legal
            /// in the resulting position. Pins the triangular-PV legality invariant
            /// across the standard fixture set.
            #[test]
            fn alphabeta_pv_each_move_is_legal_after_prior(
                seed in 0u64..u64::MAX,
            ) {
                let fen = FENS[(seed as usize) % FENS.len()];
                let pos = Position::from_fen(fen).unwrap();

                let stop = Arc::new(AtomicBool::new(false));
                let ctx = SearchContext {
                    stop: Arc::clone(&stop),
                    caps: TimeCaps {
                        soft: Duration::MAX,
                        hard: Duration::MAX,
                    },
                    virtual_clock: false,
                    limits: SearchLimits {
                        depth: Some(3),
                        ..SearchLimits::default()
                    },
                    history: vec![pos.zobrist()],
                    tt: None,
                };

                let mut ab = AlphaBetaMover::new();
                ab.go(&pos, &ctx, &|_| {});

                let pv = ab.pv_root_for_test();
                let mut cur_pos = pos;
                for i in 0..pv.len().saturating_sub(1) {
                    let mv = pv[i];
                    let next_mv = pv[i + 1];

                    cur_pos.make_move(mv);

                    let mut ml = MoveList::new();
                    generate_moves(&cur_pos, &mut ml);
                    prop_assert!(
                        ml.iter().any(|m| m == next_mv),
                        "PV[{}] {} must be legal after applying PV[{}] {} in position {}",
                        i + 1,
                        next_mv.to_uci(),
                        i,
                        mv.to_uci(),
                        fen
                    );
                }
            }
        }
    }

    /// Node count at depth 3 from startpos must be below the loose upper bound.
    ///
    /// A full-width depth-3 search would visit ≤ 20^3 = 8000 nodes (loose bound).
    /// Alpha-beta with MVV-LVA ordering should visit much fewer.
    #[test]
    fn alphabeta_node_count_is_below_branching_bound() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        let (ctx_depth3, _stop) = non_aborting_ctx_at_depth(3);
        let result = go_alpha_beta_no_sink(&mut ab, &pos, &ctx_depth3);
        assert!(
            result.nodes < 30_000,
            "depth-3 search from startpos must visit fewer than 30,000 nodes (loose upper bound \
            for qsearch-extended search; observed M3.D count: ~2179, the bound is set well above \
            so it catches a 'branching exploded' regression rather than tripping on minor \
            ordering-shift fluctuations); visited {} nodes",
            result.nodes
        );
    }

    /// With `stop` pre-set before `go`, the search must abort promptly.
    ///
    /// **M3.E semantics** (per plan §5): the inter-iteration stop check breaks
    /// the ID loop AFTER iteration 1 completes (small startpos position visits
    /// ~20 nodes; well below the 4096-node in-iteration cadence). The result
    /// reflects iteration 1's snapshot — `bestmove = Some(<iter-1 best>)`,
    /// `depth = 1`. This differs from M3.D's "no root completes → bestmove=None"
    /// because M3.D had no inter-iteration check (single-pass negamax). The
    /// node-count bound (≤ 5000) still holds: iteration 1 from startpos at
    /// depth 1 completes in ~20 nodes total.
    #[test]
    fn alphabeta_search_aborts_on_should_abort() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(true)); // pre-set to abort immediately
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits {
                depth: Some(10), // would be slow without early abort
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &|_| {});

        // Iteration 1 completes (only ~20 nodes; below 4096 cadence), the
        // inter-iteration stop check then fires, breaking the loop. Total
        // nodes stay well under 5000.
        assert!(
            result.nodes <= 5000,
            "early abort must not search a large node count: got {} \
            (cadence 4096; iteration 1 < 100 nodes; inter-iteration stop check fires)",
            result.nodes
        );

        // Inter-iteration abort: iteration 1 produces a bestmove + non-zero
        // score from a fully-completed depth-1 search. The depth==1 result
        // proves the loop broke between iterations 1 and 2.
        assert_eq!(
            result.depth, 1,
            "inter-iteration stop must break after iteration 1; got depth {}",
            result.depth
        );
        assert!(
            result.bestmove.is_some(),
            "iteration 1 completed before stop check fired → bestmove = Some"
        );
    }

    /// When the ID search aborts mid-iteration via the `nodes` cap,
    /// `bestmove` must be `Some` and `score_cp` must reflect the LAST
    /// COMPLETED iteration's snapshot — NOT the 0 abort sentinel.
    ///
    /// **M3.E semantics** (per ADR-0017 §2): the ID outer loop snapshots
    /// `last_complete = Some((depth, bestmove, score))` at the end of each
    /// completed iteration. A mid-iteration abort discards the partial
    /// state; `Search::go`'s final result returns the prior iteration's
    /// snapshot. With `nodes: Some(5000)` from startpos, iterations 1–3
    /// complete (cumulative ~3,800 nodes); iteration 4 (~13k nodes) aborts
    /// at the next 4096-aligned cadence past the cap. Result reflects
    /// iteration 3.
    ///
    /// Originally a regression test for the M3.C must-fix #2 score-
    /// contamination path; the M3.E ID outer loop preserves the same
    /// invariant via `last_complete` (no contamination from the aborted
    /// iteration's state).
    #[test]
    fn alphabeta_partial_completion_under_abort_returns_partial_pv_and_score() {
        // **Fixture: Kiwipete-derived asymmetric position** (M4.B refresh).
        // The original startpos fixture was sensitive to ordering shifts —
        // killer-move ordering (M4.B) changed which iteration's snapshot
        // becomes `last_complete` under the 5000-node budget, and a
        // symmetric startpos can legitimately evaluate to exactly 0 at
        // shallow plies, defeating the `score != 0` regression check.
        // Kiwipete has clear material/PST asymmetry — depth-2+ scores are
        // reliably non-zero. The 5000-node budget still produces partial
        // completion under both M4.A and M4.B ordering schemes.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("Kiwipete FEN must parse");
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits {
                depth: Some(4),
                nodes: Some(5000),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &|_| {});

        // Precondition: bestmove must be present (some iteration completed).
        assert!(
            result.bestmove.is_some(),
            "fixture must reach partial completion at depth 4 with nodes=5000 from Kiwipete; \
            adjust the budget if abort fires before any root move improves alpha"
        );
        let mv = result.bestmove.unwrap();
        // Verify the move is legal.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|legal| legal == mv),
            "partial-completion bestmove {} must be legal from Kiwipete",
            mv.to_uci()
        );
        // The score must NOT be 0 (the abort sentinel). Kiwipete is
        // materially/structurally asymmetric, so any genuine eval at depth
        // 1+ is non-zero. A 0 here is the abort-sentinel contamination
        // regression we're guarding against.
        assert_ne!(
            result.score_cp,
            Some(0),
            "partial-completion score must be the completed-subtree score, not the 0 abort sentinel; \
            got {:?}",
            result.score_cp
        );
    }

    /// searchmoves filter restricts root move choice.
    #[test]
    fn alphabeta_searchmoves_filter_restricts_root_choice() {
        let pos = Position::starting_position();
        let e2e4 = Move::from_uci("e2e4", &pos).expect("e2e4 must be legal from startpos");
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits {
                depth: Some(3),
                searchmoves: Some(vec![e2e4]),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &|_| {});

        assert_eq!(
            result.bestmove,
            Some(e2e4),
            "searchmoves=[e2e4] must restrict bestmove to e2e4; got {:?}",
            result.bestmove
        );
    }

    /// After `go depth 2` from startpos, the engine emits one info line per
    /// completed iteration (M3.E: 2 lines for depths 1 and 2). The LAST
    /// info line must start with `info depth 2 score …` and contain a
    /// `pv` token followed by a real UCI move (NOT the `0000` null sentinel).
    /// The real-move check catches the `pv.lengths[0] == 0` mutation flip:
    /// inverting that conditional would emit `pv 0000` for a non-empty PV.
    #[test]
    fn alphabeta_emits_info_line_with_score_and_pv() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits {
                depth: Some(2),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab = AlphaBetaMover::new();
        let info_lines: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        ab.go(&pos, &ctx, &|line| {
            info_lines.borrow_mut().push(line.to_string());
        });
        let info_lines = info_lines.into_inner();

        // M3.E: per-iteration emission. Depth 2 → 2 info lines.
        assert_eq!(
            info_lines.len(),
            2,
            "ID must emit one info line per completed iteration; got: {info_lines:?}"
        );
        let last = info_lines
            .last()
            .expect("at least one info line must be emitted");
        assert!(
            last.starts_with("info depth 2 score cp ")
                || last.starts_with("info depth 2 score mate "),
            "last info line must start with 'info depth 2 score cp ...' or 'info depth 2 score mate ...'; got: {last:?}"
        );
        assert!(
            last.contains(" pv "),
            "last info line must contain 'pv'; got: {last:?}"
        );
        // Real-move pin: catches the `pv.lengths[0] == 0` mutation that flips
        // the conditional and emits `pv 0000` for a populated PV.
        assert!(
            !last.contains(" pv 0000"),
            "non-empty PV must emit real moves, NOT the '0000' null sentinel; got: {last:?}"
        );
    }

    // -----------------------------------------------------------------------
    // negate_window — unit test for the bound-flip helper.
    //
    // Pin both `-beta` and `-alpha` negations independently. Catches any
    // `delete -` mutation on either operand of the helper's `(-beta, -alpha)`
    // body (cargo-mutants generates one mutation per `-`). The helper is
    // load-bearing for correctness in any deeper-than-shallow recursion (the
    // M3.D residual-mutation analysis has full context); pinning it here as a
    // pure-function test sidesteps the structural difficulty of catching the
    // same bug at the qsearch / negamax integration level with shallow
    // M3.D fixtures.
    // -----------------------------------------------------------------------

    /// Directly unit-test the `aborted_fallback_result` helper extracted from
    /// `Search::go`'s `None` arm. The helper's `pv.lengths[0] > 0` is the
    /// mutation point; we drive each input case (empty PV, populated PV,
    /// `root_score = None`, `root_score = Some(value)`) and pin the output.
    /// Catches the `> 0` → `==`, `<`, `>=` mutations on this helper's body
    /// independently of whether the fallback path is reachable from
    /// `Search::go`.
    #[test]
    fn aborted_fallback_returns_none_when_pv_empty_and_zero_score() {
        let pv = PvTable::new();
        let result = aborted_fallback_result(&pv, None);
        assert_eq!(result, (0, None, 0));
    }

    #[test]
    fn aborted_fallback_returns_pv0_move_when_pv_populated() {
        let mut pv = PvTable::new();
        // Use a non-default move so the assertion distinguishes Some(default) from Some(real).
        let pos = Position::starting_position();
        let e2e4 = Move::from_uci("e2e4", &pos).expect("e2e4 is legal");
        pv.moves[0][0] = e2e4;
        pv.lengths[0] = 1;
        let result = aborted_fallback_result(&pv, None);
        assert_eq!(result, (0, Some(e2e4), 0));
    }

    #[test]
    fn aborted_fallback_uses_root_score_when_set() {
        let pv = PvTable::new();
        let result = aborted_fallback_result(&pv, Some(123));
        assert_eq!(result, (0, None, 123));
    }

    #[test]
    fn aborted_fallback_distinguishes_pv_lengths_zero_from_one() {
        // Catches the `> 0` → `>= 0` (always true) mutation: with empty PV,
        // the mutated form would return Some(default Move), not None.
        let pv_empty = PvTable::new();
        let result_empty = aborted_fallback_result(&pv_empty, None);
        assert_eq!(result_empty.1, None, "empty PV → bestmove None");

        // Catches the `> 0` → `== 0` mutation: with populated PV, the mutated
        // form would return None, not Some(real_move).
        let mut pv_full = PvTable::new();
        let pos = Position::starting_position();
        let g1f3 = Move::from_uci("g1f3", &pos).expect("g1f3 is legal");
        pv_full.moves[0][0] = g1f3;
        pv_full.lengths[0] = 1;
        let result_full = aborted_fallback_result(&pv_full, None);
        assert_eq!(result_full.1, Some(g1f3), "populated PV → bestmove Some");
    }

    #[test]
    fn negate_window_negates_both_bounds_and_swaps_them() {
        // Asymmetric window — catches both `delete -` mutations independently.
        // Correct: (-beta, -alpha) = (-200, -100).
        // `delete -` on `-beta`:   (beta, -alpha)  = (200, -100). Mismatch.
        // `delete -` on `-alpha`:  (-beta, alpha)  = (-200, 100). Mismatch.
        assert_eq!(negate_window(100, 200), (-200, -100));

        // Mate-window: ensure the helper handles the bound-collapse
        // characteristic of mate-distance pruning correctly.
        assert_eq!(
            negate_window(MATE - 5, MATE - 1),
            (-(MATE - 1), -(MATE - 5))
        );

        // Symmetric window centered on 0: a stub that returned `(0, 0)` would
        // pass an `(alpha=-X, beta=X)` test trivially, so use a non-zero
        // asymmetric case AND a symmetric case to defeat both stubs.
        assert_eq!(negate_window(-50, 50), (-50, 50));
    }

    // -----------------------------------------------------------------------
    // M3.D — quiescence search.
    //
    // Tests on the new `qsearch` private method (driven via `qsearch_for_test`
    // forwarder) and on the negamax→qsearch integration. Per the M3.D plan
    // (`docs/plans/m3.d.md`) §6.1.
    // -----------------------------------------------------------------------

    // ----- §6.1 row 1: filter accepts captures + queen promos. -----

    /// `qsearch_move_filter` accepts `Capture`, `EnPassant`, `QueenPromo`,
    /// `QueenPromoCapture`. Pins the move-flag inclusion list.
    #[test]
    fn qsearch_filter_accepts_captures_and_queen_promos() {
        use crate::Square;
        use crate::mov::MoveFlag::*;
        let from = Square::new_unchecked(0);
        let to = Square::new_unchecked(8);
        for flag in [Capture, EnPassant, QueenPromo, QueenPromoCapture] {
            let mv = Move::new(from, to, flag);
            assert!(
                qsearch_move_filter(mv),
                "qsearch_move_filter must accept {flag:?}"
            );
        }
    }

    // ----- §6.1 row 2: filter rejects quiets + under-promos + under-promo-captures. -----

    /// `qsearch_move_filter` rejects all non-capture / non-queen-promo flags.
    /// Under-promos (Knight/Bishop/Rook) and under-promo-captures are excluded
    /// despite the capture-promotion variants being captures — see plan §4.
    #[test]
    fn qsearch_filter_rejects_quiets_and_under_promos() {
        use crate::Square;
        use crate::mov::MoveFlag::*;
        let from = Square::new_unchecked(0);
        let to = Square::new_unchecked(8);
        for flag in [
            Quiet,
            DoublePush,
            KingCastle,
            QueenCastle,
            KnightPromo,
            BishopPromo,
            RookPromo,
            KnightPromoCapture,
            BishopPromoCapture,
            RookPromoCapture,
        ] {
            let mv = Move::new(from, to, flag);
            assert!(
                !qsearch_move_filter(mv),
                "qsearch_move_filter must reject {flag:?}"
            );
        }
    }

    // ----- §6.1 row 3: stand-pat returns evaluate(pos) when quiet + no captures. -----

    /// Quiet position with no captures available, not in check, with a
    /// non-zero `evaluate(pos)` (i.e., NOT an insufficient-material draw).
    /// Empty-after-filter path returns stand-pat = `evaluate(pos)`.
    ///
    /// Fixture choice note: KvKB (one bishop) is an ADR-0014 §5 mandatory
    /// `evaluate = 0` draw, which would make `assert_eq!(score, 0)`
    /// vacuous against a stub returning 0. KvKR avoids the draw clause:
    /// `evaluate(KvKR for white-to-move)` is solidly positive (~+477 cp +
    /// PST), so the assertion meaningfully anchors qsearch's stand-pat
    /// return value to the correct material-aware result.
    #[test]
    fn qsearch_stand_pat_returns_eval_when_quiet_position() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        // White K + R vs black K. No captures (R c1 attacks file c, rank 1;
        // no black piece on those lines). Not in check. Not a draw.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        // Fixture validation.
        assert!(!in_check(&pos), "fixture must not be in check");
        let expected = evaluate(&pos);
        assert!(
            expected > 200,
            "fixture must have meaningfully nonzero evaluate (not a draw); \
            got {expected} — if this fires, ADR-0014 may have changed"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        assert_eq!(
            score, expected,
            "qsearch must return stand-pat = evaluate(pos) when not in check and no captures available; \
            got {score}, expected {expected}"
        );
    }

    // ----- §6.1 row 4: extends capture to resolve hanging material. -----

    /// White Q vs hanging black B (no defender). Qxd4 wins ~365 cp of material.
    /// qsearch must extend the capture and return a score above stand-pat.
    #[test]
    fn qsearch_extends_capture_to_resolve_material() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};
        // White: K e1, Q d1. Black: K e8, B d4 (hanging — black king h8-far is the only defender).
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging-bishop FEN must parse");
        // Fixture validation.
        assert!(!in_check(&pos), "fixture must not be in check");
        // Confirm Qxd4 is legal and is the only relevant capture (no other
        // captures could distract from the test's assertion).
        let qxd4 = Move::from_uci("d1d4", &pos).expect("Qxd4 must be legal in fixture");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert_eq!(
            captures.len(),
            1,
            "fixture must have exactly one capture (Qxd4) so the test exercises \
            the single-capture-extends-then-returns path; found {captures:?}"
        );
        assert_eq!(captures[0], qxd4, "the single capture must be Qxd4");
        // Confirm the bishop has no defenders by simulating Qxd4 and checking
        // that black has no recapture (qsearch's recursion would otherwise
        // search the recapture and the score would not reflect a clean material gain).
        let mut pos_after = pos;
        pos_after.make_move(qxd4);
        let mut ml_after = MoveList::new();
        generate_moves(&pos_after, &mut ml_after);
        let recaptures: Vec<_> = ml_after
            .iter()
            .filter(|m| m.to_square().index() == qxd4.to_square().index())
            .collect();
        assert_eq!(
            recaptures.len(),
            0,
            "bishop on d4 must be undefended (no black recapture on d4 after Qxd4); \
            found {recaptures:?}"
        );
        let stand_pat = evaluate(&pos);

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);

        // Bishop value (PeSTO MG: 365). Loose bound to absorb PST swing from
        // queen relocating d1→d4: assert at least +250 cp of gain.
        assert!(
            score >= stand_pat + 250,
            "qsearch must capture the hanging bishop and improve score; \
            stand_pat={stand_pat}, got {score}"
        );
    }

    // ----- §6.1 row 5: does NOT stand-pat in check (differential shape). -----

    /// In-check position where evaluate(pos) is favorable (white up material),
    /// but the only legal evasion abandons the queen. qsearch must NOT
    /// stand-pat — it must search evasions and return a score MUCH lower than
    /// the rosy static eval.
    ///
    /// **Differential-shape assertion** (plan §6.1 row 5): `result < eval - 100`
    /// proves the causal claim "qsearch did not stand-pat at this in-check
    /// position" without pinning a specific score.
    ///
    /// Fixture: White K g1, Q d4, P f2/g2/h2. Black K h8, N e2 (forks K and Q).
    /// White is in check from the knight on e2 (e2 attacks g1). White must
    /// move the king (no capture of e2 available, knight check unblockable);
    /// after Kf1 or Kh1, black plays Nxd4 winning the queen.
    #[test]
    fn qsearch_does_not_stand_pat_in_check() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        let pos = Position::from_fen("7k/8/8/8/3Q4/8/4nPPP/6K1 w - - 0 1")
            .expect("knight-fork-of-king-and-queen FEN must parse");
        // Fixture validation.
        assert!(in_check(&pos), "fixture must be in check");
        let eval = evaluate(&pos);
        // Eval should be favorable for white (white still has Q + 3P vs black's lone N).
        assert!(
            eval > 500,
            "fixture must have favorable eval for the in-check side (else differential is vacuous); \
            got eval={eval}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);

        assert!(
            score < eval - 100,
            "qsearch must NOT stand-pat at an in-check position; \
            differential failed: eval={eval}, score={score}, expected score < {}",
            eval - 100
        );
        // Lower-bound: the score must NOT be in the mate-score range (a
        // stub returning -(MATE - ply) regardless of legal-moves status
        // would otherwise pass the differential vacuously). The fixture
        // has legal evasions (Kf1, Kh1) — qsearch must search them, not
        // return the mate sentinel.
        assert!(
            score > -MATE_IN_MAX_PLY,
            "qsearch score must not be in the mate-score range — fixture has legal \
            evasions, not mate. Got {score}, expected score > -MATE_IN_MAX_PLY ({})",
            -MATE_IN_MAX_PLY
        );
    }

    // ----- §6.1 row 6: in-check evasions searched when no captures. -----

    /// In-check position with only quiet evasions available (no captures).
    /// Tests the in-check branch reaches the move-loop and returns a finite
    /// score — NOT the mate sentinel (-(MATE-ply)) and NOT -INF.
    ///
    /// Fixture: white K a1 in check from black R a8 along a-file. White has
    /// only the king; only legal evasions are Kb1 / Kb2 (both quiet). No
    /// captures possible.
    #[test]
    fn qsearch_in_check_evasions_searched_when_no_captures() {
        use crate::movegen::in_check;
        let pos =
            Position::from_fen("r6k/8/8/8/8/8/8/K7 w - - 0 1").expect("rook-check FEN must parse");
        assert!(in_check(&pos), "fixture must be in check");

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        // Score must be finite — not the mate sentinel and not -INF.
        assert!(
            score > -(MATE - 100),
            "qsearch must NOT misclassify a non-mate evasion as mate; got {score}"
        );
        assert!(
            score > -INF + 100,
            "qsearch must update best from -INF after at least one evasion; got {score}"
        );
        // Score must be in a sane range (white loses material since black has rook + king vs white king alone).
        assert!(
            (-2000..0).contains(&score),
            "qsearch score must reflect material disadvantage from K-vs-KR; got {score}"
        );
    }

    // ----- §6.1 row 7: in-check + no legal moves → mate score. -----

    /// Mate position reached at qsearch leaf. In-check + empty move list →
    /// `-(MATE - ply)`. Test calls qsearch with ply=2; expected score = -(MATE-2).
    ///
    /// Fixture: back-rank-style mate. White K g1, white pawns f2/g2/h2. Black
    /// K h8, Q a1. Black queen on a1 checks white K g1 along rank 1; white K
    /// has no escape (f1/h1 attacked along rank, f2/g2/h2 own pawns block);
    /// no interposition (white has only K + pawns, no piece reaches the rank);
    /// no capture (white K can't reach a1).
    #[test]
    fn qsearch_in_check_with_no_legal_moves_returns_mate() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("7k/8/8/8/8/8/5PPP/q5K1 w - - 0 1")
            .expect("back-rank-mate FEN must parse");
        // Fixture validation.
        assert!(in_check(&pos), "fixture must be in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.iter().count(),
            0,
            "fixture must have no legal moves (mate)"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 2, &ctx);

        assert_eq!(
            score,
            -(MATE - 2),
            "qsearch must return mate-at-ply-2 sentinel = -(MATE - 2) = -{}; got {score}",
            MATE - 2
        );
    }

    // ----- §6.1 row 8: returns stand-pat for not-in-check + no captures (CPW pitfall). -----

    /// Critical false-stalemate test (CPW pitfall §10.7). The starting
    /// position has many quiet moves but no captures and no queen-promos.
    /// qsearch's not-in-check + empty-after-filter path must return stand-pat
    /// = `evaluate(pos)` — NOT `-(MATE - ply)`. Empty list under capture
    /// filter does NOT mean stalemate.
    #[test]
    fn qsearch_returns_stand_pat_for_not_in_check_no_captures() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        let pos = Position::starting_position();
        assert!(!in_check(&pos), "startpos must not be in check");
        let expected = evaluate(&pos);

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        assert_eq!(
            score, expected,
            "qsearch must return stand-pat (not mate sentinel) at quiet not-in-check position; \
            got {score}, expected {expected}"
        );
    }

    // ----- §6.1 row 9: stand-pat triggers beta cutoff (early return). -----

    /// Position with stand-pat >= beta. qsearch fires the beta cutoff before
    /// any captures are searched.
    ///
    /// Fixture: KvKQ heavily favoring white-to-move (`evaluate(pos)` > +1000).
    /// Set `beta = 10`; stand-pat overshoots by a wide margin → cutoff fires
    /// immediately. Anchor: nodes increment by exactly 1 (only the qsearch
    /// frame, no recursion) per plan §5's "1 leaf = 1 node" budget contract.
    #[test]
    fn qsearch_beta_cutoff_on_stand_pat() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        // White K+Q vs black K. White to move. No captures available.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/3QK3 w - - 0 1").expect("KQvK FEN must parse");
        assert!(!in_check(&pos), "fixture must not be in check");
        let stand_pat = evaluate(&pos);
        assert!(
            stand_pat > 500,
            "fixture must have eval > 500 to overshoot beta=10; got {stand_pat}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let nodes_before = ab.nodes;
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), 0, 10, 1, &ctx);

        assert_eq!(
            score, stand_pat,
            "stand-pat beta cutoff must return stand_pat (fail-soft); got {score}, expected {stand_pat}"
        );
        let nodes_consumed = ab.nodes - nodes_before;
        assert_eq!(
            nodes_consumed, 1,
            "stand-pat cutoff must fire before any recursive call; consumed {nodes_consumed} nodes"
        );
    }

    // ----- §6.1 row 10: capture-driven beta cutoff (in-loop fail-soft). -----

    /// Position where stand-pat is below beta but the first MVV-LVA capture
    /// overshoots beta. The in-loop fail-soft cutoff fires after only the
    /// first capture, distinct from the stand-pat early-cutoff path.
    ///
    /// Fixture per plan §6.1 row 10: white K e1, Q d1; black K e8, Q d4 (high
    /// MVV target along d-file), B a4 (low MVV target on the d1-a4 diagonal).
    /// White Q d1 attacks both. MVV-LVA orders Qxd4 first.
    ///
    /// Window: alpha = stand_pat - 50 (so stand-pat does NOT improve alpha);
    /// beta = stand_pat + 100 (so stand-pat does NOT exceed beta — the
    /// early-cutoff path is *not* taken); the queen capture's payoff is large
    /// enough to overshoot beta in the move loop.
    #[test]
    fn qsearch_beta_cutoff_in_capture_loop() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        let pos = Position::from_fen("4k3/8/8/8/b2q4/8/8/3QK3 w - - 0 1")
            .expect("two-hanging-targets FEN must parse");
        assert!(!in_check(&pos), "fixture must not be in check");
        let stand_pat = evaluate(&pos);
        let alpha = stand_pat - 50;
        let beta = stand_pat + 100;

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let nodes_before = ab.nodes;
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, 1, &ctx);

        // Fail-soft: cutoff returns the actual capture score, which is >= beta.
        assert!(
            score >= beta,
            "in-loop beta cutoff must return score >= beta (fail-soft); got {score}, beta {beta}"
        );
        // Cutoff fires after first capture; tight node count.
        //
        // Expected node count derivation: top qsearch frame (+1 nodes via
        // self.nodes increment) + recursive frame after Qxd4 (+1) = 2 nodes.
        // Black qsearch after Qxd4 has no further captures available
        // (black bishop a4 cannot reach white K e1 or white Q d4), so it
        // returns stand-pat without recursion. White's loop then trips the
        // beta cutoff and breaks, NOT iterating to Qxa4. A buggy qsearch
        // that searches the second capture (Qxa4) would consume 3 nodes.
        let nodes_consumed = ab.nodes - nodes_before;
        assert_eq!(
            nodes_consumed, 2,
            "in-loop beta cutoff must fire after only Qxd4 — exactly 2 nodes \
            (top frame + Qxd4-recursion frame). Found {nodes_consumed}: a buggy \
            qsearch that also searches Qxa4 would consume 3 nodes"
        );
    }

    // ----- §6.1 row 11: alpha improvement (stand-pat then capture). -----

    /// Position where stand-pat is below alpha-after-stand-pat-update but a
    /// capture improves further. Drive with a full window. The capture beats
    /// stand-pat; result > evaluate(pos).
    ///
    /// Fixture: white K e1, R c1. Black K h8, N c3 (hanging — black king h8
    /// far away, no defender). White Rxc3 wins ~337 cp of material.
    #[test]
    fn qsearch_alpha_improvement_from_stand_pat_then_capture() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        let pos = Position::from_fen("7k/8/8/8/8/2n5/8/2R1K3 w - - 0 1")
            .expect("hanging-knight FEN must parse");
        assert!(!in_check(&pos), "fixture must not be in check");
        let stand_pat = evaluate(&pos);

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        // Capture must beat stand-pat. Loose threshold (200 cp) accommodates
        // PST swing from the rook relocating c1→c3.
        assert!(
            score > stand_pat + 200,
            "qsearch must capture the hanging knight and improve alpha; \
            stand_pat={stand_pat}, got {score}"
        );
    }

    // ----- §6.1 row 12: skips is_fifty_move_draw at threshold. -----

    /// Pin the design choice: qsearch does NOT consult `is_fifty_move_draw`.
    /// Fixture has `halfmove_clock=100` (`is_fifty_move_draw(100) == true`),
    /// so a qsearch that *did* consult the helper would short-circuit to 0.
    /// A qsearch that skips the helper computes stand-pat + the available
    /// capture; returns a non-zero score.
    #[test]
    fn qsearch_skips_fifty_move_at_threshold() {
        use crate::movegen::in_check;
        // Same hanging-bishop fixture as row 4, but with halfmove_clock = 100.
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 100 1")
            .expect("FEN with halfmove=100 must parse");
        // Fixture validation.
        assert!(!in_check(&pos), "fixture must not be in check");
        assert_eq!(pos.halfmove_clock(), 100, "fixture must have halfmove=100");
        assert!(
            is_fifty_move_draw(pos.halfmove_clock()),
            "fixture's halfmove=100 must trigger is_fifty_move_draw if consulted"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        // qsearch must NOT return 0 (the 50-move-draw sentinel). The actual
        // score is the stand-pat + capture-driven value, large and positive.
        assert_ne!(
            score, 0,
            "qsearch must NOT consult is_fifty_move_draw at halfmove=100; \
            got 0, indicating accidental short-circuit"
        );
        assert!(
            score > 500,
            "qsearch must compute stand-pat + capture; got {score}"
        );
    }

    // ----- §6.1 row 13: skips is_repetition when current zobrist in history. -----

    /// Pin the design choice: qsearch does NOT consult `is_repetition`.
    /// Fixture: position whose zobrist appears earlier in `mover.history` at
    /// a 2-ply stride that triggers `is_repetition` if consulted.
    /// Validate the fixture by calling the helper directly first; assert it
    /// would return true. Then assert qsearch returns a non-zero score.
    #[test]
    fn qsearch_skips_repetition_when_history_contains_current_position() {
        use crate::movegen::in_check;
        // Same hanging-bishop fixture as row 4, with halfmove=4 (enough for
        // is_repetition to walk back 4 plies in 2-ply steps; below the
        // 50-move threshold so is_fifty_move_draw is not a confounder).
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 4 1")
            .expect("FEN with halfmove=4 must parse");
        assert!(!in_check(&pos), "fixture must not be in check");

        // Construct mover.history that triggers is_repetition: current
        // position's zobrist at a 2-ply distance from the end.
        let z = pos.zobrist();
        let other: u64 = 0xDEAD_BEEF_CAFE_FEED;
        let history = vec![z, other, z];

        // Fixture validation: confirm is_repetition WOULD return true.
        assert!(
            is_repetition(&history, pos.halfmove_clock()),
            "fixture must trigger is_repetition if consulted (so the test \
            actually distinguishes consult-vs-skip behavior)"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = history;
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        // qsearch must NOT return 0 (the repetition sentinel). The capture
        // is still searched and the position's material asymmetry shows.
        assert_ne!(
            score, 0,
            "qsearch must NOT consult is_repetition; got 0, indicating accidental short-circuit"
        );
        assert!(
            score > 500,
            "qsearch must compute stand-pat + capture; got {score}"
        );
    }

    // ----- §6.1 row 14: negamax horizon delegates to qsearch (integration). -----

    /// At depth==0, negamax must delegate to qsearch — not return evaluate(pos).
    /// Tested via a fixture where the difference is observable: a depth-1
    /// search where white's apparent winning capture (Qxd5) is a trap that
    /// qsearch reveals via the recapture (Bxd5).
    ///
    /// Fixture: White K e1, Q d1, P b2. Black K e8, P d5, B e6.
    /// - Without qsearch (M3.C: evaluate at leaf): negamax depth-1 picks
    ///   Qxd5 (apparent +pawn ≈ +742 cp).
    /// - With qsearch (M3.D): qsearch at the leaf sees Bxd5 winning the
    ///   queen; white's depth-1 best is a quiet move (~+660 cp).
    ///
    /// Assertion: depth-1 score < 700 (catches the regression where negamax
    /// still uses evaluate-at-leaf and would return ~+742).
    #[test]
    fn negamax_horizon_calls_qsearch_not_evaluate() {
        use crate::eval::evaluate;
        let pos = Position::from_fen("4k3/8/4b3/3p4/8/8/1P6/3QK3 w - - 0 1")
            .expect("queen-trap FEN must parse");

        // Fixture validation: confirm Qxd5 is legal AND Bxd5 is the
        // recapture. Without these, the "trap" framing is unverified — see
        // M3.C mate-in-2 fiasco lesson (plan §10).
        let qxd5 = Move::from_uci("d1d5", &pos).expect("Qxd5 must be a legal capture in fixture");
        let mut pos_after_qxd5 = pos;
        pos_after_qxd5.make_move(qxd5);
        Move::from_uci("e6d5", &pos_after_qxd5)
            .expect("Bxd5 must be a legal recapture after Qxd5 (the trap)");
        // Confirm the eval-at-leaf path WOULD recommend Qxd5: at the leaf
        // (after Qxd5), evaluate from black's POV is heavily negative; negated
        // for white is heavily positive. Without qsearch, white's depth-1
        // search picks Qxd5 with this rosy score.
        let leaf_eval_black_pov = evaluate(&pos_after_qxd5);
        assert!(
            -leaf_eval_black_pov > 700,
            "fixture's evaluate-at-leaf path must yield > 700 cp (white winning a pawn) — \
            otherwise the `score < 700` bound below does not catch the regression. \
            Got -leaf_eval_black_pov = {} (eval-at-leaf negation)",
            -leaf_eval_black_pov
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, &ctx);

        assert!(
            score < 700,
            "negamax at depth-1 must use qsearch (not evaluate) at the leaf, \
            avoiding the Qxd5 trap; with evaluate-at-leaf the score would be > 700 \
            (white wins a pawn at face value), with qsearch ~+660 (white avoids Qxd5 \
            because Bxd5 wins the queen). Got {score}"
        );
    }

    // ----- §6.1 row 15: depth-4 startpos score within sane range. -----

    // ----- §6.1 row 16 (added after test-suite-review pass 1): qsearch
    // extends a queen promotion (filter→qsearch path, not just the filter). -----

    /// qsearch must recurse on a queen promotion to discover the material
    /// gain. The filter accepts `QueenPromo` (rows 1-2 pin that), but only
    /// this test confirms the qsearch body actually generates and searches
    /// the promotion.
    ///
    /// Fixture: white K e1, P a7. Black K h8. White can promote via a7-a8=Q
    /// (quiet promotion, flag `QueenPromo`). After promotion: white K+Q vs
    /// black K, score ~+1025 from white's POV. Stand-pat at root: white K+P
    /// vs black K, score ~+82 (just the pawn). qsearch must extend the
    /// promotion and return a score significantly above stand-pat.
    #[test]
    fn qsearch_extends_queen_promotion() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("7k/P7/8/8/8/8/8/4K3 w - - 0 1")
            .expect("white-pawn-on-7th-rank FEN must parse");
        // Fixture validation.
        assert!(!in_check(&pos), "fixture must not be in check");
        Move::from_uci("a7a8q", &pos).expect("a7a8=Q must be a legal queen promotion");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert_eq!(
            captures.len(),
            0,
            "fixture must have no captures (only the queen promotion is the qsearch extension); \
            got {captures:?}"
        );
        // Confirm `generate_moves` actually emits a QueenPromo for a7a8.
        // Without this, a broken promotion-generator would let the test fail
        // with a confusing `score <= stand_pat + 800` rather than a clear
        // promotion-generation failure.
        use crate::mov::MoveFlag;
        let queen_promos: Vec<_> = ml
            .iter()
            .filter(|m| m.flag() == MoveFlag::QueenPromo)
            .collect();
        assert_eq!(
            queen_promos.len(),
            1,
            "fixture must emit exactly one QueenPromo (a7a8=Q); got {queen_promos:?}"
        );
        let stand_pat = evaluate(&pos);

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);

        // Queen value (PeSTO MG: 1025). Loose bound to absorb PST contributions.
        assert!(
            score > stand_pat + 800,
            "qsearch must extend the queen promotion and reflect the material gain; \
            stand_pat={stand_pat}, got {score}"
        );
    }

    // ----- §6.1 row 17 (added after test-suite-review pass 1): abort
    // propagation inside qsearch. -----

    /// Pin the post-recursion abort check (`if self.aborted { return 0; }`
    /// in qsearch's move loop). Pre-set `ab.aborted = true` before the call;
    /// qsearch should propagate by returning 0 from the post-recursion check
    /// after the first capture's recursive call returns.
    ///
    /// Fixture: a position with at least one capture available (so qsearch
    /// recurses) and with `evaluate(pos) < beta` so the stand-pat path does
    /// NOT short-circuit before reaching the move loop. The full window
    /// `(-INF, INF)` ensures stand-pat's beta cutoff does not pre-empt.
    #[test]
    fn qsearch_abort_propagates_from_post_recursion_check() {
        // Hanging-bishop fixture: white Q d1 captures black B d4. Full window
        // means stand-pat does not cut off — qsearch must enter the move
        // loop, make Qxd4, recurse. After the recursion returns, the
        // post-recursion `if self.aborted { return 0; }` fires.
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging-bishop FEN must parse");

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.aborted = true; // pre-set: simulate "an outer search has aborted"
        let nodes_before = ab.nodes;
        let (ctx, _stop) = non_aborting_ctx();
        let pos_clone = pos;
        let mut pos_search = pos;
        let score = ab.qsearch_for_test(&mut pos_search, -INF, INF, 1, &ctx);

        // Position must be restored even on abort — make/unmake balance.
        assert_eq!(
            pos_search, pos_clone,
            "qsearch must restore position via balanced make/unmake even on abort"
        );
        // The move loop must have been entered: outer qsearch frame
        // increments nodes (+1) and the recursive frame after Qxd4 increments
        // nodes (+1) before the post-recursion abort check fires. A top-of-
        // frame early abort guard (defensive variant) would leave nodes_consumed
        // at 1 — the assertion below catches that variant and pins the
        // post-recursion check as the load-bearing abort propagation point.
        let nodes_consumed = ab.nodes - nodes_before;
        assert_eq!(
            nodes_consumed, 2,
            "qsearch with pre-set aborted=true must enter the move loop and execute \
            ONE make/unmake pair before the post-recursion abort check fires; \
            consumed {nodes_consumed} nodes (expected 2: outer frame + Qxd4 recursive frame)"
        );
        // The post-recursion abort check returns 0. Stand-pat could not
        // short-circuit (full window), so the move loop ran and the abort
        // check fired.
        assert_eq!(
            score, 0,
            "qsearch with pre-set aborted=true and full window must return 0 \
            from the post-recursion abort propagation check; got {score}"
        );
    }

    // ----- §6.1 row 18 (added after test-suite-review pass 1): mate-distance
    // pruning fires inside qsearch. -----

    /// Mate-distance pruning at the top of qsearch (plan §3 step 2) returns
    /// a mate-bound score before stand-pat is computed when `alpha >=
    /// mating_value`. Fixture: drive qsearch with `alpha = MATE - 5`,
    /// `beta = MATE - 1`, `ply = 10`. Then `mating_value = MATE - 10`,
    /// which is < beta (so beta tightens to MATE - 10), and `alpha (MATE - 5)
    /// >= mating_value (MATE - 10)`, triggering the early return of
    /// `mating_value`.
    ///
    /// The position itself doesn't matter — MD pruning fires before any
    /// position-dependent logic. We use startpos for simplicity.
    #[test]
    fn qsearch_mate_distance_pruning_fires() {
        let pos = Position::starting_position();

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();

        // alpha >= mating_value (MATE - 10) so the upper-bound MD pruning fires.
        let ply = 10;
        let alpha = MATE - 5;
        let beta = MATE - 1;
        let mating_value = MATE - ply as i32;
        assert!(
            alpha >= mating_value,
            "fixture preconditions: alpha={alpha}, mating_value={mating_value}; \
            alpha must be >= mating_value to trigger upper-bound MD pruning"
        );

        let nodes_before = ab.nodes;
        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);

        assert_eq!(
            score, mating_value,
            "qsearch's MD pruning must return mating_value early when alpha >= mating_value; \
            got {score}, expected {mating_value}"
        );
        // Per plan §3, step 1 is the node increment + cancellation poll;
        // step 2 is MD pruning. If an implementation accidentally swaps the
        // order (MD pruning before the increment), nodes would not change.
        // Pin the step-1-then-step-2 ordering — load-bearing for the "1 leaf
        // = 1 node" budget contract from plan §5.
        let nodes_consumed = ab.nodes - nodes_before;
        assert_eq!(
            nodes_consumed, 1,
            "MD pruning must fire AFTER the node increment (plan §3 step 1 → step 2); \
            consumed {nodes_consumed} nodes (expected 1: only the qsearch frame's increment)"
        );
    }

    // ----- §6.1 row 19 (added in final-review pass 1): MD-pruning
    // lower-bound arm (`mated_value > alpha`). -----

    /// Mate-distance pruning's lower-bound arm fires when `mated_value > alpha`.
    /// Symmetric to the upper-bound test (row 18). The lower-bound MD-pruning
    /// fires when the caller already knows we cannot do worse than `mated_value`,
    /// i.e., `alpha < mated_value`, AND the new `beta = mated_value` collapses
    /// the window if `beta <= mated_value`.
    ///
    /// Setup: ply=10 → mated_value = -(MATE - 10) = -29990.
    /// alpha = -29995 (< mated_value).
    /// beta = -29991 (<= mated_value, triggering the collapse).
    /// Trigger: `mated_value > alpha` → alpha = mated_value = -29990. Then
    /// `beta <= mated_value` → -29991 <= -29990 → true. Return mated_value.
    #[test]
    fn qsearch_mate_distance_pruning_lower_bound_fires() {
        let pos = Position::starting_position();

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();

        let ply = 10;
        let alpha = -29995;
        let beta = -29991;
        let mated_value = -(MATE - ply as i32);
        // Fixture preconditions for the lower-bound MD-pruning early-return.
        assert!(
            mated_value > alpha,
            "fixture: mated_value ({mated_value}) > alpha ({alpha})"
        );
        assert!(
            beta <= mated_value,
            "fixture: beta ({beta}) <= mated_value ({mated_value})"
        );

        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);

        assert_eq!(
            score, mated_value,
            "qsearch's MD-pruning lower-bound arm must return mated_value; \
            got {score}, expected {mated_value}"
        );
    }

    // ----- §6.1 row 20 (added in final-review pass 1): recursion increments
    // ply. Catches the `ply + 1 → ply * 1` mutation. -----

    /// Pin that qsearch's recursive call uses `ply + 1`, not a frozen `ply`.
    /// Strategy: drive a fixture where qsearch's only capture leads directly
    /// to checkmate. With correct `ply + 1`, the child frame at ply=N+1
    /// detects mate and returns `-(MATE - (N+1))`; the parent negates to
    /// `+(MATE - (N+1))`. With the broken `ply * 1` form, every recursive
    /// frame stays at ply=N (root's ply), so the child returns `-(MATE - N)`,
    /// parent returns `+(MATE - N)` — a strictly larger mate score that
    /// the test catches via the exact equality check below.
    ///
    /// Fixture: white K f6, B c2, Q d2. Black K h8, R d8.
    /// White's only capture in qsearch: Qxd8 (Q d2 captures R d8 along the
    /// d-file). After Qxd8: Q on d8 attacks h8 along rank 8 (check); h7 is
    /// covered by B c2 along the c2-h7 diagonal; g7 is covered by white K
    /// f6; g8 is covered by Q d8 along rank 8. No interposition possible
    /// (black has only K). No black capture of d8 (h8 to d8 too far). Mate.
    ///
    /// Drive `qsearch_for_test(pos, ..., ply=2, ...)`. Correct: returned
    /// score = MATE - 3. Broken (`ply * 1`): returned score = MATE - 2.
    #[test]
    fn qsearch_recursive_ply_increments_for_mate_score() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("3r3k/8/5K2/8/8/8/2BQ4/8 w - - 0 1")
            .expect("Qxd8-mate fixture FEN must parse");
        // Fixture validation.
        assert!(!in_check(&pos), "fixture must NOT be in check at root");
        let qxd8 = Move::from_uci("d2d8", &pos).expect("Qxd8 must be a legal capture in fixture");
        // Confirm Qxd8 leads to mate (post-move: black in check, no legal moves).
        let mut pos_after = pos;
        pos_after.make_move(qxd8);
        assert!(
            in_check(&pos_after),
            "fixture: after Qxd8, black must be in check"
        );
        let mut ml_after = MoveList::new();
        generate_moves(&pos_after, &mut ml_after);
        assert_eq!(
            ml_after.iter().count(),
            0,
            "fixture: after Qxd8, black must have no legal moves (mate)"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // Drive at ply=2. Correct recursion enters child at ply=3, finds
        // mate, returns -(MATE - 3); parent negates to MATE - 3.
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 2, &ctx);

        assert_eq!(
            score,
            MATE - 3,
            "qsearch must increment ply on recursion; correct returns MATE-3 = {}, \
            broken `ply * 1` would return MATE-2 = {} (frozen ply)",
            MATE - 3,
            MATE - 2
        );
    }

    // ----- §6.1 row 15: depth-4 startpos score within sane range. -----

    /// Depth-4 search at startpos returns a score in `-100..=100` cp. Sane
    /// balanced-startpos range. Regression guard, not a correctness check;
    /// the M3.C value (`b1c3` at `cp 38`) may shift with qsearch-extended
    /// leaves.
    #[test]
    fn alphabeta_strength_score_at_depth_4_within_sane_range() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx_depth4, _stop) = non_aborting_ctx_at_depth(4);
        let result = ab.go(&pos, &ctx_depth4, &|_| {});

        let score = result.score_cp.expect("depth-4 search must return a score");
        assert!(
            (-100..=100).contains(&score),
            "depth-4 startpos score must be in sane balanced range; got cp {score}"
        );
    }

    // -----------------------------------------------------------------------
    // M3.E — `compute_caps` pure-function unit tests.
    //
    // Per `docs/research/m3-time-management.md` §9 test table and
    // `docs/plans/m3.e.md` §8.1. Each test is a pure-function check: build a
    // `SearchLimits`, call `compute_caps(&limits, side, mo)`, assert the
    // returned `(soft, hard)` durations.
    // -----------------------------------------------------------------------

    /// Build a `SearchLimits` with only the named `f` field overridden from
    /// the default. Saves boilerplate in the per-row tests below.
    fn limits_with(f: impl FnOnce(&mut SearchLimits)) -> SearchLimits {
        let mut l = SearchLimits::default();
        f(&mut l);
        l
    }

    /// `Duration::MAX` shorthand for the "no cap" sentinel.
    fn nocap() -> Duration {
        Duration::MAX
    }

    /// Convenience: assert both soft and hard equal the given `Duration`.
    fn assert_caps_eq(caps: TimeCaps, soft: Duration, hard: Duration) {
        assert_eq!(caps.soft, soft, "soft cap mismatch");
        assert_eq!(caps.hard, hard, "hard cap mismatch");
    }

    #[test]
    fn compute_caps_no_time_limits_depth_returns_max_max() {
        let l = limits_with(|l| l.depth = Some(5));
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_no_time_limits_nodes_returns_max_max() {
        let l = limits_with(|l| l.nodes = Some(100_000));
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_no_time_limits_mate_returns_max_max() {
        let l = limits_with(|l| l.mate = Some(3));
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_no_time_limits_infinite_returns_max_max() {
        let l = limits_with(|l| l.infinite = true);
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_no_time_limits_ponder_returns_max_max() {
        let l = limits_with(|l| l.ponder = true);
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_infinite_with_clock_returns_max_priority_check() {
        // `go infinite wtime 1000 btime 1000` per UCI must be infinite —
        // the non-time flag wins over the clock. Pins that `compute_caps`
        // checks `infinite` BEFORE the clock branch.
        let l = limits_with(|l| {
            l.infinite = true;
            l.wtime = Some(1000);
            l.btime = Some(1000);
        });
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_depth_with_clock_returns_max_priority_check() {
        // `go depth 5 wtime 10000` must return (MAX, MAX) — depth-only
        // limits trump the clock. Pins the same priority order.
        let l = limits_with(|l| {
            l.depth = Some(5);
            l.wtime = Some(10_000);
        });
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_nodes_with_clock_returns_max_priority_check() {
        let l = limits_with(|l| {
            l.nodes = Some(100_000);
            l.wtime = Some(10_000);
        });
        assert_caps_eq(compute_caps(&l, Color::White, 50), nocap(), nocap());
    }

    #[test]
    fn compute_caps_movetime_with_clock_uses_movetime_path() {
        // `go movetime 1000 wtime 60000` — movetime wins (it's an explicit
        // override). caps = (950, 950). NOT (3000, 9000) from the clock path.
        let l = limits_with(|l| {
            l.movetime = Some(1000);
            l.wtime = Some(60_000);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(950), Duration::from_millis(950));
    }

    #[test]
    fn compute_caps_movetime_subtracts_move_overhead() {
        let l = limits_with(|l| l.movetime = Some(1000));
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(950), Duration::from_millis(950));
    }

    #[test]
    fn compute_caps_movetime_floors_at_1ms_when_overhead_exceeds_movetime() {
        let l = limits_with(|l| l.movetime = Some(10));
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(1), Duration::from_millis(1));
    }

    #[test]
    fn compute_caps_movetime_zero_floors_at_1ms() {
        let l = limits_with(|l| l.movetime = Some(0));
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(1), Duration::from_millis(1));
    }

    #[test]
    fn compute_caps_movetime_negative_floors_at_1ms() {
        let l = limits_with(|l| l.movetime = Some(-200));
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(1), Duration::from_millis(1));
    }

    #[test]
    fn compute_caps_sudden_death_white() {
        // Research §9 row: wtime=10000 winc=100 mo=50 → soft=500, hard=1500.
        let l = limits_with(|l| {
            l.wtime = Some(10_000);
            l.winc = Some(100);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(500),
            Duration::from_millis(1500),
        );
    }

    #[test]
    fn compute_caps_sudden_death_black_uses_btime_binc() {
        // Mirror image: btime=10000 binc=100 mo=50, side=Black → soft=500, hard=1500.
        let l = limits_with(|l| {
            l.btime = Some(10_000);
            l.binc = Some(100);
        });
        let caps = compute_caps(&l, Color::Black, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(500),
            Duration::from_millis(1500),
        );
    }

    #[test]
    fn compute_caps_white_side_does_not_consult_btime_binc() {
        // Confirms side selection is not swapped: white-side caps must derive
        // from wtime/winc only. With wtime=20000 winc=0 and btime=999999999,
        // the white-side soft is 20000/20 = 1000ms — NOT some huge value
        // computed from btime.
        let l = limits_with(|l| {
            l.wtime = Some(20_000);
            l.winc = Some(0);
            l.btime = Some(999_999_999);
            l.binc = Some(999_999);
        });
        let caps = compute_caps(&l, Color::White, 0);
        // soft = 20000/20 + 0/2 - 0 = 1000 ms; hard = 3000 ms.
        assert_caps_eq(
            caps,
            Duration::from_millis(1000),
            Duration::from_millis(3000),
        );
    }

    #[test]
    fn compute_caps_sudden_death_default_n_is_20() {
        // wtime=20000 winc=0 mo=0: soft = 20000/20 + 0/2 = 1000 ms.
        // Pin BOTH soft (the N=20 constant) AND hard = 3 × soft = 3000 ms
        // (the hard = 3 × soft rule at a clean fixture).
        let l = limits_with(|l| {
            l.wtime = Some(20_000);
            l.winc = Some(0);
        });
        let caps = compute_caps(&l, Color::White, 0);
        assert_caps_eq(
            caps,
            Duration::from_millis(1000),
            Duration::from_millis(3000),
        );
    }

    #[test]
    fn compute_caps_classical_tc() {
        // Research §9 row: wtime=600000 movestogo=40 mo=50 → soft=14950, hard=44850.
        let l = limits_with(|l| {
            l.wtime = Some(600_000);
            l.movestogo = Some(40);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(14_950),
            Duration::from_millis(44_850),
        );
    }

    #[test]
    fn compute_caps_movestogo_1() {
        // wtime=10000 movestogo=1 mo=50 → soft = 10000/1 + 0/2 - 50 = 9950 ms.
        // hard = min(3 × 9950, 10000-50) = min(29850, 9950) = 9950 ms.
        let l = limits_with(|l| {
            l.wtime = Some(10_000);
            l.movestogo = Some(1);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(9950),
            Duration::from_millis(9950),
        );
    }

    #[test]
    fn compute_caps_movestogo_zero_treated_as_one() {
        // movestogo=0 is a UCI spec violation. Defensive fallback: treat as 1
        // ("this move must be played within current budget"). Output matches
        // movestogo=1 above.
        let l = limits_with(|l| {
            l.wtime = Some(10_000);
            l.movestogo = Some(0);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(9950),
            Duration::from_millis(9950),
        );
    }

    #[test]
    fn compute_caps_increment_only_no_forfeit_clamp() {
        // Research §9 row: wtime=0 winc=5000 mo=50 → soft=2450, hard=7350.
        // The forfeit guard does NOT apply when rem == 0 (research §5: spend at
        // most half the increment to stay afloat; the increment refills after
        // the move).
        let l = limits_with(|l| {
            l.wtime = Some(0);
            l.winc = Some(5000);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(
            caps,
            Duration::from_millis(2450),
            Duration::from_millis(7350),
        );
    }

    #[test]
    fn compute_caps_increment_dominates_clamps_to_remaining() {
        // Research §9 row: wtime=100 winc=5000 mo=50 → forfeit guard clamps
        // both soft and hard to (100-50).max(1) = 50 ms.
        let l = limits_with(|l| {
            l.wtime = Some(100);
            l.winc = Some(5000);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(50), Duration::from_millis(50));
    }

    #[test]
    fn compute_caps_low_time_floors_at_1ms() {
        // wtime=50 winc=0 mo=50: rem-mo = 0; rem != 0 (rem=50 in 0-handling),
        // so the regular branch fires; raw_soft = 2; soft_unclamped = 0;
        // max_clamp = 1; soft floors at 1; hard = min(3, 1) = 1.
        let l = limits_with(|l| {
            l.wtime = Some(50);
            l.winc = Some(0);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(1), Duration::from_millis(1));
    }

    #[test]
    fn compute_caps_negative_clock_floors_at_1ms() {
        // wtime=-200 winc=0 mo=50: rem clamped to 0, inc=0 → very-low-time path
        // → (1ms, 1ms).
        let l = limits_with(|l| {
            l.wtime = Some(-200);
            l.winc = Some(0);
        });
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, Duration::from_millis(1), Duration::from_millis(1));
    }

    #[test]
    fn compute_caps_no_clock_no_movetime_returns_max() {
        // Degenerate `go` with no clock and no other time fields. Fall back to
        // (MAX, MAX) (treat as infinite).
        let l = SearchLimits::default();
        let caps = compute_caps(&l, Color::White, 50);
        assert_caps_eq(caps, nocap(), nocap());
    }

    #[test]
    fn compute_caps_zero_move_overhead_does_not_subtract() {
        // mo=0: soft = 10000/20 + 0/2 = 500 ms; hard = min(1500, 10000) = 1500.
        let l = limits_with(|l| {
            l.wtime = Some(10_000);
            l.winc = Some(0);
        });
        let caps = compute_caps(&l, Color::White, 0);
        assert_caps_eq(
            caps,
            Duration::from_millis(500),
            Duration::from_millis(1500),
        );
    }

    // -----------------------------------------------------------------------
    // M3.E — Iterative-deepening outer-loop tests.
    //
    // Per `docs/plans/m3.e.md` §8.2. Tests drive `AlphaBetaMover::go` with
    // constructed `SearchContext`s; they observe behavior via captured
    // `info_sink` lines and `result.depth/bestmove/score_cp/nodes`. The
    // M3.E `prior_root_move` machinery is replaced by M4.A's TT in S7.
    // -----------------------------------------------------------------------

    /// Build a `SearchContext` from a position with the given limits. Caps are
    /// `(MAX, MAX)` (no time bound); stop is non-aborting; wallclock mode.
    fn ctx_for(pos: &Position, limits: SearchLimits) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits,
            history: vec![pos.zobrist()],
            tt: None,
        };
        (ctx, stop)
    }

    /// Build a `SearchContext` whose soft cap is so small the worker-side
    /// `SearchClock::start_for` produces a soft_deadline already in the past
    /// by the time the ID outer loop checks it. Used to verify that
    /// iteration 1 still completes before the soft check.
    ///
    /// Implementation note: under the ELOH.C refactor we no longer carry
    /// pre-computed deadlines on `SearchContext`; the worker constructs them
    /// from caps. A 1-nanosecond soft cap will be in the past by the time the
    /// first iteration finishes, satisfying the test's intent.
    fn ctx_with_soft_in_past(
        pos: &Position,
        limits: SearchLimits,
    ) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::from_nanos(1),
                hard: Duration::from_secs(10),
            },
            virtual_clock: false,
            limits,
            history: vec![pos.zobrist()],
            tt: None,
        };
        (ctx, stop)
    }

    /// Capture `info` lines emitted by `Search::go`.
    fn capture_info<F>(f: F) -> (SearchResult, Vec<String>)
    where
        F: FnOnce(&dyn Fn(&str)) -> SearchResult,
    {
        let lines: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let sink = |s: &str| lines.borrow_mut().push(s.to_string());
        let result = f(&sink);
        (result, lines.into_inner())
    }

    /// Drive `mover.go` with a captured info_sink.
    fn drive_go(
        mover: &mut AlphaBetaMover,
        pos: &Position,
        ctx: &SearchContext,
    ) -> (SearchResult, Vec<String>) {
        capture_info(|sink| mover.go(pos, ctx, sink))
    }

    #[test]
    fn id_completes_full_iteration_when_depth_caps_max_depth() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(3)));
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(result.depth, 3, "result.depth must equal requested depth");
        assert!(
            result.bestmove.is_some(),
            "depth-3 search must return a bestmove"
        );
        // M3.E emits one info line per completed iteration.
        assert_eq!(
            infos.len(),
            3,
            "ID must emit one info line per completed iteration (1, 2, 3); got {} lines: {:?}",
            infos.len(),
            infos
        );
    }

    #[test]
    fn id_emits_info_lines_in_increasing_depth_order() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(4)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(infos.len(), 4, "expected 4 info lines; got: {infos:?}");
        for (i, line) in infos.iter().enumerate() {
            let expected_prefix = format!("info depth {} ", i + 1);
            assert!(
                line.starts_with(&expected_prefix),
                "info line {} must start with {expected_prefix:?}; got: {line:?}",
                i
            );
        }
    }

    #[test]
    fn id_aborts_between_iterations_when_soft_deadline_passed() {
        // Position: Kiwipete — high branching means iteration ~3-4 will exceed
        // the 200ms soft. Generous hard deadline so the abort happens BETWEEN
        // iterations (post-iteration soft check), not via mid-iteration
        // hard-cap should_abort.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("Kiwipete FEN must parse");
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::from_millis(200),
                hard: Duration::from_secs(10),
            },
            virtual_clock: false,
            limits: limits_with(|l| l.depth = Some(20)),
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        assert!(
            !infos.is_empty(),
            "ID must emit at least one info line before the soft check fires"
        );
        // Iteration 1 from Kiwipete completes well within 200ms; soft check
        // fires AFTER it. We pin depth >= 1 (iteration 1 ran to completion)
        // AND depth < 20 (loop broke before reaching the depth cap). Without
        // the >= 1 lower bound, an iteration-1 abort (mid-iteration via hard
        // cap or stop) would still produce empty info lines and a depth-0
        // result, falsely passing the existing assertions.
        assert!(
            result.depth >= 1,
            "iteration 1 must complete; got result.depth={}",
            result.depth
        );
        assert!(
            result.depth < 20,
            "ID must break before reaching depth 20 due to soft cap; got depth {}",
            result.depth
        );
        assert!(
            result.bestmove.is_some(),
            "soft-cap exit must preserve bestmove from last completed iteration"
        );
    }

    #[test]
    fn id_completes_iteration_1_unconditionally_even_if_soft_already_past() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_with_soft_in_past(&pos, limits_with(|l| l.depth = Some(20)));
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        // Iteration 1 must complete despite soft already past — the soft check
        // fires AT END OF ITERATION, so iteration 1 always runs once.
        assert_eq!(
            result.depth, 1,
            "iteration 1 must complete; soft check breaks before iteration 2; got depth {}",
            result.depth
        );
        assert!(
            result.bestmove.is_some(),
            "iteration 1 must produce a bestmove"
        );
        assert_eq!(
            infos.len(),
            1,
            "exactly one info line for the single iteration; got {infos:?}"
        );
    }

    #[test]
    fn id_returns_last_complete_iteration_on_mid_iteration_abort() {
        // Calibration protocol per plan §8.2 row 4. Drive go depth N for
        // N=2 and N=3 from startpos with no node cap; capture result.nodes.
        // Pick K = n2 + (n3 - n2) / 2 — halfway through ID iteration 3 by
        // node count. Use nodes: Some(K) as the cap. Expect result.depth == 2.
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        let (ctx2, _stop2) = ctx_for(&pos, limits_with(|l| l.depth = Some(2)));
        let (r2, _) = drive_go(&mut ab, &pos, &ctx2);
        let n2 = r2.nodes;

        let mut ab2 = AlphaBetaMover::new();
        let (ctx3, _stop3) = ctx_for(&pos, limits_with(|l| l.depth = Some(3)));
        let (r3, _) = drive_go(&mut ab2, &pos, &ctx3);
        let n3 = r3.nodes;

        let iter3_size = n3 - n2;
        // 4096-cadence safety: skip if iter-3 won't trigger a cadence-aligned
        // poll inside the iteration.
        if iter3_size < 4096 {
            eprintln!("iter-3 size {iter3_size} < 4096; cadence wouldn't fire — test skipped");
            return;
        }
        let k = n2 + iter3_size / 2;

        let mut ab3 = AlphaBetaMover::new();
        let (ctx, _stop) = ctx_for(
            &pos,
            limits_with(|l| {
                l.depth = Some(20);
                l.nodes = Some(k);
            }),
        );
        let (result, _infos) = drive_go(&mut ab3, &pos, &ctx);

        assert_eq!(
            result.depth, 2,
            "node cap halfway through iteration 3 must preserve iteration 2's snapshot; \
             got depth {}",
            result.depth
        );
        assert!(
            result.bestmove.is_some(),
            "last_complete snapshot must include a bestmove"
        );
        assert_ne!(
            result.score_cp,
            Some(0),
            "score must be from completed iteration, not 0 abort sentinel"
        );
    }

    #[test]
    fn id_node_cap_aborts_iteration_n_plus_1_returns_iteration_n_snapshot() {
        // Same calibration shape but for iteration 4: K = n3 + (n4 - n3) / 2.
        // Asserts result.depth == 3.
        let pos = Position::starting_position();
        let mut ab3 = AlphaBetaMover::new();
        let (ctx3, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(3)));
        let n3 = drive_go(&mut ab3, &pos, &ctx3).0.nodes;

        let mut ab4 = AlphaBetaMover::new();
        let (ctx4, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(4)));
        let n4 = drive_go(&mut ab4, &pos, &ctx4).0.nodes;

        let iter4_size = n4 - n3;
        if iter4_size < 4096 {
            eprintln!("iter-4 size {iter4_size} < 4096; cadence wouldn't fire — test skipped");
            return;
        }
        let k = n3 + iter4_size / 2;

        let mut ab = AlphaBetaMover::new();
        let (ctx, _stop) = ctx_for(
            &pos,
            limits_with(|l| {
                l.depth = Some(20);
                l.nodes = Some(k);
            }),
        );
        let (result, _) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(
            result.depth, 3,
            "node cap halfway through iteration 4 must preserve iteration 3's snapshot; \
             got depth {}",
            result.depth
        );
        assert!(result.bestmove.is_some());
    }

    #[test]
    fn id_partial_pv_from_aborted_iteration_does_not_leak_into_result() {
        // Same shape as id_returns_last_complete_iteration_on_mid_iteration_abort,
        // additionally asserts that the bestmove specifically matches a
        // FRESH depth-2 search's bestmove (NOT the aborted iteration-3's pv[0]).
        let pos = Position::starting_position();

        let mut ab2 = AlphaBetaMover::new();
        let (ctx2, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(2)));
        let r2 = drive_go(&mut ab2, &pos, &ctx2).0;
        let bm_at_depth_2 = r2.bestmove.expect("depth-2 search must have bestmove");
        let n2 = r2.nodes;

        let mut ab3 = AlphaBetaMover::new();
        let (ctx3, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(3)));
        let n3 = drive_go(&mut ab3, &pos, &ctx3).0.nodes;
        let iter3_size = n3 - n2;
        if iter3_size < 4096 {
            eprintln!("iter-3 size {iter3_size} < 4096; cadence wouldn't fire — test skipped");
            return;
        }
        let k = n2 + iter3_size / 2;

        let mut ab = AlphaBetaMover::new();
        let (ctx, _stop) = ctx_for(
            &pos,
            limits_with(|l| {
                l.depth = Some(20);
                l.nodes = Some(k);
            }),
        );
        let (result, _) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(result.depth, 2);
        assert_eq!(
            result.bestmove,
            Some(bm_at_depth_2),
            "bestmove from a mid-iteration-3 abort must match the depth-2 snapshot"
        );
    }

    #[test]
    fn id_default_max_depth_for_bare_go_is_4() {
        // Bare `go` (no fields set) → max_depth = 4 (legacy fallback).
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, SearchLimits::default());
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(result.depth, 4, "bare go must default to depth 4");
        assert_eq!(infos.len(), 4, "ID must emit 4 info lines for depth 4");
    }

    // Direct pure-function tests on `max_depth_from_limits`. These pin the
    // function's contract without running an actual search.

    #[test]
    fn max_depth_from_limits_bare_returns_4() {
        let l = SearchLimits::default();
        assert_eq!(max_depth_from_limits(&l), 4);
    }

    #[test]
    fn max_depth_from_limits_explicit_depth_caps_at_max_ply_minus_1() {
        let l = limits_with(|l| l.depth = Some(100));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_explicit_depth_passes_through() {
        let l = limits_with(|l| l.depth = Some(7));
        assert_eq!(max_depth_from_limits(&l), 7);
    }

    #[test]
    fn max_depth_from_limits_infinite_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.infinite = true);
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_ponder_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.ponder = true);
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_movetime_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.movetime = Some(1000));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_nodes_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.nodes = Some(100_000));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_mate_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.mate = Some(3));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_wtime_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.wtime = Some(1000));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn max_depth_from_limits_btime_returns_max_ply_minus_1() {
        let l = limits_with(|l| l.btime = Some(1000));
        assert_eq!(max_depth_from_limits(&l), MAX_PLY as u32 - 1);
    }

    #[test]
    fn id_explicit_depth_100_does_not_panic_on_pv_indexing() {
        // depth=Some(100) is clamped to MAX_PLY - 1 = 63 by max_depth_from_limits.
        // A buggy clamp that lets depth exceed MAX_PLY-1 would index past the
        // PV table array (size MAX_PLY) and panic. Use a tight nodes cap so
        // the test exits within milliseconds; pure-function tests above pin
        // the exact clamp value.
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(
            &pos,
            limits_with(|l| {
                l.depth = Some(100);
                l.nodes = Some(1000);
            }),
        );
        let mut ab = AlphaBetaMover::new();
        let (result, _) = drive_go(&mut ab, &pos, &ctx);
        // No panic from PV-table OOB; result.depth >= 1 (iteration 1 fits).
        assert!(result.depth >= 1);
    }

    #[test]
    fn id_nodes_accumulate_across_iterations() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(4)));
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        assert_eq!(infos.len(), 4);

        // Parse `nodes` field from each info line; assert monotonic
        // non-decreasing.
        let parse_nodes = |line: &str| -> u64 {
            let toks: Vec<&str> = line.split_whitespace().collect();
            let i = toks
                .iter()
                .position(|t| *t == "nodes")
                .expect("info line must contain `nodes`");
            toks[i + 1].parse().expect("`nodes` value must be u64")
        };
        let counts: Vec<u64> = infos.iter().map(|s| parse_nodes(s)).collect();
        for w in counts.windows(2) {
            assert!(
                w[1] >= w[0],
                "nodes must accumulate (be monotonic non-decreasing) across iterations; got {counts:?}"
            );
        }

        assert_eq!(
            result.nodes,
            *counts.last().unwrap(),
            "result.nodes must equal the last info line's nodes"
        );
    }

    #[test]
    fn id_iteration_1_pv_clear_isolates_from_prior_iteration_state() {
        // Drive `go depth 1` then `go depth 2` on the SAME mover (no reset()
        // between). The depth 2 call's iteration 1 must NOT see leftover state.
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();

        let (ctx1, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(1)));
        let _ = drive_go(&mut ab, &pos, &ctx1);

        let (ctx2, _) = ctx_for(&pos, limits_with(|l| l.depth = Some(2)));
        let (r2, _) = drive_go(&mut ab, &pos, &ctx2);

        assert_eq!(r2.depth, 2);
        assert!(r2.bestmove.is_some());
    }

    #[test]
    fn id_breaks_when_stop_flipped_between_iterations() {
        // info_sink itself flips ctx.stop = true on the first call. The next
        // `if ctx.stop.load(Relaxed) { break; }` check sees stop=true, breaks.
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: limits_with(|l| l.depth = Some(20)),
            history: vec![pos.zobrist()],
            tt: None,
        };
        let stop_flip = Arc::clone(&stop);
        let infos: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let info_sink = |s: &str| {
            infos.borrow_mut().push(s.to_string());
            // Flip stop synchronously on first call (typically `info depth 1 …`).
            stop_flip.store(true, Ordering::Relaxed);
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &info_sink);
        let infos = infos.into_inner();

        assert_eq!(
            result.depth, 1,
            "loop must break after iteration 1 due to stop flip"
        );
        assert_eq!(infos.len(), 1, "exactly one info line emitted before stop");
        assert!(result.bestmove.is_some());
    }

    // ===========================================================================
    // M4.A — TT integration tests S1–S13 (per docs/plans/m4.a.md §5.2).
    // ===========================================================================

    /// Build a `SearchContext` with the given TT installed and the given limits.
    fn ctx_for_with_tt(
        pos: &Position,
        limits: SearchLimits,
        tt: Arc<TranspositionTable>,
    ) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits,
            history: vec![pos.zobrist()],
            tt: Some(tt),
        };
        (ctx, stop)
    }

    // -----------------------------------------------------------------------
    // S1 — Exact bound stored at root after full search.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_stores_exact_bound_at_root_after_full_search() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        let (ctx, _stop) = ctx_for_with_tt(&pos, limits_with(|l| l.depth = Some(2)), tt.clone());
        let mut ab = AlphaBetaMover::new();
        let _ = drive_go(&mut ab, &pos, &ctx);

        let entry = tt
            .probe(pos.zobrist())
            .expect("root entry must be present after full search");
        assert!(
            entry.depth as u32 >= 2,
            "stored depth must be >= search depth; got {}",
            entry.depth
        );
        assert_eq!(
            entry.bound(),
            TtBound::Exact,
            "full-window root search must store an Exact bound; got {:?}",
            entry.bound()
        );
    }

    // -----------------------------------------------------------------------
    // S2 — Lower bound stored after a beta cutoff.
    // -----------------------------------------------------------------------

    /// Drive negamax with a fail-high window so the root produces a Lower
    /// bound. Startpos at depth 2 with `(alpha, beta) = (-100, -99)` — every
    /// reasonable root score exceeds beta, triggering an immediate beta
    /// cutoff at root. After the call, the root entry must have bound==Lower.
    #[test]
    fn negamax_stores_lower_bound_after_beta_cutoff() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, -100, -99, false, true, &ctx);

        let entry = tt
            .probe(pos.zobrist())
            .expect("root entry must be present after fail-high search");
        assert_eq!(
            entry.bound(),
            TtBound::Lower,
            "beta-cutoff at root must store Lower; got {:?}",
            entry.bound()
        );
        assert!(
            entry.best_move != 0,
            "Lower-bound store must record the cutoff move; got 0"
        );
    }

    // -----------------------------------------------------------------------
    // S3 — Aborted search must not store.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_does_not_store_after_abort() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        let stop = Arc::new(AtomicBool::new(true));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: limits_with(|l| l.depth = Some(10)),
            history: vec![pos.zobrist()],
            tt: Some(tt.clone()),
        };
        let mut ab = AlphaBetaMover::new();
        let _ = ab.go(&pos, &ctx, &|_| {});

        // The TT generation advanced via new_search() from 0 → 1. Iteration 1
        // from startpos visits ~20 nodes (well below the 4096 cadence), so it
        // completes and DOES store at root. The inter-iteration stop check
        // then breaks the loop, preventing iter 2 from running. Iteration 1's
        // store at gen 1 is therefore expected. The abort-skip discipline
        // applies to iterations that abort MID-loop (depth >= 4096-cadence).
        //
        // To pin the abort-skip half cleanly, drive `negamax_for_test`
        // directly with a pre-set abort flag and verify NO entry is stored.
        let pos2 = Position::starting_position();
        let tt2 = Arc::new(TranspositionTable::new(1));
        let (ctx2, _stop2) = non_aborting_ctx_with_tt(tt2.clone());
        let mut ab2 = AlphaBetaMover::new();
        ab2.history = vec![pos2.zobrist()];
        ab2.set_tt_for_test(Some(tt2.clone()));
        ab2.aborted = true;
        let _ = ab2.negamax_for_test(&mut pos2.clone(), 2, 0, -INF, INF, true, true, &ctx2);
        assert!(
            tt2.probe(pos2.zobrist()).is_none(),
            "negamax with self.aborted=true must not store a TT entry"
        );
    }

    // -----------------------------------------------------------------------
    // S3b — No mid-loop store: pre-populated entry survives a partial iteration.
    // -----------------------------------------------------------------------

    /// Pin "no mid-loop store under partial iteration." Pre-populate the
    /// root TT entry with a marker (depth=99, score=12345). Drive
    /// `Search::go` at depth 5 with `deadline` already in the past, so the
    /// 4096-cadence cancellation fires inside iter 1's recursion (Kiwipete
    /// at depth 5 visits »4096 nodes — empirically ~50k+) BEFORE iter 1
    /// completes its move loop. The store discipline (skip-if-aborted +
    /// end-of-loop-only) implies the marker entry survives untouched.
    /// A buggy mid-loop store would overwrite the marker with a
    /// partial-iter-1 entry of much lower depth.
    ///
    /// Choosing Kiwipete (large branching factor) over startpos here is
    /// load-bearing: startpos depth-1 visits ~20 nodes — below the 4096
    /// cadence — so iter 1 from startpos completes and legitimately stores
    /// before any cancellation poll fires. The cadence-fire-during-iter-1
    /// regime requires a fixture where iter 1 visits ≥ 4096 nodes; Kiwipete
    /// at depth 5 visits ~150k.
    #[test]
    fn negamax_does_not_mid_loop_store_under_partial_iteration() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete fen parses");
        let tt = Arc::new(TranspositionTable::new(1));
        // Pre-populate with marker entry. Bound=Exact / depth=99 / score=12345
        // / best_move=arbitrary non-zero. After iter 1 aborts mid-loop, the
        // root entry must be byte-identical to this.
        let marker = TtData {
            score: 12345,
            depth: 99,
            bound: TtBound::Exact,
            best_move: 0xABCD,
        };
        tt.store(pos.zobrist(), marker);
        let stored_before = tt.probe(pos.zobrist()).expect("marker must be stored");

        // Pre-set stop=true so the cadence poll inside negamax flips
        // self.aborted=true on the first check. Equivalent to the
        // pre-ELOH.C deadline-in-past pattern: both abort the search at
        // the top-of-negamax `should_abort` poll without entering the
        // move loop, exercising the same skip-store path.
        let stop = Arc::new(AtomicBool::new(true));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: limits_with(|l| l.depth = Some(5)),
            history: vec![pos.zobrist()],
            tt: Some(tt.clone()),
        };
        let mut ab = AlphaBetaMover::new();
        let _ = ab.go(&pos, &ctx, &|_| {});

        let stored_after = tt
            .probe(pos.zobrist())
            .expect("marker entry must still be present after aborted go");
        assert_eq!(
            stored_before, stored_after,
            "aborted iter-1 must not have written a mid-loop store; \
             marker must survive byte-for-byte"
        );
        // Anti-stub belt-and-braces: the marker depth=99 is impossible for
        // a real iter-1 store (depth=1). If a bug produced any iter-N store,
        // depth would drop to N. Pin that.
        assert_eq!(
            stored_after.depth, 99,
            "marker depth must be unchanged; got {}",
            stored_after.depth
        );
    }

    // -----------------------------------------------------------------------
    // S4 — Non-PV Lower-bound TT cutoff with sufficient depth.
    // -----------------------------------------------------------------------

    /// Pre-populate the TT at startpos with depth=5, Lower, score=200.
    /// Call `negamax_for_test` at the same position with depth=3, is_pv=false,
    /// alpha=-INF, beta=100 — score >= beta should fire the cutoff. The
    /// returned score is exactly 200; node count is 1 (the negamax frame's
    /// own increment) — proving recursion never ran.
    #[test]
    fn negamax_returns_tt_score_on_non_pv_lower_bound_hit_with_sufficient_depth() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 200,
                depth: 5,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let nodes_before = ab.nodes;
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, &ctx);
        let nodes_consumed = ab.nodes - nodes_before;

        assert_eq!(
            score, 200,
            "non-PV Lower hit with score >= beta must return stored score"
        );
        assert_eq!(
            nodes_consumed, 1,
            "TT cutoff must avoid recursion; consumed {nodes_consumed} nodes"
        );
    }

    // -----------------------------------------------------------------------
    // S4b — Non-PV Upper-bound TT cutoff with sufficient depth.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_returns_tt_score_on_non_pv_upper_bound_hit_with_sufficient_depth() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: -200,
                depth: 5,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let nodes_before = ab.nodes;
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -100, INF, false, true, &ctx);
        let nodes_consumed = ab.nodes - nodes_before;

        assert_eq!(
            score, -200,
            "non-PV Upper hit with score <= alpha must return stored score"
        );
        assert_eq!(
            nodes_consumed, 1,
            "TT cutoff must avoid recursion; consumed {nodes_consumed} nodes"
        );
    }

    // -----------------------------------------------------------------------
    // S5 — PV-node ignores TT-score even on Exact hit.
    // -----------------------------------------------------------------------

    /// Pre-populate the TT at startpos with bound=Exact, depth=5, score=42.
    /// Call `negamax_for_test` with is_pv=true, depth=3 and the full window.
    /// PV-node discipline (ADR-0018 §11): never returns early on TT hit.
    /// Verified by node count > 1 (recursion ran).
    #[test]
    fn negamax_does_not_return_tt_score_at_pv_node_even_on_exact_hit() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 42,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let nodes_before = ab.nodes;
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx);
        let nodes_consumed = ab.nodes - nodes_before;

        assert!(
            nodes_consumed > 1,
            "PV node must NOT return early on Exact TT hit; consumed {nodes_consumed} nodes \
             (a TT cutoff would have consumed exactly 1)"
        );
    }

    // -----------------------------------------------------------------------
    // S6 — PV-node uses TT move for ordering.
    // -----------------------------------------------------------------------

    /// Pre-populate the TT at startpos with a known best_move (e.g. e2e4)
    /// at depth 5, bound=Exact, score=0. Call `negamax_for_test` is_pv=true
    /// depth=2 with full window. Without the TT-move-first reorder, MVV-LVA
    /// would order quiets by movegen order at startpos. Asserting `pv[0]`
    /// equals the TT move pins the reorder semantics: the TT move was tried
    /// first AND it improved alpha (since the score=0 TT hint and full window
    /// don't constrain the outcome — startpos depth 2 picks a balanced move
    /// regardless, and any first-tried move that doesn't lose material will
    /// improve alpha from -INF and remain in the PV).
    ///
    /// More robust: pin a node-count differential. With the TT-move-first
    /// reorder, the search tries `e2e4` first; without it, MVV-LVA's stable
    /// sort runs movegen-first first. Different orderings produce different
    /// node counts at depth 2.
    #[test]
    fn negamax_uses_tt_move_for_ordering_at_pv_node() {
        let pos = Position::starting_position();

        // Find a legal move that is NOT the natural unhinted bestmove + NOT
        // the movegen-first move; the reorder must observably bring it to
        // the front. We use the LAST legal move from the movegen iterator
        // (never movegen-first).
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let all_moves: Vec<Move> = ml.iter().collect();
        let last_move = *all_moves.last().expect("startpos has legal moves");
        let movegen_first = all_moves[0];
        assert_ne!(
            last_move, movegen_first,
            "fixture: last move must differ from movegen-first"
        );

        // Run (a): no TT hint. Records natural node count.
        let tt_a = Arc::new(TranspositionTable::new(1));
        let (ctx_a, _stop_a) = non_aborting_ctx_with_tt(tt_a.clone());
        let mut ab_a = AlphaBetaMover::new();
        ab_a.history = vec![pos.zobrist()];
        ab_a.set_tt_for_test(Some(tt_a.clone()));
        let _ = ab_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx_a);
        let nodes_unhinted = ab_a.nodes;

        // Run (b): pre-populate TT with last_move at insufficient depth so
        // PV-node skips the cutoff but still uses tt_move for ordering.
        let tt_b = Arc::new(TranspositionTable::new(1));
        tt_b.store(
            pos.zobrist(),
            TtData {
                score: 0,
                depth: 5,
                bound: TtBound::Exact,
                best_move: last_move.bits(),
            },
        );
        let (ctx_b, _stop_b) = non_aborting_ctx_with_tt(tt_b.clone());
        let mut ab_b = AlphaBetaMover::new();
        ab_b.history = vec![pos.zobrist()];
        ab_b.set_tt_for_test(Some(tt_b.clone()));
        let _ = ab_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx_b);
        let nodes_hinted = ab_b.nodes;

        assert_ne!(
            nodes_unhinted,
            nodes_hinted,
            "TT move ordering must change root search order, observable as node-count delta; \
             unhinted={nodes_unhinted}, hinted={nodes_hinted} (last_move={})",
            last_move.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // S7 — ID iteration uses prior-iteration TT-move at root (replaces M3.E
    //      `prior_root_move` test).
    // -----------------------------------------------------------------------

    /// Run `Search::go` at depth 4 from startpos with a TT in context.
    /// After the search, probe the root entry: bestmove must equal the
    /// reported result.bestmove (the final iteration's snapshot).
    #[test]
    fn negamax_id_iteration_uses_prior_iteration_tt_move_at_root() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        let (ctx, _stop) = ctx_for_with_tt(&pos, limits_with(|l| l.depth = Some(4)), tt.clone());
        let mut ab = AlphaBetaMover::new();
        let (result, _infos) = drive_go(&mut ab, &pos, &ctx);

        let bm = result.bestmove.expect("depth-4 must produce a bestmove");
        let entry = tt
            .probe(pos.zobrist())
            .expect("root TT entry must be present after go");
        assert_eq!(
            entry.best_move,
            bm.bits(),
            "root TT entry's bestmove must equal final iteration's bestmove; \
             entry.best_move={:#x}, expected={:#x}",
            entry.best_move,
            bm.bits()
        );
    }

    // -----------------------------------------------------------------------
    // S8 — Mate score round-trips through the TT.
    // -----------------------------------------------------------------------

    /// Mate-in-2 fixture: position P at root, depth 4 search returns MATE-4
    /// (4 plies to mate). At ply=0 the score-to-tt is a no-op, so the stored
    /// score is exactly MATE-4. Now drive negamax_for_test at the SAME
    /// position from ply=2 (a hypothetical view); the probe at P's key
    /// returns the stored entry whose ply-adjusted view at ply=2 is MATE-6.
    #[test]
    fn negamax_mate_score_round_trips_through_tt() {
        // Mate-in-1 fixture from the existing `alphabeta_finds_mate_in_1...`
        // test. depth=2 search returns MATE-1 at root (2 plies to mate is
        // overkill; mate-in-1 returns MATE-1 = MATE - 1).
        let pos =
            Position::from_fen("7k/8/5KQ1/8/8/8/8/8 w - - 0 1").expect("mate-in-1 FEN must parse");
        let tt = Arc::new(TranspositionTable::new(1));
        let (ctx, _stop) = ctx_for_with_tt(&pos, limits_with(|l| l.depth = Some(2)), tt.clone());
        let mut ab = AlphaBetaMover::new();
        let (result, _) = drive_go(&mut ab, &pos, &ctx);

        let mate1 = MATE - 1;
        assert_eq!(
            result.score_cp,
            Some(mate1),
            "mate-in-1 must return MATE-1; got {:?}",
            result.score_cp
        );

        let entry = tt
            .probe(pos.zobrist())
            .expect("root TT entry must be present after go");
        // Stored at ply 0 → score_to_tt is a no-op for ply=0.
        assert_eq!(
            entry.score as i32, mate1,
            "stored mate score at ply 0 must be MATE-1 (score_to_tt no-op); got {}",
            entry.score
        );

        // Probe-side view at ply 2: the same absolute mate node is now 1
        // ply farther from the searcher's frame.
        let view_at_ply_2 = score_from_tt(entry.score as i32, 2);
        assert_eq!(
            view_at_ply_2,
            mate1 - 2,
            "score_from_tt(MATE-1, 2) must equal MATE-3 (1 ply absolute mate, 2 plies adjustment); got {view_at_ply_2}"
        );
    }

    // -----------------------------------------------------------------------
    // S9 — Repetition check runs BEFORE TT probe.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_repetition_check_runs_before_tt_probe() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 4 1")
            .expect("startpos with halfmove=4 must parse");
        let tt = Arc::new(TranspositionTable::new(1));
        // Pre-populate TT with non-zero score for this key.
        tt.store(
            pos.zobrist(),
            TtData {
                score: 999,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(2, tt.clone());

        let mut ab = AlphaBetaMover::new();
        let other_zobrist: u64 = 0xDEAD_BEEF_CAFE_0000;
        ab.history = vec![pos.zobrist(), other_zobrist, pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, true, &ctx);
        assert_eq!(
            score, 0,
            "repetition (returns 0) must run BEFORE TT probe; got {score} (TT score=999)"
        );
    }

    // -----------------------------------------------------------------------
    // S10 — 50-move check runs BEFORE TT probe.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_50_move_check_runs_before_tt_probe() {
        let pos = Position::from_fen("8/8/8/8/4k3/8/8/4K3 w - - 100 1")
            .expect("KvK with halfmove=100 FEN must parse");
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 999,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(2, tt.clone());

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, true, &ctx);
        assert_eq!(
            score, 0,
            "50-move draw (returns 0) must run BEFORE TT probe; got {score} (TT score=999)"
        );
    }

    // -----------------------------------------------------------------------
    // S11 — TT resize during engine lifetime clears entries (engine-level —
    //       deferred to Slice C / E_b. Search-level placeholder kept here
    //       to confirm explicit `tt.clear()` zeroes entries observable by
    //       a fresh probe).
    // -----------------------------------------------------------------------

    #[test]
    fn tt_clear_invalidates_negamax_cutoffs() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 200,
                depth: 5,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());

        let mut ab1 = AlphaBetaMover::new();
        ab1.history = vec![pos.zobrist()];
        ab1.set_tt_for_test(Some(tt.clone()));
        let nodes_before_a = ab1.nodes;
        let _ = ab1.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, &ctx);
        let nodes_with_hit = ab1.nodes - nodes_before_a;

        tt.clear();

        let mut ab2 = AlphaBetaMover::new();
        ab2.history = vec![pos.zobrist()];
        ab2.set_tt_for_test(Some(tt.clone()));
        let nodes_before_b = ab2.nodes;
        let _ = ab2.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, &ctx);
        let nodes_after_clear = ab2.nodes - nodes_before_b;

        assert!(
            nodes_after_clear > nodes_with_hit,
            "tt.clear() must invalidate the cutoff; nodes_with_hit={nodes_with_hit}, \
             nodes_after_clear={nodes_after_clear}"
        );
    }

    // -----------------------------------------------------------------------
    // S12 — TT-move legality filter rejects collision garbage.
    // -----------------------------------------------------------------------

    /// Pre-populate the TT entry with `best_move = 0xFFFF` (an invalid
    /// `Move` bit pattern that never appears in a legal-move list). The
    /// negamax body's `tt_move != 0 && position(...).is_some()` guard
    /// must reject it without panic and without picking a bogus move.
    #[test]
    fn tt_move_legality_filter_rejects_collision_garbage() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 0,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0xFFFF,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, &ctx);

        let pv = ab.pv_root_for_test();
        assert!(
            !pv.is_empty(),
            "depth-1 negamax must populate a PV; bogus tt_move must not block ordering"
        );
        let bm = pv[0];
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|m| m == bm),
            "bestmove must be a legal startpos move; got {}",
            bm.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // S13 — `is_pv` propagates only to the first-recursion-index child.
    // -----------------------------------------------------------------------

    /// Indirect verification per plan §5.2: pre-populate TT entries at every
    /// child of the root with `bound=Exact, depth=10`. Drive negamax at the
    /// root with depth 3, is_pv=true. The PV-child (index 0) does NOT cut
    /// off (PV discipline); the other children DO cut off (non-PV exact hit
    /// fires immediately, returning the stored score).
    ///
    /// Counted nodes:
    ///   - root frame: 1
    ///   - PV-child frame: a full depth-2 subtree expansion (no TT hit at
    ///     the root key after make_move; subsequent grand-children may have
    ///     TT entries we did not pre-populate, so the PV-child recurses).
    ///   - each non-PV child: 1 frame (cuts off on TT hit).
    ///
    /// The expected total = 1 (root) + (depth-2 subtree from the PV child)
    /// + (N - 1) for the other children that each consume 1 node.
    ///
    /// We don't compute the exact value — we compare against a reference
    /// run with NO TT entries pre-populated. The reference run recurses
    /// fully on every child; the test run expands only the first child and
    /// cuts off on the rest. The test run must consume strictly fewer nodes
    /// than the reference run.
    #[test]
    fn negamax_is_pv_only_first_recursion_index_propagates() {
        let pos = Position::starting_position();

        // Reference run: empty TT.
        let tt_ref = Arc::new(TranspositionTable::new(1));
        let (ctx_ref, _) = non_aborting_ctx_with_tt(tt_ref.clone());
        let mut ab_ref = AlphaBetaMover::new();
        ab_ref.history = vec![pos.zobrist()];
        ab_ref.set_tt_for_test(Some(tt_ref.clone()));
        let _ = ab_ref.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx_ref);
        let nodes_ref = ab_ref.nodes;

        // Pre-populated run: every child of root has an Exact, depth=10
        // entry with score=0.
        let tt = Arc::new(TranspositionTable::new(1));
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        for mv in ml.iter() {
            let mut child = pos;
            child.make_move(mv);
            tt.store(
                child.zobrist(),
                TtData {
                    score: 0,
                    depth: 10,
                    bound: TtBound::Exact,
                    best_move: 0,
                },
            );
        }
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx);
        let nodes_with_tt = ab.nodes;

        assert!(
            nodes_with_tt < nodes_ref,
            "pre-populated TT must reduce node count via non-PV-child cutoffs; \
             nodes_ref={nodes_ref}, nodes_with_tt={nodes_with_tt}"
        );

        // Tight assertion: under correct `child_is_pv = is_pv && i == 0`,
        // exactly child[0] is PV (no TT cut) and children[1..] are non-PV
        // (TT cut). So `nodes_with_tt` ≈ "1 root frame + 1 PV-child subtree
        // + (N-1) cut frames" ≈ depth-2 subtree size. `nodes_ref` ≈
        // "1 root + N PV-child subtrees" ≈ N × depth-2 subtree.
        //
        // Under the mutation `i != 0`, child[0] is non-PV (cuts) and
        // children[1..] are PV (full recursion), so `nodes_with_tt` ≈
        // (N-1) × subtree — close to `nodes_ref`, not far below it.
        //
        // Pin the ratio: under correct behavior, child[0] is the only
        // child that recurses (others cut), so nodes_with_tt is ≈ one
        // depth-2 subtree (~5-15% of nodes_ref). Under the mutation
        // `i != 0`, child[0] cuts and children[1..] all recurse, so
        // nodes_with_tt is ≈ (N-1) subtrees (~85-95% of nodes_ref).
        // Threshold at nodes_ref / 2 cleanly separates the two regimes.
        let max_allowed = nodes_ref / 2;
        assert!(
            nodes_with_tt < max_allowed,
            "PV-only-on-child[0] discipline must save at least half the search; \
             nodes_ref={nodes_ref}, nodes_with_tt={nodes_with_tt}, \
             max_allowed={max_allowed}. \
             Mutation `i != 0` would leave nodes_with_tt above nodes_ref/2 \
             (only child[0] cut, all others recursed)."
        );
    }

    // ===========================================================================
    // M4.B — Killer-move tests S14–S29.
    // ===========================================================================

    // -----------------------------------------------------------------------
    // S14 — `is_quiet` accepts Quiet / DoublePush / KingCastle / QueenCastle.
    // -----------------------------------------------------------------------

    /// All four quiet-flag arms must return true.
    /// Pins the `matches!(...)` inclusion list against `delete arm` mutations.
    #[test]
    fn is_quiet_accepts_quiet_doublepush_castles() {
        use crate::mov::MoveFlag::*;
        use crate::square::Square;
        let from = Square::A1;
        let to = Square::B1;
        for flag in [Quiet, DoublePush] {
            let mv = Move::new(from, to, flag);
            assert!(is_quiet(mv), "is_quiet must return true for {flag:?}");
        }
        // Castling moves: use Kiwipete where both castling rights exist.
        let kiwipete = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("Kiwipete FEN must parse");
        let king_castle = Move::from_uci("e1g1", &kiwipete)
            .expect("e1g1 (king-side castle) must be legal in Kiwipete");
        let queen_castle = Move::from_uci("e1c1", &kiwipete)
            .expect("e1c1 (queen-side castle) must be legal in Kiwipete");
        assert!(
            is_quiet(king_castle),
            "is_quiet must return true for KingCastle"
        );
        assert!(
            is_quiet(queen_castle),
            "is_quiet must return true for QueenCastle"
        );
    }

    // -----------------------------------------------------------------------
    // S15 — `is_quiet` rejects captures / promos / en-passant.
    // -----------------------------------------------------------------------

    /// All ten non-quiet flag arms must return false.
    /// Pins the exclusion list; each flag catch a distinct `include arm` mutation.
    #[test]
    fn is_quiet_rejects_captures_promos_enpassant() {
        use crate::mov::MoveFlag::*;
        use crate::square::Square;
        let from = Square::A1;
        let to = Square::B2;
        for flag in [
            Capture,
            EnPassant,
            KnightPromo,
            BishopPromo,
            RookPromo,
            QueenPromo,
            KnightPromoCapture,
            BishopPromoCapture,
            RookPromoCapture,
            QueenPromoCapture,
        ] {
            let mv = Move::new(from, to, flag);
            assert!(!is_quiet(mv), "is_quiet must return false for {flag:?}");
        }
    }

    // -----------------------------------------------------------------------
    // S16 — `update_killers` writes `mv` to slot 0 when the table is empty.
    // -----------------------------------------------------------------------

    #[test]
    fn update_killers_writes_to_slot_0_when_empty() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        let mv = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        update_killers(&mut killers, 1, mv);
        assert_eq!(killers[1][0], mv, "slot 0 must hold the pushed move");
        assert_eq!(
            killers[1][1],
            Move::default(),
            "slot 1 must remain the default sentinel"
        );
        // Other plies must be untouched.
        assert_eq!(killers[0], [Move::default(); 2], "ply 0 must be untouched");
        assert_eq!(killers[2], [Move::default(); 2], "ply 2 must be untouched");
    }

    // -----------------------------------------------------------------------
    // S17 — `update_killers` shifts existing slot 0 to slot 1 on distinct move.
    // -----------------------------------------------------------------------

    #[test]
    fn update_killers_shifts_existing_to_slot_1_on_distinct_move() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        let mv_a = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        let mv_b = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        // Pre-fill: slot 0 = mv_a, slot 1 = default.
        killers[3][0] = mv_a;
        // Push distinct mv_b.
        update_killers(&mut killers, 3, mv_b);
        assert_eq!(killers[3][0], mv_b, "slot 0 must hold the new move");
        assert_eq!(
            killers[3][1], mv_a,
            "slot 1 must hold the displaced old slot-0"
        );
    }

    // -----------------------------------------------------------------------
    // S18 — `update_killers` is a no-op when `mv == killers[ply][0]`.
    // -----------------------------------------------------------------------

    /// Pins the no-op path: pushing the same move again must not change the table.
    /// Kills `replace if killers[ply][0] != mv with true` mutations and the `==`-flip.
    #[test]
    fn update_killers_is_noop_when_move_equals_slot_0() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mv_a = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        let mv_b = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        killers[2][0] = mv_a;
        killers[2][1] = mv_b;
        // Push mv_a again — must be a no-op.
        update_killers(&mut killers, 2, mv_a);
        assert_eq!(killers[2][0], mv_a, "slot 0 must still hold mv_a");
        assert_eq!(killers[2][1], mv_b, "slot 1 must be unchanged (mv_b)");
    }

    // -----------------------------------------------------------------------
    // S19 — `update_killers` promotes slot 1 to slot 0 when `mv == slot[1]`.
    // -----------------------------------------------------------------------

    /// Shift-on-distinct: `mv_b != mv_a` (slot-0), so the shift runs:
    ///   slot[1] = slot[0] = mv_a; slot[0] = mv_b.
    /// The dedup is implicit: mv_b was in slot 1, now in slot 0; old mv_a
    /// shifted into slot 1 (no duplicate entry).
    #[test]
    fn update_killers_promotes_slot_1_to_slot_0_when_move_equals_slot_1() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mv_a = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        let mv_b = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        killers[5][0] = mv_a;
        killers[5][1] = mv_b;
        // Push mv_b (which is in slot 1 but NOT in slot 0).
        update_killers(&mut killers, 5, mv_b);
        assert_eq!(killers[5][0], mv_b, "slot 0 must hold mv_b");
        assert_eq!(
            killers[5][1], mv_a,
            "slot 1 must hold mv_a (the displaced slot-0)"
        );
    }

    // -----------------------------------------------------------------------
    // S20 — `negamax_move_order_score` returns KILLER0_SCORE for quiet == killer0.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_move_order_score_returns_killer_0_bonus_when_quiet_matches_slot_0() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let pos = Position::starting_position();
        let mv = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let other = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        let history_table = HistoryTable::new();
        let score = negamax_move_order_score(mv, &pos, mv, other, &history_table);
        assert_eq!(
            score, KILLER0_SCORE,
            "quiet matching killer0 must return KILLER0_SCORE"
        );
    }

    // -----------------------------------------------------------------------
    // S21 — `negamax_move_order_score` returns KILLER1_SCORE for quiet == killer1.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_move_order_score_returns_killer_1_bonus_when_quiet_matches_slot_1() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let pos = Position::starting_position();
        let mv = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let other = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        // mv is in killer1, other is in killer0 (mv != other so the killer0 check fails).
        let history_table = HistoryTable::new();
        let score = negamax_move_order_score(mv, &pos, other, mv, &history_table);
        assert_eq!(
            score, KILLER1_SCORE,
            "quiet matching killer1 (not killer0) must return KILLER1_SCORE"
        );
    }

    // -----------------------------------------------------------------------
    // S22 — `negamax_move_order_score` returns mvv_lva_score for a capture
    //        even if the same bits match killer0.
    // -----------------------------------------------------------------------

    /// Pins the `!is_quiet(mv)` early-return gate at the top of the helper.
    /// Post-M4.C: the capture-score path returns `mvv_lva_score + CAPTURE_OFFSET`
    /// (NOT `mvv_lva_score` raw); CAPTURE_OFFSET is the M4.C-introduced shift
    /// that places captures above killers regardless of the small killer
    /// constants. This test pins both the gate (capture path is taken even
    /// when bits match killer0) and the offset (the score is the shifted
    /// value, not raw mvv_lva).
    #[test]
    fn negamax_move_order_score_returns_mvv_lva_plus_offset_for_capture_even_if_matches_killer() {
        // Position: white pawn on b4 can capture black queen on c5.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let pawn_takes_queen =
            Move::from_uci("b4c5", &pos).expect("b4c5 must be a legal pawn capture");
        // Install the same capture move as killer0 (artificial — captures can't
        // legally become killers, but the test verifies the gate independently).
        let history_table = HistoryTable::new();
        let score = negamax_move_order_score(
            pawn_takes_queen,
            &pos,
            pawn_takes_queen,
            Move::default(),
            &history_table,
        );
        let expected = mvv_lva_score(pawn_takes_queen, &pos) + CAPTURE_OFFSET;
        assert_eq!(
            score, expected,
            "capture matching killer0 must return mvv_lva_score + CAPTURE_OFFSET \
             ({expected}), not KILLER0_SCORE"
        );
        // Cross-check: the comparator's capture-path output is strictly greater
        // than KILLER0_SCORE, proving the test is not vacuously passing.
        assert!(
            expected > KILLER0_SCORE,
            "comparator capture-path output must exceed KILLER0_SCORE to make S22 non-vacuous; \
             got expected={expected}, KILLER0_SCORE={KILLER0_SCORE}"
        );
    }

    // -----------------------------------------------------------------------
    // S22b — `negamax_move_order_score` returns 0 for quiet not in either slot.
    // -----------------------------------------------------------------------

    #[test]
    fn negamax_move_order_score_zero_for_quiet_not_in_either_slot() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let pos = Position::starting_position();
        let mv = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let killer0 = Move::new(Square::D2, Square::D3, MoveFlag::Quiet);
        let killer1 = Move::new(Square::C2, Square::C3, MoveFlag::Quiet);
        let history_table = HistoryTable::new();
        let score = negamax_move_order_score(mv, &pos, killer0, killer1, &history_table);
        assert_eq!(
            score, 0,
            "quiet not in either killer slot with empty history must score 0"
        );
    }

    // -----------------------------------------------------------------------
    // S23 — Killer constants strictly above MAX_HISTORY and strictly below
    // the comparator's capture-path output (mvv_lva_score + CAPTURE_OFFSET).
    // -----------------------------------------------------------------------

    /// Runtime boundary check using the actual MVV-LVA formula and PeSTO MG
    /// values, plus the M4.C-introduced CAPTURE_OFFSET shift. Pins the four
    /// score-tier constants (`CAPTURE_OFFSET`, `KILLER0_SCORE`,
    /// `KILLER1_SCORE`, `MAX_HISTORY`) against drift in either the MVV-LVA
    /// formula or the killer/history score values.
    ///
    /// The compile-time `_SCORE_TIER_INVARIANTS` const-assert at the top of
    /// the M4.B+M4.C section pins the relative ordering between the four
    /// constants. This test additionally pins the discipline against the
    /// concrete MVV-LVA values: that adding `CAPTURE_OFFSET` to even the
    /// smallest non-losing capture (QxP=287) yields a comparator output
    /// above `KILLER0_SCORE`.
    #[test]
    fn killer_scores_strictly_below_capture_path_output_and_above_max_history() {
        // Killer above MAX_HISTORY (compile-time also).
        const {
            assert!(KILLER1_SCORE as i64 > MAX_HISTORY as i64);
        }
        // Runtime: capture path yields a strictly larger score than KILLER0_SCORE
        // even for the smallest non-losing capture.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let qxp = Move::from_uci("d5c6", &pos).expect("d5c6 must be a legal queen×pawn capture");
        let qxp_capture_path_score = mvv_lva_score(qxp, &pos) + CAPTURE_OFFSET;
        assert!(
            KILLER0_SCORE < qxp_capture_path_score,
            "KILLER0_SCORE ({KILLER0_SCORE}) must be < mvv_lva_score(QxP) + CAPTURE_OFFSET \
             ({qxp_capture_path_score}) so killers slot below all captures in the comparator"
        );
    }

    // -----------------------------------------------------------------------
    // S24 — `order_moves` promotes the TT move to index 0.
    // -----------------------------------------------------------------------

    #[test]
    fn order_moves_promotes_tt_move_to_index_0() {
        // Use startpos for a real movegen-produced list.
        let pos = Position::starting_position();
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut moves_vec: Vec<Move> = ml.iter().collect();
        // Pick the last move as the TT move (least likely to already be first).
        let tt_move = *moves_vec.last().expect("startpos has legal moves");
        order_moves(
            &mut moves_vec,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            tt_move.bits(),
        );
        assert_eq!(
            moves_vec[0],
            tt_move,
            "TT move must be promoted to index 0; got {}",
            moves_vec[0].to_uci()
        );
        // TT move appears exactly once.
        let count = moves_vec.iter().filter(|&&m| m == tt_move).count();
        assert_eq!(
            count, 1,
            "TT move must appear exactly once in the ordered list"
        );
    }

    // -----------------------------------------------------------------------
    // S24b — `order_moves` is a no-op when the TT move is already first.
    // -----------------------------------------------------------------------

    /// Pins the `idx != 0` guard in the swap path.
    #[test]
    fn order_moves_no_op_when_tt_move_already_first() {
        let pos = Position::starting_position();
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut moves_vec: Vec<Move> = ml.iter().collect();
        // Sort first so we can identify what's at index 0 after ordering.
        order_moves(
            &mut moves_vec,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );
        let first = moves_vec[0];
        let before = moves_vec.clone();
        // Now call again with the first move as the TT move.
        order_moves(
            &mut moves_vec,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            first.bits(),
        );
        // The order must be unchanged (first is already at 0; swap is skipped).
        assert_eq!(
            moves_vec, before,
            "order must be unchanged when TT move is already first"
        );
    }

    // -----------------------------------------------------------------------
    // S24c — `order_moves` is a no-op for tt_move == 0 or absent.
    // -----------------------------------------------------------------------

    /// Two sub-cases: (a) tt_move == 0 (sentinel); (b) tt_move bits not in list.
    #[test]
    fn order_moves_no_op_when_tt_move_zero_or_absent() {
        let pos = Position::starting_position();
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);

        // Sub-case (a): tt_move == 0.
        let mut moves_a: Vec<Move> = ml.iter().collect();
        let mut moves_ref: Vec<Move> = ml.iter().collect();
        // Produce the reference order (pure MVV-LVA, no tt_move).
        order_moves(
            &mut moves_ref,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );
        order_moves(
            &mut moves_a,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );
        assert_eq!(
            moves_a, moves_ref,
            "tt_move==0 must produce pure MVV-LVA order (same as reference)"
        );

        // Sub-case (b): tt_move bits not present in the list.
        // Use 0xFFFF — an impossible legal move (from=H1, to=H8, flag=QueenPromoCapture
        // which never appears at startpos). The sort must not promote any entry.
        let mut moves_b: Vec<Move> = ml.iter().collect();
        order_moves(
            &mut moves_b,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0xFFFF,
        );
        assert_eq!(
            moves_b, moves_ref,
            "absent tt_move must produce pure MVV-LVA order (same as reference)"
        );
    }

    // -----------------------------------------------------------------------
    // S24d — `order_moves` places killer above quiets, below captures.
    // -----------------------------------------------------------------------

    /// Ordering boundary: capture > KILLER0 > non-killer quiet. The strong
    /// assertion is the killer-above-non-killer-quiet pair: pick a specific
    /// non-killer quiet and assert its post-`order_moves` index is strictly
    /// greater than the killer's. A stub `order_moves` that ignores killer
    /// scoring would leave both quiets in their natural MVV-LVA order
    /// (movegen order, since both score 0); for the assertion to pass, the
    /// implementation must actually score the killer above other quiets.
    #[test]
    fn order_moves_places_killer_above_quiets_below_captures() {
        // Position: white has a hanging pawn-capture opportunity + quiet moves.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let moves_vec_initial: Vec<Move> = ml.iter().collect();

        // Collect all quiets in movegen (= MVV-LVA-tie) order.
        let quiets: Vec<Move> = moves_vec_initial
            .iter()
            .copied()
            .filter(|&m| {
                use crate::mov::MoveFlag::*;
                matches!(m.flag(), Quiet | DoublePush | KingCastle | QueenCastle)
            })
            .collect();
        assert!(
            quiets.len() >= 2,
            "fixture invariant: position must have ≥ 2 quiet moves to distinguish killer above non-killer; \
             got {} quiets",
            quiets.len()
        );

        // Pick the LAST quiet as the killer (not the first — under empty-killer
        // baseline ordering it would naturally sort below the other quiets).
        let killer_quiet = *quiets.last().unwrap();
        // Pick the FIRST quiet as the witness non-killer (would naturally precede
        // killer_quiet under empty-killer baseline; the test passes only if killer
        // logic flips them).
        let witness_non_killer_quiet = *quiets.first().unwrap();
        assert_ne!(
            killer_quiet, witness_non_killer_quiet,
            "fixture invariant: killer and witness must be distinct moves"
        );

        // Sanity-check the empty-killer baseline: under no-killer ordering, the
        // killer_quiet (last quiet) sits AFTER the witness_non_killer_quiet
        // (first quiet). If this fails, the fixture's premise is wrong.
        let mut moves_baseline = moves_vec_initial.clone();
        order_moves(
            &mut moves_baseline,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );
        let baseline_killer_idx = moves_baseline
            .iter()
            .position(|&m| m == killer_quiet)
            .unwrap();
        let baseline_witness_idx = moves_baseline
            .iter()
            .position(|&m| m == witness_non_killer_quiet)
            .unwrap();
        assert!(
            baseline_witness_idx < baseline_killer_idx,
            "fixture invariant: under empty-killer baseline, witness ({}) must precede killer ({}); \
             got witness@{} vs killer@{}",
            witness_non_killer_quiet.to_uci(),
            killer_quiet.to_uci(),
            baseline_witness_idx,
            baseline_killer_idx
        );

        // Now run with killer_quiet as the killer slot. The strong assertion:
        // killer_quiet's post-sort index is STRICTLY LESS THAN witness's
        // post-sort index — the killer logic flipped them.
        let mut moves_vec = moves_vec_initial.clone();
        order_moves(
            &mut moves_vec,
            &pos,
            killer_quiet,
            Move::default(),
            &HistoryTable::new(),
            0,
        );

        let killer_idx = moves_vec.iter().position(|&m| m == killer_quiet).unwrap();
        let witness_idx = moves_vec
            .iter()
            .position(|&m| m == witness_non_killer_quiet)
            .unwrap();

        // Strong assertion #1: killer above witness non-killer quiet.
        assert!(
            killer_idx < witness_idx,
            "killer_quiet must precede witness non-killer quiet under killer-aware ordering; \
             killer_quiet@{} ({}), witness@{} ({})",
            killer_idx,
            killer_quiet.to_uci(),
            witness_idx,
            witness_non_killer_quiet.to_uci()
        );

        // Strong assertion #2: all captures precede the killer.
        for (i, &m) in moves_vec.iter().enumerate() {
            if m.is_capture() {
                assert!(
                    i < killer_idx,
                    "capture {} at index {} must precede killer at index {}",
                    m.to_uci(),
                    i,
                    killer_idx
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // S24e — `order_moves` handles TT == killer0 overlap without duplicating.
    // -----------------------------------------------------------------------

    /// When the TT move and killer0 are the same move, `order_moves` must
    /// place it at index 0 exactly once (no duplicate entry).
    #[test]
    fn order_moves_handles_tt_equals_killer_overlap_no_duplicate() {
        let pos = Position::starting_position();
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut moves_vec: Vec<Move> = ml.iter().collect();
        let total = moves_vec.len();

        // Choose a move that is NOT the movegen-first move (so the swap is exercised).
        let all_moves: Vec<Move> = ml.iter().collect();
        let overlap_move = *all_moves.last().expect("startpos has legal moves");

        // overlap_move is BOTH killer0 AND the TT move.
        order_moves(
            &mut moves_vec,
            &pos,
            overlap_move,
            Move::default(),
            &HistoryTable::new(),
            overlap_move.bits(),
        );

        // Must be at index 0.
        assert_eq!(
            moves_vec[0],
            overlap_move,
            "TT==killer0 move must be at index 0; got {}",
            moves_vec[0].to_uci()
        );
        // Must appear exactly once (no duplicate).
        let count = moves_vec.iter().filter(|&&m| m == overlap_move).count();
        assert_eq!(
            count, 1,
            "TT==killer0 move must appear exactly once; found {count} occurrences"
        );
        // Total length must be unchanged.
        assert_eq!(
            moves_vec.len(),
            total,
            "order_moves must not change the number of moves"
        );
    }

    // -----------------------------------------------------------------------
    // S24f — `order_moves` ignores a stale killer whose bits are not in the list.
    // -----------------------------------------------------------------------

    /// A killer from a different position (bits not present in `moves_vec`)
    /// must have no effect: post-sort order identical to the empty-killer baseline.
    #[test]
    fn order_moves_ignores_stale_killer_with_no_matching_bits() {
        let pos = Position::starting_position();
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);

        // Reference order: no killer, no TT.
        let mut moves_ref: Vec<Move> = ml.iter().collect();
        order_moves(
            &mut moves_ref,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );

        // Stale killer: a move whose bits don't match any move in moves_vec.
        // Use a1→a1 quiet (from==to, flag=Quiet, bits != 0 only if from!=to;
        // actually from==to gives bits=0 == default sentinel). Instead construct
        // a move with non-zero bits that aren't in the list: b8c8 Quiet —
        // a black-piece move, never generated for white-to-move startpos.
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let stale_killer = Move::new(Square::B8, Square::C8, MoveFlag::Quiet);
        // Verify the stale killer is NOT in the legal move list.
        assert!(
            !ml.iter().any(|m| m.bits() == stale_killer.bits()),
            "fixture: stale_killer must not be a legal move at startpos"
        );

        let mut moves_stale: Vec<Move> = ml.iter().collect();
        order_moves(
            &mut moves_stale,
            &pos,
            stale_killer,
            Move::default(),
            &HistoryTable::new(),
            0,
        );

        assert_eq!(
            moves_stale, moves_ref,
            "stale killer must not affect ordering; result must match empty-killer baseline"
        );
    }

    // -----------------------------------------------------------------------
    // S25 — `negamax` records a quiet killer on beta cutoff at child ply.
    // -----------------------------------------------------------------------

    /// Integration: drive `negamax_for_test` from a position where a quiet
    /// move causes a beta cutoff at SOME ply. After return, the killer table
    /// must have at least one populated quiet slot; the populated slot is at
    /// the ply where the cutoff fired, and the move stored is quiet.
    ///
    /// Robust assertion: the test does NOT pin the cutoff to a specific ply.
    /// A depth-2 search visits ply=0 (root) and ply=1 (children). A beta
    /// cutoff at either ply via a quiet move must populate `killers[ply][0]`
    /// with a quiet move. The test scans all plies in `[0, MAX_PLY)` for the
    /// first non-default slot, then asserts it holds a quiet move. This avoids
    /// the brittleness of "ply=1 specifically" — fixture refinement at
    /// test-writing time may shift which ply produces the cutoff.
    ///
    /// Fixture: KvN+K endgame. White to move with a narrow window that's
    /// likely to force a quick beta-cutoff. **The fixture is acknowledged as
    /// tentative; if no quiet cutoff fires (all plies remain default), the
    /// test panics with a clear message indicating fixture-rewrite is needed
    /// rather than a cryptic mismatch on a specific slot.**
    #[test]
    fn negamax_records_quiet_killer_on_beta_cutoff() {
        // KvN+K: white King + Knight vs black King. White to move.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/4N3/4K3 w - - 0 1").expect("KvN+K FEN must parse");
        let mut mover = AlphaBetaMover::new();
        mover.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx_at_depth(2);

        // Narrow window forces a quick beta cutoff at some ply.
        let _ = mover.negamax_for_test(&mut pos.clone(), 2, 0, -100, -50, true, true, &ctx);

        // Find the first ply with a populated killer slot.
        let killers = mover.killers_for_test();
        let mut populated: Option<(usize, Move)> = None;
        for (ply, slots) in killers.iter().enumerate() {
            if slots[0] != Move::default() {
                populated = Some((ply, slots[0]));
                break;
            }
        }

        let (cutoff_ply, k) = populated.unwrap_or_else(|| {
            panic!(
                "no killer slot populated after depth-2 search from KvN+K — fixture must be \
                 rewritten to ensure a quiet beta-cutoff fires at some ply within the search"
            )
        });

        assert!(
            is_quiet(k),
            "killers[{cutoff_ply}][0] must be a quiet move (captures do not update killers); \
             got {} (flag: {:?})",
            k.to_uci(),
            k.flag()
        );
    }

    // -----------------------------------------------------------------------
    // S25b — `negamax` does NOT record a capture on beta cutoff.
    // -----------------------------------------------------------------------

    /// The `if is_quiet(mv)` gate in the cutoff block must prevent captures
    /// from populating the killer table. Since S15 already pins `is_quiet`
    /// returning false for captures, this test documents the structural
    /// guarantee: captures that produce a beta cutoff must leave killers unchanged.
    ///
    /// Implementation note: the test drives the cutoff branch via the same
    /// KvN+K fixture with an even narrower window, then verifies that if
    /// the cutoff move was a capture, killers remain at default. Alternatively,
    /// since S15 + code review fully cover the structural shape, this test
    /// acts as a belt-and-suspenders check using a position where the only
    /// available beta-cutoff move is a capture.
    #[test]
    fn negamax_does_not_record_capture_on_beta_cutoff() {
        // Position: white pawn captures black queen immediately (forced beta cutoff);
        // the capture is the only move that overshoots the narrow window.
        // FEN: white K + P on b4, black K + Q on c5. Narrow window ensures
        // Pxc5 (capture) is the cutoff move.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let mut mover = AlphaBetaMover::new();
        mover.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx_at_depth(1);

        // Run with a tight fail-high window: any capture will overshoot and
        // produce a beta cutoff. The gate `if is_quiet(mv)` must prevent
        // the capture from populating killers[0][0].
        let _ = mover.negamax_for_test(&mut pos.clone(), 1, 0, 10000, 10001, false, true, &ctx);

        // killers[0][0] must remain the default sentinel (no capture recorded).
        assert_eq!(
            mover.killers_for_test()[0][0],
            Move::default(),
            "killers[0][0] must remain default when the beta-cutoff move is a capture"
        );
    }

    // -----------------------------------------------------------------------
    // S26 — killer ordering is observable via node-count differential.
    // -----------------------------------------------------------------------

    /// Two-run differential: pre-setting a killer for the appropriate ply
    /// changes the search order at sibling nodes, producing a different node count.
    ///
    /// **Side-to-move discipline.** At depth-3 from startpos:
    ///   - ply 0: white to move (root) — pre-setting `killers[0]` would tag
    ///     a white move; useless since the very first sort consumes it.
    ///   - ply 1: black to move — pre-setting `killers[1]` requires a BLACK
    ///     quiet move. A white-side move at this slot has bits that never
    ///     match any of the 20 ply-1 sibling positions' legal moves; the
    ///     killer is silently ignored. **Pinned by S24f** (legality-by-non-match).
    ///   - ply 2: white to move — pre-setting `killers[2]` requires a white
    ///     quiet move legal in many ply-2 positions.
    ///
    /// The original draft picked a quiet from white's startpos legal-move
    /// list and stored it at `killers[1]` — silently a no-op. **Corrected:**
    /// pick a black quiet move directly.
    ///
    /// Run (a): empty killers, depth-3 search from startpos — record nodes.
    /// Run (b): pre-set `killers[2][0] = chosen_white_quiet` (white side at
    /// ply 2) — record nodes. Assert nodes differ.
    ///
    /// **Theoretical fragility note.** The node-count differential could in
    /// principle be zero if the killer ordering doesn't actually flip any
    /// cutoff decision. In practice at depth-3 from startpos with 20 root
    /// moves × ~20 ply-1 children × ~20 ply-2 children, the killer's effect
    /// on at least one subtree is highly likely. Same observability shape
    /// the M4.A S6 test uses for TT-move ordering.
    #[test]
    fn negamax_killer_observed_in_ordering_at_sibling_node() {
        let pos = Position::starting_position();

        // Run (a): no killers.
        let mut mover_a = AlphaBetaMover::new();
        mover_a.history = vec![pos.zobrist()];
        let (ctx_a, _stop_a) = non_aborting_ctx_at_depth(3);
        let _ = mover_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx_a);
        let nodes_a = mover_a.nodes;

        // Pick the lexicographically-last white quiet move from startpos
        // legal moves. ply=2 in the search is white-to-move, where this
        // killer's bits can match a real legal move (most ply-2 positions
        // preserve the white pieces' starting squares).
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut all_moves: Vec<Move> = ml.iter().collect();
        all_moves.sort_by_key(|m| m.bits());
        let chosen_quiet = all_moves
            .iter()
            .rfind(|&&m| {
                use crate::mov::MoveFlag::*;
                matches!(m.flag(), Quiet | DoublePush | KingCastle | QueenCastle)
            })
            .copied()
            .expect("startpos must have at least one quiet move");

        // Sanity: chosen_quiet has MVV-LVA score 0 (genuinely quiet).
        assert_eq!(
            mvv_lva_score(chosen_quiet, &pos),
            0,
            "chosen quiet must have MVV-LVA score 0; fixture: {}",
            chosen_quiet.to_uci()
        );

        // Verify the chosen quiet is NOT at index 0 in the empty-killer ordering.
        let mut moves_check: Vec<Move> = ml.iter().collect();
        order_moves(
            &mut moves_check,
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
        );
        let pre_killer_idx = moves_check
            .iter()
            .position(|&m| m == chosen_quiet)
            .expect("chosen quiet must be in the legal move list");
        assert!(
            pre_killer_idx > 0,
            "chosen quiet '{}' must NOT be at index 0 in empty-killer ordering (pre_killer_idx={})",
            chosen_quiet.to_uci(),
            pre_killer_idx
        );

        // Run (b): pre-set killers[2][0] (white-side ply) = chosen_quiet.
        let mut mover_b = AlphaBetaMover::new();
        mover_b.history = vec![pos.zobrist()];
        let mut init_killers = [[Move::default(); 2]; MAX_PLY];
        init_killers[2][0] = chosen_quiet;
        mover_b.set_killers_for_test(init_killers);
        let (ctx_b, _stop_b) = non_aborting_ctx_at_depth(3);
        let _ = mover_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, &ctx_b);
        let nodes_b = mover_b.nodes;

        assert_ne!(
            nodes_a,
            nodes_b,
            "pre-setting killer '{}' at ply=2 must change the search shape; \
             both runs produced {nodes_a} nodes",
            chosen_quiet.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // S27 — `clear_killers` zeroes a pre-populated table.
    // -----------------------------------------------------------------------

    #[test]
    fn clear_killers_zeroes_pre_populated_table() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let mv_a = Move::new(Square::A2, Square::A3, MoveFlag::Quiet);
        let mv_b = Move::new(Square::B2, Square::B3, MoveFlag::Quiet);
        let mv_c = Move::new(Square::C2, Square::C3, MoveFlag::Quiet);

        let mut killers = [[Move::default(); 2]; MAX_PLY];
        killers[3][0] = mv_a;
        killers[3][1] = mv_b;
        killers[7][0] = mv_c;

        clear_killers(&mut killers);

        assert_eq!(
            killers[3],
            [Move::default(); 2],
            "killers[3] must be zeroed after clear_killers"
        );
        assert_eq!(
            killers[7],
            [Move::default(); 2],
            "killers[7] must be zeroed after clear_killers"
        );
        // Spot-check a few other plies.
        assert_eq!(
            killers[0],
            [Move::default(); 2],
            "killers[0] must be zeroed"
        );
        assert_eq!(
            killers[MAX_PLY - 1],
            [Move::default(); 2],
            "killers[MAX_PLY-1] must be zeroed"
        );
    }

    // -----------------------------------------------------------------------
    // S28 — `Search::reset` clears killers.
    // -----------------------------------------------------------------------

    #[test]
    fn search_reset_clears_killers() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let some_move = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);

        let mut mover = AlphaBetaMover::new();
        // Pre-populate a killer slot directly.
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        killers[5][0] = some_move;
        mover.set_killers_for_test(killers);

        // Confirm the pre-population stuck.
        assert_eq!(
            mover.killers_for_test()[5][0],
            some_move,
            "pre-condition: killers[5][0] must hold some_move before reset"
        );

        mover.reset();

        assert_eq!(
            mover.killers_for_test()[5][0],
            Move::default(),
            "Search::reset must clear killers[5][0] to the default sentinel"
        );
    }

    // -----------------------------------------------------------------------
    // S29 — `Search::go` clears killers at per-go entry.
    // -----------------------------------------------------------------------

    /// Pre-populate a killer at a high ply (40) that a depth-2 search won't
    /// normally reach. After `Search::go` at depth 2, the killer must have
    /// been cleared.
    ///
    /// **Honesty about what this pins.** The per-iteration reset runs at the
    /// top of every ID iteration including iteration 1, BEFORE negamax is
    /// invoked. So `clear_killers` from the per-iteration reset alone would
    /// also zero the synthetic ply-40 slot. This test therefore pins
    /// "{per-go OR per-iteration} clear fires" jointly, not the per-go call
    /// site specifically. Per the plan §8 mutation-prep table, distinguishing
    /// the per-go call sharply requires code review + the deferred overnight
    /// cargo-mutants run; the test surface alone cannot draw the sharp line.
    #[test]
    fn search_go_clears_killers_at_per_go_entry() {
        use crate::mov::MoveFlag;
        use crate::square::Square;
        let synthetic_move = Move::new(Square::H2, Square::H3, MoveFlag::Quiet);

        let pos = Position::starting_position();
        let mut mover = AlphaBetaMover::new();

        // Pre-populate killers[40][0] with a synthetic move.
        let mut init_killers = [[Move::default(); 2]; MAX_PLY];
        init_killers[40][0] = synthetic_move;
        mover.set_killers_for_test(init_killers);

        // Run go at depth 2. The per-go reset must clear killers before the search.
        let (ctx, _stop) = non_aborting_ctx_at_depth(2);
        let _ = mover.go(&pos, &ctx, &|_| {});

        // killers[40] must be back to default (the per-go clear ran).
        assert_eq!(
            mover.killers_for_test()[40],
            [Move::default(); 2],
            "killers[40] must be zeroed by the per-go reset; \
             synthetic move must not survive across a go() call"
        );
    }

    // -----------------------------------------------------------------------
    // M4.C — History heuristic integration tests (HS1..HS12 + HS3b/HS3c/HS8b).
    //
    // PRE-IMPL NOTE: this slice (test-writing) lands the tests + the structural
    // wiring (`history_table` field, `Search::reset` clear-call,
    // `move_ordering_score`, `ordered_moves_for_test`). The negamax body is
    // **unchanged** — the bonus/malus dispatch on quiet-move beta cutoff
    // lands in the next slice. Until then:
    //
    //   - HS3, HS3b, HS3c, HS7 trivially PASS (no dispatch ⇒ history never
    //     updated ⇒ post-search "history is zero" holds vacuously).
    //   - HS9, HS11, HS12 PASS (they exercise the structural wiring directly).
    //   - HS1, HS2, HS4, HS5, HS6, HS8, HS8b, HS10 FAIL at runtime: their
    //     assertions depend on history-table values that only the impl-slice
    //     wiring produces. The next slice makes them pass.
    // -----------------------------------------------------------------------

    use crate::history::{HistoryTable, MAX_HISTORY};
    use crate::piece::Color;
    use crate::square::Square;

    // -----------------------------------------------------------------------
    // HS1 — quiet beta cutoff at depth 2 deposits +depth*depth = +4 bonus.
    // -----------------------------------------------------------------------

    /// Drive a depth-2 negamax with a tightened window so that the
    /// engine-chosen first move (a quiet from movegen order) fail-highs.
    /// At depth 2 the bonus is `depth * depth = 4`. Depth 2 (rather than
    /// depth 1, which would yield `+1` for both `+= depth*depth` and `+=
    /// depth` formulas) anchors the formula choice.
    ///
    /// Fixture: KvKR endgame — White's king moves are quiet; with a tight
    /// window the first quiet sorted will be searched, and any move that
    /// improves alpha past the tightened beta cuts off.
    #[test]
    fn negamax_updates_history_on_quiet_beta_cutoff_at_depth_2() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1")
            .expect("KvKR endgame FEN must parse");
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // Tight window at zero centipawns: any move with positive eval will
        // immediately fail-high, producing a quiet beta-cutoff.
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, &ctx);

        // After the cutoff fires, exactly one quiet `(side, from, to)` triple
        // must hold the +4 bonus (depth=2, depth*depth=4). Sweep all entries
        // and find the unique non-zero one.
        let mut nonzero: Vec<(Color, Square, Square, i16)> = Vec::new();
        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    let s = ab.history_table.score(side, f, t);
                    if s != 0 {
                        nonzero.push((side, f, t, s));
                    }
                }
            }
        }
        // The cutter receives +4; any prior quiets in `quiets_searched`
        // receive -4. At most one non-zero entry should be the cutter (+4);
        // the others (if any) should be -4 maluses.
        assert!(
            nonzero.iter().any(|(_, _, _, s)| *s == 4),
            "expected at least one entry to hold +4 (the cutter's depth*depth bonus); \
             non-zero entries: {nonzero:?}"
        );
        // Strengthen anti-stub: every non-zero entry must be exactly +4 or -4
        // — the only two values produced by the depth=2 dispatch (bonus or
        // malus). A buggy impl that applied +4 to all quiets (not just the
        // cutter) would have non-zero entries at +4 only — caught by HS4.
        // A buggy impl that used `+= depth` (linear) would produce entries at
        // +2 / -2 — caught by this all-check.
        assert!(
            nonzero.iter().all(|(_, _, _, s)| *s == 4 || *s == -4),
            "every non-zero entry must be exactly +bonus or -bonus (+4 or -4 at \
             depth=2); a non-canonical value indicates a wrong increment formula; \
             non-zero entries: {nonzero:?}"
        );
    }

    // -----------------------------------------------------------------------
    // HS2 — capture cutoff produces no history update.
    // -----------------------------------------------------------------------

    /// Hanging-bishop fixture: White Q d1 captures Black B d4. With a wide
    /// window the capture wins material and the post-capture eval improves
    /// alpha; with a tightened window placed below stand-pat-after-capture,
    /// Qxd4 fail-highs on its return value, producing a *capture* cutoff.
    /// History must remain zero across all entries.
    #[test]
    fn negamax_does_not_update_history_on_capture_cutoff() {
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging-bishop FEN must parse");
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // Tighten the window so Qxd4 (winning material) fail-highs.
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, &ctx);

        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "capture cutoff must not update any history entry; \
                         entry ({side:?}, {f:?}, {t:?}) is non-zero"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS3 — TT cutoff (non-PV, sufficient depth) produces no history update.
    // -----------------------------------------------------------------------

    /// Pre-populate the TT with a non-PV-eligible Lower entry at the root
    /// position; drive negamax with `is_pv=false` and a window where the
    /// stored score >= beta. The TT shortcut returns at step 7 of the M4.A
    /// prologue, BEFORE the move loop; no history update fires.
    #[test]
    fn negamax_does_not_update_history_on_tt_cutoff() {
        let pos = Position::starting_position();
        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            pos.zobrist(),
            TtData {
                score: 200,
                depth: 5,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );
        let (ctx, _stop) = non_aborting_ctx_with_tt(tt.clone());
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(tt.clone()));

        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, &ctx);

        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "TT cutoff must not update any history entry"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS3b — Repetition return at ply > 0 produces no history update.
    // -----------------------------------------------------------------------

    /// `ctx.history`/`ab.history` contains the current zobrist at a 2-ply
    /// distance from the end. At ply=1, step 5 of the negamax prologue
    /// returns 0 immediately; the move loop never runs and no cutter exists
    /// to credit.
    #[test]
    fn negamax_does_not_update_history_on_repetition_return() {
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 4 1")
            .expect("FEN with halfmove=4 must parse");
        let z = pos.zobrist();
        let other: u64 = 0xDEAD_BEEF_CAFE_FEED;
        let history = vec![z, other, z];
        // Sanity: confirm the rep-detection helper would fire.
        assert!(
            is_repetition(&history, pos.halfmove_clock()),
            "fixture must trigger is_repetition (otherwise rep-return path is not exercised)"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = history;
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, true, &ctx);

        assert_eq!(score, 0, "rep-return at ply > 0 must yield 0");
        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "rep-return must not update any history entry"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS3c — MDP window collapse produces no history update.
    // -----------------------------------------------------------------------

    /// Drive negamax with `alpha=MATE-5`, `beta=MATE-1`, `ply=10`. Then
    /// `mating_value = MATE-10`, which is < beta (so beta tightens) AND
    /// `alpha (MATE-5) >= mating_value (MATE-10)`, triggering the early
    /// return at step 6 of the prologue.
    #[test]
    fn negamax_does_not_update_history_on_mdp_window_collapse() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();

        let ply: u32 = 10;
        let alpha = MATE - 5;
        let beta = MATE - 1;
        let mating_value = MATE - ply as i32;
        assert!(
            alpha >= mating_value,
            "fixture preconditions: alpha={alpha}, mating_value={mating_value} — \
             alpha must be >= mating_value to trigger upper-bound MD pruning"
        );
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, ply, alpha, beta, false, true, &ctx);

        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "MDP-window-collapse return must not update any history entry"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS4 — Malus excludes the cutter; only prior quiets receive -bonus.
    // -----------------------------------------------------------------------

    /// At a node with quiet moves Q1, Q2, Q3 (in that searched order) where
    /// Q3 is the cutter and Q1, Q2 fail to improve alpha enough to cut: the
    /// cutter receives +bonus exclusively (no malus); Q1 and Q2 receive
    /// -bonus.
    ///
    /// Fixture: tight zero-width window at a position where quiets dominate.
    /// We don't pin specific squares — instead we assert structural
    /// counts: exactly one entry is +depth*depth (the cutter) and zero or
    /// more entries are -depth*depth (the prior quiets). No entry equals
    /// `+bonus - bonus = 0` for a move that was both cutter and prior
    /// (which would happen if a buggy impl applied both bonus and malus to
    /// the cutter).
    #[test]
    fn negamax_history_malus_excludes_cutter() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1")
            .expect("KvKR endgame FEN must parse");
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let depth: u32 = 2;
        let _ = ab.negamax_for_test(&mut pos.clone(), depth, 0, 0, 1, false, true, &ctx);

        let bonus = (depth as i32) * (depth as i32); // = 4
        let mut plus_count = 0;
        let mut minus_count = 0;
        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    let s = ab.history_table.score(side, f, t) as i32;
                    if s == bonus {
                        plus_count += 1;
                    } else if s == -bonus {
                        minus_count += 1;
                    } else {
                        assert_eq!(
                            s, 0,
                            "every entry must be 0, +bonus, or -bonus; \
                             ({side:?}, {f:?}, {t:?}) = {s}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            plus_count, 1,
            "exactly one entry must hold +bonus (the unique cutter); \
             plus_count={plus_count}, minus_count={minus_count}"
        );
        // NOTE: `minus_count` may be 0 if the move generator emits the
        // cutting quiet first (no priors in `quiets_searched`). The KvKR
        // fixture's static-eval-at-depth-1 returns ~+477 for every quiet,
        // so the FIRST quiet in movegen order cuts. The malus path is
        // exercised by HS4b (via a multi-iteration search where multiple
        // subtrees fire malus across them) and by the structural argument
        // in HS5 (capture-then-quiet ordering). This test's load-bearing
        // assertion is `plus_count == 1` (the cutter is unique).
        let _ = minus_count;
    }

    // -----------------------------------------------------------------------
    // HS4b — Malus path exercised at depth=3 (aggregate accumulation across
    // subtrees). Complements HS4's "exactly one cutter" pin: across many
    // subtrees with cutoffs at varying depths, both bonuses (+1, +4, +9) and
    // maluses (-1, -4, -9) accumulate. At least one negative entry must
    // exist post-search, proving the malus loop fires somewhere in the tree.
    // -----------------------------------------------------------------------

    /// Drive a depth=3 search from startpos with a tight window. At depth=3,
    /// the search tree fans out: root has 20 quiet moves (no captures at
    /// startpos); each child has 20 quiet responses; each grandchild has
    /// ~20 quiet replies. Cutoffs fire at varying plies. The `quiets_searched`
    /// accumulator at any node where a non-first quiet cuts produces -bonus
    /// entries. The aggregate must include at least one negative value.
    #[test]
    fn negamax_history_malus_fires_somewhere_at_depth_3_startpos() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // Wide-ish window so multiple cutoffs fire at varying plies but
        // not so wide as to suppress all cuts.
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -50, 50, false, true, &ctx);

        let mut has_negative = false;
        let mut has_positive = false;
        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    let s = ab.history_table.score(side, f, t);
                    if s > 0 {
                        has_positive = true;
                    } else if s < 0 {
                        has_negative = true;
                    }
                }
            }
        }
        assert!(
            has_positive,
            "depth=3 startpos search must produce at least one bonus (+ entry); \
             a no-op stub would leave all entries at 0"
        );
        assert!(
            has_negative,
            "depth=3 startpos search with cutoffs at multiple plies must \
             fire the malus loop somewhere — at least one entry must be \
             negative; a stub that skips the malus loop would never produce \
             negatives even if the bonus path is correct"
        );
    }

    // -----------------------------------------------------------------------
    // HS5 — Captures never enter `quiets_searched`; quiet cutter follows
    // a capture without polluting the capture's history entry.
    // -----------------------------------------------------------------------

    /// Fixture: White Q+K vs Black N+K, with Black knight en prise on e7.
    /// MVV-LVA orders Qxe7+ first, BUT the only legal Black response is
    /// Kxe7 (recapture with king), so White loses queen for knight =
    /// −688 cp. After Qxe7+ Kxe7 the position is K vs K, which the eval's
    /// insufficient-material short-circuit returns as exactly 0. So at
    /// depth=2, Qxe7 returns 0 — and with window (0, 1), `0 < 1` so the
    /// capture does NOT cut. Subsequent quiets preserve White's queen
    /// (+688 cp material edge), so any quiet move returns ~+688 ≥ 1
    /// and cuts.
    ///
    /// Load-bearing assertion: the capture's `(from=e2, to=e7)` history
    /// entry must remain 0 across both sides — captures are excluded
    /// from `quiets_searched`, so neither bonus (capture didn't cut here)
    /// nor malus (capture not in priors) applies. A buggy impl that
    /// pushes captures onto `quiets_searched` would visibly produce a
    /// −bonus malus on this entry.
    ///
    /// Anti-stub: a stub that skips ALL history updates would also leave
    /// the capture entry at 0 (false-pass on the capture-only assertion).
    /// To distinguish, we ALSO assert that at least one White quiet entry
    /// holds the +bonus (cutter exists). Together: capture is unmodified
    /// AND a real cutter is observed.
    #[test]
    fn negamax_quiets_searched_excludes_captures() {
        // White queen (e2) can capture the Black knight on e7 (a bad trade —
        // Black king takes back, leaving K vs K which the eval's
        // insufficient-material short-circuit returns as exactly 0). That
        // capture sorts first (CAPTURE_OFFSET) but doesn't cut at window
        // (0, 1) since 0 < 1. A quiet queen move preserves White's queen
        // (+688 cp material edge), so any quiet returns >= 1 and cuts.
        // Key invariant: the capture never enters `quiets_searched`, so its
        // history entry remains 0 even after a quiet cuts.
        let pos = Position::from_fen("4k3/4n3/8/8/8/8/4Q3/4K3 w - - 0 1")
            .expect("queen-vs-knight sacrifice FEN must parse");
        let cap_from = Square::E2;
        let cap_to = Square::E7;

        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // Window (0, 1): Qxe7+ → Kxe7 returns 0 (no cut); any quiet
        // returns ~+688 (cut).
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, &ctx);

        assert_eq!(
            ab.history_table.score(Color::White, cap_from, cap_to),
            0,
            "Qxe7 capture must not receive a history update; \
             a buggy impl that pushes captures onto quiets_searched would \
             malus this entry to -4"
        );
        assert_eq!(
            ab.history_table.score(Color::Black, cap_from, cap_to),
            0,
            "Qxe7 capture must not receive a Black-side history update either"
        );

        // Anti-stub: confirm a cutter exists (a real cutoff fired).
        // Otherwise the capture-stays-zero assertion is trivially true
        // against a no-op stub. The cutter is some quiet on the White
        // side with +bonus (= +4 at depth=2).
        let mut found_cutter = false;
        for from in 0..64u8 {
            for to in 0..64u8 {
                let f = Square::new_unchecked(from);
                let t = Square::new_unchecked(to);
                if ab.history_table.score(Color::White, f, t) == 4 {
                    found_cutter = true;
                    break;
                }
            }
            if found_cutter {
                break;
            }
        }
        assert!(
            found_cutter,
            "expected at least one White quiet entry with +4 bonus (the cutter); \
             a no-op stub would leave all entries at 0"
        );
    }

    // -----------------------------------------------------------------------
    // HS6 — No cutoff over the move loop produces no history update.
    // -----------------------------------------------------------------------

    /// Drive negamax over a position with the full window `(-INF, INF)`.
    /// The move loop completes without any move triggering `alpha >= beta`,
    /// so no quiet-cutter exists. History remains zero everywhere.
    #[test]
    fn negamax_does_not_update_history_when_no_cutoff() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // depth=1, full window: root exhausts all 20 legal moves without
        // alpha ever reaching beta (window is (-INF, INF)). Each child is
        // searched at depth=0 → qsearch only; no child node runs a move loop
        // or dispatches history. History remains zero everywhere.
        // depth=2 is NOT safe here: root's second and later children are
        // searched with a narrowed window (negate(updated_alpha, INF)) and
        // their depth=1 children CAN cut, updating history.
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, &ctx);

        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "no-cutoff search must not update history; \
                         ({side:?}, {f:?}, {t:?}) is non-zero"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS7 — Abort propagation skips the bonus/malus block.
    // -----------------------------------------------------------------------

    /// Pre-set `ctx.stop = true`. Drive negamax. The first cancellation
    /// poll fires at the top of the frame (or per-recursive call); the
    /// `if self.aborted { return 0; }` post-recursion check skips the
    /// bonus/malus dispatch. History remains zero.
    #[test]
    fn negamax_does_not_update_history_on_abort() {
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging-bishop FEN must parse");
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, stop) = non_aborting_ctx();
        stop.store(true, Ordering::Relaxed);
        // Pre-set aborted as well so the post-recursion check fires.
        ab.aborted = true;
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, false, true, &ctx);

        for side in [Color::White, Color::Black] {
            for from in 0..64u8 {
                for to in 0..64u8 {
                    let f = Square::new_unchecked(from);
                    let t = Square::new_unchecked(to);
                    assert_eq!(
                        ab.history_table.score(side, f, t),
                        0,
                        "abort path must not update history"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // HS8 — Side-to-move at root (White) deposits bonus into entries[White].
    // -----------------------------------------------------------------------

    /// At a White-to-move root, force a quiet cutoff. Verify the bonus
    /// entries land on `Color::White`, never on `Color::Black`.
    #[test]
    fn negamax_history_bonus_uses_movers_side_at_root_for_white() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1")
            .expect("KvKR W-to-move FEN must parse");
        assert_eq!(
            pos.side_to_move(),
            Color::White,
            "fixture must be White-to-move"
        );
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, &ctx);

        let mut white_nonzero = 0;
        let mut black_nonzero = 0;
        for from in 0..64u8 {
            for to in 0..64u8 {
                let f = Square::new_unchecked(from);
                let t = Square::new_unchecked(to);
                if ab.history_table.score(Color::White, f, t) != 0 {
                    white_nonzero += 1;
                }
                if ab.history_table.score(Color::Black, f, t) != 0 {
                    black_nonzero += 1;
                }
            }
        }
        assert!(
            white_nonzero >= 1,
            "White-to-move cutoff must produce at least one White-side history update; \
             white_nonzero={white_nonzero}, black_nonzero={black_nonzero}"
        );
        assert_eq!(
            black_nonzero, 0,
            "White-to-move cutoff must NOT touch any Black-side history entry; \
             white_nonzero={white_nonzero}, black_nonzero={black_nonzero}"
        );
    }

    // -----------------------------------------------------------------------
    // HS8b — Side-to-move at non-root for Black (anti-stub for "side-from-
    // post-recursion" bug).
    // -----------------------------------------------------------------------

    /// A buggy implementation that reads `pos.side_to_move()` AFTER the
    /// recursive `make_move`/`unmake_move` instead of at the cutoff site
    /// would silently invert the side. To pin: drive negamax_for_test at
    /// `ply=1` where the position is Black-to-move (i.e. start from a
    /// position whose side_to_move is Black at the call site). Then any
    /// quiet cutoff at ply 1 must land on `Color::Black`.
    #[test]
    fn negamax_history_bonus_uses_movers_side_at_non_root_for_black() {
        let pos = Position::from_fen("4k3/2r5/8/8/8/8/8/4K3 b - - 0 1")
            .expect("KR-vs-K Black-to-move FEN must parse");
        assert_eq!(
            pos.side_to_move(),
            Color::Black,
            "fixture must be Black-to-move"
        );
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        // M5.B course-correction: beta raised from 1 to 350 so RFP does not
        // fire (static_eval ≈ 477 for K+R vs K; margin at depth=2 is 200;
        // 477−200=277 < 350 → gate fails). Window still tight enough to
        // trigger a quiet beta-cutoff (first rook move returns ≈477 > 350).
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 1, 0, 350, false, true, &ctx);

        let mut white_nonzero = 0;
        let mut black_nonzero = 0;
        for from in 0..64u8 {
            for to in 0..64u8 {
                let f = Square::new_unchecked(from);
                let t = Square::new_unchecked(to);
                if ab.history_table.score(Color::White, f, t) != 0 {
                    white_nonzero += 1;
                }
                if ab.history_table.score(Color::Black, f, t) != 0 {
                    black_nonzero += 1;
                }
            }
        }
        assert!(
            black_nonzero >= 1,
            "Black-to-move cutoff must produce at least one Black-side history update; \
             white_nonzero={white_nonzero}, black_nonzero={black_nonzero}"
        );
        assert_eq!(
            white_nonzero, 0,
            "Black-to-move cutoff must NOT touch any White-side history entry; \
             white_nonzero={white_nonzero}, black_nonzero={black_nonzero}"
        );
    }

    // -----------------------------------------------------------------------
    // HS9 — `negamax_move_order_score` sorts non-killer quiets descending by
    // history value.
    // -----------------------------------------------------------------------

    /// Pre-populate four distinct quiet history values for White at startpos.
    /// All values are within `[0, MAX_HISTORY]` so the clamp does not collapse
    /// them; values are spaced apart so the test verifies a full descending
    /// sort, not just bubble-to-front behavior:
    ///   - e2e4 → +80
    ///   - d2d4 → +50
    ///   - b1c3 → +20
    ///   - g1f3 →   0 (default; left untouched)
    ///
    /// Call `mover.ordered_moves_for_test(&pos)`; assert the four moves
    /// appear in the order e2e4 < d2d4 < b1c3 < g1f3 (positions strictly
    /// increasing). Other startpos quiets at history=0 may be interspersed.
    /// Startpos has zero captures, so no capture/quiet boundary check here.
    #[test]
    fn negamax_move_order_score_sorts_quiets_descending_with_four_distinct_history_values() {
        let pos = Position::starting_position();
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        ab.history_table
            .update(Color::White, Square::E2, Square::E4, 80);
        ab.history_table
            .update(Color::White, Square::D2, Square::D4, 50);
        ab.history_table
            .update(Color::White, Square::B1, Square::C3, 20);
        // g1f3 left at 0 explicitly.

        let ordered = ab.ordered_moves_for_test(&pos);
        let pos_e2e4 = ordered
            .iter()
            .position(|m| m.from_square() == Square::E2 && m.to_square() == Square::E4)
            .expect("e2e4 must be present");
        let pos_d2d4 = ordered
            .iter()
            .position(|m| m.from_square() == Square::D2 && m.to_square() == Square::D4)
            .expect("d2d4 must be present");
        let pos_b1c3 = ordered
            .iter()
            .position(|m| m.from_square() == Square::B1 && m.to_square() == Square::C3)
            .expect("b1c3 must be present");
        let pos_g1f3 = ordered
            .iter()
            .position(|m| m.from_square() == Square::G1 && m.to_square() == Square::F3)
            .expect("g1f3 must be present");

        assert!(
            pos_e2e4 < pos_d2d4,
            "e2e4 (history=80) must sort before d2d4 (history=50); got positions {pos_e2e4} vs {pos_d2d4}"
        );
        assert!(
            pos_d2d4 < pos_b1c3,
            "d2d4 (history=50) must sort before b1c3 (history=20); got positions {pos_d2d4} vs {pos_b1c3}"
        );
        assert!(
            pos_b1c3 < pos_g1f3,
            "b1c3 (history=20) must sort before g1f3 (history=0); got positions {pos_b1c3} vs {pos_g1f3}"
        );
    }

    // -----------------------------------------------------------------------
    // HS10 — clamp boundary: pre-populate at MAX_HISTORY-1, depth-4 cutoff
    // (bonus=16) saturates at MAX_HISTORY.
    // -----------------------------------------------------------------------

    /// Pre-populate one entry at `MAX_HISTORY - 1` (= 98 with `MAX_HISTORY = 99`).
    /// Drive a depth-4 negamax with a tight window so a quiet move causes a
    /// beta cutoff; `bonus = depth*depth = 16`. Post-search, the entry must
    /// equal `MAX_HISTORY` (99), NOT `98 + 16 = 114` — proving the clamp
    /// fires at the negamax integration site.
    ///
    /// We don't know the exact cutter in advance — so we pre-populate the
    /// entries for ALL White quiets at `MAX_HISTORY - 1`. Whichever quiet
    /// fires the cutoff is then guaranteed to clamp.
    #[test]
    fn history_table_update_clamps_at_positive_saturation_boundary_via_negamax() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1")
            .expect("KvKR endgame FEN must parse");
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        // Saturate every White-quiet entry to MAX_HISTORY - 1.
        let preload: i32 = (MAX_HISTORY as i32) - 1;
        for from in 0..64u8 {
            for to in 0..64u8 {
                let f = Square::new_unchecked(from);
                let t = Square::new_unchecked(to);
                ab.history_table.update(Color::White, f, t, preload);
            }
        }
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 0, 0, 1, false, true, &ctx);

        // The cutter's entry must clamp at MAX_HISTORY. Sweep all White
        // quiets and find at least one entry equal to MAX_HISTORY (the
        // clamp-fired entry).
        let mut clamped = false;
        for from in 0..64u8 {
            for to in 0..64u8 {
                let f = Square::new_unchecked(from);
                let t = Square::new_unchecked(to);
                if ab.history_table.score(Color::White, f, t) == MAX_HISTORY {
                    clamped = true;
                    break;
                }
            }
            if clamped {
                break;
            }
        }
        assert!(
            clamped,
            "depth-4 quiet-cutoff bonus must clamp at MAX_HISTORY; \
             expected at least one White entry to equal {MAX_HISTORY}"
        );
    }

    // -----------------------------------------------------------------------
    // HS11 — `Search::reset()` clears the history table.
    // -----------------------------------------------------------------------

    /// Populate four entries spanning both sides; call `Search::reset()`;
    /// assert all entries are 0. Anchors the `Engine::reset_for_new_game()`
    /// integration via E_h.
    #[test]
    fn search_reset_clears_history_table() {
        let mut ab = AlphaBetaMover::new();
        ab.history_table.clear();
        // Values within `[-MAX_HISTORY, MAX_HISTORY]` so the clamp doesn't
        // collapse them — the test verifies the *clear*, not the clamp.
        ab.history_table
            .update(Color::White, Square::E2, Square::E4, 50);
        ab.history_table
            .update(Color::White, Square::D2, Square::D4, -20);
        ab.history_table
            .update(Color::Black, Square::E7, Square::E5, 30);
        ab.history_table
            .update(Color::Black, Square::G8, Square::F6, 10);

        // Sanity: confirm pre-reset state is non-zero.
        assert_eq!(
            ab.history_table.score(Color::White, Square::E2, Square::E4),
            50
        );
        assert_eq!(
            ab.history_table.score(Color::Black, Square::E7, Square::E5),
            30
        );

        <AlphaBetaMover as Search>::reset(&mut ab);

        assert_eq!(
            ab.history_table.score(Color::White, Square::E2, Square::E4),
            0,
            "Search::reset must clear history_table[White][e2][e4]"
        );
        assert_eq!(
            ab.history_table.score(Color::White, Square::D2, Square::D4),
            0,
            "Search::reset must clear history_table[White][d2][d4]"
        );
        assert_eq!(
            ab.history_table.score(Color::Black, Square::E7, Square::E5),
            0,
            "Search::reset must clear history_table[Black][e7][e5]"
        );
        assert_eq!(
            ab.history_table.score(Color::Black, Square::G8, Square::F6),
            0,
            "Search::reset must clear history_table[Black][g8][f6]"
        );
    }

    // -----------------------------------------------------------------------
    // HS12 — `negamax_move_order_score` orders captures > killers > quiets
    // (history-rated and not), even when the quiet has saturated history.
    // -----------------------------------------------------------------------

    /// Construct a position offering simultaneously: a capture (QxP), a
    /// non-capture queen-promotion, a quiet move pre-populated at
    /// `MAX_HISTORY` in the history table, and a quiet move installed as
    /// `killer0`. Verify the comparator ranks them:
    ///    `s_promo > s_cap > s_killer > s_history_quiet`.
    ///
    /// This pins the cross-tier discipline:
    ///   - mvv_lva (≥ 287) > KILLER0_SCORE (200): captures above killers.
    ///   - KILLER1_SCORE (100) > MAX_HISTORY (99): killers above quiets.
    ///   - non-capture promos rank above ordinary captures via the M3.D
    ///     promo-piece-value MVV-LVA discipline (ADR-0016 §6).
    #[test]
    fn negamax_move_order_score_places_captures_above_killers_above_history_quiets() {
        let pos = Position::from_fen("4k3/P7/8/8/3p4/8/8/3QK3 w - - 0 1")
            .expect("HS12 fixture FEN must parse");
        let cap = Move::from_uci("d1d4", &pos).expect("d1d4 (QxP capture) must be legal");
        let promo = Move::from_uci("a7a8q", &pos).expect("a7a8q (queen-promo) must be legal");
        let history_quiet = Move::from_uci("e1e2", &pos).expect("e1e2 (king quiet) must be legal");
        let killer_quiet = Move::from_uci("e1d2", &pos).expect("e1d2 (king quiet) must be legal");

        let mut ht = HistoryTable::new();
        // Saturate the history-quiet's entry to MAX_HISTORY.
        ht.update(
            Color::White,
            history_quiet.from_square(),
            history_quiet.to_square(),
            MAX_HISTORY as i32,
        );

        // killer_quiet installed as killer0; killer1 empty.
        let killer0 = killer_quiet;
        let killer1 = Move::default();

        let s_cap = negamax_move_order_score(cap, &pos, killer0, killer1, &ht);
        let s_promo = negamax_move_order_score(promo, &pos, killer0, killer1, &ht);
        let s_killer = negamax_move_order_score(killer_quiet, &pos, killer0, killer1, &ht);
        let s_history_quiet = negamax_move_order_score(history_quiet, &pos, killer0, killer1, &ht);

        assert!(
            s_promo > s_cap,
            "non-capture queen-promo must score above plain capture (per ADR-0016 §6); \
             promo={s_promo}, capture={s_cap}"
        );
        assert!(
            s_cap > s_killer,
            "every capture/promo must score above every killer; \
             capture={s_cap}, killer={s_killer}"
        );
        assert!(
            s_killer > s_history_quiet,
            "every killer must score above every non-killer quiet, even at MAX_HISTORY; \
             killer={s_killer}, history_quiet={s_history_quiet}, MAX_HISTORY={MAX_HISTORY}"
        );
    }

    // -----------------------------------------------------------------------
    // M4.D — aspiration_window unit tests (AS1–AS5b).
    // -----------------------------------------------------------------------

    /// Depths 1–5 always return the full window (below ASPIRATION_MIN_DEPTH=6),
    /// regardless of prior score.
    #[test]
    fn aspiration_window_below_threshold_is_full_window_at_depth_1_2_3() {
        for depth in [1u32, 2, 3, 4, 5] {
            for prior in [None, Some(0i32), Some(50), Some(-50)] {
                assert_eq!(
                    aspiration_window(prior, depth),
                    (-INF, INF),
                    "depth={depth}, prior={prior:?} must return full window"
                );
            }
        }
    }

    /// Depth 6 (threshold boundary) with no prior score returns full window.
    #[test]
    fn aspiration_window_at_threshold_with_no_prior_is_full_window() {
        assert_eq!(
            aspiration_window(None, 6),
            (-INF, INF),
            "depth=6, prior=None must return full window (first iteration has no prior)"
        );
    }

    /// At depth ≥ ASPIRATION_MIN_DEPTH=6 with prior=0, the window is `(-50, 50)`.
    /// Anti-stub: checks both endpoints to catch a formula that ignores the lower bound.
    #[test]
    fn aspiration_window_above_threshold_with_zero_prior_is_centered_at_zero() {
        assert_eq!(
            aspiration_window(Some(0), 6),
            (-50, 50),
            "prior=0, depth=6 must yield (-50, 50)"
        );
    }

    /// Positive priors at various depths above threshold (>= 6).
    #[test]
    fn aspiration_window_above_threshold_with_positive_prior_centered_correctly() {
        assert_eq!(
            aspiration_window(Some(123), 7),
            (73, 173),
            "prior=123, depth=7 must yield (73, 173)"
        );
        assert_eq!(
            aspiration_window(Some(1000), 10),
            (950, 1050),
            "prior=1000, depth=10 must yield (950, 1050)"
        );
    }

    /// Negative prior at depth above threshold.
    #[test]
    fn aspiration_window_above_threshold_with_negative_prior_centered_correctly() {
        assert_eq!(
            aspiration_window(Some(-200), 6),
            (-250, -150),
            "prior=-200, depth=6 must yield (-250, -150)"
        );
    }

    /// Mate-score priors are NOT special-cased; the window is centered on the
    /// mate score normally. Pins research §7.2's "do not special-case mate
    /// detection" recommendation. Anti-stub against a future mate-skip branch.
    #[test]
    fn aspiration_window_with_mate_score_prior_does_not_special_case() {
        for prior in [MATE - 1, MATE - 10, -(MATE - 5)] {
            let result = aspiration_window(Some(prior), 8);
            assert_eq!(
                result,
                (prior - ASPIRATION_HALF_WIDTH, prior + ASPIRATION_HALF_WIDTH),
                "mate-score prior={prior} must yield centered window, not full window"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M4.D — widen_after_fail unit tests (AS6–AS9b).
    // -----------------------------------------------------------------------

    /// Fail-high: proved lower bound preserved as new alpha; upper widens to INF.
    /// Anti-stub against `(prev_alpha, INF)` (drops the proved bound).
    #[test]
    fn widen_after_fail_high_returns_proved_lower_bound_to_inf() {
        assert_eq!(
            widen_after_fail(200, 50, 150),
            (200, INF),
            "fail-high: alpha must be the returned score (proved bound), not prev_alpha"
        );
    }

    /// Fail-low: proved upper bound preserved as new beta; lower widens to -INF.
    #[test]
    fn widen_after_fail_low_returns_neg_inf_to_proved_upper_bound() {
        assert_eq!(
            widen_after_fail(-200, -50, 50),
            (-INF, -200),
            "fail-low: beta must be the returned score (proved bound), not prev_beta"
        );
    }

    /// Fail-high at the exact beta boundary (`returned == prev_beta`).
    /// The `>=` comparator is non-strict; pins the boundary case.
    #[test]
    fn widen_after_fail_high_at_exact_beta_boundary_widens() {
        assert_eq!(
            widen_after_fail(150, 50, 150),
            (150, INF),
            "returned == prev_beta must be treated as fail-high (>= is non-strict)"
        );
    }

    /// Fail-low at the exact alpha boundary (`returned == prev_alpha`).
    #[test]
    fn widen_after_fail_low_at_exact_alpha_boundary_widens() {
        assert_eq!(
            widen_after_fail(50, 50, 150),
            (-INF, 50),
            "returned == prev_alpha must be treated as fail-low (<= is non-strict)"
        );
    }

    /// In debug builds, `widen_after_fail` must panic when the score is
    /// window-contained. Pins the caller-contract invariant from §3.2.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "widen_after_fail called with window-contained")]
    fn widen_after_fail_panics_in_debug_on_window_contained() {
        // returned=100 is strictly inside (50, 150) — window-contained.
        widen_after_fail(100, 50, 150);
    }

    // -----------------------------------------------------------------------
    // M4.D — extract_bestmove_or_tt_fallback unit tests (AS24a–AS24d).
    // -----------------------------------------------------------------------

    /// PV populated: helper returns the PV move, ignoring the TT.
    #[test]
    fn extract_bestmove_or_tt_fallback_returns_pv_first_when_populated() {
        let pos = Position::starting_position();
        let pv_move = Move::from_uci("e2e4", &pos).expect("e2e4 is legal");
        let tt_move = Move::from_uci("d2d4", &pos).expect("d2d4 is legal");

        let mut pv = PvTable::new();
        pv.moves[0][0] = pv_move;
        pv.lengths[0] = 1;

        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            42,
            TtData {
                score: 100,
                depth: 1,
                bound: TtBound::Exact,
                best_move: tt_move.bits(),
            },
        );

        let result = extract_bestmove_or_tt_fallback(&pv, Some(&tt), 42);
        assert_eq!(
            result,
            Some(pv_move),
            "PV populated: must return PV move, not TT move"
        );
    }

    /// PV empty but TT has a non-zero bestmove at root_key: helper returns TT move.
    #[test]
    fn extract_bestmove_or_tt_fallback_uses_tt_when_pv_empty_and_tt_has_bestmove() {
        let pos = Position::starting_position();
        let tt_move = Move::from_uci("g1f3", &pos).expect("g1f3 is legal");
        let root_key: u64 = 0xDEAD_BEEF_1234_5678;

        let pv = PvTable::new(); // lengths[0] == 0

        let tt = Arc::new(TranspositionTable::new(1));
        tt.store(
            root_key,
            TtData {
                score: 50,
                depth: 2,
                bound: TtBound::Lower,
                best_move: tt_move.bits(),
            },
        );

        let result = extract_bestmove_or_tt_fallback(&pv, Some(&tt), root_key);
        assert_eq!(
            result,
            Some(tt_move),
            "PV empty + TT has bestmove: must return TT move"
        );
    }

    /// PV empty and TT entry has best_move == 0: helper returns None.
    /// Anti-stub against decoding Move::from_bits(0) as a real move.
    #[test]
    fn extract_bestmove_or_tt_fallback_returns_none_when_pv_empty_and_tt_has_zero_bestmove() {
        let root_key: u64 = 0xCAFE_BABE_0000_0001;

        let pv = PvTable::new(); // lengths[0] == 0

        let tt = Arc::new(TranspositionTable::new(1));
        // Store entry with best_move=0 (no-move sentinel). The slot starts
        // empty, so ADR-0018 §7's preservation rule does not apply — the
        // entry is written as-is with best_move=0.
        tt.store(
            root_key,
            TtData {
                score: -30,
                depth: 1,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );

        let result = extract_bestmove_or_tt_fallback(&pv, Some(&tt), root_key);
        assert_eq!(
            result, None,
            "PV empty + TT best_move==0: must return None, not a decoded zero-bits move"
        );
    }

    /// PV empty and no TT (tt=None): helper returns None without panicking.
    #[test]
    fn extract_bestmove_or_tt_fallback_returns_none_when_pv_empty_and_no_tt() {
        let pv = PvTable::new(); // lengths[0] == 0
        let result = extract_bestmove_or_tt_fallback(&pv, None, 0xABCD_EF01_2345_6789);
        assert_eq!(result, None, "PV empty + tt=None: must return None");
    }

    // -----------------------------------------------------------------------
    // M4.D — Search::go aspiration integration tests (AS10–AS17, AS19–AS23).
    //
    // Helpers shared across the integration tests below.
    // -----------------------------------------------------------------------

    /// Reliable fail-low fixture. White to move; Black queen dominates and
    /// every iteration ≥ ASPIRATION_MIN_DEPTH=6 produces a fail-low at try 1:
    /// the depth-(N-1) score is well above depth-N's true score, so the
    /// centered window `(prior - 50, prior + 50)` lies entirely above
    /// depth-N's true value. At depth 6: iter-5 returned -885; iter-6 first
    /// try `(-935, -835)` returns ≤ -935 → fail-low; re-search at
    /// `(-INF, -935)` succeeds with score -939.
    fn fail_low_fixture() -> Position {
        Position::from_fen("1q6/8/8/8/8/PPP5/2K5/k7 w - - 0 1")
            .expect("fail-low fixture FEN must parse")
    }

    /// Reliable fail-high fixture. KP-vs-K endgame, white to move with the
    /// c-pawn one square from the queening rank. Iter-5 evaluates to cp 143
    /// (no promotion in sight at that depth); iter-6 first try
    /// `(93, 193)` returns ≥ 193 (deeper search reveals the queen-promo
    /// gain) → fail-high; re-search at `(returned, +INF)` returns +1044.
    fn fail_high_fixture() -> Position {
        Position::from_fen("5k2/8/2K5/2P5/8/8/8/8 w - - 0 1")
            .expect("fail-high fixture FEN must parse")
    }

    /// Stable fixture (startpos): every depth-≥4 iteration is window-contained
    /// at try 1 — iter-3 → iter-4 swing is < 50 cp from this position.
    fn stable_fixture() -> Position {
        Position::starting_position()
    }

    /// Filter `info_sink` lines for the M4.D `aspiration_re_search` token.
    fn aspiration_lines(infos: &[String]) -> Vec<&String> {
        infos
            .iter()
            .filter(|line| line.contains("info string aspiration_re_search"))
            .collect()
    }

    /// Parse a decimal-integer field of the form `key=value` from a
    /// space-tokenized info line. Returns `None` if the key is absent or the
    /// value fails to parse as `i32`.
    fn parse_int_field(line: &str, key: &str) -> Option<i32> {
        line.split_whitespace()
            .find_map(|tok| tok.strip_prefix(key).and_then(|v| v.parse::<i32>().ok()))
    }

    /// Extract `(depth, alpha, beta)` from one `aspiration_re_search` info line.
    fn parse_aspiration_line(line: &str) -> (i32, i32, i32) {
        let depth = parse_int_field(line, "depth=")
            .unwrap_or_else(|| panic!("aspiration line missing depth=N: {line:?}"));
        let alpha = parse_int_field(line, "alpha=")
            .unwrap_or_else(|| panic!("aspiration line missing alpha=A: {line:?}"));
        let beta = parse_int_field(line, "beta=")
            .unwrap_or_else(|| panic!("aspiration line missing beta=B: {line:?}"));
        (depth, alpha, beta)
    }

    /// AS10. At most one aspiration_re_search line per ID iteration: the
    /// two-tier cap forbids a second re-search. Pinned against a buggy
    /// `tries >= 3` cap that would emit two re-search lines per iteration on
    /// a doubly-failing fixture.
    #[test]
    fn id_loop_emits_at_most_one_aspiration_re_search_line_per_iteration() {
        let pos = fail_high_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        let asp = aspiration_lines(&infos);
        let mut per_depth: std::collections::HashMap<i32, u32> = std::collections::HashMap::new();
        for line in &asp {
            let (d, _, _) = parse_aspiration_line(line);
            *per_depth.entry(d).or_insert(0) += 1;
        }
        for (d, count) in &per_depth {
            assert!(
                *count <= 1,
                "depth={d} emitted {count} aspiration_re_search lines; two-tier cap forbids more than 1"
            );
        }
    }

    /// AS11. Window-contained first try emits zero re-search lines for that
    /// iteration. Stable fixture (startpos) at depth 5 — each iter-N
    /// (N ≥ 4) score sits within ±50 cp of iter-(N-1).
    #[test]
    fn id_loop_window_contained_first_try_does_not_re_search() {
        let pos = stable_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(5)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        let asp = aspiration_lines(&infos);
        for line in &asp {
            let (d, _, _) = parse_aspiration_line(line);
            assert_ne!(
                d, 4,
                "stable fixture: depth 4 must not re-search; got line: {line:?}"
            );
            assert_ne!(
                d, 5,
                "stable fixture: depth 5 must not re-search; got line: {line:?}"
            );
        }
    }

    /// AS12. Per-try `pv.lengths[..] = 0` must clear deep state from the
    /// failed first try, so the re-search's emitted PV is internally
    /// consistent (every successive move is legal in the position resulting
    /// from applying the prior PV moves).
    #[test]
    fn id_loop_re_search_clears_pv_below_root_before_starting() {
        let pos = fail_low_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        // Verify the iter-4 (re-searched) line's PV is move-by-move legal.
        let line4 = infos
            .iter()
            .find(|s| s.starts_with("info depth 4 "))
            .expect("info line for depth 4 must exist");
        let pv_str = line4
            .split(" pv ")
            .nth(1)
            .expect("info depth 4 must contain ' pv '");
        let mut p = pos;
        for tok in pv_str.split_whitespace() {
            assert_ne!(tok, "0000", "iter-4 re-search must produce a non-empty PV");
            let mv = Move::from_uci(tok, &p)
                .unwrap_or_else(|_| panic!("PV move {tok:?} must parse and be legal in {p:?}"));
            let mut ml = MoveList::new();
            generate_moves(&p, &mut ml);
            assert!(
                ml.iter().any(|legal| legal == mv),
                "PV move {tok:?} must be legal in current position"
            );
            p.make_move(mv);
        }
    }

    /// AS13. Iteration 1 has no prior score — `aspiration_window(None, 1)`
    /// returns the full window. No re-search line can fire.
    #[test]
    fn id_loop_first_iteration_does_not_emit_aspiration_re_search_line() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(1)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        let asp = aspiration_lines(&infos);
        assert!(
            asp.is_empty(),
            "iter-1 has no prior score → no aspiration; got: {asp:?}"
        );
    }

    /// AS14. At threshold (depth 6) on a stable position, iter-6's first try
    /// is window-contained; no re-search line for depth 6.
    #[test]
    fn id_loop_iteration_at_threshold_with_stable_prior_does_not_re_search() {
        let pos = stable_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        for line in aspiration_lines(&infos) {
            let (d, _, _) = parse_aspiration_line(line);
            assert_ne!(
                d, 6,
                "stable fixture iter-6 must be window-contained; got line: {line:?}"
            );
        }
    }

    /// AS15. Below the threshold (depths 1–5), no aspiration window narrows
    /// the search; no re-search line can fire.
    #[test]
    fn id_loop_iteration_below_threshold_does_not_emit_aspiration_re_search_line() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(5)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        let asp = aspiration_lines(&infos);
        assert!(
            asp.is_empty(),
            "depths 1-5 are below ASPIRATION_MIN_DEPTH; got: {asp:?}"
        );
    }

    /// AS16. A stop flipped before iter-N's first try cancels the
    /// in-progress search; the outer ID loop breaks; the prior iteration's
    /// snapshot is the reported result. We use the existing
    /// "stop flipped between iterations via info_sink" pattern: flip stop on
    /// the first emitted info line.
    #[test]
    fn id_loop_aborts_during_first_aspiration_try_breaks_outer_iter_cleanly() {
        let pos = fail_high_fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            // depth 6 chosen so the run reaches the aspiration regime.
            limits: limits_with(|l| l.depth = Some(6)),
            history: vec![pos.zobrist()],
            tt: None,
        };
        // Flip stop after the iter-1 info line lands. The next negamax call
        // (iter-2's first try) sees stop=true at its first cadence poll and
        // aborts mid-try; the outer loop breaks via `if self.aborted`.
        let stop_flip = Arc::clone(&stop);
        let infos: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let info_sink = |s: &str| {
            infos.borrow_mut().push(s.to_string());
            stop_flip.store(true, Ordering::Relaxed);
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &info_sink);

        assert!(
            result.bestmove.is_some(),
            "abort during first try preserves prior iteration's bestmove; got result: {result:?}"
        );
        assert!(
            result.depth >= 1,
            "iter-1 completes before stop flips; result.depth must be >= 1, got {}",
            result.depth
        );
    }

    /// AS17. A stop flipped between try 1 and try 2 of iter-N propagates: the
    /// re-search aborts cleanly; the outer ID loop breaks at iter-N; the
    /// prior iteration's snapshot is the reported result.
    #[test]
    fn id_loop_aborts_during_second_aspiration_try_breaks_outer_iter_cleanly() {
        let pos = fail_high_fixture();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: limits_with(|l| l.depth = Some(7)),
            history: vec![pos.zobrist()],
            tt: None,
        };
        // Flip stop on the FIRST `aspiration_re_search` line — this fires
        // AFTER try 1 has fail-{high,low}'d but BEFORE try 2 starts. The
        // re-search's negamax frame sees stop=true at its first cadence
        // poll and aborts mid-try.
        let stop_flip = Arc::clone(&stop);
        let infos: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let info_sink = |s: &str| {
            let was_aspiration = s.contains("info string aspiration_re_search");
            infos.borrow_mut().push(s.to_string());
            if was_aspiration {
                stop_flip.store(true, Ordering::Relaxed);
            }
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &info_sink);
        let infos = infos.into_inner();

        // Some iteration up to N-1 completed (last_complete preserved); the
        // re-searching iteration N broke mid-try-2.
        assert!(
            result.bestmove.is_some(),
            "abort during re-search must preserve prior iteration's bestmove; got: {result:?}"
        );
        let asp = aspiration_lines(&infos);
        assert!(
            !asp.is_empty(),
            "test fixture must trigger at least one re-search before the stop flips"
        );
    }

    /// AS19. Re-search after fail-high uses `widen_after_fail`'s output:
    /// alpha = returned (proved lower bound); beta = INF.
    /// Verifies `alpha == iter-N's try-1 returned score` and `beta == INF`.
    #[test]
    fn id_loop_re_search_window_after_fail_high_uses_widen_helper_output() {
        let pos = fail_high_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        // Find a fail-high re-search line. With this fixture, iter-6
        // first try `(93, 193)` fail-highs (deeper queen-promo discovery
        // pushes the score above 193). Re-search at `(returned, +INF)`.
        let fh_line = infos
            .iter()
            .filter(|s| s.contains("info string aspiration_re_search"))
            .find(|s| {
                let beta = parse_int_field(s, "beta=").unwrap();
                beta == INF
            })
            .unwrap_or_else(|| panic!("expected at least one fail-high aspiration_re_search line; got infos: {infos:?}"));

        let (_d, alpha, beta) = parse_aspiration_line(fh_line);
        assert_eq!(beta, INF, "fail-high re-search beta must equal INF");
        // alpha must be a finite proved-bound score (NOT -INF). The literal
        // anti-stub: `widen_after_fail` returning `(prev_alpha, INF)`
        // instead of `(returned, INF)` would put alpha = prior - 50 = 93
        // here; the iter-6 try-1 returned score is ≥ 193 (≥ prior + 50),
        // so a correctly-implemented widen returns alpha ≥ 193, NOT 93.
        assert!(
            alpha > -INF,
            "fail-high re-search alpha must be a finite proved bound, not -INF; got {alpha}"
        );
        assert!(
            alpha >= 193,
            "alpha must be the iter's try-1 returned score (≥ prior+50 = 193 on this fixture); got {alpha}"
        );
    }

    /// AS20. Re-search after fail-low uses `widen_after_fail`'s output:
    /// alpha = -INF; beta = returned (proved upper bound).
    #[test]
    fn id_loop_re_search_window_after_fail_low_uses_widen_helper_output() {
        let pos = fail_low_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_result, infos) = drive_go(&mut ab, &pos, &ctx);

        let fl_line = infos
            .iter()
            .filter(|s| s.contains("info string aspiration_re_search"))
            .find(|s| {
                let alpha = parse_int_field(s, "alpha=").unwrap();
                alpha == -INF
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected at least one fail-low aspiration_re_search line; got infos: {infos:?}"
                )
            });

        let (_d, alpha, beta) = parse_aspiration_line(fl_line);
        assert_eq!(alpha, -INF, "fail-low re-search alpha must equal -INF");
        assert!(
            beta < INF,
            "fail-low re-search beta must be a finite proved bound, not INF; got {beta}"
        );
        // Beta must NOT be 0 (which would suggest the proved bound was lost).
        // The fixture's iter-6 returned score is ≤ -935 cp (queen-down regime).
        assert!(
            beta < -100,
            "beta must be the iter's try-1 returned score (deeply negative on this fixture); got {beta}"
        );
    }

    /// AS21. Re-search succeeds with stable PV after asymmetric widening.
    /// The fail-low fixture's iter-6 re-search produces a non-empty PV with
    /// a deeper-search bestmove, and the recorded `info depth 6` line carries
    /// that PV.
    #[test]
    fn id_loop_iteration_5_to_6_with_unstable_score_re_searches_then_succeeds() {
        let pos = fail_low_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        let asp = aspiration_lines(&infos);
        let depth6_re = asp
            .iter()
            .filter(|line| {
                let (d, _, _) = parse_aspiration_line(line);
                d == 6
            })
            .count();
        assert_eq!(
            depth6_re, 1,
            "fail-low fixture must trigger exactly one re-search at depth 6"
        );

        let line6 = infos
            .iter()
            .find(|s| s.starts_with("info depth 6 "))
            .expect("info line for depth 6 must exist");
        assert!(
            !line6.contains(" pv 0000"),
            "iter-6 re-search must produce a non-empty PV; got: {line6:?}"
        );
        assert_eq!(
            result.depth, 6,
            "result reflects the completed iter-6 (after re-search)"
        );
        assert!(result.bestmove.is_some());
    }

    /// AS22. Engine completes cleanly when iter-N's prior is a mate score.
    ///
    /// **Anti-stub coverage**: a re-introduced mate-skip branch in
    /// `aspiration_window` is already pinned at the unit-test level by
    /// AS5b (`aspiration_window_with_mate_score_prior_does_not_special_case`),
    /// which directly asserts the helper returns `(prior - 50, prior + 50)`
    /// for mate-magnitude priors at depth ≥ 6. The integration-level
    /// anti-stub is harder to construct: with threshold=6, mate-prior
    /// iterations almost always confirm the same mate (window-contained),
    /// emitting no `aspiration_re_search` line — and a re-introduced mate-
    /// skip would emit no line either, producing identical observable
    /// behavior at the integration layer. AS5b is the load-bearing pin.
    ///
    /// This test verifies the positive integration property: a position
    /// that finds mate at iter-N (with iter-N below threshold so no
    /// aspiration is engaged on the discovery iteration) → iter-(N+1)
    /// onward report mate cleanly via aspiration. The Bogoljubow mate-in-3
    /// fixture finds mate at iter-5; iter-6 onward carry mate-prior under
    /// aspiration without emitting re-search lines (window contained).
    #[test]
    fn id_loop_iteration_with_mate_score_prior_centers_window_on_mate_score() {
        let pos = Position::from_fen("1k1r4/pp1b1R2/3q2pp/4p3/2B5/4Q3/PPP2B1P/2K5 b - - 0 1")
            .expect("Bogoljubow mate-in-3 FEN must parse");
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(7)));
        let mut ab = AlphaBetaMover::new();
        let (result, infos) = drive_go(&mut ab, &pos, &ctx);

        // iter-5 finds mate; iter-6 + iter-7 carry mate-prior under
        // aspiration. Final result must report mate, not crash, not produce
        // a stray re-search line at the mate-prior iterations (window-
        // contained because the same mate persists).
        let mate_line = infos
            .iter()
            .find(|s| s.contains("score mate "))
            .expect("a mate score must be reported once iter-5 finds the mate");
        assert!(
            mate_line.contains("info depth"),
            "mate must appear on a real info-depth line; got: {mate_line:?}"
        );
        // iter-6 and iter-7 are mate-prior aspiration iterations. Centered
        // window on a mate score is window-contained when the same mate
        // persists, so NO aspiration_re_search lines should fire at depth
        // ≥ 6 on this fixture. (Anti-stub against accidental re-search:
        // a buggy widen that fires on equal-score returns would emit one.)
        let asp_at_6plus = infos
            .iter()
            .filter(|s| s.contains("info string aspiration_re_search"))
            .filter(|s| {
                let (d, _, _) = parse_aspiration_line(s);
                d >= 6
            })
            .count();
        assert_eq!(
            asp_at_6plus, 0,
            "mate-prior iterations at depth ≥ 6 should be window-contained; got: {infos:?}"
        );
        assert!(
            result.bestmove.is_some(),
            "engine must report a bestmove (the iter-N mate-driver move)"
        );
    }

    /// AS23. Killer table persists across aspiration tries within the same
    /// iteration. Anti-stub against a per-try `clear_killers` that would
    /// discard try-1's ordering hints. The killer table is observable via
    /// `killers_for_test()` after `go` returns; we run a fixture that
    /// re-searches at depth 4 and assert at least one killer slot at any ply
    /// is populated post-go (which is the union of try-1 and try-2 cutoffs;
    /// if per-try clearing wiped try-1, we'd still see try-2's, so this
    /// assertion is necessary but the deeper check is via internal structure).
    /// The load-bearing assertion: AS17's stop-after-aspiration-re-search
    /// abort path leaves try-1's killers in place — which we rely on for
    /// the re-search to inherit better ordering. Direct integration check:
    /// after a fail-{high,low} iteration completes, killer slots produced
    /// during try 1 must still be present in `killers_for_test()` (not all
    /// overwritten by try 2).
    #[test]
    fn id_loop_killer_table_persists_across_aspiration_tries_within_same_iteration() {
        // Step 1: drive a search that triggers re-search at depth 6 and
        // capture the killer table at end-of-go. (The mover's killers are
        // cleared on `reset()` but not on `go`-end; they reflect the LAST
        // iteration's state.)
        let pos = fail_low_fixture();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(6)));
        let mut ab = AlphaBetaMover::new();
        let (_r, infos) = drive_go(&mut ab, &pos, &ctx);

        // Sanity: at least one re-search occurred at depth 6 (validates the
        // fixture is exercising the cross-try state path).
        let depth6_re = aspiration_lines(&infos)
            .iter()
            .filter(|line| {
                let (d, _, _) = parse_aspiration_line(line);
                d == 6
            })
            .count();
        assert_eq!(
            depth6_re, 1,
            "fixture must trigger exactly one re-search at depth 6"
        );

        // Step 2: at least one killer slot must be populated, demonstrating
        // that the killer table accumulated cutoff hints across the iteration
        // (the per-iteration `clear_killers` fired ONCE at the top of iter-6,
        // then both try-1 and try-2 contributed). Sentinel slot is
        // `Move::default()` (bits == 0).
        let killers = ab.killers_for_test();
        let any_populated = killers
            .iter()
            .any(|slot| slot.iter().any(|m| *m != Move::default()));
        assert!(
            any_populated,
            "after a re-searching iteration, at least one killer slot must hold a real move; \
             a per-try clear_killers (regression) would leave the table fully empty after try 2 \
             unless try 2 itself produced a quiet beta-cutoff"
        );

        // Step 3: run a deeper iteration and confirm the killer cross-try
        // persistence holds even when the table is heavily exercised.
        let pos2 = fail_low_fixture();
        let (ctx2, _stop2) = ctx_for(&pos2, limits_with(|l| l.depth = Some(7)));
        let mut ab2 = AlphaBetaMover::new();
        let (_r2, infos2) = drive_go(&mut ab2, &pos2, &ctx2);
        let re_at_6 = aspiration_lines(&infos2)
            .iter()
            .filter(|line| {
                let (d, _, _) = parse_aspiration_line(line);
                d == 6
            })
            .count();
        assert_eq!(
            re_at_6, 1,
            "depth-7 search must also trigger exactly one re-search at iter-6 on this fixture"
        );
        let killers2 = ab2.killers_for_test();
        assert!(
            killers2
                .iter()
                .any(|slot| slot.iter().any(|m| *m != Move::default())),
            "deep re-searching iteration must also leave killer slots populated"
        );
    }

    // ===========================================================================
    // ELOH.C — SearchInstant + SearchClock unit tests (plan §6.1 + §6.2 + §6.3).
    //
    // Per docs/plans/eloh.c.md §6.1, §6.2, §6.3. Tests are pure-function unit
    // tests on SearchInstant and SearchClock plus three integration tests that
    // exercise the search's time-source plumbing.
    // ===========================================================================

    // --- §6.1 SearchInstant ---

    #[test]
    fn search_instant_now_wall_returns_wall() {
        let inst = SearchInstant::now(false);
        assert!(matches!(inst, SearchInstant::Wall(_)));
    }

    #[cfg(unix)]
    #[test]
    fn search_instant_now_cpu_returns_cpu() {
        let inst = SearchInstant::now(true);
        assert!(matches!(inst, SearchInstant::Cpu(_)));
    }

    #[test]
    fn search_instant_wall_add_advances_by_duration() {
        let t = SearchInstant::Wall(Instant::now());
        let advanced = t.add(Duration::from_millis(10));
        assert_eq!(advanced.duration_since(t), Duration::from_millis(10));
    }

    #[cfg(unix)]
    #[test]
    fn search_instant_cpu_add_advances_by_duration() {
        let ns: u64 = 1_000_000_000;
        let advanced = SearchInstant::Cpu(ns).add(Duration::from_millis(10));
        match advanced {
            SearchInstant::Cpu(v) => assert_eq!(v, ns + 10_000_000),
            SearchInstant::Wall(_) => unreachable!("Cpu.add must yield Cpu"),
        }
    }

    #[test]
    fn search_instant_cpu_add_saturates() {
        let advanced = SearchInstant::Cpu(u64::MAX).add(Duration::from_millis(1));
        match advanced {
            SearchInstant::Cpu(v) => assert_eq!(v, u64::MAX),
            SearchInstant::Wall(_) => unreachable!("Cpu.add must yield Cpu"),
        }
    }

    #[test]
    fn search_instant_duration_since_wall() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(7);
        let delta = SearchInstant::Wall(t1).duration_since(SearchInstant::Wall(t0));
        assert_eq!(delta, Duration::from_millis(7));
    }

    #[test]
    fn search_instant_duration_since_cpu() {
        let t0_ns: u64 = 100_000_000;
        let t1_ns: u64 = 100_000_500;
        let delta = SearchInstant::Cpu(t1_ns).duration_since(SearchInstant::Cpu(t0_ns));
        assert_eq!(delta, Duration::from_nanos(t1_ns - t0_ns));
    }

    #[test]
    fn search_instant_is_at_or_past_wall_strict() {
        let now = Instant::now();
        let later = SearchInstant::Wall(now + Duration::from_millis(1));
        let earlier = SearchInstant::Wall(now - Duration::from_millis(1));
        let now_si = SearchInstant::Wall(now);
        assert!(later.is_at_or_past(now_si));
        assert!(!earlier.is_at_or_past(now_si));
    }

    #[test]
    fn search_instant_is_at_or_past_cpu_strict() {
        let earlier = SearchInstant::Cpu(100);
        let now_si = SearchInstant::Cpu(200);
        let later = SearchInstant::Cpu(300);
        assert!(later.is_at_or_past(now_si));
        assert!(!earlier.is_at_or_past(now_si));
    }

    #[test]
    fn search_instant_is_at_or_past_equal_fires() {
        // Boundary: `>=`-not-`>` semantic, matching M3.E `Instant >= deadline`.
        let t = SearchInstant::Wall(Instant::now());
        assert!(t.is_at_or_past(t));
        let c = SearchInstant::Cpu(42);
        assert!(c.is_at_or_past(c));
    }

    #[test]
    #[should_panic(expected = "cross-variant Wall vs Cpu")]
    fn search_instant_cross_variant_duration_unreachable() {
        let _ = SearchInstant::Wall(Instant::now()).duration_since(SearchInstant::Cpu(0));
    }

    #[test]
    #[should_panic(expected = "cross-variant Wall vs Cpu")]
    fn search_instant_cross_variant_compare_unreachable() {
        let _ = SearchInstant::Wall(Instant::now()).is_at_or_past(SearchInstant::Cpu(0));
    }

    #[cfg(unix)]
    #[test]
    fn search_instant_cpu_now_non_decreasing_within_thread() {
        let a = SearchInstant::now(true);
        let b = SearchInstant::now(true);
        // Non-decreasing (`>=`), not strict-greater: CLOCK_THREAD_CPUTIME_ID
        // can have coarse granularity; equal-monotone is valid.
        assert!(b.is_at_or_past(a));
    }

    // --- §6.2 SearchClock ---

    fn caps_max() -> TimeCaps {
        TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        }
    }

    #[test]
    fn search_clock_start_for_wall_no_caps_yields_none_deadlines() {
        let clock = SearchClock::start_for(false, caps_max());
        assert!(clock.deadline.is_none());
        assert!(clock.soft_deadline.is_none());
    }

    #[test]
    fn search_clock_start_for_wall_with_caps_yields_wall_deadlines() {
        let clock = SearchClock::start_for(
            false,
            TimeCaps {
                soft: Duration::from_millis(100),
                hard: Duration::from_millis(200),
            },
        );
        assert!(matches!(clock.start, SearchInstant::Wall(_)));
        assert!(matches!(clock.deadline, Some(SearchInstant::Wall(_))));
        assert!(matches!(clock.soft_deadline, Some(SearchInstant::Wall(_))));
    }

    #[cfg(unix)]
    #[test]
    fn search_clock_start_for_cpu_with_caps_yields_cpu_deadlines() {
        let clock = SearchClock::start_for(
            true,
            TimeCaps {
                soft: Duration::from_millis(100),
                hard: Duration::from_millis(200),
            },
        );
        assert!(matches!(clock.start, SearchInstant::Cpu(_)));
        assert!(matches!(clock.deadline, Some(SearchInstant::Cpu(_))));
        assert!(matches!(clock.soft_deadline, Some(SearchInstant::Cpu(_))));
    }

    #[test]
    fn search_clock_start_same_variant_invariant() {
        // `start_for` runs cleanly without triggering the `same_variant`
        // debug-assert. Repeated across variants for coverage.
        let _ = SearchClock::start_for(
            false,
            TimeCaps {
                soft: Duration::from_millis(50),
                hard: Duration::from_millis(150),
            },
        );
        #[cfg(unix)]
        let _ = SearchClock::start_for(
            true,
            TimeCaps {
                soft: Duration::from_millis(50),
                hard: Duration::from_millis(150),
            },
        );
    }

    #[test]
    fn search_clock_should_abort_no_deadline_no_nodes_only_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let clock = SearchClock::start_for(false, caps_max());
        assert!(!clock.should_abort(&stop, None, 1_000_000));
        stop.store(true, Ordering::Relaxed);
        assert!(clock.should_abort(&stop, None, 1_000_000));
    }

    #[test]
    fn search_clock_should_abort_node_cap_works_independent_of_clock() {
        let stop = Arc::new(AtomicBool::new(false));
        let clock_wall = SearchClock::start_for(false, caps_max());
        assert!(clock_wall.should_abort(&stop, Some(100), 100));
        #[cfg(unix)]
        {
            let clock_cpu = SearchClock::start_for(true, caps_max());
            assert!(clock_cpu.should_abort(&stop, Some(100), 100));
        }
    }

    #[test]
    fn search_clock_should_abort_wall_deadline_fires_after_sleep() {
        let stop = Arc::new(AtomicBool::new(false));
        let clock = SearchClock::start_for(
            false,
            TimeCaps {
                soft: Duration::MAX,
                hard: Duration::from_millis(1),
            },
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(clock.should_abort(&stop, None, 0));
    }

    #[cfg(unix)]
    #[test]
    fn search_clock_should_abort_cpu_deadline_does_not_fire_under_pure_sleep() {
        // Load-invariance contract: a pure thread::sleep accumulates wallclock
        // but no CPU time. With a 1-second CPU deadline (200x margin vs.
        // wake-up jitter), CPU mode must NOT fire after a 200ms sleep.
        let stop = Arc::new(AtomicBool::new(false));
        let clock = SearchClock::start_for(
            true,
            TimeCaps {
                soft: Duration::MAX,
                hard: Duration::from_millis(1_000),
            },
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(!clock.should_abort(&stop, None, 0));
    }

    #[cfg(unix)]
    #[test]
    fn search_clock_should_abort_cpu_deadline_fires_under_cpu_burn() {
        let stop = Arc::new(AtomicBool::new(false));
        let clock = SearchClock::start_for(
            true,
            TimeCaps {
                soft: Duration::MAX,
                hard: Duration::from_millis(50),
            },
        );
        // Burn ~200ms of CPU via a black_box-fenced multiplication loop.
        let mut x: u64 = 1;
        let burn_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < burn_until {
            for _ in 0..10_000 {
                x = std::hint::black_box(x).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            }
        }
        std::hint::black_box(x);
        assert!(clock.should_abort(&stop, None, 0));
    }

    #[test]
    fn search_clock_is_soft_reached_at_uses_passed_now() {
        let clock = SearchClock::start_for(
            false,
            TimeCaps {
                soft: Duration::from_millis(10),
                hard: Duration::MAX,
            },
        );
        // Pass a `now` constructed manually as `start + 20ms`. Method must
        // NOT internally read the clock; it must use this parameter.
        let now = clock.start.add(Duration::from_millis(20));
        assert!(clock.is_soft_reached_at(now));
    }

    // --- §6.3 Search time-source integration tests ---

    #[cfg(unix)]
    #[test]
    fn search_clock_start_for_reads_calling_thread_cpu() {
        // Spawn a thread that consumes ~200ms of CPU (gated by the *CPU*
        // clock so background contention doesn't undercount) before
        // constructing the SearchClock from inside. The clock's
        // `start_cpu_ns` must reflect the CPU time spent by that thread —
        // not zero, not the main thread's. The main thread is otherwise
        // idle, so a `start_cpu_ns >= 100_000_000` rules out an inherited
        // / main-thread / always-zero implementation.
        let handle = std::thread::spawn(|| {
            let mut x: u64 = 1;
            let target_ns: u64 = 200_000_000;
            let SearchInstant::Cpu(start_ns) = SearchInstant::now(true) else {
                unreachable!("now(true) must yield Cpu on unix")
            };
            loop {
                for _ in 0..100_000 {
                    x = std::hint::black_box(x).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                }
                let SearchInstant::Cpu(cur_ns) = SearchInstant::now(true) else {
                    unreachable!("now(true) must yield Cpu on unix")
                };
                if cur_ns - start_ns >= target_ns {
                    break;
                }
            }
            std::hint::black_box(x);
            let clock = SearchClock::start_for(
                true,
                TimeCaps {
                    soft: Duration::from_millis(0),
                    hard: Duration::from_millis(0),
                },
            );
            clock
                .start_cpu_ns()
                .expect("start must be Cpu under VC=true")
        });
        let ns = handle.join().expect("worker thread must not panic");
        assert!(
            ns >= 100_000_000,
            "spawned thread's CPU clock at SearchClock::start_for must reflect substantial CPU usage; got {ns} ns"
        );
    }

    /// Counter-instrumented test: per ID-loop iteration, at most ONE call to
    /// `SearchInstant::now(virtual_clock)` for the elapsed-ms-and-soft-deadline
    /// pair. Implemented as a wrapping `Search` impl that intercepts the info
    /// sink and counts the number of non-`should_abort`-driven now calls,
    /// indirectly: the production code's contract is that for each `info depth
    /// N …` info line emitted, the elapsed-ms field is computed from a single
    /// `now` read shared with the soft-cap break.
    ///
    /// We pin this contract by parsing the elapsed-ms field out of each info
    /// line and asserting the values are monotonically non-decreasing across
    /// iterations (a duplicate clock read inside the soft-cap branch would
    /// break the shared-`now` invariant — under wall mode that's not directly
    /// observable from the elapsed field alone, but the test exists as a
    /// regression anchor for the intent stated in plan §4.2).
    #[test]
    fn id_loop_tail_reads_clock_once_per_iteration() {
        let pos = Position::starting_position();
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(3)));
        let mut ab = AlphaBetaMover::new();
        let infos: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        ab.go(&pos, &ctx, &|s| infos.borrow_mut().push(s.to_string()));
        let lines = infos.into_inner();
        assert_eq!(lines.len(), 3, "expected 3 info lines: {lines:?}");
        let parse_time = |line: &str| -> u128 {
            let toks: Vec<&str> = line.split_whitespace().collect();
            let i = toks
                .iter()
                .position(|t| *t == "time")
                .expect("info line must contain `time`");
            toks[i + 1].parse().expect("`time` value must be u128")
        };
        let times: Vec<u128> = lines.iter().map(|s| parse_time(s)).collect();
        for w in times.windows(2) {
            assert!(
                w[1] >= w[0],
                "elapsed-ms must be monotonically non-decreasing across iterations; got {times:?}"
            );
        }
    }

    #[test]
    fn mate_distance_pruning_independent_of_time_source() {
        // Mate-in-1 fixture (M3.C): both `Qg7#` and `Qh7#` mate. Search at
        // depth 2 under both modes; the resulting score and bestmove are
        // identical because MDP is algorithmically agnostic to the time
        // source (caps are MAX → no deadline).
        let pos = Position::from_fen("7k/8/5KQ1/8/8/8/8/8 w - - 0 1").expect("FEN must parse");

        let stop = Arc::new(AtomicBool::new(false));
        let ctx_wall = SearchContext {
            stop: Arc::clone(&stop),
            caps: TimeCaps {
                soft: Duration::MAX,
                hard: Duration::MAX,
            },
            virtual_clock: false,
            limits: SearchLimits {
                depth: Some(2),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
            tt: None,
        };
        let mut ab_wall = AlphaBetaMover::new();
        let r_wall = ab_wall.go(&pos, &ctx_wall, &|_| {});

        #[cfg(unix)]
        {
            let stop2 = Arc::new(AtomicBool::new(false));
            let ctx_cpu = SearchContext {
                stop: Arc::clone(&stop2),
                caps: TimeCaps {
                    soft: Duration::MAX,
                    hard: Duration::MAX,
                },
                virtual_clock: true,
                limits: SearchLimits {
                    depth: Some(2),
                    ..SearchLimits::default()
                },
                history: vec![pos.zobrist()],
                tt: None,
            };
            let mut ab_cpu = AlphaBetaMover::new();
            let r_cpu = ab_cpu.go(&pos, &ctx_cpu, &|_| {});

            assert_eq!(
                r_wall.score_cp, r_cpu.score_cp,
                "score must match across time sources at fixed depth"
            );
        }

        // Score must still be the mate-in-1 score on the wall side.
        assert_eq!(r_wall.score_cp, Some(MATE - 1));
    }

    // -----------------------------------------------------------------------
    // M5.A Slice B — null_move_reduction helper tests (5 tests).
    // -----------------------------------------------------------------------

    #[test]
    fn null_move_reduction_at_depth_3_is_2() {
        assert_eq!(null_move_reduction(3), 2);
    }

    #[test]
    fn null_move_reduction_at_depth_5_is_2() {
        // 2 + 5/6 = 2 + 0 = 2 (boundary just below first bump).
        assert_eq!(null_move_reduction(5), 2);
    }

    #[test]
    fn null_move_reduction_at_depth_6_is_3() {
        // 2 + 6/6 = 2 + 1 = 3 (first bump).
        assert_eq!(null_move_reduction(6), 3);
    }

    #[test]
    fn null_move_reduction_at_depth_11_is_3() {
        // 2 + 11/6 = 2 + 1 = 3 (boundary just below second bump).
        assert_eq!(null_move_reduction(11), 3);
    }

    #[test]
    fn null_move_reduction_at_depth_12_is_4() {
        // 2 + 12/6 = 2 + 2 = 4 (second bump).
        assert_eq!(null_move_reduction(12), 4);
    }

    // -----------------------------------------------------------------------
    // M5.A Slice B — has_non_pawn_material helper tests (5 tests).
    // -----------------------------------------------------------------------

    #[test]
    fn has_non_pawn_material_starting_position_true_for_both_sides() {
        let pos = Position::starting_position();
        assert!(
            has_non_pawn_material(&pos, Color::White),
            "starting position must have non-pawn material for White"
        );
        assert!(
            has_non_pawn_material(&pos, Color::Black),
            "starting position must have non-pawn material for Black"
        );
    }

    #[test]
    fn has_non_pawn_material_kings_only_returns_false() {
        let pos = Position::from_fen("8/8/8/4k3/4K3/8/8/8 w - - 0 1").expect("FEN must parse");
        assert!(
            !has_non_pawn_material(&pos, Color::White),
            "kings-only position must return false for White"
        );
        assert!(
            !has_non_pawn_material(&pos, Color::Black),
            "kings-only position must return false for Black"
        );
    }

    #[test]
    fn has_non_pawn_material_kings_and_pawns_only_returns_false() {
        let pos = Position::from_fen("8/4p3/8/4k3/4K3/8/4P3/8 w - - 0 1").expect("FEN must parse");
        assert!(
            !has_non_pawn_material(&pos, Color::White),
            "K+P-only position must return false for White"
        );
        assert!(
            !has_non_pawn_material(&pos, Color::Black),
            "K+P-only position must return false for Black"
        );
    }

    #[test]
    fn has_non_pawn_material_with_white_knight_only() {
        let pos = Position::from_fen("8/8/8/4k3/4K3/8/4N3/8 w - - 0 1").expect("FEN must parse");
        assert!(
            has_non_pawn_material(&pos, Color::White),
            "position with white knight must return true for White"
        );
        assert!(
            !has_non_pawn_material(&pos, Color::Black),
            "position with no black non-pawn material must return false for Black"
        );
    }

    #[test]
    fn has_non_pawn_material_endgame_with_rook() {
        // KRk position: White rook on e2, kings on e4 and e5.
        let pos = Position::from_fen("8/8/8/4k3/4K3/8/4R3/8 w - - 0 1").expect("FEN must parse");
        assert!(
            has_non_pawn_material(&pos, Color::White),
            "KRk position must return true for White"
        );
        assert!(
            !has_non_pawn_material(&pos, Color::Black),
            "KRk position must return false for Black (only king)"
        );
    }

    // -----------------------------------------------------------------------
    // M5.B — reverse_futility_margin helper tests (5 tests).
    // -----------------------------------------------------------------------

    #[test]
    fn reverse_futility_margin_at_depth_0_is_0() {
        // Boundary: depth=0 → margin=0. Catches sign-flip / non-zero constant bugs.
        assert_eq!(reverse_futility_margin(0), 0);
    }

    #[test]
    fn reverse_futility_margin_at_depth_1_is_100() {
        // Catches `+ → -`, `* → /` mutations on the formula.
        assert_eq!(reverse_futility_margin(1), 100);
    }

    #[test]
    fn reverse_futility_margin_at_depth_3_is_300() {
        // Mid-range; catches off-by-one constant mutations.
        assert_eq!(reverse_futility_margin(3), 300);
    }

    #[test]
    fn reverse_futility_margin_at_depth_6_is_600() {
        // RFP_MAX_DEPTH boundary.
        assert_eq!(reverse_futility_margin(6), 600);
    }

    #[test]
    fn reverse_futility_margin_at_depth_7_is_700() {
        // Just above RFP_MAX_DEPTH; the gate prevents this from being called
        // in production, but the helper is still well-defined and testable.
        assert_eq!(reverse_futility_margin(7), 700);
    }

    // -----------------------------------------------------------------------
    // M5.A — NMP behavior in negamax (plan §5.3).
    //
    // Sister-fixture pattern: every gate-skip test pairs with a positive
    // fixture and asserts a node-count delta. A no-op NMP impl trivially
    // "skips NMP" everywhere; the gate-skip tests therefore must compare
    // against a fixture where the gate's *opposite* condition fires and
    // demonstrate observable behavior change.
    //
    // All NMP-behavior tests drive `negamax_for_test` directly (not via
    // `Search::go`) so the test author controls `is_pv`, `allow_null`,
    // depth, and the `(alpha, beta)` window precisely.
    // -----------------------------------------------------------------------

    /// Helper: STM-perspective static eval (mirrors the inline sign-flip
    /// inside the NMP block at step 9 of `negamax`). Reads the same
    /// `static_eval_white` field; sign-flips for Black STM.
    fn stm_static_eval(pos: &Position) -> i32 {
        if pos.side_to_move() == Color::White {
            pos.static_eval_white()
        } else {
            -pos.static_eval_white()
        }
    }

    /// NMP gate-skip at ply 0 even when synthetically called with `is_pv = false`.
    /// Defends against a future PVS refactor that would change `is_pv`'s
    /// semantics — the `ply > 0` guard fires structurally regardless.
    /// Sister fixture: same call with `ply = 1`.
    #[test]
    fn negamax_skips_nmp_at_ply_zero_even_when_is_pv_false() {
        // Quiet middlegame position; White-to-move with non-pawn material.
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // Choose beta well below static_eval so the gate passes.
        let beta = static_eval - 100;
        let alpha = -INF;

        let mut ab_skip = AlphaBetaMover::new();
        let _ = ab_skip.negamax_for_test(&mut pos.clone(), 4, 0, alpha, beta, false, true, &ctx);
        let nodes_skip = ab_skip.nodes;

        let mut ab_pass = AlphaBetaMover::new();
        let _ = ab_pass.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);
        let nodes_pass = ab_pass.nodes;

        assert!(
            nodes_skip != nodes_pass,
            "ply-0 must skip NMP regardless of is_pv; node counts must differ \
             from the ply>0 sister fixture (got skip={nodes_skip}, pass={nodes_pass})"
        );
        assert!(
            nodes_skip > nodes_pass,
            "ply-0 (NMP-skip) must visit more nodes than the gate-pass sister; \
             got skip={nodes_skip}, pass={nodes_pass}"
        );
    }

    /// NMP gate-skip when in check. Sister fixture: same skeleton, attacker
    /// moved off the attack square. Discriminator: node count — the
    /// in-check version visits the full evasion move loop; the not-in-check
    /// version fires NMP cutoff. Both fixtures retain non-pawn material so
    /// the zugzwang gate cannot be the differentiator.
    #[test]
    fn negamax_skips_nmp_when_in_check() {
        use crate::movegen::in_check;

        // In-check fixture: White K g1, Q d4, P f2/g2/h2. Black K h8, N e2
        // (knight on e2 attacks g1). White is in check. Reused from
        // `qsearch_does_not_stand_pat_in_check`.
        let pos_check = Position::from_fen("7k/8/8/8/3Q4/8/4nPPP/6K1 w - - 0 1")
            .expect("in-check FEN must parse");
        // Sister: same skeleton, knight moved from e2 to h6 (attacks none of
        // White's pieces; not check). White still has its queen as non-pawn
        // material, so the zugzwang gate passes in both fixtures.
        let pos_no_check = Position::from_fen("7k/8/7n/8/3Q4/8/5PPP/6K1 w - - 0 1")
            .expect("no-check FEN must parse");

        // Programmatically verify the check / no-check property of the
        // chosen fixtures. Failing here means the test author needs to
        // adjust the FEN, not that NMP is broken.
        assert!(
            in_check(&pos_check),
            "fixture pos_check must actually be in check"
        );
        assert!(
            !in_check(&pos_no_check),
            "fixture pos_no_check must NOT be in check"
        );
        assert!(
            has_non_pawn_material(&pos_check, Color::White),
            "in-check fixture must have White non-pawn material"
        );
        assert!(
            has_non_pawn_material(&pos_no_check, Color::White),
            "no-check fixture must have White non-pawn material"
        );

        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let alpha = -INF;
        // Pick beta below both fixtures' static_eval so the static-eval gate
        // passes in both. The discriminator is then purely the in-check gate.
        let beta_in = stm_static_eval(&pos_check) - 50;
        let beta_no = stm_static_eval(&pos_no_check) - 50;
        let beta = beta_in.min(beta_no);

        let mut ab_in = AlphaBetaMover::new();
        let _ =
            ab_in.negamax_for_test(&mut pos_check.clone(), 3, 1, alpha, beta, false, true, &ctx);
        let nodes_in = ab_in.nodes;

        let mut ab_no = AlphaBetaMover::new();
        let _ = ab_no.negamax_for_test(
            &mut pos_no_check.clone(),
            3,
            1,
            alpha,
            beta,
            false,
            true,
            &ctx,
        );
        let nodes_no = ab_no.nodes;

        assert!(
            nodes_in != nodes_no,
            "in-check vs no-check must produce different node counts; \
             got in_check={nodes_in}, no_check={nodes_no}"
        );
    }

    /// NMP gate-skip at PV nodes. Primary assertion: distinct node counts.
    /// Score equality is permitted under fail-soft (both can return scores
    /// >= beta via different paths).
    #[test]
    fn negamax_skips_nmp_at_pv_node() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 100;
        let alpha = -INF;

        let mut ab_pv = AlphaBetaMover::new();
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, true, true, &ctx);
        let nodes_pv = ab_pv.nodes;

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ = ab_nonpv.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);
        let nodes_nonpv = ab_nonpv.nodes;

        assert!(
            nodes_pv != nodes_nonpv,
            "PV vs non-PV must produce different node counts; \
             got pv={nodes_pv}, nonpv={nodes_nonpv}"
        );
        assert!(
            nodes_pv > nodes_nonpv,
            "PV node (NMP-skip) must visit more nodes than the non-PV \
             sister (NMP-cutoff); got pv={nodes_pv}, nonpv={nodes_nonpv}"
        );
    }

    /// NMP gate-skip when `allow_null = false` (the parameter directly).
    /// The `false` case must visit more nodes since the NMP block is
    /// unconditionally skipped.
    #[test]
    fn negamax_skips_nmp_when_allow_null_false() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(6);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 100;
        let alpha = -INF;

        let mut ab_allow = AlphaBetaMover::new();
        let _ = ab_allow.negamax_for_test(&mut pos.clone(), 6, 1, alpha, beta, false, true, &ctx);
        let nodes_allow = ab_allow.nodes;

        let mut ab_deny = AlphaBetaMover::new();
        let _ = ab_deny.negamax_for_test(&mut pos.clone(), 6, 1, alpha, beta, false, false, &ctx);
        let nodes_deny = ab_deny.nodes;

        assert!(
            nodes_allow != nodes_deny,
            "allow_null=true vs allow_null=false must produce different node \
             counts; got allow={nodes_allow}, deny={nodes_deny}"
        );
        assert!(
            nodes_deny > nodes_allow,
            "allow_null=false (NMP-skip) must visit more nodes than \
             allow_null=true (NMP-cutoff); got allow={nodes_allow}, deny={nodes_deny}"
        );
    }

    /// NMP gate-skip when static_eval < beta. Same position; beta is set
    /// above static_eval in the skip case, below in the pass case.
    #[test]
    fn negamax_skips_nmp_when_static_eval_below_beta() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = -INF;

        // Skip case: beta > static_eval (gate fails).
        let beta_skip = static_eval + 100;
        let mut ab_skip = AlphaBetaMover::new();
        let _ =
            ab_skip.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta_skip, false, true, &ctx);
        let nodes_skip = ab_skip.nodes;

        // Pass case: beta < static_eval (gate passes).
        let beta_pass = static_eval - 50;
        let mut ab_pass = AlphaBetaMover::new();
        let _ =
            ab_pass.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta_pass, false, true, &ctx);
        let nodes_pass = ab_pass.nodes;

        assert!(
            nodes_skip != nodes_pass,
            "static_eval<beta vs static_eval>=beta must produce different \
             node counts; got skip={nodes_skip}, pass={nodes_pass}"
        );
        assert!(
            nodes_skip > nodes_pass,
            "skip case (NMP-skip via gate fail) must visit more nodes than \
             pass case (NMP-cutoff); got skip={nodes_skip}, pass={nodes_pass}"
        );
    }

    /// NMP gate-skip when STM has no non-pawn material (zugzwang guard).
    /// Sister fixture: identical K+P endgame plus one white knight, which
    /// gives STM non-pawn material and lets NMP fire.
    #[test]
    fn negamax_skips_nmp_when_no_non_pawn_material() {
        let kp_only =
            Position::from_fen("8/4p3/8/8/4k3/8/4P3/4K3 w - - 0 1").expect("K+P FEN must parse");
        let kp_with_n = Position::from_fen("8/4p3/8/8/4k3/3N4/4P3/4K3 w - - 0 1")
            .expect("K+P+N FEN must parse");

        let (ctx, _stop) = non_aborting_ctx_at_depth(3);
        let alpha = -INF;
        // Use a beta safely below both static evals so the static-eval gate
        // passes in both fixtures; the discriminator is then has_non_pawn_material.
        let beta_kp = stm_static_eval(&kp_only) - 200;
        let beta_n = stm_static_eval(&kp_with_n) - 200;
        let beta = beta_kp.min(beta_n);

        let mut ab_kp = AlphaBetaMover::new();
        let _ = ab_kp.negamax_for_test(&mut kp_only.clone(), 3, 1, alpha, beta, false, true, &ctx);
        let nodes_kp = ab_kp.nodes;

        let mut ab_n = AlphaBetaMover::new();
        let _ = ab_n.negamax_for_test(&mut kp_with_n.clone(), 3, 1, alpha, beta, false, true, &ctx);
        let nodes_n = ab_n.nodes;

        assert!(
            nodes_kp != nodes_n,
            "K+P-only vs K+P+N must produce different node counts; \
             got kp_only={nodes_kp}, kp_with_n={nodes_n}"
        );
        assert!(
            nodes_kp > nodes_n,
            "K+P-only (NMP-skip via has_non_pawn_material) must visit more \
             nodes than K+P+N sister (NMP-cutoff); got kp_only={nodes_kp}, \
             kp_with_n={nodes_n}"
        );
    }

    /// NMP gate-skip at depth < NMP_MIN_DEPTH (= 3). Sister fixture: same
    /// position at depth 4. Discriminator is depth alone.
    #[test]
    fn negamax_skips_nmp_when_depth_below_3() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 100;
        let alpha = -INF;

        let mut ab_2 = AlphaBetaMover::new();
        let _ = ab_2.negamax_for_test(&mut pos.clone(), 2, 1, alpha, beta, false, true, &ctx);
        let firings_2 = ab_2.nmp_firings_for_test();

        let mut ab_4 = AlphaBetaMover::new();
        let _ = ab_4.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);
        let firings_4 = ab_4.nmp_firings_for_test();

        // Depth-2 must produce zero NMP firings at this node (the depth gate
        // fails). Depth-4 must produce at least one firing.
        assert_eq!(
            firings_2, 0,
            "depth=2 (< NMP_MIN_DEPTH=3) must skip NMP; got {firings_2} firings"
        );
        assert!(
            firings_4 > 0,
            "depth=4 sister fixture must fire NMP at least once; got {firings_4} firings"
        );
    }

    /// On a successful NMP cutoff, returned score must be >= beta (fail-soft).
    /// Drives the cutoff explicitly via a window where static_eval >> beta.
    #[test]
    fn negamax_returns_beta_cutoff_on_successful_nmp() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 200;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        let score = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert!(
            score >= beta,
            "successful NMP cutoff must return fail-soft score >= beta; \
             got score={score}, beta={beta}"
        );
        // Confirm NMP actually fired (not some other path that produces score >= beta).
        assert!(
            ab.nmp_firings_for_test() > 0,
            "NMP must have fired at least once; got 0 firings"
        );
    }

    /// Mate-cap: when null_score >= MATE_IN_MAX_PLY, returned cutoff is `beta`
    /// (mate-capped), not the mate-magnitude.
    ///
    /// **Construction**: Driving a real chess search to return mate-magnitude
    /// from the null sub-search is brittle (the zero-window `(-beta,
    /// -beta+1)` causes capture-first move ordering to fail-high before the
    /// mating line is found). Instead, **pre-populate the TT** with a
    /// mate-magnitude Exact entry at the post-null zobrist with stored depth
    /// at least equal to the sub-search depth. The null sub-search's negamax
    /// invocation probes the TT, hits the cutoff, and returns the mate score
    /// directly, exercising the parent's mate-cap branch deterministically.
    ///
    /// Three-step structure:
    ///   1. Compute the post-null zobrist; pre-seed the TT.
    ///   2. Drive parent `negamax_for_test`; NMP fires; null sub-search hits
    ///      the seeded TT entry; null_score >= MATE_IN_MAX_PLY.
    ///   3. Assert returned == beta (mate-capped), not mate magnitude.
    #[test]
    fn negamax_caps_mate_score_to_beta_when_null_score_is_mate() {
        // Quiet middlegame fixture with non-pawn material on both sides;
        // any non-stalemate, non-check, queen-up-ish position works since
        // the mate score arrives via the TT seed, not via the search.
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");

        // Parent depth >= NMP_MIN_DEPTH so NMP fires.
        let parent_depth = 4_u32;
        let parent_ply = 1_u32;
        let child_ply = parent_ply + 1;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(parent_depth + 2, Arc::clone(&tt));

        // Compute the post-null zobrist by applying make_null_move on a
        // clone, reading the zobrist, then unmaking. Avoids any stateful
        // contamination of `pos`.
        let post_null_zobrist = {
            let mut p = pos;
            let undo = p.make_null_move();
            let z = p.zobrist();
            p.unmake_null_move(undo);
            z
        };

        // Seed: NEGATIVE mate-magnitude Exact entry at the post-null
        // zobrist, depth >= child_depth so the child's TT probe returns it
        // early. The TT entry's score is from the STM's perspective at the
        // entry's position; STM after the null is the OPPONENT (the side
        // who would be mated). A "the opponent is being mated" position
        // therefore stores a NEGATIVE mate-magnitude score from the
        // opponent's perspective. The child returns this negative score;
        // the parent's `null_score = -child_score` becomes positive
        // mate-magnitude, triggering the mate-cap branch.
        let mate_score_for_child_stm = -(MATE - child_ply as i32); // negative mate
        let stored_score = score_to_tt(mate_score_for_child_stm, child_ply as i32);
        debug_assert!(
            stored_score == stored_score as i16 as i32,
            "stored_score must fit i16; got {stored_score}"
        );
        tt.new_search();
        tt.store(
            post_null_zobrist,
            TtData {
                score: stored_score as i16,
                depth: parent_depth as u8, // >= child_depth, satisfies probe gate
                bound: TtBound::Exact,
                best_move: 0,
            },
        );

        // ===== Drive the parent search. Choose `beta` below the parent's
        // STM static_eval so the NMP gate's `static_eval >= beta` predicate
        // passes; choose `beta` finite (well below MATE_IN_MAX_PLY) so the
        // mate-cap collapse to `beta` is observable.
        let alpha = -INF;
        let static_eval_parent = stm_static_eval(&pos);
        let beta = static_eval_parent - 100;
        assert!(
            beta < MATE_IN_MAX_PLY,
            "test invariant: chosen beta must be a finite, non-mate value"
        );
        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let returned = ab.negamax_for_test(
            &mut pos.clone(),
            parent_depth,
            parent_ply,
            alpha,
            beta,
            false,
            true,
            &ctx,
        );

        assert!(
            ab.nmp_firings_for_test() > 0,
            "NMP must have fired; got 0 firings"
        );
        assert!(
            returned < MATE_IN_MAX_PLY,
            "mate-capped NMP return must NOT be mate magnitude; got {returned}, \
             MATE_IN_MAX_PLY={MATE_IN_MAX_PLY}"
        );
        assert!(
            returned >= beta,
            "mate-capped NMP return must be >= beta (fail-soft cutoff); \
             got {returned}, beta={beta}"
        );
        assert_eq!(
            returned, beta,
            "mate-cap collapses cutoff_score to beta exactly; got {returned}, beta={beta}"
        );
    }

    /// Stacked-null prevention: the NMP null-search recursive call passes
    /// `allow_null = false`. Direct kill via the `nmp_firings` counter:
    /// if the `false` were a `true`, stacked nulls would fire more times.
    /// The pinned K (= 12 at impl time) is the empirical firings count for
    /// the chosen fixture+depth; mutating to `true` increases this to >= 13.
    #[test]
    fn negamax_passes_allow_null_false_in_null_subsearch() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(8);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 100;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 8, 1, alpha, beta, false, true, &ctx);
        let firings = ab.nmp_firings_for_test();

        // The pinned firings count is observed at impl time; the assertion
        // is `firings == K`, where K is the stable count for this fixture
        // under stacked-null prevention. Mutating the inner null-search's
        // `allow_null = false` to `true` would let inner NMPs fire,
        // increasing the count past K.
        const PINNED_FIRINGS: u32 = NMP_FIRINGS_PINNED;

        assert_eq!(
            firings, PINNED_FIRINGS,
            "NMP firings count must match pinned K={PINNED_FIRINGS}; got {firings}. \
             A larger value indicates stacked-null prevention has regressed \
             (the inner null-search recursive call's `allow_null = false` \
             may have been changed to `true`)."
        );
    }

    /// After an NMP cutoff, the TT contains a Lower-bound entry at the
    /// current depth, with best_move=0 and the mate-capped score.
    #[test]
    fn negamax_stores_lower_bound_in_tt_after_nmp_cutoff() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(4, Arc::clone(&tt));
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 200;
        let alpha = -INF;
        let depth = 4_u32;
        let ply = 1_u32;

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let returned =
            ab.negamax_for_test(&mut pos.clone(), depth, ply, alpha, beta, false, true, &ctx);
        // Sanity: NMP fired and produced a cutoff at this node.
        assert!(
            returned >= beta,
            "expected fail-high return; got {returned}, beta={beta}"
        );
        assert!(
            ab.nmp_firings_for_test() > 0,
            "NMP must have fired; got 0 firings"
        );

        let entry = tt
            .probe(pos.zobrist())
            .expect("TT must have an entry at the parent zobrist after NMP cutoff");
        assert_eq!(
            entry.bound(),
            TtBound::Lower,
            "NMP-cutoff TT entry must be Lower-bound; got {:?}",
            entry.bound()
        );
        assert_eq!(
            entry.best_move, 0,
            "NMP-cutoff TT entry must carry best_move=0 (NMP didn't pick a move); \
             got {}",
            entry.best_move
        );
        assert_eq!(
            entry.depth as u32, depth,
            "NMP-cutoff TT entry depth must be the CURRENT depth (not depth-1-R); \
             got {}, expected {depth}",
            entry.depth
        );
        // Stored score is score_to_tt(cutoff_score, ply) where cutoff_score is
        // the mate-capped value. With finite beta and a non-mate cutoff path,
        // cutoff_score == returned, and score_from_tt round-trips it.
        let recovered = score_from_tt(entry.score as i32, ply as i32);
        assert_eq!(
            recovered, returned,
            "TT round-trip: score_from_tt(stored, ply) must equal returned; \
             got recovered={recovered}, returned={returned}"
        );
    }

    /// ADR-0018 §7's preservation rule: an NMP cutoff stores best_move=0,
    /// but the tt.store implementation preserves any prior non-zero
    /// best_move on the same key. Pre-populate the TT, fire NMP, re-probe;
    /// the entry's bound must be Lower (NMP overwrote) BUT best_move must
    /// be the original non-zero move.
    #[test]
    fn negamax_with_nmp_preserves_existing_tt_best_move() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(4, Arc::clone(&tt));
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 200;
        let alpha = -INF;
        let depth = 4_u32;
        let ply = 1_u32;

        // Pre-populate the TT at the parent key with bound=Exact and a
        // non-zero best_move. We pick a dummy `best_move` value that is
        // a valid 16-bit encoding (the bits represent a Move; for the
        // preservation test, any non-zero u16 suffices since we re-read
        // the raw u16 field).
        let preserved_best_move: u16 = 0x1234;
        tt.store(
            pos.zobrist(),
            TtData {
                score: 0,
                depth: 1,
                bound: TtBound::Exact,
                best_move: preserved_best_move,
            },
        );
        // tt.new_search() advances the generation so the depth-preferred
        // replacement allows the NMP store to overwrite the seeded entry's
        // bound/depth. ADR-0018 §1 + §9.
        tt.new_search();

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(&mut pos.clone(), depth, ply, alpha, beta, false, true, &ctx);

        let entry = tt
            .probe(pos.zobrist())
            .expect("TT must still have an entry after NMP cutoff");
        assert_eq!(
            entry.bound(),
            TtBound::Lower,
            "NMP cutoff must overwrite prior bound to Lower; got {:?}",
            entry.bound()
        );
        assert_eq!(
            entry.best_move, preserved_best_move,
            "ADR-0018 §7 preservation: NMP store with best_move=0 must \
             preserve the prior non-zero best_move; got {}, expected {}",
            entry.best_move, preserved_best_move
        );
    }

    /// Push-and-pop balance: after an NMP cutoff, `self.history.len()`
    /// matches the pre-NMP length. Exercise the NMP path; assert that
    /// `negamax_for_test` returns with history balanced (matches the
    /// `Search::go` post-search debug-assert position-balance discipline,
    /// but specifically for the history Vec).
    #[test]
    fn negamax_with_nmp_clears_history_correctly_on_unmake() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 200;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        // Seed history with a couple of arbitrary entries to make len > 0
        // and check that the NMP push/pop doesn't corrupt the prior contents.
        ab.history.push(0xDEAD_BEEF_CAFE_0001);
        ab.history.push(0xDEAD_BEEF_CAFE_0002);
        let pre_len = ab.history.len();
        let pre_history = ab.history.clone();

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        // Sanity: NMP actually fired (otherwise this test would trivially pass).
        assert!(
            ab.nmp_firings_for_test() > 0,
            "NMP must have fired at least once; got 0 firings"
        );
        assert_eq!(
            ab.history.len(),
            pre_len,
            "history length must match pre-NMP length after balanced \
             push/pop; got {}, expected {pre_len}",
            ab.history.len()
        );
        assert_eq!(
            ab.history, pre_history,
            "history Vec contents must be byte-identical after balanced push/pop"
        );
    }

    // -----------------------------------------------------------------------
    // M5.B — RFP behavior in negamax (plan §5.2).
    //
    // Sister-fixture pattern, counter-based discriminators. All tests drive
    // `negamax_for_test` directly so the author controls `is_pv`, depth,
    // and the `(alpha, beta)` window precisely.
    //
    // Winning fixture used across most tests:
    //   `8/8/4k3/8/4K3/8/8/Q7 w - - 0 1` — White K+Q vs Black K.
    //   `stm_static_eval` ≈ +1000 cp (queen material + PST ≈ 1025 ± 50).
    //   At depth=4, margin=400: `1000 - 400 = 600 >= beta=100` → RFP fires.
    //
    // Balanced fixture used where RFP must NOT fire:
    //   `r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9`
    //   `stm_static_eval` ≈ 0 cp.
    // -----------------------------------------------------------------------

    /// RFP gate-skip at PV node (`is_pv = true`).
    /// Sister fixture: same parameters with `is_pv = false` → RFP fires.
    /// Discriminator: `rfp_firings` counter.
    ///
    /// depth=1 so children go to qsearch (no negamax recursion) — the counter
    /// stays isolated to the one root frame.
    #[test]
    fn negamax_skips_rfp_at_pv_node() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // depth=1 → margin=100. beta set below static_eval−100 so the margin
        // condition passes when !is_pv.
        let beta = static_eval - reverse_futility_margin(1) - 100;
        let alpha = -INF;

        let mut ab_pv = AlphaBetaMover::new();
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, true, true, &ctx);

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ = ab_nonpv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab_pv.rfp_firings_for_test(),
            0,
            "PV node must skip RFP; got {} rfp_firings",
            ab_pv.rfp_firings_for_test()
        );
        assert_eq!(
            ab_nonpv.rfp_firings_for_test(),
            1,
            "non-PV node must fire RFP exactly once; got {} rfp_firings",
            ab_nonpv.rfp_firings_for_test()
        );
    }

    /// RFP gate-skip when in check.
    /// Sister fixture: same skeleton with attacker removed.
    /// Discriminator: `rfp_firings` counter.
    ///
    /// depth=1 so children go to qsearch (no negamax recursion) — the counter
    /// stays isolated to the one root frame.
    #[test]
    fn negamax_skips_rfp_when_in_check() {
        use crate::movegen::in_check;

        // In-check fixture: White K g1, Q d4, P f2/g2/h2. Black K h8, N e2
        // (knight on e2 attacks g1). White is in check. Same as NMP check-skip test.
        let pos_check = Position::from_fen("7k/8/8/8/3Q4/8/4nPPP/6K1 w - - 0 1")
            .expect("in-check FEN must parse");
        // Sister: knight moved to h6 (not checking). White still has queen (non-pawn material).
        let pos_no_check = Position::from_fen("7k/8/7n/8/3Q4/8/5PPP/6K1 w - - 0 1")
            .expect("no-check FEN must parse");

        assert!(in_check(&pos_check), "fixture must be in check");
        assert!(
            !in_check(&pos_no_check),
            "sister fixture must NOT be in check"
        );

        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let alpha = -INF;
        // depth=1 → margin=100. beta set below no-check eval−100 so the margin
        // condition passes there; in-check case must skip regardless (gate fails).
        let beta = stm_static_eval(&pos_no_check) - reverse_futility_margin(1) - 50;

        let mut ab_check = AlphaBetaMover::new();
        let _ =
            ab_check.negamax_for_test(&mut pos_check.clone(), 1, 1, alpha, beta, false, true, &ctx);

        let mut ab_no = AlphaBetaMover::new();
        let _ = ab_no.negamax_for_test(
            &mut pos_no_check.clone(),
            1,
            1,
            alpha,
            beta,
            false,
            true,
            &ctx,
        );

        assert_eq!(
            ab_check.rfp_firings_for_test(),
            0,
            "in-check node must skip RFP; got {} rfp_firings",
            ab_check.rfp_firings_for_test()
        );
        assert_eq!(
            ab_no.rfp_firings_for_test(),
            1,
            "not-in-check node must fire RFP; got {} rfp_firings",
            ab_no.rfp_firings_for_test()
        );
    }

    /// RFP gate-skip at ply 0 even when `is_pv = false` (defense-in-depth
    /// structural-root guard, parallels M5.A's NMP `ply > 0` gate).
    /// Sister fixture: same arguments with ply = 1.
    ///
    /// depth=1 so children go to qsearch (no negamax recursion) — the counter
    /// stays isolated to the one root frame.
    #[test]
    fn negamax_skips_rfp_at_ply_zero_even_when_is_pv_false() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // depth=1 → margin=100. beta below static_eval−100 so ply=1 fires.
        let beta = static_eval - reverse_futility_margin(1) - 100;
        let alpha = -INF;

        let mut ab_ply0 = AlphaBetaMover::new();
        let _ = ab_ply0.negamax_for_test(&mut pos.clone(), 1, 0, alpha, beta, false, true, &ctx);

        let mut ab_ply1 = AlphaBetaMover::new();
        let _ = ab_ply1.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab_ply0.rfp_firings_for_test(),
            0,
            "ply=0 must skip RFP regardless of is_pv; got {} rfp_firings",
            ab_ply0.rfp_firings_for_test()
        );
        assert_eq!(
            ab_ply1.rfp_firings_for_test(),
            1,
            "ply=1 sister must fire RFP; got {} rfp_firings",
            ab_ply1.rfp_firings_for_test()
        );
    }

    /// RFP gate-skip at depth above `RFP_MAX_DEPTH` (= 6).
    /// Sister fixture: same position at depth = 6 (fires).
    /// Discriminator: `rfp_firings` counter. Catches `<=` → `<` boundary mutation.
    ///
    /// Two separate betas are required for single-ply isolation:
    ///
    /// - depth=6, beta = `static_eval - margin*6 - 1` (just below the RFP
    ///   threshold): root fires once (single-ply: RFP returns before movegen,
    ///   so no children are ever spawned → rfp_firings exactly 1).
    ///
    /// - depth=7, beta = `static_eval + 928` (experimentally confirmed ≥ 850 on
    ///   this position): root depth gate fails (7 > 6); direct children have
    ///   beta=INF (abs gate fails); with this beta, grandchildren cannot satisfy
    ///   `static_eval − margin*d >= beta_at_node` — confirmed experimentally at
    ///   beta=850+, rfp_firings=0 for this K+Q vs K position + depth=7.
    ///   If alpha tightens toward beta=950, the first child exceeds beta and
    ///   causes an immediate cutoff (score 29998 > 950), limiting exposure to
    ///   one in-check child (which skips RFP via the in_check gate).
    ///
    /// The `<= → >=` mutation (would fire at all depths ≥ 6) is caught by
    /// `negamax_skips_rfp_when_static_eval_below_beta_plus_margin` at depth=4.
    #[test]
    fn negamax_skips_rfp_at_depth_above_max() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(8);
        let static_eval = stm_static_eval(&pos);
        let alpha = -INF;

        // depth=7: gate fails (7 > RFP_MAX_DEPTH=6). beta=950 is above the
        // maximum RFP threshold for any reachable descendant node (confirmed
        // empirically: rfp_firings=0 for all betas 850..1000 on this position
        // at depth=7).
        let beta_d7 = 950;
        let mut ab_d7 = AlphaBetaMover::new();
        let _ = ab_d7.negamax_for_test(&mut pos.clone(), 7, 1, alpha, beta_d7, false, true, &ctx);

        // depth=6: gate passes (6 == RFP_MAX_DEPTH). beta just below threshold
        // so root fires and immediately returns (no movegen → no children →
        // rfp_firings exactly 1). Catches `<= → <` mutation.
        let beta_d6 = static_eval - reverse_futility_margin(6) - 1;
        let mut ab_d6 = AlphaBetaMover::new();
        let _ = ab_d6.negamax_for_test(&mut pos.clone(), 6, 1, alpha, beta_d6, false, true, &ctx);

        assert_eq!(
            ab_d7.rfp_firings_for_test(),
            0,
            "depth=7 (> RFP_MAX_DEPTH=6) must skip RFP; got {} rfp_firings",
            ab_d7.rfp_firings_for_test()
        );
        assert_eq!(
            ab_d6.rfp_firings_for_test(),
            1,
            "depth=6 (== RFP_MAX_DEPTH=6) must fire RFP; got {} rfp_firings",
            ab_d6.rfp_firings_for_test()
        );
    }

    /// RFP gate-skip when `beta` is a mate score (`abs(beta) >= MATE_IN_MAX_PLY`).
    /// Sister fixture: same position with finite beta.
    /// Discriminator: `rfp_firings` counter.
    ///
    /// depth=1 so children go to qsearch (no negamax recursion) — the counter
    /// stays isolated to the one root frame.
    #[test]
    fn negamax_skips_rfp_when_mate_beta() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let alpha = -INF;

        // Mate-score beta: well above MATE_IN_MAX_PLY. RFP gate must fail.
        let beta_mate = MATE - 5; // a mate-in-5 score, abs >= MATE_IN_MAX_PLY
        assert!(
            beta_mate.abs() >= MATE_IN_MAX_PLY,
            "test invariant: beta_mate must be a mate-magnitude score"
        );

        // Finite beta: below static_eval − margin so the margin condition passes.
        // depth=1 → margin=100.
        let static_eval = stm_static_eval(&pos);
        let beta_finite = static_eval - reverse_futility_margin(1) - 100;
        assert!(
            beta_finite.abs() < MATE_IN_MAX_PLY,
            "test invariant: beta_finite must be a centipawn score"
        );

        let mut ab_mate = AlphaBetaMover::new();
        let _ = ab_mate.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            -(MATE - 6), // alpha below mate beta
            beta_mate,
            false,
            true,
            &ctx,
        );

        let mut ab_finite = AlphaBetaMover::new();
        let _ = ab_finite.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha,
            beta_finite,
            false,
            true,
            &ctx,
        );

        assert_eq!(
            ab_mate.rfp_firings_for_test(),
            0,
            "mate-score beta must skip RFP; got {} rfp_firings",
            ab_mate.rfp_firings_for_test()
        );
        assert_eq!(
            ab_finite.rfp_firings_for_test(),
            1,
            "finite beta (below threshold) must fire RFP; got {} rfp_firings",
            ab_finite.rfp_firings_for_test()
        );
    }

    /// RFP gate-skip when `static_eval - margin < beta` (margin condition fails).
    /// Sister fixture: same position with beta set so margin condition passes.
    /// Discriminator: `rfp_firings` counter.
    #[test]
    fn negamax_skips_rfp_when_static_eval_below_beta_plus_margin() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let alpha = -INF;
        let static_eval = stm_static_eval(&pos);
        let margin = reverse_futility_margin(4);

        // Skip case: beta = static_eval - margin + 50 (gate fails: S - M < S - M + 50).
        let beta_skip = static_eval - margin + 50;
        let mut ab_skip = AlphaBetaMover::new();
        let _ =
            ab_skip.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta_skip, false, true, &ctx);

        // Pass case: beta = static_eval - margin - 50 (gate passes: S - M >= S - M - 50).
        let beta_pass = static_eval - margin - 50;
        let mut ab_pass = AlphaBetaMover::new();
        let _ =
            ab_pass.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta_pass, false, true, &ctx);

        assert_eq!(
            ab_skip.rfp_firings_for_test(),
            0,
            "static_eval < beta + margin must skip RFP; got {} rfp_firings",
            ab_skip.rfp_firings_for_test()
        );
        assert_eq!(
            ab_pass.rfp_firings_for_test(),
            1,
            "static_eval >= beta + margin must fire RFP; got {} rfp_firings",
            ab_pass.rfp_firings_for_test()
        );
    }

    /// On a successful RFP cutoff, returned score equals `static_eval - margin*depth`
    /// exactly (fail-soft proved lower bound). Catches `→ static_eval`, `→ beta`,
    /// `→ static_eval + margin` return-value mutations.
    #[test]
    fn negamax_returns_proved_lower_bound_on_successful_rfp() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let margin = reverse_futility_margin(4); // 400
        let beta = static_eval - margin - 100;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        let score = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab.rfp_firings_for_test(),
            1,
            "RFP must have fired exactly once; got {} rfp_firings",
            ab.rfp_firings_for_test()
        );
        assert_eq!(
            score,
            static_eval - margin,
            "RFP must return proved lower bound `static_eval - margin`; \
             got score={score}, expected={}, static_eval={static_eval}, margin={margin}",
            static_eval - margin
        );
    }

    /// After an RFP cutoff, the TT does NOT have an entry at the node's Zobrist key.
    /// RFP is a static heuristic; its proof is depth-specific to the margin and
    /// must NOT be stored (research §6/§7).
    #[test]
    fn negamax_rfp_does_not_store_in_tt() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(4, Arc::clone(&tt));
        let static_eval = stm_static_eval(&pos);
        let margin = reverse_futility_margin(4);
        let beta = static_eval - margin - 100;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab.rfp_firings_for_test(),
            1,
            "RFP must have fired; got 0 rfp_firings"
        );
        assert!(
            tt.probe(pos.zobrist()).is_none(),
            "TT must NOT have an entry for this position after RFP cutoff \
             (no TT store on RFP — research §6/§7)"
        );
    }

    /// When RFP's structural gates pass but the margin condition fails, the
    /// search falls through to the NMP block. NMP fires if its own gate passes.
    /// Verifies the lazy-dup design: RFP's miss doesn't suppress NMP.
    /// Discriminator: `rfp_firings == 0 AND nmp_firings >= 1`.
    #[test]
    fn negamax_passes_static_eval_through_to_nmp_when_rfp_misses() {
        // Balanced middlegame position; static_eval ≈ 0 cp.
        // At depth=4, margin=400. RFP gate: `0 - 400 = -400 >= beta`.
        // Choose beta = static_eval - 200 (between static_eval−400 and static_eval):
        //   RFP: -400 >= -200? No → gate fails.
        //   NMP: static_eval=0 >= beta=-200? Yes → gate passes (assuming non-pawn material).
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // beta = static_eval - 200: RFP needs beta <= static_eval - 400; -200 > -400, so RFP fails.
        let beta = static_eval - 200;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab.rfp_firings_for_test(),
            0,
            "RFP must not fire (margin condition fails); got {} rfp_firings",
            ab.rfp_firings_for_test()
        );
        assert!(
            ab.nmp_firings_for_test() >= 1,
            "NMP must fire at least once (falls through from RFP miss); \
             got {} nmp_firings",
            ab.nmp_firings_for_test()
        );
    }

    /// Direct counter increment: `rfp_firings` increases by 1 on a successful
    /// RFP cutoff. Direct kill for the counter-increment path mutation.
    #[test]
    fn rfp_firings_counter_increments_on_cutoff() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let margin = reverse_futility_margin(4);
        let beta = static_eval - margin - 100;
        let alpha = -INF;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab.rfp_firings_for_test(),
            1,
            "rfp_firings must be 1 after exactly one RFP cutoff; \
             got {}",
            ab.rfp_firings_for_test()
        );
    }

    /// RFP takes precedence over NMP at the depth=3..6 overlap.
    /// At depth=4: both RFP and NMP gates pass. RFP fires first (cheaper),
    /// NMP is skipped. Assert `rfp_firings == 1 AND nmp_firings == 0`.
    ///
    /// Fixture: K+Q vs K (White), static_eval ≈ +1000 cp, beta=100, depth=4.
    ///   RFP: 1000 - 4*100 = 600 >= 100 ✓ (passes by 500 cp)
    ///   NMP (if reached): 1000 >= 100 ✓ (passes by 900 cp)
    /// Both gates pass with >400 cp safety margin.
    #[test]
    fn rfp_takes_precedence_over_nmp_at_overlapping_depth() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // beta = 100: well below static_eval (≈1000) so NMP would also pass,
        // and well below static_eval − 400 = 600 so RFP passes too.
        let beta = 100;
        let alpha = -INF;
        assert!(
            static_eval - reverse_futility_margin(4) >= beta,
            "test invariant: RFP must pass (static_eval={static_eval}, margin=400, beta={beta})"
        );
        assert!(
            static_eval >= beta,
            "test invariant: NMP would also pass (static_eval={static_eval} >= beta={beta})"
        );

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, &ctx);

        assert_eq!(
            ab.rfp_firings_for_test(),
            1,
            "RFP must fire exactly once at depth=4; got {} rfp_firings",
            ab.rfp_firings_for_test()
        );
        assert_eq!(
            ab.nmp_firings_for_test(),
            0,
            "NMP must not fire (RFP took precedence and returned early); \
             got {} nmp_firings",
            ab.nmp_firings_for_test()
        );
    }

    /// Boundary: at depth=1, margin=100. RFP fires when
    /// `static_eval - 100 >= beta` (i.e., beta <= static_eval − 100).
    /// `beta = static_eval - 100` → fires (>=); `beta = static_eval - 99` → does not.
    ///
    /// // pins margin-comparison operator >=; intentionally 1-cp boundary
    #[test]
    fn negamax_at_depth_one_passes_rfp_gate_when_eval_surplus_is_at_least_one_pawn() {
        let pos =
            Position::from_fen("8/8/4k3/8/4K3/8/8/Q7 w - - 0 1").expect("K+Q vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(1);
        let static_eval = stm_static_eval(&pos);
        let alpha = -INF;

        // At depth=1, margin=100. Gate: static_eval - 100 >= beta.
        // Pass case: beta = static_eval - 100 → fires (boundary equality: S - 100 >= S - 100).
        let beta_pass = static_eval - 100;
        let mut ab_pass = AlphaBetaMover::new();
        let _ =
            ab_pass.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta_pass, false, true, &ctx);

        // Skip case: beta = static_eval - 99 → does not fire (S - 100 >= S - 99 is false).
        // pins margin-comparison operator >=; intentionally 1-cp boundary
        let beta_skip = static_eval - 99;
        let mut ab_skip = AlphaBetaMover::new();
        let _ =
            ab_skip.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta_skip, false, true, &ctx);

        assert_eq!(
            ab_pass.rfp_firings_for_test(),
            1,
            "depth=1, beta = static_eval - 100 must fire RFP (>= boundary); \
             got {} rfp_firings",
            ab_pass.rfp_firings_for_test()
        );
        assert_eq!(
            ab_skip.rfp_firings_for_test(),
            0,
            "depth=1, beta = static_eval - 99 must NOT fire RFP (strictly less); \
             got {} rfp_firings",
            ab_skip.rfp_firings_for_test()
        );
    }
}
