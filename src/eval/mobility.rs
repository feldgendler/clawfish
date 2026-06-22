//! Piece-mobility eval term (M6.D). White-perspective (mg, eg); black
//! pieces subtract symmetrically. Computed live in `evaluate_core` (not
//! pawn-only ⇒ never cached; ADR-0032 §3 class). Pseudolegal (pins ignored),
//! no x-ray — deliberate deferrals (plan §1; research §4/§5).

use crate::eval::data::{
    BISHOP_MOBILITY_EG, BISHOP_MOBILITY_MG, KNIGHT_MOBILITY_EG, KNIGHT_MOBILITY_MG,
    QUEEN_MOBILITY_EG, QUEEN_MOBILITY_MG, ROOK_MOBILITY_EG, ROOK_MOBILITY_MG,
};
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::{magic, movegen};

/// Sparse `(core_index, raw_count)` feature accessor for mobility — the
/// Phase-3 Texel seam (ADR-0037 §2). Shares the mobility-area + per-kind
/// attack-bitboard detection with [`mobility_term_white`]; emits a white-POV
/// signed histogram (white +1, black −1 per detected piece) with BOTH the MG
/// and EG core index of each occupied bucket, following the layout in
/// [`crate::texel::layout::Group::MobN`] / `MobB` / `MobR` / `MobQ`.
/// `dot(mobility_features, shipped core weights)` reproduces
/// `mobility_term_white` (pinned by `accessor_dot_weights_equals_term_fn`).
///
/// `#[allow(dead_code)]`: the seam is consumed by the test cross-check and by
/// the `texel::features` cache builder (a later slice), not by `evaluate`.
pub(crate) fn mobility_features(pos: &Position) -> Vec<(u16, i32)> {
    use crate::texel::layout::{Group, group_range};

    let occ_all = pos.occupied_all();
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let white_pawn_atk = wp.shift_north_east() | wp.shift_north_west();
    let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();

    // Per-kind histograms of signed bucket counts (white +1, black −1).
    let mut knight = [0i32; KNIGHT_MOBILITY_MG.len()];
    let mut bishop = [0i32; BISHOP_MOBILITY_MG.len()];
    let mut rook = [0i32; ROOK_MOBILITY_MG.len()];
    let mut queen = [0i32; QUEEN_MOBILITY_MG.len()];

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let enemy_pawn_atk = match side {
            Color::White => black_pawn_atk,
            Color::Black => white_pawn_atk,
        };
        let area = !(pos.occupied(side) | enemy_pawn_atk);

        for sq in pos.pieces_colored(side, PieceKind::Knight).iter() {
            let c = (movegen::masks::knight_attacks(sq) & area).count() as usize;
            debug_assert!(c < knight.len(), "knight mob idx {c}");
            knight[c] += sign;
        }
        for sq in pos.pieces_colored(side, PieceKind::Bishop).iter() {
            let c = (magic::bishop_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < bishop.len(), "bishop mob idx {c}");
            bishop[c] += sign;
        }
        for sq in pos.pieces_colored(side, PieceKind::Rook).iter() {
            let c = (magic::rook_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < rook.len(), "rook mob idx {c}");
            rook[c] += sign;
        }
        for sq in pos.pieces_colored(side, PieceKind::Queen).iter() {
            let c = (magic::queen_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < queen.len(), "queen mob idx {c}");
            queen[c] += sign;
        }
    }

    // Each group lays MG[0..N] then EG[0..N]; a bucket emits both indices with
    // the same signed count.
    let mut out = Vec::new();
    for (group, hist) in [
        (Group::MobN, &knight[..]),
        (Group::MobB, &bishop[..]),
        (Group::MobR, &rook[..]),
        (Group::MobQ, &queen[..]),
    ] {
        let start = group_range(group).start;
        let n = hist.len();
        for (bucket, &count) in hist.iter().enumerate() {
            if count != 0 {
                out.push(((start + bucket) as u16, count));
                out.push(((start + n + bucket) as u16, count));
            }
        }
    }
    out
}

