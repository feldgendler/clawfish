//! Search trait and value types.
//!
//! Defined at M2.C so M2.D's random-mover and M3+'s alpha-beta plug into the
//! orchestrator without trait churn. `SearchContext` carries the cancellation
//! flag, deadline, start time, and parsed `SearchLimits`; `Search::go` is
//! polled by the orchestrator's worker thread and must obey `should_abort`
//! (per ADR-0011 and `docs/plans/m2.c.md` §3).
//!
//! M3.C ships the [`AlphaBetaMover`] implementation: fail-soft negamax +
//! alpha-beta with triangular PV recovery, MVV-LVA move ordering, and
//! repetition/50-move draw detection. See ADR-0016 and plan docs/plans/m3.c.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::tt::{TranspositionTable, TtBound, TtData, score_from_tt, score_to_tt};
use crate::{Color, Move, Position};

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
#[derive(Clone)]
pub struct SearchContext {
    /// Flipped by the orchestrator on `stop` / time expiry. Polled by
    /// `should_abort`. Cleared by the orchestrator at the start of each
    /// `go`. See plan §7 for the cleared-then-spawned ordering.
    pub stop: Arc<AtomicBool>,
    /// Hard-cap wallclock deadline. M3.D semantics: computed from `movetime`.
    /// M3.E semantics: computed from the time-management `compute_caps` hard-cap
    /// path. `None` = no hard cap. Polled by `should_abort`.
    pub deadline: Option<Instant>,
    /// Soft-cap wallclock deadline (M3.E). Polled by the iterative-deepening
    /// outer loop in `Search::go` BETWEEN iterations only — never inside
    /// `should_abort` (which remains the hard-cap path). When the soft cap has
    /// elapsed at the end of an iteration, the ID loop exits before starting
    /// the next iteration. `None` = no soft cap (e.g. `go infinite`,
    /// `go depth N`, `go nodes N`).
    pub soft_deadline: Option<Instant>,
    /// `Instant::now()` at the moment `handle_go` built the context. Used
    /// by future `info time` emission (M3+).
    pub start: Instant,
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

impl SearchContext {
    /// `true` ⇒ cancel this iteration immediately. `nodes_searched` is the
    /// caller's running node count; compared against `self.limits.nodes`
    /// (the cap from `go nodes <N>`). `Relaxed` ordering is sufficient
    /// per ADR-0011 §"Ordering and safety".
    #[inline]
    pub fn should_abort(&self, nodes_searched: u64) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return true;
        }
        if let Some(d) = self.deadline
            && Instant::now() >= d
        {
            return true;
        }
        if let Some(cap) = self.limits.nodes
            && nodes_searched >= cap
        {
            return true;
        }
        false
    }
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
        // Per-go reset.
        self.history = ctx.history.clone();
        self.nodes = 0;
        self.aborted = false;
        self.root_score = None;
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
            // Per-iteration reset (subset of per-go reset). `nodes` is NOT
            // reset — it accumulates across iterations. The TT survives across
            // iterations (the cross-iteration hint is what makes ID worthwhile).
            self.aborted = false;
            self.root_score = None;
            for i in 0..MAX_PLY {
                self.pv.lengths[i] = 0;
            }
            clear_killers(&mut self.killers);

            let returned = self.negamax(&mut pos_clone, depth, 0, -INF, INF, true, ctx);

            if self.aborted {
                // Mid-iteration abort: discard partial PV/score; preserve prior
                // last_complete snapshot.
                break;
            }

            // Iteration completed. Snapshot.
            let bestmove = (self.pv.lengths[0] > 0).then(|| self.pv.moves[0][0]);
            last_complete = Some((depth, bestmove, returned));

            // Single Instant::now() reused for both the elapsed-ms field and
            // the soft-cap check below — avoids a duplicate syscall and keeps
            // the two reads coherent.
            let now = Instant::now();
            let elapsed_ms = (now - ctx.start).as_millis();
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
            if let Some(soft) = ctx.soft_deadline
                && now >= soft
            {
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
            while !ctx.should_abort(self.nodes) {
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
        self.history.clear();
        clear_killers(&mut self.killers);
        // pv and nodes are reset per-go; history table joins this list at M4.C; TT lives in engine.
    }
}

impl AlphaBetaMover {
    /// Fail-soft negamax with alpha-beta pruning and triangular PV recovery.
    ///
    /// `is_pv` is the synthetic ordering predicate that gates TT cutoffs
    /// (ADR-0018 §11). `true` at the root and at the first child of a PV
    /// parent (recursion-order index 0); `false` everywhere else. PVS at M4.D
    /// will replace it with the window-based `beta - alpha == 1` check.
    #[allow(clippy::too_many_arguments)]
    fn negamax(
        &mut self,
        pos: &mut Position,
        depth: u32,
        ply: u32,
        mut alpha: i32,
        mut beta: i32,
        is_pv: bool,
        ctx: &SearchContext,
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
            return self.qsearch(pos, alpha, beta, ply, ctx);
        }

