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
//! repetition/50-move draw detection. See ADR-0013 and plan docs/plans/m3.c.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{Move, Position};

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
    /// Wallclock cap, computed from `movetime` (M2.D / M3 will compute
    /// from `wtime`/`btime`/`winc` etc.). `None` = no time cap.
    pub deadline: Option<Instant>,
    /// `Instant::now()` at the moment `handle_go` built the context. Used
    /// by future `info time` emission (M3+).
    pub start: Instant,
    /// Parsed `go` parameters for this search invocation.
    pub limits: SearchLimits,
    /// Zobrist trajectory from the start of the game through the current
    /// position. Cloned from `Engine::game_history` at `go`-start.
    /// M3.C negamax will push/pop entries on make/unmake during recursion.
    pub history: Vec<u64>,
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
const MATE_IN_MAX_PLY: i32 = MATE - MAX_PLY as i32; // 29_936

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
/// triangular table. Mate scores are ply-adjusted (see ADR-0013 §3).
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
        for i in 0..MAX_PLY {
            self.pv.lengths[i] = 0;
        }

        // Determine target depth. Default 4 when `go` did not specify depth.
        let depth = ctx.limits.depth.unwrap_or(4).min(MAX_PLY as u32 - 1);

        // Single-iteration negamax. Position is cloned for mutation; make/unmake
        // are balanced by the recursion.
        let mut pos_clone = *position;
        let returned_score = self.negamax(&mut pos_clone, depth, 0, -INF, INF, ctx);

        // Position must be restored by balanced make/unmake.
        debug_assert_eq!(
            pos_clone, *position,
            "negamax must restore position via balanced make/unmake"
        );

        // When the search aborts mid-root, `returned_score` is the 0 sentinel
        // propagated up from the abort point — it does not reflect any completed
        // subtree. Use `root_score` instead: it is updated in lockstep with the PV
        // at ply 0 and therefore always reflects the score of the last fully
        // explored root-move subtree. If no root move completed before the abort,
        // `root_score` is `None` and we fall back to 0 (matching the `pv.lengths[0]
        // == 0 → bestmove None` case).
        let score = if self.aborted {
            self.root_score.unwrap_or(0)
        } else {
            returned_score
        };

        // bestmove comes from pv[0][0] if any move improved alpha; otherwise None.
        let bestmove = if self.pv.lengths[0] > 0 {
            Some(self.pv.moves[0][0])
        } else {
            None
        };

        // Emit info line BEFORE the wait loop.
        let elapsed_ms = ctx.start.elapsed().as_millis();
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
            "info depth {} score {} nodes {} time {} pv {}",
            depth,
            score_to_uci(score),
            self.nodes,
            elapsed_ms,
            pv_str
        ));

        // Honor infinite/movetime/ponder wait loop. Same shape as prior movers.
        let wait = ctx.limits.infinite || ctx.limits.movetime.is_some() || ctx.limits.ponder;
        if wait {
            while !ctx.should_abort(self.nodes) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        SearchResult {
            bestmove,
            depth,
            score_cp: Some(score),
            nodes: self.nodes,
            ponder: None,
        }
    }

    fn reset(&mut self) {
        self.history.clear();
        // pv and nodes are reset per-go; M4 will clear killer/history/TT here.
    }
}

impl AlphaBetaMover {
    /// Fail-soft negamax with alpha-beta pruning and triangular PV recovery.
    fn negamax(
        &mut self,
        pos: &mut Position,
        depth: u32,
        ply: u32,
        mut alpha: i32,
        mut beta: i32,
        ctx: &SearchContext,
    ) -> i32 {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves, in_check};

        // 1. Cancellation poll at 4096-node cadence.
        self.nodes += 1;
        if self.nodes & 4095 == 0 && ctx.should_abort(self.nodes) {
            self.aborted = true;
            return 0;
        }

        // 2. Clear PV slot at this ply so a no-improving-move return leaves length 0.
        self.pv.clear_ply(ply as usize);

        // 3. Repetition + 50-move draw checks — only at ply > 0 (root must pick a move).
        if ply > 0 {
            if is_fifty_move_draw(pos.halfmove_clock()) {
                return 0;
            }
            if is_repetition(&self.history, pos.halfmove_clock()) {
                return 0;
            }
        }

        // 4. Mate-distance pruning.
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

        // 5. Horizon: call evaluate directly (qsearch is M3.D).
        if depth == 0 {
            return evaluate(pos);
        }