/// Returns the white-perspective `(mg, eg)` mobility contribution for all
/// N/B/R/Q pieces. Black pieces subtract symmetrically.
pub(crate) fn mobility_term_white(pos: &Position) -> (i32, i32) {
    let occ_all = pos.occupied_all();
    let (mut mg, mut eg) = (0i32, 0i32);

    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let white_pawn_atk = wp.shift_north_east() | wp.shift_north_west();
    let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let enemy_pawn_atk = match side {
            Color::White => black_pawn_atk,
            Color::Black => white_pawn_atk,
        };
        // mobility area = all − (own-occupied ∪ enemy-pawn-attacked)
        let area = !(pos.occupied(side) | enemy_pawn_atk);

        for sq in pos.pieces_colored(side, PieceKind::Knight).iter() {
            let c = (movegen::masks::knight_attacks(sq) & area).count() as usize;
            debug_assert!(c < KNIGHT_MOBILITY_MG.len(), "knight mob idx {c}");
            mg += sign * KNIGHT_MOBILITY_MG[c];
            eg += sign * KNIGHT_MOBILITY_EG[c];
        }
        for sq in pos.pieces_colored(side, PieceKind::Bishop).iter() {
            let c = (magic::bishop_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < BISHOP_MOBILITY_MG.len(), "bishop mob idx {c}");
            mg += sign * BISHOP_MOBILITY_MG[c];
            eg += sign * BISHOP_MOBILITY_EG[c];
        }
        for sq in pos.pieces_colored(side, PieceKind::Rook).iter() {
            let c = (magic::rook_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < ROOK_MOBILITY_MG.len(), "rook mob idx {c}");
            mg += sign * ROOK_MOBILITY_MG[c];
            eg += sign * ROOK_MOBILITY_EG[c];
        }
        for sq in pos.pieces_colored(side, PieceKind::Queen).iter() {
            let c = (magic::queen_attacks(sq, occ_all) & area).count() as usize;
            debug_assert!(c < QUEEN_MOBILITY_MG.len(), "queen mob idx {c}");
            mg += sign * QUEEN_MOBILITY_MG[c];
            eg += sign * QUEEN_MOBILITY_EG[c];
        }
    }
    (mg, eg)
}

#[cfg(test)]
mod tests {
    use super::mobility_term_white;
    use crate::eval::data::{
        BISHOP_MOBILITY_EG, BISHOP_MOBILITY_MG, KNIGHT_MOBILITY_EG, KNIGHT_MOBILITY_MG,
        QUEEN_MOBILITY_EG, QUEEN_MOBILITY_MG, ROOK_MOBILITY_EG, ROOK_MOBILITY_MG,
    };
    use crate::piece::{Color, PieceKind};
    use crate::position::Position;
    use crate::square::Square;

    fn pos_of(fen: &str) -> Position {
        Position::from_fen(fen).expect("fixture FEN must parse")
    }

    // -----------------------------------------------------------------------
    // Structural/table-integrity tests (green against stub).
    // -----------------------------------------------------------------------

    /// Start position is perfectly symmetric: every piece kind has an
    /// equal-but-opposite Black counterpart. Both sides have identical piece
    /// layouts so the white and black contributions cancel: `(0, 0)`.
    ///
    /// TDD state: GREEN against stub (stub returns (0,0); real impl also
    /// returns (0,0) by symmetry). No weights involved — purely structural.
    #[test]
    fn mobility_startpos_is_zero() {
        let pos = Position::starting_position();
        assert_eq!(
            mobility_term_white(&pos),
            (0, 0),
            "start position is symmetric → white and black cancel → (0,0)"
        );
    }

    /// Table lengths guard (vendoring-typo guard — ensures the arrays were
    /// transcribed with the correct element count).
    ///
    /// TDD state: GREEN against stub (pure data assertion, does not call the
    /// function).
    #[test]
    fn mobility_tables_have_expected_lengths() {
        assert_eq!(KNIGHT_MOBILITY_MG.len(), 9);
        assert_eq!(KNIGHT_MOBILITY_EG.len(), 9);
        assert_eq!(BISHOP_MOBILITY_MG.len(), 14);
        assert_eq!(BISHOP_MOBILITY_EG.len(), 14);
        assert_eq!(ROOK_MOBILITY_MG.len(), 15);
        assert_eq!(ROOK_MOBILITY_EG.len(), 15);
        assert_eq!(QUEEN_MOBILITY_MG.len(), 28);
        assert_eq!(QUEEN_MOBILITY_EG.len(), 28);
    }

