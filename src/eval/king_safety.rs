//! King-safety eval term (M6.E). White-perspective (mg, eg); the side under
//! attack contributes ±sign symmetrically. Computed live in `evaluate_core`
//! (not pawn-only ⇒ never cached — ADR-0033 §6, supersedes ADR-0032 §3).
//! Zone = 3×3 ring + forward wedge; per-kind attacker units → SAFETY_TABLE;
//! castled-king pawn-shield; MG-only open/semi-open-file penalty. All weights
//! zeroed at ship (score-neutral, ADR-0033 §8) — term math live, M6.F-ready.

use crate::bitboard::{self, Bitboard};
use crate::eval::data::{
    KING_ATTACK_WEIGHT_B, KING_ATTACK_WEIGHT_N, KING_ATTACK_WEIGHT_Q, KING_ATTACK_WEIGHT_R,
    KING_SAFETY_TABLE, KS_ADJ_SEMI_OPEN_MG, KS_BOTH_ADJ_SEMI_OPEN_MG, KS_KFILE_OPEN_MG,
    KS_KFILE_SEMI_OPEN_MG, SHIELD_1_EG, SHIELD_1_MG, SHIELD_2_EG, SHIELD_2_MG,
};
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;
use crate::{magic, movegen};

/// King-safety zone for `side`: the 3×3 ring around `ksq` plus the three
/// squares one rank toward the enemy beyond the ring's forward edge (ADR-0033
/// §1). The king's own square is excluded (`& !from_square(ksq)` is
/// load-bearing — see plan §3 comment). Single source of truth called by both
/// `king_safety_term_white` and the zone tests.
pub(crate) fn king_zone(side: Color, ksq: Square) -> Bitboard {
    let ring = movegen::masks::king_attacks(ksq);
    let fwd = match side {
        Color::White => ring.shift_north(),
        Color::Black => ring.shift_south(),
    };
    ring | (fwd & !Bitboard::from_square(ksq))
}

/// Signed (white − black) raw counts for the 8 `KsShieldFile` slots, in layout
/// order: SHIELD_1, SHIELD_1, SHIELD_2, SHIELD_2 (MG/EG pairs), then the 4
/// MG-only open/semi-open-file penalties. Shares the shield + open-file
/// detection with [`king_safety_term_white`]; EXCLUDES the frozen attacker
/// S-curve entirely (ADR-0037 §3, deferred).
#[allow(dead_code)] // Texel seam: consumed by tests + `texel::features` (later slice).
fn shield_open_file_signed_counts(pos: &Position) -> [i32; 8] {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let all_pawn_files = bitboard::file_fill(wp | bp);

    // Slots: 0=SHIELD_1, 1=SHIELD_1(EG), 2=SHIELD_2, 3=SHIELD_2(EG),
    // 4=KFILE_SEMI_OPEN, 5=KFILE_OPEN, 6=ADJ_SEMI_OPEN, 7=BOTH_ADJ_SEMI_OPEN.
    let mut counts = [0i32; 8];
    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let (own_pawns, own_pawn_files) = match side {
            Color::White => (wp, bitboard::file_fill(wp)),
            Color::Black => (bp, bitboard::file_fill(bp)),
        };
        let ksq = pos.king_square(side);
        let kf = ksq.file();

        // (b) pawn shield — only when `side`'s king is on a castled-side file.
        if kf >= 5 || kf <= 2 {
            let (r2, r3) = match side {
                Color::White => (1u8, 2u8),
                Color::Black => (6u8, 5u8),
            };
            let lo = kf.saturating_sub(1);
            let hi = (kf + 1).min(7);
            for f in lo..=hi {
                let on2 = Square::from_file_rank(f, r2).is_some_and(|s| own_pawns.contains(s));
                let on3 = Square::from_file_rank(f, r3).is_some_and(|s| own_pawns.contains(s));
                if on2 {
                    counts[0] += sign; // SHIELD_1 MG
                    counts[1] += sign; // SHIELD_1 EG
                } else if on3 {
                    counts[2] += sign; // SHIELD_2 MG
                    counts[3] += sign; // SHIELD_2 EG
                }
            }
        }

        // (c) open / semi-open files toward the king — MG-only.
        let has_own =
            |f: u8| Square::from_file_rank(f, 0).is_some_and(|s| own_pawn_files.contains(s));
        let has_any =
            |f: u8| Square::from_file_rank(f, 0).is_some_and(|s| all_pawn_files.contains(s));
        if !has_any(kf) {
            counts[5] += sign; // KS_KFILE_OPEN_MG
        } else if !has_own(kf) {
            counts[4] += sign; // KS_KFILE_SEMI_OPEN_MG
        }
        let adj: [Option<u8>; 2] = [kf.checked_sub(1), if kf < 7 { Some(kf + 1) } else { None }];
        let mut semi_adj = 0u32;
        for f in adj.into_iter().flatten() {
            if !has_own(f) {
                counts[6] += sign; // KS_ADJ_SEMI_OPEN_MG (per adjacent file)
                semi_adj += 1;
            }
        }
        if semi_adj == 2 {
            counts[7] += sign; // KS_BOTH_ADJ_SEMI_OPEN_MG
        }
    }
    counts
}

/// Sparse `(core_index, raw_count)` feature accessor for the linear
/// shield + open/semi-open-file sub-term of king safety — the Phase-3 Texel
/// seam (ADR-0037 §2). White-POV signed counts (white +, black −) over the 8
/// `KsShieldFile` slots. EXCLUDES the frozen S-curve attacker contribution.
/// `dot(features, shipped core weights)` equals [`shield_open_file_term_white`]
/// (pinned by `accessor_dot_weights_equals_term_fn`).
#[allow(dead_code)] // Texel seam: consumed by tests + `texel::features` (later slice).
pub(crate) fn shield_open_file_features(pos: &Position) -> Vec<(u16, i32)> {
    use crate::texel::layout::{Group, group_range};
    let counts = shield_open_file_signed_counts(pos);
    let start = group_range(Group::KsShieldFile).start;
    let mut out = Vec::new();
    for (local, &count) in counts.iter().enumerate() {
        if count != 0 {
            out.push(((start + local) as u16, count));
        }
    }
    out
}

/// White-perspective `(mg, eg)` of the LINEAR shield + open/semi-open-file
/// sub-term (no S-curve). The shield+open-file split of
/// [`king_safety_term_white`]; used by the Phase-3 accessor cross-check.
#[allow(dead_code)] // Texel seam: consumed by tests + `texel::features` (later slice).
pub(crate) fn shield_open_file_term_white(pos: &Position) -> (i32, i32) {
    let c = shield_open_file_signed_counts(pos);
    // SHIELD_1/2 are tapered (MG+EG); the open-file penalties are MG-only.
    let mg = SHIELD_1_MG * c[0]
        + SHIELD_2_MG * c[2]
        + KS_KFILE_SEMI_OPEN_MG * c[4]
        + KS_KFILE_OPEN_MG * c[5]
        + KS_ADJ_SEMI_OPEN_MG * c[6]
        + KS_BOTH_ADJ_SEMI_OPEN_MG * c[7];
    let eg = SHIELD_1_EG * c[1] + SHIELD_2_EG * c[3];
    (mg, eg)
}

