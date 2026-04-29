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
        use crate::movegen::{MoveList, generate_moves, in_check};

        // 1. Clear this ply's PV slot. Stays BEFORE the depth==0 delegation so
        //    even leaves reset their slot — otherwise a prior subtree at the
        //    same ply could leave lengths[ply] > 0, which qsearch wouldn't
        //    touch, and the parent's pv.update would copy stale child PV moves.
        self.pv.clear_ply(ply as usize);

        // 2. Horizon: delegate to qsearch BEFORE incrementing self.nodes.
        //    qsearch's own per-frame increment + cancellation poll covers the leaf.
        //    Preserves M3.C's "1 leaf = 1 node" budget under `go nodes <N>`.
        if depth == 0 {
            return self.qsearch(pos, alpha, beta, ply, ctx);
        }

        // 3. Per-frame nodes increment + cancellation poll (non-leaf only).
        self.nodes += 1;
        if self.nodes & 4095 == 0 && ctx.should_abort(self.nodes) {
            self.aborted = true;
            return 0;
        }

        // 4. Repetition + 50-move draw checks — only at ply > 0 (root must pick a move).
        if ply > 0 {
            if is_fifty_move_draw(pos.halfmove_clock()) {
                return 0;
            }
            if is_repetition(&self.history, pos.halfmove_clock()) {
                return 0;
            }
        }

        // 5. Mate-distance pruning.
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
            let (child_alpha, child_beta) = negate_window(alpha, beta);
            let score = -self.negamax(pos, depth - 1, ply + 1, child_alpha, child_beta, ctx);
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
        //
        // M3.D note (plan §6.2): qsearch at the depth==0 horizon adds leaf
        // nodes to the count. The 5000-node budget remains adequate because
        // startpos qsearch leaves are mostly stand-pat returns (no captures
        // in the early opening). The precondition `bestmove.is_some()`
        // catches a vacuous-pass scenario; if a future eval/ordering shift
        // pushes the first root move's subtree above 5000 nodes, that
        // assertion fires with a helpful message.
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
        let score = ab.negamax_for_test(&mut pos.clone(), 1, 0, -INF, INF, &ctx);

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
}