    /// A position and its exact rank-flipped color-swap must yield negated
    /// `(mg, eg)`. The mirror: swap White↔Black and flip all ranks (rank r
    /// becomes rank 7-r).
    ///
    /// pos_w: White Ka1 + Ne4, Black Ka8. (FEN rank 4 = e4 for white knight.)
    /// pos_b: exact swap — White Ka1, Black Ka8 + Ne5. (Ne4 at rank 3 → Ne5
    ///   at rank 4 after flip; kings exchange colors.)
    ///
    /// Hand-derivation for pos_w (white side only; black has no N/B/R/Q):
    ///   White Ne4 area = !(white_occ | black_pawn_atk) = !{e4,a1} (no black pawns).
    ///   Ne4 attacks: {d2,f2,c3,g3,c5,g5,d6,f6} = 8 squares; none in {e4,a1}.
    ///   White contribution: KNIGHT_MG[8]=36, KNIGHT_EG[8]=29. Black = 0.
    ///   mobility_term_white(pos_w) = (36, 29).
    ///
    /// Hand-derivation for pos_b (black side only; white has no N/B/R/Q):
    ///   Black Ne5 area_black = !(black_occ | white_pawn_atk) = !{e5,a8}.
    ///   Ne5 attacks: {d3,f3,c4,g4,c6,g6,d7,f7} = 8 squares; none in {e5,a8}.
    ///   Black contribution: -KNIGHT_MG[8]=-36, -KNIGHT_EG[8]=-29. White = 0.
    ///   mobility_term_white(pos_b) = (-36, -29) = -(36, 29). ✓
    ///
    /// TDD state: GREEN against stub — vacuous: pos_w and pos_b both return
    /// (0,0) and (0,0) == -(0,0). Genuinely non-vacuous post-impl since both
    /// positions have non-zero (and mutually negated) knight mobility.
    #[test]
    fn mobility_color_mirror_antisymmetric() {
        // pos_w: White Ka1 + Ne4, Black Ka8.
        let pos_w = pos_of("k7/8/8/8/4N3/8/8/K7 w - - 0 1");
        // pos_b: exact rank-flip color-swap — White Ka1, Black Ka8 + Ne5.
        let pos_b = pos_of("k7/8/8/4n3/8/8/8/K7 w - - 0 1");

        let (mg_w, eg_w) = mobility_term_white(&pos_w);
        let (mg_b, eg_b) = mobility_term_white(&pos_b);
        assert_eq!(
            (mg_w, eg_w),
            (-mg_b, -eg_b),
            "pos_w and its color-mirror pos_b must yield negated (mg,eg); \
             pos_w=(36,29) vs pos_b=(-36,-29) once impl lands"
        );
    }

    // -----------------------------------------------------------------------
    // Definitional/structural tests (TDD-red against stub).
    // -----------------------------------------------------------------------

    /// A knight on c3 with a friendly pawn on d5 (one of the attack squares)
    /// must NOT count d5 in its mobility (friendly-occupied exclusion).
    ///
    /// Fixture: White Ke1, Nc3, Pd5. Black Ke8 (lone king → black contributes 0).
    /// Knight c3 attacks: {b1,d1,a2,e2,a4,e4,b5,d5} = 8 squares.
    /// own_occ = {c3,d5,e1}; enemy_pawn_atk = ∅ (no black pawns).
    /// area = !(own_occ | ∅) = all except {c3,d5,e1}.
    /// attacks ∩ area = {b1,d1,a2,e2,a4,e4,b5} = 7.
    /// Expected (full position) = (KNIGHT_MG[7], KNIGHT_EG[7]).
    ///
    /// TDD state: RED against stub (KNIGHT_MOBILITY[7] ≠ (0,0)).
    #[test]
    fn mobility_area_excludes_friendly_occupied() {
        // White: Ke1, Nc3, Pd5. Black: Ke8 (lone king; contributes zero mobility).
        let pos = pos_of("4k3/8/8/3P4/8/2N5/8/4K3 w - - 0 1");
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();
        let area = !(pos.occupied(Color::White) | black_pawn_atk);
        let nc3_attacks = crate::movegen::masks::knight_attacks(Square::C3);
        let count = (nc3_attacks & area).count();
        assert_eq!(
            count, 7,
            "d5 (friendly pawn) must be excluded; expected 7 squares, got {count}"
        );
        assert!(
            !area.contains(Square::D5),
            "d5 (friendly-occupied) must not be in the mobility area"
        );
        assert_eq!(
            mobility_term_white(&pos),
            (KNIGHT_MOBILITY_MG[7], KNIGHT_MOBILITY_EG[7]),
            "mobility must reflect the 7-square area for the knight on c3"
        );
    }

    /// An empty square attacked by an enemy pawn must NOT be counted in the
    /// mobility area.
    ///
    /// Fixture: White Ke1, Nc4. Black Ke8, pd6 (lone king + one pawn; pawn
    /// contributes no N/B/R/Q mobility, so black piece contribution = 0).
    /// Black pawn on d6 attacks c5 and e5.
    /// Knight c4 attacks: {b2,d2,a3,e3,a5,e5,b6,d6} = 8 squares.
    /// area_white = !({c4,e1} | {c5,e5}).
    /// attacks ∩ area_white = {b2,d2,a3,e3,a5,b6,d6} = 7 (e5 excluded).
    /// Expected (full position) = (KNIGHT_MG[7], KNIGHT_EG[7]).
    ///
    /// TDD state: RED against stub.
    #[test]
    fn mobility_area_excludes_enemy_pawn_attacked_empty_sq() {
        // White: Ke1, Nc4. Black: Ke8, pd6 (pawn contributes no mobility).
        let pos = pos_of("4k3/8/3p4/8/2N5/8/8/4K3 w - - 0 1");
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();
        let area = !(pos.occupied(Color::White) | black_pawn_atk);
        let nc4_attacks = crate::movegen::masks::knight_attacks(Square::C4);
        // e5 is empty and attacked by the black pawn on d6 → must be excluded.
        assert!(
            !area.contains(Square::E5),
            "e5 is attacked by the enemy pawn on d6 → must not be in the area"
        );
        let count = (nc4_attacks & area).count();
        assert_eq!(
            count, 7,
            "e5 (enemy-pawn-attacked empty sq) excluded; expected 7, got {count}"
        );
        assert_eq!(
            mobility_term_white(&pos),
            (KNIGHT_MOBILITY_MG[7], KNIGHT_MOBILITY_EG[7]),
            "mobility must be KNIGHT_MOBILITY[7] with e5 excluded"
        );
    }