        // 6. Generate moves.
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);
        let mut moves_vec = ml.iter().collect::<Vec<_>>();

        // Searchmoves filter at root only.
        if ply == 0
            && let Some(filter) = &ctx.limits.searchmoves
        {
            moves_vec.retain(|m| filter.contains(m));
        }

        // 7. Terminal: no legal moves.
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

        // 8. Order: MVV-LVA descending.
        moves_vec.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));

        // 9. Recurse fail-soft.
        let mut best = -INF;
        for mv in moves_vec {
            let undo = pos.make_move(mv);
            self.history.push(pos.zobrist());
            let score = -self.negamax(pos, depth - 1, ply + 1, -beta, -alpha, ctx);
            self.history.pop();
            pos.unmake_move(mv, undo);

            // 10. Abort check: score from an aborted search is invalid.
            if self.aborted {
                return 0;
            }

            if score > best {
                best = score;
                if score > alpha {
                    alpha = score;
                    // 11. PV update: this move improved alpha at this ply.
                    self.pv.update(ply as usize, mv);
                    // Track the root score in lockstep with the PV so that if
                    // the search aborts later, `go` can report the score of the
                    // last fully-explored root subtree instead of the 0 sentinel.
                    if ply == 0 {
                        self.root_score = Some(score);
                    }
                }
                if alpha >= beta {
                    break; // beta cutoff — fail-soft: return `best`, not `beta`
                }
            }
        }

        best
    }

    /// Test-only entry point that forwards verbatim to `negamax`. Exists
    /// because tests cannot call private methods directly; production code
    /// never calls this.
    #[cfg(test)]
    pub(super) fn negamax_for_test(
        &mut self,
        pos: &mut Position,
        depth: u32,
        ply: u32,
        alpha: i32,
        beta: i32,
        ctx: &SearchContext,
    ) -> i32 {
        self.negamax(pos, depth, ply, alpha, beta, ctx)
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
            start: Instant::now(),
            limits: SearchLimits::default(),
            history: Vec::new(),
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
                start: Instant::now(),
                limits: SearchLimits::default(),
                history: Vec::new(),
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
                start: Instant::now(),
                limits: SearchLimits::default(),
                history: Vec::new(),
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
                start: Instant::now(),
                limits: SearchLimits {
                    nodes: Some(500),
                    ..SearchLimits::default()
                },
                history: Vec::new(),
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, &ctx_depth2);
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

        let score = ab.negamax_for_test(&mut pos.clone(), 2, 1, -INF, INF, &ctx_depth2);
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
                    start: std::time::Instant::now(),
                    limits: SearchLimits {
                        depth: Some(3),
                        ..SearchLimits::default()
                    },
                    history: vec![pos.zobrist()],
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
            result.nodes < 8000,
            "depth-3 search from startpos must visit fewer than 8000 nodes (loose upper bound); \
            visited {} nodes",
            result.nodes
        );
    }

    /// With `stop` pre-set before `go`, the search must abort promptly.
    ///
    /// The pre-set abort fires at the first 4096-node poll, before any root move
    /// improves alpha → `bestmove` is always `None` and `score_cp` is the 0 sentinel.
    #[test]
    fn alphabeta_search_aborts_on_should_abort() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(true)); // pre-set to abort immediately
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: std::time::Instant::now(),
            limits: SearchLimits {
                depth: Some(10), // would be slow without early abort
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &|_| {});

        // Cancellation poll cadence is `if self.nodes & 4095 == 0` (every 4096 nodes).
        assert!(
            result.nodes <= 5000,
            "early abort must not search a large node count: got {} \
            (cadence 4096; first abort fires at nodes=4096)",
            result.nodes
        );

        // Pre-set abort fires before any root move can complete.
        assert_eq!(
            result.bestmove, None,
            "pre-set abort fires before any root move can complete"
        );
        assert_eq!(
            result.score_cp,
            Some(0),
            "abort sentinel score must be 0 when no root move completed"
        );
    }

    /// When the search aborts AFTER the first root move's subtree completes,
    /// `bestmove` must be `Some` and `score_cp` must reflect the completed
    /// subtree score — NOT the 0 abort sentinel.
    ///
    /// This is a regression test for must-fix #2 (aborted-search score contamination).
    #[test]
    fn alphabeta_partial_completion_under_abort_returns_partial_pv_and_score() {
        // 5000-node budget: enough for the first root move's depth-4 subtree at
        // startpos to complete (typically ~200–1000 nodes per root move), but
        // exhausted before all 20 root moves finish.
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: std::time::Instant::now(),
            limits: SearchLimits {
                depth: Some(4),
                nodes: Some(5000),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
        };
        let mut ab = AlphaBetaMover::new();
        let result = ab.go(&pos, &ctx, &|_| {});

        // Precondition: with `nodes: Some(5000)` from startpos at depth 4,
        // partial completion is reliably reached today (cancellation poll fires
        // at the 4096-node alignment, after the first root move's subtree has
        // updated the root PV + score). If a future change shifts node
        // accounting (cadence, ordering, or budget), the test would fall
        // through to a vacuous pass — so we assert the precondition explicitly.
        assert!(
            result.bestmove.is_some(),
            "fixture must reach partial completion at depth 4 with nodes=5000; \
            adjust the budget if abort fires before any root move improves alpha"
        );
        let mv = result.bestmove.unwrap();
        // Verify the move is legal.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|legal| legal == mv),
            "partial-completion bestmove {} must be legal from startpos",
            mv.to_uci()
        );
        // The score must NOT be 0 (the abort sentinel). A real depth-4
        // centipawn score from startpos is non-zero (empirically ~-30 cp).
        // We don't pin the exact value; we just assert it differs from the
        // abort sentinel to catch the score-contamination regression.
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
            start: std::time::Instant::now(),
            limits: SearchLimits {
                depth: Some(3),
                searchmoves: Some(vec![e2e4]),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
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

    /// After `go depth 2` from startpos, exactly one `info` line must be emitted,
    /// starting with `info depth 2 score ...` and containing a `pv` token.
    #[test]
    fn alphabeta_emits_info_line_with_score_and_pv() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: std::time::Instant::now(),
            limits: SearchLimits {
                depth: Some(2),
                ..SearchLimits::default()
            },
            history: vec![pos.zobrist()],
        };
        let mut ab = AlphaBetaMover::new();
        let info_lines: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        ab.go(&pos, &ctx, &|line| {
            info_lines.borrow_mut().push(line.to_string());
        });
        let info_lines = info_lines.into_inner();

        assert_eq!(
            info_lines.len(),
            1,
            "exactly one info line must be emitted; got: {info_lines:?}"
        );
        let line = &info_lines[0];
        assert!(
            line.starts_with("info depth 2 score cp ")
                || line.starts_with("info depth 2 score mate "),
            "info line must start with 'info depth 2 score cp ...' or 'info depth 2 score mate ...'; got: {line:?}"
        );
        assert!(
            line.contains(" pv "),
            "info line must contain 'pv'; got: {line:?}"
        );
    }
}
