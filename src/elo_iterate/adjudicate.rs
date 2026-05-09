//! Native game-over detection.
//!
//! Covers: checkmate, stalemate, 50-move rule, threefold repetition
//! (FIDE 9.2 — 3 occurrences total in full game history), and insufficient
//! material (KK, KBK, KNK, KBKB-same-colour only).
//!
//! `detect_native_game_over` does NOT cover time forfeit; that is computed
//! by the match loop from per-side clock state.

use super::driver;
use crate::{
    Color, MoveList, PieceKind, Position, generate_moves, in_check, search::is_fifty_move_draw,
};

/// Reason a game has ended, as detected by native adjudication.
#[derive(Debug)]
pub(crate) enum GameOver {
    /// The side to move is in checkmate. Carries the colour that delivered
    /// the mate (= opponent of the mated side).
    Checkmate(Color),
    /// The side to move has no legal moves and is not in check.
    Stalemate,
    /// The 50-move rule threshold (halfmove clock ≥ 100) has been reached.
    FiftyMove,
    /// The current position has appeared at least three times in the game
    /// record (FIDE 9.2).
    ThreefoldRepetition,
    /// Neither side has sufficient material to force checkmate.
    InsufficientMaterial,
    /// A side exceeded its time allowance. Carries the colour that forfeited.
    /// Reserved for future use; time-forfeit detection currently lives in
    /// `match_loop::GameOutcome::TimeForfeit` rather than adjudication.
    #[allow(dead_code)]
    TimeForfeit(Color),
    /// The just-moved side resigned — its score reached the threshold.
    /// Carries the *resigning* (= losing) color.
    ResignAdjudicated(Color),
    /// Both sides agreed on a near-zero score after the movenumber floor.
    DrawAdjudicated,
}

/// Check for a native game-over condition after a move has been made.
///
/// `history` is the full-game Zobrist trail (every position from game start,
/// including the current position as the last entry).
///
/// Order of checks (each pair pinned by a precedence test):
/// 1. Legal-move count → Checkmate or Stalemate.
/// 2. `is_fifty_move_draw` → FiftyMove.
/// 3. `is_threefold_repetition_for_adjudication` → ThreefoldRepetition.
/// 4. `is_insufficient_material` → InsufficientMaterial.
pub(crate) fn detect_native_game_over(pos: &Position, history: &[u64]) -> Option<GameOver> {
    let mut moves = MoveList::new();
    generate_moves(pos, &mut moves);
    if moves.is_empty() {
        return if in_check(pos) {
            Some(GameOver::Checkmate(pos.side_to_move().flip()))
        } else {
            Some(GameOver::Stalemate)
        };
    }
    if is_fifty_move_draw(pos.halfmove_clock()) {
        return Some(GameOver::FiftyMove);
    }
    if is_threefold_repetition_for_adjudication(history) {
        return Some(GameOver::ThreefoldRepetition);
    }
    if is_insufficient_material(pos) {
        return Some(GameOver::InsufficientMaterial);
    }
    None
}

/// True iff neither side has material sufficient to force checkmate.
///
/// Covers: KK, KBK (either side), KNK (either side), KBKB-same-colour.
/// Negative cases (KNK N, KBNK, KQK, KRK, KPVK) return false.
pub(crate) fn is_insufficient_material(pos: &Position) -> bool {
    // Any pawn, rook, or queen on the board → sufficient material.
    if pos.pieces(PieceKind::Pawn).any()
        || pos.pieces(PieceKind::Rook).any()
        || pos.pieces(PieceKind::Queen).any()
    {
        return false;
    }

    let white_bishops = pos.pieces_colored(Color::White, PieceKind::Bishop);
    let black_bishops = pos.pieces_colored(Color::Black, PieceKind::Bishop);
    let white_knights = pos.pieces_colored(Color::White, PieceKind::Knight);
    let black_knights = pos.pieces_colored(Color::Black, PieceKind::Knight);

    let white_minors = white_bishops.count() + white_knights.count();
    let black_minors = black_bishops.count() + black_knights.count();

    match (white_minors, black_minors) {
        // KK
        (0, 0) => true,
        // KBK or KNK (one side has a single minor, the other has none)
        (1, 0) | (0, 1) => true,
        // KBKB — both sides have exactly one bishop; insufficient iff same-colour squares
        (1, 1)
            if white_bishops.count() == 1
                && black_bishops.count() == 1
                && white_knights.is_empty()
                && black_knights.is_empty() =>
        {
            // Square colour: (file + rank) % 2.
            // LERF: index = rank*8 + file, so index % 2 = file % 2 (rank*8 is
            // always even) — NOT the same as (file+rank)%2. The correct parity
            // is (file XOR rank) & 1 = (index XOR (index>>3)) & 1.
            let w_sq = white_bishops.lsb().expect("count==1 guarantees a set bit");
            let b_sq = black_bishops.lsb().expect("count==1 guarantees a set bit");
            let colour_parity = |sq: crate::Square| {
                let idx = sq.index();
                (idx ^ (idx >> 3)) & 1
            };
            colour_parity(w_sq) == colour_parity(b_sq)
        }
        _ => false,
    }
}