    /// An enemy non-pawn on a slider's first-blocker square (not pawn-attacked)
    /// IS counted — magic attacks include the blocker square, and a non-pawn
    /// enemy piece is in the mobility area (not own-occupied, not pawn-attacked).
    /// This specifically tests the magic-attack-includes-blocker semantic; a
    /// pawn blocker would not exercise it (no occupancy dependence for knights).
    ///
    /// Fixture: White Ke1, Ra1. Black Ka8, Na5.
    /// occ_all = {a1,e1,a5,a8}.
    ///
    /// White Ra1 attacks (occ_all):
    ///   North: a2,a3,a4,a5 (stops at a5, black knight — blocker included).
    ///   East:  b1,c1,d1,e1 (stops at e1, own king).
    ///   Total = 8 squares.
    /// area_white = !(white_occ | black_pawn_atk) = !{a1,e1} (no black pawns).
    /// White Ra1 attacks ∩ area_white = {a2,a3,a4,a5,b1,c1,d1} = 7.
    /// White contribution: ROOK_MG[7]=14, ROOK_EG[7]=120.
    ///
    /// Black Na5 attacks (file=0, rank=4):
    ///   (+1,+2)=b7, (+1,-2)=b3, (+2,+1)=c6, (+2,-1)=c4 = 4 squares.
    ///   (all other knight deltas go off-board from a5)
    /// area_black = !(black_occ | white_pawn_atk) = !{a5,a8} (no white pawns).
    /// None of {b7,b3,c6,c4} are in {a5,a8} → count = 4.
    /// Black contribution: -KNIGHT_MG[4]=-6, -KNIGHT_EG[4]=-5.
    ///
    /// Expected total = (ROOK_MG[7] - KNIGHT_MG[4], ROOK_EG[7] - KNIGHT_EG[4])
    ///                = (14-6, 120-5) = (8, 115).
    ///
    /// TDD state: RED against stub.
    #[test]
    fn mobility_slider_blocker_enemy_nonpawn_counts() {
        // White: Ke1, Ra1. Black: Ka8, Na5.
        let pos = pos_of("k7/8/8/n7/8/8/8/R3K3 w - - 0 1");
        let occ_all = pos.occupied_all();
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();
        let white_area = !(pos.occupied(Color::White) | black_pawn_atk);
        let rook_attacks = crate::magic::rook_attacks(Square::A1, occ_all);
        // a5 (enemy knight) must be in the area and in the rook's attack set.
        assert!(
            white_area.contains(Square::A5),
            "a5 (enemy knight, non-pawn) must be in the white mobility area"
        );
        assert!(
            rook_attacks.contains(Square::A5),
            "rook attacks must include the first-blocker square a5 (magic includes blocker)"
        );
        let white_rook_count = (rook_attacks & white_area).count() as usize;
        assert_eq!(
            white_rook_count, 7,
            "white rook on a1 with enemy knight on a5: expected 7, got {white_rook_count}"
        );
        // Black Na5 mobility (hand-derived above): count = 4.
        let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
        let white_pawn_atk = wp.shift_north_east() | wp.shift_north_west();
        let black_area = !(pos.occupied(Color::Black) | white_pawn_atk);
        let na5_attacks = crate::movegen::masks::knight_attacks(Square::A5);
        let black_knight_count = (na5_attacks & black_area).count() as usize;
        assert_eq!(
            black_knight_count, 4,
            "black knight on a5: expected 4 mobility squares, got {black_knight_count}"
        );
        // Full two-sided expected: white rook term + black knight term (−sign).
        let expected = (
            ROOK_MOBILITY_MG[white_rook_count] - KNIGHT_MOBILITY_MG[black_knight_count],
            ROOK_MOBILITY_EG[white_rook_count] - KNIGHT_MOBILITY_EG[black_knight_count],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "two-sided: white ROOK[7] minus black KNIGHT[4] = ({}, {})",
            ROOK_MOBILITY_MG[7] - KNIGHT_MOBILITY_MG[4],
            ROOK_MOBILITY_EG[7] - KNIGHT_MOBILITY_EG[4]
        );
    }

