//! Material + PST evaluation.
//!
//! `evaluate(&Position) -> i32` returns a side-to-move-relative score in
//! centipawns. The backing field `Position::static_eval_white` is maintained
//! incrementally by `make_move` / `unmake_move` (M3.A Slice 2); this module
//! provides the from-scratch recomputation used on construction, parse, and
//! in debug-build round-trip asserts.
//!
//! PST data: PeSTO middlegame tables, vendored verbatim (Slice 1).
//! Insufficient-material detection: KvK, KvN, KvB → 0 (see plan §4).

use crate::piece::{Color, PieceKind};
use crate::position::Position;

mod data;
use data::{
    MATERIAL, MG_BISHOP_PST, MG_KING_PST, MG_KNIGHT_PST, MG_PAWN_PST, MG_QUEEN_PST, MG_ROOK_PST,
};

// ---------------------------------------------------------------------------
// PSQT — preflattened lookup table (plan §1).
//
// PSQT[color][kind][sq] is the signed contribution of a (color, kind, sq)
// piece to `static_eval_white`.
//   White: +(MATERIAL[kind] + MG_PST[kind][sq ^ 56])
//   Black: -(MATERIAL[kind] + MG_PST[kind][sq])
//
// Vendored data lives in `src/eval/data.rs` (excluded from cargo-mutants).
// PeSTO arrays use a8-origin indexing; `build_psqt` applies the LERF ↔ PeSTO
// flip via `sq ^ 56` for white-piece lookups.
// ---------------------------------------------------------------------------

/// `PSQT[color_idx][kind_idx][sq_idx]`. Indexed by `Color as usize`,
/// `PieceKind as usize`, and `Square::index() as usize`.
pub(crate) const PSQT: [[[i32; 64]; 6]; 2] = build_psqt();

/// Build the PSQT from `MATERIAL` and the six `MG_*_PST` arrays.
/// Called once at compile time; result is a constant.
const fn build_psqt() -> [[[i32; 64]; 6]; 2] {
    // Per-kind MG PST tables, indexed by PieceKind::index() (P=0..K=5).
    const MG_TABLES: [[i32; 64]; 6] = [
        MG_PAWN_PST,
        MG_KNIGHT_PST,
        MG_BISHOP_PST,
        MG_ROOK_PST,
        MG_QUEEN_PST,
        MG_KING_PST,
    ];

    let mut psqt = [[[0i32; 64]; 6]; 2];
    let mut kind = 0usize;
    while kind < 6 {
        let mut sq = 0usize;
        while sq < 64 {
            let mat = MATERIAL[kind];
            // White lookup: sq ^ 56 converts LERF index to PeSTO a8-origin index.
            psqt[Color::White as usize][kind][sq] = mat + MG_TABLES[kind][sq ^ 56];
            // Black lookup: negate; use sq directly (PeSTO a8-origin = LERF index for black).
            psqt[Color::Black as usize][kind][sq] = -(mat + MG_TABLES[kind][sq]);
            sq += 1;
        }
        kind += 1;
    }
    psqt
}

// Ensure build_psqt is not dead code.
const _BUILD_PSQT_COMPILES: [[[i32; 64]; 6]; 2] = build_psqt();

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Recompute `static_eval_white` from scratch. Iterates all pieces and sums
/// `PSQT[color][kind][sq]` for each occupied square. Used by
/// `Position::refresh_static_eval`, by debug-build round-trip asserts in
/// `make_move` / `unmake_move`, and by the eval tests.
pub(crate) fn eval_white_from_scratch(pos: &Position) -> i32 {
    let mut score = 0i32;
    for kind in PieceKind::ALL {
        for color in [Color::White, Color::Black] {
            let mut bb = pos.pieces_colored(color, kind);
            while let Some(sq) = bb.pop_lsb() {
                score += PSQT[color as usize][kind.index()][sq.index() as usize];
            }
        }
    }
    score
}