/// True iff the last entry of `history` appears at least three times in
/// the full history (current + 2 prior occurrences — FIDE 9.2).
///
/// The walk is **whole-history**, not since-last-irreversible-move.
/// FIDE 9.2 counts occurrences across the full game record; restricting
/// the walk to since-last-irreversible would break FIDE-correctness.
/// Equal Zobrist keys guarantee equal position state (see plan §3.2 for
/// the proof). DO NOT "fix" this to a bounded walk.
pub(crate) fn is_threefold_repetition_for_adjudication(history: &[u64]) -> bool {
    let Some(&current) = history.last() else {
        return false;
    };
    let count = history.iter().filter(|&&h| h == current).count();
    count >= 3
}

/// **Just-moved-side discipline.** Called after the side `mover` plays a move
/// and pushes its score onto its history. Returns `true` if `mover` should
/// resign — its trailing `movecount` scores are all at-or-below
/// `-score_threshold` (Cp) or are losing-mate (`Mate(n)` with `n < 0`).
/// Caller wraps the result as `GameOver::ResignAdjudicated(mover)`.
///
/// `mover_history.len() < movecount` → returns `false`.
/// `None` entries break the streak.
/// `Mate(n)` with `n >= 0` does NOT resign (engine sees winning mate).
pub(crate) fn resign_threshold_check(
    mover_history: &[Option<driver::Score>],
    movecount: u32,
    score_threshold: i32,
) -> bool {
    let n = movecount as usize;
    if mover_history.len() < n {
        return false;
    }
    let window = &mover_history[mover_history.len() - n..];
    window.iter().all(|entry| match entry {
        Some(driver::Score::Cp(s)) => *s <= -score_threshold,
        Some(driver::Score::Mate(n)) => *n < 0,
        None => false,
    })
}