    /// A friendly-pawn-attacked empty square stays in the area (own-pawn
    /// attacks are NOT subtracted; only enemy-pawn attacks are excluded).
    ///
    /// Fixture: White Ke1, Ra1, Pb2. Black Ke8 (lone king → black contrib = 0).
    /// White pawn b2 attacks a3 and c3; a3 is empty and on rook's north ray.
    /// Rook a1 attacks (occ_all = {a1,b2,e1,e8}):
    ///   North: a2,a3,a4,a5,a6,a7,a8 (no blockers on a-file).
    ///   East:  b1,c1,d1,e1 (stops at e1, own king).
    ///   Total = 11 squares.
    /// area_white = !(white_occ | black_pawn_atk) = !{a1,b2,e1} (no black pawns).
    /// attacks ∩ area_white = {a2,a3,a4,a5,a6,a7,a8,b1,c1,d1} = 10.
    /// a3 (empty, friendly-pawn-attacked) IS counted — not excluded.
    /// Expected (full position) = (ROOK_MG[10], ROOK_EG[10]).
    ///
    /// TDD state: RED against stub (ROOK_MOBILITY[10] ≠ (0,0)).
    #[test]
    fn mobility_friendly_pawn_attacked_empty_sq_counts() {
        // White: Ke1, Ra1, Pb2. Black: Ke8 (lone king; contributes zero mobility).
        let pos = pos_of("4k3/8/8/8/8/8/1P6/R3K3 w - - 0 1");
        let occ_all = pos.occupied_all();
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();
        let area = !(pos.occupied(Color::White) | black_pawn_atk);
        let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
        let white_pawn_atk = wp.shift_north_east() | wp.shift_north_west();
        assert!(
            white_pawn_atk.contains(Square::A3),
            "fixture invariant: b2 pawn must attack a3"
        );
        assert!(
            area.contains(Square::A3),
            "a3 (friendly-pawn-attacked, empty) must remain in the area"
        );
        let rook_attacks = crate::magic::rook_attacks(Square::A1, occ_all);
        let count = (rook_attacks & area).count();
        assert_eq!(
            count, 10,
            "rook on a1 with friendly pawn on b2: expected 10 (a3 stays), got {count}"
        );
        assert_eq!(
            mobility_term_white(&pos),
            (ROOK_MOBILITY_MG[10], ROOK_MOBILITY_EG[10]),
            "mobility must be ROOK_MOBILITY[10] (friendly-pawn-atk sq not excluded)"
        );
    }

    // -----------------------------------------------------------------------
    // Symbolic value tests (TDD-red against stub).
    // -----------------------------------------------------------------------

    /// Hand-counted knight mobility → `(KNIGHT_MOBILITY_MG[c], …EG[c])`.
    ///
    /// Fixture: White Kh1, Nd4. Black Ke8 (lone king → black contributes 0).
    /// occ_all = {d4,h1,e8}.
    /// Knight d4 attacks: {b3,b5,c2,c6,e2,e6,f3,f5} = 8 squares.
    /// area_white = !(white_occ | ∅) = all except {d4,h1}.
    /// None of the 8 attack squares are d4 or h1 → count = 8.
    /// Expected = (KNIGHT_MOBILITY_MG[8], KNIGHT_MOBILITY_EG[8]).
    ///
    /// TDD state: RED against stub (stub returns (0,0)).
    /// No M6.F-dormant discriminator.
    #[test]
    fn mobility_knight_symbolic_value_from_data() {
        // White Kh1, Nd4. Black Ke8 (lone king; black contributes 0).
        let pos = pos_of("4k3/8/8/8/3N4/8/8/7K w - - 0 1");
        // Independent derivation: knight on d4, no pawns either side.
        // attack set: b3,b5,c2,c6,e2,e6,f3,f5 — none land on h1 or d4.
        let expected_count: usize = 8;
        let expected = (
            KNIGHT_MOBILITY_MG[expected_count],
            KNIGHT_MOBILITY_EG[expected_count],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "knight on d4 (no area exclusions): expected KNIGHT_MOBILITY[8]=({},{})",
            KNIGHT_MOBILITY_MG[8],
            KNIGHT_MOBILITY_EG[8]
        );
    }

    /// Hand-counted bishop mobility → `(BISHOP_MOBILITY_MG[c], …EG[c])`.
    ///
    /// Fixture: White Ke1, Bc1. Black Ke8 (lone king → black contributes 0).
    /// occ_all = {c1,e1,e8}.
    /// Bishop c1 attacks (magic):
    ///   NE diagonal: d2,e3,f4,g5,h6 (5; no blocker on this diagonal in occ_all).
    ///   NW diagonal: b2,a3 (2; reaches board edge at a3).
    ///   SE: nothing (rank 0).  SW: nothing (rank 0).
    ///   Total = 7 squares.
    /// area_white = all except {c1,e1}.
    /// None of the 7 attacks are c1 or e1 → count = 7.
    /// Expected = (BISHOP_MOBILITY_MG[7], BISHOP_MOBILITY_EG[7]).
    ///
    /// TDD state: RED against stub.
    /// No M6.F-dormant discriminator.
    #[test]
    fn mobility_bishop_symbolic_value_from_data() {
        // White Ke1, Bc1. Black Ke8 (lone king; black contributes 0).
        let pos = pos_of("4k3/8/8/8/8/8/8/2B1K3 w - - 0 1");
        let expected_count: usize = 7;
        let expected = (
            BISHOP_MOBILITY_MG[expected_count],
            BISHOP_MOBILITY_EG[expected_count],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "bishop on c1: expected BISHOP_MOBILITY[7]=({},{})",
            BISHOP_MOBILITY_MG[7],
            BISHOP_MOBILITY_EG[7]
        );
    }