/// Side-to-move-relative score in centipawns.
///
/// Returns 0 for insufficient-material positions (KvK, KvN, KvB).
/// Otherwise returns `static_eval_white` negated for Black.
pub fn evaluate(pos: &Position) -> i32 {
    let total_count = pos.occupied_all().count();

    // KvK: only the two kings remain.
    if total_count == 2 {
        return 0;
    }

    if total_count == 3 {
        // KvN or KvB: exactly one minor piece besides the two kings.
        // The "two kings" half is guaranteed by `validate_post_parse` (FEN
        // parser rejects positions without exactly one king per color), so
        // total_count == 3 implies exactly one non-king piece.
        let knight_count = pos.pieces(PieceKind::Knight).count();
        let bishop_count = pos.pieces(PieceKind::Bishop).count();
        if knight_count == 1 || bishop_count == 1 {
            return 0;
        }
    }

    let e = pos.static_eval_white();
    if pos.side_to_move() == Color::White {
        e
    } else {
        -e
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::{Color, PieceKind};
    use crate::position::Position;
    use crate::square::Square;

    // -----------------------------------------------------------------------
    // A1 — starting position evaluates to 0 (color symmetry).
    // -----------------------------------------------------------------------

    #[test]
    fn eval_starting_position_is_zero() {
        let pos = Position::starting_position();
        assert_eq!(
            evaluate(&pos),
            0,
            "starting position must evaluate to 0 (symmetric material + PST)"
        );
    }

    // -----------------------------------------------------------------------
    // A2 — e-pawn missing → ±67 cp.
    // -----------------------------------------------------------------------

    /// Starting position minus white's e2 pawn → -67 for white to move (pawn
    /// down). Starting position minus black's e7 pawn → +67 for white to move
    /// (pawn up). Pins the combined material+PST contribution at e2/e7.
    ///
    /// Derivation: e2 is sq=12 (LERF). White lookup is
    /// `+(MATERIAL[Pawn] + MG_PAWN_PST[12 ^ 56])` = `+(82 + MG_PAWN_PST[52])`.
    /// MG_PAWN_PST[52] is PeSTO row 6, col 4 (e-file) = -15 (from
    /// `docs/research/m3-eval-material-pst.md` "MG Pawn PST" row
    /// `-35, -1, -20, -23, -15, 24, 38, -22`). Total = +(82 + (-15)) = +67.
    ///
    /// For e7 (sq=52): Black lookup is `-(82 + MG_PAWN_PST[52]) = -67`.
    /// Both positions have symmetric non-e-pawn material, so the net eval
    /// is ±67 cp.
    #[test]
    fn eval_one_e_pawn_missing_is_67_cp() {
        // White is missing the e2 pawn (down one pawn). FEN built from
        // startpos by removing the e2 pawn from rank 2.
        let pos_white_pawn_missing =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1")
                .expect("FEN must parse");
        assert_eq!(
            evaluate(&pos_white_pawn_missing),
            -67,
            "white missing e2 pawn: evaluate must be -67 cp \
            (material 82 + PST[e2^56=52]=-15 → net 67, pawn down)"
        );

        // Black is missing the e7 pawn (white is up one pawn), white to move.
        let pos_black_pawn_missing =
            Position::from_fen("rnbqkbnr/pppp1ppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
                .expect("FEN must parse");
        assert_eq!(
            evaluate(&pos_black_pawn_missing),
            67,
            "black missing e7 pawn: evaluate must be +67 cp \
            (material 82 + PST[52]=-15 → net 67, pawn up)"
        );
    }

    // -----------------------------------------------------------------------
    // A3 — side-to-move perspective flip.
    // -----------------------------------------------------------------------

    /// Non-symmetric FEN; clone with side-to-move flipped (via FEN string
    /// edit). Assert `evaluate(white_to_move) == -evaluate(black_to_move)`.
    #[test]
    fn eval_perspective_flips_on_side_to_move() {
        // White up a pawn; white to move.
        let fen_w = "rnbqkbnr/pppp1ppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        // Same material imbalance but black to move (flip the 'w' to 'b').
        let fen_b = "rnbqkbnr/pppp1ppp/8/8/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1";
        let pos_w = Position::from_fen(fen_w).expect("FEN must parse");
        let pos_b = Position::from_fen(fen_b).expect("FEN must parse");
        assert_eq!(
            evaluate(&pos_w),
            -evaluate(&pos_b),
            "evaluate must be negated when side-to-move flips: white={}, black={}",
            evaluate(&pos_w),
            evaluate(&pos_b)
        );
    }

    // -----------------------------------------------------------------------
    // A4–A6 — insufficient-material positions → 0.
    // -----------------------------------------------------------------------

    #[test]
    fn eval_kvk_returns_zero() {
        let pos = Position::from_fen("8/8/8/4k3/8/8/8/4K3 w - - 0 1").expect("KvK FEN");
        assert_eq!(
            evaluate(&pos),
            0,
            "KvK must evaluate to 0 (insufficient material)"
        );
    }

    #[test]
    fn eval_kvn_returns_zero_either_color() {
        // White owns the knight.
        let pos_wn =
            Position::from_fen("8/8/8/4k3/8/8/8/4K2N w - - 0 1").expect("KvN (white knight) FEN");
        assert_eq!(
            evaluate(&pos_wn),
            0,
            "KvN (white knight) must evaluate to 0 (insufficient material)"
        );

        // Black owns the knight.
        let pos_bn =
            Position::from_fen("8/8/8/4k1n1/8/8/8/4K3 w - - 0 1").expect("KvN (black knight) FEN");
        assert_eq!(
            evaluate(&pos_bn),
            0,
            "KvN (black knight) must evaluate to 0 (insufficient material)"
        );
    }

    #[test]
    fn eval_kvb_returns_zero_either_color() {
        // White owns the bishop.
        let pos_wb =
            Position::from_fen("8/8/8/4k3/8/8/8/4K2B w - - 0 1").expect("KvB (white bishop) FEN");
        assert_eq!(
            evaluate(&pos_wb),
            0,
            "KvB (white bishop) must evaluate to 0 (insufficient material)"
        );

        // Black owns the bishop.
        let pos_bb =
            Position::from_fen("8/8/8/4k1b1/8/8/8/4K3 w - - 0 1").expect("KvB (black bishop) FEN");
        assert_eq!(
            evaluate(&pos_bb),
            0,
            "KvB (black bishop) must evaluate to 0 (insufficient material)"
        );
    }

    // -----------------------------------------------------------------------
    // A7 — KBvKB same-color bishops does NOT return zero (deferred per ADR-0014).
    // -----------------------------------------------------------------------

    /// Same-color bishops: c4 and e2 are both light squares (convention:
    /// `(file + rank) even ⇒ dark`; `(file + rank) odd ⇒ light`; e2 has
    /// file=4, rank=1, sum=5 odd → light; c4 has file=2, rank=3, sum=5 odd
    /// → light). M3.A deliberately does NOT detect this as insufficient
    /// material; this test pins that the implementation returns a non-zero
    /// evaluation — a regression would be a future M6 change that erroneously
    /// adds same-color-bishop detection at this phase.
    #[test]
    fn eval_kbvkb_does_not_return_zero() {
        // White bishop on e2 (sq=12, light square: file=4, rank=1, sum=5 odd → light).
        // Black bishop on c4 (sq=26, light square: file=2, rank=3, sum=5 odd → light).
        // Both light. Positional imbalance in PST means the eval is non-zero.
        let pos =
            Position::from_fen("4k3/8/8/8/2b5/8/4B3/4K3 w - - 0 1").expect("KBvKB FEN must parse");
        // The test pins that eval is non-zero (not zero). The exact value
        // depends on PST data filled in by the implementation.
        assert_ne!(
            evaluate(&pos),
            0,
            "KBvKB (same-color bishops) must not return 0 at M3.A — insufficient-material \
            detection for this case is deferred to M6 (ADR-0014)"
        );
    }

    // -----------------------------------------------------------------------
    // A8–A10 — PSQT anchor values.
    // -----------------------------------------------------------------------

    /// A2 is sq=8 (LERF). White lookup: +(MATERIAL[Pawn] + MG_PAWN_PST[8^56])
    /// = +(82 + MG_PAWN_PST[48]). PeSTO row 6 (rank-2 from White's perspective),
    /// col 0 (a-file) = -35. Total = +(82 + (-35)) = +47.
    #[test]
    fn psqt_white_pawn_a2_anchor() {
        assert_eq!(
            PSQT[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            47,
            "PSQT[White][Pawn][A2] must be 47 (82 material + (-35) PST)"
        );
    }

    /// A7 is sq=48. Black lookup: -(MATERIAL[Pawn] + MG_PAWN_PST[48])
    /// = -(82 + (-35)) = -47. Pins that Black uses `table[sq]` directly (no
    /// XOR), and that the value is the negation of the symmetric white A2 entry.
    #[test]
    fn psqt_black_pawn_a7_anchor() {
        assert_eq!(
            PSQT[Color::Black as usize][PieceKind::Pawn as usize][Square::A7.index() as usize],
            -47,
            "PSQT[Black][Pawn][A7] must be -47 (negation of White A2 entry)"
        );
    }

    /// E1 is sq=4. White lookup: +(MATERIAL[King] + MG_KING_PST[4^56])
    /// = +(0 + MG_KING_PST[60]). PeSTO row 7 (rank-1), col 4 (e-file) = 8.
    /// Pins (a) king material is 0, (b) PeSTO MG king PST is included.
    #[test]
    fn psqt_white_king_e1_anchor() {
        assert_eq!(
            PSQT[Color::White as usize][PieceKind::King as usize][Square::E1.index() as usize],
            8,
            "PSQT[White][King][E1] must be 8 (0 material + 8 PST)"
        );
    }

    // -----------------------------------------------------------------------
    // B1 — PSQT symmetry: PSQT[White][k][sq] == -PSQT[Black][k][sq ^ 56].
    // -----------------------------------------------------------------------

    /// For all (kind, sq), `PSQT[White][kind][sq] == -PSQT[Black][kind][sq ^ 56]`.
    /// Pure constant invariant, exhaustive over all 384 (kind, sq) combinations.
    /// A plain `#[test]` rather than a proptest because the domain is small
    /// and full enumeration gives precise failure diagnostics per (kind, sq).
    #[test]
    fn psqt_symmetry_at_indices() {
        // Guard against vacuous pass: PSQT must contain real data, not all zeros.
        // Without this, the symmetry loop trivially holds (`0 == -0`).
        assert_ne!(
            PSQT[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            0,
            "PSQT must contain non-zero data — symmetry test would pass vacuously otherwise \
            (standard rule: (file + rank) even ⇒ dark square, odd ⇒ light square)"
        );

        for kind in PieceKind::ALL {
            for sq in Square::all() {
                let sq_idx = sq.index() as usize;
                let sq_flip = (sq.index() ^ 56) as usize;
                let white_val = PSQT[Color::White as usize][kind.index()][sq_idx];
                let black_val = PSQT[Color::Black as usize][kind.index()][sq_flip];
                assert_eq!(
                    white_val, -black_val,
                    "PSQT symmetry violated for {:?} at sq={sq}: \
                    PSQT[White][{:?}][{sq_idx}]={white_val} != -PSQT[Black][{:?}][{sq_flip}]={black_val}",
                    kind, kind, kind,
                );
            }
        }
    }
}