        // 3. Per-frame nodes increment + cancellation poll (non-leaf only).
        self.nodes += 1;
        if self.nodes & 4095 == 0 && ctx.should_abort(self.nodes) {
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

        // 8. Generate moves.
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves_vec = ml.iter().collect::<Vec<_>>();

        // Searchmoves filter at root only.
        if ply == 0
            && let Some(filter) = &ctx.limits.searchmoves
        {
            moves_vec.retain(|m| filter.contains(m));
        }

        // 9. Terminal: no legal moves.
        //    At ply==0 with searchmoves filter active, an empty list is a degenerate
        //    user input (all-illegal or empty filter). Short-circuit BEFORE the
        //    in_check triage — otherwise a check position with a degenerate filter
        //    would falsely return -MATE.
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

        // 10. Order: killer-aware scoring (captures > killers > other quiets)
        //     descending, then promote the TT move (if any) to index 0.
        //     `Move::default().bits() == 0` is the no-move sentinel and is
        //     never produced by movegen, so `tt_move == 0` falls through.
        //     The legality scan over the legal-move list rejects garbage
        //     values (ADR-0018 §12).
        let killer0 = self.killers[ply as usize][0];
        let killer1 = self.killers[ply as usize][1];
        order_moves(&mut moves_vec, pos, killer0, killer1, tt_move);

        // 11. Recurse fail-soft. `child_is_pv = is_pv && i == 0` per ADR-0018 §11
        //     where `i` is the recursion-order index (post-step-10 reorder).
        let mut best = -INF;
        let mut cutoff_move: Option<Move> = None;
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
                ctx,
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
                    if is_quiet(mv) {
                        update_killers(&mut self.killers, ply as usize, mv);
                    }
                    break; // beta cutoff — fail-soft: return `best`, not `beta`
                }
            }
        }

        // 12. Store on completion. Skip on abort (partial bounds are not real)
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
        ctx: &SearchContext,
    ) -> i32 {
        self.negamax(pos, depth, ply, alpha, beta, is_pv, ctx)
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
    ) -> i32 {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};

        // 1. Per-frame nodes increment + cancellation poll. Negamax's depth==0
        //    branch delegates to qsearch BEFORE its own increment, so this is the
        //    sole counter for the depth==0 leaf — preserves M3.C's "1 leaf = 1
        //    node" budget interpretation under `go nodes <N>`.
        self.nodes += 1;
        if self.nodes & 4095 == 0 && ctx.should_abort(self.nodes) {
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
            let score = -self.qsearch(pos, child_alpha, child_beta, ply + 1, ctx);
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
        self.qsearch(pos, alpha, beta, ply, ctx)
    }
}

// ---------------------------------------------------------------------------
// M4.B — Killer-move helpers + ordering.
// ---------------------------------------------------------------------------

/// Bonus score for the most-recent quiet beta-cutoff at this ply (slot 0).
/// Strictly between 0 and the smallest MVV-LVA capture score (QxP ≈ 287 cp)
/// so killers slot above remaining quiets but below all captures and promos.
/// Pinned by S23 at runtime against the actual MVV-LVA formula.
const KILLER0_SCORE: i32 = 200;

/// Bonus score for the prior quiet beta-cutoff at this ply (slot 1).
/// Must satisfy `KILLER1_SCORE < KILLER0_SCORE` and `KILLER1_SCORE > 0`.
const KILLER1_SCORE: i32 = 100;

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

