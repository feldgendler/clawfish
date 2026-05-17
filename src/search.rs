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

/// Classify the bound for a completed qsearch result.
///
/// Per Stockfish commit 45e5e65: qsearch's restricted move set (captures,
/// EP, queen-promo, plus in-check evasions) does not produce a true minimax
/// value over all legal moves. Calling `Exact` on a non-terminal qsearch
/// result overstates precision; storing it can short-circuit a future
/// negamax PV-node probe with an unsound score. M5.F therefore stores
/// **only Lower / Upper** for non-terminal qsearch results.
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
pub(crate) fn qsearch_tt_bound_for_completed_node(best: i32, beta: i32) -> TtBound {
    debug_assert!(
        best != -INF,
        "qsearch_tt_bound_for_completed_node: best == -INF unreachable at production sites"
    );
    if best >= beta {
        TtBound::Lower
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
            pawn_hash: crate::eval::pawns::PawnHashTable::new(),
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
    /// parent (recursion-order index 0); `false` everywhere else. PVS at M4.D
    /// will replace it with the window-based `beta - alpha == 1` check.
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
                let bound = qsearch_tt_bound_for_completed_node(score, beta);
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
        for mv in moves_vec {
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

        let bound = qsearch_tt_bound_for_completed_node(best, beta);
        let best_move_packed = match bound {
            TtBound::Lower => cutoff_move.map(|m| m.bits()).unwrap_or(0),
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

        let score = ab.negamax_for_test(
            &mut pos.clone(),
            2,
            1,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx_depth2,
        );
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

        let score = ab.negamax_for_test(
            &mut pos.clone(),
            2,
            1,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx_depth2,
        );
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

    // ----- M5.E #4: qsearch_short_circuit_at_ply_ceiling helper unit tests. -----

    /// At the ply ceiling (`ply == MAX_PLY - 1`) with a not-in-check
    /// position, the helper returns `true`. Mutation-killing for the
    /// MAX_PLY arithmetic (`MAX_PLY - 1` → `MAX_PLY + 1` would push the
    /// threshold to 65 and the helper would return `false`).
    #[test]
    fn qsearch_short_circuit_at_ply_ceiling_not_in_check_returns_true() {
        let pos = Position::starting_position();
        assert!(qsearch_short_circuit_at_ply_ceiling(
            MAX_PLY as u32 - 1,
            &pos
        ));
    }

    /// Below the ceiling (any `ply < MAX_PLY - 1`), the helper returns
    /// `false` regardless of in-check state. Discriminates a mutation that
    /// drops the `>=` comparison (e.g. `>=` → `<=` would fire at low ply).
    #[test]
    fn qsearch_short_circuit_below_ceiling_returns_false() {
        let pos = Position::starting_position();
        assert!(!qsearch_short_circuit_at_ply_ceiling(0, &pos));
        assert!(!qsearch_short_circuit_at_ply_ceiling(
            MAX_PLY as u32 - 2,
            &pos
        ));
    }

    /// At the ply ceiling but in check, the helper returns `false` —
    /// the `!in_check` arm is the discriminator. Mutation-killing for the
    /// `delete !` mutation (without the `!`, the helper would fire on
    /// in-check positions and propagate fabricated mate scores via the
    /// caller's `evaluate(pos)` return). Fixture: white in check from
    /// black rook on e2.
    #[test]
    fn qsearch_short_circuit_in_check_at_ceiling_returns_false() {
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1").expect("in-check FEN must parse");
        assert!(crate::movegen::in_check(&pos));
        assert!(!qsearch_short_circuit_at_ply_ceiling(
            MAX_PLY as u32 - 1,
            &pos
        ));
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
        let score = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, None, &ctx);

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

        // qsearch must extend the queen promotion: score must exceed stand-pat.
        // The exact margin depends on the tapered eval blend (MG queen ≈ 1025,
        // EG queen ≈ 936); asserting a specific threshold is fragile across
        // eval phase changes. The load-bearing property is score > stand_pat.
        assert!(
            score > stand_pat,
            "qsearch must extend the queen promotion above stand_pat; \
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

        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, -100, -99, false, true, None, &ctx);

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
        let _ = ab2.negamax_for_test(&mut pos2.clone(), 2, 0, -INF, INF, true, true, None, &ctx2);
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
    /// at depth 5 visits »4096 nodes) BEFORE iter 1 completes its move
    /// loop. The store discipline (skip-if-aborted + end-of-loop-only)
    /// implies the marker entry survives untouched when iter 1 is aborted.
    /// A buggy mid-loop store would overwrite the marker with a
    /// partial-iter-1 entry of much lower depth.
    ///
    /// M5.F note: qsearch TT probes reduce the per-position node count,
    /// so kiwipete at depth=1 may now visit < 4096 nodes (empirically
    /// ~4053 with M5.F), meaning iter-1 sometimes COMPLETES before the
    /// cadence abort fires. In that case, the depth-1 negamax store is
    /// valid (not a partial/mid-loop store). The test accepts either
    /// outcome: marker unchanged (iter-1 aborted) OR depth=1 entry (iter-1
    /// completed). A partial mid-loop store would produce a depth that is
    /// neither 99 (marker) nor 1 (iter-1 completed) — the assertion
    /// catches it.
    #[test]
    fn negamax_does_not_mid_loop_store_under_partial_iteration() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete fen parses");
        let tt = Arc::new(TranspositionTable::new(1));
        // Pre-populate with marker entry. Bound=Exact / depth=99 / score=12345
        // / best_move=arbitrary non-zero.
        let marker = TtData {
            score: 12345,
            depth: 99,
            bound: TtBound::Exact,
            best_move: 0xABCD,
        };
        tt.store(pos.zobrist(), marker);
        let stored_before = tt.probe(pos.zobrist()).expect("marker must be stored");
        let _ = stored_before; // referenced below for context clarity

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

        // The root entry is either:
        //   (a) unchanged marker (depth=99): iter-1 aborted mid-loop; the
        //       skip-if-aborted gate at negamax step 14 prevented any store.
        //   (b) a valid depth=1 entry: iter-1 completed before the cadence
        //       abort fired (M5.F reduces qsearch node count, so iter-1 may
        //       visit < 4096 nodes and finish before any cancellation poll).
        //
        // In both cases, the stored depth must be exactly 99 or 1. Any
        // other depth (e.g., 2, 3, …, 98) would signal a mid-loop partial
        // store from a deeper iteration — which is the bug this test guards.
        let stored_after = tt
            .probe(pos.zobrist())
            .expect("root TT slot must still be populated after aborted go");
        assert!(
            stored_after.depth == 99 || stored_after.depth == 1,
            "root entry depth must be 99 (marker survived, iter-1 aborted) or \
             1 (iter-1 completed before abort cadence); got depth={}. \
             Any other depth indicates a mid-loop partial-iteration store.",
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
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, None, &ctx);
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
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -100, INF, false, true, None, &ctx);
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
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx);
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
        let _ = ab_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx_a);
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
        let _ = ab_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx_b);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, true, None, &ctx);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, true, None, &ctx);
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
        let _ = ab1.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, None, &ctx);
        let nodes_with_hit = ab1.nodes - nodes_before_a;

        tt.clear();

        let mut ab2 = AlphaBetaMover::new();
        ab2.history = vec![pos.zobrist()];
        ab2.set_tt_for_test(Some(tt.clone()));
        let nodes_before_b = ab2.nodes;
        let _ = ab2.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, None, &ctx);
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

        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, None, &ctx);

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
        let _ = ab_ref.negamax_for_test(
            &mut pos.clone(),
            3,
            0,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx_ref,
        );
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
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx);
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

    #[test]
    fn update_history_on_quiet_cutoff_applies_bonus_once_and_malus_once_per_prior() {
        use crate::mov::MoveFlag;
        use crate::square::Square;

        let mut history_table = HistoryTable::new();
        let side = Color::White;
        let cutoff_move = Move::new(Square::G6, Square::G7, MoveFlag::Quiet);
        let prior_a = Move::new(Square::F6, Square::E6, MoveFlag::Quiet);
        let prior_b = Move::new(Square::F6, Square::F5, MoveFlag::Quiet);
        let mut quiets_searched = MoveList::new();
        quiets_searched.push(prior_a);
        quiets_searched.push(prior_b);

        update_history_on_quiet_cutoff(&mut history_table, side, cutoff_move, &quiets_searched, 4);

        assert_eq!(
            history_table.score(side, cutoff_move.from_square(), cutoff_move.to_square()) as i32,
            16,
            "cutting quiet must receive exactly one +depth^2 bonus"
        );
        assert_eq!(
            history_table.score(side, prior_a.from_square(), prior_a.to_square()) as i32,
            -16,
            "first prior quiet must receive exactly one -depth^2 malus"
        );
        assert_eq!(
            history_table.score(side, prior_b.from_square(), prior_b.to_square()) as i32,
            -16,
            "second prior quiet must receive exactly one -depth^2 malus"
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
        let _ = mover.negamax_for_test(&mut pos.clone(), 2, 0, -100, -50, true, true, None, &ctx);

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
        let _ = mover.negamax_for_test(
            &mut pos.clone(),
            1,
            0,
            10000,
            10001,
            false,
            true,
            None,
            &ctx,
        );

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
        let _ =
            mover_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx_a);
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
        let _ =
            mover_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, true, None, &ctx_b);
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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, None, &ctx);

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

        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, true, None, &ctx);

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
        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, true, None, &ctx);

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
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            2,
            ply,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );

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
        let _ = ab.negamax_for_test(&mut pos.clone(), depth, 0, 0, 1, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -50, 50, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, 0, 1, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 1, 0, 350, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 0, 0, 1, false, true, None, &ctx);

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
    // M6.B Slice E — pawn-hash wiring (ADR-0032 §2).
    // -----------------------------------------------------------------------

    /// `Search::reset()` clears the search-owned pawn hash (same
    /// `ucinewgame`/per-bench-position discipline as `history_table`).
    /// Dirty a slot, reset, assert all slots empty. Works against the stub
    /// (`clear()` is real) — this is the wiring-presence guard.
    #[test]
    fn pawn_hash_cleared_on_reset() {
        let mut ab = AlphaBetaMover::new();
        ab.pawn_hash_for_test_mut().dirty_one_slot_for_test();
        assert!(
            !ab.pawn_hash_for_test_mut().all_slots_empty_for_test(),
            "pre-condition: pawn hash must be dirty before reset"
        );
        <AlphaBetaMover as Search>::reset(&mut ab);
        assert!(
            ab.pawn_hash_for_test_mut().all_slots_empty_for_test(),
            "Search::reset must clear the pawn hash (ADR-0032 §2)"
        );
    }

    /// Determinism: from a fresh mover, `reset()` then `go()` to fixed depth
    /// twice on the same position must produce an identical node count
    /// (pins ADR-0010 / ADR-0032 §5 — the pawn hash changes speed, never
    /// result; per-search clearing keeps reruns bit-identical). This is the
    /// search-level analogue of the `bench` twice-at-same-HEAD invariant.
    ///
    /// Precondition: `evaluate_cached` must be callable without panicking
    /// (Slice E wires it; until then this test panics on the first search
    /// that reaches a leaf — that is the test-first gate).
    #[test]
    fn pawn_hash_search_is_deterministic_across_reset() {
        use crate::eval::evaluate_cached;
        use crate::eval::pawns::PawnHashTable;
        // Precondition: evaluate_cached reachable on a real position without panic.
        // Fails with `unimplemented!` until Slice C/D/E — this is the gate.
        let probe_pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let mut probe_ph = PawnHashTable::new();
        let _ = evaluate_cached(&probe_pos, &mut probe_ph);

        let pos = Position::from_fen(
            "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
        )
        .expect("middlegame fixture");
        let (ctx, _stop) = ctx_for(&pos, limits_with(|l| l.depth = Some(5)));

        let mut ab = AlphaBetaMover::new();
        ab.reset();
        let r1 = drive_go(&mut ab, &pos, &ctx).0;
        ab.reset();
        let r2 = drive_go(&mut ab, &pos, &ctx).0;

        assert_eq!(
            r1.nodes, r2.nodes,
            "node count must be identical across reset+rerun \
             (pawn hash must not perturb determinism): {} vs {}",
            r1.nodes, r2.nodes
        );
        assert_eq!(
            r1.bestmove, r2.bestmove,
            "bestmove must be identical across reset+rerun"
        );
    }

    /// Behavioral: qsearch's stand-pat leaf score on a quiet,
    /// not-in-check, pawn-structure-rich position must equal
    /// `evaluate_cached(pos, fresh_hash)` — i.e. qsearch sources its leaf
    /// eval through the cached entry point (Slice E call-site swap), and the
    /// pawn-structure term is present in that score. Fails with
    /// `unimplemented!` until Slice C/D/E land — the test-first gate.
    ///
    /// Fixture: a quiet position with a clear pawn-structure imbalance
    /// (white doubled+isolated a-pawns vs black sound pawns) and no legal
    /// captures / checks, so qsearch returns the stand-pat immediately.
    #[test]
    fn qsearch_leaf_uses_evaluate_cached_with_pawn_structure() {
        use crate::eval::evaluate_cached;
        use crate::eval::pawns::PawnHashTable;

        // White: Kg1, pawns a2,a4 (doubled + isolated). Black: Kg8, pawns
        // f7,g7,h7 (sound). No captures available, neither side in check.
        let pos = Position::from_fen("6k1/5ppp/8/8/P7/8/P7/6K1 w - - 0 1")
            .expect("quiet pawn-structure fixture");

        let mut ab = AlphaBetaMover::new();
        let (ctx, _stop) = ctx_for(&pos, SearchLimits::default());
        let qscore = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);

        let mut ph = PawnHashTable::new();
        let expected = evaluate_cached(&pos, &mut ph);
        assert_eq!(
            qscore, expected,
            "qsearch stand-pat must equal evaluate_cached (Slice-E call-site \
             swap; pawn structure present in the leaf score)"
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

    /// Reliable fail-low fixture. White to move; black queen + rook dominate.
    /// iter-6 produces a fail-low at try 1 under the tapered evaluator: the
    /// centered window `(prior - 50, prior + 50)` lies entirely above depth-6's
    /// true value.
    ///
    /// **M6.B re-pin (ADR-0032).** The original M6.A fixture
    /// (`1q4r1/8/8/8/8/PPP5/2K5/k7`, Kc2 vs Ka1) relied on a pawn-tactical
    /// inflection in the a3/b3/c3 phalanx. M6.B's connected-pawn bonus
    /// flattened the iter-5→iter-6 trajectory enough that the original fixture
    /// no longer fails low at iter-6 try-1 (iter-5 -1226, iter-6 -1237 —
    /// inside the ±50 window). A pawn-structure-neutral replacement preserving
    /// the fail-low dynamic is not achievable: no-pawn endgames with comparable
    /// material either find a forced mate before the aspiration regime (iter ≥ 6)
    /// or have monotone-smooth trajectories with no fail-low inflection (the
    /// inflection is intrinsically pawn-tactical). Per `docs/plans/m6.b.md` §7
    /// step 6–7 the fixture is re-pinned: white king moves c2 → d2, the phalanx
    /// is reduced to b3/c3, giving a different material-imbalance trajectory
    /// that reliably fails low at iter-6 try-1.
    ///
    /// **Legality (M6.B defect fix).** The intermediate re-pin
    /// (`…/2K5/3k4`) placed Kc2 and Kd1 diagonally adjacent (Chebyshev
    /// distance 1) — an illegal position; `make_move` would generate
    /// `c2xd1 (Capture)`, tripping `debug_assert!` at `src/mov.rs:589` in
    /// debug builds. This fixture is explicitly legal: white Kd2 vs black Ka1,
    /// Chebyshev distance max(|d-a|=3, |2-1|=1)=**3** — kings are not
    /// adjacent. Neither king is in check (Qb8 and Rg8 do not attack d2; Pb3,
    /// Pc3, and Kd2 do not attack a1).
    ///
    /// Empirically re-derived M6.B trajectory (deterministic, verified
    /// with `cargo test --lib` debug asserts enabled):
    /// - iter-1: -1167; iter-2: -1256; iter-3: -1246; iter-4: -1330.
    /// - iter-5 returns **-1336**.
    /// - iter-6 first try centers `(-1386, -1286)`; the true value equals
    ///   -1386 (exactly at the fail-low boundary → returned ≤ prev_alpha).
    ///   `widen_after_fail` re-searches `(-INF, -1386)`, which succeeds with
    ///   score **-1386** (exactly one re-search at depth 6, alpha=-INF).
    /// - At depth 7: same iter-6 fail-low (exactly one `depth=6` re-search
    ///   line); iter-7 succeeds window-contained at **-1411** (no depth-7
    ///   re-search). Killer-persistence and PV-legality properties hold at
    ///   both depth caps.
    fn fail_low_fixture() -> Position {
        Position::from_fen("1q4r1/8/8/8/8/1PP5/3K4/k7 w - - 0 1")
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
        // The M6.B re-pinned fixture's iter-6 try-1 returned score is -1386
        // (deep queen-down regime; see fail_low_fixture docstring).
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

        // Under M5.C the mate is first found one ply later on this fixture,
        // so the durable property is narrower: once a mate score is the
        // prior iteration's result, the next mate-prior aspiration search
        // must be window-contained.
        let mate_line = infos
            .iter()
            .find(|s| s.contains("score mate "))
            .expect("a mate score must be reported once the search reaches the mating depth");
        assert!(
            mate_line.contains("info depth"),
            "mate must appear on a real info-depth line; got: {mate_line:?}"
        );
        // After the first mate-scored iteration, the next mate-prior
        // iteration is depth 7 on this fixture. Centered window on the same
        // mate score must be window-contained there, so NO
        // aspiration_re_search lines should fire at depth ≥ 7. (Anti-stub
        // against accidental re-search: a buggy widen that fires on
        // equal-score returns would emit one.)
        let asp_at_7plus = infos
            .iter()
            .filter(|s| s.contains("info string aspiration_re_search"))
            .filter(|s| {
                let (d, _, _) = parse_aspiration_line(s);
                d >= 7
            })
            .count();
        assert_eq!(
            asp_at_7plus, 0,
            "mate-prior iterations at depth ≥ 7 should be window-contained; got: {infos:?}"
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

    /// `elapsed_at` returns the exact duration between `start` and `now`.
    /// Pins the mutation `elapsed_at → Duration::default()` which would
    /// always return zero instead of the true elapsed duration.
    #[test]
    fn search_clock_elapsed_at_returns_exact_duration() {
        // Use Cpu instants so the arithmetic is exact (no OS scheduling jitter).
        let start = SearchInstant::Cpu(0);
        let clock = SearchClock {
            start,
            deadline: None,
            soft_deadline: None,
        };
        let now = SearchInstant::Cpu(1_000_000); // 1 ms in nanoseconds
        assert_eq!(
            clock.elapsed_at(now),
            Duration::from_nanos(1_000_000),
            "elapsed_at must return now.duration_since(start), not a constant zero"
        );
    }

    /// `same_variant` returns `false` when `start` is Cpu but `deadline` is Wall.
    /// Pins the `same_variant → true` whole-function mutation and the
    /// `&&→||` body mutation. The `&&→||` mutant returns `deadline_ok ||
    /// soft_ok`; with deadline_ok=false and soft_ok=true (no soft_deadline),
    /// it returns `true` instead of the correct `false`.
    #[test]
    fn search_clock_same_variant_returns_false_for_mismatched_deadline_variant() {
        let clock = SearchClock {
            start: SearchInstant::Cpu(0),
            deadline: Some(SearchInstant::Wall(std::time::Instant::now())),
            soft_deadline: None, // soft_ok = true (None branch)
        };
        assert!(
            !clock.same_variant(),
            "mismatched Cpu start / Wall deadline must yield same_variant() == false"
        );
    }

    /// `same_variant` returns `false` when `start` is Wall but `soft_deadline` is Cpu.
    /// Pins the `&&→||` body mutation on the soft_ok branch: with
    /// deadline_ok=true (None) and soft_ok=false, the `||` mutant returns
    /// `true || false = true` instead of the correct `false`.
    #[cfg(unix)]
    #[test]
    fn search_clock_same_variant_returns_false_for_mismatched_soft_deadline_variant() {
        let clock = SearchClock {
            start: SearchInstant::Wall(std::time::Instant::now()),
            deadline: None,                             // deadline_ok = true
            soft_deadline: Some(SearchInstant::Cpu(0)), // soft_ok = false
        };
        assert!(
            !clock.same_variant(),
            "mismatched Wall start / Cpu soft_deadline must yield same_variant() == false"
        );
    }

    /// `same_variant` returns `true` for a consistently-typed Wall clock.
    /// Positive control ensuring the method is not hardwired to `false`.
    #[test]
    fn search_clock_same_variant_returns_true_for_consistent_wall_clock() {
        let now = std::time::Instant::now();
        let clock = SearchClock {
            start: SearchInstant::Wall(now),
            deadline: Some(SearchInstant::Wall(now)),
            soft_deadline: Some(SearchInstant::Wall(now)),
        };
        assert!(
            clock.same_variant(),
            "all-Wall SearchClock must yield same_variant() == true"
        );
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
        let _ =
            ab_skip.negamax_for_test(&mut pos.clone(), 4, 0, alpha, beta, false, true, None, &ctx);
        let nodes_skip = ab_skip.nodes;

        let mut ab_pass = AlphaBetaMover::new();
        let _ =
            ab_pass.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);
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
    ///
    /// **Illegal-fixture defect fix (M6.B Legality precedent; ADR-0032 §7).**
    /// The defect was discovered during the M6.C contingency screen but is
    /// **independent of the deferred passed-pawn weights** — M6.C ships
    /// score-neutral (eval == `M6.B` byte-for-byte), so this fix is a
    /// standalone correctness repair, not an eval-magnitude recalibration.
    /// The original M6.B fixtures
    /// (`7k/8/8/8/3Q4/8/4nPPP/6K1 w` and `7k/8/7n/8/3Q4/8/5PPP/6K1 w`)
    /// were **illegal**: White Qd4's d4→h8 diagonal is unobstructed, so the
    /// *black* king on h8 is in check with White to move — the side NOT to
    /// move is in check (FIDE-illegal). M6.B's move ordering never explored
    /// the king-capture branch, masking the defect; exploring it surfaces
    /// `generate_moves` emitting `Qd4xh8`, tripping `debug_assert!` at
    /// `src/mov.rs:589`. The fixtures' validation only checked the
    /// side-to-move's check status, never the opponent's — the same
    /// defect-hiding gap as M6.B's `fail_low_fixture` "Legality (M6.B defect
    /// fix)" lesson, generalized; reverting it would knowingly re-introduce
    /// a FIDE-illegal fixture (the project's correctness-over-Elo precedent,
    /// M5.E). Both fixtures are re-pinned to the queen on b3 (`1Q6`), which
    /// attacks neither king: White Kg1, Qb3, Pf2/Pg2/Ph2; Black Kh8.
    /// In-check fixture adds Black Ne2 (a knight on e2 attacks g1 → White in
    /// check); the no-check sister moves the knight to h6 (attacks none of
    /// White's pieces). Qb3 covers neither g1 nor h8 (b3's diagonals run
    /// b3-c4-d5-e6-f7-g8 and b3-a2 / b3-c2-d1, its file is b, its rank is 3
    /// — none reach h8; none reach g1 either), so the side-not-to-move is
    /// never in check in either fixture. A new `opponent-not-in-check`
    /// assertion (via `make_null_move` then `in_check`, mirroring the
    /// production NMP/zobrist-probe precedent at search.rs ~1619 / ~10942)
    /// pins the previously-missing invariant. Both retain White's queen so
    /// the zugzwang gate passes in both. Observed under the shipped
    /// score-neutral config (debug, deterministic across reruns,
    /// `cargo test --lib`): shared `beta = min(stm_static_eval-50) = 889`;
    /// `nodes_in = 38` (full evasion loop), `nodes_no = 2` (NMP cutoff) →
    /// distinct, `nodes_in > nodes_no` holds.
    #[test]
    fn negamax_skips_nmp_when_in_check() {
        use crate::movegen::in_check;

        // Helper: is the side NOT to move in check? (The previously-missing
        // legality invariant — a position with the opponent in check while
        // it is the other side's move is FIDE-illegal and would let
        // `generate_moves` emit a king capture, tripping mov.rs:589.)
        // Implemented via a null move (pass the turn) then the standard
        // side-to-move `in_check`, mirroring the production NMP make_null /
        // zobrist-probe precedent elsewhere in this module.
        let opponent_in_check = |pos: &Position| -> bool {
            let mut p = *pos;
            let undo = p.make_null_move();
            let c = in_check(&p);
            p.unmake_null_move(undo);
            c
        };

        // In-check fixture: White Kg1, Qb3, Pf2/Pg2/Ph2. Black Kh8, Ne2
        // (a knight on e2 attacks g1). White is in check; Qb3 attacks
        // neither king, so Black is NOT in check (legal position).
        let pos_check = Position::from_fen("7k/8/8/8/8/1Q6/4nPPP/6K1 w - - 0 1")
            .expect("in-check FEN must parse");
        // Sister: same skeleton, knight moved from e2 to h6 (attacks none of
        // White's pieces; not check). White still has its queen as non-pawn
        // material, so the zugzwang gate passes in both fixtures.
        let pos_no_check = Position::from_fen("7k/8/7n/8/8/1Q6/5PPP/6K1 w - - 0 1")
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
        // Legality invariant (M6.C illegal-fixture defect fix): the side NOT
        // to move must also not be in check, else the position is illegal
        // and `generate_moves` can emit an opponent-king capture, tripping
        // the `src/mov.rs:589` debug_assert. This is the invariant the
        // pre-M6.C fixture validation omitted.
        assert!(
            !opponent_in_check(&pos_check),
            "in-check fixture must be legal: the side NOT to move (Black) must NOT be in check"
        );
        assert!(
            !opponent_in_check(&pos_no_check),
            "no-check fixture must be legal: the side NOT to move (Black) must NOT be in check"
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
        let _ = ab_in.negamax_for_test(
            &mut pos_check.clone(),
            3,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );
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
            None,
            &ctx,
        );
        let nodes_no = ab_no.nodes;

        // Directional, not merely `!=` (test-suite-review nit): the in-check
        // fixture skips NMP and runs the full evasion move loop (≫ nodes);
        // the no-check sister fires the NMP cutoff (≪ nodes). Observed
        // 38 vs 2 under the shipped score-neutral config (debug,
        // deterministic). `>` closes the pathological-pass gap where a
        // broken NMP gate produced fewer in-check nodes.
        assert!(
            nodes_in > nodes_no,
            "in-check (NMP-skipped, full evasion loop) must visit MORE nodes \
             than the no-check sister (NMP cutoff); got in_check={nodes_in}, \
             no_check={nodes_no}"
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
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, true, true, None, &ctx);
        let nodes_pv = ab_pv.nodes;

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ =
            ab_nonpv.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);
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
        let _ =
            ab_allow.negamax_for_test(&mut pos.clone(), 6, 1, alpha, beta, false, true, None, &ctx);
        let nodes_allow = ab_allow.nodes;

        let mut ab_deny = AlphaBetaMover::new();
        let _ = ab_deny.negamax_for_test(
            &mut pos.clone(),
            6,
            1,
            alpha,
            beta,
            false,
            false,
            None,
            &ctx,
        );
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
        let _ = ab_skip.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            alpha,
            beta_skip,
            false,
            true,
            None,
            &ctx,
        );
        let nodes_skip = ab_skip.nodes;

        // Pass case: beta < static_eval (gate passes).
        let beta_pass = static_eval - 50;
        let mut ab_pass = AlphaBetaMover::new();
        let _ = ab_pass.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            alpha,
            beta_pass,
            false,
            true,
            None,
            &ctx,
        );
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
        let _ = ab_kp.negamax_for_test(
            &mut kp_only.clone(),
            3,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );
        let nodes_kp = ab_kp.nodes;

        let mut ab_n = AlphaBetaMover::new();
        let _ = ab_n.negamax_for_test(
            &mut kp_with_n.clone(),
            3,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );
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
        let _ = ab_2.negamax_for_test(&mut pos.clone(), 2, 1, alpha, beta, false, true, None, &ctx);
        let firings_2 = ab_2.nmp_firings_for_test();

        let mut ab_4 = AlphaBetaMover::new();
        let _ = ab_4.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);
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
        let score =
            ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
            None,
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
    /// `allow_null = false` (line ~1628, ADR-0023 §5).
    ///
    /// **M6.D finding — the literal `false → true` flip is provably
    /// behaviorally inert; no runtime observable can discriminate it.**
    /// `allow_null` gates exactly one thing: a node's own NMP block (the
    /// step-9 `if ply > 0 && allow_null && …`). The NMP null-search recurses
    /// with the zero-width window `(-beta, -beta + 1)`, and `make_null_move`
    /// leaves the board (hence `static_eval_white`) unchanged, so a node
    /// reached with `allow_null == false` (always an NMP child) has
    /// `child_beta = -parent_beta + 1` and `child_static_eval =
    /// -parent_static_eval`. The parent fired NMP (`parent_static_eval >=
    /// parent_beta`), so the child's `static_eval >= child_beta` gate reduces
    /// to `parent_static_eval <= parent_beta - 1` — a contradiction. The
    /// child therefore cannot fire NMP whether `allow_null` is `false` OR
    /// `true`; its subtree (and `nmp_firings`) is byte-identical either way.
    /// Verified empirically: mutant firing count == correct count on all 27
    /// fixture/depth combinations (incl. depth-12, ~15k firings), and
    /// `STACKED_NULL_GATE_REACHABLE == 0` on every one. The `allow_null =
    /// false` at line ~1628 is *defense-in-depth* (ADR-0023 §5) for a
    /// hypothetical future non-zero-window null search — it does no
    /// firing-count-observable work today. A firing-count pin therefore
    /// CANNOT discriminate the flip; re-pinning it as the discriminator
    /// would be the forbidden coverage gap (so it is demoted to a drift
    /// anchor — see `NMP_FIRINGS_PINNED`). **This impossibility is a
    /// structural property, not fixture drift; surfaced to the orchestrator
    /// in the M6.D recalibration report.**
    ///
    /// **Load-bearing assertion: the reachability invariant** —
    /// `STACKED_NULL_GATE_REACHABLE == 0`: zero nodes reached with
    /// `allow_null == false` satisfy *every other* NMP gate (`ply > 0`,
    /// `!is_pv`, `depth >= NMP_MIN_DEPTH`, `!in_check`,
    /// `has_non_pawn_material`, `static_eval >= beta`). This is the exact
    /// *provable* statement that stacked-null prevention holds (the zero
    /// window genuinely makes the stacked-null gate unreachable at every
    /// `allow_null == false` node). It is a genuine correctness witness +
    /// canary: a realistic future regression — switching the null search to
    /// a non-zero window (e.g. a PVS refactor) — would make the gate
    /// reachable at an `allow_null == false` node, flipping this counter
    /// non-zero and *failing this test*, which is the precise signal that
    /// the `allow_null = false` has become load-bearing and must be verified
    /// (and that flipping it would then truly regress search). It does NOT
    /// claim to kill the source-literal flip itself (provably impossible per
    /// the finding above).
    ///
    /// The `nmp_firings == NMP_FIRINGS_PINNED` check is a secondary
    /// regression *anchor* only (gross tree-shape drift). M6.D lands at the
    /// score-neutral floor (all mobility weights zeroed ⇒ `evaluate`
    /// byte-identical to `M6.C` ⇒ search tree byte-identical ⇒ this firing
    /// count == M6.C's value), so K reverts to **2**; explicitly NOT the
    /// discriminator (the eval-independent `STACKED_NULL_GATE_REACHABLE == 0`
    /// reachability invariant is — see `NMP_FIRINGS_PINNED` doc).
    #[test]
    fn negamax_passes_allow_null_false_in_null_subsearch() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(8);
        let static_eval = stm_static_eval(&pos);
        let beta = static_eval - 100;
        let alpha = -INF;

        STACKED_NULL_GATE_REACHABLE.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 8, 1, alpha, beta, false, true, None, &ctx);
        let firings = ab.nmp_firings_for_test();
        let stacked_reachable =
            STACKED_NULL_GATE_REACHABLE.load(std::sync::atomic::Ordering::Relaxed);

        // LOAD-BEARING (correctness witness + non-zero-window canary; see fn
        // doc): no node reached with allow_null==false may satisfy every other
        // NMP gate. Provably 0 under the current zero-window NMP design; a
        // future non-zero-window null search would resurrect reachability and
        // flip this non-zero, signalling allow_null=false has become
        // load-bearing.
        assert_eq!(
            stacked_reachable, 0,
            "stacked-null reachability invariant violated: {stacked_reachable} node(s) \
             reached with allow_null==false satisfied every other NMP gate \
             (static_eval>=beta included) — `allow_null` is now the sole suppressor \
             of a stacked NMP fire. Provably 0 under the zero-window null search; \
             non-zero means the null-search window changed and stacked-null \
             prevention must be re-verified (ADR-0023 §5)."
        );

        // Secondary anchor (NOT the discriminator — see fn doc): gross
        // tree-shape drift. K == 2 at the M6.D score-neutral floor (eval
        // byte-identical to M6.C ⇒ M6.C firing count).
        const PINNED_FIRINGS: u32 = NMP_FIRINGS_PINNED;
        assert_eq!(
            firings, PINNED_FIRINGS,
            "NMP firing-count drift anchor: expected pinned K={PINNED_FIRINGS}, got \
             {firings}. This is a regression *anchor* for gross tree-shape change \
             (re-pinned per search reshaping), NOT the stacked-null discriminator — \
             that is the STACKED_NULL_GATE_REACHABLE assertion above."
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
        let returned = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );
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
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );

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

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, true, true, None, &ctx);

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ =
            ab_nonpv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab_check.negamax_for_test(
            &mut pos_check.clone(),
            1,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        let mut ab_no = AlphaBetaMover::new();
        let _ = ab_no.negamax_for_test(
            &mut pos_no_check.clone(),
            1,
            1,
            alpha,
            beta,
            false,
            true,
            None,
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
        let _ =
            ab_ply0.negamax_for_test(&mut pos.clone(), 1, 0, alpha, beta, false, true, None, &ctx);

        let mut ab_ply1 = AlphaBetaMover::new();
        let _ =
            ab_ply1.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab_d7.negamax_for_test(
            &mut pos.clone(),
            7,
            1,
            alpha,
            beta_d7,
            false,
            true,
            None,
            &ctx,
        );

        // depth=6: gate passes (6 == RFP_MAX_DEPTH). beta just below threshold
        // so root fires and immediately returns (no movegen → no children →
        // rfp_firings exactly 1). Catches `<= → <` mutation.
        let beta_d6 = static_eval - reverse_futility_margin(6) - 1;
        let mut ab_d6 = AlphaBetaMover::new();
        let _ = ab_d6.negamax_for_test(
            &mut pos.clone(),
            6,
            1,
            alpha,
            beta_d6,
            false,
            true,
            None,
            &ctx,
        );

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
            None,
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
            None,
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
        let _ = ab_skip.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            alpha,
            beta_skip,
            false,
            true,
            None,
            &ctx,
        );

        // Pass case: beta = static_eval - margin - 50 (gate passes: S - M >= S - M - 50).
        let beta_pass = static_eval - margin - 50;
        let mut ab_pass = AlphaBetaMover::new();
        let _ = ab_pass.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            alpha,
            beta_pass,
            false,
            true,
            None,
            &ctx,
        );

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
        let score =
            ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

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
        let _ = ab_pass.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha,
            beta_pass,
            false,
            true,
            None,
            &ctx,
        );

        // Skip case: beta = static_eval - 99 → does not fire (S - 100 >= S - 99 is false).
        // pins margin-comparison operator >=; intentionally 1-cp boundary
        let beta_skip = static_eval - 99;
        let mut ab_skip = AlphaBetaMover::new();
        let _ = ab_skip.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha,
            beta_skip,
            false,
            true,
            None,
            &ctx,
        );

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

    // -----------------------------------------------------------------------
    // M5.C — LMR helper and move-loop behavior tests.
    // -----------------------------------------------------------------------

    fn mate_in_one_lmr_fixture() -> (Position, Move, Move) {
        let pos =
            Position::from_fen("7k/8/5KQ1/8/8/8/8/8 w - - 0 1").expect("mate-in-1 FEN must parse");
        let weak_quiet = Move::from_uci("f6e6", &pos).expect("f6e6 must be a legal quiet move");
        let mate_quiet = Move::from_uci("g6g7", &pos).expect("g6g7 must be a legal quiet mate");
        (pos, weak_quiet, mate_quiet)
    }

    fn capture_then_quiets_lmr_fixture() -> (Position, Move) {
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let first_quiet = Move::from_uci("e1e2", &pos).expect("e1e2 must be a legal quiet move");
        (pos, first_quiet)
    }

    #[test]
    fn late_move_reduction_at_depth_3_quiet_2_is_1() {
        assert_eq!(
            late_move_reduction(3, 2),
            1,
            "second quiet at depth 3 must reduce by 1 under the pinned v1 constants"
        );
    }

    #[test]
    fn late_move_reduction_grows_with_depth_for_fixed_quiet_index() {
        let shallow = late_move_reduction(3, 4);
        let deep = late_move_reduction(8, 4);
        assert!(
            deep >= shallow,
            "LMR reduction must not shrink as depth grows for the same quiet index; \
             shallow={shallow}, deep={deep}"
        );
    }

    #[test]
    fn late_move_reduction_grows_with_quiet_index_for_fixed_depth() {
        let early = late_move_reduction(6, 2);
        let late = late_move_reduction(6, 6);
        assert!(
            late >= early,
            "LMR reduction must not shrink for later quiets at fixed depth; \
             early={early}, late={late}"
        );
    }

    #[test]
    fn late_move_reduction_clamps_to_depth_minus_two() {
        let reduction = late_move_reduction(3, 1_000);
        assert_eq!(
            reduction, 1,
            "LMR reduction at depth 3 must clamp to depth-2 = 1 even for huge quiet indices; \
             got {reduction}"
        );
    }

    #[test]
    fn is_lmr_eligible_quiet_rejects_first_quiet() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 must be legal");
        let history_table = HistoryTable::new();
        assert!(
            !is_lmr_eligible_quiet(
                mv,
                &pos,
                1,
                Move::default(),
                Move::default(),
                &history_table
            ),
            "first quiet must not be LMR-eligible"
        );
    }

    #[test]
    fn is_lmr_eligible_quiet_rejects_killer_moves() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 must be legal");
        let history_table = HistoryTable::new();
        assert!(
            !is_lmr_eligible_quiet(mv, &pos, 2, mv, Move::default(), &history_table),
            "killer0 quiet must not be LMR-eligible"
        );
        assert!(
            !is_lmr_eligible_quiet(mv, &pos, 2, Move::default(), mv, &history_table),
            "killer1 quiet must not be LMR-eligible"
        );
    }

    #[test]
    fn is_lmr_eligible_quiet_rejects_high_history_quiet() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 must be legal");
        let mut history_table = HistoryTable::new();
        history_table.update(
            Color::White,
            mv.from_square(),
            mv.to_square(),
            LMR_HIGH_HISTORY_THRESHOLD as i32,
        );
        assert!(
            !is_lmr_eligible_quiet(
                mv,
                &pos,
                2,
                Move::default(),
                Move::default(),
                &history_table
            ),
            "quiet with history score at threshold must not be LMR-eligible"
        );
    }

    #[test]
    fn is_lmr_eligible_quiet_accepts_just_below_history_threshold() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 must be legal");
        let mut history_table = HistoryTable::new();
        history_table.update(
            Color::White,
            mv.from_square(),
            mv.to_square(),
            (LMR_HIGH_HISTORY_THRESHOLD - 1) as i32,
        );
        assert!(
            is_lmr_eligible_quiet(
                mv,
                &pos,
                2,
                Move::default(),
                Move::default(),
                &history_table
            ),
            "quiet with history score one below threshold must remain LMR-eligible"
        );
    }

    #[test]
    fn is_lmr_eligible_quiet_accepts_plain_late_quiet() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 must be legal");
        let history_table = HistoryTable::new();
        assert!(
            is_lmr_eligible_quiet(
                mv,
                &pos,
                2,
                Move::default(),
                Move::default(),
                &history_table
            ),
            "ordinary second quiet with empty history and no killer match must be LMR-eligible"
        );
    }

    #[test]
    fn negamax_skips_lmr_for_first_quiet_even_after_captures() {
        let (pos, first_quiet) = capture_then_quiets_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        // Promote this quiet to the top of the quiet pool while keeping it
        // below the high-history exemption threshold.
        ab.history_table_for_test_mut().update(
            Color::White,
            first_quiet.from_square(),
            first_quiet.to_square(),
            (LMR_HIGH_HISTORY_THRESHOLD - 1) as i32,
        );

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        assert!(
            !ab.lmr_reduced_moves_for_test().contains(&first_quiet),
            "the first quiet in the node's quiet-only ordering must not be reduced, \
             even when captures appear before it in the full move list"
        );
        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "fixture must still reduce some later quiet so this is not a vacuous pass"
        );
    }

    #[test]
    fn negamax_skips_lmr_for_killer_quiet_at_move_loop_boundary() {
        // Loading both killer slots is load-bearing: KILLER0_SCORE > KILLER1_SCORE >
        // any history score, so the comparator sorts killer0 to the top of the quiet
        // pool, then killer1 second. With only one killer seeded, the seeded quiet
        // would land at quiet_index = 1 and be rejected by the `quiet_index <
        // LMR_MIN_QUIET_INDEX` arm BEFORE reaching the killer-equality arm — making
        // the test pass for the wrong reason. By seeding the decoy at slot 0 and the
        // mate at slot 1, the mate quiet lands at quiet_index = 2 and the
        // killer-equality arm of `is_lmr_eligible_quiet` becomes the load-bearing
        // gate the test name claims to pin.
        let (pos, weak_quiet, mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        // Sister run: no preseeded killers → verify LMR fires at all (anti-vacuous).
        let mut ab_no_killer = AlphaBetaMover::new();
        let _ = ab_no_killer.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );
        assert!(
            ab_no_killer.lmr_reduced_searches_for_test() > 0,
            "sister run without killer must confirm LMR fires; got {}",
            ab_no_killer.lmr_reduced_searches_for_test()
        );

        // Primary run: decoy in slot 0, mate quiet in slot 1.
        let mut ab = AlphaBetaMover::new();
        let mut killers = [[Move::default(); 2]; MAX_PLY];
        killers[1][0] = weak_quiet;
        killers[1][1] = mate_quiet;
        ab.set_killers_for_test(killers);

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "primary run must still confirm LMR fires on some other quiet under the same search state \
             (defends against a tree-shape shift after killer seeding silently zeroing all firings); got {}",
            ab.lmr_reduced_searches_for_test()
        );
        assert!(
            !ab.lmr_reduced_moves_for_test().contains(&mate_quiet),
            "killer-1 quiet at quiet_index = 2 must not take the reduced-first-pass path \
             (the killer-equality arm of `is_lmr_eligible_quiet` is the load-bearing gate here, \
             not the first-quiet arm — see test header comment)"
        );
    }

    #[test]
    fn negamax_skips_lmr_for_high_history_quiet_at_move_loop_boundary() {
        // Same load-bearing structure as the killer test above: pre-seed a *decoy*
        // quiet with a higher history score than the test target so the comparator
        // sorts the decoy to the top of the quiet pool (quiet_index = 1, where the
        // first-quiet arm rejects it for an unrelated reason) and the test target
        // lands at quiet_index = 2, where the high-history arm of
        // `is_lmr_eligible_quiet` is the load-bearing gate.
        let (pos, weak_quiet, mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        // Sister run: no preseeded history → verify LMR fires at all (anti-vacuous).
        let mut ab_no_history = AlphaBetaMover::new();
        let _ = ab_no_history.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );
        assert!(
            ab_no_history.lmr_reduced_searches_for_test() > 0,
            "sister run without high history must confirm LMR fires; got {}",
            ab_no_history.lmr_reduced_searches_for_test()
        );

        // Primary run: decoy gets a higher score than mate_quiet so it sorts first.
        let mut ab = AlphaBetaMover::new();
        ab.history_table_for_test_mut().update(
            Color::White,
            weak_quiet.from_square(),
            weak_quiet.to_square(),
            MAX_HISTORY as i32,
        );
        ab.history_table_for_test_mut().update(
            Color::White,
            mate_quiet.from_square(),
            mate_quiet.to_square(),
            LMR_HIGH_HISTORY_THRESHOLD as i32,
        );

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "primary run must still confirm LMR fires on some other quiet under the same search state \
             (defends against a tree-shape shift after high-history seeding silently zeroing all firings); got {}",
            ab.lmr_reduced_searches_for_test()
        );
        assert!(
            !ab.lmr_reduced_moves_for_test().contains(&mate_quiet),
            "high-history quiet at quiet_index = 2 must not take the reduced-first-pass path \
             (the high-history arm of `is_lmr_eligible_quiet` is the load-bearing gate here, \
             not the first-quiet arm — see test header comment)"
        );
    }

    #[test]
    fn negamax_skips_lmr_at_pv_node() {
        let (pos, _weak_quiet, _mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        let mut ab_pv = AlphaBetaMover::new();
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, true, true, None, &ctx);

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ =
            ab_nonpv.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        assert_eq!(
            ab_pv.lmr_reduced_searches_for_test(),
            0,
            "PV node must skip LMR reduced first passes"
        );
        assert!(
            ab_nonpv.lmr_reduced_searches_for_test() > 0,
            "non-PV sister must perform at least one LMR reduced first pass; got {}",
            ab_nonpv.lmr_reduced_searches_for_test()
        );
    }

    #[test]
    fn negamax_skips_lmr_at_ply_zero_even_when_is_pv_false() {
        let (pos, _weak_quiet, _mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        let mut ab_root = AlphaBetaMover::new();
        let _ =
            ab_root.negamax_for_test(&mut pos.clone(), 4, 0, -INF, INF, false, true, None, &ctx);

        let mut ab_interior = AlphaBetaMover::new();
        let _ = ab_interior.negamax_for_test(
            &mut pos.clone(),
            4,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab_root.lmr_reduced_searches_for_test(),
            0,
            "ply=0 must skip LMR even when is_pv=false"
        );
        assert!(
            ab_interior.lmr_reduced_searches_for_test() > 0,
            "ply=1 sister must perform at least one LMR reduced first pass; got {}",
            ab_interior.lmr_reduced_searches_for_test()
        );
    }

    #[test]
    fn negamax_skips_lmr_when_in_check() {
        use crate::movegen::in_check;

        let pos_check = Position::from_fen("7k/8/8/8/8/3Q4/4nPPP/6K1 w - - 0 1")
            .expect("in-check FEN must parse");
        let pos_no_check = Position::from_fen("7k/7n/8/8/8/3Q4/5PPP/6K1 w - - 0 1")
            .expect("no-check FEN must parse");
        assert!(in_check(&pos_check), "fixture must be in check");
        assert!(
            !in_check(&pos_no_check),
            "sister fixture must not be in check"
        );

        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        let mut ab_check = AlphaBetaMover::new();
        let _ = ab_check.negamax_for_test(
            &mut pos_check.clone(),
            4,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        let mut ab_no = AlphaBetaMover::new();
        let _ = ab_no.negamax_for_test(
            &mut pos_no_check.clone(),
            4,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab_check.lmr_reduced_searches_for_test(),
            0,
            "in-check node must skip LMR reduced first passes"
        );
        assert!(
            ab_no.lmr_reduced_searches_for_test() > 0,
            "not-in-check sister must perform at least one LMR reduced first pass; got {}",
            ab_no.lmr_reduced_searches_for_test()
        );
    }

    #[test]
    fn negamax_skips_lmr_below_min_depth() {
        let (pos, _weak_quiet, _mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx2, _stop2) = non_aborting_ctx_at_depth(2);
        let (ctx3, _stop3) = non_aborting_ctx_at_depth(3);

        let mut ab_d2 = AlphaBetaMover::new();
        let _ = ab_d2.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, true, None, &ctx2);

        let mut ab_d3 = AlphaBetaMover::new();
        let _ = ab_d3.negamax_for_test(&mut pos.clone(), 3, 1, -INF, INF, false, true, None, &ctx3);

        assert_eq!(
            ab_d2.lmr_reduced_searches_for_test(),
            0,
            "depth below LMR_MIN_DEPTH must skip LMR"
        );
        assert!(
            ab_d3.lmr_reduced_searches_for_test() > 0,
            "depth at LMR_MIN_DEPTH must perform at least one reduced first pass; got {}",
            ab_d3.lmr_reduced_searches_for_test()
        );
    }

    #[test]
    fn negamax_reduces_late_quiet_when_all_gates_pass() {
        let (pos, weak_quiet, mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        ab.history_table_for_test_mut().update(
            Color::White,
            weak_quiet.from_square(),
            weak_quiet.to_square(),
            LMR_HIGH_HISTORY_THRESHOLD as i32,
        );

        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "LMR-eligible node must perform at least one reduced first pass; got {}",
            ab.lmr_reduced_searches_for_test()
        );
        assert!(
            ab.lmr_reduced_moves_for_test().contains(&mate_quiet),
            "the intended late quiet must be observed on the reduced-first-pass path"
        );
    }

    #[test]
    fn negamax_researches_full_depth_when_reduced_score_beats_alpha() {
        let (pos, weak_quiet, mate_quiet) = mate_in_one_lmr_fixture();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        ab.history_table_for_test_mut().update(
            Color::White,
            weak_quiet.from_square(),
            weak_quiet.to_square(),
            LMR_HIGH_HISTORY_THRESHOLD as i32,
        );

        let alpha = -INF;
        let beta = INF;
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, alpha, beta, false, true, None, &ctx);

        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "fixture must take the reduced-first-pass path before testing re-search"
        );
        assert!(
            ab.lmr_full_researches_for_test() > 0,
            "reduced score above alpha must trigger full-depth re-search; got {}",
            ab.lmr_full_researches_for_test()
        );
        assert!(
            ab.lmr_researched_moves_for_test().contains(&mate_quiet),
            "the intended late quiet must appear in the re-search path when it beats alpha"
        );
    }

    #[test]
    fn negamax_reduced_only_quiet_does_not_enter_quiets_searched() {
        // Starting position at depth=4, ply=1, alpha=900 cp. Many quiets
        // → quiet_index >= 2 cases are LMR-eligible. Reduced child depth ~2
        // cannot score near 900 cp from startpos, so reduced-only quiets exist.
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, 900, INF, false, true, None, &ctx);

        let reduced_only: Vec<Move> = ab
            .lmr_reduced_moves_for_test()
            .iter()
            .copied()
            .filter(|mv| !ab.lmr_researched_moves_for_test().contains(mv))
            .collect();

        assert!(
            !reduced_only.is_empty(),
            "fixture must produce at least one reduced-only quiet (anti-vacuous); \
             lmr_reduced={}, lmr_researched={}",
            ab.lmr_reduced_searches_for_test(),
            ab.lmr_full_researches_for_test()
        );

        let history_candidates = ab.lmr_history_candidates_for_test();
        for mv in &reduced_only {
            assert!(
                !history_candidates.contains(mv),
                "reduced-only quiets must not enter quiets_searched"
            );
        }
    }

    #[test]
    fn best_is_full_depth_after_score_upgrades_equal_score_to_full_depth() {
        assert!(
            best_is_full_depth_after_score(42, false, 42, true),
            "a later full-depth tie on the node's best score must restore TT-store eligibility"
        );
        assert!(
            !best_is_full_depth_after_score(42, false, 42, false),
            "a reduced-only tie must not claim full-depth provenance"
        );
        assert!(
            best_is_full_depth_after_score(42, true, 10, false),
            "once a best score has a full-depth witness, lower scores must not clear that provenance"
        );
        assert!(
            !best_is_full_depth_after_score(42, false, 10, true),
            "a lower-score full-depth witness must not upgrade a reduced-only best's provenance"
        );
        // Equal-score, prior full-depth, current reduced-only: the prior
        // provenance must persist (the OR semantics in the equal-score arm
        // preserve a `true` flag against an incoming `false`). Pinned to kill
        // a `> → >=` mutation on `score > best` that would treat equal-score
        // as a strict improvement and replace `true` with the incoming
        // reduced-only `false`.
        assert!(
            best_is_full_depth_after_score(42, true, 42, false),
            "an equal-score reduced-only tie must not clear an existing full-depth provenance flag"
        );
    }

    #[test]
    fn lmr_needs_full_research_strict_greater_than_alpha() {
        assert!(
            !lmr_needs_full_research(100, 100),
            "reduced score equal to alpha must not trigger a full-depth re-search"
        );
        assert!(
            !lmr_needs_full_research(99, 100),
            "reduced score below alpha must not trigger a full-depth re-search"
        );
        assert!(
            lmr_needs_full_research(101, 100),
            "reduced score strictly above alpha must trigger a full-depth re-search"
        );
    }

    #[test]
    fn negamax_does_not_research_when_reduced_score_stays_at_or_below_alpha() {
        // Starting position at depth=4, ply=1, alpha=900 cp.
        // 16+ quiets → quiet_index >= 2 cases exist → LMR fires.
        // Reduced child depth ~2: from startpos no forced mate is reachable
        // at depth 2, and ordinary middlegame eval at depth 2 stays well
        // below 900 cp, so every reduced search lands at or below alpha and
        // the `lmr_needs_full_research` gate must keep `lmr_full_researches`
        // at 0 throughout the search.
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, 900, INF, false, true, None, &ctx);

        assert!(
            ab.lmr_reduced_searches_for_test() > 0,
            "fixture must trigger at least one LMR reduced first pass; got {}",
            ab.lmr_reduced_searches_for_test()
        );
        assert_eq!(
            ab.lmr_full_researches_for_test(),
            0,
            "no reduced search may exceed alpha=900 in this fixture; got {} full re-searches",
            ab.lmr_full_researches_for_test()
        );
    }

    #[test]
    fn tt_bound_for_completed_node_suppresses_when_reduced_only() {
        // All three bound shapes are suppressed when best_is_full_depth=false.
        // §4.6: only the Upper shape is unsound to store, but the helper
        // suppresses unconditionally; the Lower/Exact arms are kept for
        // symmetry and to ensure a mutation on the `best_is_full_depth` guard
        // is killed by all three arms simultaneously.
        assert!(
            tt_bound_for_completed_node(200, 100, 0, false).is_none(),
            "would-be Lower (best >= beta) must be suppressed when reduced-only"
        );
        assert!(
            tt_bound_for_completed_node(42, 100, -100, false).is_none(),
            "would-be Exact (original_alpha < best < beta) must be suppressed when reduced-only"
        );
        assert!(
            tt_bound_for_completed_node(-200, 100, -100, false).is_none(),
            "would-be Upper (best <= original_alpha) must be suppressed when reduced-only"
        );
    }

    #[test]
    fn tt_bound_for_completed_node_classifies_full_depth() {
        assert_eq!(
            tt_bound_for_completed_node(150, 100, -100, true),
            Some(TtBound::Lower),
            "full-depth-proven fail-high node must still classify as Lower"
        );
        assert_eq!(
            tt_bound_for_completed_node(20, 100, -100, true),
            Some(TtBound::Exact),
            "full-depth-proven node that improved original alpha without failing high must classify as Exact"
        );
        assert_eq!(
            tt_bound_for_completed_node(-200, 100, -100, true),
            Some(TtBound::Upper),
            "full-depth-proven fail-low node must still classify as Upper"
        );
        // Boundary: best == original_alpha must classify as Upper, not Exact.
        // ADR-0018 §13: an Exact bound requires the score to *strictly* improve
        // the parent's α; a score that ties original_alpha is at the upper bound
        // of the fail-low band and stays Upper. Pinned to kill a `> → >=`
        // mutation on the `best > original_alpha` arm.
        assert_eq!(
            tt_bound_for_completed_node(0, 100, 0, true),
            Some(TtBound::Upper),
            "best == original_alpha must classify as Upper (Exact requires strict improvement)"
        );
    }

    #[test]
    fn late_move_reduction_returns_zero_below_min_depth() {
        assert_eq!(
            late_move_reduction(2, 5),
            0,
            "depth=2 < LMR_MIN_DEPTH must return 0"
        );
        assert_eq!(
            late_move_reduction(0, 5),
            0,
            "depth=0 < LMR_MIN_DEPTH must return 0"
        );
        assert_eq!(
            late_move_reduction(LMR_MIN_DEPTH - 1, 5),
            0,
            "depth = LMR_MIN_DEPTH-1 must return 0"
        );
        assert_ne!(
            late_move_reduction(LMR_MIN_DEPTH, LMR_MIN_QUIET_INDEX),
            0,
            "at-min boundary must reduce by at least 1 (anti-vacuous: helper does not always return 0)"
        );
    }

    #[test]
    fn late_move_reduction_returns_zero_below_min_quiet_index() {
        assert_eq!(
            late_move_reduction(8, 1),
            0,
            "quiet_index=1 < LMR_MIN_QUIET_INDEX must return 0"
        );
        assert_eq!(
            late_move_reduction(8, 0),
            0,
            "quiet_index=0 < LMR_MIN_QUIET_INDEX must return 0"
        );
        assert_eq!(
            late_move_reduction(8, LMR_MIN_QUIET_INDEX - 1),
            0,
            "quiet_index = LMR_MIN_QUIET_INDEX-1 must return 0"
        );
        assert_ne!(
            late_move_reduction(LMR_MIN_DEPTH, LMR_MIN_QUIET_INDEX),
            0,
            "at-min boundary must reduce by at least 1 (anti-vacuous: helper does not always return 0)"
        );
    }

    // §4.6 worked-case anchor. best=200, beta=100, original_alpha=0:
    //   - reduced-only provenance (best_is_full_depth=false) must suppress the store.
    //   - full-depth provenance (best_is_full_depth=true) must classify as Lower.
    // Together these two assertions kill both a stub that always returns None
    // and a mutation that bypasses suppression on the Lower arm.
    #[test]
    fn tt_bound_for_completed_node_lower_requires_full_depth_witness() {
        assert!(
            tt_bound_for_completed_node(200, 100, 0, false).is_none(),
            "best=200 >= beta=100 would classify as Lower, but reduced-only provenance must suppress the store"
        );
        assert_eq!(
            tt_bound_for_completed_node(200, 100, 0, true),
            Some(TtBound::Lower),
            "same numerics with full-depth provenance must classify as Lower, not suppressed"
        );
    }

    #[test]
    fn best_is_full_depth_after_score_strict_improvement_takes_new_provenance() {
        // A strict improvement installs the new move's provenance regardless of prior.
        assert!(
            best_is_full_depth_after_score(10, false, 20, true),
            "strict improvement with full-depth provenance must set flag to true"
        );
        assert!(
            !best_is_full_depth_after_score(10, true, 20, false),
            "strict improvement with reduced-only provenance must set flag to false"
        );
        assert!(
            best_is_full_depth_after_score(10, true, 20, true),
            "strict improvement with full-depth on both sides must stay true"
        );
    }

    // Anti-vacuous sister to `negamax_reduced_only_quiet_does_not_enter_quiets_searched`.
    // A quiet that takes the non-LMR path (either ineligible due to quiet_index=1,
    // or re-searched after reduced score > alpha) MUST appear in lmr_history_candidates.
    // The KR-vs-K position at depth=4, ply=1, alpha=-INF/INF ensures the first quiet
    // (quiet_index=1, ineligible for LMR, full-depth by definition) is searched and,
    // if it fails low without causing a cutoff, enters quiets_searched.
    #[test]
    fn negamax_full_depth_quiet_still_enters_quiets_searched() {
        // KR vs K: all moves are quiets; first quiet (quiet_index=1) takes the
        // full-depth path and must appear in lmr_history_candidates when it fails low.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KR vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        // Full window so the search completes without beta cutoffs, ensuring
        // the first quiet (quiet_index=1) is searched at full depth and recorded.
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        let candidates = ab.lmr_history_candidates_for_test();
        assert!(
            !candidates.is_empty(),
            "at least one full-depth quiet must appear in the history-candidate trace; got none"
        );
        // Trace must be scoped to the traced frame only. Generate the legal
        // White moves at the test fixture's root position; every recorded
        // candidate must come from that set. A `lmr_trace_root_ply` filter
        // mutation that records descendants instead would push Black moves
        // (post-1...K?) which are not legal as White moves from this FEN.
        let mut legal_at_root = MoveList::new();
        crate::movegen::generate_moves(&pos, &mut legal_at_root);
        for &mv in candidates {
            assert!(
                legal_at_root.iter().any(|legal| legal == mv),
                "history-candidate trace must record only moves at the traced ply; \
                 got {mv:?} which is not a legal White move at root (descendants leaked into the trace)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M5.D — FFP helper and move-loop behavior tests.
    // -----------------------------------------------------------------------

    // ---- Helper tests for `frontier_futility_margin` ----

    #[test]
    fn frontier_futility_margin_at_depth_1_is_100() {
        assert_eq!(
            frontier_futility_margin(1),
            100,
            "depth=1 margin must equal FFP_MARGIN_D1 = 100 cp"
        );
    }

    #[test]
    fn frontier_futility_margin_at_depth_2_is_150() {
        assert_eq!(
            frontier_futility_margin(2),
            150,
            "depth=2 margin must equal FFP_MARGIN_D2 = 150 cp"
        );
    }

    /// Pinned even though FFP_MARGIN_D3 is inactive at v1 (FFP_MAX_DEPTH = 2).
    /// The constant is defined for forward-compat (post-landing SPRT-tune).
    #[test]
    fn frontier_futility_margin_at_depth_3_is_250() {
        assert_eq!(
            frontier_futility_margin(3),
            250,
            "depth=3 margin must equal FFP_MARGIN_D3 = 250 cp (defined but inactive at v1)"
        );
    }

    #[test]
    fn frontier_futility_margin_at_depth_0_is_0() {
        assert_eq!(
            frontier_futility_margin(0),
            0,
            "depth=0 must return 0 (out of FFP's active range)"
        );
    }

    #[test]
    fn frontier_futility_margin_at_depth_4_is_0() {
        assert_eq!(
            frontier_futility_margin(4),
            0,
            "depth=4 must return 0 (above the table)"
        );
    }

    /// Anti-vacuous: helper does not always return 0.
    #[test]
    fn frontier_futility_margin_anti_vacuous_not_always_zero() {
        assert_ne!(
            frontier_futility_margin(1),
            0,
            "anti-vacuous: at least one in-domain depth must return non-zero"
        );
    }

    // ---- Helper tests for `ffp_pruned_bound` ----

    /// Helper-level domain guard pin: d=0 returns None even if arithmetic
    /// would otherwise yield a passing condition. Defense-in-depth against a
    /// future call-site refactor that drops the node-level `depth >= 1` gate.
    #[test]
    fn ffp_pruned_bound_returns_none_at_depth_zero() {
        assert!(
            ffp_pruned_bound(0, 0, 1_000_000).is_none(),
            "depth=0 must return None even with arithmetically-passing margin/alpha"
        );
    }

    /// Helper-level domain guard pin: d > FFP_MAX_DEPTH returns None.
    #[test]
    fn ffp_pruned_bound_returns_none_at_depth_above_max() {
        assert!(
            ffp_pruned_bound(0, FFP_MAX_DEPTH + 1, 1_000_000).is_none(),
            "depth = FFP_MAX_DEPTH + 1 must return None even with arithmetically-passing condition"
        );
    }

    /// Boundary inequality `<=` (inclusive at equality), pinned at d=1.
    /// `0 + 100 == 100 ≤ 100` → fires; `0 + 100 == 100 > 99` → does not fire.
    /// Pins `<=` → `<` mutation.
    #[test]
    fn ffp_pruned_bound_at_depth_1_boundary_inequality_inclusive() {
        assert_eq!(
            ffp_pruned_bound(0, 1, 100),
            Some(100),
            "boundary equality (static_eval + margin == alpha) must fire FFP at d=1"
        );
        assert!(
            ffp_pruned_bound(0, 1, 99).is_none(),
            "boundary - 1 (static_eval + margin > alpha) must NOT fire FFP at d=1"
        );
    }

    /// At v2 (FFP_MAX_DEPTH = 1), depth=2 returns None even with arithmetically
    /// passing margin/alpha — the helper-level domain guard rules it out.
    /// The constant `FFP_MARGIN_D2 = 150` is still defined for forward compat
    /// (a post-tune `FFP_MAX_DEPTH = 2` revival can re-activate it), and
    /// `frontier_futility_margin_at_depth_2_is_150` still pins the constant's
    /// value; this test pins the gate's domain-guard behavior.
    #[test]
    fn ffp_pruned_bound_at_depth_2_returns_none_under_v2_max_depth() {
        assert!(
            ffp_pruned_bound(0, 2, 150).is_none(),
            "depth=2 must return None at v2 (FFP_MAX_DEPTH = 1) — even with arithmetically passing condition"
        );
        assert!(
            ffp_pruned_bound(0, 2, 1_000_000).is_none(),
            "depth=2 must return None at v2 even with very high alpha"
        );
    }

    /// When `static_eval` is high enough that `alpha - margin < static_eval`,
    /// the move can already reach alpha without help; FFP must not fire.
    #[test]
    fn ffp_pruned_bound_returns_none_when_static_eval_above_alpha_minus_margin() {
        assert!(
            ffp_pruned_bound(0, 1, -50).is_none(),
            "static_eval already at or above alpha - margin must not trigger pruning"
        );
    }

    /// Payload arithmetic pin at d=1: bound equals static_eval + 100.
    /// `static_eval = -200, alpha = 0`: `-200 + 100 = -100 ≤ 0` → Some(-100).
    #[test]
    fn ffp_pruned_bound_payload_equals_static_eval_plus_margin_at_d1() {
        assert_eq!(
            ffp_pruned_bound(-200, 1, 0),
            Some(-100),
            "FFP-proved bound payload at d=1 must equal static_eval + FFP_MARGIN_D1"
        );
    }

    /// At v2 (FFP_MAX_DEPTH = 1), the d=2 payload is unreachable: the gate
    /// returns None before the payload is computed. This test fortifies the
    /// `_returns_none_under_v2_max_depth` pin against a refactor that would
    /// drop the call-site `depth ≤ FFP_MAX_DEPTH` check while keeping the
    /// arithmetic intact (helper-level defense-in-depth, M5.C
    /// `late_move_reduction` precedent).
    #[test]
    fn ffp_pruned_bound_at_depth_2_is_unreachable_under_v2_max_depth() {
        // Arithmetic that WOULD pass the inequality if the gate didn't block:
        // static_eval = -200, margin(2) = 150 → bound would be -50, alpha 0,
        // -50 ≤ 0. Under v1 this was Some(-50); under v2 it must be None.
        assert!(
            ffp_pruned_bound(-200, 2, 0).is_none(),
            "depth=2 with passing arithmetic must still return None at v2 (FFP_MAX_DEPTH = 1)"
        );
    }

    /// Overflow defense: `i32::MAX + 100` saturates to `i32::MAX`. The gate
    /// `bound <= alpha` with alpha = i32::MAX yields true → Some(i32::MAX).
    /// No panic.
    #[test]
    fn ffp_pruned_bound_does_not_overflow_on_max_static_eval() {
        let result = ffp_pruned_bound(i32::MAX, 1, i32::MAX);
        assert_eq!(
            result,
            Some(i32::MAX),
            "saturating_add must keep payload at i32::MAX without overflowing"
        );
    }

    // ---- Move-loop behavior tests ----

    /// FFP gate-skip at PV node. Sister fixture: same call with `is_pv = false`.
    /// Discriminator: `ffp_firings` counter.
    ///
    /// At depth=1, root is the FFP-eligible frame; FFP fires per quiet move.
    /// PV must skip; non-PV must fire at least once.
    #[test]
    fn negamax_skips_ffp_at_pv_node() {
        // Quiet middlegame position with many quiet moves available.
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // Pick an alpha well above static_eval + margin so FFP fires for every
        // quiet at depth 1 (margin=100). Add an upper guard against MATE.
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab_pv = AlphaBetaMover::new();
        let _ = ab_pv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, true, true, None, &ctx);

        let mut ab_nonpv = AlphaBetaMover::new();
        let _ =
            ab_nonpv.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        assert_eq!(
            ab_pv.ffp_firings_for_test(),
            0,
            "PV node must skip FFP; got {} ffp_firings",
            ab_pv.ffp_firings_for_test()
        );
        assert!(
            ab_nonpv.ffp_firings_for_test() > 0,
            "non-PV sister must fire FFP at least once; got 0"
        );
    }

    /// FFP gate-skip at ply 0 even with `is_pv = false`. Defense-in-depth
    /// structural-root guard. Sister: same call with ply = 1.
    #[test]
    fn negamax_skips_ffp_at_ply_zero_even_when_is_pv_false() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab_ply0 = AlphaBetaMover::new();
        let _ =
            ab_ply0.negamax_for_test(&mut pos.clone(), 1, 0, alpha, beta, false, true, None, &ctx);

        let mut ab_ply1 = AlphaBetaMover::new();
        let _ =
            ab_ply1.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        assert_eq!(
            ab_ply0.ffp_firings_for_test(),
            0,
            "ply=0 must skip FFP regardless of is_pv; got {}",
            ab_ply0.ffp_firings_for_test()
        );
        assert!(
            ab_ply1.ffp_firings_for_test() > 0,
            "ply=1 sister must fire FFP at least once; got 0"
        );
    }

    /// FFP gate-skip when in check.
    /// Sister: not-in-check fixture.
    ///
    /// **Legal-position invariant.** Both fixtures must be LEGAL chess
    /// positions (only the side-to-move can be in check; the opposite side
    /// must not be). FFP's move loop runs on the gate-skipped arm too (it
    /// just doesn't fire FFP); a movegen capture of the opponent's king
    /// would only be emitted from an illegal position. RFP's analogous
    /// test escapes this constraint because RFP returns early without
    /// running the move loop, but FFP does iterate.
    #[test]
    fn negamax_skips_ffp_when_in_check() {
        use crate::movegen::in_check;

        // In-check fixture (legal): White Kh1 in check from Black Bd5
        // along the d5-h1 diagonal. Black king on e8; not attacked by
        // White's lone king.
        let pos_check =
            Position::from_fen("4k3/8/8/3b4/8/8/8/7K w - - 0 1").expect("in-check FEN must parse");
        // Sister (legal, not-in-check): standard middlegame, White to
        // move, neither side in check, plenty of quiet moves available.
        let pos_no_check =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("no-check FEN must parse");

        assert!(in_check(&pos_check), "fixture must be in check");
        assert!(
            !in_check(&pos_no_check),
            "sister fixture must NOT be in check"
        );

        let (ctx, _stop) = non_aborting_ctx_at_depth(4);

        // Each arm gets its own alpha derived from its own static_eval, so
        // both arms have an FFP-passing alpha (we don't need both to fire,
        // but we need the no-check sister to fire and the in-check arm to
        // be blocked by the gate alone).
        let alpha_check = stm_static_eval(&pos_check) + frontier_futility_margin(1) + 200;
        let alpha_no_check = stm_static_eval(&pos_no_check) + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab_check = AlphaBetaMover::new();
        let _ = ab_check.negamax_for_test(
            &mut pos_check.clone(),
            1,
            1,
            alpha_check,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        let mut ab_no = AlphaBetaMover::new();
        let _ = ab_no.negamax_for_test(
            &mut pos_no_check.clone(),
            1,
            1,
            alpha_no_check,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab_check.ffp_firings_for_test(),
            0,
            "in-check node must skip FFP; got {}",
            ab_check.ffp_firings_for_test()
        );
        assert!(
            ab_no.ffp_firings_for_test() > 0,
            "not-in-check sister must fire FFP at least once; got 0"
        );
    }

    /// FFP gate-skip at depth above FFP_MAX_DEPTH. Sister: depth = FFP_MAX_DEPTH.
    /// Pins `<=` boundary at FFP_MAX_DEPTH (catches `<= → <` mutation).
    #[test]
    fn negamax_skips_ffp_at_depth_above_max() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(8);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(2) + 200;
        let beta = MATE_IN_MAX_PLY;

        // depth = FFP_MAX_DEPTH + 1 = 3 → root gate fails. The counter is
        // gated to `lmr_trace_root_ply == Some(1)`, so only firings at the
        // root frame (ply == 1) are recorded. The root frame's depth-3 fails
        // the `depth <= FFP_MAX_DEPTH` gate, so the counter stays 0. FFP
        // firings at descendant frames (ply > 1, depth ≤ 2) are NOT counted
        // — those plies don't match the trace root. The test discriminates
        // the boundary at the root frame only; descendant FFP behavior is
        // covered separately by `negamax_skips_ffp_at_pv_node` and friends.
        let mut ab_above = AlphaBetaMover::new();
        let _ = ab_above.negamax_for_test(
            &mut pos.clone(),
            FFP_MAX_DEPTH + 1,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        // Sister: depth = FFP_MAX_DEPTH = 2 → root gate passes; FFP fires.
        let mut ab_at = AlphaBetaMover::new();
        let _ = ab_at.negamax_for_test(
            &mut pos.clone(),
            FFP_MAX_DEPTH,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab_above.ffp_firings_for_test(),
            0,
            "depth = FFP_MAX_DEPTH + 1 must skip FFP at this frame; got {}",
            ab_above.ffp_firings_for_test()
        );
        assert!(
            ab_at.ffp_firings_for_test() > 0,
            "depth = FFP_MAX_DEPTH sister must fire FFP at least once; got 0"
        );
    }

    /// FFP gate-skip when `alpha.abs() >= MATE_IN_MAX_PLY`. The mate-window
    /// guard is load-bearing because centipawn margins are meaningless at
    /// near-mate alpha. Sister: alpha well below MATE_IN_MAX_PLY.
    #[test]
    fn negamax_skips_ffp_when_alpha_near_mate() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let beta = MATE;

        // Mate-magnitude alpha: gate `alpha.abs() < MATE_IN_MAX_PLY` fails →
        // FFP never fires.
        let alpha_mate = MATE_IN_MAX_PLY;
        let mut ab_mate = AlphaBetaMover::new();
        let _ = ab_mate.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha_mate,
            beta,
            false,
            true,
            None,
            &ctx,
        );

        // Sister: ordinary high alpha (FFP fires).
        let static_eval = stm_static_eval(&pos);
        let alpha_normal = static_eval + frontier_futility_margin(1) + 200;
        // Defensive: ensure sister alpha is well below MATE_IN_MAX_PLY.
        assert!(
            alpha_normal.abs() < MATE_IN_MAX_PLY,
            "test fixture invariant: sister alpha must satisfy mate-window gate; \
             got alpha={alpha_normal}, MATE_IN_MAX_PLY={MATE_IN_MAX_PLY}"
        );
        let mut ab_normal = AlphaBetaMover::new();
        let _ = ab_normal.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha_normal,
            MATE_IN_MAX_PLY,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab_mate.ffp_firings_for_test(),
            0,
            "alpha = MATE_IN_MAX_PLY must skip FFP via mate-window gate; got {}",
            ab_mate.ffp_firings_for_test()
        );
        assert!(
            ab_normal.ffp_firings_for_test() > 0,
            "ordinary-alpha sister must fire FFP at least once; got 0"
        );
    }

    /// FFP must not fire on capture moves (only on quiets). Fixture: a position
    /// where every legal move is a capture — FFP firings count must be 0.
    /// Sister: a position with quiets — FFP fires at least once.
    #[test]
    fn negamax_skips_ffp_for_capture_moves() {
        // Captures-only fixture: White K, Black K, single White piece that can
        // only capture. Construct: White Kg1 + queen on a1 attacking only Black
        // K at b2 — but the king cannot be captured; movegen would find the K-K
        // adjacency illegal. Use a position where the only legal move is a
        // capture.
        //
        // Simpler: White K e1 + Q d4. Black K e6, B d5. White to move; only
        // legal moves include the queen capture Qxd5 alongside several quiets.
        // We can't truly construct "captures only" without contrivances. Use a
        // pawn-up endgame with at least one capture and assert FFP doesn't fire
        // on the capture by checking the captured square doesn't appear in
        // ffp_skipped_moves.
        let pos = Position::from_fen("4k3/8/4p3/3P4/8/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        // The capture move d5xe6: assert it is NOT in the FFP-skipped move list.
        let capture = Move::from_uci("d5e6", &pos).expect("d5e6 must be a legal capture");
        let skipped: Vec<Move> = ab.ffp_skipped_moves_for_test().to_vec();
        assert!(
            !skipped.contains(&capture),
            "the d5xe6 capture must NOT appear in the FFP-skipped move list; \
             skipped = {skipped:?}"
        );

        // Anti-vacuous: at least one quiet is in the skipped list (king moves).
        assert!(
            !skipped.is_empty(),
            "anti-vacuous: at least one quiet (king move) must be FFP-skipped; \
             skipped = {skipped:?}"
        );

        // Counter / list invariant: ffp_firings must equal the length of the
        // ffp_skipped_moves list. Catches a bug where one tracking field is
        // updated but not the other (e.g., increment counter on captures
        // without pushing to the move list, or vice versa).
        assert_eq!(
            ab.ffp_firings_for_test() as usize,
            skipped.len(),
            "ffp_firings counter must match the length of ffp_skipped_moves; \
             got firings={}, skipped.len()={}",
            ab.ffp_firings_for_test(),
            skipped.len()
        );
    }

    /// FFP fires at least once when all gates pass and the per-quiet condition
    /// holds (`static_eval + margin <= alpha`). Pins the positive case.
    #[test]
    fn negamax_fires_ffp_when_quiet_cannot_reach_alpha() {
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
                .expect("FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        // alpha well above static_eval + margin so FFP fires for every quiet
        // at depth 1.
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        assert!(
            ab.ffp_firings_for_test() > 0,
            "FFP must fire at least once when gates pass and condition holds; got 0"
        );
    }

    /// Exact firings-counter pin (mirrors M5.B `rfp_firings_counter_increments_on_cutoff`).
    /// At depth=1, a quiet position with N legal quiet moves and 0 captures →
    /// FFP fires exactly N times. Captures-only mixed positions are handled in
    /// `negamax_skips_ffp_for_capture_moves`.
    #[test]
    fn ffp_firings_counter_increments_on_skip() {
        // King-only fixture: only legal moves are king moves (all quiet);
        // count them and assert ffp_firings == count.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        // Count expected legal quiet moves up front.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let expected_quiets: u32 = ml.iter().filter(|m| is_quiet(*m)).count() as u32;
        assert!(
            expected_quiets > 0,
            "test fixture invariant: position must have at least one legal quiet move"
        );

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        assert_eq!(
            ab.ffp_firings_for_test(),
            expected_quiets,
            "FFP must fire exactly once per legal quiet move at this fixture; \
             expected {expected_quiets}, got {}",
            ab.ffp_firings_for_test()
        );
    }

    /// FFP-skipped quiets must NOT enter `quiets_searched` (would otherwise
    /// receive history malus on a later beta cutoff at the same node, training
    /// history on coarse static-comparison-rejected moves). ADR-0026 §8.
    /// Anti-vacuous sister: at this node a non-skipped quiet exists (e.g. a
    /// capture or non-FFP-eligible move) and IS recorded in lmr_history_candidates.
    #[test]
    fn ffp_skipped_quiet_does_not_enter_quiets_searched() {
        // KvK-only fixture: only quiet moves; all FFP-skipped at the configured
        // alpha. Every move enters ffp_skipped_moves; none enters
        // lmr_history_candidates (which represents quiets_searched recordings).
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        let skipped: Vec<Move> = ab.ffp_skipped_moves_for_test().to_vec();
        let history_candidates: Vec<Move> = ab.lmr_history_candidates_for_test().to_vec();

        // Anti-vacuous (skipped side): at least one quiet was FFP-skipped.
        assert!(
            !skipped.is_empty(),
            "fixture must FFP-skip at least one quiet; got 0"
        );

        // Main assertion: no FFP-skipped move appears in quiets_searched.
        for mv in &skipped {
            assert!(
                !history_candidates.contains(mv),
                "FFP-skipped quiet {mv:?} must NOT enter quiets_searched; \
                 history_candidates = {history_candidates:?}"
            );
        }
    }

    /// Anti-vacuous sister to `ffp_skipped_quiet_does_not_enter_quiets_searched`:
    /// when alpha is set so FFP does NOT fire (alpha well below static_eval),
    /// quiets are searched normally and at least one enters quiets_searched.
    /// This proves the previous test's assertion is non-vacuous (lmr_history_candidates
    /// is populated by the trace at this ply when FFP doesn't intercept).
    #[test]
    fn negamax_full_depth_quiet_enters_quiets_searched_when_ffp_does_not_fire() {
        // KR-vs-K fixture (reused from M5.C): all moves are quiets; with alpha
        // = -INF, FFP cannot fire (no positive margin can make
        // static_eval + margin <= -INF), so quiets are searched normally.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KR vs K FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 4, 1, -INF, INF, false, true, None, &ctx);

        // FFP did not fire at all (alpha = -INF blocks every positive margin).
        assert_eq!(
            ab.ffp_firings_for_test(),
            0,
            "FFP must not fire when alpha = -INF; got {}",
            ab.ffp_firings_for_test()
        );

        // Anti-vacuous: at least one quiet entered quiets_searched (proves the
        // previous test's "no FFP-skipped move appears in quiets_searched"
        // assertion is non-vacuous).
        assert!(
            !ab.lmr_history_candidates_for_test().is_empty(),
            "at least one full-depth quiet must enter quiets_searched when FFP does not fire"
        );
    }

    /// Sign-convention pin: with Black to move, FFP must use the STM-relative
    /// static eval (`-static_eval_white`), NOT the raw white-relative value.
    /// Failure mode under a sign-flip bug: FFP would fire when the white-side
    /// score `+W + margin <= alpha` instead of the black-side score
    /// `-W + margin <= alpha`. For a position where +W is large and -W is
    /// small, the sign-bug fires when it shouldn't, and vice versa.
    ///
    /// Fixture: a Black-to-move position whose `static_eval_white` is positive
    /// (White-favored); from STM=Black's perspective the eval is negative.
    /// Set `alpha` so that `-W + margin <= alpha` (FFP fires for the correct
    /// STM-relative test) but `+W + margin > alpha` (would NOT fire under a
    /// sign-flip bug — FFP firings would be 0).
    #[test]
    fn negamax_ffp_with_black_to_move_uses_stm_relative_not_white_relative_static_eval() {
        // White-favored quiet middlegame, Black to move. PeSTO MG eval is
        // dominated by the f1 bishop pair / better king safety. We confirm
        // sign by reading `static_eval_white()` directly (must be positive).
        let pos =
            Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 b - - 0 9")
                .expect("FEN must parse");
        let static_eval_white = pos.static_eval_white();
        let stm_eval = stm_static_eval(&pos);
        assert!(
            static_eval_white > 0,
            "fixture invariant: static_eval_white must be positive (White-favored); got {static_eval_white}"
        );
        assert!(
            stm_eval < 0,
            "fixture invariant: STM-relative eval (Black) must be negative; got {stm_eval}"
        );

        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        // Pick alpha such that:
        //   stm_eval + 100 ≤ alpha  (correct: FFP fires at d=1)
        //   −stm_eval + 100 > alpha (sign-flip bug: would NOT fire)
        // Concretely, set alpha = stm_eval + 100 (boundary inclusive), and
        // confirm |stm_eval| + 100 > alpha.
        let alpha = stm_eval + frontier_futility_margin(1);
        assert!(
            -stm_eval + frontier_futility_margin(1) > alpha,
            "sign-discriminator: under a sign-flip bug, FFP must NOT fire for this alpha"
        );
        let beta = MATE_IN_MAX_PLY;

        let mut ab = AlphaBetaMover::new();
        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        assert!(
            ab.ffp_firings_for_test() > 0,
            "FFP must fire when STM-relative eval + margin <= alpha; \
             got 0 firings (likely a sign-flip bug — code is using +static_eval_white \
             instead of negating for Black STM)"
        );
    }

    /// Pruned-bound contribution to `best` is visible in the returned score
    /// when no other move improved alpha. Pinned to **exact equality**, not
    /// `>=`, in a single-move fixture so the result is determined entirely by
    /// the FFP contribution. A stub returning the constant directly would be
    /// killed by the firings-counter pin (firings would be 0).
    ///
    /// ADR-0026 §7 / research §13 Pitfall 6: without the contribution, the
    /// node's `best = -INF` would propagate to the parent as +INF, registering
    /// a phantom fail-high.
    #[test]
    fn negamax_ffp_pruned_bound_contributed_to_best_when_only_pruned_quiets() {
        // KvK with one legal quiet (a king move). At alpha well above
        // static_eval + 100, FFP fires for that king move; no other move runs.
        // Returned score must equal exactly `static_eval + 100` (the pruned
        // bound), NOT -INF.
        //
        // Use a position with EXACTLY one legal move where possible. KvK has 5
        // king moves typically; we don't need exactly one. We need: every move
        // is FFP-skipped, AND nothing improves `best` past the pruned bound.
        // Because all king moves at this position have the same parent
        // static_eval, all pruned bounds are equal to static_eval + 100. So
        // `best` = static_eval + 100 regardless of how many quiets fire.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN must parse");
        let (ctx, _stop) = non_aborting_ctx_at_depth(4);
        let static_eval = stm_static_eval(&pos);
        let alpha = static_eval + frontier_futility_margin(1) + 200;
        let beta = MATE_IN_MAX_PLY;

        let mut ab = AlphaBetaMover::new();
        let score =
            ab.negamax_for_test(&mut pos.clone(), 1, 1, alpha, beta, false, true, None, &ctx);

        // Anti-stub: if the implementation always returned static_eval+margin
        // without firing FFP, this assertion would still pass — but the
        // firings counter would be 0. Pin both.
        assert!(
            ab.ffp_firings_for_test() > 0,
            "FFP must fire (else the bound contribution is vacuous); got 0"
        );

        let expected = static_eval + frontier_futility_margin(1);
        assert_eq!(
            score, expected,
            "returned score must equal exactly static_eval + FFP_MARGIN_D1 \
             when every legal move is FFP-skipped; expected {expected}, got {score}"
        );
    }

    /// Provenance-downgrade pin (ADR-0026 §7, load-bearing TT-store
    /// correctness). The mixed case to discriminate:
    ///
    ///   1. A non-quiet move (capture) is searched at full depth, returns
    ///      `capture_score`, and updates `best = capture_score, flag = true`.
    ///   2. An FFP-pruned quiet contributes `pruned_bound > capture_score`
    ///      and **also `pruned_bound ≤ alpha`** (so it does not improve
    ///      alpha, but DOES update `best` past the capture's score).
    ///   3. End of loop: under WITHOUT-downgrade, `best = pruned_bound,
    ///      flag = true (stale)`, `best ≤ original_alpha` → would store
    ///      Upper at depth 1 with score = pruned_bound (inflated coarse-
    ///      static bound). Under WITH-downgrade, `flag = false`, store is
    ///      suppressed.
    ///
    /// To reach this regime, two fixture invariants must hold simultaneously:
    /// (a) `pruned_bound ≤ alpha` (FFP fires); (b) `pruned_bound >
    /// capture_score` (so the FFP contribution improves `best`, exercising
    /// the strict-improvement-with-reduced-only-provenance arm of
    /// `best_is_full_depth_after_score`). The fixture below uses a position
    /// where White is up a queen and the only available captures are
    /// Q-for-P trades that LOSE the queen on the recapture. The capture's
    /// qsearch return is therefore a large negative number (~-900 cp from
    /// White's perspective), while `pruned_bound = static_eval + 100` is
    /// roughly +900 cp (White's pre-move material advantage). So
    /// `pruned_bound > capture_score` by ~1800 cp, structurally guaranteed.
    /// We additionally assert this invariant at runtime via the post-search
    /// state.
    ///
    /// `alpha` is set just below the mate-window ceiling so it is trivially
    /// above `pruned_bound` (FFP fires for quiets) and trivially above any
    /// realistic capture score (so the capture cannot improve alpha).
    ///
    /// Anti-vacuous sister: same fixture with `alpha = -INF`. FFP cannot
    /// fire; capture improves best from -INF; `best > original_alpha` →
    /// Exact bound stored. TT entry IS present (proves the suppression
    /// assertion is non-vacuous — the TT path is reachable when FFP is
    /// not blocking it).
    #[test]
    fn negamax_ffp_contribution_downgrades_best_is_full_depth() {
        // Fixture: White Kg1 + Qd4 vs Black Ke8 + pd7. The only legal
        // capture is Qxd7 — pd7 is defended by Ke8, so the queen is
        // recaptured (Kxd7), losing Q for P. The capture's qsearch score
        // from White's perspective lands well below `pruned_bound`.
        // Quiets are king moves and queen non-capture moves. The runtime
        // invariants below pin the regime structurally.
        let pos = Position::from_fen("4k3/3p4/8/8/3Q4/8/8/6K1 w - - 0 1").expect("FEN must parse");

        // Verify the position has at least one capture and one quiet so the
        // mixed case is possible.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let n_quiets: usize = ml.iter().filter(|m| is_quiet(*m)).count();
        let n_captures: usize = ml.iter().filter(|m| !is_quiet(*m)).count();
        assert!(
            n_captures >= 1 && n_quiets >= 1,
            "fixture invariant: at least one capture and one quiet; got {n_captures} captures, {n_quiets} quiets"
        );
        // Confirm the only capture is Qxd7 (a losing capture defended by king).
        let captures: Vec<Move> = ml.iter().filter(|m| !is_quiet(*m)).collect();
        assert_eq!(
            captures.len(),
            1,
            "fixture invariant: exactly one capture in this fixture (Qxd7); got {} captures",
            captures.len()
        );

        // Fixture-pre-move static_eval (used to compute expected pruned_bound).
        let static_eval = stm_static_eval(&pos);
        let pruned_bound_d1 = static_eval + frontier_futility_margin(1);

        // alpha is set just below the mate-window gate's ceiling. No realistic
        // depth-1 capture can score this high, so the capture cannot improve
        // alpha — it only improves `best` from -INF and sets the flag, leaving
        // the FFP contribution to overwrite `best` and discriminate the
        // downgrade behavior.
        let alpha = MATE_IN_MAX_PLY - 100; // 29836; |alpha| < MATE_IN_MAX_PLY = 29936.
        assert!(
            alpha.abs() < MATE_IN_MAX_PLY,
            "fixture invariant: alpha must satisfy mate-window FFP gate; \
             got alpha={alpha}, MATE_IN_MAX_PLY={MATE_IN_MAX_PLY}"
        );
        assert!(
            pruned_bound_d1 < alpha,
            "fixture invariant: pruned_bound (={pruned_bound_d1}) must be below alpha (={alpha}) so FFP fires"
        );
        let beta = MATE_IN_MAX_PLY;

        // Test arm: with FFP active. Provide a TT and check post-search whether
        // an entry exists at the test node's Zobrist key.
        let tt_test = Arc::new(TranspositionTable::new(1));
        let (ctx_test, _stop_test) = non_aborting_ctx_at_depth_with_tt(4, tt_test.clone());
        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(tt_test.clone()));
        // Advance the generation once (mirroring `Search::go`'s behavior — the
        // store path requires the per-search generation counter to be set).
        tt_test.new_search();
        let returned_score = ab.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            alpha,
            beta,
            false,
            true,
            None,
            &ctx_test,
        );
        let firings = ab.ffp_firings_for_test();
        assert!(
            firings > 0,
            "fixture invariant: FFP must fire (anti-vacuous for the suppression assertion); got 0"
        );
        // Runtime invariant for the regime under test: the returned score
        // must equal `pruned_bound`. This proves both:
        //   (a) the FFP contribution actually overwrote `best` past the
        //       capture's score (i.e., `pruned_bound > capture_score` —
        //       the strict-improvement-with-reduced-only-provenance arm
        //       was reached);
        //   (b) the FFP contribution was the LAST thing to update `best`
        //       (no quiet recursion produced a higher score that would
        //       have overwritten it again).
        // If either fails, the regime is not exercised and the suppression
        // assertion below is vacuous.
        assert_eq!(
            returned_score, pruned_bound_d1,
            "regime-exercised invariant: returned score ({returned_score}) must equal pruned_bound \
             ({pruned_bound_d1}); the FFP contribution must be the highest-scoring move at this node \
             for the downgrade arm to fire"
        );
        let entry_after_ffp = tt_test.probe(pos.zobrist());
        // The load-bearing assertion: NO Upper-bounded entry exists at
        // depth=1 for this key from the FFP-influenced root frame. Without
        // the downgrade, the captured-flag-true path would store Upper at
        // score = pruned_bound (inflated coarse-static-witness bound). With
        // the downgrade, the store is suppressed.
        if let Some(e) = entry_after_ffp {
            assert!(
                !(e.depth as u32 == 1 && matches!(e.bound(), TtBound::Upper)),
                "FFP suppression failed: stored Upper entry at depth=1 with score={} \
                 (would be inflated coarse-static-witness bound)",
                e.score
            );
        }

        // Anti-vacuous sister: alpha = -INF → FFP cannot fire (no positive
        // margin satisfies static_eval + margin ≤ -INF). The capture is
        // searched at full depth; its score updates `best` past
        // original_alpha = -INF → Exact bound stored. TT entry IS present
        // (proves the suppression assertion is non-vacuous — the TT path is
        // reachable when FFP is not blocking it).
        let tt_sister = Arc::new(TranspositionTable::new(1));
        let (ctx_sister, _stop_sister) = non_aborting_ctx_at_depth_with_tt(4, tt_sister.clone());
        let mut ab_sister = AlphaBetaMover::new();
        ab_sister.set_tt_for_test(Some(tt_sister.clone()));
        tt_sister.new_search();
        let _ = ab_sister.negamax_for_test(
            &mut pos.clone(),
            1,
            1,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx_sister,
        );
        assert_eq!(
            ab_sister.ffp_firings_for_test(),
            0,
            "sister fixture invariant: FFP must NOT fire under alpha = -INF; got {}",
            ab_sister.ffp_firings_for_test()
        );
        assert!(
            tt_sister.probe(pos.zobrist()).is_some(),
            "sister: with FFP not firing, the TT path must be reachable and an entry stored \
             (proves the FFP-test assertion is non-vacuous)"
        );
    }

    // -----------------------------------------------------------------------
    // M5.E — qsearch correctness.
    //
    // Tests for:
    //   #1 single-reply extension (one legal quiet, no captures, not in check),
    //   #2 true-stalemate detection at the qsearch horizon,
    //   #3 stalemate-conditional rook/bishop under-promotion in the move loop,
    //   #4 MAX_PLY ceiling guard in the !in_check arm.
    //
    // Counter resets are wired in `qsearch_for_test` and `negamax_for_test`
    // (both reset both M5.E counters), so direct-qsearch tests below can
    // read counters cleanly. Per plan §5.
    // -----------------------------------------------------------------------

    // ===== §5.1 — Helper tests for `stalemate_avoiding_under_promos`. =====

    /// Input `QueenPromo` from a7→a8 → returns
    /// `[Some(Move::new(A7, A8, RookPromo)), Some(Move::new(A7, A8, BishopPromo))]`.
    /// Pins the helper's flag-mapping table for the non-capture queen-promo arm.
    #[test]
    fn stalemate_avoiding_under_promos_for_queen_promo_returns_rook_and_bishop() {
        use crate::Square;
        use crate::mov::MoveFlag;
        let qp = Move::new(Square::A7, Square::A8, MoveFlag::QueenPromo);
        let result = stalemate_avoiding_under_promos(qp);
        assert_eq!(
            result[0],
            Some(Move::new(Square::A7, Square::A8, MoveFlag::RookPromo)),
            "first slot must be RookPromo at the same (from, to)"
        );
        assert_eq!(
            result[1],
            Some(Move::new(Square::A7, Square::A8, MoveFlag::BishopPromo)),
            "second slot must be BishopPromo at the same (from, to)"
        );
    }

    /// Input `QueenPromoCapture` → returns the `*PromoCapture` variants at
    /// the same (from, to). Pins the capture-aware branch of the helper.
    #[test]
    fn stalemate_avoiding_under_promos_for_queen_promo_capture_returns_capture_variants() {
        use crate::Square;
        use crate::mov::MoveFlag;
        let qpc = Move::new(Square::B7, Square::A8, MoveFlag::QueenPromoCapture);
        let result = stalemate_avoiding_under_promos(qpc);
        assert_eq!(
            result[0],
            Some(Move::new(
                Square::B7,
                Square::A8,
                MoveFlag::RookPromoCapture
            )),
            "first slot must be RookPromoCapture at the same (from, to)"
        );
        assert_eq!(
            result[1],
            Some(Move::new(
                Square::B7,
                Square::A8,
                MoveFlag::BishopPromoCapture
            )),
            "second slot must be BishopPromoCapture at the same (from, to)"
        );
    }

    /// Input is any non-queen-promo flag → returns `[None, None]`. Pins the
    /// helper-level domain guard against future call-site refactors that
    /// drop the queen-promo gate. Exhaustive across all 12 non-queen-promo
    /// flags so a flag-list mutation in the helper would be caught.
    #[test]
    fn stalemate_avoiding_under_promos_for_non_queen_promo_returns_empty() {
        use crate::Square;
        use crate::mov::MoveFlag::*;
        // Every flag except QueenPromo and QueenPromoCapture must produce
        // `[None, None]`. Includes RookPromo / BishopPromo / KnightPromo and
        // their capture variants — under-promos passed in directly should
        // not synthesize further variants.
        for flag in [
            Quiet,
            DoublePush,
            KingCastle,
            QueenCastle,
            Capture,
            EnPassant,
            KnightPromo,
            BishopPromo,
            RookPromo,
            KnightPromoCapture,
            BishopPromoCapture,
            RookPromoCapture,
        ] {
            let mv = Move::new(Square::A1, Square::B1, flag);
            let result = stalemate_avoiding_under_promos(mv);
            assert_eq!(
                result,
                [None, None],
                "stalemate_avoiding_under_promos({flag:?}) must return [None, None]"
            );
        }
    }

    /// (from, to) of returned moves equals the input's (from, to). Pins the
    /// helper does not accidentally swap squares or rebase the move geometry.
    #[test]
    fn stalemate_avoiding_under_promos_preserves_from_and_to() {
        use crate::Square;
        use crate::mov::MoveFlag;
        // Sample a few (from, to) pairs at the rank-7→rank-8 promotion
        // geometry (white) and rank-2→rank-1 (black).
        for (from, to, flag) in [
            (Square::A7, Square::A8, MoveFlag::QueenPromo),
            (Square::B7, Square::C8, MoveFlag::QueenPromoCapture),
            (Square::H2, Square::H1, MoveFlag::QueenPromo),
            (Square::E2, Square::F1, MoveFlag::QueenPromoCapture),
        ] {
            let mv = Move::new(from, to, flag);
            let [r, b] = stalemate_avoiding_under_promos(mv);
            let r = r.expect("rook variant must be Some for queen-promo input");
            let b = b.expect("bishop variant must be Some for queen-promo input");
            assert_eq!(r.from_square(), from, "rook variant from-square preserved");
            assert_eq!(r.to_square(), to, "rook variant to-square preserved");
            assert_eq!(
                b.from_square(),
                from,
                "bishop variant from-square preserved"
            );
            assert_eq!(b.to_square(), to, "bishop variant to-square preserved");
        }
    }

    /// The two synthesized moves are NEVER `KnightPromo*` or `QueenPromo*`.
    /// Pins the M5.E scope commitment (knight-promo's fork-tactics motivation
    /// is separate; out of scope per ADR-0027 §1) and protects against a
    /// future flag-table mutation that would silently re-enable knight
    /// synthesis.
    #[test]
    fn stalemate_avoiding_under_promos_does_not_include_knight_or_queen() {
        use crate::Square;
        use crate::mov::MoveFlag;
        for input_flag in [MoveFlag::QueenPromo, MoveFlag::QueenPromoCapture] {
            let mv = Move::new(Square::A7, Square::A8, input_flag);
            let [r, b] = stalemate_avoiding_under_promos(mv);
            for opt in [r, b] {
                let m = opt.expect("queen-promo input must produce Some variants");
                let f = m.flag();
                assert!(
                    !matches!(
                        f,
                        MoveFlag::KnightPromo
                            | MoveFlag::KnightPromoCapture
                            | MoveFlag::QueenPromo
                            | MoveFlag::QueenPromoCapture
                    ),
                    "synthesized variant must not be Knight* or Queen*; got {f:?}"
                );
            }
        }
    }

    // ===== §5.2 — Single-reply extension behavior tests. =====

    /// M5.E #1 fires when filter is empty AND exactly one legal quiet exists,
    /// not in check. Anti-vacuous sister: a fixture with 2+ legal quiets
    /// returns stand-pat (counter stays 0).
    ///
    /// Fixture (1) — single legal quiet: Black to move; Black King a8 is
    /// trapped (a7/b7/b8 all attacked by Qb6); Black pawn a5 has the unique
    /// legal move a5→a4 (a quiet single-push). The qsearch filter rejects
    /// quiets, so `moves_vec` is empty, but `ml.len() == 1` and the single
    /// move is `is_quiet`. Single-reply extension must fire.
    ///
    /// Anti-vacuous sister: KvK with white to move has multiple legal king
    /// moves; the M5.E #1 path's `ml.len() == 1` condition is not met and
    /// the path falls through to stand-pat. Counter stays 0.
    #[test]
    fn qsearch_single_reply_fires_when_filter_empty_and_one_legal_quiet() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        // Single-reply fixture: black to move, exactly one legal quiet
        // (pa5→a4). White pawn placed on h2 (not h1) so the position is
        // legal (white pawns on rank 1 are not legal in real chess).
        let pos = Position::from_fen("k7/8/1Q6/p7/8/8/7P/K7 b - - 0 1")
            .expect("single-reply fixture FEN must parse");

        // Fixture validation: not in check, no captures available, exactly
        // one legal move, and that move is quiet.
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.len(),
            1,
            "fixture invariant: exactly one legal move; got {}",
            ml.len()
        );
        let only_mv = ml.iter().next().expect("ml.len() == 1");
        assert!(
            is_quiet(only_mv),
            "fixture invariant: the unique legal move must be quiet; got flag {:?}",
            only_mv.flag()
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);
        assert!(
            ab.qsearch_single_reply_firings_for_test() >= 1,
            "M5.E #1 must fire at the root frame on the single-legal-quiet fixture; \
             got firings = {}",
            ab.qsearch_single_reply_firings_for_test()
        );

        // Anti-vacuous sister: KvK with multiple legal king moves. ml.len() != 1
        // → M5.E #1 path not entered. Counter stays 0.
        let pos_sister =
            Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN must parse");
        let mut ml_sister = MoveList::new();
        generate_moves(&pos_sister, &mut ml_sister);
        assert!(
            ml_sister.len() >= 2,
            "sister fixture invariant: 2+ legal moves; got {}",
            ml_sister.len()
        );
        let mut ab_sister = AlphaBetaMover::new();
        ab_sister.history = vec![pos_sister.zobrist()];
        let (ctx_sister, _stop_sister) = non_aborting_ctx();
        let _score_sister =
            ab_sister.qsearch_for_test(&mut pos_sister.clone(), -INF, INF, 1, &ctx_sister);
        assert_eq!(
            ab_sister.qsearch_single_reply_firings_for_test(),
            0,
            "sister: M5.E #1 must NOT fire when ml.len() != 1; got {}",
            ab_sister.qsearch_single_reply_firings_for_test()
        );
    }

    /// In-check arm runs first; M5.E #1 path is unreachable when in_chk is
    /// true. Pins the §4.2 ordering (in-check arm precedes the not-in-check
    /// branches).
    ///
    /// Fixture: White Ke1 in check from Black Re2. Multiple legal evasions
    /// (Kxe2, Kd1, Kf1) — the in-check arm runs the evasion search and
    /// returns its score; the M5.E #1 single-reply branch is structurally
    /// unreachable.
    #[test]
    fn qsearch_single_reply_does_not_fire_when_in_check() {
        use crate::movegen::in_check;
        let pos = Position::from_fen("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1")
            .expect("in-check fixture FEN must parse");
        assert!(in_check(&pos), "fixture invariant: white must be in check");

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "M5.E #1 must NOT fire when in check; got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    /// Filter is non-empty (capture available) → the move loop runs and the
    /// M5.E #1 path is unreachable. Pins the §4.2 condition that M5.E #1 is
    /// gated on `moves_vec.is_empty()`.
    ///
    /// Fixture: White Pe4 can capture pd5. The qsearch filter accepts the
    /// capture, populating `moves_vec`. The empty-filter terminal branch
    /// (where M5.E #1 lives) is never reached.
    #[test]
    fn qsearch_single_reply_does_not_fire_when_filter_nonempty() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("4k3/8/8/3p4/4P3/8/8/4K3 w - - 0 1")
            .expect("capture-available fixture FEN must parse");
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert!(
            !captures.is_empty(),
            "fixture invariant: at least one capture present; got 0"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "M5.E #1 must NOT fire when the filter is non-empty; got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    /// M5.E #1's recursion propagates the child's score (negated), NOT the
    /// parent's stand-pat. Construct the parent's qsearch result via
    /// `qsearch_for_test`, then manually make the unique move and compute
    /// `-qsearch(child, -beta, -alpha, ply+1)`; the two must match.
    ///
    /// Pins the recursion's window-negation, ply-increment, and result
    /// negation against any of the three possible mutation classes.
    #[test]
    fn qsearch_single_reply_recursion_propagates_score_correctly() {
        use crate::movegen::{MoveList, generate_moves};
        let pos = Position::from_fen("k7/8/1Q6/p7/8/8/7P/K7 b - - 0 1")
            .expect("single-reply fixture FEN must parse");

        // Confirm fixture exhibits the regime (ml.len() == 1, quiet move).
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(ml.len(), 1, "fixture invariant: ml.len() == 1");
        let only_mv = ml.iter().next().expect("one move");
        assert!(
            is_quiet(only_mv),
            "fixture invariant: unique legal move must be quiet"
        );

        // Drive parent qsearch — fires M5.E #1 → recurses → returns negated child.
        let mut ab_parent = AlphaBetaMover::new();
        ab_parent.history = vec![pos.zobrist()];
        let (ctx_parent, _stop_parent) = non_aborting_ctx();
        let parent_score = ab_parent.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx_parent);

        // Manual reconstruction: make the unique move, qsearch the child at
        // ply+1 with negated window, negate the result.
        let mut child_pos = pos;
        let _undo = child_pos.make_move(only_mv);
        let mut ab_child = AlphaBetaMover::new();
        ab_child.history = vec![pos.zobrist(), child_pos.zobrist()];
        let (ctx_child, _stop_child) = non_aborting_ctx();
        let (ca, cb) = negate_window(-INF, INF);
        let child_score = ab_child.qsearch_for_test(&mut child_pos, ca, cb, 2, &ctx_child);
        let expected = -child_score;

        assert_eq!(
            parent_score, expected,
            "M5.E #1 must propagate the negated child score; got parent={parent_score}, expected={expected}"
        );
    }

    /// True stalemate at the qsearch horizon (M5.E #2): zero legal moves,
    /// not in check → return 0. Pre-M5.E this conflated with the false-
    /// stalemate guard and returned stand-pat.
    ///
    /// Fixture: Black Kh8, White Qf7, White Kg6, Black to move. Black is
    /// stalemated — g7 attacked by Qf7 (rank 7), h7 attacked by Kg6 + Qf7,
    /// g8 attacked by Qf7 (f7-g8 diagonal). h8 itself is unattacked (Qf7→h8
    /// is 2 files / 1 rank — not a queen line; Kg6→h8 is 1 file / 2 ranks
    /// — not adjacent), so Black is NOT in check. Black's stand-pat is
    /// large-negative (down a queen), which defeats an always-return-0
    /// stub: a correct M5.E #2 implementation must return 0 *despite* the
    /// non-zero stand-pat.
    #[test]
    fn qsearch_returns_zero_on_true_stalemate_when_not_in_check() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1")
            .expect("true-stalemate fixture FEN must parse");

        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.is_empty(),
            "fixture invariant: stalemate position must have zero legal moves; got {}",
            ml.len()
        );
        assert!(
            !in_check(&pos),
            "fixture invariant: stalemate is not-in-check"
        );
        // Anti-vacuous: stand-pat must be != 0 to defeat an always-return-0 stub.
        let sp = evaluate(&pos);
        assert!(
            sp != 0,
            "fixture invariant: evaluate must be non-zero so the test discriminates an always-return-0 stub; got {sp}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);
        assert_eq!(
            score, 0,
            "true stalemate must return 0 (FIDE 9.2 draw); got {score}"
        );
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "M5.E #1 must NOT fire on the empty-ml stalemate path; got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    /// M3.D anchor: in-check + zero legal moves → `-(MATE - ply)`. Unchanged
    /// across the M5.E refinement; the test pins it as a regression anchor.
    ///
    /// Fixture: Standard K+Q mating-net. Black Ka8 in check from Qb7
    /// (diagonal). Kings on a8 and c6 (not adjacent — legal position).
    /// Black escapes: Ka7 (Qb7 attacks rank 7 → illegal), Kb8 (Qb7 attacks
    /// file b → illegal), Kxb7 (Qb7 defended by Kc6 → illegal). 0 evasions
    /// in check → checkmate.
    #[test]
    fn qsearch_returns_mate_in_ply_on_true_mate() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("k7/1Q6/2K5/8/8/8/8/8 b - - 0 1")
            .expect("true-mate fixture FEN must parse");
        assert!(in_check(&pos), "fixture invariant: black in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.len(),
            0,
            "fixture invariant: zero legal moves (checkmate); got {}",
            ml.len()
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        assert_eq!(
            score,
            -(MATE - ply as i32),
            "in-check + no legal moves must return -(MATE - ply); got {score}"
        );
    }

    /// MAX_PLY ceiling guard (M5.E #4): not in check at `ply == MAX_PLY - 1`
    /// → return `evaluate(pos)` (the side-to-move-relative stand-pat) without
    /// recursing. Counter stays 0 (no recursion firings possible).
    ///
    /// Fixture: KvKR (`4k3/8/8/8/8/8/8/4K2R w - - 0 1`). White materially up
    /// a rook → `evaluate(pos) > 0`, which defeats an always-return-0 stub
    /// (the prior KvK fixture had `evaluate == 0`, making the test vacuous).
    /// Drive at `ply = MAX_PLY as u32 - 1`.
    #[test]
    fn qsearch_max_ply_guard_returns_stand_pat_at_ceiling_when_not_in_check() {
        use crate::eval::evaluate;
        use crate::movegen::in_check;
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K2R w - - 0 1")
            .expect("KvKR fixture FEN must parse");
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let expected = evaluate(&pos);
        assert!(
            expected != 0,
            "anti-vacuous: stand-pat must be non-zero on this fixture; got {expected}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let ply = MAX_PLY as u32 - 1;
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        assert_eq!(
            score, expected,
            "MAX_PLY ceiling guard must return evaluate(pos) = stand-pat; got {score}, expected {expected}"
        );
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "no recursion at the ceiling means no M5.E #1 firings possible; got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    /// MAX_PLY ceiling guard does NOT fire on the in-check arm. The plan's
    /// design choice (§4.1) is to leave in-check at ceiling unguarded —
    /// returning a fabricated mate-in-ply score would propagate a false
    /// mate; the in-check evasion arm runs naturally even at ply=MAX_PLY-1.
    /// The structural rarity of an in-check chain reaching MAX_PLY makes
    /// this acceptable.
    ///
    /// Fixture: White Ke1 + Qa1 in check from Re2 (Black Ke8). Kxe2
    /// captures the attacking rook; with white still owning a queen
    /// post-capture, qsearch returns a materially-winning score from
    /// white's POV. Other legal evasions (Kd1, Kf1) leave the rook on
    /// the board; MVV-LVA orders Kxe2 first and the capture's score
    /// dominates. Queen on a1 deliberately positioned so it neither
    /// attacks e2 (cannot capture rook itself) nor blocks the check
    /// (cannot interpose on e-file). Drive at `ply = MAX_PLY as u32 - 1`.
    ///
    /// Assertions:
    ///   - `score.abs() < MATE_IN_MAX_PLY` (no fabricated mate-in-ply).
    ///   - `score > 0` (Kxe2 wins material — defeats a stub returning 0).
    ///   - `qsearch_single_reply_firings == 0` (in-check arm runs the
    ///     evasion path; M5.E #1 single-reply extension lives on the
    ///     not-in-check arm and must NOT fire here).
    #[test]
    fn qsearch_max_ply_guard_does_not_fire_in_check() {
        use crate::movegen::in_check;
        let pos = Position::from_fen("4k3/8/8/8/8/8/4r3/Q3K3 w - - 0 1")
            .expect("in-check fixture FEN must parse");
        assert!(in_check(&pos), "fixture invariant: white in check");

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let ply = MAX_PLY as u32 - 1;
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        assert!(
            score.abs() < MATE_IN_MAX_PLY,
            "in-check arm at MAX_PLY-1 must return a centipawn score (no fabricated mate); \
             got score = {score}, MATE_IN_MAX_PLY = {MATE_IN_MAX_PLY}"
        );
        assert!(
            score > 0,
            "Kxe2 evasion wins the rook → score must be positive (defeats an always-zero \
             stub); got {score}"
        );
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "in-check arm must not invoke the M5.E #1 single-reply extension \
             (lives on the not-in-check arm); got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    // NOTE: The plan listed `qsearch_single_reply_chain_fires_recursively`
    // (recursive M5.E #1 chains across both sides). A natural fixture
    // requires both sides simultaneously zugzwanged into a single quiet
    // reply, which is hard to construct from scratch. The recursion's
    // correctness is structurally invariant — qsearch's single-reply
    // branch is one recursive call site with no special chain handling,
    // so `qsearch_single_reply_recursion_propagates_score_correctly`
    // (which exercises one recursion level) is sufficient to pin the
    // recursive structure; the chain test would add no independent
    // coverage.

    /// Documents the structural invariant that under-promo single-reply
    /// uniqueness (`ml.len() == 1` AND the unique move is an under-promo)
    /// is unreachable: legal-direct movegen (`src/movegen/pawn.rs`) emits
    /// all four promotion variants whenever a promotion is geometrically
    /// legal, so an under-promo can never appear alone. This invariant is
    /// what allows M5.E #1's single-reply branch to recurse on the unique
    /// move without an `is_quiet` gate (plan §0 #1; ADR-0027): when the
    /// qsearch filter rejects the unique move and `ml.len() == 1`, the
    /// move is provably a non-promo quiet (Quiet | DoublePush | KingCastle
    /// | QueenCastle).
    ///
    /// Pinned by enumerating several promotion-bearing positions (quiet
    /// promo, capture promo, near-stalemate-with-promo) and asserting the
    /// movelist contains all four promo variants in each case.
    #[test]
    fn qsearch_single_reply_under_promo_uniqueness_is_structurally_unreachable() {
        use crate::mov::MoveFlag;
        use crate::movegen::{MoveList, generate_moves};
        for fen in [
            // Quiet promotion (white pawn a7 → a8, no capture available).
            "4k3/P7/8/8/8/8/8/4K3 w - - 0 1",
            // Capture promotion (white pawn b7 can capture ra8 OR push to b8).
            "r3k3/1P6/8/8/8/8/8/4K3 w - - 0 1",
            // Near-stalemate-with-promo: shared M5.E #3 fixture; f7 → f8
            // promotion exists in a position where most other moves are
            // sharply constrained.
            "6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1",
        ] {
            let pos = Position::from_fen(fen).expect("promo fixture FEN must parse");
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            // Find the (from, to) of any promotion in the list — there must
            // be at least one for the fixture to exercise the invariant.
            let promo_to = ml
                .iter()
                .find(|m| {
                    matches!(
                        m.flag(),
                        MoveFlag::KnightPromo
                            | MoveFlag::BishopPromo
                            | MoveFlag::RookPromo
                            | MoveFlag::QueenPromo
                            | MoveFlag::KnightPromoCapture
                            | MoveFlag::BishopPromoCapture
                            | MoveFlag::RookPromoCapture
                            | MoveFlag::QueenPromoCapture
                    )
                })
                .map(|m| (m.from_square(), m.to_square()))
                .expect("fixture invariant: at least one promotion must be present");

            // Capture vs quiet path: collect the promo variants at this (from, to).
            let mut have_knight = false;
            let mut have_bishop = false;
            let mut have_rook = false;
            let mut have_queen = false;
            let mut is_capture_path = false;
            for m in ml.iter() {
                if (m.from_square(), m.to_square()) != promo_to {
                    continue;
                }
                match m.flag() {
                    MoveFlag::KnightPromo => have_knight = true,
                    MoveFlag::BishopPromo => have_bishop = true,
                    MoveFlag::RookPromo => have_rook = true,
                    MoveFlag::QueenPromo => have_queen = true,
                    MoveFlag::KnightPromoCapture => {
                        have_knight = true;
                        is_capture_path = true;
                    }
                    MoveFlag::BishopPromoCapture => {
                        have_bishop = true;
                        is_capture_path = true;
                    }
                    MoveFlag::RookPromoCapture => {
                        have_rook = true;
                        is_capture_path = true;
                    }
                    MoveFlag::QueenPromoCapture => {
                        have_queen = true;
                        is_capture_path = true;
                    }
                    _ => {}
                }
            }
            assert!(
                have_knight && have_bishop && have_rook && have_queen,
                "fixture {fen}: movegen must emit all four promo variants at {promo_to:?} \
                 (capture_path = {is_capture_path}); got knight = {have_knight}, \
                 bishop = {have_bishop}, rook = {have_rook}, queen = {have_queen}"
            );
        }
    }

    /// Two legal quiet moves and no captures → the qsearch capture filter
    /// is empty, `ml.len() != 1`, so the M5.E #1 single-reply path does
    /// NOT fire. qsearch returns stand-pat (= `evaluate(pos)`).
    ///
    /// Fixture: `8/8/8/8/8/4k3/4P3/4K3 w - - 0 1` — white Ke1 + Pe2,
    /// black Ke3 (kings not adjacent: e1 ↔ e3 are 2 ranks apart). Pe2
    /// cannot push to e3 (blocked by black king); pe2 has no capture. White
    /// king's only legal moves are Kd1, Kf1 (d2/e2/f2 blocked or attacked).
    /// ml.len() == 2, both quiet — confirms the regime.
    #[test]
    fn qsearch_two_legal_quiets_and_no_captures_returns_stand_pat() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("8/8/8/8/8/4k3/4P3/4K3 w - - 0 1")
            .expect("two-legal-quiets fixture FEN must parse");
        assert!(!in_check(&pos), "fixture invariant: not in check");

        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.len() >= 2,
            "fixture invariant: at least two legal moves; got {}",
            ml.len()
        );
        assert!(
            ml.iter().all(|m| !m.is_capture()),
            "fixture invariant: no captures present"
        );

        let expected = evaluate(&pos);
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 1, &ctx);
        assert_eq!(
            score, expected,
            "ml.len() != 1 with no captures must return stand-pat; got {score}, expected {expected}"
        );
        assert_eq!(
            ab.qsearch_single_reply_firings_for_test(),
            0,
            "M5.E #1 must NOT fire when ml.len() != 1; got {}",
            ab.qsearch_single_reply_firings_for_test()
        );
    }

    // ===== §5.3 — Stalemate-conditional under-promo behavior tests. =====
    //
    // Shared fixture for the under-promo tests:
    //
    //   White Ke6, Bg8, Pf7, Bd3. Black Kh8.
    //   FEN: `6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1`.
    //
    // After 1.f8=Q (the only filtered move qsearch considers):
    //   - Qf8 attacks rank 8 (blocked at Bg8) — does NOT see h8.
    //   - Qf8 attacks the f8-g7-h6 diagonal (covers g7).
    //   - Bd3 covers h7. Bg8 (own piece) occupies g8 and is defended by Qf8
    //     via rank 8.
    //   - Black Kh8 has no escape: g7 attacked by Qf8 (diagonal), g8 capture
    //     blocked (Bg8 defended), h7 attacked by Bd3.
    //   - Black not in check (Qf8 blocked by own Bg8 from seeing h8).
    //   ⇒ STALEMATE. Queen-promo's child qsearch returns 0 under M5.E #2.
    //
    // After 1.f8=R (rook-promo at the same (from, to)):
    //   - Rf8 has the same rank-8 + file-f attacks as Qf8 (rank 8 blocked at
    //     Bg8) but NO diagonal — g7 is no longer attacked by the promoted
    //     piece. Ke6 also doesn't attack g7. Bd3 doesn't attack g7.
    //   ⇒ Black plays Kg7 (the unique legal quiet); the M5.E #1 single-reply
    //     extension fires at the child frame, recursing on Kg7. White's
    //     qsearch sees no further captures → returns the materially winning
    //     stand-pat (white has K + B + B + R; black has K only). Negated
    //     back, the rook-promo's contribution is large and positive.
    //
    // After 1.f8=B (bishop-promo at the same (from, to)):
    //   - Bf8 has only diagonal attacks (no rank-8 → does NOT defend Bg8).
    //   - Black Kh8 plays Kxg8 (capture of the now-undefended Wbishop on g8).
    //   - Resulting position: White K + B + B vs Black K → eval positive
    //     (sufficient material). Negated back, bishop-promo also contributes
    //     a positive (smaller) score.
    //
    // The under-promo loop runs both rook + bishop unconditionally on the
    // queen_promo_stalemates flag, so `qsearch_under_promo_firings == 2` on
    // the default (-INF, INF) window. The returned `best` is the max of
    // {queen-promo (0), rook-promo (large +), bishop-promo (smaller +)} ⇒
    // strictly > 0, exposing the M5.E #3 strength gain.

    /// M5.E #3 fires when queen-promo's post-make child is stalemate. Both
    /// the rook-promo and bishop-promo synthesized variants are recursed →
    /// `qsearch_under_promo_firings >= 2`.
    #[test]
    fn qsearch_under_promo_fires_when_queen_promo_stalemates() {
        use crate::mov::MoveFlag;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1")
            .expect("queen-promo-stalemate fixture FEN must parse");
        assert!(
            !in_check(&pos),
            "fixture invariant: white not in check at root"
        );

        // Fixture invariant: 1.f8=Q must stalemate Black (zero legal replies
        // AND not in check) — this is what gates the M5.E #3 firing.
        let qp_move = Move::new(crate::Square::F7, crate::Square::F8, MoveFlag::QueenPromo);
        let mut after_qp = pos;
        let _undo_qp = after_qp.make_move(qp_move);
        let mut child_ml = MoveList::new();
        generate_moves(&after_qp, &mut child_ml);
        assert!(
            child_ml.is_empty() && !in_check(&after_qp),
            "fixture invariant: 1.f8=Q must stalemate (zero legal moves AND not in check); \
             got moves = {}, in_check = {}",
            child_ml.len(),
            in_check(&after_qp)
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);
        // Assert exact firing count (== 2, not >= 2) — harmonizes with the
        // other two under-promo tests on this fixture and pins double-counting
        // regressions (e.g., a bug that re-runs the helper).
        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            2,
            "M5.E #3 must fire rook + bishop under-promo recursions exactly once each; got firings = {}",
            ab.qsearch_under_promo_firings_for_test()
        );
    }

    /// M5.E #3 does NOT fire when queen-promo gives checkmate (post-make
    /// child has 0 legal moves AND in_check). The predicate's `!in_check`
    /// half rejects this case.
    ///
    /// Fixture: White Kf6, Bd5, Pf7. Black Kh8, Ng8. White is in check from
    /// Ng8 (knight on g8 attacks f6). White's evasion 1.fxg8=Q+ promotes
    /// the f7-pawn while capturing the checking knight. After 1.fxg8=Q+,
    /// Qg8 checks BK h8 (rank 8 adjacent), and the only nominal escapes
    /// are: Kg7 (attacked by Qg8 + Kf6), Kh7 (attacked by Qg8 diagonal),
    /// Kxg8 (Qg8 defended by Bd5 along the now-cleared d5-g8 diagonal once
    /// the f7 pawn has promoted away). All escapes illegal → checkmate.
    /// The queen-promo's child is in_check → predicate false → counter 0.
    ///
    /// Note: white is in check at the root, so qsearch's in-check arm runs
    /// with no capture filter (full evasion list). Several other evasions
    /// also exist (Ke5, Kg5, Kg7, the under-promo variants of fxg8); none
    /// of them is a queen-promo with stalemate child → counter stays 0
    /// across the whole move loop.
    #[test]
    fn qsearch_under_promo_does_not_fire_when_queen_promo_checkmates() {
        use crate::mov::MoveFlag;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("6nk/5P2/5K2/3B4/8/8/8/8 w - - 0 1")
            .expect("queen-promo-checkmate fixture FEN must parse");
        // White is in check from Ng8 — the in-check arm of qsearch runs the
        // full evasion list, including the f7×g8=Q capture-promotion that
        // delivers checkmate. The fixture invariant for THIS test is that
        // the queen-promo evasion's child is mate (in_check), not stalemate.
        assert!(in_check(&pos), "fixture invariant: white in check at root");

        // Fixture invariant: 1.fxg8=Q+ must checkmate Black (zero legal
        // replies AND in check) — this is what blocks the M5.E #3 firing.
        let qpc = Move::new(
            crate::Square::F7,
            crate::Square::G8,
            MoveFlag::QueenPromoCapture,
        );
        let mut after = pos;
        let _undo = after.make_move(qpc);
        let mut child_ml = MoveList::new();
        generate_moves(&after, &mut child_ml);
        assert!(
            child_ml.is_empty() && in_check(&after),
            "fixture invariant: 1.fxg8=Q+ must checkmate (zero legal moves AND in check); \
             got moves = {}, in_check = {}",
            child_ml.len(),
            in_check(&after)
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);
        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            0,
            "M5.E #3 must NOT fire when queen-promo checkmates (child in check); got {}",
            ab.qsearch_under_promo_firings_for_test()
        );
    }

    /// M5.E #3 does NOT fire when queen-promo leaves the opponent at least
    /// one legal reply (no stalemate). The predicate's `child_ml.is_empty()`
    /// half rejects this case.
    ///
    /// Fixture: White Kc1, Pe7. Black Ka1. After 1.e8=Q: Black Ka1 has Ka2
    /// legal (a2 not attacked by Qe8 nor by Kc1). Counter == 0.
    #[test]
    fn qsearch_under_promo_does_not_fire_when_queen_promo_leaves_opponent_with_legal_replies() {
        use crate::mov::MoveFlag;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("8/4P3/8/8/8/8/8/k1K5 w - - 0 1")
            .expect("queen-promo-with-replies fixture FEN must parse");
        assert!(
            !in_check(&pos),
            "fixture invariant: white not in check at root"
        );

        // Fixture invariant: 1.e8=Q must leave Black with at least one
        // legal reply — this is what blocks the M5.E #3 firing.
        let qp = Move::new(crate::Square::E7, crate::Square::E8, MoveFlag::QueenPromo);
        let mut after = pos;
        let _undo = after.make_move(qp);
        let mut child_ml = MoveList::new();
        generate_moves(&after, &mut child_ml);
        assert!(
            !child_ml.is_empty(),
            "fixture invariant: 1.e8=Q must leave Black with at least one legal reply; got 0 legal moves"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);
        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            0,
            "M5.E #3 must NOT fire when queen-promo leaves opponent with replies; got {}",
            ab.qsearch_under_promo_firings_for_test()
        );
    }

    /// M5.E #3 does NOT fire on non-queen-promo moves. The predicate's
    /// flag-match (`QueenPromo | QueenPromoCapture`) gates the entire
    /// stalemate detection; on a Capture / Quiet move the helper would also
    /// return `[None, None]`, but the gate fires first and skips the
    /// movegen-and-stalemate-check work.
    ///
    /// Fixture: White Pe4 captures Black pd5. Filter accepts the capture;
    /// no queen-promo present in the move list. Counter == 0.
    #[test]
    fn qsearch_under_promo_does_not_fire_for_non_queen_promo_moves() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("4k3/8/8/3p4/4P3/8/8/4K3 w - - 0 1")
            .expect("capture-fixture FEN must parse");
        assert!(!in_check(&pos), "fixture invariant: white not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert!(
            !captures.is_empty(),
            "fixture invariant: at least one capture present"
        );
        // Sanity: no queen-promos in the move list.
        let qpromos: Vec<_> = ml
            .iter()
            .filter(|m| {
                matches!(
                    m.flag(),
                    crate::mov::MoveFlag::QueenPromo | crate::mov::MoveFlag::QueenPromoCapture
                )
            })
            .collect();
        assert!(
            qpromos.is_empty(),
            "fixture invariant: no queen-promos present (so the M5.E #3 flag-gate is the discriminator)"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let _score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);
        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            0,
            "M5.E #3 must NOT fire for non-queen-promo moves; got {}",
            ab.qsearch_under_promo_firings_for_test()
        );
    }

    /// M5.E #3's strength gain: queen-promo stalemates (returns 0) while a
    /// rook-promo at the same (from, to) DOES NOT stalemate, exposing a
    /// winning continuation. The returned `best` reflects the under-promo's
    /// material gain.
    ///
    /// **Fixture construction notes** (per plan §5.3):
    ///
    /// FEN: `6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1` (White Ke6, Bg8, Pf7, Bd3.
    /// Black Kh8.)
    ///
    /// Regime invariants (verified via runtime fixture-validation
    /// assertions below):
    ///
    ///   (a) After 1.f8=Q, Black has zero legal replies AND is not in check
    ///       — true stalemate. The queen-promo's recursed-and-negated score
    ///       is therefore 0 under M5.E #2.
    ///   (b) After 1.f8=R, Black has at least one legal reply (Kg7) —
    ///       rook-promo does NOT stalemate. The diagonal attack on g7 from
    ///       the queen-promo is the load-bearing difference: Rf8 has no
    ///       diagonal coverage of g7, while Bd3 does NOT cover g7 either,
    ///       so Black escapes to g7.
    ///   (c) After 1...Kg7 in the rook-promo line, white has K + R + B + B
    ///       vs lone black K → stand-pat is materially winning. Negated
    ///       back to the under-promo recursion's parent, this surfaces a
    ///       large positive score.
    ///   (d) After 1.f8=B, Black plays Kxg8 (capturing the now-undefended
    ///       white bishop). Resulting position: K + B + B vs K → still a
    ///       sufficient-material position (eval > 0); contributes a
    ///       positive (smaller) score.
    ///
    /// The fixture is hand-constructed; the under-promo loop is documented
    /// to fire both rook AND bishop unconditionally per §4.3, so
    /// `qsearch_under_promo_firings == 2` (the expected steady-state count).
    /// Returned `best` is `max(queen=0, rook=+large, bishop=+positive) >= rook >> 0`.
    #[test]
    fn qsearch_under_promo_finds_winning_rook_promo_when_queen_stalemates() {
        use crate::mov::MoveFlag;
        use crate::movegen::{MoveList, generate_moves, in_check};
        let pos = Position::from_fen("6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1")
            .expect("under-promo-winning fixture FEN must parse");
        assert!(!in_check(&pos), "fixture invariant: white not in check");

        // Regime invariant (a): queen-promo at f8 stalemates.
        let qp_move = Move::new(crate::Square::F7, crate::Square::F8, MoveFlag::QueenPromo);
        let mut after_qp = pos;
        let _undo_qp = after_qp.make_move(qp_move);
        let mut child_ml_qp = MoveList::new();
        generate_moves(&after_qp, &mut child_ml_qp);
        assert!(
            child_ml_qp.is_empty() && !in_check(&after_qp),
            "fixture invariant (a): 1.f8=Q must stalemate (zero legal moves AND not in check); \
             got moves = {}, in_check = {}",
            child_ml_qp.len(),
            in_check(&after_qp)
        );

        // Regime invariant (b): rook-promo at f8 does NOT stalemate.
        let rp_move = Move::new(crate::Square::F7, crate::Square::F8, MoveFlag::RookPromo);
        let mut after_rp = pos;
        let _undo_rp = after_rp.make_move(rp_move);
        let mut child_ml_rp = MoveList::new();
        generate_moves(&after_rp, &mut child_ml_rp);
        assert!(
            !child_ml_rp.is_empty(),
            "fixture invariant (b): 1.f8=R must NOT stalemate (rook-promo leaves an escape); got 0 legal moves"
        );

        // Capture stand-pat at the root before driving qsearch — the
        // tightened assertion below requires that the under-promo's
        // contribution surface ABOVE stand-pat (otherwise stand-pat alone
        // would mask sign-flip / wrong-direction-best-update mutations
        // on the under-promo branch).
        let stand_pat = crate::eval::evaluate(&pos);
        assert!(
            stand_pat > 0,
            "fixture invariant: stand-pat must be positive (white materially up at root); got {stand_pat}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, 0, &ctx);

        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            2,
            "M5.E #3 must fire BOTH rook + bishop under-promo recursions \
             (the under-promo loop runs unconditionally on the stalemate flag); got {}",
            ab.qsearch_under_promo_firings_for_test()
        );
        // Tightened assertion (mutation-killing): the under-promo's
        // contribution must surface ABOVE stand-pat. The rook-promo's
        // continuation reaches K+R+B+B vs K (~+1200 cp), well above the
        // root stand-pat (~+800 cp). A sign-flip mutation on the
        // under-promo recursion (line 2194:39) would leave best at
        // stand-pat; a wrong-direction `best`-update (line 2202:36) would
        // also leave best at stand-pat. Only the production sign + update
        // direction surfaces a score > stand-pat. Margin of 100 cp covers
        // PSQT variation across the path.
        assert!(
            score > stand_pat + 100,
            "fixture's M5.E #3 strength gain: returned best must reflect the winning under-promo \
             continuation ABOVE stand-pat ({stand_pat}); got {score}. The under-promo's score \
             (~+1200 cp from K+R+B+B vs K) must surface — wrong-sign / wrong-direction-update \
             mutations would leave best at stand-pat."
        );
    }

    // M5.E #3 ordering note (§4.3): the production code runs the under-promo
    // loop BEFORE the per-move beta cutoff. Constructing a discriminating
    // fixture is hard: the discrimination requires `stand_pat < beta <= 0` so
    // the move loop runs (no step-4 stand-pat cutoff) AND the queen-promo's
    // stalemate score (0) raises alpha to >= beta. Both conditions together
    // demand the side-to-move be materially behind (stand_pat < 0), but the
    // M5.E #3 trigger fires only on a queen-promo whose post-make child is
    // stalemated, which typically requires the promoting side to be materially
    // ahead (so the queen + remaining material covers the opponent's escape
    // squares). The two requirements push in opposite directions; no compact
    // natural fixture satisfies both.
    //
    // The ordering invariant is documented in plan §4.3 and ADR-0027 §6, and
    // pinned structurally by the production code's block layout (10a → 10b →
    // 10c). Final review's code reading is the load-bearing check; the test
    // is omitted rather than shipping a non-discriminating placeholder.

    /// Cross-check of helper integration: the synthesized rook + bishop
    /// moves preserve (from, to) of the underlying queen-promo. Pinned via
    /// the helper directly (no qsearch driving needed — the helper's
    /// from/to-preservation is the load-bearing invariant for the under-
    /// promo loop's make/unmake correctness).
    #[test]
    fn qsearch_under_promo_synthetic_moves_have_same_from_to_as_queen_promo() {
        use crate::Square;
        use crate::mov::MoveFlag;
        // Sample several (from, to) pairs spanning rank-7→8 (white) and
        // rank-2→1 (black) promotions, including capture variants.
        for (from, to, flag) in [
            (Square::A7, Square::A8, MoveFlag::QueenPromo),
            (Square::H7, Square::H8, MoveFlag::QueenPromo),
            (Square::B7, Square::A8, MoveFlag::QueenPromoCapture),
            (Square::A2, Square::A1, MoveFlag::QueenPromo),
            (Square::H2, Square::H1, MoveFlag::QueenPromo),
            (Square::E2, Square::F1, MoveFlag::QueenPromoCapture),
        ] {
            let qp = Move::new(from, to, flag);
            let [r, b] = stalemate_avoiding_under_promos(qp);
            let r = r.expect("rook variant must be Some for queen-promo input");
            let b = b.expect("bishop variant must be Some for queen-promo input");
            assert_eq!(
                (r.from_square(), r.to_square()),
                (from, to),
                "rook variant must preserve (from, to) of the queen-promo input"
            );
            assert_eq!(
                (b.from_square(), b.to_square()),
                (from, to),
                "bishop variant must preserve (from, to) of the queen-promo input"
            );
        }
    }

    /// Under-promo recursions fire AND demonstrate the alpha-respecting
    /// fail-soft contract: the under-promo scores update `best` but do NOT
    /// push it above the high alpha. Pins that the cutoff *within* the
    /// under-promo loop is keyed on `alpha >= beta` and that fail-soft
    /// returns from the under-promo recursions cannot exceed beta.
    ///
    /// Window: `alpha = 2000, beta = 2001`. The under-promo continuations
    /// score ~+1200 cp at the shared fixture (white up R+B+B vs lone K
    /// after rook-promo + Black's escape), so neither under-promo can beat
    /// alpha = 2000. The under-promo loop must still fire (gated on the
    /// stalemate flag, not on alpha/beta); `best` reflects the under-promos'
    /// fail-soft contributions but stays below alpha = 2000.
    #[test]
    fn qsearch_under_promo_respects_existing_alpha_beta_cutoff() {
        let pos = Position::from_fen("6Bk/5P2/4K3/8/8/3B4/8/8 w - - 0 1")
            .expect("under-promo-alpha-beta fixture FEN must parse");

        // alpha = 2000 is high enough that the under-promos' material gain
        // (~+1200 cp) cannot reach it; beta = 2001 keeps the window narrow
        // but ensures `alpha < beta` throughout. The under-promo loop must
        // execute both rook + bishop regardless of alpha-improvement.
        let alpha = 2000;
        let beta = 2001;

        // Fixture-invariant: stand-pat must be strictly below beta so the
        // step-4 stand-pat cutoff does NOT fire, otherwise the move loop
        // never runs and the test passes vacuously regardless of M5.E #3.
        let sp = crate::eval::evaluate(&pos);
        assert!(
            sp < beta,
            "fixture invariant: stand-pat must be < beta={beta} so the move loop runs; got {sp}"
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        let (ctx, _stop) = non_aborting_ctx();
        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, 0, &ctx);
        assert_eq!(
            ab.qsearch_under_promo_firings_for_test(),
            2,
            "under-promo loop must run BOTH rook + bishop regardless of alpha-improvement; got {}",
            ab.qsearch_under_promo_firings_for_test()
        );
        assert!(
            score > 0,
            "under-promo's contribution must surface in `best` (fail-soft return); got {score}"
        );
        assert!(
            score < alpha,
            "under-promo scores cannot exceed alpha = {alpha}; got {score}"
        );
    }

    // ===========================================================================
    // M5.F — Qsearch in TT.
    //
    // Tests for:
    //   §6.3 — `qsearch_tt_bound_for_completed_node` helper unit tests.
    //   §6.4 — probe behavior tests (will fail until probe block implemented).
    //   §6.5 — store behavior tests (will fail until store block implemented).
    //   §6.6 — TT-move ordering tests (will fail until ordering wired).
    //   §6.7 — negamax-qsearch interaction tests.
    //   §6.8 — mate-score round-trip tests.
    //   §6.10 — counter reset tests (marked #[ignore] — vacuous without impl).
    //
    // §6.1 and §6.2 tests are in src/tt.rs (the discriminator changes live there).
    // ===========================================================================

    // -----------------------------------------------------------------------
    // M5.F §6.3 — Helper unit tests for `qsearch_tt_bound_for_completed_node`
    //
    // The helper classifies a completed (non-terminal) qsearch node:
    //   - `best >= beta` → Lower
    //   - `best < beta`  → Upper
    //   - NEVER Exact (Stockfish 45e5e65 — non-terminal qsearch)
    //
    // The boundary case `best == beta` must be Lower (inclusive, kills
    // `>= → >` mutation). The "would-be-Exact zone" (`original_alpha <
    // best < beta`) must return Upper (kills any mutation that re-introduces
    // Exact for non-terminal qsearch).
    // -----------------------------------------------------------------------

    /// `qsearch_tt_bound_for_completed_node(best, beta)` returns `Lower`
    /// when `best > beta` and when `best == beta` (inclusive boundary).
    /// Sister cases:
    ///   - `best = beta + 1` → Lower (strict fail-high)
    ///   - `best = beta`     → Lower (boundary — kills `>= → >` mutation)
    ///   - `best = beta + 100` → Lower (clear fail-high)
    #[test]
    fn qsearch_tt_bound_lower_when_best_geq_beta() {
        let beta = 500;

        // Strict fail-high (best > beta).
        assert_eq!(
            qsearch_tt_bound_for_completed_node(beta + 1, beta),
            TtBound::Lower,
            "best > beta must be Lower"
        );
        assert_eq!(
            qsearch_tt_bound_for_completed_node(beta + 100, beta),
            TtBound::Lower,
            "best >> beta must be Lower"
        );

        // Boundary: `best == beta` must be Lower (inclusive `>=` not `>`).
        // This kills the `>= → >` mutation: with `>`, this case would be Upper.
        assert_eq!(
            qsearch_tt_bound_for_completed_node(beta, beta),
            TtBound::Lower,
            "best == beta must be Lower (inclusive boundary — kills >= → > mutation)"
        );
    }

    /// `qsearch_tt_bound_for_completed_node(best, beta)` returns `Upper`
    /// when `best < beta`. Sister case: `best = beta - 1` (just below boundary).
    #[test]
    fn qsearch_tt_bound_upper_when_best_lt_beta() {
        let beta = 500;

        // Just below boundary — kills `< → <=` mutation (with `<=`, `best = beta`
        // would be Upper, contradicting the `lower_at_boundary_zero` test).
        assert_eq!(
            qsearch_tt_bound_for_completed_node(beta - 1, beta),
            TtBound::Upper,
            "best = beta - 1 must be Upper"
        );
        assert_eq!(
            qsearch_tt_bound_for_completed_node(0, beta),
            TtBound::Upper,
            "best = 0 (stand-pat fail-low) must be Upper"
        );
        assert_eq!(
            qsearch_tt_bound_for_completed_node(-200, beta),
            TtBound::Upper,
            "negative best must be Upper"
        );
    }

    /// In debug builds, calling the helper with `best == -INF` panics due to
    /// the `debug_assert!` precondition. The precondition documents that
    /// `-INF` is structurally unreachable at every production call site
    /// (see helper doc for full audit). `#[should_panic]` fires only in
    /// debug builds; in release the assert is stripped and the function
    /// returns `Upper` (since `-INF < any_beta`). The project runs
    /// `cargo test --release` by default per docs/workflow.md, so gate
    /// the test with `cfg(debug_assertions)`.
    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn qsearch_tt_bound_panics_in_debug_when_best_is_neg_inf() {
        let _ = qsearch_tt_bound_for_completed_node(-INF, 0);
    }

    /// Boundary at `best = 0, beta = 0` → `Lower` (0 >= 0 is true).
    /// Complements the generic `lower_when_best_geq_beta` test with an
    /// all-zero input that pinpoints the inclusive `>=` without the positive
    /// beta context.
    #[test]
    fn qsearch_tt_bound_lower_at_boundary_zero() {
        assert_eq!(
            qsearch_tt_bound_for_completed_node(0, 0),
            TtBound::Lower,
            "best=0, beta=0 must be Lower (0 >= 0 is true)"
        );
    }

    /// The "would-be-Exact zone" (`original_alpha < best < beta`) must
    /// return `Upper` — never `Exact`. This pins the core no-Exact rule
    /// (Stockfish 45e5e65) that distinguishes M5.F's qsearch bound from
    /// negamax's three-way classification. The helper doesn't receive
    /// `original_alpha`, so it can only produce Lower/Upper; the test
    /// verifies that for representative triples in the zone, `Upper` is
    /// always returned.
    ///
    /// ~12 sample triples covering varied alpha/beta/best relationships:
    #[test]
    fn qsearch_tt_bound_non_exact_ever_in_would_be_exact_zone() {
        // Triples: (best, beta) where `some_alpha < best < beta`.
        // In a real search, original_alpha would be in (−∞, best); the helper
        // only sees (best, beta), so we verify it returns Upper for all
        // `best < beta` cases (the "would be Exact" zone is a subset of Upper).
        let cases: &[(i32, i32)] = &[
            (1, 100),       // small positive best, moderately positive beta
            (50, 100),      // midpoint
            (99, 100),      // just below boundary
            (100, 200),     // positive with large gap
            (-50, 0),       // negative best, zero beta
            (-100, -1),     // both negative, best < beta
            (0, 1),         // zero best, unit beta
            (499, 500),     // near boundary
            (1000, 2000),   // large values
            (-5000, -4999), // large negative values
            (MATE - MAX_PLY as i32 - 1, MATE - MAX_PLY as i32), // near mate boundary
            (100, MATE_IN_MAX_PLY), // just below MATE_IN_MAX_PLY
        ];

        for &(best, beta) in cases {
            let result = qsearch_tt_bound_for_completed_node(best, beta);
            assert_eq!(
                result,
                TtBound::Upper,
                "best={best} < beta={beta}: helper must return Upper (no Exact in qsearch); \
                 got {result:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M5.F §6.4 — Probe behavior tests (4 tests)
    //
    // Each test pre-populates the TT at the fixture position's Zobrist key,
    // drives `qsearch_for_test`, and asserts probe-related behavior.
    //
    // WITHOUT the production probe block (step 2.5), all four tests FAIL:
    //   - `qsearch_tt_probes_for_test()` stays 0.
    //   - The returned score is raw qsearch (not the TT-stub value).
    // -----------------------------------------------------------------------

    /// Pre-populate the TT with an Exact entry at the qsearch fixture's key.
    /// The probe must return the stored score immediately (cutoff fires).
    ///
    /// Fixture: KvKR quiet position (no captures available, white to move).
    /// `evaluate(pos)` ≈ +477 cp. We pre-populate the TT with an Exact score
    /// of 123 (distinct from eval). Without the probe block, qsearch ignores
    /// the TT and returns eval (not 123); the counter stays 0.
    #[test]
    fn qsearch_tt_probe_exact_returns_score() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        let tt = Arc::new(TranspositionTable::new(1));
        // Pre-populate at this position's Zobrist key with Exact score 123.
        let stored_score: i32 = 123;
        let ply: i32 = 1;
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stored_score, ply) as i16,
                depth: 0,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply as u32, &ctx);

        // Without probe block: probes=0, score=eval (not 123). With impl: probes=1, score=123.
        assert_eq!(
            ab.qsearch_tt_probes_for_test(),
            1,
            "qsearch must probe the TT at entry; got probes={}",
            ab.qsearch_tt_probes_for_test()
        );
        assert_eq!(
            score, stored_score,
            "Exact TT entry must short-circuit qsearch and return the stored score; \
             got {score}, expected {stored_score}"
        );
    }

    /// Pre-populate with a Lower entry whose stored score `>= beta`. The probe
    /// must return the Lower entry's score (beta cutoff fires).
    ///
    /// Fixture: same KvKR position. Alpha=0, beta=50. Pre-populate with
    /// Lower(score=200) → 200 >= 50, so cutoff fires and returns 200.
    /// Without probe block: qsearch ignores TT and returns eval (~477).
    #[test]
    fn qsearch_tt_probe_lower_returns_when_geq_beta() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        let tt = Arc::new(TranspositionTable::new(1));
        let stored_score: i32 = 200;
        let ply: u32 = 1;
        let beta = 50;
        // Stored score 200 >= beta=50 → Lower cutoff fires.
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stored_score, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), 0, beta, ply, &ctx);

        assert_eq!(
            ab.qsearch_tt_probes_for_test(),
            1,
            "qsearch must probe TT at entry; got probes={}",
            ab.qsearch_tt_probes_for_test()
        );
        assert_eq!(
            score, stored_score,
            "Lower TT entry with score >= beta must return stored score; \
             got {score}, expected {stored_score}"
        );
    }

    /// Pre-populate with an Upper entry whose stored score `<= alpha`. The
    /// probe must return the Upper entry's score (alpha cutoff fires).
    ///
    /// Fixture: KvKR. Alpha=300, beta=500. Pre-populate Upper(score=100) →
    /// 100 <= 300=alpha, so cutoff fires and returns 100.
    /// Without probe block: qsearch ignores TT and returns eval (~477).
    #[test]
    fn qsearch_tt_probe_upper_returns_when_leq_alpha() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        let tt = Arc::new(TranspositionTable::new(1));
        let stored_score: i32 = 100;
        let ply: u32 = 1;
        let alpha = 300;
        let beta = 500;
        // Stored score 100 <= alpha=300 → Upper cutoff fires.
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stored_score, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);

        assert_eq!(
            ab.qsearch_tt_probes_for_test(),
            1,
            "qsearch must probe TT at entry; got probes={}",
            ab.qsearch_tt_probes_for_test()
        );
        assert_eq!(
            score, stored_score,
            "Upper TT entry with score <= alpha must return stored score; \
             got {score}, expected {stored_score}"
        );
    }

    /// Pre-populate with a Lower entry whose score `< beta` — cutoff does NOT
    /// fire. The TT move is retained for ordering, but qsearch continues to
    /// completion. Assert `qsearch_tt_probes == 1` (probed) and the result
    /// reflects a real qsearch (not the stub value).
    ///
    /// Fixture: KvKR. Alpha=0, beta=10000. Pre-populate Lower(score=-100) →
    /// -100 < 10000, so cutoff does NOT fire. Qsearch runs to completion and
    /// returns eval (~477). Without probe block: probes=0 (not the stub).
    #[test]
    fn qsearch_tt_probe_no_cutoff_when_bound_does_not_fire_then_stores() {
        use crate::eval::evaluate;
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        let expected_eval = evaluate(&pos);

        let tt = Arc::new(TranspositionTable::new(1));
        let stored_score: i32 = -100; // well below beta=10000 → no cutoff
        let ply: u32 = 1;
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stored_score, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), 0, 10000, ply, &ctx);

        // Without probe block: probes=0. With impl: probes=1.
        assert_eq!(
            ab.qsearch_tt_probes_for_test(),
            1,
            "qsearch must probe TT even when no cutoff fires; got probes={}",
            ab.qsearch_tt_probes_for_test()
        );
        // Score should be from real qsearch (not the -100 stub), i.e., ~eval.
        assert_eq!(
            score, expected_eval,
            "non-firing probe must not affect the qsearch result; \
             expected eval={expected_eval}, got {score}"
        );
        // Probe-without-cutoff feeds into the normal store path: KvKR is path
        // A (stand_pat = +477 >> beta=10000? no — beta is HIGH here, so it's
        // path E fall-through). Either way the move-loop completes (no
        // captures available; ml.len() >= 2 from quiet rook + king moves),
        // and the store fires. Asserting `>= 1` keeps this robust to whether
        // path A or E classifies first.
        assert!(
            ab.qsearch_tt_stores_for_test() >= 1,
            "non-firing probe must still proceed to the normal store path; \
             stores={}",
            ab.qsearch_tt_stores_for_test()
        );
    }

    // -----------------------------------------------------------------------
    // M5.F §6.5 — Store behavior tests (6 tests, one per real-result path)
    //
    // Each test verifies that qsearch produces a TT entry at the correct
    // (score, bound, best_move) for its specific return path. WITHOUT the
    // production store block, ALL six tests fail: `qsearch_tt_stores_for_test()`
    // stays 0 and `tt.probe(key)` returns None.
    // -----------------------------------------------------------------------

    /// Path A: stand-pat fail-high. Fixture has no captures (filter empty);
    /// static eval is clearly positive and exceeds beta. The stand-pat cutoff
    /// fires at step 4, and qsearch must store a Lower entry with score=stand_pat,
    /// best_move=0.
    ///
    /// Fixture: KvKR (white K+R vs black K). Eval ≈ +477 cp (well above any
    /// plausible low beta). Window: alpha=0, beta=10 (stand_pat >> beta).
    #[test]
    fn qsearch_tt_store_stand_pat_fail_high_lower() {
        use crate::eval::evaluate;
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        let expected_eval = evaluate(&pos);
        let beta = 10;
        assert!(
            expected_eval >= beta,
            "fixture invariant: eval must be >= beta so stand-pat fail-high fires; \
             eval={expected_eval}, beta={beta}"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;

        let score = ab.qsearch_for_test(&mut pos.clone(), 0, beta, ply, &ctx);
        assert_eq!(
            score, expected_eval,
            "qsearch must return stand_pat on fail-high; expected {expected_eval}, got {score}"
        );

        // Without store block: stores=0 and probe returns None.
        assert_eq!(
            ab.qsearch_tt_stores_for_test(),
            1,
            "path A (stand-pat fail-high) must produce exactly one TT store; got {}",
            ab.qsearch_tt_stores_for_test()
        );
        let entry = tt
            .probe(pos.zobrist())
            .expect("stand-pat fail-high must create TT entry");
        assert_eq!(entry.depth, 0, "qsearch stores at depth=0");
        assert_eq!(
            entry.bound(),
            TtBound::Lower,
            "stand-pat fail-high is a lower bound"
        );
        assert_eq!(
            score_from_tt(entry.score as i32, ply as i32),
            expected_eval,
            "stored score must round-trip to stand_pat"
        );
        assert_eq!(entry.best_move, 0, "stand-pat has no best_move");
    }

    /// Path B: true stalemate. The position is a stalemate for the side to move.
    /// qsearch must return 0 and store an Exact entry with score=0, best_move=0.
    ///
    /// Fixture: `7k/5Q2/6K1/8/8/8/8/8 b - - 0 1` — black king h8 is stalemated
    /// (white Q covers all escape squares; king not in check). M5.E true-stalemate
    /// fixture carried forward.
    #[test]
    fn qsearch_tt_store_true_stalemate_exact_zero() {
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos = Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1")
            .expect("stalemate fixture FEN must parse");

        // Fixture validation: true stalemate (not in check, no legal moves).
        use crate::movegen::{MoveList, generate_moves, in_check};
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.len(),
            0,
            "fixture invariant: zero legal moves (true stalemate)"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;

        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        assert_eq!(score, 0, "stalemate must return 0");

        // Without store block: stores=0 and probe returns None.
        assert_eq!(
            ab.qsearch_tt_stores_for_test(),
            1,
            "path B (true stalemate) must produce exactly one TT store; got {}",
            ab.qsearch_tt_stores_for_test()
        );
        let entry = tt
            .probe(pos.zobrist())
            .expect("stalemate must create TT entry");
        assert_eq!(entry.depth, 0);
        assert_eq!(
            entry.bound(),
            TtBound::Exact,
            "true stalemate is FIDE-definite → Exact"
        );
        assert_eq!(
            score_from_tt(entry.score as i32, ply as i32),
            0,
            "stored stalemate score must round-trip to 0"
        );
        assert_eq!(entry.best_move, 0);
    }

    /// Path D: mate at horizon. In-check position with no legal evasions.
    /// qsearch must return `-(MATE - ply)` and store an Exact entry.
    ///
    /// Fixture: White king e1 in checkmate (back-rank mate). Must be a true
    /// checkmate position: in_check=true AND no legal moves.
    /// FEN: `4k3/8/8/8/8/8/3r4/3rK3 w - - 0 1` — White king on e1,
    /// black rooks on d2 and d1 delivering back-rank checkmate.
    #[test]
    fn qsearch_tt_store_mate_at_horizon_exact_neg_mate() {
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos = Position::from_fen("4k3/8/8/8/8/8/3r4/3rK3 w - - 0 1")
            .expect("checkmate fixture FEN must parse");

        use crate::movegen::{MoveList, generate_moves, in_check};
        assert!(in_check(&pos), "fixture invariant: white must be in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.len(),
            0,
            "fixture invariant: checkmate — zero legal moves"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 2;

        let expected_score = -(MATE - ply as i32);
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        assert_eq!(
            score, expected_score,
            "checkmate at qsearch horizon must return -(MATE - ply); \
             got {score}, expected {expected_score}"
        );

        // Without store block: stores=0.
        assert_eq!(
            ab.qsearch_tt_stores_for_test(),
            1,
            "path D (mate at horizon) must produce exactly one TT store; got {}",
            ab.qsearch_tt_stores_for_test()
        );
        let entry = tt
            .probe(pos.zobrist())
            .expect("mate at horizon must create TT entry");
        assert_eq!(entry.depth, 0);
        assert_eq!(
            entry.bound(),
            TtBound::Exact,
            "mate at horizon is FIDE-definite → Exact"
        );
        // Mate score is ply-adjusted via score_to_tt/score_from_tt.
        assert_eq!(
            score_from_tt(entry.score as i32, ply as i32),
            expected_score,
            "stored mate score must round-trip"
        );
    }

    /// Path F: completed move loop with a beta cutoff — Lower bound.
    /// Exactly one capture causes the cutoff. The TT entry must record
    /// Lower bound and the cutoff move's bits as best_move.
    ///
    /// Fixture: `4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1` — White Q d1 can capture
    /// black B d4 (Qxd4). Narrow window alpha=0, beta=10. The queen captures
    /// the bishop for ~365 cp, well above beta=10 — the single capture causes
    /// a cutoff and the loop terminates with Lower.
    #[test]
    fn qsearch_tt_store_completed_loop_lower() {
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging bishop FEN must parse");

        // Fixture validation: white queen can capture black bishop.
        let qxd4 = Move::from_uci("d1d4", &pos).expect("Qxd4 must be legal");
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert_eq!(
            captures.len(),
            1,
            "fixture must have exactly one capture; got {captures:?}"
        );
        assert_eq!(captures[0], qxd4, "the single capture must be Qxd4");

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;
        let alpha = 0;
        // beta must be > stand_pat (≈620 for K+Q vs K+B) so path A (stand-pat
        // fail-high) does NOT fire, and < the post-capture score (≈1025 for
        // K+Q vs K) so path F (move-loop cutoff) DOES fire. 700 sits in that
        // window; stand_pat ≈620 < 700 and Qxd4 yields ≈1025 > 700.
        let beta = 700;

        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);
        assert!(
            score >= beta,
            "fixture must produce a beta cutoff; got score={score}, beta={beta}"
        );

        // Without store block: stores=0. With impl: at least 1 (current frame),
        // possibly more from recursive children's path-A/E stores at the
        // post-capture child position. The discriminating assertion is the
        // probe-based check below; the counter is a coarse witness that the
        // store path was reached.
        assert!(
            ab.qsearch_tt_stores_for_test() >= 1,
            "path F (completed loop, cutoff) must produce at least one TT store; got {}",
            ab.qsearch_tt_stores_for_test()
        );
        let entry = tt
            .probe(pos.zobrist())
            .expect("completed-loop cutoff must create TT entry");
        assert_eq!(entry.depth, 0);
        assert_eq!(
            entry.bound(),
            TtBound::Lower,
            "beta cutoff in move loop → Lower bound"
        );
        // Lower bound must carry the cutoff move's bits.
        assert_eq!(
            entry.best_move,
            qxd4.bits(),
            "Lower bound entry must record the cutoff move (Qxd4={}); got {}",
            qxd4.bits(),
            entry.best_move
        );
        assert!(
            score_from_tt(entry.score as i32, ply as i32) >= beta,
            "stored score must be >= beta after round-trip"
        );
    }

    /// Path F: completed move loop with no cutoff — Upper bound.
    /// Multiple captures available, none reach beta. The TT entry must record
    /// Upper bound and best_move=0 (no cutoff move to store).
    ///
    /// Fixture: Both pieces trade for equal material — `4k3/8/8/3p4/4P3/8/8/4K3 w - - 0 1`.
    /// White exd5 wins a pawn, but black response also recaptures: in qsearch, after exd5,
    /// black's response is also a pawn recapture (if legal) giving ~0 net. We need a position
    /// where the captures don't score above beta.
    ///
    /// Simpler fixture: KvKP (white king + pawn vs black king pawn position where
    /// no captures are available from white's side, so only stand-pat runs).
    ///
    /// Actually, we need CAPTURES to be available but NONE reaching beta. Use:
    /// `4k3/8/8/8/8/8/4p3/4K3 w - - 0 1` — White king can capture black pawn on e2.
    /// Window: alpha=-1000, beta=1000. After Kxe2, qsearch recurses; the captured
    /// pawn gives ~+82 cp < beta=1000. No cutoff → Upper.
    #[test]
    fn qsearch_tt_store_completed_loop_upper() {
        use crate::eval::evaluate;
        use crate::tt::{TranspositionTable, score_from_tt};
        // White king e1 can capture black pawn e2. beta=1000 is above any
        // realistic capture gain (~82 cp for a pawn), so no cutoff fires.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/4p3/4K3 w - - 0 1").expect("KxP FEN must parse");

        use crate::movegen::{MoveList, generate_moves, in_check};
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert!(
            !captures.is_empty(),
            "fixture must have at least one capture; got none"
        );

        let stand_pat = evaluate(&pos);
        let alpha = -INF;
        let beta = 1000;
        assert!(
            stand_pat < beta,
            "fixture invariant: stand_pat={stand_pat} must be < beta={beta} so move loop runs"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;

        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);
        assert!(
            score < beta,
            "fixture must NOT produce a beta cutoff (all captures score < beta); got score={score}"
        );

        // Without store block: stores=0. With impl: at least 1 (current frame),
        // possibly more from recursive children's stores at post-capture
        // positions. Probe-based check below is the discriminator.
        assert!(
            ab.qsearch_tt_stores_for_test() >= 1,
            "path F (completed loop, no cutoff) must produce at least one TT store; got {}",
            ab.qsearch_tt_stores_for_test()
        );
        let entry = tt
            .probe(pos.zobrist())
            .expect("completed-loop upper must create TT entry");
        assert_eq!(entry.depth, 0);
        assert_eq!(
            entry.bound(),
            TtBound::Upper,
            "no-cutoff completed loop → Upper bound"
        );
        assert_eq!(
            entry.best_move, 0,
            "Upper bound entry must have best_move=0 (no cutoff move)"
        );
        let recovered = score_from_tt(entry.score as i32, ply as i32);
        assert_eq!(
            recovered, score,
            "stored score must round-trip exactly to the returned score; got {recovered}, expected {score}"
        );
        assert!(
            recovered < beta,
            "stored score must remain < beta after round-trip (Upper bound semantic)"
        );
    }

    /// MAX_PLY ceiling guard (path X) — no TT store.
    /// At `ply == MAX_PLY - 1` with `!in_check`, qsearch returns `evaluate(pos)`
    /// immediately without recursing or storing. The store counter must stay 0,
    /// and probing the TT after the call must return None.
    ///
    /// Fixture: KvKR (white K+R vs black K) — `evaluate(pos) != 0` (solidly
    /// positive ~+477 cp). The non-zero eval protects against a "vacuous pass"
    /// where a zero score might alias with an empty TT slot. With the new M5.F
    /// discriminator (`key == 0`), this is not a real concern, but the plan
    /// §6.5 requires it explicitly.
    #[test]
    fn qsearch_tt_store_skipped_at_max_ply_guard() {
        use crate::eval::evaluate;
        use crate::tt::TranspositionTable;
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        let expected_eval = evaluate(&pos);
        assert!(
            expected_eval != 0,
            "fixture invariant: eval must be non-zero to distinguish from empty-slot aliasing"
        );

        use crate::movegen::in_check;
        assert!(
            !in_check(&pos),
            "fixture invariant: not in check (ceiling guard only fires !in_check)"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        // Drive at ply = MAX_PLY - 1 (ceiling).
        let ply: u32 = MAX_PLY as u32 - 1;
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);

        assert_eq!(
            score, expected_eval,
            "ceiling guard must return evaluate(pos); got {score}, expected {expected_eval}"
        );
        // Key assertion: no store.
        assert_eq!(
            ab.qsearch_tt_stores_for_test(),
            0,
            "MAX_PLY ceiling guard must NOT store in TT; stores={}",
            ab.qsearch_tt_stores_for_test()
        );
        // Strongest assertion: TT entry does not exist at this key.
        assert!(
            tt.probe(pos.zobrist()).is_none(),
            "MAX_PLY ceiling guard path must produce no TT entry at this key"
        );
    }

    /// Path C (M5.E #1 single-reply extension): `!in_chk && moves_vec.is_empty()
    /// && ml.len() == 1`. Recurse on the unique forced quiet; M5.F stores the
    /// recursed-and-negated score with `best_move = only_mv.bits()` and bound
    /// classified by `qsearch_tt_bound_for_completed_node(score, beta)`. Plan
    /// §4.2 row C; addresses test-suite-review pass-1 must-fix gap.
    ///
    /// Fixture from M5.E `qsearch_single_reply_fires_when_filter_empty_and_one_legal_quiet`:
    /// `k7/8/1Q6/p7/8/8/7P/K7 b - - 0 1` — black to move, exactly one legal
    /// move (pa5→a4), not in check, no captures available.
    #[test]
    fn qsearch_tt_store_single_reply_extension_uses_helper_bound() {
        use crate::movegen::{MoveList, generate_moves, in_check};
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos = Position::from_fen("k7/8/1Q6/p7/8/8/7P/K7 b - - 0 1")
            .expect("single-reply fixture FEN must parse");

        // Fixture invariants pinning path C entry conditions.
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(ml.len(), 1, "fixture invariant: exactly one legal move");
        let only_mv = ml
            .iter()
            .next()
            .expect("ml.len() == 1 → at least one move in iter");

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 3;
        let alpha = -INF;
        let beta = INF;
        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);

        // Single-reply must have fired (M5.E counter).
        assert!(
            ab.qsearch_single_reply_firings_for_test() >= 1,
            "M5.E single-reply must fire on this fixture"
        );

        // M5.F: store happened. The current frame's store and the recursive
        // child's store BOTH increment qsearch_tt_stores; assert >= 1 (current
        // frame at minimum). Strongest discriminator: the entry exists at this
        // position's Zobrist with the expected (bound, best_move).
        assert!(
            ab.qsearch_tt_stores_for_test() >= 1,
            "M5.F path C must store the single-reply result; stores={}",
            ab.qsearch_tt_stores_for_test()
        );

        let entry = tt
            .probe(pos.zobrist())
            .expect("path C must produce a TT entry at this position's key");
        assert_eq!(
            entry.depth, 0,
            "qsearch entries are at depth=0 (Option A discriminator)"
        );
        // Bound: classified by qsearch_tt_bound_for_completed_node(score, beta).
        // With beta = INF, score (centipawn-bounded) is always < INF, so bound = Upper.
        // Verify against the helper directly to keep the test robust to score
        // changes from eval / pst tunings.
        let expected_bound = qsearch_tt_bound_for_completed_node(score, beta);
        assert_eq!(
            entry.bound(),
            expected_bound,
            "path C bound must match qsearch_tt_bound_for_completed_node(score, beta)"
        );
        // best_move: the unique forced quiet's bits.
        assert_eq!(
            entry.best_move,
            only_mv.bits(),
            "path C must store best_move = only_mv.bits() ({:#x})",
            only_mv.bits()
        );
        // score round-trips via score_from_tt.
        assert_eq!(
            score_from_tt(entry.score as i32, ply as i32),
            score,
            "path C stored score must round-trip to the returned score"
        );
    }

    /// Path E (false-stalemate guard fall-through): `!in_chk &&
    /// moves_vec.is_empty() && ml.len() >= 2`. The qsearch filter rejected
    /// every legal move (no captures / queen-promos), AND there are 2+ legal
    /// quiet moves. M3.D's pre-M5.E behavior: returns `stand_pat` (best_init).
    /// M5.F preserves the score; adds a TT store with `bound=Upper`,
    /// `score=stand_pat`, `best_move=0`. Plan §4.2 row E; addresses
    /// test-suite-review pass-1 must-fix gap.
    ///
    /// Fixture: KvKR (`4k3/8/8/8/8/8/8/2R1K3 w - - 0 1`). White has no
    /// captures (no black piece on any reachable square), 2+ legal quiet
    /// moves (rook slides, king moves). Pre-M5.F discriminator: `beta`
    /// must be > `stand_pat` so path A (stand-pat fail-high) doesn't fire.
    #[test]
    fn qsearch_tt_store_false_stalemate_guard_upper_stand_pat() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        // Fixture invariants for path E.
        assert!(!in_check(&pos), "fixture invariant: not in check");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.len() >= 2,
            "fixture invariant: 2+ legal moves to fall through to path E; got {}",
            ml.len()
        );
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert!(
            captures.is_empty(),
            "fixture invariant: no captures (qsearch filter empties moves_vec); got {captures:?}"
        );
        let stand_pat = evaluate(&pos);
        assert!(
            stand_pat > 200,
            "fixture invariant: stand_pat decisively positive (KvKR ≈ +477); got {stand_pat}"
        );

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        // beta > stand_pat so path A (sp >= beta) does NOT fire; alpha < stand_pat
        // so the move loop runs (filter empties → path E fall-through).
        let ply: u32 = 1;
        let alpha = -INF;
        let beta = stand_pat + 1000;
        let score = ab.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx);

        assert_eq!(
            score, stand_pat,
            "path E returns stand_pat = evaluate(pos); got {score}, expected {stand_pat}"
        );

        let entry = tt
            .probe(pos.zobrist())
            .expect("path E must produce a TT entry at this position's key");
        assert_eq!(entry.depth, 0, "qsearch entries are at depth=0");
        assert_eq!(
            entry.bound(),
            crate::tt::TtBound::Upper,
            "path E stored bound must be Upper (stand_pat fall-through, no move improved)"
        );
        assert_eq!(
            entry.best_move, 0,
            "path E carries no best_move (move loop didn't run; stand_pat is the score)"
        );
        assert_eq!(
            score_from_tt(entry.score as i32, ply as i32),
            stand_pat,
            "path E stored score must round-trip to stand_pat"
        );
    }

    // -----------------------------------------------------------------------
    // M5.F §6.6 — TT-move ordering tests (3 tests)
    //
    // Verify that the TT move from a probe is used for move ordering in
    // the qsearch move loop. WITHOUT the probe + ordering block, all three
    // tests fail because the move loop sees the default MVV-LVA order.
    // -----------------------------------------------------------------------

    /// A capture stored as the TT best_move is promoted to index 0 in the
    /// qsearch move loop. Drives a two-run node-count differential: run (a)
    /// without a TT pre-population, run (b) with a TT entry whose best_move
    /// is the ordering-relevant capture. Different node counts confirm ordering
    /// changed.
    ///
    /// Fixture: `4k3/8/8/8/3b1q2/8/3Q4/4K3 w - - 0 1` — white Q d2 can
    /// capture black B d4 (Qxd4) or black Q f4 (Qxf4). MVV-LVA: QxQ > QxB.
    /// Pre-populate TT with best_move = Qxd4 (lower MVV-LVA score). If ordering
    /// works, run (b) searches Qxd4 first and may cut earlier/later, changing
    /// node count.
    #[test]
    fn qsearch_tt_move_promoted_to_index_0_when_capture() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos = Position::from_fen("4k3/8/8/8/3b1q2/8/3Q4/4K3 w - - 0 1")
            .expect("two-capture FEN must parse");

        // Verify both captures are available.
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let qxd4 = Move::from_uci("d2d4", &pos).expect("Qxd4 must be legal");
        let qxf4 = Move::from_uci("d2f4", &pos).expect("Qxf4 must be legal");
        let captures: Vec<_> = ml.iter().filter(|m| m.is_capture()).collect();
        assert!(
            captures.contains(&qxd4),
            "fixture must have Qxd4 capture; got {captures:?}"
        );
        assert!(
            captures.contains(&qxf4),
            "fixture must have Qxf4 capture; got {captures:?}"
        );

        let ply: u32 = 1;

        // Run (a): no TT.
        let mut ab_a = AlphaBetaMover::new();
        ab_a.history = vec![pos.zobrist()];
        let (ctx_a, _stop_a) = non_aborting_ctx();
        let _ = ab_a.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx_a);
        let nodes_a = ab_a.nodes;

        // Run (b): pre-populate TT with the LOWER-ranked capture (Qxd4 by MVV-LVA,
        // since QxB < QxQ) as the ordering hint. If ordering works, Qxd4 moves to
        // index 0, changing the search order and potentially the node count.
        let tt_b = Arc::new(TranspositionTable::new(1));
        tt_b.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(100, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: qxd4.bits(),
            },
        );

        let mut ab_b = AlphaBetaMover::new();
        ab_b.history = vec![pos.zobrist()];
        ab_b.set_tt_for_test(Some(Arc::clone(&tt_b)));
        let (ctx_b, _stop_b) = non_aborting_ctx();
        let _ = ab_b.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx_b);
        let nodes_b = ab_b.nodes;

        // Without ordering impl, nodes_a == nodes_b (same search order). With
        // impl, the order changes and nodes differ. The probe also registers.
        assert_eq!(
            ab_b.qsearch_tt_probes_for_test(),
            1,
            "run (b) must probe the TT; got probes={}",
            ab_b.qsearch_tt_probes_for_test()
        );
        assert_ne!(
            nodes_a, nodes_b,
            "TT-move ordering must change the search shape; both runs produced {nodes_a} nodes. \
             This test will pass only after the ordering block is implemented."
        );
    }

    /// A quiet TT move at a not-in-check position must be silently skipped by
    /// the move-loop membership scan (the quiet is not in `moves_vec` because
    /// the qsearch filter admits only captures + queen-promos). The ordering
    /// must be identical to the no-TT baseline.
    ///
    /// Fixture: `4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1` — only capture is Qxd4.
    /// Pre-populate TT with best_move = e1f1 (quiet king move; d1 is
    /// occupied by the queen so e1d1 is illegal). This quiet is
    /// not in qsearch's `moves_vec` (filter rejects it) → swap skipped → same
    /// ordering as baseline (Qxd4 first by MVV-LVA).
    #[test]
    fn qsearch_tt_move_skipped_when_quiet_at_not_in_check() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos = Position::from_fen("4k3/8/8/8/3b4/8/8/3QK3 w - - 0 1")
            .expect("hanging bishop FEN must parse");

        let ply: u32 = 1;
        // e1f1: king steps to f1 (not attacked by the d4 bishop); d1 is
        // occupied by the queen so e1d1 is illegal.
        let quiet_king_move = Move::from_uci("e1f1", &pos).expect("Ke1f1 must be legal quiet");
        assert!(
            is_quiet(quiet_king_move),
            "fixture invariant: e1f1 must be quiet"
        );

        // Run (a): no TT (baseline node count).
        let mut ab_a = AlphaBetaMover::new();
        ab_a.history = vec![pos.zobrist()];
        let (ctx_a, _stop_a) = non_aborting_ctx();
        let score_a = ab_a.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx_a);
        let nodes_a = ab_a.nodes;

        // Run (b): TT pre-populated with quiet best_move. Quiet is not in
        // moves_vec (filter rejects it). Ordering unchanged from baseline.
        let tt_b = Arc::new(TranspositionTable::new(1));
        tt_b.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(50, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: quiet_king_move.bits(),
            },
        );

        let mut ab_b = AlphaBetaMover::new();
        ab_b.history = vec![pos.zobrist()];
        ab_b.set_tt_for_test(Some(Arc::clone(&tt_b)));
        let (ctx_b, _stop_b) = non_aborting_ctx();
        let score_b = ab_b.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx_b);
        let nodes_b = ab_b.nodes;

        // When the cutoff does NOT fire at probe time (Lower score=50 < INF=beta),
        // the probe registers but the quiet tt_move has no effect on ordering.
        // Score must be the same as baseline (not the TT stub).
        assert_eq!(
            score_a, score_b,
            "quiet TT move must not affect qsearch result; baseline={score_a}, with-TT={score_b}"
        );
        assert_eq!(
            nodes_a, nodes_b,
            "quiet TT move must not change node count (membership scan rejects it); \
             baseline={nodes_a}, with-TT={nodes_b}"
        );
    }

    /// In-check arm: the TT move is any legal evasion, so the membership scan
    /// can promote it to index 0. Drives a two-run differential similar to
    /// §6.6 capture test.
    ///
    /// Fixture: `4k3/8/8/8/8/8/4r3/4K3 w - - 0 1` — White Ke1 in check from
    /// black Re2. Legal evasions: Kxe2, Kd1, Kf1 (and possibly Kd2). Pre-populate
    /// TT with best_move = Kd1 (a quiet evasion). If ordering is wired, the
    /// in-check arm promotes Kd1 to index 0.
    #[test]
    fn qsearch_tt_move_used_when_in_check() {
        use crate::movegen::in_check;
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1").expect("in-check FEN must parse");
        assert!(in_check(&pos), "fixture invariant: white must be in check");

        let ply: u32 = 1;
        // Choose a quiet evasion as the TT move hint.
        let kd1 = Move::from_uci("e1d1", &pos).expect("Kd1 must be legal evasion");

        // Window: alpha=-50, beta=-30. With these bounds:
        //   - Kxe2 (capture rook) yields score 0 to parent → 0 > alpha (-50) →
        //     alpha=0 ≥ beta (-30) → cutoff fires on the FIRST capture.
        //   - Kd1 (quiet, loses the rook) yields ≈ -477 to parent → below alpha.
        // Natural MVV-LVA order (Kxe2 first): 1 child explored → 2 total nodes.
        // TT-hinted order (Kd1 first): Kd1 then Kxe2 → 3 total nodes.
        // The node-count difference is the observable signal of ordering.
        let alpha = -50_i32;
        let beta = -30_i32;

        // The pre-populated TT entry must NOT fire a probe-time cutoff:
        // Upper bound only cuts if tt_score <= alpha, and Lower only if
        // tt_score >= beta. Use Lower with score between alpha and beta
        // (-40) so the probe doesn't short-circuit, leaving tt_move for ordering.
        let stub_score = -40_i32; // alpha < -40 < beta: no cutoff for Lower

        // Run (a): no TT.
        let mut ab_a = AlphaBetaMover::new();
        ab_a.history = vec![pos.zobrist()];
        let (ctx_a, _stop_a) = non_aborting_ctx();
        let _ = ab_a.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx_a);
        let nodes_a = ab_a.nodes;

        // Run (b): pre-populate TT with Kd1 as best_move; Lower, score=-40
        // (no probe-time cutoff; Kd1 is promoted to index 0 by ordering).
        let tt_b = Arc::new(TranspositionTable::new(1));
        tt_b.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stub_score, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: kd1.bits(),
            },
        );
        let mut ab_b = AlphaBetaMover::new();
        ab_b.history = vec![pos.zobrist()];
        ab_b.set_tt_for_test(Some(Arc::clone(&tt_b)));
        let (ctx_b, _stop_b) = non_aborting_ctx();
        let _ = ab_b.qsearch_for_test(&mut pos.clone(), alpha, beta, ply, &ctx_b);
        let nodes_b = ab_b.nodes;

        // The probe registers.
        assert_eq!(
            ab_b.qsearch_tt_probes_for_test(),
            1,
            "in-check: must probe TT; got probes={}",
            ab_b.qsearch_tt_probes_for_test()
        );
        // Node count changes when ordering changes (Kd1 first in run_b wastes
        // one node before Kxe2 cuts; run_a cuts on the first node Kxe2).
        assert_ne!(
            nodes_a, nodes_b,
            "in-check TT-move ordering must change search shape; \
             nodes_a={nodes_a}, nodes_b={nodes_b}. \
             This test will pass only after the ordering block is implemented."
        );
    }

    // -----------------------------------------------------------------------
    // M5.F §6.7 — Negamax-qsearch interaction tests (3 tests)
    // -----------------------------------------------------------------------

    /// A qsearch entry (depth=0) pre-populated in the TT must NOT cut a negamax
    /// search at depth >= 1. The existing negamax probe rule
    /// `entry.depth as u32 >= depth` with depth >= 1 naturally rejects depth=0
    /// entries. BUT the qsearch entry's best_move CAN still be used for ordering.
    ///
    /// Fixture: startpos, depth=2. Pre-populate TT with a depth=0 entry at
    /// startpos's Zobrist key. Assert:
    ///   1. Negamax does NOT cut (runs to completion, producing a real score).
    ///   2. The score differs from the stub's score (proves no cutoff occurred).
    #[test]
    fn negamax_probe_skips_qsearch_entry_at_depth_geq_1() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos = Position::starting_position();

        // Pre-populate TT at root's key with depth=0, Exact, score=9999
        // (an absurd score that negamax should NOT return if it correctly
        // ignores depth=0 entries during its depth-gated probe).
        let tt = Arc::new(TranspositionTable::new(16));
        let stub_score: i32 = 9999;
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stub_score, 0) as i16,
                depth: 0,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx_at_depth_with_tt(2, Arc::clone(&tt));

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, true, true, None, &ctx);

        // Negamax at depth=2 must NOT use the depth=0 entry for cutoff.
        // At startpos depth=2, the score should be a centipawn score, not 9999.
        assert_ne!(
            score, stub_score,
            "negamax at depth=2 must ignore depth=0 qsearch entry; \
             expected real score (not {stub_score}), got {score}"
        );
        assert!(
            score.abs() < MATE_IN_MAX_PLY,
            "negamax at depth=2 from startpos must return a normal cp score; got {score}"
        );
    }

    /// A negamax entry (depth >= 1) at the qsearch fixture's key must be used
    /// for a qsearch cutoff. The probe applies `Exact / Lower≥β / Upper≤α`
    /// cutoffs regardless of the stored depth (qsearch does not depth-gate).
    ///
    /// Fixture: KvKR. Pre-populate with depth=5, Exact, score=300.
    /// Qsearch (ply=1, alpha=-INF, beta=INF) must probe and short-circuit
    /// with score=300. Without probe block, score=eval.
    #[test]
    fn qsearch_uses_negamax_entry_for_cutoff_when_bound_fires() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");

        let tt = Arc::new(TranspositionTable::new(1));
        let stored_score: i32 = 300;
        let ply: u32 = 1;
        // Store as depth=5, Exact — a deeper negamax-tier entry.
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(stored_score, ply as i32) as i16,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);

        // Without probe block: probes=0, score=eval (not 300).
        assert_eq!(
            ab.qsearch_tt_probes_for_test(),
            1,
            "qsearch must probe the negamax-tier TT entry; got probes={}",
            ab.qsearch_tt_probes_for_test()
        );
        assert_eq!(
            score, stored_score,
            "negamax-tier Exact entry must short-circuit qsearch; \
             expected {stored_score}, got {score}"
        );
    }

    /// Negamax extracts a qsearch entry's best_move for ordering, even though
    /// the depth-gated score cutoff doesn't fire (depth=0 < depth=2).
    ///
    /// Fixture: `4k3/8/8/8/3b1q2/8/3Q4/4K3 w - - 0 1` — two captures.
    /// Pre-populate TT at root key with depth=0, best_move=Qxd4 (lower MVV-LVA).
    /// Negamax at depth=2 does NOT cut on depth=0. But if ordering extraction
    /// works, Qxd4 moves to index 0, changing the node count vs. baseline.
    #[test]
    fn negamax_extracts_tt_move_from_qsearch_entry() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos = Position::from_fen("4k3/8/8/8/3b1q2/8/3Q4/4K3 w - - 0 1")
            .expect("two-capture FEN must parse");

        let qxd4 = Move::from_uci("d2d4", &pos).expect("Qxd4 must be legal");

        // Run (a): no TT.
        let mut ab_a = AlphaBetaMover::new();
        ab_a.history = vec![pos.zobrist()];
        let (ctx_a, _stop_a) = non_aborting_ctx_at_depth(2);
        let _ = ab_a.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, true, true, None, &ctx_a);
        let nodes_a = ab_a.nodes;

        // Run (b): TT has depth=0, best_move=Qxd4. Negamax probe at depth=2
        // extracts tt_move=Qxd4 for ordering but does NOT cut (depth=0 < depth=2).
        let tt_b = Arc::new(TranspositionTable::new(16));
        tt_b.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(100, 0) as i16,
                depth: 0,
                bound: TtBound::Lower,
                best_move: qxd4.bits(),
            },
        );
        let mut ab_b = AlphaBetaMover::new();
        ab_b.history = vec![pos.zobrist()];
        ab_b.set_tt_for_test(Some(Arc::clone(&tt_b)));
        let (ctx_b, _stop_b) = non_aborting_ctx_at_depth_with_tt(2, Arc::clone(&tt_b));
        let _ = ab_b.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, true, true, None, &ctx_b);
        let nodes_b = ab_b.nodes;

        // Ordering changes via extracted tt_move → node count changes.
        // Without the extraction, nodes_a == nodes_b.
        assert_ne!(
            nodes_a, nodes_b,
            "negamax must use depth=0 entry's best_move for ordering; \
             node counts must differ (baseline={nodes_a}). \
             This test will pass only after TT-move extraction from qsearch entry is wired."
        );
    }

    // -----------------------------------------------------------------------
    // M5.F §6.8 — Mate-score round-trip tests (2 tests)
    //
    // Verify that mate scores stored by qsearch are ply-adjusted correctly
    // via `score_to_tt` / `score_from_tt`, matching the ADR-0018 §3 discipline.
    // WITHOUT the store block, both tests fail: no entry is produced.
    // -----------------------------------------------------------------------

    /// Store a mate-at-horizon score from qsearch at ply=5, probe at ply=10.
    /// The `score_from_tt` at a different ply must adjust to the correct
    /// distance from the probing frame.
    ///
    /// Fixture: `4k3/8/8/8/8/8/3r4/3rK3 w - - 0 1` — White king in checkmate.
    /// qsearch at ply=5 returns `-(MATE - 5)`. Stored value = `score_to_tt(-(MATE-5), 5)`.
    /// At ply=10: `score_from_tt(stored, 10)` = `stored + 10` = `-(MATE - 5 - 5 + 10) = -(MATE - 5)`.
    /// Actually: `score_to_tt(-(MATE-5), 5) = -(MATE-5) - 5 = -(MATE)` (negative mate gets ply subtracted).
    /// Then `score_from_tt(-(MATE), 10) = -(MATE) + 10 = -(MATE - 10)`.
    /// This represents "mated in 10 plies from the probing frame" — the position's absolute
    /// mate distance is preserved correctly. ✓
    #[test]
    fn qsearch_tt_store_mate_score_ply_adjusted_correctly() {
        use crate::tt::{TranspositionTable, score_from_tt, score_to_tt};
        let pos = Position::from_fen("4k3/8/8/8/8/8/3r4/3rK3 w - - 0 1")
            .expect("checkmate fixture FEN must parse");

        let store_ply: u32 = 5;
        let probe_ply: i32 = 10;

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        // Drive qsearch at store_ply=5. Returns -(MATE - 5) for checkmate.
        let expected_at_store = -(MATE - store_ply as i32);
        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, store_ply, &ctx);
        assert_eq!(
            score, expected_at_store,
            "qsearch at ply=5 in checkmate must return -(MATE-5); got {score}"
        );

        // Without store block: no entry. With impl: entry at depth=0.
        let entry = tt
            .probe(pos.zobrist())
            .expect("checkmate must produce TT entry");

        // Probe at a different ply: the ply-adjusted score must reflect the
        // correct distance from probe_ply.
        let probed_score = score_from_tt(entry.score as i32, probe_ply);
        let expected_at_probe_ply = -(MATE - probe_ply);
        assert_eq!(
            probed_score,
            expected_at_probe_ply,
            "mate score probed at ply=10 must adjust to -(MATE-10); \
             got {probed_score}, expected {expected_at_probe_ply}. \
             Stored raw={}, after score_to_tt({}, {})={}",
            entry.score,
            expected_at_store,
            store_ply,
            score_to_tt(expected_at_store, store_ply as i32)
        );
    }

    /// A normal (centipawn) qsearch score must NOT be ply-adjusted when stored
    /// and retrieved. Only mate scores (`|score| > MATE_IN_MAX_PLY`) are
    /// ply-adjusted; centipawn scores pass through unchanged.
    ///
    /// Fixture: KvKR (eval ≈ +477 cp). Drive at ply=3. Store and probe at
    /// different plies. The round-tripped centipawn score must be unchanged.
    #[test]
    fn qsearch_tt_store_normal_score_not_ply_adjusted() {
        use crate::eval::evaluate;
        use crate::tt::{TranspositionTable, score_from_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        let expected_eval = evaluate(&pos);
        assert!(
            expected_eval.abs() < MATE_IN_MAX_PLY,
            "fixture invariant: eval must be a centipawn score (not mate); got {expected_eval}"
        );

        let store_ply: u32 = 3;

        let tt = Arc::new(TranspositionTable::new(1));
        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        let score = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, store_ply, &ctx);
        assert_eq!(
            score, expected_eval,
            "qsearch at quiet position must return eval"
        );

        // Without store block: no entry.
        let entry = tt
            .probe(pos.zobrist())
            .expect("quiet qsearch must produce TT entry");
        assert_eq!(entry.depth, 0);

        // Probe at different plies: centipawn score must not change.
        for probe_ply in [0_i32, 1, 3, 10, 20] {
            let probed = score_from_tt(entry.score as i32, probe_ply);
            assert_eq!(
                probed, expected_eval,
                "centipawn score must not be ply-adjusted; probe_ply={probe_ply}, \
                 got {probed}, expected {expected_eval}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M5.F §6.10 — Counter reset tests (2 tests, #[ignore])
    //
    // These tests verify that the M5.F counters reset correctly between
    // consecutive `qsearch_for_test` calls. WITHOUT the production probe +
    // store, the counters are always 0 — the "no carry-over" assertion passes
    // vacuously. Marking #[ignore] so the test suite reviewer notices the
    // tests are only meaningful after the implementation lands.
    // -----------------------------------------------------------------------

    /// Drive `qsearch_for_test` twice in succession. The second call must see
    /// `qsearch_tt_probes == 0` at entry (reset by the test-entry wrapper)
    /// AND the correct count after the second call (not a sum of both).
    ///
    /// Will become meaningful once M5.F production probe + store land.
    #[test]
    fn qsearch_tt_probes_resets_per_qsearch_for_test() {
        use crate::tt::{TranspositionTable, TtBound, TtData, score_to_tt};
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        let tt = Arc::new(TranspositionTable::new(1));
        let ply: u32 = 1;
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(100, ply as i32) as i16,
                depth: 0,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();

        // First call.
        let _ = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        let probes_after_first = ab.qsearch_tt_probes_for_test();

        // Second call — counter must reset, then fire once, total=1 (not 2).
        let _ = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        let probes_after_second = ab.qsearch_tt_probes_for_test();

        assert_eq!(
            probes_after_first, probes_after_second,
            "second qsearch_for_test call must see same probe count as first \
             (counter resets at entry, not accumulated); first={probes_after_first}, \
             second={probes_after_second}"
        );
        assert_eq!(
            probes_after_second, 1,
            "each qsearch_for_test call must produce exactly 1 probe on this fixture"
        );
    }

    /// Drive `qsearch_for_test` twice. The store counter must reset between
    /// calls (not accumulate across calls).
    ///
    /// Will become meaningful once M5.F production probe + store land.
    #[test]
    fn qsearch_tt_stores_resets_per_qsearch_for_test() {
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/2R1K3 w - - 0 1").expect("KvKR FEN must parse");
        let tt = Arc::new(TranspositionTable::new(1));

        let mut ab = AlphaBetaMover::new();
        ab.history = vec![pos.zobrist()];
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let (ctx, _stop) = non_aborting_ctx();
        let ply: u32 = 1;

        // First call.
        let _ = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        let stores_after_first = ab.qsearch_tt_stores_for_test();

        // Second call — counter must reset, then fire once.
        let _ = ab.qsearch_for_test(&mut pos.clone(), -INF, INF, ply, &ctx);
        let stores_after_second = ab.qsearch_tt_stores_for_test();

        assert_eq!(
            stores_after_first, stores_after_second,
            "second qsearch_for_test call must see same store count as first \
             (counter resets at entry, not accumulated); first={stores_after_first}, \
             second={stores_after_second}"
        );
        assert_eq!(
            stores_after_second, 1,
            "each qsearch_for_test call must produce exactly 1 store on this fixture"
        );
    }

    // M5.G — Singular extensions.
    //
    // Tests for:
    //   §8 helper-level tests (pure functions `singular_beta`,
    //   `verification_depth`, `singular_extension_eligible`) and
    //   §8 integration tests (actual `negamax` driving SE via
    //   `negamax_for_test` with pre-populated TT entries).
    //
    // Counter resets are wired in `negamax_for_test` (clears `se_extensions`),
    // so back-to-back invocations are independent. Per plan §8.
    // -----------------------------------------------------------------------

    // ===== Helper tests for `singular_beta` =====

    /// Base case: score=0, depth=0 → 0. Pins `raw = 0` and that the floor
    /// `max(-(MATE-1))` doesn't alter a non-negative result.
    #[test]
    fn singular_beta_zero_score_zero_depth_returns_zero() {
        assert_eq!(singular_beta(0, 0), 0);
    }

    /// Formula pin: `tt_score=100, depth=8` → `100 - 8*SE_MARGIN_PER_DEPTH`.
    /// At `SE_MARGIN_PER_DEPTH = 1` this yields `100 - 8 = 92`.
    #[test]
    fn singular_beta_subtracts_margin_per_depth() {
        let expected = 100 - SE_MARGIN_PER_DEPTH * 8_i32;
        assert_eq!(
            singular_beta(100, 8),
            expected,
            "singular_beta(100, 8) must equal {expected}"
        );
    }

    /// Floor clamp: when `raw` would go below `-(MATE - 1)`, the result is
    /// pinned at `-(MATE - 1)`. Input `tt_score = -(MATE_IN_MAX_PLY + 1)` at
    /// any depth drives raw well below the floor.
    #[test]
    fn singular_beta_floors_at_negative_mate_minus_one() {
        // Any deeply negative score must be floored.
        let very_negative = -(MATE - 2); // just above -(MATE-1) boundary
        let floor = -(MATE - 1);
        let result = singular_beta(very_negative, 100);
        assert_eq!(
            result, floor,
            "singular_beta at very negative input must floor at -(MATE-1); \
             got {result}, floor={floor}"
        );
    }

    /// Saturating subtraction prevents `i32` underflow when `tt_score` is
    /// near `i32::MIN`. `SE_MARGIN_PER_DEPTH * depth` could overflow if not
    /// handled — `saturating_sub` is the guard.
    #[test]
    fn singular_beta_saturating_handles_negative_score_near_min_int() {
        // i32::MIN saturating_sub with large depth must not panic in debug.
        let result = singular_beta(i32::MIN, u32::MAX);
        // saturating_sub yields i32::MIN, then max(-(MATE-1)) applies.
        let floor = -(MATE - 1);
        assert_eq!(
            result, floor,
            "singular_beta with i32::MIN input must return floor; got {result}"
        );
    }

    /// Gate boundary: when `tt_score = MATE_IN_MAX_PLY - 1` (highest allowed
    /// by clause 8) and `depth = SE_MIN_DEPTH = 8`, `singular_beta` must still
    /// be strictly below `MATE_IN_MAX_PLY`. Pins that the gate (clause 8) plus
    /// the helper together keep `singular_beta` out of the mate band.
    #[test]
    fn singular_beta_at_max_allowed_tt_score_is_below_mate_in_max_ply() {
        let max_tt_score = MATE_IN_MAX_PLY - 1;
        let result = singular_beta(max_tt_score, SE_MIN_DEPTH);
        assert!(
            result < MATE_IN_MAX_PLY,
            "singular_beta at max allowed tt_score must stay below MATE_IN_MAX_PLY; \
             got {result}, MATE_IN_MAX_PLY={MATE_IN_MAX_PLY}"
        );
    }

    // ===== Helper tests for `verification_depth` =====

    /// `(SE_MIN_DEPTH - 1) / 2 = (6 - 1) / 2 = 2` (integer division). Pins the
    /// formula at the minimum depth that triggers SE. Value updated at v2
    /// landing (SE_MIN_DEPTH retuned 8 → 6 per `bench/sprt/2026-05-10-m5.g-v2-min-depth-6-vs-m5f-mixed-tc.md`).
    #[test]
    fn verification_depth_at_se_min_depth_is_two() {
        assert_eq!(
            verification_depth(SE_MIN_DEPTH),
            2,
            "verification_depth(SE_MIN_DEPTH = {SE_MIN_DEPTH}) must equal (SE_MIN_DEPTH-1)/2 = 2"
        );
    }

    /// Monotone property: `verification_depth` is non-decreasing in `depth`.
    /// Checked exhaustively for `depth` in `[SE_MIN_DEPTH, SE_MIN_DEPTH + 16]`.
    #[test]
    fn verification_depth_monotonic_in_depth() {
        for d in SE_MIN_DEPTH..(SE_MIN_DEPTH + 16) {
            assert!(
                verification_depth(d) <= verification_depth(d + 1),
                "verification_depth must be monotone non-decreasing; \
                 verification_depth({d}) > verification_depth({})",
                d + 1
            );
        }
    }

    // ===== Helper tests for `singular_extension_eligible` =====

    /// Helper to build a representative happy-path call to
    /// `singular_extension_eligible` with all 9 clauses passing.
    fn se_eligible_happy_path() -> bool {
        singular_extension_eligible(
            None,                 // clause 1: no excluded move
            1,                    // clause 2: ply > 0
            false,                // clause 3: not PV
            false,                // clause 4: not in check
            SE_MIN_DEPTH,         // clause 5: depth >= SE_MIN_DEPTH
            TtBound::Lower,       // clause 6: Lower bound
            (SE_MIN_DEPTH) as u8, // clause 7: tt_depth >= depth - SE_TT_DEPTH_DELTA
            0,                    // clause 8: score 0 < MATE_IN_MAX_PLY
            1,                    // clause 9: non-zero tt_move
        )
    }

    /// Happy path: all 9 clauses pass → returns `true`.
    #[test]
    fn singular_extension_eligible_passes_full_gate() {
        assert!(
            se_eligible_happy_path(),
            "all 9 clauses passing must return true"
        );
    }

    /// Clause 1: `excluded_move = Some(...)` → immediate re-entrancy guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_when_excluded_move_some() {
        let dummy_move = Move::from_bits(1);
        let result = singular_extension_eligible(
            Some(dummy_move), // clause 1: excluded move set → reject
            1,
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "excluded_move = Some(...) must reject");
    }

    /// Clause 2: `ply == 0` → root guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_at_root_ply() {
        let result = singular_extension_eligible(
            None,
            0, // clause 2: ply == 0 → reject
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "ply == 0 must reject");
    }

    /// Clause 3: `is_pv = true` → PV guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_at_pv() {
        let result = singular_extension_eligible(
            None,
            1,
            true, // clause 3: is_pv → reject
            false,
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "is_pv = true must reject");
    }

    /// Clause 4: `in_check_flag = true` → check guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_in_check() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            true, // clause 4: in_check → reject
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "in_check = true must reject");
    }

    /// Clause 5: `depth < SE_MIN_DEPTH` → depth guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_below_min_depth() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH - 1, // clause 5: below threshold → reject
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "depth < SE_MIN_DEPTH must reject");
    }

    /// Clause 6a: `tt_bound = Exact` → Lower-only gate rejects.
    #[test]
    fn singular_extension_eligible_rejects_on_exact_bound() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Exact, // clause 6: Exact → reject
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "TtBound::Exact must reject (Lower-only gate)");
    }

    /// Clause 6b: `tt_bound = Upper` → Lower-only gate rejects.
    #[test]
    fn singular_extension_eligible_rejects_on_upper_bound() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Upper, // clause 6: Upper → reject
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(!result, "TtBound::Upper must reject (Lower-only gate)");
    }

    /// Clause 7: `tt_depth < depth - SE_TT_DEPTH_DELTA` → stale TT rejects.
    #[test]
    fn singular_extension_eligible_rejects_on_stale_tt_depth() {
        let depth = SE_MIN_DEPTH;
        let stale_tt_depth = (depth - SE_TT_DEPTH_DELTA - 1) as u8;
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            depth,
            TtBound::Lower,
            stale_tt_depth, // clause 7: too stale → reject
            0,
            1,
        );
        assert!(!result, "stale tt_depth must reject");
    }

    /// Clause 8: `tt_score >= MATE_IN_MAX_PLY` → mate-score guard rejects.
    #[test]
    fn singular_extension_eligible_rejects_on_mate_score() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            MATE_IN_MAX_PLY, // clause 8: mate-range score → reject
            1,
        );
        assert!(!result, "tt_score >= MATE_IN_MAX_PLY must reject");
    }

    /// Clause 9: `tt_move == 0` → sentinel TT move rejects.
    #[test]
    fn singular_extension_eligible_rejects_on_zero_tt_move() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH,
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            0, // clause 9: zero tt_move → reject
        );
        assert!(!result, "tt_move == 0 must reject");
    }

    /// Clause 5 boundary (pass): `depth == SE_MIN_DEPTH` must pass.
    #[test]
    fn singular_extension_eligible_passes_at_exact_threshold_depth() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH, // exactly at threshold → pass
            TtBound::Lower,
            SE_MIN_DEPTH as u8,
            0,
            1,
        );
        assert!(result, "depth == SE_MIN_DEPTH must pass clause 5");
    }

    /// Clause 5 boundary (reject): `depth == SE_MIN_DEPTH - 1` must reject.
    #[test]
    fn singular_extension_eligible_rejects_one_below_threshold_depth() {
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            SE_MIN_DEPTH - 1, // one below threshold → reject
            TtBound::Lower,
            (SE_MIN_DEPTH - 1) as u8, // fresh enough if depth were valid
            0,
            1,
        );
        assert!(!result, "depth == SE_MIN_DEPTH - 1 must reject clause 5");
    }

    /// Clause 7 boundary (pass): `tt_depth == depth - SE_TT_DEPTH_DELTA` must pass.
    #[test]
    fn singular_extension_eligible_passes_at_tt_depth_delta_boundary() {
        let depth = SE_MIN_DEPTH;
        let min_fresh_tt_depth = (depth - SE_TT_DEPTH_DELTA) as u8;
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            depth,
            TtBound::Lower,
            min_fresh_tt_depth, // exactly at the minimum fresh depth → pass
            0,
            1,
        );
        assert!(
            result,
            "tt_depth == depth - SE_TT_DEPTH_DELTA must pass clause 7; \
             tt_depth={min_fresh_tt_depth}, depth={depth}"
        );
    }

    /// Clause 7 boundary (reject): `tt_depth == depth - SE_TT_DEPTH_DELTA - 1` must reject.
    #[test]
    fn singular_extension_eligible_rejects_at_tt_depth_one_below_boundary() {
        let depth = SE_MIN_DEPTH;
        let too_stale = (depth - SE_TT_DEPTH_DELTA - 1) as u8;
        let result = singular_extension_eligible(
            None,
            1,
            false,
            false,
            depth,
            TtBound::Lower,
            too_stale, // one below minimum fresh depth → reject
            0,
            1,
        );
        assert!(
            !result,
            "tt_depth == depth - SE_TT_DEPTH_DELTA - 1 must reject clause 7; \
             tt_depth={too_stale}, depth={depth}"
        );
    }

    // ===== Property tests =====

    proptest::proptest! {
        /// `singular_beta` is monotone non-increasing in `depth` for a fixed
        /// `tt_score` in the valid range. Sanity check on the formula.
        #[test]
        fn singular_beta_is_monotone_decreasing_in_depth_for_fixed_tt_score(
            tt_score in -5000_i32..5000,
            depth in SE_MIN_DEPTH..(SE_MIN_DEPTH + 32),
        ) {
            let sb_d = singular_beta(tt_score, depth);
            let sb_d1 = singular_beta(tt_score, depth + 1);
            proptest::prop_assert!(
                sb_d >= sb_d1,
                "singular_beta({tt_score}, {depth})={sb_d} must be >= singular_beta({tt_score}, {})={sb_d1}",
                depth + 1
            );
        }

        /// `singular_extension_eligible` is invariant under `tt_score`
        /// perturbations within `(-MATE_IN_MAX_PLY, MATE_IN_MAX_PLY)` (clause 8
        /// passes for all, other clauses held constant and passing).
        #[test]
        fn singular_extension_eligible_independent_of_score_within_mate_band(
            tt_score in -(MATE_IN_MAX_PLY - 1)..(MATE_IN_MAX_PLY),
        ) {
            // All other clauses fixed and passing; only tt_score varies.
            let result = singular_extension_eligible(
                None,
                1,
                false,
                false,
                SE_MIN_DEPTH,
                TtBound::Lower,
                SE_MIN_DEPTH as u8,
                tt_score, // varies within mate band
                1,
            );
            proptest::prop_assert!(
                result,
                "singular_extension_eligible must pass for all tt_score within \
                 (-MATE_IN_MAX_PLY, MATE_IN_MAX_PLY); tt_score={tt_score}"
            );
        }
    }

    // ===== Integration tests for SE in `negamax` =====
    //
    // Each integration test needs a position + a pre-seeded TT entry that drives
    // a specific SE scenario. The approach: use a tactical position where the
    // best move is well-established (high-eval), pre-seed the parent's TT entry
    // as Lower bound at sufficient depth, and verify the counter / behavior.
    //
    // Position: "r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9"
    // (E1 — standard quiet middlegame, White-to-move, used extensively elsewhere).

    /// Helper: the standard SE test position.
    fn se_test_pos() -> Position {
        Position::from_fen("r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9")
            .expect("SE test position FEN must parse")
    }

    /// Helper: get the first legal move at `pos` as a `u16` bits-packed move.
    /// This is used as the `best_move` for TT seeding so that step-12 promotes
    /// it to `moves_vec[0]` (provided it's the highest-scored move, which a
    /// pre-run depth-1 search confirms).
    fn se_first_legal_move_bits(pos: &Position) -> u16 {
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        ml.iter()
            .next()
            .expect("SE test position must have legal moves")
            .bits()
    }

    /// Helper: run `negamax_for_test` at `depth=1` to find the best move at the
    /// test position, returning its bits. Used to seed TT entries with a real
    /// best move so that step-12 promotes it and `moves_vec[0].bits() == tt_move`
    /// holds.
    fn se_find_best_move_bits(pos: &Position) -> u16 {
        let mut ab = AlphaBetaMover::new();
        let (ctx, _) = non_aborting_ctx();
        ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, None, &ctx);
        // The best move is pv[0][0] after a depth-1 PV search.
        if ab.pv.lengths[0] > 0 {
            ab.pv.moves[0][0].bits()
        } else {
            se_first_legal_move_bits(pos)
        }
    }

    /// SE does NOT fire at depth < SE_MIN_DEPTH. Clause 5 rejects.
    /// Uses depth = SE_MIN_DEPTH - 1 even with an ideal TT entry.
    #[test]
    fn negamax_se_block_does_not_fire_below_min_depth() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH - 1;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        let tt_score = 50_i32;
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "depth = SE_MIN_DEPTH - 1 must not fire SE (clause 5); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire at a PV node. Clause 3 rejects.
    #[test]
    fn negamax_se_block_does_not_fire_at_pv_node() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        // is_pv = true → clause 3 rejects
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "is_pv = true must not fire SE (clause 3); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire at ply == 0. Clause 2 rejects.
    #[test]
    fn negamax_se_block_does_not_fire_at_ply_zero() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 0_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        // ply = 0 → clause 2 rejects
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "ply == 0 must not fire SE (clause 2); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire when TT bound is Exact. Clause 6 rejects (Lower-only).
    #[test]
    fn negamax_se_block_does_not_fire_when_tt_bound_is_exact() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Exact, // not Lower → clause 6 rejects
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "TtBound::Exact must not fire SE (clause 6); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire when TT bound is Upper. Clause 6 rejects (Lower-only).
    #[test]
    fn negamax_se_block_does_not_fire_when_tt_bound_is_upper() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Upper, // not Lower → clause 6 rejects
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "TtBound::Upper must not fire SE (clause 6); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire when TT score is in the mate band. Clause 8 rejects.
    #[test]
    fn negamax_se_block_does_not_fire_when_tt_score_is_mate() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        let mate_score = MATE_IN_MAX_PLY; // in the mate band
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: mate_score as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "tt_score in mate band must not fire SE (clause 8); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE does NOT fire when TT depth is too stale (clause 7).
    /// `tt_depth = depth - SE_TT_DEPTH_DELTA - 1` is one below the minimum.
    #[test]
    fn negamax_se_block_does_not_fire_when_tt_depth_is_stale() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        let stale_depth = (depth - SE_TT_DEPTH_DELTA - 1) as u8; // one below the threshold
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: stale_depth,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "stale tt_depth must not fire SE (clause 7); stale_depth={stale_depth}, \
             threshold={}, got {}",
            depth - SE_TT_DEPTH_DELTA,
            ab.se_extensions_for_test()
        );
    }

    /// Re-entrancy guard: calling `negamax_for_test` with `excluded_move = Some(...)`
    /// directly (simulating the verification frame) must NOT fire SE (clause 1
    /// rejects). The TT entry is otherwise ideal for SE.
    #[test]
    fn negamax_se_re_entrancy_clause_blocks_immediate_verification_frame() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        let excluded = Move::from_bits(best_move_bits);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        // Pass `excluded_move = Some(...)` directly, simulating the verification frame.
        // Clause 1 must reject SE at this frame.
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            Some(excluded),
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "excluded_move = Some(...) must block SE via clause 1 (re-entrancy guard); \
             got {}",
            ab.se_extensions_for_test()
        );
    }

    /// SE counter resets per `negamax_for_test` invocation. Back-to-back calls
    /// on the same mover must each start fresh. The first call uses a Lower-bound
    /// TT entry with a very high `tt_score` (same fixture as the self-cutoff test)
    /// so SE actually fires → `after_first == 1`. The second call gets a fresh
    /// counter → reads 0 initially, and if SE fires again it reads 1 (not 2).
    #[test]
    fn negamax_se_counter_resets_per_test_invocation() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        // Lower bound with MATE_IN_MAX_PLY - 1: verification will fail low (high bar),
        // guaranteeing SE fires on this call.
        let tt_score_raw = MATE_IN_MAX_PLY - 1;
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));

        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );
        let after_first = ab.se_extensions_for_test();

        // First call must fire SE (Lower bound, high score, balanced position).
        assert_eq!(
            after_first, 1,
            "first negamax_for_test call with Lower TT entry must fire SE once; \
             got after_first={after_first}"
        );

        // Second call on the same `ab`: counter must reset to 0 at entry.
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );
        let after_second = ab.se_extensions_for_test();

        // The key invariant: `negamax_for_test` resets `se_extensions` to 0 at entry,
        // so `after_second` reflects only what the second call did, not first + second.
        // If SE fires again (same fixture), after_second == 1 (not 2).
        // The TT may have been populated by the first call's subtree, but the top-level
        // seeded entry at pos.zobrist() may have been evicted; either way, after_second
        // must be 0 or 1, not the cumulative 2 that a missing reset would give.
        assert!(
            after_second <= 1,
            "second negamax_for_test call must start with a fresh counter (reset at entry); \
             after_first={after_first}, after_second={after_second} (expected <= 1, not 2)"
        );
    }

    /// The verification-frame TT cutoff is suppressed (load-bearing SE guard).
    /// This test constructs an ideal SE scenario (Lower TT entry, depth ≥ 8,
    /// non-PV, ply ≥ 1, etc.) and asserts `se_extensions >= 1`. Without the
    /// `excluded_move.is_none()` guard at step-7's cutoff branch, the same
    /// Lower entry would self-cut the verification frame every time, returning
    /// `tt_score >= singular_beta` and causing SE to never extend (counter = 0).
    ///
    /// This test fires SE via a real search; if SE fires, `se_extensions == 1`.
    /// If the step-7 cutoff guard is removed (mutant), `se_extensions == 0`.
    #[test]
    fn negamax_se_verification_frame_does_not_self_cutoff_via_tt_probe() {
        // Choose a middlegame position; run a depth-1 search first to get the
        // best move so the TT entry's `best_move` points to a real legal move
        // that step-12 promotes to `moves_vec[0]`.
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH; // 8
        let ply = 1_u32;

        // Find the best move by running a depth-1 PV search.
        let best_move_bits = se_find_best_move_bits(&pos);

        // Seed the TT: Lower bound, tt_depth == depth (fresh enough), score
        // chosen well above `singular_beta = tt_score - depth * SE_MARGIN = tt_score - 8`.
        // For SE to extend, the verification search at (depth-1)/2 = 3, with all
        // other moves available but no TT entries for them, must fail below
        // `singular_beta = tt_score - 8`. We choose a low positive `tt_score` so
        // `singular_beta = tt_score - 8` is negative, and the verification
        // (searching only non-TT-move moves at depth 3) should return around
        // `stm_static_eval` which in a balanced middlegame is near 0, potentially
        // below `singular_beta` only when `singular_beta` is highly negative.
        //
        // To guarantee the verification fails low (needed for `se_extensions >= 1`),
        // use a large `tt_score` so `singular_beta = tt_score - 8` is still positive
        // but the verification returns a score well below that (since the non-TT moves
        // in a non-trivial position should not all beat a large singular_beta).
        //
        // Conservative approach: set tt_score = MATE_IN_MAX_PLY - 1 (max allowed by
        // clause 8). Then singular_beta = tt_score - 8 = MATE_IN_MAX_PLY - 9.
        // The verification at depth 3 without the TT move will very likely fail
        // low against such a high bar (essentially proving no move is nearly as
        // good as the TT move).
        let tt_score_raw = MATE_IN_MAX_PLY - 1;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8, // exactly at threshold (depth - 0 >= depth - SE_TT_DEPTH_DELTA)
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            1,
            "SE must fire exactly once when all eligibility clauses pass and verification fails low; \
             se_extensions = {}. This test also pins the step-7 cutoff-suppression guard: \
             without `excluded_move.is_none()` the verification frame self-cuts and \
             se_extensions would be 0.",
            ab.se_extensions_for_test()
        );

        // Sister assertion: the verification search must have recursed into at
        // least one child of `pos` (excluding `best_move_bits`). Confirm that the
        // TT was written at a child zobrist — i.e. the verification search was
        // not a no-op but actually explored the position.
        use crate::mov::make_move;
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let child_has_tt_entry = ml.iter().any(|mv| {
            if mv.bits() == best_move_bits {
                return false; // skip the excluded (TT) move
            }
            let mut child = pos;
            let _undo = make_move(&mut child, mv);
            tt.probe(child.zobrist()).is_some()
        });
        assert!(
            child_has_tt_entry,
            "verification search must have stored at least one TT entry at a child of pos \
             (excluding the TT move); implies the verification actually ran its move loop"
        );
    }

    /// Excluded-move skip: with `excluded_move = Some(mv)`, the move `mv` is
    /// never searched in the move loop. Pins the `continue` at the top of the
    /// loop. Direct evidence: call `negamax_for_test` with an excluded move
    /// and assert the search completes without that move being played from the
    /// position (the PV from the frame doesn't start with the excluded move).
    #[test]
    fn negamax_skips_excluded_move_in_move_loop() {
        let pos = se_test_pos();
        let depth = 2_u32;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let excluded = Move::from_bits(best_move_bits);

        let (ctx, _) = non_aborting_ctx();

        // Score WITHOUT exclusion — normal search, the excluded move may be chosen.
        let mut ab_full = AlphaBetaMover::new();
        let score_full = ab_full.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx,
        );

        // Score WITH exclusion — excluded move is unconditionally skipped.
        let mut ab_excl = AlphaBetaMover::new();
        let score_excl = ab_excl.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            true,
            true,
            Some(excluded),
            &ctx,
        );

        // If `excluded` is the best move, the score with exclusion must be no better
        // than the score without it. (Equal scores can happen when another move ties.)
        // This is unconditional: the excluded move is the depth-1 best move, so
        // restricting it cannot improve the score from the same position.
        assert!(
            score_excl <= score_full,
            "excluding the best move must not improve the score; \
             score_full={score_full}, score_excl={score_excl}, excluded={excluded:?}"
        );

        // The PV at ply must NOT start with the excluded move.
        // Length is always > 0 because the position has legal non-excluded moves.
        assert!(
            ab_excl.pv.lengths[ply as usize] > 0,
            "search with excluded move must still return a PV (other legal moves exist)"
        );
        let pv_first = ab_excl.pv.moves[ply as usize][0];
        assert_ne!(
            pv_first, excluded,
            "excluded move must not appear as PV[0] at ply {ply}; excluded={excluded:?}"
        );
    }

    /// Post-loop TT store is suppressed at the verification frame.
    /// Call `negamax_for_test` with `excluded_move = Some(mv)` and assert no
    /// new TT entry appears at `pos.zobrist()` after the call.
    /// (The move-loop children still store at THEIR zobrist keys — different from
    /// the parent's key — so the TT may have entries, just not at pos.zobrist().)
    #[test]
    fn negamax_does_not_store_tt_at_verification_frame() {
        let pos = se_test_pos();
        let depth = 2_u32;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let excluded = Move::from_bits(best_move_bits);

        let tt = Arc::new(TranspositionTable::new(16));
        // Install TT in the context so the store would fire if not gated.
        let (ctx, _) = non_aborting_ctx_with_tt(Arc::clone(&tt));
        tt.new_search();

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            Some(excluded),
            &ctx,
        );

        // The verification frame must NOT have stored at pos.zobrist().
        assert!(
            tt.probe(pos.zobrist()).is_none(),
            "verification frame (excluded_move = Some) must not store at pos.zobrist()"
        );
    }

    /// NMP store is suppressed at the verification frame.
    /// Set up conditions so NMP fires at the verification frame (depth ≥ NMP_MIN_DEPTH,
    /// static_eval >= beta, non-PV, non-check, has non-pawn material) while
    /// `excluded_move = Some(...)`. Assert no TT entry appears at pos.zobrist().
    #[test]
    fn negamax_nmp_does_not_store_tt_at_verification_frame() {
        // Position where NMP is likely to fire: non-PV, not in check, with
        // non-pawn material (standard middlegame), beta set very low so
        // static_eval >> beta.
        let pos = se_test_pos();
        let static_eval = stm_static_eval(&pos);
        // Choose beta well below static_eval so the NMP gate's `static_eval >= beta`
        // fires. At depth = NMP_MIN_DEPTH = 3, NMP is eligible.
        let depth = NMP_MIN_DEPTH; // 3
        let beta = static_eval - 500; // well below static_eval
        let alpha = beta - 1;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let excluded = Move::from_bits(best_move_bits);

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_with_tt(Arc::clone(&tt));
        tt.new_search();

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            alpha,
            beta,
            false,
            true,
            Some(excluded),
            &ctx,
        );

        // NMP may have fired (ab.nmp_firings > 0), but must NOT store at pos.zobrist().
        assert!(
            tt.probe(pos.zobrist()).is_none(),
            "NMP store at verification frame (excluded_move = Some) must be suppressed"
        );
    }

    /// The `se_extensions` counter is zero at node-depth below SE_MIN_DEPTH,
    /// confirming the depth-budget invariant: at parent_depth = SE_MIN_DEPTH,
    /// verification runs at depth (SE_MIN_DEPTH-1)/2 = 3, and that subtree
    /// cannot itself fire SE (clause 5 rejects at depth < 8).
    #[test]
    fn negamax_se_in_deep_subtree_bounded_by_depth_halving() {
        // Run at SE_MIN_DEPTH with an ideal TT entry (same as the
        // _does_not_self_cutoff test). At most 1 SE extension should fire
        // (the parent's own), and no recursive SE in the verification subtree
        // (which runs at depth 3, well below SE_MIN_DEPTH = 8).
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let tt_score_raw = MATE_IN_MAX_PLY - 1;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // At SE_MIN_DEPTH, the verification subtree depth is (8-1)/2 = 3 < 8 =
        // SE_MIN_DEPTH, so no recursive SE fires. Exactly 1 extension fires at the
        // parent frame; none fire recursively inside the verification subtree.
        assert_eq!(
            ab.se_extensions_for_test(),
            1,
            "at parent_depth = SE_MIN_DEPTH = 8, exactly 1 SE extension must fire \
             (depth-halving bounds recursive SE to 0); got {} extensions",
            ab.se_extensions_for_test()
        );
    }

    /// Verification fail-high: when the verification search returns `>= singular_beta`,
    /// no extension fires. This pins the `<` operator against `<=`: a `<=` mutant
    /// would extend when `verif_score == singular_beta` (fail-equal), which is wrong.
    ///
    /// Strategy: use a very low `tt_score` so `singular_beta = tt_score - depth`
    /// is also low (around -8). A depth-3 search of a balanced middlegame returns
    /// roughly `stm_static_eval` which is near 0, far above -8, guaranteeing
    /// `verif_score >= singular_beta` (fail-high). No extension fires.
    ///
    /// Complementary to `negamax_se_verification_frame_does_not_self_cutoff_via_tt_probe`
    /// (which uses a very high `tt_score` to guarantee fail-low → extension fires).
    #[test]
    fn negamax_skips_extension_when_verification_fails_high() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH; // 8
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);

        // Low tt_score so singular_beta = 0 - 8 = -8.
        // The verification at depth 3 returns roughly static_eval ≈ 0 >> -8 → fail-high.
        let tt_score_raw: i32 = 0;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // Verification returns > singular_beta = -8 (balanced position ≈ 0), so no extension.
        // A `<=` mutant at the `verif_score < s_beta` check would still not fire here
        // unless verif_score == s_beta exactly, but together with the fail-low test it
        // covers both sides of the boundary.
        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "verification fail-high must NOT fire SE extension; \
             tt_score={tt_score_raw}, singular_beta={}, se_extensions={}",
            singular_beta(tt_score_raw, depth),
            ab.se_extensions_for_test()
        );
    }

    /// Boundary-engineering fixture for the verification window `(s_beta - 1, s_beta)`
    /// at `src/search.rs`'s SE verification call. Closes three mutants that survived
    /// M5.G's `cargo mutants --in-diff` campaign:
    ///   - `s_beta - 1` → `s_beta + 1`: inverted window (alpha > beta).
    ///   - `s_beta - 1` → `s_beta / 1` = `s_beta`: zero-width window (alpha == beta).
    ///   - `<` → `<=` on the outer `if verif_score < s_beta { se_extensions += 1; }`
    ///     (test-suite review pass-2 should-fix).
    ///
    /// Why a boundary fixture is needed. The fail-soft cutoff at the verification
    /// frame is `if score > best { best = score; … if alpha >= beta { break; } }`
    /// (see step-13 move-loop body). The `alpha >= beta` check runs *inside* the
    /// `score > best` block but *outside* the `score > alpha` alpha-update, so a
    /// mutated alpha that is already ≥ beta at frame entry fires cutoff on the
    /// very first move searched — returning that move's raw fail-soft score.
    /// Production's alpha = `s_beta - 1` is strictly below beta = `s_beta`, so
    /// production cuts off only when a child's score actually reaches `s_beta`.
    /// Both `+` and `/` mutants enter the move loop with alpha ≥ beta and cut at
    /// the first non-excluded child; production iterates until a child lifts best
    /// up to `s_beta`.
    ///
    /// Engineered fixture. The position has many legal moves; we pre-store Exact
    /// TT entries at every non-excluded child's zobrist so each child's step-7
    /// probe short-circuits to a known score. The first non-excluded move in
    /// stager order returns `s_beta - δ` (δ > 0); every later non-excluded move
    /// returns exactly `s_beta`. Trace:
    ///   - Production iter 1 (first non-excluded): score = `s_beta - δ`. `best`
    ///     updates to `s_beta - δ`; alpha = `s_beta - 1` (no update — score is
    ///     not > `s_beta - 1`); cutoff check `s_beta - 1 >= s_beta` is false;
    ///     continue.
    ///   - Production iter 2 (second non-excluded): score = `s_beta`. `best`
    ///     updates to `s_beta`; alpha updates to `s_beta`; cutoff fires
    ///     (`s_beta >= s_beta`). Returns `verif_score = s_beta`.
    ///   - Production outer check `s_beta < s_beta` is false → `se_extensions = 0`.
    ///   - Mutant `+` iter 1: alpha already > beta. Returns `best = s_beta - δ`.
    ///     `verif_score = s_beta - δ < s_beta` → extension fires (se_extensions = 1).
    ///   - Mutant `/` iter 1: alpha == beta. Same outcome as `+`.
    ///   - Mutant `<=`: production trace, `s_beta <= s_beta` true → extension fires.
    ///
    /// Closes `docs/tuning-backlog.md` M5.G item 4.1 and the matching backlog
    /// line in ADR-0029 "Open / tuning backlog".
    #[test]
    fn negamax_se_extension_at_singular_beta_boundary() {
        use crate::history::HistoryTable;

        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;
        let tt_move_bits = se_find_best_move_bits(&pos);

        // Probe the verification frame's stager yield order. The verification
        // frame constructs MoveStager with the *same* inputs available here:
        // ply=1 (same as parent), fresh killer table (empty at ply 1 from a
        // newly-constructed AlphaBetaMover — no cutoff has yet fired at ply 1
        // when the SE block runs), fresh history table, and the parent's TT
        // move. Reproducing those inputs gives us the exact yield order the
        // verification frame will see.
        let probe_history = HistoryTable::new();
        let probe_stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &probe_history,
            tt_move_bits,
            None,
        );
        let yield_seq = probe_stager.yield_sequence();
        assert!(
            yield_seq.len() >= 3,
            "fixture invariant: position must have >= 3 legal moves to give the \
             verification frame a 'first non-excluded' AND a 'later non-excluded' \
             move; got {} legal moves",
            yield_seq.len()
        );
        assert_eq!(
            yield_seq[0].bits(),
            tt_move_bits,
            "fixture invariant: TT-move promotion must place the TT move at yield \
             index 0; got yield_seq[0]={:?}, tt_move_bits={}",
            yield_seq[0],
            tt_move_bits
        );

        // s_beta = 50: well below RFP-firing threshold for the verification
        // frame's depth-2 RFP gate (margin = 200; would need static_eval ≥ 250
        // to fire, but se_test_pos has symmetric material so |static_eval| is
        // far smaller), and well below MATE_IN_MAX_PLY so SE clause 8 passes
        // and `score_to_tt`/`score_from_tt` are identity transforms.
        let s_beta_value: i32 = 50;
        let s_beta_delta: i32 = 5;
        let tt_score_raw: i32 = s_beta_value + depth as i32;
        assert_eq!(
            singular_beta(tt_score_raw, depth),
            s_beta_value,
            "test setup: singular_beta({tt_score_raw}, {depth}) must equal {s_beta_value}"
        );

        // Build TT and seed entries.
        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));
        tt.new_search();

        // Parent (the node SE fires at): Lower-bound at depth = SE_MIN_DEPTH,
        // score = tt_score_raw. Satisfies all 9 SE eligibility clauses.
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: tt_move_bits,
            },
        );

        // Pre-store Exact entries at every non-excluded child's zobrist:
        //   yield_seq[1] (first searched non-excluded) → score = -(s_beta - δ),
        //     so parent's negated child return is s_beta - δ < s_beta.
        //   yield_seq[2..] (every later non-excluded) → score = -s_beta,
        //     so parent's negated child return is exactly s_beta.
        //
        // Pre-storing yield_seq[2..] uniformly (not just yield_seq[2]) defends
        // against an unrelated child going through real step-8 RFP / step-13
        // search at depth 1 — RFP cannot push verif_score above s_beta from the
        // parent's perspective (RFP fires only if child static_eval ≥ 51, which
        // forces the negated parent score to be < 50), but a real recursive
        // search could conceivably return any value. Pre-storing every later
        // child makes the trace fully deterministic.
        for (idx, &mv) in yield_seq.iter().enumerate() {
            if idx == 0 {
                continue; // TT-move (excluded); never reached at verification frame
            }
            let child_score: i32 = if idx == 1 {
                -(s_beta_value - s_beta_delta) // -45
            } else {
                -s_beta_value // -50
            };
            let mut child_pos = pos;
            let _undo = child_pos.make_move(mv);
            tt.store(
                child_pos.zobrist(),
                TtData {
                    score: child_score as i16,
                    depth: depth as u8,
                    bound: TtBound::Exact,
                    best_move: 0,
                },
            );
        }

        // Drive the search.
        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // Strict `<` rejects equality at the boundary: production verif_score
        // lands exactly at s_beta (= 50), and `50 < 50` is false → no extension.
        // All three mutants (`+`, `/`, `<=`) flip this single assertion to 1.
        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "boundary case: verif_score landing at s_beta = {s_beta_value} must NOT \
             extend (strict `<` rejects equality); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// Positive SE integration test: with all eligibility clauses passing and a
    /// very high `tt_score` (so `singular_beta` is unreachably high for the
    /// verification search), `se_extensions` increments exactly once.
    ///
    /// Complementary to `negamax_skips_extension_when_verification_fails_high`
    /// (fail-high → no extension). This test pins the fail-low branch of the
    /// `if verif_score < s_beta` check. Distinct from the self-cutoff test in
    /// intent: this test focuses on "extension IS set when verification fails low",
    /// not on "the cutoff guard is present".
    #[test]
    fn negamax_sets_extension_when_verification_fails_low() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        // Very high tt_score so singular_beta = tt_score - depth is also very high.
        // Verification at depth 3 cannot beat this bar → fail-low → extension fires.
        let tt_score_raw = MATE_IN_MAX_PLY - 1;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            1,
            "SE must fire exactly once when verification fails low; got {}",
            ab.se_extensions_for_test()
        );
    }

    /// When SE fires, the TT move (i == 0) is searched at `depth` (= depth - 1 + 1)
    /// instead of `depth - 1`. The `se_tt_move_search_depth_for_test` field records
    /// the effective depth for the TT move's non-LMR dispatch. Without the extension
    /// (`move_extension == 0`), the TT move would be searched at `depth - 1`.
    #[test]
    fn negamax_se_extension_increments_child_depth_by_one() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let tt_score_raw = MATE_IN_MAX_PLY - 1; // guarantees SE fires (fail-low verification)

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // SE must have fired (precondition for testing the depth increment).
        assert_eq!(
            ab.se_extensions_for_test(),
            1,
            "SE must fire before the depth-increment assertion can be meaningful"
        );

        // The TT move was dispatched at `depth - 1 + move_extension`.
        // When SE fires, `move_extension == 1`, so effective depth == depth.
        assert_eq!(
            ab.se_tt_move_search_depth_for_test(),
            Some(depth),
            "SE extension must increment TT move dispatch depth from {} to {}; got {:?}",
            depth - 1,
            depth,
            ab.se_tt_move_search_depth_for_test()
        );
    }

    /// Companion to `negamax_se_block_does_not_fire_below_min_depth`: at depth <
    /// SE_MIN_DEPTH (clause 5 blocks), `move_extension == 0` for all moves and
    /// the `se_tt_move_search_depth` recorder (which only fires when
    /// `move_extension > 0`) stays `None`. Pins clause-5 blocking via the
    /// search-depth-recorder discriminator, complementing the counter-based
    /// guard. Does NOT pin the `if i == 0 { tt_move_extension } else { 0 }`
    /// per-iteration predicate — that mutation surface is structurally covered
    /// (the predicate is the only consumer of `tt_move_extension` and a mutant
    /// flipping `i == 0` to `i == 1` would break the `se_tt_move_search_depth
    /// == Some(depth)` assertion in `negamax_se_extension_increments_child_depth_by_one`).
    #[test]
    fn negamax_se_below_min_depth_does_not_record_extension_depth() {
        let pos = se_test_pos();
        // Use depth < SE_MIN_DEPTH so SE is guaranteed not to fire (clause 5).
        let depth = SE_MIN_DEPTH - 1; // 7: SE won't fire
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(MATE_IN_MAX_PLY - 1, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower, // Lower but depth < SE_MIN_DEPTH → clause 5 blocks
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // No SE fires at depth < SE_MIN_DEPTH.
        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "SE must not fire at depth = SE_MIN_DEPTH - 1 (clause 5); got {}",
            ab.se_extensions_for_test()
        );

        // `se_tt_move_search_depth` is only set when `move_extension > 0`.
        // With SE blocked, `move_extension == 0` for all moves, so the field stays None.
        assert_eq!(
            ab.se_tt_move_search_depth_for_test(),
            None,
            "without SE, se_tt_move_search_depth must be None (extension never applied); got {:?}",
            ab.se_tt_move_search_depth_for_test()
        );
    }

    /// Double-extension guard: SE cannot fire recursively at depth < SE_MIN_DEPTH,
    /// so a single-ply SE extension from a depth-8 node creates a depth-9 child;
    /// that child's depth (9) is still >= SE_MIN_DEPTH, but the re-entrancy guard
    /// (clause 1: `excluded_move.is_none()`) prevents nested SE at the verification
    /// frame. In a normal call (non-verification), the child at depth 9 could in
    /// principle fire SE, raising the counter to 2.
    ///
    /// The `negamax_se_in_deep_subtree_bounded_by_depth_halving` test already asserts
    /// `se_extensions == 1` for the SE_MIN_DEPTH fixture, which implicitly confirms
    /// that the depth-halving keeps verification depth at 3 (< SE_MIN_DEPTH) so no
    /// recursive SE fires inside the verification subtree. That test is the
    /// "double-extension does not happen in the verification subtree" guard.
    ///
    /// A true "double-extension in the normal call tree" scenario requires a deeper
    /// search (depth >= SE_MIN_DEPTH + 1 = 9) with multiple qualifying TT entries at
    /// different plies, which is infeasible to control deterministically in a unit
    /// test without engine-level fixture engineering. The depth-halving bound in
    /// `verification_depth` (plan §6.4) makes the verification subtree structurally
    /// excluded from recursive SE. No explicit double-extension unit test is added;
    /// the depth-halving test covers the in-verification case, and the property is
    /// documented here.
    #[allow(dead_code)]
    fn negamax_se_extension_does_not_double() {
        // See doc comment above. This function is intentionally not annotated
        // with `#[test]`; it documents the "double-extension" reasoning inline.
        // The actual guard is exercised by `negamax_se_in_deep_subtree_bounded_by_depth_halving`.
    }

    /// SE does NOT fire when the node is in check. Clause 4 rejects.
    /// Construct a position where the side to move is in check.
    #[test]
    fn negamax_se_block_does_not_fire_in_check() {
        // White king on e1, Black queen on e2: White is in check from the queen.
        let pos = Position::from_fen("4k3/8/8/8/8/8/4q3/4K3 w - - 0 1").expect("FEN must parse");

        use crate::movegen::in_check;
        assert!(in_check(&pos), "test fixture: White must be in check");

        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        // Find the first legal evasion move to use as tt_move.
        use crate::movegen::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let first_move_bits = ml
            .iter()
            .next()
            .expect("in-check position must have legal moves")
            .bits();

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: first_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "in_check = true must not fire SE (clause 4); got {}",
            ab.se_extensions_for_test()
        );
    }

    /// Abort during verification propagates correctly: the SE block checks
    /// `self.aborted` after the verification call and returns 0 immediately,
    /// without incrementing the SE extensions counter.
    ///
    /// Strategy: pre-set `ab.aborted = true` via `set_aborted_for_test` before
    /// calling `negamax_for_test`. The abort flag is NOT reset by `negamax_for_test`,
    /// so it persists. Inside the SE verification call, the first move-loop
    /// `if self.aborted { return 0; }` guard fires immediately after the first
    /// child returns (even if the child ran normally), making the verification
    /// return 0. Back in the SE block, `if self.aborted { return 0; }` then fires,
    /// preventing `se_extensions` from being incremented.
    ///
    /// This is deterministic: the pre-set abort flag is always visible to every
    /// `if self.aborted { return 0; }` check in the move loops, regardless of
    /// node count or search depth.
    #[test]
    fn negamax_aborts_during_verification_propagate_correctly() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let best_move_bits = se_find_best_move_bits(&pos);
        let tt_score_raw = MATE_IN_MAX_PLY - 1;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: score_to_tt(tt_score_raw, ply as i32) as i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        // Pre-set the aborted flag. `negamax_for_test` does NOT reset it, so
        // the flag persists into the search. The first move-loop abort check
        // inside the verification fires, causing verification to return 0.
        ab.set_aborted_for_test(true);
        let returned = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // The pre-set abort flag is observed by the SE block's post-verification
        // `if self.aborted { return 0; }` guard. This guard fires BEFORE the
        // `if verif_score < s_beta { se_extensions += 1; }` site.
        assert!(ab.aborted, "aborted flag must remain true after the search");
        assert_eq!(returned, 0, "aborted search must return 0; got {returned}");
        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "abort observed after verification must prevent se_extensions increment; got {}",
            ab.se_extensions_for_test()
        );
    }

    // ===================================================================
    // M5.H1 — MoveStager tests.
    //
    // H1-S*  : pure unit tests on MoveStager construction / iteration.
    // H1-H*  : pure unit tests on helper functions (extract_move_by_bits etc.)
    // H1-I*  : negamax integration tests (driven through negamax_for_test).
    // H1-B*  : bench/E51 pin tests (H1-B2 is the renamed E51 pin — see below).
    // H1-P1  : equivalence proptest: MoveStager::yield_sequence == order_moves.
    //
    // Plan: docs/plans/m5.h1.md §8.
    // ===================================================================

    // -------------------------------------------------------------------
    // Test helpers shared by H1-S / H1-H tests.
    // -------------------------------------------------------------------

    /// Build a `MoveStager` with all-default (sentinel) killer/TT inputs.
    fn plain_stager(pos: &Position) -> MoveStager {
        MoveStager::new(
            pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
            None,
        )
    }

    /// Return the legal-move list for `pos` (in generate_moves emit order).
    fn legal_moves(pos: &Position) -> Vec<Move> {
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        ml.iter().collect()
    }

    // -------------------------------------------------------------------
    // H1-S1 — empty (stalemate) position yields nothing.
    // -------------------------------------------------------------------
    #[test]
    fn stager_empty_position_yields_nothing() {
        // "7k/5Q2/6K1/8/8/8/8/8 b - - 0 1" — stalemate (Black has no legal moves).
        let pos =
            Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").expect("stalemate FEN must parse");
        let mut stager = plain_stager(&pos);
        assert_eq!(stager.len(), 0, "H1-S1: stalemate → len == 0");
        assert!(stager.is_empty(), "H1-S1: stalemate → is_empty");
        assert_eq!(stager.next(), None, "H1-S1: stalemate → next() == None");
        assert_eq!(stager.peek(), None, "H1-S1: stalemate → peek() == None");
    }

    // -------------------------------------------------------------------
    // H1-S2 — TT move is yielded first when its bits are in the legal list.
    //          Uses a middle-index move so the move would NOT naturally sort
    //          first — pins that the stager actually promotes it.
    // -------------------------------------------------------------------
    #[test]
    fn stager_yields_tt_first_when_in_legal_list() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        assert!(
            all.len() >= 5,
            "H1-S2 fixture must have ≥5 legal moves; starting position has 20"
        );
        // Use a middle-index move — it would not naturally sort first (movegen
        // order is neither MVV-LVA-first nor history-first), so a stager that
        // merely returns the first legal move would fail this test.
        let tt_target = all[all.len() / 2];
        let history = HistoryTable::new();
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &history,
            tt_target.bits(),
            None,
        );
        assert_eq!(
            stager.peek().map(|m| m.bits()),
            Some(tt_target.bits()),
            "H1-S2: TT move must be promoted to position 0 even when it would not naturally sort first"
        );
    }

    // -------------------------------------------------------------------
    // H1-S3 — stale/absent TT bits → no TT yield.
    // -------------------------------------------------------------------
    #[test]
    fn stager_skips_tt_when_bits_not_in_legal_list() {
        let pos = Position::starting_position();
        // A bits value that no legal move can have: pick 0xFFFF (garbage).
        let stale_tt_bits: u16 = 0xFFFF;
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            stale_tt_bits,
            None,
        );
        // The stager must still yield the legal moves (no TT stage).
        let seq = stager.yield_sequence();
        assert!(
            seq.iter().all(|m| m.bits() != stale_tt_bits),
            "H1-S3: stale TT bits must never appear in the yield sequence"
        );
        // Total moves should equal the full legal-move count (stale TT is
        // silently ignored, not subtracted from the count).
        let legal_count = legal_moves(&pos).len();
        assert_eq!(
            stager.len(),
            legal_count,
            "H1-S3: stale TT → len must equal full legal-move count ({legal_count})"
        );
    }

    // -------------------------------------------------------------------
    // H1-S4 — tt_move == 0 → no TT yield.
    // -------------------------------------------------------------------
    #[test]
    fn stager_skips_tt_when_tt_move_zero() {
        let pos = Position::starting_position();
        let legal_count = legal_moves(&pos).len();
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0, // zero → no TT
            None,
        );
        let seq = stager.yield_sequence();
        // No move should have bits == 0 (Move::default() is never in legal list).
        assert!(
            seq.iter().all(|m| m.bits() != 0),
            "H1-S4: tt_move == 0 must produce no zero-bits move in yield sequence"
        );
        assert_eq!(
            stager.len(),
            legal_count,
            "H1-S4: tt_move == 0 → len must equal full legal-move count ({legal_count})"
        );
    }

    // -------------------------------------------------------------------
    // H1-S5 — captures are yielded in MVV-LVA-desc order.
    // -------------------------------------------------------------------
    #[test]
    fn stager_yields_captures_in_mvv_lva_desc_order() {
        // Kiwipete: rich capture landscape (8+ captures available).
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete FEN must parse");

        let stager = plain_stager(&pos);
        let seq = stager.yield_sequence();

        // Collect the capture-stage moves.  tt_move == 0 so no TT stage.
        let captures: Vec<Move> = seq.iter().copied().filter(|&m| !is_quiet(m)).collect();

        // Precondition: kiwipete must have captures so the ordering check is
        // non-vacuous.  If this fires, the fixture or the filter changed.
        assert!(
            !captures.is_empty(),
            "H1-S5 fixture must have at least one capture (kiwipete); got empty — \
             either the fixture FEN changed or the stub is not filtering correctly"
        );

        for window in captures.windows(2) {
            let s0 = mvv_lva_score(window[0], &pos);
            let s1 = mvv_lva_score(window[1], &pos);
            assert!(
                s0 >= s1,
                "H1-S5: capture block must be MVV-LVA non-increasing; \
                 got score({:?})={s0} < score({:?})={s1}",
                window[0],
                window[1]
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-S5b — tied MVV-LVA captures preserve movegen-emit order (plan §4.1
    //           Case 3: stable sort within equal-score groups).
    // -------------------------------------------------------------------
    #[test]
    fn stager_tied_mvv_lva_captures_preserve_movegen_emit_order() {
        // Two white pawns on c4/e4, black pawn on d5.  Both cxd5 and exd5
        // score identically (PAWN*16 - PAWN), so the stager must preserve
        // movegen-emit order within the tied group.
        // Kings on e1/e8 satisfy the FEN parser's MissingKing invariant
        // without participating in captures at d5 (different rank, different
        // file alignment).
        let pos = Position::from_fen("4k3/8/8/3p4/2P1P3/8/8/4K3 w - - 0 1")
            .expect("tied-capture FEN must parse");

        let stager = plain_stager(&pos);
        let seq = stager.yield_sequence();

        // Collect only the capture moves from the sequence.
        let captures: Vec<Move> = seq.iter().copied().filter(|&m| !is_quiet(m)).collect();
        assert!(
            captures.len() >= 2,
            "H1-S5b: fixture must have at least 2 captures (cxd5 and exd5); got {}",
            captures.len()
        );

        // Confirm all captured pairs are tied on MVV-LVA.
        let s0 = mvv_lva_score(captures[0], &pos);
        let s1 = mvv_lva_score(captures[1], &pos);
        assert_eq!(
            s0, s1,
            "H1-S5b: fixture expects tied MVV-LVA scores; got s0={s0} s1={s1} — fixture changed?"
        );

        // Build the reference order from movegen (what order did the generator
        // emit these two captures?).
        let all = legal_moves(&pos);
        let gen_caps: Vec<Move> = all.iter().copied().filter(|&m| !is_quiet(m)).collect();
        assert!(
            gen_caps.len() >= 2,
            "H1-S5b: movegen must see at least 2 captures; got {}",
            gen_caps.len()
        );

        // The stager's capture block must equal the movegen-emit order
        // (stable sort preserves relative order within tied score group).
        assert_eq!(
            captures.iter().map(|m| m.bits()).collect::<Vec<_>>(),
            gen_caps.iter().map(|m| m.bits()).collect::<Vec<_>>(),
            "H1-S5b: tied-MVV-LVA captures must appear in movegen-emit order;\n\
             stager: {:?}\n\
             movegen: {:?}",
            captures.iter().map(|m| m.bits()).collect::<Vec<u16>>(),
            gen_caps.iter().map(|m| m.bits()).collect::<Vec<u16>>(),
        );
    }

    // -------------------------------------------------------------------
    // H1-S6 — TT capture excluded from capture stage.
    // -------------------------------------------------------------------
    #[test]
    fn stager_excludes_tt_capture_from_capture_stage() {
        // Use kiwipete — has captures.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete FEN must parse");

        // Find the first non-quiet (capture) legal move to use as TT.
        let all = legal_moves(&pos);
        let capture_mv = all.iter().find(|&&m| !is_quiet(m)).copied();
        let Some(cap) = capture_mv else {
            // If no captures available, this test is vacuous.
            return;
        };

        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            cap.bits(),
            None,
        );
        let seq = stager.yield_sequence();

        // The TT move must appear exactly once.
        let count = seq.iter().filter(|&&m| m.bits() == cap.bits()).count();
        assert_eq!(
            count, 1,
            "H1-S6: TT capture must appear exactly once in yield sequence; found {count}"
        );

        // If TT was yielded first, it must not reappear in the capture block.
        if seq.first().map(|m| m.bits()) == Some(cap.bits()) {
            let cap_stage: Vec<Move> = seq[1..].iter().copied().filter(|&m| !is_quiet(m)).collect();
            assert!(
                cap_stage.iter().all(|m| m.bits() != cap.bits()),
                "H1-S6: TT capture must not appear in the capture stage after being yielded first"
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-S7 — killer0 yielded iff it is a legal quiet.
    // -------------------------------------------------------------------
    #[test]
    fn stager_yields_killer0_iff_present_and_quiet() {
        // Use kiwipete for a richer position: has both captures and quiets
        // so we can exercise both the "captures-before-killer" and
        // "killer-before-quiets" ordering constraints.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete FEN must parse");
        let all = legal_moves(&pos);
        let Some(k0) = all.iter().find(|&&m| is_quiet(m)).copied() else {
            return;
        };
        let killer1 = Move::default();

        let stager = MoveStager::new(&pos, k0, killer1, &HistoryTable::new(), 0, None);
        let seq = stager.yield_sequence();

        // k0 must appear exactly once.
        let count = seq.iter().filter(|&&m| m == k0).count();
        assert_eq!(
            count, 1,
            "H1-S7: legal killer0 quiet must appear exactly once in the sequence; found {count}"
        );

        let k0_idx = seq
            .iter()
            .position(|&m| m == k0)
            .expect("k0 must be in seq");

        // All captures (non-quiet) must appear before k0.
        for (idx, &mv) in seq.iter().enumerate() {
            if !is_quiet(mv) {
                assert!(
                    idx < k0_idx,
                    "H1-S7: all captures must appear before killer0; \
                     capture at {idx} but k0 at {k0_idx}"
                );
            }
        }

        // All non-killer quiets must appear after k0 (killer0 < quiets ordering).
        let first_non_killer_quiet_idx = seq
            .iter()
            .position(|&m| is_quiet(m) && m != k0 && m != killer1);
        if let Some(q_idx) = first_non_killer_quiet_idx {
            assert!(
                k0_idx < q_idx,
                "H1-S7: killer0 must appear before non-killer quiets; \
                 k0 at {k0_idx} but first non-killer quiet at {q_idx}"
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-S8 — killer0 == TT → killer0 not separately yielded.
    // -------------------------------------------------------------------
    #[test]
    fn stager_skips_killer0_when_equal_to_tt() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        // Use the first quiet as both TT move and killer0.
        let Some(quiet_mv) = all.iter().find(|&&m| is_quiet(m)).copied() else {
            return;
        };

        let stager = MoveStager::new(
            &pos,
            quiet_mv, // killer0 == TT
            Move::default(),
            &HistoryTable::new(),
            quiet_mv.bits(), // tt_move == killer0
            None,
        );
        let seq = stager.yield_sequence();

        // The move must appear exactly once (as the TT yield, not again as k0).
        let count = seq.iter().filter(|&&m| m.bits() == quiet_mv.bits()).count();
        assert_eq!(
            count, 1,
            "H1-S8: move used as both TT and killer0 must appear exactly once; found {count}"
        );
    }

    // -------------------------------------------------------------------
    // H1-S9 — killer1 == killer0 → killer1 not separately yielded.
    // -------------------------------------------------------------------
    #[test]
    fn stager_skips_killer1_when_equal_to_killer0() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let quiets: Vec<Move> = all.into_iter().filter(|&m| is_quiet(m)).collect();
        if quiets.is_empty() {
            return;
        }
        let k = quiets[0];

        let stager = MoveStager::new(
            &pos,
            k, // killer0
            k, // killer1 == killer0
            &HistoryTable::new(),
            0,
            None,
        );
        let seq = stager.yield_sequence();

        // k must appear exactly once.
        let count = seq.iter().filter(|&&m| m == k).count();
        assert_eq!(
            count, 1,
            "H1-S9: killer1 == killer0 → the move appears exactly once; found {count}"
        );
    }

    // -------------------------------------------------------------------
    // H1-S10 — killer1 == TT → killer1 not separately yielded.
    // -------------------------------------------------------------------
    #[test]
    fn stager_skips_killer1_when_equal_to_tt() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let quiets: Vec<Move> = all.into_iter().filter(|&m| is_quiet(m)).collect();
        if quiets.len() < 2 {
            return;
        }
        let k0 = quiets[0];
        let tt_quiet = quiets[1]; // this will be TT and also killer1

        let stager = MoveStager::new(
            &pos,
            k0,
            tt_quiet, // killer1 == TT
            &HistoryTable::new(),
            tt_quiet.bits(), // tt_move == killer1
            None,
        );
        let seq = stager.yield_sequence();

        // tt_quiet must appear exactly once.
        let count = seq.iter().filter(|&&m| m.bits() == tt_quiet.bits()).count();
        assert_eq!(
            count, 1,
            "H1-S10: move used as both TT and killer1 must appear exactly once; found {count}"
        );
    }

    // -------------------------------------------------------------------
    // H1-S11 — quiet stage has TT, k0, k1 deduped.
    //           Uses a non-first quiet as TT (quiets[2]) so TT promotion is
    //           exercised rather than the naturally-first slot.
    // -------------------------------------------------------------------
    #[test]
    fn stager_yields_quiets_history_desc_with_tt_killer_dedup() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let quiets: Vec<Move> = all.into_iter().filter(|&m| is_quiet(m)).collect();
        if quiets.len() < 3 {
            return;
        }
        // tt_q is quiets[2] (non-first) to ensure TT promotion is exercised;
        // k0 and k1 are quiets[0] and quiets[1].
        let (k0_q, k1_q, tt_q) = (quiets[0], quiets[1], quiets[2]);

        let stager = MoveStager::new(&pos, k0_q, k1_q, &HistoryTable::new(), tt_q.bits(), None);
        let seq = stager.yield_sequence();

        // Each of the three moves appears exactly once in the full sequence.
        for (name, mv) in [("TT", tt_q), ("k0", k0_q), ("k1", k1_q)] {
            let count = seq.iter().filter(|&&m| m == mv).count();
            assert_eq!(
                count, 1,
                "H1-S11: {name} move must appear exactly once; found {count}"
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-S12 — total_len matches legal-move count when no filter.
    // -------------------------------------------------------------------
    #[test]
    fn stager_total_len_matches_legal_count() {
        let pos = Position::starting_position();
        let legal_count = legal_moves(&pos).len();
        let stager = plain_stager(&pos);
        assert_eq!(
            stager.len(),
            legal_count,
            "H1-S12: len() must equal the legal-move count ({legal_count})"
        );
    }

    // -------------------------------------------------------------------
    // H1-S13 — peek() is idempotent under repeated calls; next() returns same.
    //           Pins idempotency at TWO stage positions: pre-iteration and
    //           mid-iteration.  The SE block calls peek() twice per M5.G
    //           design (once in the gate predicate, once in the verification
    //           excluded_move argument) so mid-flow stability is load-bearing.
    // -------------------------------------------------------------------
    #[test]
    fn stager_peek_idempotent_under_repeated_calls() {
        let pos = Position::starting_position();
        let history = HistoryTable::new();
        let mut stager = MoveStager::new(&pos, Move::default(), Move::default(), &history, 0, None);

        // (a) Pre-iteration: peek must be Some (starting position has 20 legal moves).
        let p0 = stager.peek();
        assert!(
            p0.is_some(),
            "H1-S13: starting position has 20 legal moves; pre-iteration peek must be Some"
        );
        assert_eq!(
            stager.peek(),
            p0,
            "H1-S13: peek call 2 must return same as call 1"
        );
        assert_eq!(
            stager.peek(),
            p0,
            "H1-S13: peek call 3 must return same as call 1"
        );

        // (b) Pre-iteration: next() yields what peek() reported.
        let yielded = stager.next();
        assert_eq!(
            yielded, p0,
            "H1-S13: first next() must yield what peek() reported"
        );

        // (c) Mid-iteration: after one next(), peek must still be Some and stable.
        //     19 moves remain after yielding one from a 20-move position.
        let p1 = stager.peek();
        assert!(
            p1.is_some(),
            "H1-S13: after first next(), 19 moves remain; mid-iteration peek must be Some"
        );
        assert_eq!(
            stager.peek(),
            p1,
            "H1-S13: mid-iteration peek call 2 must equal call 1"
        );
        assert_eq!(
            stager.peek(),
            p1,
            "H1-S13: mid-iteration peek call 3 must equal call 1"
        );

        // (d) Mid-iteration: next() yields what the mid-iteration peek reported.
        let yielded2 = stager.next();
        assert_eq!(
            yielded2, p1,
            "H1-S13: second next() must yield what mid-iteration peek() reported"
        );
    }

    // -------------------------------------------------------------------
    // H1-S13b — peek() at Stage::Captures returns the next capture without
    //           advancing.  H1-S13 only exercises peek at Stage::Tt and
    //           Stage::Quiets (starting position has no captures); this test
    //           specifically lands peek() at Stage::Captures so the helper
    //           `peek_from_captures` is on the call path.
    //
    //           Mutation discrimination: kills the
    //           `replace < with > in MoveStager::peek_from_captures`
    //           mutant (`if self.cap_idx < self.captures.len()` arm).
    // -------------------------------------------------------------------
    #[test]
    fn stager_peek_at_captures_stage_returns_next_capture() {
        let pos = Position::from_fen(
            // Kiwipete — many captures.
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("Kiwipete FEN must parse");
        let mut stager = plain_stager(&pos);

        // Consume the Tt stage (no TT move; first next() advances to Captures
        // and yields the first capture).
        let first_yield = stager.next();
        assert!(
            first_yield.is_some(),
            "Kiwipete must yield at least one move"
        );
        // The first yielded move must be a capture or promo (since no TT was
        // set, the first stage with content is Captures).  If for some reason
        // the position had no captures the test would degenerate; assert.
        assert!(
            !is_quiet(first_yield.unwrap()),
            "Kiwipete fixture must have captures; first non-TT yield must be a capture"
        );

        // Now stager.stage == Stage::Captures with cap_idx > 0.  peek() routes
        // through peek_from_captures and must return the SECOND capture (the
        // value `next()` would yield next).
        let peeked_a = stager.peek();
        let peeked_b = stager.peek();
        let peeked_c = stager.peek();
        assert!(
            peeked_a.is_some(),
            "after consuming first capture, more captures remain in Kiwipete; peek must be Some"
        );
        assert_eq!(
            peeked_b, peeked_a,
            "peek call 2 at Captures must equal call 1"
        );
        assert_eq!(
            peeked_c, peeked_a,
            "peek call 3 at Captures must equal call 1"
        );

        // The yielded next move must equal what peek reported.
        let next_yield = stager.next();
        assert_eq!(
            next_yield, peeked_a,
            "next() at Captures must yield the move peek_from_captures reported"
        );
    }

    // -------------------------------------------------------------------
    // H1-S14 — peek() returns None after full iteration.
    // -------------------------------------------------------------------
    #[test]
    fn stager_peek_after_done_returns_none() {
        let pos = Position::starting_position();
        let mut stager = plain_stager(&pos);
        // Drain all moves.
        while stager.next().is_some() {}
        assert_eq!(
            stager.peek(),
            None,
            "H1-S14: peek() must return None after full iteration"
        );
    }

    // -------------------------------------------------------------------
    // H1-S15a — searchmoves filter constrains yield set.
    // -------------------------------------------------------------------
    #[test]
    fn stager_searchmoves_filter_constrains_yield_set() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        if all.len() < 2 {
            return;
        }
        let filter = &all[..2];
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
            Some(filter),
        );
        let seq = stager.yield_sequence();
        for mv in &seq {
            assert!(
                filter.iter().any(|f| f == mv),
                "H1-S15a: every yielded move must be in the filter; {:?} is not",
                mv
            );
        }
        assert_eq!(
            stager.len(),
            filter.len(),
            "H1-S15a: len() must equal filter length when all filter moves are legal"
        );
    }

    // -------------------------------------------------------------------
    // H1-S15b — filter excluding TT bits → no TT stage; non-TT moves still yielded.
    // -------------------------------------------------------------------
    #[test]
    fn stager_searchmoves_filter_excluding_tt_yields_no_tt_stage() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        if all.len() < 2 {
            return;
        }
        // TT is all[0]; filter is all[1..] (excludes TT).
        let tt_mv = all[0];
        let filter: Vec<Move> = all[1..].to_vec();
        let expected_count = filter.len();

        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            tt_mv.bits(),
            Some(&filter),
        );
        let seq = stager.yield_sequence();

        // Negative: TT bits must not appear.
        assert!(
            seq.iter().all(|m| m.bits() != tt_mv.bits()),
            "H1-S15b: TT move excluded by filter must not appear in yield sequence"
        );
        // Positive: all non-TT legal moves must still be yielded.
        assert_eq!(
            seq.len(),
            expected_count,
            "H1-S15b: stager must yield all {expected_count} non-TT legal moves; \
             got {} — filter must not suppress non-TT moves",
            seq.len()
        );
    }

    // -------------------------------------------------------------------
    // H1-S15c — filter excluding killer0 → no killer0 stage; other moves yielded.
    // -------------------------------------------------------------------
    #[test]
    fn stager_searchmoves_filter_excluding_killer_yields_no_killer_stage() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let quiets: Vec<Move> = all.iter().copied().filter(|&m| is_quiet(m)).collect();
        if quiets.is_empty() || all.len() < 2 {
            return;
        }
        let k0 = quiets[0];
        // Filter excludes k0; all other legal moves are included.
        let filter: Vec<Move> = all.iter().copied().filter(|&m| m != k0).collect();
        if filter.is_empty() {
            return;
        }
        let expected_count = filter.len();

        let stager = MoveStager::new(
            &pos,
            k0,
            Move::default(),
            &HistoryTable::new(),
            0,
            Some(&filter),
        );
        let seq = stager.yield_sequence();

        // Negative: killer0 must not appear.
        assert!(
            seq.iter().all(|&m| m != k0),
            "H1-S15c: killer0 excluded by filter must not appear in yield sequence"
        );
        // Positive: all non-k0 legal moves must still be yielded.
        assert_eq!(
            seq.len(),
            expected_count,
            "H1-S15c: stager must yield all {expected_count} non-killer0 legal moves; \
             got {} — filter must not suppress non-killer moves",
            seq.len()
        );
    }

    // -------------------------------------------------------------------
    // H1-S15d — Some(&[]) filter → empty yield.
    // -------------------------------------------------------------------
    #[test]
    fn stager_searchmoves_filter_empty_yields_nothing() {
        let pos = Position::starting_position();
        let mut stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
            Some(&[]), // empty filter
        );
        assert_eq!(stager.len(), 0, "H1-S15d: empty filter → len == 0");
        assert!(stager.is_empty(), "H1-S15d: empty filter → is_empty");
        assert_eq!(
            stager.next(),
            None,
            "H1-S15d: empty filter → next() == None"
        );
    }

    // -------------------------------------------------------------------
    // H1-S15e — None filter is identical to no-filter.
    // -------------------------------------------------------------------
    #[test]
    fn stager_searchmoves_filter_none_is_no_op() {
        let pos = Position::starting_position();
        let legal_count = legal_moves(&pos).len();
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
            None,
        );
        assert_eq!(
            stager.len(),
            legal_count,
            "H1-S15e: None filter → len equals full legal-move count ({legal_count})"
        );
    }

    // -------------------------------------------------------------------
    // H1-S16 — total_len reflects filtered count.
    // -------------------------------------------------------------------
    #[test]
    fn stager_total_len_post_filter_correct() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        if all.len() < 3 {
            return;
        }
        let filter = &all[..3];
        let stager = MoveStager::new(
            &pos,
            Move::default(),
            Move::default(),
            &HistoryTable::new(),
            0,
            Some(filter),
        );
        assert_eq!(
            stager.len(),
            3,
            "H1-S16: three-move filter → len == 3; got {}",
            stager.len()
        );
    }

    // -------------------------------------------------------------------
    // H1-S17 — total_len is unchanged after full iteration (temporal contract).
    //           Uses legal_moves().len() as the absolute reference so the test
    //           catches both "wrong initial len" and "len decrements on next()"
    //           bugs independently.
    // -------------------------------------------------------------------
    #[test]
    fn stager_done_after_full_iteration_total_len_unchanged() {
        let pos = Position::starting_position();
        // Absolute reference: the real legal-move count (20 for starting position).
        let expected_len = legal_moves(&pos).len();
        let history = HistoryTable::new();
        let mut stager = MoveStager::new(&pos, Move::default(), Move::default(), &history, 0, None);

        // Pre-iteration: len must equal the legal-move count.
        assert_eq!(
            stager.len(),
            expected_len,
            "H1-S17: pre-iteration len must equal legal-move count ({expected_len})"
        );

        // Drain all moves.
        let mut yielded = 0usize;
        while stager.next().is_some() {
            yielded += 1;
        }

        // Post-iteration: next() returns None.
        assert_eq!(
            stager.next(),
            None,
            "H1-S17: next() after exhaustion must return None"
        );
        // Temporal contract: len() must NOT have decremented.
        assert_eq!(
            stager.len(),
            expected_len,
            "H1-S17: len() after full iteration must be unchanged (temporal contract; \
             must not decrement on next()); expected {expected_len}, got {}",
            stager.len()
        );
        // All moves must have been yielded.
        assert_eq!(
            yielded, expected_len,
            "H1-S17: yielded move count must equal pre-iteration len; \
             yielded {yielded} but legal count is {expected_len}"
        );
    }

    // -------------------------------------------------------------------
    // H1-S18 — capture in killer slot is not yielded as a killer
    //           (defensive: ADR-0019 guarantees killers are quiets, but the
    //           stager guards against bad caller inputs).
    // -------------------------------------------------------------------
    #[test]
    fn stager_killer_with_capture_flag_not_yielded_as_killer() {
        // Use kiwipete — has captures.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete FEN must parse");
        let all = legal_moves(&pos);
        // Find a capture to pass as killer0 (adversarial: violates ADR-0019).
        let Some(cap) = all.iter().find(|&&m| !is_quiet(m)).copied() else {
            return;
        };

        let stager = MoveStager::new(
            &pos,
            cap, // capture in killer0 slot — should NOT be yielded as killer
            Move::default(),
            &HistoryTable::new(),
            0,
            None,
        );
        let seq = stager.yield_sequence();

        // The capture must appear exactly once (in the capture stage, not again as k0).
        let count = seq.iter().filter(|&&m| m == cap).count();
        assert_eq!(
            count, 1,
            "H1-S18: capture in killer slot must appear at most once (in capture stage); \
             found {count} occurrences"
        );
    }

    // -------------------------------------------------------------------
    // H1-S19 — killer0.bits() == 0 (Move::default sentinel) is ignored.
    // -------------------------------------------------------------------
    #[test]
    fn stager_killer_with_default_sentinel_is_ignored() {
        let pos = Position::starting_position();
        let legal_count = legal_moves(&pos).len();
        let stager = MoveStager::new(
            &pos,
            Move::default(), // bits == 0 → sentinel; must be ignored, not scanned
            Move::default(),
            &HistoryTable::new(),
            0,
            None,
        );
        // Sentinel ignored: no crash, no extra yields, len unchanged.
        assert_eq!(
            stager.len(),
            legal_count,
            "H1-S19: default killer sentinel must be ignored; \
             len must equal legal-move count ({legal_count})"
        );
    }

    // -------------------------------------------------------------------
    // H1-H1 — extract_move_by_bits(v, 0) returns None without scanning.
    // -------------------------------------------------------------------
    #[test]
    fn extract_move_by_bits_zero_returns_none_without_scanning() {
        let pos = Position::starting_position();
        let mut v = legal_moves(&pos);
        let original_len = v.len();
        let result = extract_move_by_bits(&mut v, 0);
        assert_eq!(result, None, "H1-H1: target == 0 must return None");
        assert_eq!(
            v.len(),
            original_len,
            "H1-H1: Vec must be unchanged when target == 0"
        );
    }

    // -------------------------------------------------------------------
    // H1-H2 — extract_move_by_bits finds first match, order-preserving.
    // -------------------------------------------------------------------
    #[test]
    fn extract_move_by_bits_finds_first_match_and_removes_in_place() {
        let pos = Position::starting_position();
        let mut v = legal_moves(&pos);
        assert!(!v.is_empty());
        let target = v[0];
        let original_rest: Vec<Move> = v[1..].to_vec();

        let result = extract_move_by_bits(&mut v, target.bits());

        assert_eq!(
            result.map(|m| m.bits()),
            Some(target.bits()),
            "H1-H2: must return the found move"
        );
        assert_eq!(
            v, original_rest,
            "H1-H2: remaining elements must be in original order (order-preserving removal)"
        );
    }

    // -------------------------------------------------------------------
    // H1-H3 — extract_move_by_bits returns None when target absent.
    // -------------------------------------------------------------------
    #[test]
    fn extract_move_by_bits_returns_none_when_target_absent() {
        let pos = Position::starting_position();
        let mut v = legal_moves(&pos);
        let original = v.clone();
        // 0xFFFF is not a valid legal move.
        let result = extract_move_by_bits(&mut v, 0xFFFF);
        assert_eq!(result, None, "H1-H3: absent target → None");
        assert_eq!(v, original, "H1-H3: Vec must be unchanged on miss");
    }

    // -------------------------------------------------------------------
    // H1-H4 — extract_move_by_eq matches full Move (flag bits included).
    // -------------------------------------------------------------------
    #[test]
    fn extract_move_by_eq_finds_full_move_match() {
        use crate::mov::MoveFlag;
        use crate::square::Square;

        // Build two Moves with the same (from, to) but different flags.
        let quiet = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        let fake_capture = Move::new(Square::E2, Square::E4, MoveFlag::Capture);

        // Full-Move equality on `quiet` must succeed.
        let result = extract_move_by_eq(&mut vec![quiet], quiet);
        assert_eq!(result, Some(quiet), "H1-H4: must find exact-flag match");

        // A different flag (same from/to) on a Vec containing only `quiet`
        // must return None — bits-only match is insufficient.
        let result_miss = extract_move_by_eq(&mut vec![quiet], fake_capture);
        assert_eq!(
            result_miss, None,
            "H1-H4: bits-only match insufficient — flag mismatch must return None"
        );
    }

    // -------------------------------------------------------------------
    // H1-H5 — partition_captures_quiets partitions by is_quiet.
    // -------------------------------------------------------------------
    #[test]
    fn partition_captures_quiets_partitions_by_is_quiet() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete must parse");
        let all = legal_moves(&pos);
        let (captures, quiets) = partition_captures_quiets(all.clone());
        assert!(
            captures.iter().all(|&m| !is_quiet(m)),
            "H1-H5: all 'captures' must be non-quiet"
        );
        assert!(
            quiets.iter().all(|&m| is_quiet(m)),
            "H1-H5: all 'quiets' must be quiet"
        );
        assert_eq!(
            captures.len() + quiets.len(),
            all.len(),
            "H1-H5: capture + quiet count must equal total move count"
        );
    }

    // -------------------------------------------------------------------
    // H1-H6 — partition preserves within-partition order.
    // -------------------------------------------------------------------
    #[test]
    fn partition_preserves_within_partition_order() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete must parse");
        let all = legal_moves(&pos);
        // Collect expected order by manually filtering.
        let expected_caps: Vec<Move> = all.iter().copied().filter(|&m| !is_quiet(m)).collect();
        let expected_quiets: Vec<Move> = all.iter().copied().filter(|&m| is_quiet(m)).collect();

        let (caps, quiets) = partition_captures_quiets(all);
        assert_eq!(
            caps, expected_caps,
            "H1-H6: capture partition must preserve movegen-emit order"
        );
        assert_eq!(
            quiets, expected_quiets,
            "H1-H6: quiet partition must preserve movegen-emit order"
        );
    }

    // -------------------------------------------------------------------
    // H1-H7 — mvv_lva_sort_in_place produces non-increasing scores.
    // -------------------------------------------------------------------
    #[test]
    fn mvv_lva_sort_in_place_descending() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete must parse");
        let all = legal_moves(&pos);
        let mut captures: Vec<Move> = all.into_iter().filter(|&m| !is_quiet(m)).collect();
        if captures.len() < 2 {
            return;
        }
        mvv_lva_sort_in_place(&mut captures, &pos);
        for window in captures.windows(2) {
            let s0 = mvv_lva_score(window[0], &pos);
            let s1 = mvv_lva_score(window[1], &pos);
            assert!(
                s0 >= s1,
                "H1-H7: MVV-LVA sort must be non-increasing; \
                 got score({:?})={s0} > score({:?})={s1}",
                window[0],
                window[1]
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-H8 — mvv_lva_sort_in_place stable within ties (movegen order preserved).
    // -------------------------------------------------------------------
    #[test]
    fn mvv_lva_sort_in_place_stable_within_ties() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("kiwipete must parse");
        let all = legal_moves(&pos);
        let mut captures: Vec<Move> = all.into_iter().filter(|&m| !is_quiet(m)).collect();

        // Use sort_by_cached_key (stable) as the reference.
        let mut reference = captures.clone();
        reference.sort_by_cached_key(|&m| -mvv_lva_score(m, &pos));

        mvv_lva_sort_in_place(&mut captures, &pos);
        assert_eq!(
            captures, reference,
            "H1-H8: mvv_lva_sort_in_place must produce the same order as \
             sort_by_cached_key(-mvv_lva_score) (stable within ties)"
        );
    }

    // -------------------------------------------------------------------
    // H1-H9 — history_sort_in_place produces non-increasing history scores.
    // -------------------------------------------------------------------
    #[test]
    fn history_sort_in_place_descending() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let mut quiets: Vec<Move> = all.into_iter().filter(|&m| is_quiet(m)).collect();
        if quiets.len() < 2 {
            return;
        }
        // Seed the history table with distinct values so there's a real ordering.
        let mut ht = HistoryTable::new();
        let stm = pos.side_to_move();
        for (i, &mv) in quiets.iter().enumerate() {
            ht.update(stm, mv.from_square(), mv.to_square(), (i as i32 + 1) * 10);
        }
        history_sort_in_place(&mut quiets, &pos, &ht);
        for window in quiets.windows(2) {
            let s0 = ht.score(stm, window[0].from_square(), window[0].to_square()) as i32;
            let s1 = ht.score(stm, window[1].from_square(), window[1].to_square()) as i32;
            assert!(
                s0 >= s1,
                "H1-H9: history sort must be non-increasing; \
                 got score({:?})={s0} < score({:?})={s1}",
                window[0],
                window[1]
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-H10 — history_sort_in_place stable within ties.
    // -------------------------------------------------------------------
    #[test]
    fn history_sort_in_place_stable_within_ties() {
        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        let mut quiets: Vec<Move> = all.into_iter().filter(|&m| is_quiet(m)).collect();
        let ht = HistoryTable::new(); // all zeros → all tied

        // Reference: stable sort by history score (all equal → original order).
        let reference = quiets.clone();

        history_sort_in_place(&mut quiets, &pos, &ht);
        assert_eq!(
            quiets, reference,
            "H1-H10: equal history scores → original order preserved (stable sort)"
        );
    }

    // -------------------------------------------------------------------
    // H1-I1 — negamax with stager at terminal position returns terminal score.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_at_terminal_position_returns_terminal_score() {
        // Stalemate: "7k/5Q2/6K1/8/8/8/8/8 b - - 0 1" — Black has no legal moves.
        let pos =
            Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").expect("stalemate FEN must parse");
        let mut ab = AlphaBetaMover::new();
        let (ctx, _) = non_aborting_ctx();
        // Drive at depth 1: no legal moves → returns 0 (stalemate).
        let score = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, true, None, &ctx);
        assert_eq!(score, 0, "H1-I1: stalemate must return 0; got {score}");

        // Mate-in-1: "4k3/4Q3/3K4/8/8/8/8/8 b - - 0 1" — Black is in check with no evasions.
        let pos_mate =
            Position::from_fen("4k3/4Q3/3K4/8/8/8/8/8 b - - 0 1").expect("mate FEN must parse");
        let score_mate = ab.negamax_for_test(
            &mut pos_mate.clone(),
            1,
            0,
            -INF,
            INF,
            true,
            true,
            None,
            &ctx,
        );
        assert!(
            score_mate < -MATE_IN_MAX_PLY,
            "H1-I1: mate score must be below -MATE_IN_MAX_PLY threshold; got {score_mate}"
        );
    }

    // -------------------------------------------------------------------
    // H1-I2 — searchmoves filter constrains the root move loop.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_searchmoves_filter_constrains_to_filter_at_root() {
        use crate::search::SearchLimits;

        let pos = Position::starting_position();
        let all = legal_moves(&pos);
        // Let the filter contain exactly one move: the first legal move.
        let filter_move = all[0];
        let filter = vec![filter_move];

        let (mut ctx, stop) = non_aborting_ctx();
        ctx.limits = SearchLimits {
            searchmoves: Some(filter),
            depth: Some(2),
            ..SearchLimits::default()
        };

        let mut ab = AlphaBetaMover::new();
        // Drive at ply 0 with the filter active.
        let _score = ab.negamax_for_test(&mut pos.clone(), 2, 0, -INF, INF, true, true, None, &ctx);
        drop(stop);

        // The PV root move must be the only filtered move (the search had no
        // other option at ply 0).
        let pv = ab.pv_root_for_test();
        if !pv.is_empty() {
            assert_eq!(
                pv[0].bits(),
                filter_move.bits(),
                "H1-I2: root move must be the only filtered move"
            );
        }
    }

    // -------------------------------------------------------------------
    // H1-I3 — SE block fires when stager.peek() matches the TT move.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_se_peek_finds_tt_move_at_front() {
        // Reuse the SE test fixture from the M5.G suite.
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false, // non-PV so SE can fire
            true,
            None,
            &ctx,
        );

        // SE should fire (or at least not be blocked by the peek() path).
        // H1-I3 just pins that SE extensions == expected from M5.G fixtures.
        // After M5.H1 refactor the stager's peek() replaces moves_vec[0].
        let se_ext = ab.se_extensions_for_test();
        assert!(
            se_ext >= 1,
            "H1-I3: SE must fire at least once for the SE fixture position at \
             depth={depth}, ply={ply}; got se_extensions={se_ext}"
        );
    }

    // -------------------------------------------------------------------
    // H1-I4 — excluded-move skip preserves position counter semantics.
    // -------------------------------------------------------------------
    // Note: the cur_i off-by-one mutation (let mut i = 0 → 1, or cur_i/i++
    // swap) is caught primarily by H1-B1 (bench-parity gate) — any deviation
    // from M5.G's position-counter semantics changes node counts.  This test
    // pins the SE counter; H1-B1 pins the depth-7 node-count signature.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_excluded_move_skip_preserves_position_counter() {
        // We can't directly observe cur_i from outside, but we can verify that
        // the SE extension fires (SE extension requires cur_i == 0 for the TT
        // move) even after the verification frame skips the excluded move.
        // Reuse the SE test fixture.
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false, // non-PV
            true,
            None,
            &ctx,
        );

        // SE must have fired: the extension was applied at cur_i == 0 (the TT
        // move position).  If cur_i were off-by-one (started at 1), the
        // extension check `if cur_i == 0` would never fire for the TT move and
        // SE extensions would be 0.
        let se_ext = ab.se_extensions_for_test();
        assert!(
            se_ext >= 1,
            "H1-I4: SE extension must fire (cur_i == 0 for TT move); got {se_ext}.\n\
             An off-by-one in the position counter would suppress this."
        );
    }

    // -------------------------------------------------------------------
    // H1-I5 — searchmoves filter excluding TT: search completes, chosen root
    //          move is within the filter set.
    //
    // Note: SE cannot fire at ply == 0 (clause 2 of singular_extension_eligible
    // requires ply > 0).  So asserting se_extensions == 0 at root would be
    // vacuous — the test instead pins that the filter is correctly applied:
    // the chosen root move must be a member of the filter (i.e., not the TT
    // move that was excluded).  Per plan §4.1 Case 8.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_searchmoves_excluding_tt_does_not_promote() {
        use crate::search::SearchLimits;

        // Use the SE test fixture position.
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;

        let tt = Arc::new(TranspositionTable::new(16));

        let best_move_bits = se_find_best_move_bits(&pos);
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: best_move_bits,
            },
        );

        // Build a searchmoves filter that EXCLUDES the TT move.
        let all = legal_moves(&pos);
        let filter: Vec<Move> = all
            .iter()
            .copied()
            .filter(|m| m.bits() != best_move_bits)
            .collect();
        if filter.is_empty() {
            return; // only one legal move — test is vacuous
        }

        let (mut ctx, _stop) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));
        ctx.limits = SearchLimits {
            searchmoves: Some(filter.clone()),
            depth: Some(depth),
            ..SearchLimits::default()
        };

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            0, // ply == 0, searchmoves filter active
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // The search must complete and select a move that is in the filter.
        // A TT move outside the filter cannot be chosen as the best root move.
        let pv = ab.pv_root_for_test();
        assert!(
            !pv.is_empty(),
            "H1-I5: search with non-empty searchmoves filter must produce a root move"
        );
        assert!(
            filter.iter().any(|f| f.bits() == pv[0].bits()),
            "H1-I5: chosen root move must be in the searchmoves filter (TT move excluded); \
             got pv[0].bits()={:#06x}, filter has {} moves",
            pv[0].bits(),
            filter.len()
        );
    }

    // -------------------------------------------------------------------
    // H1-I6 — stale TT (bits not in legal list) does not fire SE.
    // -------------------------------------------------------------------
    #[test]
    fn negamax_with_stager_stale_tt_does_not_fire_se() {
        let pos = se_test_pos();
        let depth = SE_MIN_DEPTH;
        let ply = 1_u32;

        let tt = Arc::new(TranspositionTable::new(16));
        let (ctx, _) = non_aborting_ctx_at_depth_with_tt(depth + 2, Arc::clone(&tt));

        // Store a TT entry with bits that don't correspond to any legal move.
        // 0xFFFF is not a valid move encoding for any legal move.
        let stale_bits: u16 = 0xFFFF;
        tt.new_search();
        tt.store(
            pos.zobrist(),
            TtData {
                score: 50_i16,
                depth: depth as u8,
                bound: TtBound::Lower,
                best_move: stale_bits,
            },
        );

        let mut ab = AlphaBetaMover::new();
        ab.set_tt_for_test(Some(Arc::clone(&tt)));
        let _ = ab.negamax_for_test(
            &mut pos.clone(),
            depth,
            ply,
            -INF,
            INF,
            false,
            true,
            None,
            &ctx,
        );

        // Stale TT bits not in the legal list → stager.peek().bits() won't match
        // snapshot.tt_move → SE predicate fails → no extension.
        assert_eq!(
            ab.se_extensions_for_test(),
            0,
            "H1-I6: stale TT move (bits not in legal list) must not fire SE; \
             got se_extensions={}",
            ab.se_extensions_for_test()
        );
    }

    // -------------------------------------------------------------------
    // H1-B2 — E51 depth-4 pin (unchanged from M5.F / M5.G landing).
    // Renamed from bench_signature_deterministic_across_two_runs_with_qsearch_tt
    // per plan §8 H1-B2.  The H1-B1 bench CLI pin (1147614 nodes at depth 7)
    // is verified via `cargo run --release -- bench` at the §12 verification
    // gate, not as an in-process test.
    // -------------------------------------------------------------------
    // (H1-B2 is the existing E51 function in tests/uci_integration.rs —
    //  name unchanged per plan §8 clarification; no rename needed here.)

    // -------------------------------------------------------------------
    // H1-P1 — Equivalence proptest: MoveStager::yield_sequence == order_moves.
    // -------------------------------------------------------------------
    mod h1_proptest {
        use super::*;
        use crate::history::{HistoryTable, MAX_HISTORY};
        use crate::movegen::test_strategies::arb_position;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            /// For arbitrary positions, TT bits, killer slots, and history
            /// tables, `MoveStager::yield_sequence()` must be byte-equivalent
            /// to the output of `order_moves()` on the same inputs.
            ///
            /// This is the load-bearing bench-parity gate: any divergence
            /// between the stager and `order_moves` indicates a bug that
            /// would change node counts and invalidate bench parity.
            #[test]
            fn prop_stager_yield_sequence_equals_order_moves_output(
                pos in arb_position(),
                tt_kind in 0u8..3,
                tt_seed in any::<u64>(),
                killer0_kind in 0u8..3,
                killer0_seed in any::<u64>(),
                killer1_kind in 0u8..3,
                killer1_seed in any::<u64>(),
                history_seed in any::<u64>(),
            ) {
                use crate::movegen::{MoveList, generate_moves};

                // Build the full legal-move list.
                let mut ml = MoveList::new();
                generate_moves(&pos, &mut ml);
                let all_moves: Vec<Move> = ml.iter().collect();

                if all_moves.is_empty() {
                    // Terminal position — both stager and order_moves produce empty.
                    return Ok(());
                }

                // Derive tt_move bits from tt_kind.
                let tt_move_bits: u16 = match tt_kind % 3 {
                    0 => 0, // no TT
                    1 => {
                        // TT is a real legal move (in-list).
                        let idx = (tt_seed as usize) % all_moves.len();
                        all_moves[idx].bits()
                    }
                    _ => {
                        // TT is a stale/absent value (not in list).
                        // Pick from u16 range but ensure it's not accidentally
                        // a real move by XOR-scrambling with a magic constant.
                        let raw = ((tt_seed >> 16) as u16) ^ 0xABCD;
                        // If this accidentally matches a legal move, fall back to 0.
                        if all_moves.iter().any(|m| m.bits() == raw) { 0 } else { raw }
                    }
                };

                // Derive killer0 from killer0_kind.
                let quiet_moves: Vec<Move> = all_moves.iter().copied().filter(|&m| is_quiet(m)).collect();
                let capture_moves: Vec<Move> = all_moves.iter().copied().filter(|&m| !is_quiet(m)).collect();
                let killer0: Move = match (killer0_kind % 3, quiet_moves.is_empty()) {
                    (0, _) | (_, true) => Move::default(), // sentinel
                    (1, false) => {
                        // real legal quiet
                        quiet_moves[(killer0_seed as usize) % quiet_moves.len()]
                    }
                    _ => {
                        // A non-quiet (capture) in the killer slot — tests the
                        // defensive path that filters out non-quiet killers.
                        if capture_moves.is_empty() {
                            Move::default()
                        } else {
                            capture_moves[(killer0_seed as usize) % capture_moves.len()]
                        }
                    }
                };

                // Derive killer1 from killer1_kind (must differ from killer0 for
                // the non-dedup path to be interesting).
                let killer1: Move = match (killer1_kind % 3, quiet_moves.is_empty()) {
                    (0, _) | (_, true) => Move::default(),
                    (1, false) => {
                        let idx = (killer1_seed as usize) % quiet_moves.len();
                        quiet_moves[idx]
                    }
                    _ => {
                        // Non-quiet in killer1 slot — defensive path exercise.
                        if capture_moves.is_empty() {
                            Move::default()
                        } else {
                            capture_moves[(killer1_seed as usize) % capture_moves.len()]
                        }
                    }
                };

                // Build a randomised history table.
                let mut ht = HistoryTable::new();
                {
                    let stm = pos.side_to_move();
                    let mut rng = history_seed;
                    for &mv in &quiet_moves {
                        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        let score = ((rng >> 48) as i32 % (MAX_HISTORY as i32 * 2 + 1)) - MAX_HISTORY as i32;
                        ht.update(stm, mv.from_square(), mv.to_square(), score);
                    }
                }

                // --- Reference: order_moves on a cloned Vec ---
                let mut reference_vec = all_moves.clone();
                order_moves(
                    &mut reference_vec,
                    &pos,
                    killer0,
                    killer1,
                    &ht,
                    tt_move_bits,
                );

                // --- Stager: yield_sequence on the same inputs ---
                let stager = MoveStager::new(
                    &pos,
                    killer0,
                    killer1,
                    &ht,
                    tt_move_bits,
                    None, // no searchmoves filter (not root)
                );
                let stager_seq = stager.yield_sequence();

                // --- Assert byte-equivalent sequences ---
                let stager_bits: Vec<u16> = stager_seq.iter().map(|m| m.bits()).collect();
                let ref_bits: Vec<u16> = reference_vec.iter().map(|m| m.bits()).collect();
                proptest::prop_assert!(
                    stager_bits == ref_bits,
                    "{}",
                    format!(
                        "MoveStager::yield_sequence must be byte-equivalent to order_moves \
                         output (bench-parity gate). \
                         tt_move_bits={tt_move_bits}, \
                         killer0.bits()={}, killer1.bits()={}, \
                         position has {} legal moves;\n\
                         stager:    {:?}\n\
                         reference: {:?}",
                        killer0.bits(), killer1.bits(), all_moves.len(),
                        stager_bits, ref_bits,
                    )
                );
            }
        }
    }
}