/// Both sides agree on a near-zero score for `movecount` consecutive own-moves
/// each, and the current `move_number` (1-based full-move) is ≥ `movenumber_floor`.
///
/// `Score::Cp(s)` with `|s| ≤ score_threshold` qualifies. `Score::Mate(_)`
/// is treated as a non-balanced score regardless of inner value — mate is
/// by definition not a near-zero evaluation, so the impl matches Cp
/// explicitly and treats Mate(_) as breaking the streak. (Note: `Mate(n)`
/// carries plies-to-mate, not a centipawn-scaled score, so a |inner| ≤ thr
/// shortcut would be wrong for small `n`. Pinned by `draw_mate_score_breaks_streak`.)
/// `None` breaks the streak. Either side's history shorter than `movecount`
/// → returns `false`.
pub(crate) fn draw_threshold_check(
    white_history: &[Option<driver::Score>],
    black_history: &[Option<driver::Score>],
    move_number: u32,
    movenumber_floor: u32,
    movecount: u32,
    score_threshold: i32,
) -> bool {
    if move_number < movenumber_floor {
        return false;
    }
    let n = movecount as usize;
    if white_history.len() < n || black_history.len() < n {
        return false;
    }
    let is_balanced = |history: &[Option<driver::Score>]| {
        let window = &history[history.len() - n..];
        window.iter().all(|entry| match entry {
            Some(driver::Score::Cp(s)) => s.abs() <= score_threshold,
            _ => false,
        })
    };
    is_balanced(white_history) && is_balanced(black_history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, Move, Position};

    // ---------------------------------------------------------------
    // is_threefold_repetition_for_adjudication unit tests (implemented)
    // ---------------------------------------------------------------

    #[test]
    fn is_threefold_repetition_for_adjudication_single_entry_returns_false() {
        assert!(!is_threefold_repetition_for_adjudication(&[42]));
    }

    #[test]
    fn is_threefold_repetition_for_adjudication_two_entries_returns_false() {
        // FIDE requires three occurrences, not two.
        assert!(!is_threefold_repetition_for_adjudication(&[7, 7]));
    }

    #[test]
    fn is_threefold_repetition_for_adjudication_three_entries_returns_true() {
        assert!(is_threefold_repetition_for_adjudication(&[7, 7, 7]));
    }

    #[test]
    fn is_threefold_repetition_for_adjudication_counts_across_intervening() {
        // FIDE 9.2 counts position occurrences across the full game
        // record; intervening positions do NOT reset the count.
        //
        // History layout: index 0 = position `1` (call it `a`),
        //                 index 1 = position `7` (call it `h`, occurrence 1),
        //                 index 2 = position `2` (call it `b`),
        //                 index 3 = position `7` (h, occurrence 2),
        //                 index 4 = position `7` (h, occurrence 3 = current).
        //
        // The "last entry" examined by the helper is `history.last()` =
        // `7` (at index 4). Counting `7`s across the full slice = 3,
        // satisfying FIDE 9.2's three-occurrence rule even though the
        // occurrences are not contiguous.
        assert!(is_threefold_repetition_for_adjudication(&[1, 7, 2, 7, 7]));
    }

    // ---------------------------------------------------------------
    // detect_native_game_over tests (bodies todo!() until impl phase)
    // ---------------------------------------------------------------

    fn fool_position() -> (Position, Vec<u64>) {
        // Fool's mate: 1. f3 e5 2. g4 Qh4#
        // After White plays f2f3, Black plays e7e5, White plays g2g4,
        // Black plays d8h4 — White king is checkmated.
        let mut pos = Position::starting_position();
        let mut history = vec![pos.zobrist()];
        for uci in ["f2f3", "e7e5", "g2g4", "d8h4"] {
            let mv = Move::from_uci(uci, &pos).unwrap();
            pos.make_move(mv);
            history.push(pos.zobrist());
        }
        (pos, history)
    }

    #[test]
    fn mate_in_two_fool_known_position() {
        let (pos, history) = fool_position();
        // After d8h4, it is White's turn and White is in checkmate.
        // detect_native_game_over should return Checkmate(Black) — Black
        // delivered the mate.
        let result = detect_native_game_over(&pos, &history);
        match result {
            Some(GameOver::Checkmate(Color::Black)) => {}
            other => panic!(
                "expected Checkmate(Black), got {:?}",
                other.map(|_| "Some(...)")
            ),
        }
    }

    #[test]
    fn stalemate_classic_kp_known_position() {
        // Classic K+P stalemate: White Kb6, White Pa7, Black Ka8.
        // Black to move has no legal moves and is not in check:
        //  - Ka8→a7: attacked by Kb6 (illegal)
        //  - Ka8→b7: attacked by Kb6 (illegal)
        //  - Ka8→b8: attacked by both Kb6 (no, distance 2) AND Pa7 (yes, pawn
        //    attacks b8 diagonally) → illegal
        //  - Kxa7 (capture pawn): attacked by Kb6 (illegal)
        // FEN: k7/P7/1K6/8/8/8/8/8 b - - 0 1
        let pos = Position::from_fen("k7/P7/1K6/8/8/8/8/8 b - - 0 1").unwrap();
        let history = vec![pos.zobrist()];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::Stalemate)),
            "expected Stalemate, got {result:?}"
        );
    }

    #[test]
    fn fifty_move_at_halfclock_100() {
        // Position with halfmove_clock=100 and at least one legal move (no mate/stalemate).
        // Use a quiet endgame with legal moves: KQ vs K, halfmove 100.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 100 50").unwrap();
        let history = vec![pos.zobrist()];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::FiftyMove)),
            "expected FiftyMove"
        );
    }

    #[test]
    fn fifty_move_at_halfclock_99_returns_none() {
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 99 50").unwrap();
        let history = vec![pos.zobrist()];
        // At 99, the 50-move rule has not been triggered. KQK is not
        // insufficient (queen can mate). History has one entry — no
        // threefold. So the only correct answer is None.
        let result = detect_native_game_over(&pos, &history);
        assert!(
            result.is_none(),
            "expected None at halfclock=99 with KQK and single-entry history; got {result:?}"
        );
    }

    #[test]
    fn threefold_via_history_three_occurrences() {
        // Position with a threefold-claimable history. Use a quiet endgame
        // with legal moves, halfclock < 100. The history is synthetic.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 0 1").unwrap();
        let z = pos.zobrist();
        // Construct a history where the current position appears 3 times.
        let history = vec![99u64, z, 88u64, z, z];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::ThreefoldRepetition)),
            "expected ThreefoldRepetition"
        );
    }

    #[test]
    fn threefold_only_two_occurrences_returns_none() {
        // Pins the FIDE-3-vs-search-1 distinction: only 2 occurrences → no threefold.
        // KQK at halfmove=0 with 2-entry history has no mate/stalemate (kings have
        // legal moves), no fifty-move (halfmove=0), no threefold (2 < 3), no
        // insufficient material (KQK is sufficient). Only correct answer: None.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 0 1").unwrap();
        let z = pos.zobrist();
        let history = vec![99u64, z, z]; // last entry z appears twice
        let result = detect_native_game_over(&pos, &history);
        assert!(
            result.is_none(),
            "expected None with only 2 occurrences of current zobrist; got {result:?}"
        );
    }

    // ---------------------------------------------------------------
    // Insufficient material tests
    // ---------------------------------------------------------------

    #[test]
    fn insufficient_kk() {
        // Only kings: always insufficient.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
        assert!(is_insufficient_material(&pos), "KK should be insufficient");
    }

    #[test]
    fn insufficient_kbk() {
        // King + Bishop vs King.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/5B2 w - - 0 1").unwrap();
        assert!(is_insufficient_material(&pos), "KBK should be insufficient");
    }

    #[test]
    fn insufficient_knk() {
        // King + Knight vs King.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/5N2 w - - 0 1").unwrap();
        assert!(is_insufficient_material(&pos), "KNK should be insufficient");
    }

    #[test]
    fn insufficient_kbkb_same_colour() {
        // K + B (light sq) vs K + B (light sq): same-colour bishops are
        // insufficient. Bishops on c1 and c8 are both on dark squares
        // ((file+rank) & 1: c=2, 1=0 → 0; c=2, 8=7 → 1, that's not same...).
        // Let's use bishops on f1 (light) and f8 (light): f=5, 1=0 → odd=light; f=5, 8=7→ even=dark.
        // Actually: square colour = (file_idx + rank_idx) % 2.
        // f1: file=5, rank=0, sum=5 → odd = dark square
        // f8: file=5, rank=7, sum=12 → even = light square. Different!
        // Let's use c1 and a3: c=2, 1=0, sum=2→even=light; a=0, 3=2, sum=2→even=light. Same!
        // FEN for Kc3 Bc1 vs Ke5 Ba3: "8/8/8/4k3/8/b1K5/8/2B5 w - - 0 1"
        let pos = Position::from_fen("8/8/8/4k3/8/b1K5/8/2B5 w - - 0 1").unwrap();
        assert!(
            is_insufficient_material(&pos),
            "KBKB same-colour should be insufficient"
        );
    }

    #[test]
    fn not_insufficient_kbkb_opposite_colour() {
        // K + B (light sq) vs K + B (dark sq): different-colour bishops can
        // deliver checkmate with co-operation.
        // c1=light (file=2,rank=0,sum=2=even); d1=dark (file=3,rank=0,sum=3=odd).
        // FEN: Ke5 Bc1 vs Ke1 Bd8: too many kings on same rank. Let's use:
        // White: Kc3 Bc1, Black: Ke5 Bd8
        // c1: file=2, rank=0, sum=2 → even = light
        // d8: file=3, rank=7, sum=10 → even = light — same again!
        // Let me carefully pick: c1 (file 2, rank 0): (2+0)%2=0=light
        //                        d2 (file 3, rank 1): (3+1)%2=0=light — same
        //                        d1 (file 3, rank 0): (3+0)%2=1=dark  ← different from c1!
        // FEN: White Kc3 Bc1, Black Ke5 Bd1
        // "8/8/8/4k3/8/2K5/8/2Bb4 w - - 0 1" — b on d1, B on c1
        let pos = Position::from_fen("8/8/8/4k3/8/2K5/8/2Bb4 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KBKB opposite-colour should NOT be insufficient"
        );
    }

    #[test]
    fn not_insufficient_knkn() {
        // K+N vs K+N: theoretically winnable per FIDE (mating positions
        // exist via cooperation). Black knight on d5, white knight on f1.
        let pos = Position::from_fen("8/8/8/3nk3/8/4K3/8/5N2 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KNvKN should NOT be insufficient"
        );
    }

    #[test]
    fn not_insufficient_two_knights_vs_king() {
        // K+N+N vs K: a helpmate exists; not insufficient by FIDE.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/4NN2 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KNNK should NOT be insufficient"
        );
    }

    #[test]
    fn not_insufficient_kpvk() {
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/4P3/8 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KPK should NOT be insufficient"
        );
    }

    #[test]
    fn not_insufficient_krkr() {
        let pos = Position::from_fen("r7/8/8/4k3/8/4K3/8/R7 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KRKR should NOT be insufficient"
        );
    }

    #[test]
    fn not_insufficient_kbnk_vs_lone_king() {
        // K + B + N vs K — two minors on one side. Mateable per FIDE
        // (KBN-vs-K is the classic technique).
        //
        // Pins the `+` → `-` mutation on `white_minors = white_bishops.count()
        // + white_knights.count()`. Under correct +: white_minors = 1+1 = 2,
        // black_minors = 0; match (2,0) falls to `_ => false`. Under buggy -:
        // white_minors = 1-1 = 0, black_minors = 0; match (0,0) returns true
        // (FALSELY declares insufficient). The existing 1-or-0-minor tests
        // don't distinguish + from -.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/3BN3 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "K+B+N vs K should NOT be insufficient (KBNK is a forced mate per FIDE)"
        );
    }

    #[test]
    fn not_insufficient_kbk_vs_kn_one_each() {
        // K + B (white) vs K + N (black) — one minor each, but DIFFERENT
        // kinds. Pins the first `&&` → `||` mutation on the (1,1) match
        // arm guard `white_bishops.count() == 1 && black_bishops.count() == 1
        // && white_knights.is_empty() && black_knights.is_empty()`.
        //
        // Under correct &&: guard is false (black_bishops != 1) → falls to
        // `_ => false`. Position is mateable in cooperation; not insufficient.
        //
        // Under buggy `||` on first conjunct: `1==1 || 0==1 && 0==0 && 1==0`
        // = `true || (...)` = true. Body runs `black_bishops.lsb()` which is
        // None (no black bishop) → `expect("count==1 guarantees a set bit")`
        // panics. Test catches the mutation via the panic.
        let pos = Position::from_fen("8/8/8/3nk3/8/4K3/8/2B5 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KBvKN with one minor each side should NOT be insufficient (different kinds)"
        );
    }

    #[test]
    fn not_insufficient_kn_vs_kb_one_each() {
        // K + N (white) vs K + B (black) — mirror of the above. Pins the
        // THIRD `&&` → `||` mutation on the (1,1) guard.
        //
        // Original guard for white_bishops=0,black_bishops=1,white_knights=1,
        // black_knights=0:
        //   `0==1 && 1==1 && 1==0 && 0==0` = false (first false short-circuits).
        //   Falls to `_ => false`. Returns false.
        //
        // Mutation #3 (third `&&` → `||`): `0==1 && 1==1 && 1==0 || 0==0`.
        // Per Rust precedence (`&&` tighter than `||`):
        //   `(0==1 && 1==1 && 1==0) || (0==0)` = `false || true` = TRUE.
        // Mutated guard fires → enters body → `let w_sq = white_bishops.lsb()`
        // is None (no white bishop) → `expect("count==1 guarantees a set bit")`
        // panics. Test catches via panic.
        //
        // Note: mutations #1 and #2 are also tested implicitly here.
        // #1 (`0==1 || 1==1 && 1==0 && 0==0`) = `false || (true && false && true)` = false → SAME as original. Not caught.
        // #2 (`0==1 && 1==1 || 1==0 && 0==0`) = `false || false` = false → SAME. (Equivalent — see mutants.toml.)
        // Only #3 differs and is caught here.
        let pos = Position::from_fen("8/8/8/3bk3/8/4K3/8/2N5 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KNvKB with one minor each side should NOT be insufficient (different kinds)"
        );
    }

    #[test]
    fn not_insufficient_k_vs_kbn_pins_black_side_minor_count() {
        // Mirror of `not_insufficient_kbnk_vs_lone_king`: black has K+B+N,
        // white has only K. Pins the BLACK-SIDE `+` → `-` mutation on
        // `let black_minors = black_bishops.count() + black_knights.count()`.
        //
        // Under correct +: black_minors = 1 + 1 = 2; white_minors = 0;
        // match (0, 2) → `_ => false` (correct: KBN-vs-K is not insufficient).
        //
        // Under buggy -: black_minors = 1 - 1 = 0 (wrapping or saturating);
        // match (0, 0) → returns true (FALSELY declares insufficient).
        //
        // The white-side analogue is pinned by `not_insufficient_kbnk_vs_lone_king`;
        // this test catches the symmetric black-side mutation.
        let pos = Position::from_fen("3bn3/8/4k3/8/4K3/8/8/8 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "K vs K+B+N should NOT be insufficient (KBN can mate per FIDE)"
        );
    }

    // These two tests specifically target the bishop-colour parity bug
    // where `index % 2` (file parity only) differs from `(file+rank) % 2`
    // (true square colour). a2 and b1 have different file parities (0 vs 1)
    // but are both on light squares because (0+1)%2 == (1+0)%2 == 1.

    #[test]
    fn insufficient_kbkb_same_colour_a2_b1_different_file_parity() {
        // Bw on a2 (file=0, rank=1) → light square: (0+1)%2 = 1.
        // Bb on b1 (file=1, rank=0) → light square: (1+0)%2 = 1.
        // Same colour, but different file parity → old `index % 2` formula
        // would have returned false (wrongly declaring sufficient material).
        // FEN: "8/8/8/4k3/8/8/B7/1bK5 w - - 0 1" — Bw=a2, Bb=b1, Wk=c1, Bk=e5.
        let pos = Position::from_fen("8/8/8/4k3/8/8/B7/1bK5 w - - 0 1").unwrap();
        assert!(
            is_insufficient_material(&pos),
            "KBKB same-colour (a2 light, b1 light, different file parity) \
             must be insufficient; old index%2 formula returned false here"
        );
    }

    #[test]
    fn not_insufficient_kbkb_opposite_colour_same_file_parity() {
        // Bw on a2 (file=0, rank=1) → light square: (0+1)%2 = 1.
        // Bb on a3 (file=0, rank=2) → dark square:  (0+2)%2 = 0.
        // Different colours, same file parity → old `index % 2` formula
        // would have returned true (wrongly declaring insufficient material).
        // FEN: "8/8/8/4k3/8/b7/B7/2K5 w - - 0 1" — Bw=a2, Bb=a3, Wk=c1, Bk=e5.
        let pos = Position::from_fen("8/8/8/4k3/8/b7/B7/2K5 w - - 0 1").unwrap();
        assert!(
            !is_insufficient_material(&pos),
            "KBKB opposite-colour (a2 light, a3 dark, same file parity) \
             must NOT be insufficient; old index%2 formula returned true here"
        );
    }

    #[test]
    fn insufficient_kbkb_same_colour_b2_a1_pins_xor_parity_formula() {
        // Bw on b2 (file=1, rank=1, LERF index=9): dark square — (1+1)%2 = 0.
        // Bb on a1 (file=0, rank=0, LERF index=0): dark square — (0+0)%2 = 0.
        // Same colour: position is insufficient material.
        //
        // This specifically pins the `(idx ^ (idx >> 3)) & 1` formula against
        // the `^ → |` mutation. For index=9: (9 ^ 1) & 1 = 8 & 1 = 0 (correct:
        // dark). Under `|`: (9 | 1) & 1 = 9 & 1 = 1 (incorrect: classified light).
        // The mutant would see parity(b2)=1 != parity(a1)=0 → returns false (wrong).
        //
        // FEN: white Bw=b2, Wk=c1, black Bb=a1, Bk=e6.
        let pos = Position::from_fen("8/8/4k3/8/8/8/1B6/b1K5 w - - 0 1").unwrap();
        assert!(
            is_insufficient_material(&pos),
            "KBKB same-colour (b2 dark, a1 dark) must be insufficient; \
             XOR parity formula bug would misclassify b2 as light"
        );
    }

    // ---------------------------------------------------------------
    // Precedence tests
    // ---------------------------------------------------------------

    #[test]
    fn precedence_mate_over_fifty() {
        // Side to move is checkmated AND halfmove_clock=100.
        // detect_native_game_over should return Checkmate, not FiftyMove.
        // Use Fool's mate position (it is checkmate) but force halfmove=100 by
        // using a FEN with that clock value.
        // After Qh4, White is mated. We fake the halfmove clock using FEN.
        // Fool's mate FEN after 1.f3 e5 2.g4 Qh4#:
        // "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3"
        // We override halfmove clock to 100:
        let pos =
            Position::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 100 3")
                .unwrap();
        let history = vec![pos.zobrist()];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::Checkmate(_))),
            "expected Checkmate, got {:?}",
            result.map(|_| "Some(...)")
        );
    }

    #[test]
    fn precedence_mate_over_threefold() {
        // Side to move is checkmated AND position appears 3× in history.
        let pos =
            Position::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 0 3")
                .unwrap();
        let z = pos.zobrist();
        let history = vec![z, 99u64, z, 88u64, z]; // current z appears 3×
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::Checkmate(_))),
            "expected Checkmate (not ThreefoldRepetition)"
        );
    }

    #[test]
    fn precedence_fifty_over_threefold() {
        // halfmove_clock=100 AND threefold-claimable history.
        // Use KQK where no checkmate/stalemate exists.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 100 50").unwrap();
        let z = pos.zobrist();
        let history = vec![z, 99u64, z, 88u64, z];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::FiftyMove)),
            "expected FiftyMove (not ThreefoldRepetition)"
        );
    }

    #[test]
    fn precedence_fifty_over_threefold_negative_pin() {
        // Companion to `precedence_fifty_over_threefold`: the same KQK
        // position with halfmove_clock=99 (not fifty-triggering) and the
        // same threefold-claimable history must return ThreefoldRepetition.
        // This proves the threefold detection is operational — without it,
        // the prior test could have passed against an impl that only
        // detects fifty-move and never reaches threefold at all.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 99 50").unwrap();
        let z = pos.zobrist();
        let history = vec![z, 99u64, z, 88u64, z];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::ThreefoldRepetition)),
            "expected ThreefoldRepetition at halfclock=99 with threefold-claimable history; got {result:?}"
        );
    }

    #[test]
    fn precedence_threefold_over_insufficient() {
        // Threefold-claimable (current position appears 3× in history) AND
        // insufficient material (KK). Under FIDE both call the game a draw;
        // the harness's precedence determines the Termination tag value.
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
        let z = pos.zobrist();
        let history = vec![z, 99u64, z, 88u64, z];
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::ThreefoldRepetition)),
            "expected ThreefoldRepetition (not InsufficientMaterial)"
        );
    }

    #[test]
    fn kk_with_no_repetition_returns_insufficient_via_detect_fn() {
        // Companion to `precedence_threefold_over_insufficient`: the same
        // KK position with a non-threefold history must return
        // InsufficientMaterial. This proves the insufficient-material
        // branch is reachable through `detect_native_game_over` (the prior
        // test exercises `is_insufficient_material` directly via
        // `insufficient_kk`, but does not pin that `detect_native_game_over`
        // routes to it correctly).
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
        let history = vec![pos.zobrist()]; // only 1 occurrence — no threefold
        let result = detect_native_game_over(&pos, &history);
        assert!(
            matches!(result, Some(GameOver::InsufficientMaterial)),
            "expected InsufficientMaterial for KK with no threefold; got {result:?}"
        );
    }

    // ---------------------------------------------------------------
    // §6.3 Threshold adjudication tests (todo!() until impl phase)
    // ---------------------------------------------------------------

    use super::super::driver::Score::{Cp, Mate};

    #[test]
    fn resign_three_consecutive_below_threshold_fires() {
        assert!(resign_threshold_check(
            &[Some(Cp(-700)), Some(Cp(-650)), Some(Cp(-720))],
            3,
            600
        ));
    }

    #[test]
    fn resign_two_below_one_above_does_not_fire() {
        // The trailing entry Cp(-100) is above −600 threshold, so streak breaks.
        assert!(!resign_threshold_check(
            &[Some(Cp(-700)), Some(Cp(-650)), Some(Cp(-100))],
            3,
            600
        ));
    }

    #[test]
    fn resign_negative_mate_score_fires() {
        // Mate(n) with n < 0 means mover gets mated; counts as losing.
        assert!(resign_threshold_check(
            &[Some(Mate(-3)), Some(Mate(-4)), Some(Mate(-5))],
            3,
            600
        ));
    }

    #[test]
    fn resign_positive_mate_does_not_fire() {
        // Mate(n) with n > 0 means mover is winning; must NOT resign.
        assert!(!resign_threshold_check(
            &[Some(Mate(3)), Some(Mate(2)), Some(Mate(1))],
            3,
            600
        ));
    }

    #[test]
    fn resign_none_entry_breaks_streak() {
        // None in the trailing window breaks the streak even if flanking entries qualify.
        assert!(!resign_threshold_check(
            &[Some(Cp(-700)), None, Some(Cp(-720))],
            3,
            600
        ));
    }

    #[test]
    fn resign_short_history_returns_false() {
        // History length < movecount must return false without panicking.
        assert!(!resign_threshold_check(
            &[Some(Cp(-700)), Some(Cp(-650))],
            3,
            600
        ));
    }

    #[test]
    fn resign_exact_threshold_fires() {
        // Pins ≤ (not <) at the boundary: score = −threshold should resign.
        assert!(resign_threshold_check(
            &[Some(Cp(-600)), Some(Cp(-600)), Some(Cp(-600))],
            3,
            600
        ));
    }

    #[test]
    fn resign_just_above_threshold_does_not_fire() {
        // Cp(-599) with threshold 600: |-599| = 599, not ≤ -threshold (i.e. -599 > -600).
        // Pins the boundary on the OTHER side from `resign_exact_threshold_fires`.
        assert!(!resign_threshold_check(
            &[
                Some(driver::Score::Cp(-599)),
                Some(driver::Score::Cp(-599)),
                Some(driver::Score::Cp(-599))
            ],
            3,
            600
        ));
    }

    #[test]
    fn draw_eight_consecutive_balanced_after_movenumber_fires() {
        // Both sides have 8 balanced entries, move_number ≥ movenumber_floor.
        let white_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
        ];
        let black_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-5)),
            Some(Cp(5)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
        ];
        assert!(draw_threshold_check(
            &white_hist,
            &black_hist,
            40,
            34,
            8,
            20
        ));
    }

    #[test]
    fn draw_before_movenumber_does_not_fire() {
        // Same balanced history but move_number < movenumber_floor → false.
        let white_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
        ];
        let black_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-5)),
            Some(Cp(5)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
        ];
        assert!(!draw_threshold_check(
            &white_hist,
            &black_hist,
            30,
            34,
            8,
            20
        ));
    }

    #[test]
    fn draw_one_side_above_threshold() {
        // Black has one entry Cp(-50): |−50| = 50 > threshold 20, breaks streak.
        let white_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
        ];
        let black_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-5)),
            Some(Cp(5)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(-50)), // breaks the balanced streak
        ];
        assert!(!draw_threshold_check(
            &white_hist,
            &black_hist,
            40,
            34,
            8,
            20
        ));
    }

    #[test]
    fn draw_mate_score_breaks_streak() {
        // Mate(_) anywhere in the trailing window of either side breaks the streak,
        // regardless of sign: mate is by definition not a near-zero evaluation.
        let balanced: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
        ];
        let white_mate: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Mate(5)), // winning mate in white's history breaks draw streak
        ];
        let black_mate: Vec<Option<driver::Score>> = vec![
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-5)),
            Some(Cp(5)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Mate(-3)), // losing mate in black's history breaks draw streak
        ];
        assert!(!draw_threshold_check(&white_mate, &balanced, 40, 34, 8, 20));
        assert!(!draw_threshold_check(&balanced, &black_mate, 40, 34, 8, 20));
    }

    #[test]
    fn draw_short_history_either_side_returns_false() {
        // Either side having fewer than movecount entries → false.
        let short: Vec<Option<driver::Score>> = vec![Some(Cp(10)), Some(Cp(-10))];
        let enough: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
        ];
        assert!(!draw_threshold_check(&short, &enough, 40, 34, 3, 20));
        assert!(!draw_threshold_check(&enough, &short, 40, 34, 3, 20));
    }

    #[test]
    fn draw_none_entry_breaks_streak() {
        // None in the trailing window on either side breaks the streak.
        let with_none: Vec<Option<driver::Score>> = vec![
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(5)),
            Some(Cp(-5)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
            None, // breaks streak
        ];
        let balanced: Vec<Option<driver::Score>> = vec![
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-5)),
            Some(Cp(5)),
            Some(Cp(-10)),
            Some(Cp(10)),
            Some(Cp(-10)),
            Some(Cp(10)),
        ];
        assert!(!draw_threshold_check(&with_none, &balanced, 40, 34, 8, 20));
        assert!(!draw_threshold_check(&balanced, &with_none, 40, 34, 8, 20));
    }

    #[test]
    fn draw_exact_threshold_fires() {
        // Pins |s| ≤ thr (not <): all entries ±20 with threshold=20 should fire.
        let white_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
        ];
        let black_hist: Vec<Option<driver::Score>> = vec![
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
            Some(Cp(-20)),
            Some(Cp(20)),
        ];
        assert!(draw_threshold_check(
            &white_hist,
            &black_hist,
            40,
            34,
            8,
            20
        ));
    }

    // ---- ELOH.B Tier-C targeted tests --------------------------------

    #[test]
    fn resign_slice_uses_subtraction_not_division() {
        // Pins the `-` in `mover_history[len - movecount..]` against the
        // `/` mutant.  History has 6 entries; movecount=3.
        //   Correct (-): window = history[3..6] → entries 3,4,5 (all ≤ −600) → fires.
        //   Mutant  (/): window = history[6/3..6] = history[2..6] → entry at
        //                index 2 is Cp(-100), which is not ≤ −600 → does NOT fire.
        let history = vec![
            Some(Cp(-100)), // index 0 — not in correct window
            Some(Cp(-100)), // index 1 — not in correct window
            Some(Cp(-100)), // index 2 — not in correct window; IS in mutant window
            Some(Cp(-700)), // index 3 — in correct window
            Some(Cp(-650)), // index 4 — in correct window
            Some(Cp(-720)), // index 5 — in correct window
        ];
        assert!(
            resign_threshold_check(&history, 3, 600),
            "last 3 entries all ≤ −600: must fire"
        );
    }

    #[test]
    fn resign_mate_zero_does_not_fire() {
        // Mate(0) means the side to move IS already mated (ply-to-mate = 0).
        // But this is an edge-case value; the original guard `n < 0` returns
        // false for n=0, so the streak is broken.
        // Mutant `< 0` → `<= 0` would treat Mate(0) as a "losing" score and
        // fire when three Mate(0) entries are present.
        assert!(
            !resign_threshold_check(&[Some(Mate(0)), Some(Mate(0)), Some(Mate(0))], 3, 600),
            "Mate(0) is not < 0 so it must NOT trigger resign"
        );
    }

    #[test]
    fn draw_at_movenumber_floor_fires() {
        // Pins the `<` in `move_number < movenumber_floor` against the `<=`
        // mutant.  move_number == movenumber_floor: original returns true
        // (condition false → doesn't short-circuit); mutant returns false
        // (condition true → early return false).
        let balanced: Vec<Option<driver::Score>> = vec![Some(Cp(5)), Some(Cp(-5)), Some(Cp(5))];
        assert!(
            draw_threshold_check(&balanced, &balanced, 34, 34, 3, 20),
            "move_number == movenumber_floor must fire (not be rejected by < guard)"
        );
    }

    #[test]
    fn draw_slice_uses_subtraction_not_division() {
        // Pins the `-` in `history[history.len() - n..]` against the `/`
        // mutant for `draw_threshold_check`.  Each history has 6 entries;
        // movecount n = 3.
        //   Correct (-): window = history[3..6] → entries 3,4,5 all balanced → fires.
        //   Mutant  (/): window = history[6/3..6] = history[2..6] → entry at
        //                index 2 has |s| > threshold → does NOT fire.
        let unbalanced_then_balanced: Vec<Option<driver::Score>> = vec![
            Some(Cp(0)),    // 0 — not in correct window
            Some(Cp(0)),    // 1 — not in correct window
            Some(Cp(-100)), // 2 — not in correct window; IS in mutant window (|s| > thr=20)
            Some(Cp(5)),    // 3 — in correct window (balanced)
            Some(Cp(-5)),   // 4 — in correct window (balanced)
            Some(Cp(5)),    // 5 — in correct window (balanced)
        ];
        assert!(
            draw_threshold_check(
                &unbalanced_then_balanced,
                &unbalanced_then_balanced,
                50,
                34,
                3,
                20
            ),
            "last 3 entries balanced: must fire"
        );
    }
}