    /// Hand-counted rook mobility → `(ROOK_MOBILITY_MG[c], …EG[c])`.
    ///
    /// Fixture: White Ke1, Ra1. Black Ke8 (lone king → black contributes 0).
    /// occ_all = {a1,e1,e8}.
    /// Rook a1 attacks (magic):
    ///   North: a2,a3,a4,a5,a6,a7,a8 (7; no blockers on a-file).
    ///   East:  b1,c1,d1,e1 (stops at e1, own king; e1 included in attacks).
    ///   Total = 11 squares.
    /// area_white = all except {a1,e1}.
    /// attacks ∩ area_white = 11 − {e1} = 10.
    /// Expected = (ROOK_MOBILITY_MG[10], ROOK_MOBILITY_EG[10]).
    ///
    /// TDD state: RED against stub.
    /// No M6.F-dormant discriminator.
    #[test]
    fn mobility_rook_symbolic_value_from_data() {
        // White Ke1, Ra1. Black Ke8 (lone king; black contributes 0).
        let pos = pos_of("4k3/8/8/8/8/8/8/R3K3 w - - 0 1");
        let expected_count: usize = 10;
        let expected = (
            ROOK_MOBILITY_MG[expected_count],
            ROOK_MOBILITY_EG[expected_count],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "rook on a1 (king on e1): expected ROOK_MOBILITY[10]=({},{})",
            ROOK_MOBILITY_MG[10],
            ROOK_MOBILITY_EG[10]
        );
    }

    /// Hand-counted queen mobility → `(QUEEN_MOBILITY_MG[c], …EG[c])`.
    ///
    /// Fixture: White Ke1, Qd1. Black Ke8 (lone king → black contributes 0).
    /// occ_all = {d1,e1,e8}.
    /// Queen d1 attacks (magic = rook ∪ bishop):
    ///   Rook north:  d2,d3,d4,d5,d6,d7,d8 (7).
    ///   Rook west:   c1,b1,a1 (3).
    ///   Rook east:   e1 (1; own king, stops here).
    ///   Rook south:  nothing (rank 0).
    ///   Bishop NE:   e2,f3,g4,h5 (4).
    ///   Bishop NW:   c2,b3,a4 (3).
    ///   Bishop SE:   nothing (rank 0).
    ///   Bishop SW:   nothing (rank 0).
    ///   Total = 7+3+1+4+3 = 18 squares.
    /// area_white = all except {d1,e1}.
    /// attacks ∩ area_white = 18 − {e1} = 17.
    /// Expected = (QUEEN_MOBILITY_MG[17], QUEEN_MOBILITY_EG[17]).
    ///
    /// TDD state: RED against stub.
    /// No M6.F-dormant discriminator.
    #[test]
    fn mobility_queen_symbolic_value_from_data() {
        // White Ke1, Qd1. Black Ke8 (lone king; black contributes 0).
        let pos = pos_of("4k3/8/8/8/8/8/8/3QK3 w - - 0 1");
        let expected_count: usize = 17;
        let expected = (
            QUEEN_MOBILITY_MG[expected_count],
            QUEEN_MOBILITY_EG[expected_count],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "queen on d1 (king on e1): expected QUEEN_MOBILITY[17]=({},{})",
            QUEEN_MOBILITY_MG[17],
            QUEEN_MOBILITY_EG[17]
        );
    }

