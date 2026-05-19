//! The pinned quiet predicate (M6.G↔M6.H interface contract).
//!
//! `QUIET ⟺ !in_check(pos) ∧ |static_eval_white − qsearch_white| <
//! QUIET_MARGIN_CP`. `static_eval_white` = white-POV `eval::evaluate`;
//! `qscore_white` = `search::QSearcher::eval_white` (the M6.G seam).
//!
//! Implemented by the M6.G `filter+quiet` coder slice per
//! `docs/plans/m6.g.md` §3.4.

use crate::Position;
use crate::eval::evaluate;
use crate::movegen::in_check;
use crate::piece::Color;

use super::QUIET_MARGIN_CP;

/// White-POV static eval (centipawns).
///
/// `eval::evaluate` is side-to-move-relative; negate for Black so the corpus
/// is White-POV (Texel needs White-positive for the `result_i` ∈ {0, 0.5, 1}
/// sigmoid).
pub fn static_eval_white(pos: &Position) -> i32 {
    let stm_score = evaluate(pos);
    if pos.side_to_move() == Color::White {
        stm_score
    } else {
        -stm_score
    }
}

/// The pinned predicate: a position is quiet when it is not in check **and**
/// the static eval is within `QUIET_MARGIN_CP` centipawns of the qsearch score.
///
/// Both `static_eval` and `qscore` are White-POV centipawns; the caller
/// supplies them so a per-worker [`crate::search::QSearcher`] can be reused
/// across positions (R6/R7 — no per-position searcher allocation).
pub fn is_quiet(_pos: &Position, static_eval: i32, qscore: i32) -> bool {
    !in_check(_pos) && (static_eval - qscore).abs() < QUIET_MARGIN_CP
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{HIGH_SCORE_CP, OPENING_SKIP_PLIES, QUIET_MARGIN_CP};
    use crate::search::QSearcher;

    // -----------------------------------------------------------------------
    // interface_constants_golden — pins the M6.G↔M6.H contract values
    // -----------------------------------------------------------------------
    #[test]
    fn interface_constants_golden() {
        assert_eq!(QUIET_MARGIN_CP, 30);
        assert_eq!(OPENING_SKIP_PLIES, 8);
        assert_eq!(HIGH_SCORE_CP, 600);
    }

    // -----------------------------------------------------------------------
    // in_check_not_quiet
    // -----------------------------------------------------------------------
    #[test]
    fn in_check_not_quiet() {
        // White king on e1 attacked by black rook on e8; black king on h8.
        let pos = Position::from_fen("4r2k/8/8/8/8/8/8/4K3 w - - 0 1").expect("check FEN");
        assert!(in_check(&pos), "sanity: should be in check");
        // is_quiet must be false regardless of the eval/qscore pair.
        assert!(!is_quiet(&pos, 0, 0));
        assert!(!is_quiet(&pos, 100, 100));
    }

    // -----------------------------------------------------------------------
    // tactical_swing_ge_margin_not_quiet — |static - qscore| ≥ MARGIN ⇒ not quiet
    // -----------------------------------------------------------------------
    #[test]
    fn tactical_swing_ge_margin_not_quiet() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(!in_check(&pos));
        // Exactly at the boundary (== MARGIN): NOT admitted (predicate uses strict <).
        assert!(!is_quiet(&pos, 0, QUIET_MARGIN_CP));
        assert!(!is_quiet(&pos, 0, -QUIET_MARGIN_CP));
        // Well above the margin.
        assert!(!is_quiet(&pos, 0, QUIET_MARGIN_CP + 10));
        assert!(!is_quiet(&pos, QUIET_MARGIN_CP + 50, 0));
    }

    // -----------------------------------------------------------------------
    // swing_lt_margin_quiet — |static - qscore| < MARGIN ∧ !in_check ⇒ quiet
    // -----------------------------------------------------------------------
    #[test]
    fn swing_lt_margin_quiet() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(!in_check(&pos));
        // One below the boundary: admitted.
        assert!(is_quiet(&pos, 0, QUIET_MARGIN_CP - 1));
        assert!(is_quiet(&pos, 0, -(QUIET_MARGIN_CP - 1)));
        // Zero swing: clearly quiet.
        assert!(is_quiet(&pos, 100, 100));
        assert!(is_quiet(&pos, -50, -50));
    }

    // -----------------------------------------------------------------------
    // static_eval_white returns White-POV regardless of side to move
    // -----------------------------------------------------------------------
    #[test]
    fn static_eval_white_is_white_pov() {
        // For a symmetric-ish position, eval from White's turn and
        // Black's turn (same material) should be close in magnitude but
        // reflect the same White-POV score.
        //
        // Use a position where it's unambiguous: White has a queen, Black does not.
        // White to move: evaluate() = +big (positive for White)
        // Black to move (same board): evaluate() = -big (positive for Black's stm)
        // static_eval_white should be +big in both cases.
        let fen_w = "4k3/8/8/8/8/8/8/4KQ2 w - - 0 1";
        let fen_b = "4k3/8/8/8/8/8/8/4KQ2 b - - 0 1";
        let pos_w = Position::from_fen(fen_w).expect("FEN");
        let pos_b = Position::from_fen(fen_b).expect("FEN");

        let eval_w = static_eval_white(&pos_w);
        let eval_b = static_eval_white(&pos_b);

        // Both must be positive (White has a queen advantage).
        assert!(
            eval_w > 0,
            "White to move: eval should be positive, got {eval_w}"
        );
        assert!(
            eval_b > 0,
            "Black to move: static_eval_white should be positive, got {eval_b}"
        );
    }

    // -----------------------------------------------------------------------
    // End-to-end: real static_eval_white + QSearcher::eval_white
    // -----------------------------------------------------------------------

    /// A genuinely quiet position (no hanging pieces, no checks) should satisfy
    /// the predicate when evaluated with the real engine seams.
    #[test]
    fn end_to_end_quiet_position_is_quiet() {
        // Quiet middlegame position (no captures available, not in check).
        // e4 e5 position — standard pawn structure, all pieces developed enough
        // to have no hanging material.
        let pos = Position::from_fen(
            "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4",
        )
        .expect("Giuoco Piano FEN");
        assert!(!in_check(&pos));

        let se = static_eval_white(&pos);
        let qs = QSearcher::new().eval_white(&pos);
        // The is_quiet predicate must agree with the real engine scores.
        assert!(
            is_quiet(&pos, se, qs),
            "quiet position should pass: static={se}, qscore={qs}, margin={QUIET_MARGIN_CP}"
        );
    }

    /// A position with a hanging piece (immediate capture available) is typically
    /// not quiet. We construct one where White's queen is en prise.
    #[test]
    fn end_to_end_tactical_position_is_not_quiet() {
        // Black rook can take the White queen immediately.
        // Ke1, Qd1 vs Ke8, Rd8 — but we need the queen actually hanging (rook
        // can take it legally without retribution).
        // Simpler: White queen on d4 attacked by Black rook on d8, queen cannot
        // escape (surrounded), king elsewhere.
        // "3r4/8/8/8/3Q4/8/8/4K2k w - - 0 1"
        // White queen on d4, Black rook on d8, White king e1, Black king h1.
        // Black to move? No, let's make White to move and the rook is not
        // currently attacking the queen sideways but will be after a move.
        // Actually we need a position where BLACK is to move and can take the queen.
        let pos =
            Position::from_fen("3r4/8/8/8/3Q4/8/8/4K2k b - - 0 1").expect("hanging queen FEN");
        assert!(!in_check(&pos));

        let se = static_eval_white(&pos);
        let qs = QSearcher::new().eval_white(&pos);
        // static_eval sees White up a queen (roughly +900), but qsearch will
        // find the rook capture and score it very differently.
        let swing = (se - qs).abs();
        // We just document the relationship; the predicate should match.
        let expected_quiet = swing < QUIET_MARGIN_CP;
        assert_eq!(
            is_quiet(&pos, se, qs),
            expected_quiet,
            "hanging queen: static={se}, qscore={qs}, swing={swing}"
        );
        // The swing should be large (queen hanging), so not quiet.
        assert!(
            !expected_quiet,
            "hanging queen should not be quiet: static={se}, qscore={qs}, swing={swing}"
        );
    }
}
