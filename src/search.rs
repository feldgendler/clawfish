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

mod ordering;
mod params;
mod qsearch;
mod time;

// Re-exports from `time` — bring the clock/caps types into the search namespace
// so that `SearchContext` (which holds `TimeCaps`) and all callers can reference
// them without a `time::` prefix, and so `use super::*` in `tests` resolves them.
pub use time::{SearchClock, SearchInstant};
pub(crate) use time::{TimeCaps, compute_caps, max_depth_from_limits};

// Re-exports from `params` — tuning constants and pure heuristic formulas.
// Split into root-body uses vs. test-only consumers so a future genuinely-dead
// re-export warns instead of being absorbed by a blanket `#[allow]`.
pub use params::AspirationParams;
pub(crate) use params::{
    ASPIRATION_MIN_DEPTH, FFP_MAX_DEPTH, INF, LMR_HIGH_HISTORY_THRESHOLD, LMR_MIN_DEPTH,
    LMR_MIN_QUIET_INDEX, MATE, MATE_IN_MAX_PLY, MAX_PLY, NMP_MIN_DEPTH, RFP_MAX_DEPTH,
    SE_MIN_DEPTH, SE_TT_DEPTH_DELTA, aspiration_window, ffp_pruned_bound, late_move_reduction,
    null_move_reduction, reverse_futility_margin, singular_beta, verification_depth,
    widen_after_fail,
};
// Consumed only by `tests` (via `use super::*`), not the root body or other
// modules. (The remaining ASPIRATION_*/FFP_MARGIN_*/NMP_*/LMR_* defaults are
// used only inside params.rs itself, so they are NOT re-exported here.)
#[cfg(test)]
pub(crate) use params::{
    ASPIRATION_HALF_WIDTH, SE_MARGIN_PER_DEPTH, aspiration_half_width, frontier_futility_margin,
};

// Re-exports from `ordering` — move ordering, killers, history helpers, MoveStager.
pub(crate) use ordering::{
    MoveStager, clear_killers, is_quiet, mvv_lva_score, qsearch_move_filter,
    stalemate_avoiding_under_promos, update_history_on_quiet_cutoff, update_killers,
};
// Score-tier consts (consumed by `tests` + the compile-time invariant in
// ordering.rs, not the root body) and the test-only ordering/sort helpers.
#[cfg(test)]
pub(crate) use ordering::{
    CAPTURE_OFFSET, KILLER0_SCORE, KILLER1_SCORE, extract_move_by_bits, extract_move_by_eq,
    history_sort_in_place, mvv_lva_sort_in_place, negamax_move_order_score, order_moves,
    partition_captures_quiets,
};