    /// An absolutely-pinned knight is credited its full pseudolegal attack
    /// count — pin detection is deferred (plan §1 / research §4).
    ///
    /// Fixture: White Ke1, Ne4. Black Ka8, Re8.
    /// The knight is absolutely pinned: Re8–Ne4–Ke1 are collinear on the e-file,
    /// so moving the knight would expose the white king. Pseudolegal mobility
    /// credits the knight's full attack set regardless.
    ///
    /// White Ne4 attacks: {d2,f2,c3,g3,c5,g5,d6,f6} = 8 squares.
    /// area_white = !{e4,e1} (no black pawns). None of 8 attacks in {e4,e1}.
    /// White knight count = 8. White contribution: KNIGHT_MG[8]=36, KNIGHT_EG[8]=29.
    ///
    /// Black Re8 attacks (occ_all = {e1,e4,e8,a8}):
    ///   North: nothing (rank 8).
    ///   South: e7,e6,e5,e4 (stops at e4, white knight — included).
    ///   West: d8,c8,b8,a8 (stops at a8, black king — included in attacks).
    ///   East: f8,g8,h8 (3).
    ///   Total = 11 squares.
    /// area_black = !{e8,a8} (black_occ; no white pawns).
    /// attacks ∩ area_black = {e7,e6,e5,e4,d8,c8,b8,f8,g8,h8} = 10.
    ///   (a8 excluded as black-occupied; e4 is white-occupied but not
    ///   black-occupied so it IS in area_black → counts).
    /// Black contribution: -ROOK_MG[10]=-31, -ROOK_EG[10]=-154.
    ///
    /// Expected total = (36-31, 29-154) = (5, -125).
    ///
    /// TDD state: RED against stub.
    #[test]
    fn mobility_pinned_piece_scored_pseudolegally() {
        // White Ke1, Ne4. Black Ka8, Re8.
        let pos = pos_of("k3r3/8/8/8/4N3/8/8/4K3 w - - 0 1");
        // Verify pin geometry: white king, white knight, black rook on e-file.
        assert_eq!(
            pos.king_square(Color::White).file(),
            Square::E1.file(),
            "fixture invariant: white king must be on e-file"
        );
        assert_eq!(
            pos.pieces_colored(Color::White, PieceKind::Knight)
                .iter()
                .next()
                .expect("must have white knight")
                .file(),
            Square::E1.file(),
            "fixture invariant: white knight must be on e-file (pinned)"
        );
        // White knight count = 8 (full pseudolegal set, pin ignored).
        // Black rook count = 10 (hand-derived above).
        // Expected total = (KNIGHT_MG[8] - ROOK_MG[10], KNIGHT_EG[8] - ROOK_EG[10]).
        let expected = (
            KNIGHT_MOBILITY_MG[8] - ROOK_MOBILITY_MG[10],
            KNIGHT_MOBILITY_EG[8] - ROOK_MOBILITY_EG[10],
        );
        assert_eq!(
            mobility_term_white(&pos),
            expected,
            "pinned knight: full pseudolegal count (8); black rook count (10); \
             expected ({}, {})",
            KNIGHT_MOBILITY_MG[8] - ROOK_MOBILITY_MG[10],
            KNIGHT_MOBILITY_EG[8] - ROOK_MOBILITY_EG[10]
        );
    }

    /// A rook behind a friendly rook (same file) is limited by the blocker
    /// — no x-ray through the friendly slider.
    ///
    /// Fixture: White Ke1, Ra1, Ra4. Black Ke8 (lone king → black contrib = 0).
    /// occ_all = {a1,a4,e1,e8}; white_occ = {a1,a4,e1}.
    ///
    /// Ra1 attacks (magic):
    ///   North: a2,a3,a4 (stops at a4; own → excluded by area).
    ///   East:  b1,c1,d1,e1 (stops at e1; own → excluded).
    ///   attacks ∩ area_white = {a2,a3,b1,c1,d1} = 5.
    ///   (a4 excluded as own-occupied; no a5 or beyond — no x-ray.)
    ///
    /// Ra4 attacks (magic):
    ///   North: a5,a6,a7,a8 (4).
    ///   South: a3,a2,a1 (stops at a1; a1 own → excluded; a3,a2 in area).
    ///   East:  b4,c4,d4,e4,f4,g4,h4 (7).
    ///   attacks ∩ area_white = {a5,a6,a7,a8,a3,a2,b4,c4,d4,e4,f4,g4,h4} = 13.
    ///
    /// Expected = (ROOK_MG[5]+ROOK_MG[13], ROOK_EG[5]+ROOK_EG[13]).
    ///
    /// TDD state: RED against stub.
    #[test]
    fn mobility_no_xray_through_friendly_slider() {
        // White Ke1, Ra1, Ra4. Black Ke8 (lone king; black contributes 0).
        let pos = pos_of("4k3/8/8/8/R7/8/8/R3K3 w - - 0 1");
        let occ_all = pos.occupied_all();
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let black_pawn_atk = bp.shift_south_east() | bp.shift_south_west();
        let area = !(pos.occupied(Color::White) | black_pawn_atk);

        // Ra1 must NOT see a5 or beyond (blocked by Ra4, no x-ray).
        let ra1_attacks = crate::magic::rook_attacks(Square::A1, occ_all);
        assert!(
            !ra1_attacks.contains(Square::A5),
            "Ra1 must not x-ray through Ra4: a5 must NOT be in Ra1's attack set"
        );
        let ra1_count = (ra1_attacks & area).count() as usize;
        assert_eq!(
            ra1_count, 5,
            "Ra1 (blocked by Ra4): expected 5, got {ra1_count}"
        );

        let ra4_attacks = crate::magic::rook_attacks(Square::A4, occ_all);
        let ra4_count = (ra4_attacks & area).count() as usize;
        assert_eq!(ra4_count, 13, "Ra4: expected 13, got {ra4_count}");

        let expected_mg = ROOK_MOBILITY_MG[ra1_count] + ROOK_MOBILITY_MG[ra4_count];
        let expected_eg = ROOK_MOBILITY_EG[ra1_count] + ROOK_MOBILITY_EG[ra4_count];
        assert_eq!(
            mobility_term_white(&pos),
            (expected_mg, expected_eg),
            "two rooks (Ra1 blocked, Ra4 open): expected ({}, {})",
            expected_mg,
            expected_eg
        );
    }

