use crate::history::{HistoryTable, MAX_HISTORY};
use crate::{Color, Move, MoveList, Position};

use super::MAX_PLY;

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
pub(crate) const CAPTURE_OFFSET: i32 = 1_000_000;

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
pub(crate) const KILLER0_SCORE: i32 = 100_001;

/// Bonus score for the prior quiet beta-cutoff at this ply (slot 1).
/// Must satisfy `KILLER1_SCORE < KILLER0_SCORE` and
/// `KILLER1_SCORE > MAX_HISTORY` (so killers always rank above the best
/// history-rated quiet).
// Used by order_moves (test-only post-M5.H1) and the compile-time invariant.
pub(crate) const KILLER1_SCORE: i32 = 100_000;

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
pub(crate) fn is_quiet(mv: Move) -> bool {
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
pub(crate) fn update_killers(killers: &mut [[Move; 2]; MAX_PLY], ply: usize, mv: Move) {
    if killers[ply][0] != mv {
        killers[ply][1] = killers[ply][0];
        killers[ply][0] = mv;
    }
}

/// Apply the M4.C butterfly-history updates for a quiet beta-cutoff:
/// `+depth^2` to the cutter and `-depth^2` to each prior quiet searched at
/// the same node.
pub(crate) fn update_history_on_quiet_cutoff(
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
pub(crate) fn negamax_move_order_score(
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
pub(crate) fn order_moves(
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
pub(crate) fn clear_killers(killers: &mut [[Move; 2]; MAX_PLY]) {
    *killers = [[Move::default(); 2]; MAX_PLY];
}

/// Returns true iff the move should be searched in qsearch (when not in check).
/// Captures, EnPassant, and queen-promo variants (with or without capture).
/// Excludes under-promotions and under-promo-captures (M3.D scope) and
/// non-capture checks (M4+).
pub(crate) fn qsearch_move_filter(mv: Move) -> bool {
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
pub(crate) fn mvv_lva_score(mv: Move, pos: &Position) -> i32 {
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
    #[allow(dead_code)] // used by tests only; production gate is is_empty()
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
pub(crate) fn extract_move_by_bits(v: &mut Vec<Move>, target: u16) -> Option<Move> {
    if target == 0 {
        return None;
    }
    let idx = v.iter().position(|m| m.bits() == target)?;
    Some(v.remove(idx))
}

/// Same as `extract_move_by_bits` but compares against a full `Move`
/// (flag bits included). Used for killer-slot extraction.
#[cfg(test)]
pub(crate) fn extract_move_by_eq(v: &mut Vec<Move>, target: Move) -> Option<Move> {
    let idx = v.iter().position(|&m| m == target)?;
    Some(v.remove(idx))
}

/// Partition `all` into `(captures, quiets)` by `is_quiet`. Manual
/// implementation rather than `Iterator::partition` to make the
/// order-preservation guarantee explicit. Within each output Vec, original
/// movegen-emit order is preserved.
#[cfg(test)]
pub(crate) fn partition_captures_quiets(all: Vec<Move>) -> (Vec<Move>, Vec<Move>) {
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
pub(crate) fn mvv_lva_sort_in_place(captures: &mut [Move], pos: &Position) {
    captures.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));
}

/// History-score-desc stable sort over a slice of quiet moves.
#[cfg(test)]
pub(crate) fn history_sort_in_place(quiets: &mut [Move], pos: &Position, history: &HistoryTable) {
    let stm = pos.side_to_move();
    quiets.sort_by_cached_key(|&m| -(history.score(stm, m.from_square(), m.to_square()) as i32));
}