/// Returns the white-perspective `(mg, eg)` king-safety contribution (zone
/// attacker S-curve + castled pawn-shield + open/semi-open-file penalty).
/// Black's king contributes with −sign. All weights zeroed at ship.
pub(crate) fn king_safety_term_white(pos: &Position) -> (i32, i32) {
    let occ_all = pos.occupied_all();
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let all_pawn_files = bitboard::file_fill(wp | bp);
    let (mut mg, mut eg) = (0i32, 0i32);

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let (enemy, own_pawns, own_pawn_files) = match side {
            Color::White => (Color::Black, wp, bitboard::file_fill(wp)),
            Color::Black => (Color::White, bp, bitboard::file_fill(bp)),
        };
        let ksq = pos.king_square(side);
        let zone = king_zone(side, ksq);

        // King-safety from `side`'s OWN perspective (penalty = negative,
        // shield = positive); white-perspective contribution = sign * ks_*.
        let mut ks_mg = 0i32;
        let mut ks_eg = 0i32;

        // (a) attacker S-curve — enemy N/B/R/Q attacking `side`'s zone.
        let mut units = 0i32;
        let mut attackers = 0u32;
        for psq in pos.pieces_colored(enemy, PieceKind::Knight).iter() {
            let h = (movegen::masks::knight_attacks(psq) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += KING_ATTACK_WEIGHT_N * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Bishop).iter() {
            let h = (magic::bishop_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += KING_ATTACK_WEIGHT_B * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Rook).iter() {
            let h = (magic::rook_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += KING_ATTACK_WEIGHT_R * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Queen).iter() {
            let h = (magic::queen_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += KING_ATTACK_WEIGHT_Q * h;
            }
        }
        let enemy_has_queen = pos.pieces_colored(enemy, PieceKind::Queen).any();
        if attackers < 2 || !enemy_has_queen {
            units = 0;
        }
        let idx = units.clamp(0, (KING_SAFETY_TABLE.len() - 1) as i32) as usize;
        ks_mg -= KING_SAFETY_TABLE[idx]; // MG-only (EG king-safety = 0, ADR-0033 §2/§5)

        // (b) pawn shield — only when `side`'s king is on a castled-side file.
        let kf = ksq.file();
        if kf >= 5 /* ≥ f */ || kf <= 2
        /* ≤ c */
        {
            let (r2, r3) = match side {
                Color::White => (1u8, 2u8), // ranks 2,3
                Color::Black => (6u8, 5u8), // ranks 7,6
            };
            let lo = kf.saturating_sub(1);
            let hi = (kf + 1).min(7);
            for f in lo..=hi {
                let on2 = Square::from_file_rank(f, r2).is_some_and(|s| own_pawns.contains(s));
                let on3 = Square::from_file_rank(f, r3).is_some_and(|s| own_pawns.contains(s));
                if on2 {
                    ks_mg += SHIELD_1_MG;
                    ks_eg += SHIELD_1_EG;
                } else if on3 {
                    ks_mg += SHIELD_2_MG;
                    ks_eg += SHIELD_2_EG;
                }
            }
        }

        // (c) open / semi-open files toward the king — MG-only.
        // File f "has own/any pawn" via the file_fill rank-0 projection.
        let has_own =
            |f: u8| Square::from_file_rank(f, 0).is_some_and(|s| own_pawn_files.contains(s));
        let has_any =
            |f: u8| Square::from_file_rank(f, 0).is_some_and(|s| all_pawn_files.contains(s));
        // King file: open (no pawn either color) takes precedence over semi-open.
        if !has_any(kf) {
            ks_mg += KS_KFILE_OPEN_MG;
        } else if !has_own(kf) {
            ks_mg += KS_KFILE_SEMI_OPEN_MG;
        }
        // Adjacent files: count semi-open (no own pawn); both ⇒ amplify.
        let adj: [Option<u8>; 2] = [kf.checked_sub(1), if kf < 7 { Some(kf + 1) } else { None }];
        let mut semi_adj = 0u32;
        for f in adj.into_iter().flatten() {
            if !has_own(f) {
                ks_mg += KS_ADJ_SEMI_OPEN_MG;
                semi_adj += 1;
            }
        }
        if semi_adj == 2 {
            ks_mg += KS_BOTH_ADJ_SEMI_OPEN_MG;
        }

        mg += sign * ks_mg;
        eg += sign * ks_eg;
    }
    (mg, eg)
}

#[cfg(test)]
mod tests {
    use super::{king_safety_term_white, king_zone};
    use crate::bitboard;
    use crate::eval::data::{
        KING_ATTACK_WEIGHT_B, KING_ATTACK_WEIGHT_N, KING_ATTACK_WEIGHT_Q, KING_ATTACK_WEIGHT_R,
        KING_SAFETY_TABLE, KS_ADJ_SEMI_OPEN_MG, KS_BOTH_ADJ_SEMI_OPEN_MG, KS_KFILE_OPEN_MG,
        KS_KFILE_SEMI_OPEN_MG, SHIELD_1_EG, SHIELD_1_MG, SHIELD_2_EG, SHIELD_2_MG,
    };
    use crate::piece::{Color, PieceKind};
    use crate::position::Position;
    use crate::square::Square;
    use crate::{magic, movegen};

    fn pos_of(fen: &str) -> Position {
        Position::from_fen(fen).expect("fixture FEN must parse")
    }

    // -----------------------------------------------------------------------
    // Structural / table-integrity tests (GREEN against stub).
    // -----------------------------------------------------------------------

    /// Start position is perfectly symmetric (White and Black king-safety
    /// contributions cancel). Returns `(0, 0)`.
    ///
    /// TDD state: GREEN against stub (stub returns (0,0); real impl also
    /// returns (0,0) by symmetry at zeroed weights). No weights involved.
    #[test]
    fn king_safety_startpos_is_zero() {
        let pos = Position::starting_position();
        assert_eq!(
            king_safety_term_white(&pos),
            (0, 0),
            "start position is symmetric → white and black cancel → (0,0)"
        );
    }

    /// `KING_SAFETY_TABLE.len() == 100` (vendoring-typo guard).
    ///
    /// TDD state: GREEN against stub (pure data assertion, does not call the
    /// function).
    #[test]
    fn king_safety_tables_have_expected_lengths() {
        assert_eq!(
            KING_SAFETY_TABLE.len(),
            100,
            "KING_SAFETY_TABLE must have exactly 100 entries (CPW S-curve)"
        );
    }

    /// `file_fill` rank-0 projection — the structural foundation for the
    /// open-file `has_own`/`has_any` helpers (ADR-0033 §4). Weight-free: this
    /// tests `bitboard::file_fill` directly, not `king_safety_term_white`.
    ///
    /// - A friendly pawn on file `f` ⇒ `Square::from_file_rank(f, 0)` bit set
    ///   in `file_fill(own_pawns)`.
    /// - An empty file ⇒ bit NOT set.
    /// - Same for `all_pawn_files` (union of both sides).
    ///
    /// TDD state: GREEN against stub (tests `file_fill` bitboard ops, does
    /// not call `king_safety_term_white`; survives the score-neutral period).
    #[test]
    fn king_safety_open_file_detection_structural() {
        // White pawn on g2 → file g (file index 6) is "own pawn present".
        // File d is empty → "no own pawn" for either side.
        // Black pawn on d7 → file d (file index 3) present in all_pawn_files.
        let pos = pos_of("4k3/3p4/8/8/8/8/6P1/4K3 w - - 0 1");

        let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let own_pawn_files = bitboard::file_fill(wp);
        let all_pawn_files = bitboard::file_fill(wp | bp);

        // File g (index 6): white pawn on g2 → own_pawn_files rank-0 bit set.
        let g0 = Square::from_file_rank(6, 0).expect("g1 is valid");
        assert!(
            own_pawn_files.contains(g0),
            "file_fill(white_pawns) must have rank-0 bit set for file g (pawn on g2)"
        );

        // File d (index 3): no white pawn → own_pawn_files rank-0 bit NOT set.
        let d0 = Square::from_file_rank(3, 0).expect("d1 is valid");
        assert!(
            !own_pawn_files.contains(d0),
            "file_fill(white_pawns) must NOT have rank-0 bit set for file d (no white pawn)"
        );

        // File d: black pawn on d7 → present in all_pawn_files.
        assert!(
            all_pawn_files.contains(d0),
            "file_fill(all_pawns) must have rank-0 bit set for file d (black pawn on d7)"
        );

        // File e (index 4): no pawn either color → NOT in all_pawn_files.
        let e0 = Square::from_file_rank(4, 0).expect("e1 is valid");
        assert!(
            !all_pawn_files.contains(e0),
            "file_fill(all_pawns) must NOT have rank-0 bit set for file e (no pawns)"
        );
    }

    /// A position and its exact rank-flipped color-swap must yield negated
    /// `(mg, eg)`. The stub returns `(0,0)` for both, so `(0,0) == -(0,0)` —
    /// trivially passes; becomes load-bearing post-impl when weights are set.
    ///
    /// Fixture: White Kg1 (castled, file g ≥ 5 → open-file terms active),
    /// Black Ka8 (a-file, open-file terms active). The fixture is **pawnless**
    /// ⇒ no shield and no attackers — so this test pins ONLY the open-file
    /// path's white-perspective `sign` convention (the shield rank-mirror and
    /// the attacker S-curve `sign` are pinned by the dedicated symbolic tests,
    /// not here). At zeroed weights both contributions are (0,0). Post-M6.F the
    /// open-file term makes ks(Kg1) ≠ ks(Ka1) and the color-mirror verifies
    /// swapping the sides negates the result (the open-file `sign` pin).
    ///
    /// Mirror: White Ka1 (a-file, same open-file geometry as original Ka8),
    /// Black Kg8 (g-file, same geometry as original Kg1). The two positions are
    /// exact rank-flips + color-swaps: king_safety_term_white(pos_b) = −term(pos_w).
    ///
    /// TDD state: GREEN against stub (0 == −0). Acceptable; load-bearing at M6.F.
    #[test]
    fn king_safety_color_mirror_antisymmetric() {
        // pos_w: White Kg1, Black Ka8. No pawns, no pieces.
        let pos_w = pos_of("k7/8/8/8/8/8/8/6K1 w - - 0 1");
        // pos_b: color+rank mirror — White Ka1, Black Kg8.
        let pos_b = pos_of("6k1/8/8/8/8/8/8/K7 w - - 0 1");

        let (mg_w, eg_w) = king_safety_term_white(&pos_w);
        let (mg_b, eg_b) = king_safety_term_white(&pos_b);
        assert_eq!(
            (mg_w, eg_w),
            (-mg_b, -eg_b),
            "color-mirror positions must yield negated (mg, eg); \
             trivially passes at zeroed weights (open-file driver), \
             load-bearing post-M6.F"
        );
    }

    // -----------------------------------------------------------------------
    // Definitional / zone-geometry tests.
    //
    // These call `king_zone(side, ksq)` directly — the single source of truth
    // that `king_safety_term_white` will also call. A wrong shift selector
    // (north/south swapped), a missing `& !from_square(ksq)`, or an off-board
    // wrap will cause these tests to FAIL NOW against a broken `king_zone`.
    //
    // TDD state: GREEN against the correct `king_zone` (bitboard ops are
    // already correct); would FAIL immediately if `king_zone` had a bug (e.g.
    // shift_north/shift_south swapped, or the `& !ksq` mask removed).
    // -----------------------------------------------------------------------

    /// White king on e4 (central): zone = ring(8) ∪ forward-wedge(3) = 11
    /// squares, with **e4 excluded** (the `& !from_square(ksq)` mask).
    ///
    /// Ring: {d3,e3,f3,d4,f4,d5,e5,f5} (8 squares).
    /// Forward wedge (north of ring, king-square excluded): {d6,e6,f6}.
    /// Total = 11 squares. e4 ∉ zone (load-bearing: the `& !from_square(ksq)`
    /// mask prevents the shift of ring-square d5 landing back on e4 — pins the
    /// king-square exclusion and the forward-wedge-of-3 shape that a back-rank
    /// king cannot distinguish).
    ///
    /// Targets plan §7 mutation surface: wrong shift selector and missing
    /// king-square-exclusion mask both cause an immediate FAIL.
    #[test]
    fn king_zone_white_e4_central_excludes_king_square() {
        let ksq = Square::E4;
        let zone = king_zone(Color::White, ksq);

        // e4 must NOT be in the zone (& !from_square(ksq) is load-bearing).
        assert!(
            !zone.contains(ksq),
            "king square e4 must be excluded from the zone (& !from_square(ksq))"
        );
        // All 8 ring squares must be in zone.
        for sq in [
            Square::D3,
            Square::E3,
            Square::F3,
            Square::D4,
            Square::F4,
            Square::D5,
            Square::E5,
            Square::F5,
        ] {
            assert!(
                zone.contains(sq),
                "ring square {sq:?} must be in the zone for king on e4"
            );
        }
        // All 3 forward-wedge squares must be in zone.
        for sq in [Square::D6, Square::E6, Square::F6] {
            assert!(
                zone.contains(sq),
                "forward-wedge square {sq:?} must be in the zone for White king on e4"
            );
        }
        // Zone must have exactly 11 squares.
        assert_eq!(
            zone.count(),
            11,
            "central king e4 zone must have exactly 11 squares, got {}",
            zone.count()
        );
    }

    /// White king on g1 (back rank): zone = 8 squares.
    ///
    /// king_attacks(g1): g1=(file=6,rank=0). Reachable neighbors:
    ///   NW=f2, N=g2, NE=h2, W=f1, E=h1, SW/S/SE=off-board.
    ///   ring = {f1,h1,f2,g2,h2} = 5 squares.
    /// shift_north(ring) = {f2,g3,h3,f3,h2} wait:
    ///   f1→f2, h1→h2, f2→f3, g2→g3, h2→h3.
    ///   shift_north({f1,h1,f2,g2,h2}) = {f2,h2,f3,g3,h3}.
    /// zone = ring | (shift_north(ring) & !g1)
    ///      = {f1,h1,f2,g2,h2} | {f2,h2,f3,g3,h3}   (g1 ∉ shift set, mask is no-op)
    ///      = {f1,h1,f2,g2,h2,f3,g3,h3} = 8 squares.
    ///
    /// Pins the back-rank shape (no forward-wedge beyond rank 3 for a rank-1
    /// king — the shift of {f2,g2,h2} correctly adds f3,g3,h3).
    #[test]
    fn king_zone_white_g1_exact_squares() {
        let ksq = Square::G1;
        let zone = king_zone(Color::White, ksq);

        assert!(!zone.contains(ksq), "g1 must not be in its own zone");

        let expected: &[Square] = &[
            Square::F1,
            Square::H1,
            Square::F2,
            Square::G2,
            Square::H2,
            Square::F3,
            Square::G3,
            Square::H3,
        ];
        for &sq in expected {
            assert!(zone.contains(sq), "square {sq:?} must be in the Kg1 zone");
        }
        assert_eq!(
            zone.count(),
            8,
            "Kg1 zone must have exactly 8 squares, got {}",
            zone.count()
        );
    }

    /// Black king on g8 (back rank, black): forward = **south** shift.
    ///
    /// king_attacks(g8): g8=(file=6,rank=7). Neighbors:
    ///   SW=f7, S=g7, SE=h7, W=f8, E=h8; NW/N/NE=off-board.
    ///   ring = {f8,h8,f7,g7,h7} = 5 squares.
    /// shift_south(ring): f8→f7, h8→h7, f7→f6, g7→g6, h7→h6.
    ///   shift_south({f8,h8,f7,g7,h7}) = {f7,h7,f6,g6,h6}.
    /// zone = ring | (shift_south(ring) & !g8)
    ///      = {f8,h8,f7,g7,h7} | {f7,h7,f6,g6,h6}   (g8 ∉ shift set, mask no-op)
    ///      = {f8,h8,f7,g7,h7,f6,g6,h6} = 8 squares.
    ///
    /// Pins the per-side `shift_north`/`shift_south` selector: if White's
    /// `shift_north` were incorrectly used for Black, the zone would contain
    /// {f9...} (off-board, dropped) + wrong wedge squares — count and square
    /// membership both fail.
    #[test]
    fn king_zone_black_g8_exact_squares() {
        let ksq = Square::G8;
        let zone = king_zone(Color::Black, ksq);

        assert!(!zone.contains(ksq), "g8 must not be in its own zone");

        let expected: &[Square] = &[
            Square::F8,
            Square::H8,
            Square::F7,
            Square::G7,
            Square::H7,
            Square::F6,
            Square::G6,
            Square::H6,
        ];
        for &sq in expected {
            assert!(
                zone.contains(sq),
                "square {sq:?} must be in the Kg8 zone (black, shift_south)"
            );
        }
        assert_eq!(
            zone.count(),
            8,
            "Kg8 zone must have exactly 8 squares, got {}",
            zone.count()
        );
    }

    /// White king on a1 (corner): zone must not wrap off the board.
    ///
    /// king_attacks(a1): a1=(0,0). Neighbors: b1,a2,b2 = 3 squares.
    /// shift_north({b1,a2,b2}) = {b2,a3,b3}.
    /// zone = {b1,a2,b2} | ({b2,a3,b3} & !a1) = {b1,a2,b2,a3,b3} = 5 squares.
    /// No c-file squares (no wrapping from a-file shift).
    ///
    /// Pins the edge/corner no-wrap invariant from ADR-0033 §1 worked example.
    #[test]
    fn king_zone_corner_a1_no_wrap() {
        let ksq = Square::A1;
        let zone = king_zone(Color::White, ksq);

        assert!(!zone.contains(ksq), "a1 must not be in its own zone");

        let expected: &[Square] = &[Square::B1, Square::A2, Square::B2, Square::A3, Square::B3];
        for &sq in expected {
            assert!(
                zone.contains(sq),
                "square {sq:?} must be in the Ka1 corner zone"
            );
        }
        assert_eq!(
            zone.count(),
            5,
            "Ka1 corner zone must have exactly 5 squares, got {}",
            zone.count()
        );
        // No c-file squares (no wrap).
        assert!(
            !zone.contains(Square::C1),
            "c1 must NOT be in Ka1 zone (no wrap)"
        );
        assert!(
            !zone.contains(Square::C2),
            "c2 must NOT be in Ka1 zone (no wrap)"
        );
        assert!(
            !zone.contains(Square::C3),
            "c3 must NOT be in Ka1 zone (no wrap)"
        );
    }

    // -----------------------------------------------------------------------
    // Gate / S-curve tests.
    // -----------------------------------------------------------------------

    /// Gate: < 2 distinct attackers ⇒ S-curve contribution = 0 (units zeroed).
    ///
    /// Fixture: White Kg1, Black Ka8 + Qe5 (enemy queen present, 1 attacker).
    ///
    /// Weight-free precondition (calls `king_zone` + magic attacks — exercises
    /// real zone code, fails NOW if zone is wrong): Qe5 must hit ≥1 zone square
    /// of White's Kg1 zone. This verifies the attacker_count = 1 claim.
    ///
    /// Two-sided derivation from eval::data:
    ///   White Kg1 S-curve: attackers=1 < 2 ⇒ units=0 ⇒ KING_SAFETY_TABLE[0]=0 ⇒ 0.
    ///   White Kg1 shield: kf=6(g)≥5, no white pawns ⇒ 0.
    ///   White Kg1 open-file: no pawns on g/f/h:
    ///     g open ⇒ KS_KFILE_OPEN_MG. f,h adj no own ⇒ 2×KS_ADJ_SEMI_OPEN_MG + KS_BOTH.
    ///     ks_mg_white = KS_KFILE_OPEN_MG + 2×KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG.
    ///   Black Ka8 S-curve: enemy=White, no white N/B/R/Q ⇒ units=0 ⇒ 0.
    ///   Black Ka8 shield: kf=0(a)≤2, files {a,b}, no black pawns ⇒ 0.
    ///   Black Ka8 open-file: no pawns on a or b:
    ///     a open ⇒ KS_KFILE_OPEN_MG. b adj, 1 adj total ⇒ KS_ADJ_SEMI_OPEN_MG.
    ///     ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///   White-perspective total:
    ///     mg = ks_mg_white − ks_mg_black
    ///        = (KS_KFILE_OPEN_MG + 2×KS_ADJ_SEMI_OPEN_MG + KS_BOTH)
    ///          − (KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG)
    ///        = KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG.
    ///     eg = 0.
    ///   At zeroed weights: mg = 0, eg = 0.
    ///
    /// TDD state: GREEN against stub (expected = (0,0) at zeroed weights).
    /// Load-bearing at M6.F (gate < 2 → units=0 is structurally tested by the
    /// precondition asserting exactly 1 zone-hitting piece).
    #[test]
    fn king_safety_attacker_gate_below_two() {
        // White Kg1, Black Ka8 + Qe5. Lone enemy queen = 1 attacker < 2.
        let pos = pos_of("k7/8/8/4q3/8/8/8/6K1 w - - 0 1");

        // Weight-free precondition: verify exactly 1 enemy piece hits the zone.
        let white_ksq = pos.king_square(Color::White);
        let zone_w = king_zone(Color::White, white_ksq);
        let occ = pos.occupied_all();
        let q_hits = magic::queen_attacks(Square::E5, occ) & zone_w;
        assert!(
            q_hits.any(),
            "fixture invariant: Qe5 must attack ≥1 square in White Kg1 zone \
             (attacker_count = 1, gate < 2 fires)"
        );
        // No other black N/B/R attacks the zone (only the queen).
        let no_other = pos
            .pieces_colored(Color::Black, PieceKind::Knight)
            .iter()
            .all(|sq| (movegen::masks::knight_attacks(sq) & zone_w).is_empty())
            && pos
                .pieces_colored(Color::Black, PieceKind::Bishop)
                .iter()
                .all(|sq| (magic::bishop_attacks(sq, occ) & zone_w).is_empty())
            && pos
                .pieces_colored(Color::Black, PieceKind::Rook)
                .iter()
                .all(|sq| (magic::rook_attacks(sq, occ) & zone_w).is_empty());
        assert!(
            no_other,
            "fixture invariant: only the queen attacks the zone (attacker_count = 1)"
        );

        // Two-sided symbolic derivation from eval::data (all constants = 0):
        let ks_mg_white = KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG;
        let expected = (ks_mg_white - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos),
            expected,
            "1 attacker (gate < 2 fires) ⇒ S-curve = 0; \
             two-sided open-file expected ({},{}) from data",
            expected.0,
            expected.1
        );
    }

    /// Gate: ≥2 attackers but NO enemy queen ⇒ S-curve contribution = 0.
    ///
    /// Fixture: White Kg1, Black Ka8 + Nf4 + Rg5 (2 zone-attacking pieces,
    /// no queen). The "no queen" clause of the gate fires.
    ///   Nf4 attacks g2,h3 ∈ zone. Rg5 slides south: g4,g3,g2 ∈ zone (blocked
    ///   by Kg1 before leaving). Two distinct pieces → attacker_count = 2.
    ///
    /// Weight-free precondition (calls `king_zone` + attack fns): verify ≥2
    /// distinct black pieces each hit ≥1 square in the white king zone.
    ///
    /// Two-sided derivation from eval::data:
    ///   White Kg1 S-curve: enemy_has_queen=false ⇒ units=0 ⇒ 0.
    ///   White Kg1 shield: no white pawns ⇒ 0.
    ///   White Kg1 open-file: same as gate_below_two fixture:
    ///     ks_mg_white = KS_KFILE_OPEN_MG + 2×KS_ADJ_SEMI_OPEN_MG + KS_BOTH.
    ///   Black Ka8 S-curve: no white N/B/R/Q ⇒ 0.
    ///   Black Ka8 open-file: ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///   Total: mg = (KS_KFILE_OPEN_MG + 2×KS_ADJ + KS_BOTH) − (KS_KFILE_OPEN_MG + KS_ADJ)
    ///             = KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG. eg = 0.
    ///   At zeroed weights: (0, 0).
    ///
    /// TDD state: GREEN against stub. Load-bearing at M6.F (no-queen clause).
    #[test]
    fn king_safety_attacker_gate_no_queen() {
        // White Kg1, Black Ka8 + Nf4 + Rg5. No black queen.
        let pos = pos_of("k7/8/8/6r1/5n2/8/8/6K1 w - - 0 1");

        // Weight-free precondition: ≥2 enemy pieces hit the zone.
        let white_ksq = pos.king_square(Color::White);
        let zone_w = king_zone(Color::White, white_ksq);
        let occ = pos.occupied_all();
        let n_hits = movegen::masks::knight_attacks(Square::F4) & zone_w;
        let r_hits = magic::rook_attacks(Square::G5, occ) & zone_w;
        assert!(
            n_hits.any(),
            "fixture invariant: Nf4 must attack ≥1 square in White Kg1 zone"
        );
        assert!(
            r_hits.any(),
            "fixture invariant: Rg5 must attack ≥1 square in White Kg1 zone"
        );
        assert!(
            !pos.pieces_colored(Color::Black, PieceKind::Queen).any(),
            "fixture invariant: no black queen (no-queen gate must fire)"
        );

        // Two-sided symbolic derivation from eval::data (constants = 0):
        let ks_mg_white = KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG;
        let expected = (ks_mg_white - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos),
            expected,
            "2 attackers but no queen ⇒ S-curve = 0 (no-queen gate fires); \
             two-sided open-file expected ({},{}) from data",
            expected.0,
            expected.1
        );
    }

    /// Gate-PASSING S-curve test: ≥2 distinct attackers AND enemy queen present.
    ///
    /// This is the key gate-PASSING fixture: attacker_count ≥ 2 AND queen
    /// present ⇒ gate does NOT fire ⇒ units flow into SAFETY_TABLE lookup.
    /// Targets the plan §7 mutation `attackers < 2` → `<= 2`: with exactly 2
    /// zone-attacking pieces, a `<= 2` mutation would wrongly fire the gate
    /// and zero out units — distinguishable at M6.F when weights are non-zero.
    ///
    /// Fixture: White Kg1, Black Ka8 + Nd3 + Qe4.
    ///   White king zone (Kg1): {f1,h1,f2,g2,h2,f3,g3,h3} (8 squares, from
    ///   king_zone_white_g1_exact_squares). occ = {g1,a8,d3,e4}.
    ///
    ///   Nd3 knight attacks (from d3=(3,2)):
    ///     (+1,+2)=e5, (+1,-2)=e1, (-1,+2)=c5, (-1,-2)=c1,
    ///     (+2,+1)=f4, (+2,-1)=f2, (-2,+1)=b4, (-2,-1)=b2.
    ///     ∩ zone {f1,h1,f2,g2,h2,f3,g3,h3}: f2 ✓. Count = 1.
    ///     KING_ATTACK_WEIGHT_N × 1 = 0 (zeroed, lit 2×1=2).
    ///
    ///   Qe4 queen attacks (magic, occ={g1,a8,d3,e4}):
    ///     Rook-N: e5,e6,e7,e8. Rook-S: e3,e2,e1. Rook-E: f4,g4,h4. Rook-W: d4,c4,b4,a4.
    ///     Bishop-NE: f5,g6,h7. Bishop-NW: d5,c6,b7,a8(blocked). Bishop-SE: f3,g2,h1.
    ///     Bishop-SW: d3(blocked by Nd3).
    ///     ∩ zone: f3 ✓, g2 ✓, h1 ✓. Count = 3.
    ///     KING_ATTACK_WEIGHT_Q × 3 = 0 (zeroed, lit 4×3=12).
    ///
    ///   Gate: attackers_with_zone_hits = {Nd3(1 hit), Qe4(3 hits)} = 2 pieces.
    ///   enemy_has_queen = true. 2 ≥ 2 AND queen present ⇒ gate does NOT fire.
    ///   units = KING_ATTACK_WEIGHT_N×1 + KING_ATTACK_WEIGHT_Q×3 = 0+0 = 0
    ///           (lit: 2+12=14).
    ///   idx = units.clamp(0,99) = 0 (lit: 14). KING_SAFETY_TABLE[0] = 0 (lit: 2).
    ///   ks_mg_white_scurve = −KING_SAFETY_TABLE[0] = 0 (lit: −2).
    ///
    ///   White Kg1 shield: no white pawns ⇒ 0.
    ///   White Kg1 open-file: no own/any pawns on g/f/h:
    ///     ks_mg_white_openfile = KS_KFILE_OPEN_MG + 2×KS_ADJ_SEMI_OPEN_MG + KS_BOTH.
    ///   ks_mg_white = ks_mg_white_scurve + ks_mg_white_openfile.
    ///
    ///   Black Ka8 S-curve: enemy=White, no white N/B/R/Q ⇒ units=0 ⇒ 0.
    ///   Black Ka8 open-file: kf=0, no pawns:
    ///     ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    ///   Total: mg = ks_mg_white − ks_mg_black
    ///             = (−KING_SAFETY_TABLE[units.clamp(0,99) as usize]
    ///                + KS_KFILE_OPEN_MG + 2×KS_ADJ + KS_BOTH)
    ///               − (KS_KFILE_OPEN_MG + KS_ADJ)
    ///             = −KING_SAFETY_TABLE[0] + KS_ADJ + KS_BOTH.
    ///          eg = 0.
    ///   At zeroed weights: mg = 0, eg = 0.
    ///
    /// TDD state: GREEN against stub (expected = (0,0) at zeroed weights).
    /// Load-bearing post-M6.F: gate-passing path + exact per-kind unit sum.
    #[test]
    fn king_safety_attack_units_symbolic_from_data() {
        // White Kg1, Black Ka8 + Nd3 + Qe4. Exactly 2 zone-attackers + queen.
        let pos = pos_of("k7/8/8/8/4q3/3n4/8/6K1 w - - 0 1");

        // Weight-free preconditions using king_zone + attack fns.
        let white_ksq = pos.king_square(Color::White);
        let zone_w = king_zone(Color::White, white_ksq);
        let occ = pos.occupied_all();

        // Nd3 hits f2 in zone.
        let nd3_zone_hits = movegen::masks::knight_attacks(Square::D3) & zone_w;
        assert!(
            nd3_zone_hits.any(),
            "fixture invariant: Nd3 must attack ≥1 square in White Kg1 zone \
             (zone-attacker count towards the gate threshold)"
        );
        assert_eq!(
            nd3_zone_hits.count(),
            1,
            "Nd3 should hit exactly 1 zone square (f2); got {}",
            nd3_zone_hits.count()
        );

        // Qe4 hits f3,g2,h1 in zone.
        let qe4_zone_hits = magic::queen_attacks(Square::E4, occ) & zone_w;
        assert!(
            qe4_zone_hits.any(),
            "fixture invariant: Qe4 must attack ≥1 square in White Kg1 zone"
        );
        assert_eq!(
            qe4_zone_hits.count(),
            3,
            "Qe4 should hit exactly 3 zone squares (f3,g2,h1); got {}",
            qe4_zone_hits.count()
        );

        // Exactly 2 distinct zone-attacking pieces; enemy queen present.
        assert!(
            pos.pieces_colored(Color::Black, PieceKind::Queen).any(),
            "fixture invariant: enemy queen must be present (gate condition requires it)"
        );
        // Gate does NOT fire: attackers = 2 ≥ 2 AND queen present.
        // (If `attackers < 2` were mutated to `<= 2`, the gate would wrongly fire
        // at M6.F with non-zero weights — this is the plan §7 mutation it targets.)

        // Two-sided symbolic derivation from eval::data (constants = 0):
        let nd3_hits = nd3_zone_hits.count() as i32; // 1
        let qe4_hits = qe4_zone_hits.count() as i32; // 3
        let units = KING_ATTACK_WEIGHT_N * nd3_hits + KING_ATTACK_WEIGHT_Q * qe4_hits; // 0
        let idx = units.clamp(0, (KING_SAFETY_TABLE.len() - 1) as i32) as usize;
        let ks_mg_white_scurve = -KING_SAFETY_TABLE[idx];
        let ks_mg_white_openfile =
            KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let ks_mg_white = ks_mg_white_scurve + ks_mg_white_openfile;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // Ka8: open a, 1 adj
        let expected = (ks_mg_white - ks_mg_black, 0i32);

        assert_eq!(
            king_safety_term_white(&pos),
            expected,
            "gate-passing: 2 attackers + queen ⇒ units flow to SAFETY_TABLE; \
             expected ({},{}) from data (lit units={})",
            expected.0,
            expected.1,
            KING_ATTACK_WEIGHT_N * nd3_hits + KING_ATTACK_WEIGHT_Q * qe4_hits,
        );
    }

    /// Gate-PASSING S-curve: bishop + rook zone-attackers (covers B and R per-kind weights).
    ///
    /// Targets the B↔R swap and wrong-per-kind-constant mutations that the N/Q
    /// fixture cannot kill (plan §7: per-kind weight pairing is a mutation surface).
    ///
    /// Fixture: White Kg1, Black Ka8 + Be4 + Rg5 + Qd8.
    ///   White king zone (Kg1): {f1,h1,f2,g2,h2,f3,g3,h3} (8 squares).
    ///   occ = {g1, a8, e4, g5, d8}.
    ///
    ///   Be4 bishop attacks (magic, occ):
    ///     SE diagonal (toward zone): f3 ✓, g2 ✓, h1 ✓ (3 zone hits).
    ///     KING_ATTACK_WEIGHT_B × 3 = 0 (zeroed, lit 2×3=6).
    ///
    ///   Rg5 rook attacks south (occ): g4, g3 ✓, g2 ✓, g1 (blocked). 2 zone hits.
    ///     KING_ATTACK_WEIGHT_R × 2 = 0 (zeroed, lit 3×2=6).
    ///
    ///   Qd8: south d7..d1, east e8..h8, diagonal SE blocked at g5. 0 zone hits.
    ///     Attackers = 2 (Be4 + Rg5, each h>0); enemy_has_queen = true.
    ///     Gate: 2 ≥ 2 AND queen ⇒ gate does NOT fire.
    ///     units = KING_ATTACK_WEIGHT_B×3 + KING_ATTACK_WEIGHT_R×2.
    ///
    ///   White Kg1 open-file (no pawns): KS_KFILE_OPEN_MG + 2×KS_ADJ + KS_BOTH.
    ///   Black Ka8 open-file: KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    ///   Total: mg = (−KING_SAFETY_TABLE[units.clamp(0,99)] + KS_KFILE_OPEN_MG + 2×KS_ADJ + KS_BOTH)
    ///               − (KS_KFILE_OPEN_MG + KS_ADJ)
    ///            = −KING_SAFETY_TABLE[0] + KS_ADJ + KS_BOTH.
    ///          eg = 0.
    ///   At zeroed weights: (0, 0).
    ///
    /// TDD state: GREEN against the real impl at zeroed weights. Load-bearing at
    /// M6.F: a B↔R swap (KING_ATTACK_WEIGHT_B↔R) or wrong-per-kind-constant
    /// mutation is distinguishable when weights are non-zero (lit: B=6, R=6, but
    /// swapping changes individual-kind multipliers differently for mixed hit-counts).
    #[test]
    fn king_safety_attack_units_all_kinds_symbolic() {
        // White Kg1, Black Ka8 + Be4 + Rg5 + Qd8.
        let pos = pos_of("k2q4/8/8/6r1/4b3/8/8/6K1 w - - 0 1");

        // Weight-free preconditions using king_zone + attack fns.
        let white_ksq = pos.king_square(Color::White);
        let zone_w = king_zone(Color::White, white_ksq);
        let occ = pos.occupied_all();

        // Be4 hits f3, g2, h1 in zone (SE diagonal, no blockers on that ray).
        let be4_zone_hits = magic::bishop_attacks(Square::E4, occ) & zone_w;
        assert!(
            be4_zone_hits.any(),
            "fixture invariant: Be4 must attack ≥1 square in White Kg1 zone"
        );
        assert_eq!(
            be4_zone_hits.count(),
            3,
            "Be4 should hit exactly 3 zone squares (f3,g2,h1); got {}",
            be4_zone_hits.count()
        );

        // Rg5 hits g3, g2 in zone (south slide blocked by Kg1).
        let rg5_zone_hits = magic::rook_attacks(Square::G5, occ) & zone_w;
        assert!(
            rg5_zone_hits.any(),
            "fixture invariant: Rg5 must attack ≥1 square in White Kg1 zone"
        );
        assert_eq!(
            rg5_zone_hits.count(),
            2,
            "Rg5 should hit exactly 2 zone squares (g3,g2); got {}",
            rg5_zone_hits.count()
        );

        // Qd8 hits 0 zone squares (south ray d-file, east e8..h8, SE blocked by Rg5).
        let qd8_zone_hits = magic::queen_attacks(Square::D8, occ) & zone_w;
        assert_eq!(
            qd8_zone_hits.count(),
            0,
            "Qd8 should hit 0 zone squares (blocked / wrong file); got {}",
            qd8_zone_hits.count()
        );

        // Gate: attackers = 2 (Be4, Rg5 each h>0); enemy_has_queen = true.
        assert!(
            pos.pieces_colored(Color::Black, PieceKind::Queen).any(),
            "fixture invariant: enemy queen must be present"
        );

        // Two-sided symbolic derivation from eval::data.
        let be4_hits = be4_zone_hits.count() as i32; // 3
        let rg5_hits = rg5_zone_hits.count() as i32; // 2
        let units = KING_ATTACK_WEIGHT_B * be4_hits + KING_ATTACK_WEIGHT_R * rg5_hits; // 0
        let idx = units.clamp(0, (KING_SAFETY_TABLE.len() - 1) as i32) as usize;
        let ks_mg_white_scurve = -KING_SAFETY_TABLE[idx];
        let ks_mg_white_openfile =
            KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let ks_mg_white = ks_mg_white_scurve + ks_mg_white_openfile;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // Ka8: open a, 1 adj
        let expected = (ks_mg_white - ks_mg_black, 0i32);

        assert_eq!(
            king_safety_term_white(&pos),
            expected,
            "B+R attackers + queen: units flow to SAFETY_TABLE; \
             expected ({},{}) from data (lit units={})",
            expected.0,
            expected.1,
            KING_ATTACK_WEIGHT_B * be4_hits + KING_ATTACK_WEIGHT_R * rg5_hits,
        );
    }

    /// White king on g1, pawns on f2, g2 (rank-2) and h3 (rank-3).
    ///
    /// Shield derivation: kf=6(g), castled-side gate passes (kf≥5).
    ///   Files lo=5(f)..=hi=7(h); rank-2=1, rank-3=2 for white.
    ///   f(5): f2 present ⇒ SHIELD_1. g(6): g2 present ⇒ SHIELD_1.
    ///   h(7): h2 absent, h3 present ⇒ SHIELD_2.
    ///   ks_shield_white = 2×SHIELD_1_MG + SHIELD_2_MG (mg), 2×SHIELD_1_EG + SHIELD_2_EG (eg).
    ///
    /// White open-file: own pawns on f,g; h has a pawn (on h3) → has_own(h)=true.
    ///   All three files (f,g,h) have own pawns ⇒ no open/semi-open penalties.
    ///   ks_openfile_white = 0.
    ///
    /// White S-curve: no enemy N/B/R/Q ⇒ 0.
    ///   ks_mg_white = 2×SHIELD_1_MG + SHIELD_2_MG. ks_eg_white = 2×SHIELD_1_EG + SHIELD_2_EG.
    ///
    /// Black Ka8 S-curve: no white N/B/R/Q ⇒ 0.
    /// Black Ka8 shield: kf=0(a)≤2, files {a,b}, no black pawns ⇒ 0.
    /// Black Ka8 open-file: no pawns on a or b:
    ///   a open ⇒ KS_KFILE_OPEN_MG. b adj, 1 adj total ⇒ KS_ADJ_SEMI_OPEN_MG.
    ///   ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// White-perspective total:
    ///   mg = ks_mg_white − ks_mg_black
    ///      = (2×SHIELD_1_MG + SHIELD_2_MG) − (KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG).
    ///   eg = (2×SHIELD_1_EG + SHIELD_2_EG) − 0.
    ///   At zeroed weights: (0, 0).
    ///
    /// TDD state: GREEN against stub. Load-bearing at M6.F.
    #[test]
    fn king_safety_shield_kingside_symbolic() {
        // White Kg1, Pf2, Pg2, Ph3. Black Ka8 (lone king, no pawns).
        let pos = pos_of("k7/8/8/8/8/7P/5PP1/6K1 w - - 0 1");
        // Fixture invariant: white king on castled-side file (g = file 6 ≥ 5).
        assert_eq!(
            pos.king_square(Color::White).file(),
            6,
            "Kg1 must be on file g"
        );
        // Two-sided derivation from eval::data:
        let ks_mg_white = 2 * SHIELD_1_MG + SHIELD_2_MG; // shield; open-file = 0 (own pawns on f/g/h)
        let ks_eg_white = 2 * SHIELD_1_EG + SHIELD_2_EG;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // Ka8 open a-file, 1 adj
        let expected_mg = ks_mg_white - ks_mg_black;
        let expected_eg = ks_eg_white; // black Ka8 eg = 0 (no shield or open-file EG)
        assert_eq!(
            king_safety_term_white(&pos),
            (expected_mg, expected_eg),
            "kingside shield: (2×SHIELD_1+SHIELD_2) minus Ka8 open-file; \
             expected ({expected_mg},{expected_eg}) from data"
        );
    }

    /// White king on e1 (central, uncastled): shield NOT evaluated.
    ///
    /// kf=4(e); gate: kf≥5=false, kf≤2=false ⇒ shield skipped.
    ///
    /// White Ke1 open-file: has_any(4)=true (own pawn e2) ⇒ no open penalty.
    ///   adj d(3): has_own(3)=true (Pd2) ⇒ 0. f(5): has_own(5)=true (Pf2) ⇒ 0.
    ///   All files covered ⇒ ks_mg_white = 0.
    ///
    /// White S-curve: no enemy N/B/R/Q ⇒ 0.
    ///
    /// Black Ka8 S-curve: no white N/B/R/Q ⇒ 0.
    /// Black Ka8 shield: kf=0(a)≤2; files {a,b}, no black pawns ⇒ 0.
    /// Black Ka8 open-file: no pawns on a or b:
    ///   ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// Total: mg = 0 − (KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG). At zeroed weights: 0.
    ///
    /// TDD state: GREEN against stub. Load-bearing at M6.F.
    #[test]
    fn king_safety_shield_not_evaluated_when_uncastled() {
        // White Ke1 + Pd2,Pe2,Pf2,Pg2. Black Ka8.
        let pos = pos_of("k7/8/8/8/8/8/3PPPP1/4K3 w - - 0 1");
        // Fixture invariant: king on central file (e, file 4).
        assert_eq!(
            pos.king_square(Color::White).file(),
            4,
            "Ke1 must be on file e"
        );
        // Two-sided derivation from eval::data:
        // White: shield skipped (kf=4 not castled); all adj files have own pawns ⇒ 0.
        // Black Ka8: KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG;
        let expected = (0 - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos),
            expected,
            "uncastled Ke1 ⇒ shield gate off; expected ({},{}) from data",
            expected.0,
            expected.1
        );
    }

    /// Black king on c8, pawns on b7 and c7 (relative rank-2 for black).
    ///
    /// Black kf=2(c)≤2 ⇒ queenside castled gate passes. For black, rank-2
    /// (relative) = actual rank 6 (index 6); rank-3 = actual rank 5 (index 5).
    /// Shield loop files: lo=kf-1=1(b), hi=kf+1=3(d).
    ///   b(1): b7=(1,6) present ⇒ SHIELD_1.
    ///   c(2): c7=(2,6) present ⇒ SHIELD_1.
    ///   d(3): no pawn on d7(1,6) or d6(3,5) ⇒ 0.
    ///   ks_shield_black_mg = 2×SHIELD_1_MG. ks_shield_black_eg = 2×SHIELD_1_EG.
    ///
    /// Black open-file: b7 and c7 on files b and c ⇒ has_own(1)=true, has_own(2)=true.
    ///   c open-file: has_own(2)=true ⇒ no penalty.
    ///   adj b(1): has_own(1)=true ⇒ 0. d(3): no own pawn ⇒ KS_ADJ_SEMI_OPEN_MG.
    ///   ks_openfile_black_mg = KS_ADJ_SEMI_OPEN_MG (1 adj semi-open: d).
    ///   ks_mg_black = ks_shield_black_mg + ks_openfile_black_mg
    ///               = 2×SHIELD_1_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// Black S-curve: enemy=White, no white N/B/R/Q ⇒ 0.
    ///   ks_eg_black = 2×SHIELD_1_EG.
    ///
    /// White Ka1 S-curve: no black N/B/R/Q ⇒ 0.
    /// White Ka1 shield: kf=0(a)≤2; files {a,b}, no white pawns ⇒ 0.
    /// White Ka1 open-file: no pawns on a or b (white side only):
    ///   a open (no any-pawn) ⇒ KS_KFILE_OPEN_MG. b adj, only 1 adj ⇒ KS_ADJ.
    ///   ks_mg_white = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// White-perspective total:
    ///   mg = ks_mg_white − ks_mg_black
    ///      = (KS_KFILE_OPEN_MG + KS_ADJ) − (2×SHIELD_1_MG + KS_ADJ) = KS_KFILE_OPEN_MG − 2×SHIELD_1_MG.
    ///   eg = 0 − 2×SHIELD_1_EG.
    ///   At zeroed weights: (0, 0).
    ///
    /// TDD state: GREEN against stub. Pins the relative-rank mirror + {b,c,d}
    /// file window (NOT a-file, which is outside the shield loop for kf=2).
    #[test]
    fn king_safety_shield_black_queenside_relative_rank() {
        // Black Kc8 + Pb7 + Pc7. White Ka1 (lone, no pawns).
        // The shield loop for Kc8 checks files {b,c,d} (lo=1,hi=3) — a-file excluded.
        let pos = pos_of("2k5/1pp5/8/8/8/8/8/K7 w - - 0 1");
        // Fixture invariants.
        assert_eq!(
            pos.king_square(Color::Black).file(),
            2,
            "Kc8 must be on file c (kf=2 ≤ 2 → queenside shield gate passes)"
        );
        // b7 and c7 are in the {b,c,d} window; a7 is NOT checked (a-file is below lo=1).
        assert!(
            pos.pieces_colored(Color::Black, PieceKind::Pawn)
                .contains(Square::B7),
            "fixture invariant: black pawn on b7 (relative rank-2)"
        );
        assert!(
            pos.pieces_colored(Color::Black, PieceKind::Pawn)
                .contains(Square::C7),
            "fixture invariant: black pawn on c7 (relative rank-2)"
        );

        // Two-sided derivation from eval::data (all constants = 0):
        // Black: 2×SHIELD_1 (b7,c7 on rank-2) + d open-adj (1×KS_ADJ_SEMI_OPEN_MG).
        let ks_mg_black = 2 * SHIELD_1_MG + KS_ADJ_SEMI_OPEN_MG;
        let ks_eg_black = 2 * SHIELD_1_EG;
        // White Ka1: open a-file + 1 adj (b).
        let ks_mg_white = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG;
        let expected_mg = ks_mg_white - ks_mg_black;
        let expected_eg = 0 - ks_eg_black;
        assert_eq!(
            king_safety_term_white(&pos),
            (expected_mg, expected_eg),
            "black queenside shield (b7,c7 rank-2) + d adj open − Ka1 open-file; \
             expected ({expected_mg},{expected_eg}) from data"
        );
    }

    /// King file open vs semi-open precedence (MG-only, White Kg1).
    ///
    /// Two sub-fixtures test open-takes-precedence-over-semi-open on the king file.
    ///
    /// (a) Semi-open g-file: enemy pawn on g7 (g has an enemy pawn, no own).
    ///     has_any(6)=true, has_own(6)=false ⇒ KS_KFILE_SEMI_OPEN_MG (not open).
    ///     adj f(5): no own pawn ⇒ KS_ADJ. adj h(7): no own pawn ⇒ KS_ADJ.
    ///     semi_adj=2 ⇒ KS_BOTH_ADJ_SEMI_OPEN_MG.
    ///     ks_mg_white_a = KS_KFILE_SEMI_OPEN_MG + 2×KS_ADJ + KS_BOTH.
    ///
    /// (b) Open g-file: no pawns anywhere.
    ///     has_any(6)=false ⇒ KS_KFILE_OPEN_MG (open takes precedence).
    ///     adj f,h: no own ⇒ same adj/both as (a).
    ///     ks_mg_white_b = KS_KFILE_OPEN_MG + 2×KS_ADJ + KS_BOTH.
    ///
    /// Black Ka8 open-file (both fixtures): kf=0, no pawns → open a, 1 adj b.
    ///   ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// At zeroed weights: both expected = (0, 0).
    ///
    /// TDD state: GREEN against stub. Load-bearing at M6.F
    /// (open-vs-semi precedence is the mutation this pins).
    #[test]
    fn king_safety_kfile_open_vs_semi_open_symbolic() {
        // (a) Semi-open g-file: enemy pawn on g7, no own pawn.
        let pos_a = pos_of("k7/6p1/8/8/8/8/8/6K1 w - - 0 1");
        assert!(
            pos_a.pieces_colored(Color::Black, PieceKind::Pawn).any(),
            "fixture (a): enemy pawn on g7 must be present"
        );
        assert!(
            pos_a
                .pieces_colored(Color::White, PieceKind::Pawn)
                .is_empty(),
            "fixture (a): no white pawns"
        );
        let ks_mg_white_a =
            KS_KFILE_SEMI_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG;
        let expected_a = (ks_mg_white_a - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_a),
            expected_a,
            "semi-open g-file: expected ({},{}) from data",
            expected_a.0,
            expected_a.1
        );

        // (b) Open g-file: no pawns at all.
        let pos_b = pos_of("k7/8/8/8/8/8/8/6K1 w - - 0 1");
        assert!(
            pos_b
                .pieces_colored(Color::White, PieceKind::Pawn)
                .is_empty(),
            "fixture (b): no white pawns"
        );
        assert!(
            pos_b
                .pieces_colored(Color::Black, PieceKind::Pawn)
                .is_empty(),
            "fixture (b): no black pawns"
        );
        let ks_mg_white_b = KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let expected_b = (ks_mg_white_b - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_b),
            expected_b,
            "open g-file (no pawns): expected ({},{}) from data",
            expected_b.0,
            expected_b.1
        );
    }

    /// Both-adjacent-semi-open amplification and a-file degeneracy.
    ///
    /// (a1) Ke1, no pawns: e open, d+f adj both semi-open → KS_BOTH fires.
    /// (a2) Ke1, Pe2 only: e not open/semi, d+f still both semi-open → KS_BOTH.
    /// (a3) Ke1, Pe2+Pd2: e+d covered; only f adj semi-open → 1×KS_ADJ, no BOTH.
    /// (b)  Ka1, no pawns: a-file king has only 1 adjacent file (b) → KS_BOTH
    ///      can never fire (semi_adj ≤ 1).
    ///
    /// Black Ka8 (all sub-cases): kf=0(a)≤2, no pawns:
    ///   ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG.
    ///
    /// At zeroed weights: all expected = (0, 0).
    ///
    /// TDD state: GREEN against stub. Load-bearing at M6.F.
    #[test]
    fn king_safety_both_adjacent_semi_open_amplifies() {
        let ks_mg_black = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // Ka8 in all sub-cases

        // (a1) Ke1, no pawns. e open ⇒ KS_KFILE_OPEN_MG; d+f adj both semi-open.
        let pos_a1 = pos_of("k7/8/8/8/8/8/8/4K3 w - - 0 1");
        let ks_mg_white_a1 = KS_KFILE_OPEN_MG + 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let expected_a1 = (ks_mg_white_a1 - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_a1),
            expected_a1,
            "Ke1 both-adj semi-open: expected ({},{}) from data",
            expected_a1.0,
            expected_a1.1
        );

        // (a2) Ke1, Pe2. e has own pawn ⇒ no e-file penalty; d+f adj both semi-open.
        let pos_a2 = pos_of("k7/8/8/8/8/8/4P3/4K3 w - - 0 1");
        let ks_mg_white_a2 = 2 * KS_ADJ_SEMI_OPEN_MG + KS_BOTH_ADJ_SEMI_OPEN_MG;
        let expected_a2 = (ks_mg_white_a2 - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_a2),
            expected_a2,
            "Ke1 e-pawn present, d+f both semi-open: expected ({},{}) from data",
            expected_a2.0,
            expected_a2.1
        );

        // (a3) Ke1, Pe2+Pd2. e+d covered; only f semi-open → 1×KS_ADJ.
        let pos_a3 = pos_of("k7/8/8/8/8/8/3PP3/4K3 w - - 0 1");
        let ks_mg_white_a3 = KS_ADJ_SEMI_OPEN_MG;
        let expected_a3 = (ks_mg_white_a3 - ks_mg_black, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_a3),
            expected_a3,
            "Ke1 one-adj semi-open: expected ({},{}) from data",
            expected_a3.0,
            expected_a3.1
        );

        // (b) Ka1, no pawns. Only 1 adjacent file (b) → KS_BOTH never fires.
        //     White Ka1: kf=0≤2, open a ⇒ KS_KFILE_OPEN_MG. 1 adj (b) ⇒ KS_ADJ. No BOTH.
        //     Black Ka8: same.
        let pos_b = pos_of("k7/8/8/8/8/8/8/K7 w - - 0 1");
        let ks_mg_white_b = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // 1 adj only
        let ks_mg_black_b = KS_KFILE_OPEN_MG + KS_ADJ_SEMI_OPEN_MG; // Ka8 same
        let expected_b = (ks_mg_white_b - ks_mg_black_b, 0i32);
        assert_eq!(
            king_safety_term_white(&pos_b),
            expected_b,
            "Ka1 a-file: only 1 adj (b); KS_BOTH never fires; expected ({},{}) from data",
            expected_b.0,
            expected_b.1
        );
        // Structural: a-file king has only 1 adjacent file.
        let ksq_b = pos_b.king_square(Color::White);
        assert_eq!(ksq_b.file(), 0, "fixture invariant: Ka1 on a-file");
        let adj_count = [
            ksq_b.file().checked_sub(1),
            if ksq_b.file() < 7 {
                Some(ksq_b.file() + 1)
            } else {
                None
            },
        ]
        .into_iter()
        .flatten()
        .count();
        assert_eq!(
            adj_count, 1,
            "a-file king has only 1 adjacent file (b); \
             KS_BOTH_ADJ_SEMI_OPEN_MG can never be triggered (semi_adj ≤ 1)"
        );
    }

    /// Index-clamp arithmetic and no-panic check.
    ///
    /// (1) `i32::MAX.clamp(0, 99) == 99` (saturating high end).
    /// (2) `(-1i32).clamp(0, 99) == 0` (saturating low end / negative-units floor).
    /// (3) `KING_SAFETY_TABLE.len() == 100` (bounds match the clamp).
    /// (4) A contrived high-attacker FEN does not panic and returns a finite score.
    ///
    /// The clamp `units.clamp(0, 99)` is a real bounds-correct clamp: with live
    /// M6.F weights, units can exceed 99 (Q×4 × ≤11 zone squares + others).
    /// The saturating S-curve tail is correct behavior, not defensive masking.
    ///
    /// TDD state: GREEN (weight-free arithmetic + no-panic).
    #[test]
    fn king_safety_index_clamp_saturates_no_panic() {
        // (3) KING_SAFETY_TABLE.len() == 100: bounds match the clamp.
        assert_eq!(
            KING_SAFETY_TABLE.len(),
            100,
            "KING_SAFETY_TABLE must be 100 entries for the clamp to be correct"
        );
        // (1) High-end clamp.
        assert_eq!(
            i32::MAX.clamp(0, (KING_SAFETY_TABLE.len() - 1) as i32) as usize,
            99,
            "i32::MAX must clamp to 99 (table upper bound)"
        );
        // (2) Negative-input clamp to 0 (units are always ≥0 by construction,
        //     but the clamp lower bound is still correct to assert).
        assert_eq!(
            (-1i32).clamp(0, (KING_SAFETY_TABLE.len() - 1) as i32) as usize,
            0,
            "-1 must clamp to 0 (units never negative; lower-bound check)"
        );
        // (4) No-panic with many attackers near the king.
        let pos = pos_of("k7/8/8/8/1nnqqrr1/8/8/4K3 w - - 0 1");
        let (mg, eg) = king_safety_term_white(&pos);
        assert!(
            mg.abs() < 100_000,
            "king_safety_term_white must return a finite mg score; got {mg}"
        );
        assert!(
            eg.abs() < 100_000,
            "king_safety_term_white must return a finite eg score; got {eg}"
        );
    }
}