    /// Max-index corner case: lone white piece + white king vs lone black king.
    /// Verifies indices stay in bounds (no panic from debug_assert!) and pins
    /// the expected value from `eval::data` at the max-geometric count for
    /// each piece kind.
    ///
    /// All fixtures: Black = lone Kh8 (contributes zero mobility).
    ///
    /// Queen on d4, White Ka1 (occ_all = {d4,a1,h8}):
    ///   N:d5-d8(4) S:d3-d1(3) E:e4-h4(4) W:c4-a4(3)
    ///   NE:e5,f6,g7,h8(4; h8 black king, in area) NW:c5,b6,a7(3)
    ///   SE:e3,f2,g1(3) SW:c3,b2,a1(3; a1 own king → excluded)
    ///   Total attacks = 27. area = !{d4,a1}. attacks ∩ area = 26.
    ///   Expected = (QUEEN_MG[26], QUEEN_EG[26]).
    ///
    /// Rook on a1, White Kh1 (occ_all = {a1,h1,h8}):
    ///   N:a2-a8(7) E:b1-h1(7; h1 own king → excluded)
    ///   Total attacks = 14. area = !{a1,h1}. attacks ∩ area = 13.
    ///   Expected = (ROOK_MG[13], ROOK_EG[13]).
    ///
    /// Bishop on d4, White Ka1 (occ_all = {d4,a1,h8}):
    ///   NE:e5,f6,g7,h8(4) NW:c5,b6,a7(3) SE:e3,f2,g1(3)
    ///   SW:c3,b2,a1(3; a1 own → excluded)
    ///   Total attacks = 13. area = !{d4,a1}. attacks ∩ area = 12.
    ///   Expected = (BISHOP_MG[12], BISHOP_EG[12]).
    ///
    /// Knight on d4, White Ka1 (occ_all = {d4,a1,h8}):
    ///   attacks: {b3,b5,c2,c6,e2,e6,f3,f5} = 8. area = !{d4,a1}. count = 8.
    ///   Expected = (KNIGHT_MG[8], KNIGHT_EG[8]).
    ///
    /// TDD state: RED against stub — the `assert_eq!`s pin non-zero
    /// `eval::data` entries (QUEEN_MG[26]=123, ROOK_MG[13]=49, …) ≠ the
    /// stub's (0,0). Goes GREEN post-impl, asserting real postconditions at
    /// near-max-geometric indices (the index-bound-invariant exercise).
    #[test]
    fn mobility_indices_within_bounds_max_geometric() {
        // Queen on d4, White Ka1, Black Kh8.
        let pos_q = pos_of("7k/8/8/8/3Q4/8/8/K7 w - - 0 1");
        assert_eq!(
            mobility_term_white(&pos_q),
            (QUEEN_MOBILITY_MG[26], QUEEN_MOBILITY_EG[26]),
            "queen on d4 (count=26): expected QUEEN_MOBILITY[26]=({},{})",
            QUEEN_MOBILITY_MG[26],
            QUEEN_MOBILITY_EG[26]
        );

        // Rook on a1, White Kh1, Black Kh8.
        let pos_r = pos_of("7k/8/8/8/8/8/8/R6K w - - 0 1");
        assert_eq!(
            mobility_term_white(&pos_r),
            (ROOK_MOBILITY_MG[13], ROOK_MOBILITY_EG[13]),
            "rook on a1, king h1 (count=13): expected ROOK_MOBILITY[13]=({},{})",
            ROOK_MOBILITY_MG[13],
            ROOK_MOBILITY_EG[13]
        );

        // Bishop on d4, White Ka1, Black Kh8.
        let pos_b = pos_of("7k/8/8/8/3B4/8/8/K7 w - - 0 1");
        assert_eq!(
            mobility_term_white(&pos_b),
            (BISHOP_MOBILITY_MG[12], BISHOP_MOBILITY_EG[12]),
            "bishop on d4 (count=12): expected BISHOP_MOBILITY[12]=({},{})",
            BISHOP_MOBILITY_MG[12],
            BISHOP_MOBILITY_EG[12]
        );

        // Knight on d4, White Ka1, Black Kh8.
        let pos_n = pos_of("7k/8/8/8/3N4/8/8/K7 w - - 0 1");
        assert_eq!(
            mobility_term_white(&pos_n),
            (KNIGHT_MOBILITY_MG[8], KNIGHT_MOBILITY_EG[8]),
            "knight on d4 (count=8, max): expected KNIGHT_MOBILITY[8]=({},{})",
            KNIGHT_MOBILITY_MG[8],
            KNIGHT_MOBILITY_EG[8]
        );
    }
}