// Re-exports from `qsearch` — quiescence helpers + the eval seam. The fns are
// called from AlphaBetaMover::qsearch (below); QS_SEE_RAMP_FLOOR is test-only
// (D0/SLOPE are used only inside qsearch.rs, so they are not re-exported).
#[cfg(test)]
pub(crate) use qsearch::QS_SEE_RAMP_FLOOR;
pub use qsearch::{QSearcher, quiescence_eval_white};
pub(crate) use qsearch::{
    qs_see_prune_threshold, qsearch_see_pruneable, qsearch_short_circuit_at_ply_ceiling,
    qsearch_tt_bound_for_completed_node,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::history::HistoryTable;
use crate::tt::{TranspositionTable, TtBound, TtData, score_from_tt, score_to_tt};
use crate::{Color, Move, MoveList, PieceKind, Position};

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
// (Constants and pure heuristic formulas live in the `params` submodule.)
// ---------------------------------------------------------------------------

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

/// M8.A PVS Step-3 re-search predicate: a null-window scout result is *interior*
/// to the real window — and so needs a full-window re-search to establish an
/// exact score — iff `alpha < score < beta`. Extracted as a named helper (the
/// `lmr_needs_full_research` / M3.D `negate_window` precedent) so its exact
/// boundary semantics are directly unit-testable and mutation-killable.
///
/// **Load-bearing:** `score > alpha` (not `> original_alpha`) — `alpha` is the
/// node's *current, running* alpha (raised by earlier moves), per research §5 /
/// ADR-0043 §2. Using the pre-move `original_alpha` here would trigger spurious
/// full-window re-searches on moves worse than an already-found one (extra nodes,
/// same value — invisible to the score/bestmove exactness anchors, which is why
/// this predicate is pinned directly). `score < beta` excludes fail-high results
/// (those cut without needing an exact score); the strict `<` is correct because
/// `score >= beta` is the beta-cutoff case.
fn pvs_needs_research(score: i32, alpha: i32, beta: i32) -> bool {
    score > alpha && score < beta
}

/// M8.A.1 depth-conditioned PVS ramp constants (ADR-0044). Tunable; see
/// `docs/research/m8.a.1-depth-conditioned-pvs.md` §5.
const PVS_RAMP_D0: u32 = 12; // root depth at/below which PVS is fully off (≡ M7.B.2)
const PVS_RAMP_BASE: u32 = 16; // scout-start rank one ply above D0 (scout only the deep tail)
const PVS_RAMP_SLOPE: u32 = 4; // move-ordering ranks unlocked for scouting per ply above D0

/// M8.A.1: lowest move-ordering rank (`cur_i`) eligible for the PVS null-window
/// scout at this ID *root* iteration depth (`AlphaBetaMover::root_depth`, ADR-0041
/// — constant across the negamax tree for the iteration, NOT the local node depth).
///
/// `u32::MAX` ⇒ no move is scouted ⇒ every non-first move takes the reference
/// full-window path ⇒ **byte-identical to `M7.B.2`** (the fast-TC safety guarantee).
/// Monotonically non-increasing in `root_depth`; reaches 1 (all non-first moves
/// scouted ⇒ full PVS ≡ M8.A) at `root_depth >= D1 = 16`. The off-regime ramp is
/// the depth-mirror of the M7.B.2 qsearch SEE-prune ramp (ADR-0041): PVS hurts at
/// shallow root depths (fast TC) and helps at deep ones (slow TC), so it is
/// suppressed shallow and engaged deep. See ADR-0044.
fn pvs_scout_start(root_depth: u32) -> u32 {
    if root_depth <= PVS_RAMP_D0 {
        return u32::MAX;
    }
    PVS_RAMP_BASE
        .saturating_sub(PVS_RAMP_SLOPE * (root_depth - PVS_RAMP_D0))
        .max(1)
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
    /// M8.A: per-`go` count of PVS Step-1 null-window scout pilots run on
    /// non-first moves at the `lmr_trace_root_ply` frame. Test-only
    /// instrumentation; gated by `#[cfg(test)]`. Naming follows the M5.A
    /// `nmp_firings` / M5.C `lmr_*` convention.
    #[cfg(test)]
    pvs_scout_searches: u32,
    /// M8.A: PVS Step-2 full-depth null-window verify count at the
    /// `lmr_trace_root_ply` frame (the relabeled ADR-0025 §5 LMR re-search;
    /// equals `lmr_full_researches` at the non-PV nodes where it fires).
    #[cfg(test)]
    pvs_lmr_verify_searches: u32,
    /// M8.A: PVS Step-3 full-window re-search count at the
    /// `lmr_trace_root_ply` frame (the genuinely-new PV-node re-search).
    #[cfg(test)]
    pvs_research_full_window: u32,
    /// M8.A test-only harness: when `true`, force every recursive child to
    /// `is_pv=true`, neutralizing all `is_pv`-gated prunes (NMP/RFP/FFP) and
    /// LMR throughout the subtree. Used by the all-PV exactness anchor.
    /// NOT reset by `negamax_for_test` — the test sets it before the call.
    #[cfg(test)]
    test_force_all_pv: bool,
    /// M8.A test-only harness: when `true`, search non-first children
    /// full-window at full depth (the pre-PVS reference move loop: LMR reduced
    /// pilot at the full window → full-depth full-window re-search if `> alpha`).
    /// The value reference for the exactness anchors. NOT reset by
    /// `negamax_for_test`.
    #[cfg(test)]
    test_disable_scout: bool,
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
            #[cfg(test)]
            pvs_scout_searches: 0,
            #[cfg(test)]
            pvs_lmr_verify_searches: 0,
            #[cfg(test)]
            pvs_research_full_window: 0,
            #[cfg(test)]
            test_force_all_pv: false,
            #[cfg(test)]
            test_disable_scout: false,
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
            self.pvs_scout_searches = 0;
            self.pvs_lmr_verify_searches = 0;
            self.pvs_research_full_window = 0;
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

        // 13. Recurse fail-soft via the M8.A PVS ladder (ADR-0043 §2-§4). Child
        //     PV-ness is now PER-STEP, not the old single `is_pv && i == 0` rule:
        //     the first searched move and the Step-3 full-window re-search inherit
        //     the parent's PV-ness (`pv_full_child`); null-window scouts (Step-1
        //     pilots / Step-2 verifies) are non-PV (`pv_scout_child`). The two are
        //     computed once below; see the per-step dispatch for where each lands.
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

        // M8.A — PVS test-harness flags. `false` in production (the `let`
        // bindings fold to constants under `cfg(not(test))`, so the harness
        // branches optimize away — zero production cost). See ADR-0043 §"Test
        // surface".
        #[cfg(test)]
        let test_force_all_pv = self.test_force_all_pv;
        #[cfg(not(test))]
        let test_force_all_pv = false;
        #[cfg(test)]
        let test_disable_scout = self.test_disable_scout;
        #[cfg(not(test))]
        let test_disable_scout = false;

        // M8.A — PVS child PV-ness (loop-invariant). The first searched move and
        // the Step-3 full-window re-search inherit the parent's PV-ness; scouts
        // (Step-1 pilots / Step-2 verifies) are non-PV. `test_force_all_pv`
        // forces every child PV, neutralizing is_pv-gated prunes for the
        // exactness anchor (production: `pv_full_child == is_pv`,
        // `pv_scout_child == false`).
        let pv_full_child = if test_force_all_pv { true } else { is_pv };
        let pv_scout_child = test_force_all_pv;

        // M8.A — PVS first-move tracking. The first *non-excluded* searched move
        // gets the full window at full depth (never a scout); robust to the
        // excluded-move skip at SE verification frames (where `cur_i == 0` is the
        // excluded TT move). Set `false` after the first real dispatch.
        let mut first_searched = true;

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

            // M8.A — PVS ladder. `R = lmr_reduction` (0 when LMR-ineligible).
            // The first searched move gets the full window at full depth (+ SE
            // extension); every later move is scouted with a null window and
            // re-searched only when the scout result is interior. See ADR-0043
            // §2–§4 and plan §3.2. At a non-PV node `beta == alpha+1`, so the
            // null window equals the node window and Step 3 is statically
            // unreachable — the loop degenerates to today's scout with no
            // special-casing.
            let r = lmr_reduction.unwrap_or(0);
            let is_first = first_searched;

            // M8.A.1 — depth-conditioned PVS (ADR-0044). A non-first move is
            // scouted only when its move-ordering rank `cur_i` reaches the
            // root-depth ramp's scout-start (≡ M7.B.2 below D0=12, full PVS at
            // d≥16). Non-scouted non-first moves take the reference full-window
            // path. `test_disable_scout` (test-only) forces the reference path
            // for the exactness anchors; it is `false` under `cfg(not(test))`,
            // so production scouting is governed purely by the ramp.
            let should_scout =
                !test_disable_scout && cur_i as u32 >= pvs_scout_start(self.root_depth);

            let score = if is_first {
                // First non-excluded move: full window, full depth (+ extension),
                // inherit parent PV-ness. SE depth recording (M5.G) lives here —
                // the TT move (cur_i == 0) is the first searched move unless
                // excluded, and only `cur_i == 0` carries `move_extension`.
                // `first_move_depth` is computed ONCE and used for BOTH the
                // search and the SE-test recording so a mutation of the
                // `depth - 1 + move_extension` arithmetic is caught by
                // `negamax_se_extension_increments_child_depth_by_one` (which
                // pins the recorded depth) — the two must not drift apart.
                let first_move_depth = depth - 1 + move_extension;
                #[cfg(test)]
                if cur_i == 0 && move_extension > 0 && self.se_tt_move_search_depth.is_none() {
                    self.se_tt_move_search_depth = Some(first_move_depth);
                }
                self.search_child(
                    pos,
                    mv,
                    first_move_depth,
                    ply,
                    alpha,
                    beta,
                    pv_full_child,
                    ctx,
                    clock,
                )
            } else if !should_scout {
                // M8.A.1 reference full-window path: the pre-PVS move loop
                // (full-window reduced pilot → full-window full-depth re-search).
                // Taken in production for non-scouted non-first moves (the ramp's
                // off-regime — `root_depth <= D0` ⇒ scout_start = MAX ⇒ every
                // non-first move lands here ⇒ byte-identical to M7.B.2), and by
                // every move under the `test_disable_scout` exactness anchor.
                //
                // LMR firing is instrumented here with the SAME `#[cfg(test)]`
                // counters as the ladder's Steps 1/2 (scalars AND the move
                // vectors, all trace-ply-gated) so the wide-window `is_pv=false`
                // LMR-firing tests stay valid with `root_depth` unset (LMR fires
                // identically on both paths — ADR-0044 §4). The `pvs_*` counters
                // are NOT mirrored: scout/verify/re-search are ladder-only events.
                if r > 0 {
                    #[cfg(test)]
                    if self.lmr_trace_root_ply == Some(ply) {
                        self.lmr_reduced_searches += 1;
                        self.lmr_reduced_moves.push(mv);
                    }
                    let reduced_score = self.search_child(
                        pos,
                        mv,
                        depth - 1 - r,
                        ply,
                        alpha,
                        beta,
                        pv_scout_child,
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
                            pv_full_child,
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
                    self.search_child(
                        pos,
                        mv,
                        depth - 1,
                        ply,
                        alpha,
                        beta,
                        pv_scout_child,
                        ctx,
                        clock,
                    )
                }
            } else {
                // PVS ladder for a non-first move.
                let null_beta = alpha + 1;

                // Step 1 — pilot: reduced depth (or full when R == 0), null window.
                // `lmr_reduced_*` count the reduced pilots (R > 0), preserving the
                // ADR-0025 LMR-counter semantics; `pvs_scout_searches` counts every
                // non-first pilot.
                #[cfg(test)]
                if self.lmr_trace_root_ply == Some(ply) {
                    self.pvs_scout_searches += 1;
                    if r > 0 {
                        self.lmr_reduced_searches += 1;
                        self.lmr_reduced_moves.push(mv);
                    }
                }
                let mut s = self.search_child(
                    pos,
                    mv,
                    depth - 1 - r,
                    ply,
                    alpha,
                    null_beta,
                    pv_scout_child,
                    ctx,
                    clock,
                );
                if self.aborted {
                    return 0;
                }
                move_is_full_depth = r == 0;

                // Step 2 — verify: full depth, null window. Fires only after a
                // reduced pilot beat alpha. At the non-PV nodes where R > 0,
                // `null_beta == beta`, so this is byte-identical to the ADR-0025 §5
                // full-depth LMR re-search (`pvs_lmr_verify_searches` and
                // `lmr_full_researches` are the same event there).
                if r > 0 && s > alpha {
                    #[cfg(test)]
                    if self.lmr_trace_root_ply == Some(ply) {
                        self.pvs_lmr_verify_searches += 1;
                        self.lmr_full_researches += 1;
                        self.lmr_researched_moves.push(mv);
                    }
                    s = self.search_child(
                        pos,
                        mv,
                        depth - 1,
                        ply,
                        alpha,
                        null_beta,
                        pv_scout_child,
                        ctx,
                        clock,
                    );
                    if self.aborted {
                        return 0;
                    }
                    move_is_full_depth = true;
                }

                // Step 3 — exact: full depth, full window. Fires only when the
                // scout result is interior to the real window (`alpha < s < beta`);
                // statically unreachable at a non-PV node (`beta == alpha+1`).
                if pvs_needs_research(s, alpha, beta) {
                    #[cfg(test)]
                    if self.lmr_trace_root_ply == Some(ply) {
                        self.pvs_research_full_window += 1;
                    }
                    s = self.search_child(
                        pos,
                        mv,
                        depth - 1,
                        ply,
                        alpha,
                        beta,
                        pv_full_child,
                        ctx,
                        clock,
                    );
                    if self.aborted {
                        return 0;
                    }
                    move_is_full_depth = true;
                }
                s
            };

            // M8.A: the first non-excluded move has now been searched; later
            // moves take the scout path. (FFP-skipped quiets `continue` above
            // without reaching here, so they never consume first-move status.)
            first_searched = false;

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
        self.negamax_for_test_inner(
            pos,
            depth,
            ply,
            alpha,
            beta,
            is_pv,
            allow_null,
            excluded_move,
            ctx,
            // M7.B.2: reset root_depth to 0 so the depth-0 qsearch delegation
            // uses the flat-0 threshold (≡ M7.B) and the M8.A.1 PVS ramp is OFF
            // (scout_start = MAX ⇒ reference path) on a reused mover instance.
            0,
        )
    }

    /// M8.A.1: test-only sibling of `negamax_for_test` that publishes
    /// `root_depth` *after* the per-entry resets (so it survives the reset) to
    /// drive the depth-conditioned PVS ramp (ADR-0044 §4). Direct mirror of
    /// `qsearch_at_root_depth_for_test`. Pass `root_depth >= 16` for full PVS.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn negamax_at_root_depth_for_test(
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
        root_depth: u32,
    ) -> i32 {
        self.negamax_for_test_inner(
            pos,
            depth,
            ply,
            alpha,
            beta,
            is_pv,
            allow_null,
            excluded_move,
            ctx,
            root_depth,
        )
    }

    /// Shared body of `negamax_for_test` / `negamax_at_root_depth_for_test`:
    /// the per-entry counter resets, then `self.root_depth = root_depth` (set
    /// AFTER the resets so it is not clobbered), then the traced `negamax` call.
    /// Extracting it keeps the two entries from drifting (ADR-0044 §4).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn negamax_for_test_inner(
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
        root_depth: u32,
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
        // M8.A: reset PVS counters symmetrically. The harness flags
        // (`test_force_all_pv` / `test_disable_scout`) are NOT reset — the test
        // sets them before this call.
        self.pvs_scout_searches = 0;
        self.pvs_lmr_verify_searches = 0;
        self.pvs_research_full_window = 0;
        // M7.B.2 / M8.A.1: publish root_depth AFTER the resets so it survives;
        // it drives both the qsearch SEE-prune ramp and the PVS scout ramp.
        self.root_depth = root_depth;
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

    /// M8.A test-only accessors for the PVS counters. Mirror `nmp_firings_for_test`.
    #[cfg(test)]
    pub(super) fn pvs_scout_searches_for_test(&self) -> u32 {
        self.pvs_scout_searches
    }

    #[cfg(test)]
    pub(super) fn pvs_lmr_verify_searches_for_test(&self) -> u32 {
        self.pvs_lmr_verify_searches
    }

    #[cfg(test)]
    pub(super) fn pvs_research_full_window_for_test(&self) -> u32 {
        self.pvs_research_full_window
    }

    /// M8.A test-only accessor for the per-`go` node count (bit-identity anchor).
    #[cfg(test)]
    pub(super) fn nodes_for_test(&self) -> u64 {
        self.nodes
    }

    /// M8.A test-only setters for the exactness-harness flags. Production code
    /// never sets these; they default to `false` and are not reset by
    /// `negamax_for_test`.
    #[cfg(test)]
    pub(super) fn set_test_force_all_pv(&mut self, v: bool) {
        self.test_force_all_pv = v;
    }

    #[cfg(test)]
    pub(super) fn set_test_disable_scout(&mut self, v: bool) {
        self.test_disable_scout = v;
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