/// Killer-aware move-ordering score for negamax (NOT qsearch). Wraps
/// `mvv_lva_score`:
///   - non-quiet move → `mvv_lva_score(mv, pos)` (captures/promos).
///   - quiet move matching `killer0` → `KILLER0_SCORE`.
///   - quiet move matching `killer1` (and not `killer0`) → `KILLER1_SCORE`.
///   - other quiet → `0`.
///
/// Boundary discipline: `KILLER0_SCORE > KILLER1_SCORE > 0` and both
/// strictly less than the smallest possible MVV-LVA capture score.
/// Pinned by S23 at runtime.
fn negamax_move_order_score(mv: Move, pos: &Position, killer0: Move, killer1: Move) -> i32 {
    if !is_quiet(mv) {
        return mvv_lva_score(mv, pos);
    }
    if mv == killer0 {
        KILLER0_SCORE
    } else if mv == killer1 {
        KILLER1_SCORE
    } else {
        0
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
    tt_move: u16,
) {
    moves_vec.sort_by_cached_key(|&m| -negamax_move_order_score(m, pos, killer0, killer1));
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
pub(crate) fn is_repetition(history: &[u64], halfmove_clock: u8) -> bool {
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
pub(crate) fn is_fifty_move_draw(halfmove_clock: u8) -> bool {
    halfmove_clock >= 100
}

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
            deadline: None,
            soft_deadline: None,
            start: Instant::now(),
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

    // D11 is carried over verbatim from M2.C — it tests SearchContext, not Search.
    #[test]
    fn should_abort_three_subcases() {
        // Sub-case 1: stop flag set.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: None,
                soft_deadline: None,
                start: Instant::now(),
                limits: SearchLimits::default(),
                history: Vec::new(),
                tt: None,
            };
            assert!(!ctx.should_abort(0), "should not abort before stop is set");
            stop.store(true, Ordering::Relaxed);
            assert!(ctx.should_abort(0), "should abort after stop flag is set");
        }

        // Sub-case 2: deadline already expired.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let expired = Instant::now() - Duration::from_millis(1);
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: Some(expired),
                soft_deadline: None,
                start: Instant::now(),
                limits: SearchLimits::default(),
                history: Vec::new(),
                tt: None,
            };
            assert!(
                ctx.should_abort(0),
                "should abort when deadline is already in the past"
            );
        }

        // Sub-case 3: node cap.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: None,
                soft_deadline: None,
                start: Instant::now(),
                limits: SearchLimits {
                    nodes: Some(500),
                    ..SearchLimits::default()
                },
                history: Vec::new(),
                tt: None,
            };
            assert!(
                ctx.should_abort(1_000),
                "should abort when nodes_searched (1000) >= cap (500)"
            );
            assert!(
                !ctx.should_abort(100),
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, &ctx_depth2);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, true, &ctx_depth2);
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
                    deadline: None,
                    soft_deadline: None,
                    start: std::time::Instant::now(),
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
            deadline: None,
            soft_deadline: None,
            start: std::time::Instant::now(),
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
            deadline: None,
            soft_deadline: None,
            start: std::time::Instant::now(),
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
            deadline: None,
            soft_deadline: None,
            start: std::time::Instant::now(),
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
            deadline: None,
            soft_deadline: None,
            start: std::time::Instant::now(),
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
        let score = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, &ctx);

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

    /// Build a `SearchContext` from a position with the given limits. Soft
    /// and hard deadlines are `None`; stop is non-aborting.
    fn ctx_for(pos: &Position, limits: SearchLimits) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            soft_deadline: None,
            start: Instant::now(),
            limits,
            history: vec![pos.zobrist()],
            tt: None,
        };
        (ctx, stop)
    }

    /// Build a `SearchContext` with a soft deadline already in the past.
    fn ctx_with_soft_in_past(
        pos: &Position,
        limits: SearchLimits,
    ) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: Some(now + Duration::from_secs(10)), // generous hard
            soft_deadline: Some(now - Duration::from_millis(1)),
            start: now,
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
        let now = Instant::now();
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: Some(now + Duration::from_secs(10)),
            soft_deadline: Some(now + Duration::from_millis(200)),
            start: now,
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
        let now = Instant::now();
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            soft_deadline: None,
            start: now,
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
            deadline: None,
            soft_deadline: None,
            start: Instant::now(),
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

        let _ = ab.negamax_for_test(&mut pos.clone(), 2, 0, -100, -99, false, &ctx);

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
            deadline: None,
            soft_deadline: None,
            start: Instant::now(),
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
        let _ = ab2.negamax_for_test(&mut pos2.clone(), 2, 0, -INF, INF, true, &ctx2);
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

        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            // Deadline already expired → first 4096-cadence poll inside
            // negamax flips self.aborted=true and returns.
            deadline: Some(Instant::now() - Duration::from_millis(1)),
            soft_deadline: None,
            start: Instant::now(),
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
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, &ctx);
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
        let score = ab.negamax_for_test(&mut pos.clone(), 3, 0, -100, INF, false, &ctx);
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
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx);
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
        let _ = ab_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx_a);
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
        let _ = ab_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx_b);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, &ctx);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, false, &ctx);
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
        let _ = ab1.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, &ctx);
        let nodes_with_hit = ab1.nodes - nodes_before_a;

        tt.clear();

        let mut ab2 = AlphaBetaMover::new();
        ab2.history = vec![pos.zobrist()];
        ab2.set_tt_for_test(Some(tt.clone()));
        let nodes_before_b = ab2.nodes;
        let _ = ab2.negamax_for_test(&mut pos.clone(), 3, 0, -INF, 100, false, &ctx);
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

        let _ = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, true, &ctx);

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
        let _ = ab_ref.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx_ref);
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
        let _ = ab.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx);
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
        let score = negamax_move_order_score(mv, &pos, mv, other);
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
        let score = negamax_move_order_score(mv, &pos, other, mv);
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
    #[test]
    fn negamax_move_order_score_returns_mvv_lva_for_capture_even_if_matches_killer() {
        // Position: white pawn on b4 can capture black queen on c5.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let pawn_takes_queen =
            Move::from_uci("b4c5", &pos).expect("b4c5 must be a legal pawn capture");
        // Install the same capture move as killer0 (artificial — captures can't
        // legally become killers, but the test verifies the gate independently).
        let score =
            negamax_move_order_score(pawn_takes_queen, &pos, pawn_takes_queen, Move::default());
        let expected = mvv_lva_score(pawn_takes_queen, &pos);
        assert_eq!(
            score, expected,
            "capture matching killer0 must return mvv_lva_score ({expected}), not KILLER0_SCORE"
        );
        // Cross-check: the mvv_lva score is strictly greater than KILLER0_SCORE,
        // proving the test is not vacuously passing because KILLER0_SCORE == expected.
        assert!(
            expected > KILLER0_SCORE,
            "mvv_lva_score for a capture must exceed KILLER0_SCORE to make S22 non-vacuous; \
             got mvv_lva={expected}, KILLER0_SCORE={KILLER0_SCORE}"
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
        let score = negamax_move_order_score(mv, &pos, killer0, killer1);
        assert_eq!(score, 0, "quiet not in either killer slot must score 0");
    }

    // -----------------------------------------------------------------------
    // S23 — `KILLER0_SCORE` and `KILLER1_SCORE` are strictly below the
    //        smallest MVV-LVA capture (QxP) and above 0.
    // -----------------------------------------------------------------------

    /// Runtime boundary check using the actual MVV-LVA formula and PeSTO MG
    /// values. Pins the constant values against any future bumps of KILLER0_SCORE
    /// past the capture floor, or changes to the MVV-LVA scaling factor.
    #[test]
    fn killer_scores_strictly_below_smallest_capture() {
        // Constant ordering invariants checked at compile time.
        const {
            assert!(
                KILLER0_SCORE > KILLER1_SCORE,
                "KILLER0_SCORE must be > KILLER1_SCORE"
            );
            assert!(KILLER1_SCORE > 0, "KILLER1_SCORE must be > 0");
        }
        // Runtime check: KILLER0_SCORE must be below the actual smallest capture
        // (QxP), which depends on the MVV-LVA formula and PeSTO MG values.
        let pos =
            Position::from_fen("4k3/8/2p5/2qQ4/1P6/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let qxp = Move::from_uci("d5c6", &pos).expect("d5c6 must be a legal queen×pawn capture");
        let qxp_score = mvv_lva_score(qxp, &pos);
        assert!(
            KILLER0_SCORE < qxp_score,
            "KILLER0_SCORE ({KILLER0_SCORE}) must be < mvv_lva_score(QxP) ({qxp_score}) \
             so killers slot below all captures"
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
        order_moves(&mut moves_vec, &pos, Move::default(), Move::default(), 0);
        let first = moves_vec[0];
        let before = moves_vec.clone();
        // Now call again with the first move as the TT move.
        order_moves(
            &mut moves_vec,
            &pos,
            Move::default(),
            Move::default(),
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
        order_moves(&mut moves_ref, &pos, Move::default(), Move::default(), 0);
        order_moves(&mut moves_a, &pos, Move::default(), Move::default(), 0);
        assert_eq!(
            moves_a, moves_ref,
            "tt_move==0 must produce pure MVV-LVA order (same as reference)"
        );

        // Sub-case (b): tt_move bits not present in the list.
        // Use 0xFFFF — an impossible legal move (from=H1, to=H8, flag=QueenPromoCapture
        // which never appears at startpos). The sort must not promote any entry.
        let mut moves_b: Vec<Move> = ml.iter().collect();
        order_moves(&mut moves_b, &pos, Move::default(), Move::default(), 0xFFFF);
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
        order_moves(&mut moves_vec, &pos, killer_quiet, Move::default(), 0);

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
        order_moves(&mut moves_ref, &pos, Move::default(), Move::default(), 0);

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
        order_moves(&mut moves_stale, &pos, stale_killer, Move::default(), 0);

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
        let _ = mover.negamax_for_test(&mut pos.clone(), 2, 0, -100, -50, true, &ctx);

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
        let _ = mover.negamax_for_test(&mut pos.clone(), 1, 0, 10000, 10001, false, &ctx);

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
        let _ = mover_a.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx_a);
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
        order_moves(&mut moves_check, &pos, Move::default(), Move::default(), 0);
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
        let _ = mover_b.negamax_for_test(&mut pos.clone(), 3, 0, -INF, INF, true, &ctx_b);
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
}
