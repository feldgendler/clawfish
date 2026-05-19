//! Tier-1 HCE features (M6.F): knight/bishop outposts, rook on open/
//! semi-open file, endgame draw-scaling. White-perspective. Computed live in
//! `evaluate_core` (not pawn-only ⇒ never cached — the ADR-0033 §6 class).
//! All additive weights zeroed + scale ≡ identity at ship (score-neutral,
//! ADR-0034 §2) — term math live, M6.H-ready.

use crate::bitboard::{self, Bitboard};
use crate::eval::data::{
    EG_SCALE_DEN, FIFTY_MOVE_FLOOR, FIFTY_MOVE_TAPER_FROM, OCB_WITH_PAWNS_SCALE, OUTPOST_BISHOP_EG,
    OUTPOST_BISHOP_MG, OUTPOST_KNIGHT_EG, OUTPOST_KNIGHT_MG, PAWNLESS_DRAW_SCALE,
    ROOK_OPEN_FILE_EG, ROOK_OPEN_FILE_MG, ROOK_SEMI_OPEN_FILE_EG, ROOK_SEMI_OPEN_FILE_MG,
};
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

/// Relative rank from `side`'s perspective: White = `sq.rank()` (0 at rank 1,
/// 7 at rank 8); Black = `7 - sq.rank()` (0 at rank 8, 7 at rank 1).
/// Always in 0..=7 — `sq.rank()` is 3-bit.
fn relative_rank(side: Color, sq: Square) -> u8 {
    match side {
        Color::White => sq.rank(),
        Color::Black => 7 - sq.rank(),
    }
}

/// Pawn-supported ∩ unchallengeable square set for `side` — the hole+support
/// predicate **pre-rank-gate** (the rank gate lives in `outpost_term_white`'s
/// scoring loop). `outpost_term_white` MUST call this seam; the structural
/// test pins the real selector (the M6.E `king_zone` seam mandate).
///
/// White hole: no black pawn can ever capture onto the square
/// (`!black_attack_front_spans(bp)`), and it is attacked by a white pawn
/// (`wp.shift_north_east() | wp.shift_north_west()`). Black is the mirror.
/// The rank gate in the caller makes the hole test sound (valid only in the
/// enemy half — see plan §3 "Outpost-hole correctness invariant").
pub(crate) fn outpost_squares(pos: &Position, side: Color) -> Bitboard {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    match side {
        Color::White => {
            let unchallengeable = !bitboard::black_attack_front_spans(bp);
            let pawn_def = wp.shift_north_east() | wp.shift_north_west();
            unchallengeable & pawn_def
        }
        Color::Black => {
            let unchallengeable = !bitboard::white_attack_front_spans(wp);
            let pawn_def = bp.shift_south_east() | bp.shift_south_west();
            unchallengeable & pawn_def
        }
    }
}

/// White-perspective `(mg, eg)` outpost contribution. Positive when white has
/// more/better outposts; negative when black does. All weights zeroed at ship.
pub(crate) fn outpost_term_white(pos: &Position) -> (i32, i32) {
    let (mut mg, mut eg) = (0i32, 0i32);
    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let outpost_sqs = outpost_squares(pos, side);
        for sq in (pos.pieces_colored(side, PieceKind::Knight) & outpost_sqs).iter() {
            let rr = relative_rank(side, sq) as usize;
            if (3..=5).contains(&rr) {
                mg += sign * OUTPOST_KNIGHT_MG[rr];
                eg += sign * OUTPOST_KNIGHT_EG[rr];
            }
        }
        for sq in (pos.pieces_colored(side, PieceKind::Bishop) & outpost_sqs).iter() {
            let rr = relative_rank(side, sq) as usize;
            if (3..=5).contains(&rr) {
                mg += sign * OUTPOST_BISHOP_MG[rr];
                eg += sign * OUTPOST_BISHOP_EG[rr];
            }
        }
    }
    (mg, eg)
}

/// White-perspective `(mg, eg)` rook-on-open/semi-open-file contribution.
/// Positive when white rooks are better placed; negative when black rooks are.
/// All weights zeroed at ship.
pub(crate) fn rook_file_term_white(pos: &Position) -> (i32, i32) {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let wp_files = bitboard::file_fill(wp);
    let bp_files = bitboard::file_fill(bp);
    let (mut mg, mut eg) = (0i32, 0i32);
    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let (own_files, enemy_files) = match side {
            Color::White => (wp_files, bp_files),
            Color::Black => (bp_files, wp_files),
        };
        for sq in pos.pieces_colored(side, PieceKind::Rook).iter() {
            let rook_bit = Bitboard::from_square(sq);
            let on_own = (own_files & rook_bit).any();
            let on_enemy = (enemy_files & rook_bit).any();
            if !on_own && !on_enemy {
                mg += sign * ROOK_OPEN_FILE_MG;
                eg += sign * ROOK_OPEN_FILE_EG;
            } else if !on_own {
                mg += sign * ROOK_SEMI_OPEN_FILE_MG;
                eg += sign * ROOK_SEMI_OPEN_FILE_EG;
            }
        }
    }
    (mg, eg)
}

/// Returns the draw-scale **numerator** out of `EG_SCALE_DEN` (identity =
/// `EG_SCALE_DEN`). `evaluate_core` applies `blended * scale / EG_SCALE_DEN`.
/// All scale tunables = `EG_SCALE_DEN` at ship ⇒ returns `EG_SCALE_DEN`
/// for every input (identity — byte-identical to M6.E). M6.H-ready.
pub(crate) fn endgame_scale(pos: &Position) -> i32 {
    let mut scale = EG_SCALE_DEN;
    if is_ocb_with_pawns(pos) {
        scale = scale.min(OCB_WITH_PAWNS_SCALE);
    }
    if is_pawnless_drawish(pos) {
        scale = scale.min(PAWNLESS_DRAW_SCALE);
    }
    // 50-move taper: linear ramp DEN → FIFTY_MOVE_FLOOR over halfmove
    // FIFTY_MOVE_TAPER_FROM..=100. At ship FLOOR == DEN ⇒ ramp flat at DEN.
    let hmc = pos.halfmove_clock() as i32;
    if hmc > FIFTY_MOVE_TAPER_FROM {
        let span = (100 - FIFTY_MOVE_TAPER_FROM).max(1);
        let prog = (hmc - FIFTY_MOVE_TAPER_FROM).clamp(0, span);
        let tapered = EG_SCALE_DEN - (EG_SCALE_DEN - FIFTY_MOVE_FLOOR) * prog / span;
        scale = scale.min(tapered);
    }
    scale.clamp(0, EG_SCALE_DEN)
}

/// `true` iff each side has exactly one bishop on **opposite** color complexes,
/// no queens, no rooks, and at least one pawn exists. The OCB draw-dampening
/// predicate (CPW Bishops-of-Opposite-Colors). Disjoint from
/// `is_kbkb_same_color`/`is_insufficient_material` (same vs opposite complex).
pub(crate) fn is_ocb_with_pawns(pos: &Position) -> bool {
    // No queens or rooks — heavy majors break OCB drawishness.
    if pos.pieces(PieceKind::Queen).any() || pos.pieces(PieceKind::Rook).any() {
        return false;
    }
    // At least one pawn must exist.
    if !pos.pieces(PieceKind::Pawn).any() {
        return false;
    }
    let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
    let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
    // Each side must have exactly one bishop.
    if wb.count() != 1 || bb.count() != 1 {
        return false;
    }
    // The two bishops must be on opposite color complexes.
    let wb_on_light = (wb & Bitboard::LIGHT_SQUARES).any();
    let bb_on_light = (bb & Bitboard::LIGHT_SQUARES).any();
    wb_on_light != bb_on_light
}

/// `true` for pawnless non-mating material residues. Zero pawns, zero queens,
/// zero rooks both sides; then either KNNvK (one side `knights==2 ∧
/// bishops==0`, other side bare K) or balanced ≤1-minor-each. Narrow
/// accept-list — explicitly excludes KBNvK, KBBvK, etc. (forced wins).
/// See ADR-0034 §3 for the full enumeration and overlap with
/// `is_insufficient_material` (unobservable in eval — early-return at
/// `eval.rs:228` precedes `endgame_scale`).
pub(crate) fn is_pawnless_drawish(pos: &Position) -> bool {
    // Zero pawns, zero queens, zero rooks — required for all accepted patterns.
    if pos.pieces(PieceKind::Pawn).any()
        || pos.pieces(PieceKind::Queen).any()
        || pos.pieces(PieceKind::Rook).any()
    {
        return false;
    }
    let wn = pos.pieces_colored(Color::White, PieceKind::Knight).count();
    let wb = pos.pieces_colored(Color::White, PieceKind::Bishop).count();
    let bn = pos.pieces_colored(Color::Black, PieceKind::Knight).count();
    let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop).count();
    let w_minors = wn + wb;
    let b_minors = bn + bb;

    // KNNvK: exactly one side has knights==2 ∧ bishops==0, other side bare K.
    let is_knnvk = (wn == 2 && wb == 0 && b_minors == 0) || (bn == 2 && bb == 0 && w_minors == 0);

    // Balanced ≤1-minor-each: each side has at most one minor piece.
    // This covers KBvKB (any complex), KNvKN, KBvKN, KBvK, KNvK, KvK.
    // KBNvK, KBBvK, etc. are excluded because they require bishops+knights≥2
    // on one side, which violates ≤1 for that side.
    let is_balanced = w_minors <= 1 && b_minors <= 1;

    is_knnvk || is_balanced
}

#[cfg(test)]
mod tests {
    use super::{
        endgame_scale, is_ocb_with_pawns, is_pawnless_drawish, outpost_squares, outpost_term_white,
        rook_file_term_white,
    };
    use crate::bitboard::{self, Bitboard};
    use crate::eval::data::{
        EG_SCALE_DEN, FIFTY_MOVE_FLOOR, FIFTY_MOVE_TAPER_FROM, OCB_WITH_PAWNS_SCALE,
        OUTPOST_BISHOP_EG, OUTPOST_BISHOP_MG, OUTPOST_KNIGHT_EG, OUTPOST_KNIGHT_MG,
        PAWNLESS_DRAW_SCALE, ROOK_OPEN_FILE_EG, ROOK_OPEN_FILE_MG, ROOK_SEMI_OPEN_FILE_EG,
        ROOK_SEMI_OPEN_FILE_MG,
    };
    use crate::piece::{Color, PieceKind};
    use crate::position::Position;
    use crate::square::Square;
    use proptest::prelude::*;

    fn pos_of(fen: &str) -> Position {
        Position::from_fen(fen).expect("fixture FEN must parse")
    }

    // -----------------------------------------------------------------------
    // Structural tests — GREEN against stub. Weight-free.
    // -----------------------------------------------------------------------

    /// Startpos: by symmetry White and Black have identical structure, so
    /// `outpost_term_white` and `rook_file_term_white` are both `(0,0)`, and
    /// `endgame_scale` is `EG_SCALE_DEN` (no draw-dampening applies at full
    /// material with many pawns). The stub trivially satisfies this contract;
    /// the real impl also returns (0,0) by material/positional symmetry and
    /// `EG_SCALE_DEN` because OCB/pawnless/50-move conditions are all false.
    ///
    /// TDD state: GREEN against stub (stub returns these exact values).
    #[test]
    fn tier1_startpos_is_neutral() {
        let pos = Position::starting_position();
        assert_eq!(
            outpost_term_white(&pos),
            (0, 0),
            "startpos is symmetric → outpost term must be (0, 0)"
        );
        assert_eq!(
            rook_file_term_white(&pos),
            (0, 0),
            "startpos is symmetric → rook-file term must be (0, 0)"
        );
        assert_eq!(
            endgame_scale(&pos),
            EG_SCALE_DEN,
            "startpos has pawns + no OCB → endgame_scale must be EG_SCALE_DEN (identity)"
        );
    }

    /// Lengths of all four outpost tables must be exactly 8 — vendoring-typo guard.
    ///
    /// TDD state: GREEN against stub (pure data assertion, does not call any fn).
    #[test]
    fn tier1_tables_have_expected_lengths() {
        assert_eq!(
            OUTPOST_KNIGHT_MG.len(),
            8,
            "OUTPOST_KNIGHT_MG must have 8 entries (relative ranks 0..7)"
        );
        assert_eq!(
            OUTPOST_KNIGHT_EG.len(),
            8,
            "OUTPOST_KNIGHT_EG must have 8 entries"
        );
        assert_eq!(
            OUTPOST_BISHOP_MG.len(),
            8,
            "OUTPOST_BISHOP_MG must have 8 entries"
        );
        assert_eq!(
            OUTPOST_BISHOP_EG.len(),
            8,
            "OUTPOST_BISHOP_EG must have 8 entries"
        );
    }

    /// Color-mirror antisymmetry: a position and its vertical color-swap must
    /// yield **negated** outpost/rook-file (mg, eg) terms, while `endgame_scale`
    /// is equal (scale is color/STM-independent).
    ///
    /// Fixture: a pawnless K+R position so neither OCB nor pawnless-drawish
    /// nor 50-move conditions fire. The halfmove_clock in both FENs is 0 (stated
    /// in the fixture comment so the scale-equality assertion is not on a moving
    /// target). At zeroed weights both terms are (0,0) trivially; the test is
    /// structural — it pins the sign convention and becomes discriminating at M6.H.
    ///
    /// Mirror: White Ka1 + Ra7, Black Kh8 ↔ White Ka8 + Ra2, Black Kh1 (exact
    /// rank/color flip). Both have halfmove_clock=0.
    ///
    /// TDD state: GREEN against stub (0,0) == −(0,0); scale DEN == DEN.
    #[test]
    fn tier1_color_mirror_antisymmetric() {
        // pos_w: White Ka1 + Ra7, Black Kh8. White to move; halfmove_clock=0.
        let pos_w = pos_of("7k/R7/8/8/8/8/8/K7 w - - 0 1");
        // pos_b: rank+color mirror — White Ka8 + Ra2, Black Kh1.
        let pos_b = pos_of("K7/8/8/8/8/8/r7/7k w - - 0 1");

        let (op_mg_w, op_eg_w) = outpost_term_white(&pos_w);
        let (op_mg_b, op_eg_b) = outpost_term_white(&pos_b);
        assert_eq!(
            (op_mg_w, op_eg_w),
            (-op_mg_b, -op_eg_b),
            "color-mirror must negate outpost (mg, eg); \
             trivially GREEN at zeroed weights, load-bearing at M6.H"
        );

        let (rf_mg_w, rf_eg_w) = rook_file_term_white(&pos_w);
        let (rf_mg_b, rf_eg_b) = rook_file_term_white(&pos_b);
        assert_eq!(
            (rf_mg_w, rf_eg_w),
            (-rf_mg_b, -rf_eg_b),
            "color-mirror must negate rook-file (mg, eg); \
             trivially GREEN at zeroed weights, load-bearing at M6.H"
        );

        assert_eq!(
            endgame_scale(&pos_w),
            endgame_scale(&pos_b),
            "endgame_scale must be equal for color-mirror positions \
             (scale is color/STM-independent; halfmove_clock=0 in both)"
        );
    }

    // -----------------------------------------------------------------------
    // Definitional / bitboard-structural tests. GREEN against stub.
    // These test the intermediate bitboard predicates that `outpost_term_white`
    // will use — independently of the function's return value.
    // -----------------------------------------------------------------------

    /// Outpost-predicate structural pin: calls `outpost_squares` (the real seam
    /// — the M6.E `king_zone`-seam mandate). A square is an outpost iff it is
    /// (a) pawn-supported AND (b) unchallengeable; the seam encapsulates both.
    ///
    /// **White-side fixtures (calls `outpost_squares(&pos, Color::White)`):**
    ///
    /// Fixture A: White Nd5, White pawns c4+e4, no black c/e pawns.
    ///   d5 ∈ white_pawn_def (c4 attacks d5, e4 attacks d5). ✓
    ///   black_attack_front_spans(∅) = ∅ → d5 ∈ unchallengeable. ✓
    ///   → d5 ∈ outpost_squares(pos, White). **RED** against EMPTY stub.
    ///
    /// Fixture B: White Nd5, no white pawns on c4/e4.
    ///   d5 ∉ pawn_def → d5 ∉ outpost_squares(pos, White). GREEN (EMPTY ⊇ false).
    ///
    /// Fixture C: White Nd5, White pawns c4+e4, Black pawn e7.
    ///   black_attack_front_spans({e7}) ⊇ d5 → challengeable → d5 ∉ outpost_squares.
    ///   GREEN against EMPTY stub.
    ///
    /// **Black-side fixture (calls `outpost_squares(&pos, Color::Black)`):**
    ///
    /// Fixture D: Black Nd4, Black pawns c5+e5, no white c/e pawns.
    ///   d4 ∈ black_pawn_def (c5 attacks d4, e5 attacks d4). ✓
    ///   white_attack_front_spans(∅) = ∅ → d4 ∈ black_unchallengeable. ✓
    ///   → d4 ∈ outpost_squares(pos, Black). **RED** against EMPTY stub.
    ///   Pins the per-side `white_attack_front_spans` / `shift_south_*` selector.
    ///
    /// TDD state: Fixture A (inclusion White) and Fixture D (inclusion Black) FAIL
    /// against EMPTY stub. Fixtures B and C (exclusion) pass against EMPTY stub.
    /// After impl all four pass.
    #[test]
    fn outpost_predicate_knight_pawn_supported_hole() {
        // Fixture A: White Nd5, c4+e4 white pawns, no black pawns.
        // d5 is pawn-supported by c4+e4 AND unchallengeable → inclusion → RED against stub.
        let pos_a = pos_of("4k3/8/8/3N4/2P1P3/8/8/4K3 w - - 0 1");
        let outpost_sqs_a = outpost_squares(&pos_a, Color::White);
        assert!(
            outpost_sqs_a.contains(Square::D5),
            "Fixture A: d5 must be in outpost_squares(White) — pawn-supported by c4+e4, \
             no black pawns (unchallengeable); RED against EMPTY stub, GREEN after impl"
        );

        // Fixture B: White Nd5, no supporting white pawns. d5 ∉ pawn_def → exclusion.
        let pos_b = pos_of("4k3/8/8/3N4/8/8/8/4K3 w - - 0 1");
        let outpost_sqs_b = outpost_squares(&pos_b, Color::White);
        assert!(
            !outpost_sqs_b.contains(Square::D5),
            "Fixture B: d5 must NOT be in outpost_squares(White) — knight is not pawn-supported"
        );

        // Fixture C: White Nd5, c4+e4 white pawns, Black pawn e7 challenges.
        // black_attack_front_spans({e7}) covers d5 → challengeable → exclusion.
        let pos_c = pos_of("4k3/4p3/8/3N4/2P1P3/8/8/4K3 w - - 0 1");
        let outpost_sqs_c = outpost_squares(&pos_c, Color::White);
        assert!(
            !outpost_sqs_c.contains(Square::D5),
            "Fixture C: d5 must NOT be in outpost_squares(White) — black pawn e7 \
             challenges via black_attack_front_spans"
        );

        // Fixture D: Black Nd4, Black pawns c5+e5, no white c/e pawns.
        // d4 ∈ black_pawn_def (c5 attacks d4 via shift_south_east, e5 via shift_south_west).
        // white_attack_front_spans(∅) = ∅ → d4 ∈ black_unchallengeable.
        // → d4 ∈ outpost_squares(pos, Black). Pins per-side selector. RED against stub.
        let pos_d = pos_of("4k3/8/8/2p1p3/3n4/8/8/4K3 w - - 0 1");
        let outpost_sqs_d = outpost_squares(&pos_d, Color::Black);
        assert!(
            outpost_sqs_d.contains(Square::D4),
            "Fixture D: d4 must be in outpost_squares(Black) — pawn-supported by c5+e5, \
             no white pawns (unchallengeable); pins the per-side shift_south_*/white_attack_front_spans \
             selector; RED against EMPTY stub, GREEN after impl"
        );
    }

    /// Outpost rank gate: only relative ranks 3..=5 (chess ranks 4–6) score.
    ///
    /// Tests two levels:
    /// 1. Rank arithmetic (weight-free, REAL pin): pins `sq.rank()` values for
    ///    boundary squares so the `(3..=5)` gate is unambiguous, plus the
    ///    out-of-band table entries `OUTPOST_*[0..2,6..7] == 0`.
    /// 2. Term-level gate exclusion (**M6.H-dormant for gate correctness, NOT a
    ///    live discriminator** — corrected per test-suite review pass 2): a
    ///    pawn-supported unchallengeable knight on rel-rank 2 / 6 produces
    ///    `(0,0)` from `outpost_term_white` — but this does **not** verify the
    ///    gate, because out-of-band table entries are pinned `0` (level 1
    ///    above), so removing/widening the `(3..=5)` gate still yields
    ///    `0 · anything = (0,0)`. The check passes against BOTH a correct impl
    ///    and a gate-absent impl, now AND post-M6.H (out-of-band entries stay
    ///    zeroed by design). The gate's correctness rests on the plan §3
    ///    design invariant (the span-complement is a valid hole test only in
    ///    the enemy half) + the bench byte-identity landing gate — NOT this
    ///    test. The term-level checks are kept only to document the fixtures
    ///    and bound the rank arithmetic (the M6.E "wrong TDD-red expectation"
    ///    lesson: do not mischaracterize what a score-neutral test pins).
    ///
    /// Fixtures:
    ///   Rel-rank 2 (White): White Ne3 (rank=2), supported by d2+f2, no black
    ///   d/f pawns (unchallengeable). Not in the enemy half.
    ///   Rel-rank 6 (White): White Ng7 (rank=6), supported by f6+h6, no black
    ///   f/h pawns (unchallengeable). Past the outpost band.
    ///
    /// TDD state: GREEN against stub. Level-1 assertions are the real
    /// weight-free pins; the level-2 term checks are M6.H-dormant for gate
    /// correctness (documented, per above).
    #[test]
    fn outpost_rank_gate_structural() {
        // e3 is rank 2 (file=4, rank=2). Rel-rank 2 for White. Gate excludes.
        assert_eq!(
            crate::square::Square::E3.rank(),
            2,
            "e3 must have rank 2 (0-indexed) so rel-rank 2 < 3 → gated out"
        );
        // g7 is rank 6 (file=6, rank=6). Rel-rank 6 for White. Gate excludes.
        assert_eq!(
            crate::square::Square::G7.rank(),
            6,
            "g7 must have rank 6 (0-indexed) so rel-rank 6 > 5 → gated out"
        );
        // d5 is rank 4 (file=3, rank=4). Rel-rank 4 for White. Gate includes → indexes [4].
        assert_eq!(
            crate::square::Square::D5.rank(),
            4,
            "d5 must have rank 4 so rel-rank 4 ∈ 3..=5 → indexes OUTPOST_KNIGHT_*[4]"
        );
        // The out-of-band table entries (ranks 0..2 and 6..7) must be 0 (they
        // are never accessed by the gated path — pinning zeroed = dead).
        for rr in [0usize, 1, 2, 6, 7] {
            assert_eq!(
                OUTPOST_KNIGHT_MG[rr], 0,
                "OUTPOST_KNIGHT_MG[{rr}] must be 0 (out-of-band, never accessed)"
            );
            assert_eq!(
                OUTPOST_KNIGHT_EG[rr], 0,
                "OUTPOST_KNIGHT_EG[{rr}] must be 0 (out-of-band, never accessed)"
            );
            assert_eq!(
                OUTPOST_BISHOP_MG[rr], 0,
                "OUTPOST_BISHOP_MG[{rr}] must be 0 (out-of-band, never accessed)"
            );
            assert_eq!(
                OUTPOST_BISHOP_EG[rr], 0,
                "OUTPOST_BISHOP_EG[{rr}] must be 0 (out-of-band, never accessed)"
            );
        }

        // Term-level gate check: rel-rank 2 — White Ne3 supported by d2+f2, no
        // black d/f pawns (unchallengeable). Gate (3..=5) must exclude Ne3 from
        // scoring ⇒ outpost_term_white == (0,0) regardless of M6.H weights.
        // d2: file=3, rank=1 attacks e3 (shift_north_east). ✓
        // f2: file=5, rank=1 attacks e3 (shift_north_west). ✓
        // Black king on a8. No black d/f pawns → unchallengeable.
        let pos_rr2 = pos_of("k7/8/8/8/8/4N3/3P1P2/4K3 w - - 0 1");
        assert_eq!(
            outpost_term_white(&pos_rr2),
            (0, 0),
            "rel-rank 2 (Ne3 pawn-supported/unchallengeable): outpost_term_white == (0,0). \
             M6.H-dormant for gate correctness — out-of-band table entries are pinned 0 \
             above, so this passes with or without the (3..=5) gate; documents the fixture"
        );

        // Term-level gate check: rel-rank 6 — White Ng7 supported by f6+h6, no
        // black f/h pawns (unchallengeable). Gate (3..=5) must exclude Ng7 from
        // scoring ⇒ outpost_term_white == (0,0).
        // f6: file=5, rank=5 attacks g7 (shift_north_east). ✓
        // h6: file=7, rank=5 attacks g7 (shift_north_west). ✓
        // Black king on a8. No black f/h pawns → unchallengeable.
        let pos_rr6 = pos_of("k7/6N1/5P1P/8/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            outpost_term_white(&pos_rr6),
            (0, 0),
            "rel-rank 6 (Ng7 pawn-supported/unchallengeable): outpost_term_white == (0,0). \
             M6.H-dormant for gate correctness — see the rel-rank-2 assertion note; \
             the gate rests on the plan §3 design invariant + the bench gate, not this test"
        );
    }

    /// Symbolic outpost value test: a known outpost configuration yields the
    /// expected value derived from `eval::data` constants (not from calling the
    /// term and asserting self-equality). Covers **rel-rank 3, 4, AND 5** to
    /// bound both gate boundaries for M6.H-revalidation.
    ///
    /// Fixture:
    ///   White Ng4 (file=6, rank=3, rel-rank 3 for White) supported by f3+h3,
    ///   unchallengeable (no black f/h pawns).
    ///   White Nd5 (file=3, rank=4, rel-rank 4) supported by c4+e4,
    ///   unchallengeable.
    ///   White Bc6 (file=2, rank=5, rel-rank 5) supported by b5, unchallengeable.
    ///   No black outposts.
    ///
    /// Expected mg = OUTPOST_KNIGHT_MG[3] + OUTPOST_KNIGHT_MG[4] + OUTPOST_BISHOP_MG[5]
    ///   (re-derived from `eval::data`).
    ///
    /// **M6.H-dormant:** at the shipped zeroed config both expected and actual
    /// are (0, 0). The test auto-revalidates when M6.H sets non-zero weights,
    /// at which point rr3 and rr5 become discriminating lower/upper gate bounds.
    ///
    /// TDD state: GREEN against stub (both expected and actual are (0, 0) at
    /// zeroed config — M6.H-dormant-by-construction).
    #[test]
    fn outpost_value_symbolic_from_data() {
        // FEN: 4k3/8/2B5/1P1N4/2P1P1N1/5P1P/8/4K3 w - - 0 1
        //   rank6: Bc6 (file=2, rank=5) rel-rank 5. Supported by b5 (file=1,rank=4
        //     attacks c6 via shift_north_east). ✓
        //   rank5: Pb5, Nd5 (file=3, rank=4) rel-rank 4. Supported by c4+e4. ✓
        //   rank4: Pc4, Pe4, Ng4 (file=6, rank=3) rel-rank 3. Supported by f3+h3
        //     (f3 attacks g4 via shift_north_east, h3 via shift_north_west). ✓
        //   rank3: Pf3, Ph3. No black pawns on any adjacent files → all unchallengeable.
        let pos = pos_of("4k3/8/2B5/1P1N4/2P1P1N1/5P1P/8/4K3 w - - 0 1");

        let expected_mg = OUTPOST_KNIGHT_MG[3] + OUTPOST_KNIGHT_MG[4] + OUTPOST_BISHOP_MG[5];
        let expected_eg = OUTPOST_KNIGHT_EG[3] + OUTPOST_KNIGHT_EG[4] + OUTPOST_BISHOP_EG[5];

        let (actual_mg, actual_eg) = outpost_term_white(&pos);
        assert_eq!(
            (actual_mg, actual_eg),
            (expected_mg, expected_eg),
            "outpost_term_white must equal \
             (OUTPOST_KNIGHT_MG[3]+OUTPOST_KNIGHT_MG[4]+OUTPOST_BISHOP_MG[5], \
              OUTPOST_KNIGHT_EG[3]+OUTPOST_KNIGHT_EG[4]+OUTPOST_BISHOP_EG[5]) \
             = ({expected_mg},{expected_eg}); covers rr 3+4+5 gate boundaries; \
             M6.H-dormant: both are 0 at zeroed config, discriminating after M6.H"
        );
    }

    /// Black outpost relative-rank mirror: Black Nd4 (rank=3 → rel-rank
    /// `7-3=4` for Black) pawn-supported/unchallengeable contributes
    /// `−OUTPOST_KNIGHT_MG[4]` (negative — Black outpost, sign=-1).
    ///
    /// This guards the `7 - rank` mirror in `relative_rank(Black, sq)`.
    ///
    /// **M6.H-dormant:** both expected and actual are 0 at zeroed config.
    ///
    /// TDD state: GREEN against stub (both are (0,0) at zeroed config).
    #[test]
    fn outpost_black_relative_rank_mirror() {
        // Black Nd4 (d4=rank3 → rel-rank 4 for Black). Black pawns c5+e5
        // support d4 (c5 attacks d4 via shift_south_east, e5 via shift_south_west).
        // No white c/e pawns → d4 unchallengeable by white pawns.
        // No white outposts (no white pawns supporting white pieces in enemy half).
        let pos = pos_of("4k3/8/8/2p1p3/3n4/8/8/4K3 w - - 0 1");

        // Verify fixture: d4 rank is 3 → black rel-rank = 7-3 = 4.
        assert_eq!(crate::square::Square::D4.rank(), 3, "d4 rank must be 3");

        let expected_mg = -OUTPOST_KNIGHT_MG[4]; // sign=-1 for Black
        let expected_eg = -OUTPOST_KNIGHT_EG[4];

        let (actual_mg, actual_eg) = outpost_term_white(&pos);
        assert_eq!(
            (actual_mg, actual_eg),
            (expected_mg, expected_eg),
            "Black outpost on d4 (rel-rank 4) must contribute \
             (-OUTPOST_KNIGHT_MG[4], -OUTPOST_KNIGHT_EG[4]) = ({expected_mg},{expected_eg}); \
             M6.H-dormant at zeroed config"
        );
    }

    // -----------------------------------------------------------------------
    // Rook-file tests.
    // -----------------------------------------------------------------------

    /// file_fill projection structural pin: the on_own/on_enemy file detection
    /// for the rook-file term uses `file_fill(own_pawns)` — any pawn on a file
    /// sets the entire file's bits, so the rook's square is set iff an own pawn
    /// is on the rook's file. Tests `bitboard::file_fill` directly (not the
    /// function), weight-free.
    ///
    /// TDD state: GREEN against stub — tests `bitboard` ops directly.
    #[test]
    fn rook_file_own_pawn_blocks_bonus_structural() {
        // White pawn on e2 → file_fill covers e1..e8.
        // White rook on e5 → same file → on_own = true.
        // White rook on d5 → different file → on_own = false.
        let pos = pos_of("4k3/8/8/3R1R2/8/8/4P3/4K3 w - - 0 1");
        let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
        let own_files = bitboard::file_fill(wp);

        let e5 = crate::square::Square::E5;
        let d5 = crate::square::Square::D5;
        let on_own_e5 = (own_files & Bitboard::from_square(e5)).any();
        let on_own_d5 = (own_files & Bitboard::from_square(d5)).any();

        assert!(
            on_own_e5,
            "file_fill: white pawn on e2 must mark e5 as 'own-pawn-on-file' \
             (rook on e5 should NOT get open-file bonus)"
        );
        assert!(
            !on_own_d5,
            "file_fill: no white pawn on d-file → d5 is NOT 'own-pawn-on-file' \
             (rook on d5 may get open/semi-open bonus)"
        );
    }

    /// Rook open-vs-semi-open precedence: rook on an open file (no pawns
    /// either color) → scores ROOK_OPEN_FILE_*; rook on semi-open (own pawn
    /// absent, enemy pawn present) → scores ROOK_SEMI_OPEN_FILE_*; rook with
    /// own pawn on file → scores 0. Values re-derived from `eval::data`.
    ///
    /// **M6.H-dormant:** all rook-file constants are 0 at ship. The test
    /// auto-revalidates when M6.H sets non-zero values.
    ///
    /// Fixture: White Ra1 (a-file: no pawns → open), White Rc1 (c-file: no
    /// white pawn, Black pawn c7 → semi-open), White Re1 (e-file: White
    /// pawn e2 → neither). One white position, one call.
    ///
    /// Expected mg = ROOK_OPEN_FILE_MG + ROOK_SEMI_OPEN_FILE_MG (from Ra1 +
    /// Rc1), re-derived from eval::data.
    ///
    /// TDD state: GREEN against stub (all constants 0, stub returns (0,0),
    /// expected is (0,0)) — M6.H-dormant.
    #[test]
    fn rook_open_vs_semi_open_precedence() {
        // Ra1: a-file, no pawns at all → open.
        // Rc1: c-file, black pawn on c7, no white c-pawn → semi-open.
        // Re1: e-file, white pawn on e2 → neither (blocked).
        // White king on h1 (not on a/c/e file so it doesn't interfere).
        let pos = pos_of("4k3/2p5/8/8/8/8/4P3/R1R1R2K w - - 0 1");

        // Verify fixture invariants via bitboard (structural, weight-free):
        let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
        let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
        let wp_files = bitboard::file_fill(wp);
        let bp_files = bitboard::file_fill(bp);

        let a1 = crate::square::Square::A1;
        let c1 = crate::square::Square::C1;
        let e1 = crate::square::Square::E1;

        let a1_bb = Bitboard::from_square(a1);
        let c1_bb = Bitboard::from_square(c1);
        let e1_bb = Bitboard::from_square(e1);

        // Ra1 on a-file: no own pawn AND no enemy pawn → open.
        assert!(
            !(wp_files & a1_bb).any() && !(bp_files & a1_bb).any(),
            "fixture: a-file must have no pawns (open file for Ra1)"
        );
        // Rc1 on c-file: no own pawn, but enemy pawn on c7 → semi-open.
        assert!(
            !(wp_files & c1_bb).any() && (bp_files & c1_bb).any(),
            "fixture: c-file must have no white pawn but a black pawn (semi-open for Rc1)"
        );
        // Re1 on e-file: white pawn on e2 → neither.
        assert!(
            (wp_files & e1_bb).any(),
            "fixture: e-file must have white pawn (blocks Re1 from open/semi-open bonus)"
        );

        // Symbolic value from eval::data (re-derived independently):
        // Ra1 → open → +ROOK_OPEN_FILE_MG/EG; Rc1 → semi-open → +ROOK_SEMI_OPEN_FILE_MG/EG
        let expected_mg = ROOK_OPEN_FILE_MG + ROOK_SEMI_OPEN_FILE_MG;
        let expected_eg = ROOK_OPEN_FILE_EG + ROOK_SEMI_OPEN_FILE_EG;
        let (actual_mg, actual_eg) = rook_file_term_white(&pos);
        assert_eq!(
            (actual_mg, actual_eg),
            (expected_mg, expected_eg),
            "rook_file_term_white: Ra1(open)+Rc1(semi-open)+Re1(blocked) → \
             (ROOK_OPEN_FILE_MG+ROOK_SEMI_OPEN_FILE_MG, \
              ROOK_OPEN_FILE_EG+ROOK_SEMI_OPEN_FILE_EG) = ({expected_mg},{expected_eg}); \
             M6.H-dormant at zeroed config"
        );
    }

    // -----------------------------------------------------------------------
    // OCB / pawnless predicates — these call `is_ocb_with_pawns` and
    // `is_pawnless_drawish` which stub to `false`. Inclusion cases FAIL
    // against stub (expected true, stub returns false). This is the intended
    // TDD-red state.
    // -----------------------------------------------------------------------

    /// OCB-with-pawns predicate: definitional cases.
    ///
    /// - KB(light)P vs KB(dark)P with no queens/rooks → `true`.
    /// - Same-complex bishops → `false`.
    /// - OCB + a rook → `false` (heavy major breaks drawishness).
    /// - OCB bishops, no pawns → `false`.
    ///
    /// TDD state: inclusion case (light/dark OCB with pawns) is RED against
    /// stub (stub returns false, test expects true). Exclusion cases are GREEN
    /// (stub returns false = correct).
    #[test]
    fn ocb_with_pawns_predicate() {
        // Light/dark OCB with pawns: White Be2 (light), Black Bc8 (dark); pawns present.
        // e2: file=4, rank=1 → 4+1=5 odd → light. c8: file=2, rank=7 → 2+7=9 odd → light.
        // Wait — c8 is light? Let me recalculate. Light squares: (file+rank) odd.
        // c8: file=2(c), rank=7 → 2+7=9 odd → light. That's same color as e2!
        // Need opposite colors. e2 is light (4+1=5 odd). Need dark: (file+rank) even.
        // d7: file=3, rank=6 → 3+6=9 odd → light. f7: file=5, rank=6 → 5+6=11 odd → light.
        // c7: file=2, rank=6 → 2+6=8 even → dark. ✓ Bc7 is dark.
        //
        // White Be2 (light, sq=12), Black Bc7 (dark, sq=50), pawns on both sides.
        // No queens, no rooks.
        let ocb_with_pawns = pos_of("4k3/2b5/8/8/8/8/4B1P1/4K3 w - - 0 1");
        // Verify light/dark: e2 light, c7 dark.
        let wb = ocb_with_pawns.pieces_colored(Color::White, PieceKind::Bishop);
        let bb_b = ocb_with_pawns.pieces_colored(Color::Black, PieceKind::Bishop);
        let wb_light = (wb & Bitboard::LIGHT_SQUARES).any();
        let bb_dark = (bb_b & Bitboard::DARK_SQUARES).any();
        assert!(
            wb_light,
            "fixture: white bishop must be on light square (e2)"
        );
        assert!(bb_dark, "fixture: black bishop must be on dark square (c7)");
        // Inclusion: must be true (FAILS against stub → RED).
        assert!(
            is_ocb_with_pawns(&ocb_with_pawns),
            "KB(light)+pawn vs KB(dark)+pawn, no majors → is_ocb_with_pawns must be true"
        );

        // Same-complex bishops → false.
        // White Be2 (light), Black Bf8 (light: file=5, rank=7 → 5+7=12 even → dark).
        // Hmm. f8: file=5(f), rank=7 → 5+7=12 even → dark. Not same as e2 (light).
        // Try Black Bg5 (light: file=6, rank=4 → 6+4=10 even → dark).
        // Actually let me use squares I can calculate reliably.
        // Light squares: (file+rank) odd. Dark squares: (file+rank) even.
        // e2 (file=4,rank=1): 4+1=5 odd → LIGHT.
        // For same-color: need another light bishop. a4 (file=0,rank=3): 0+3=3 odd → LIGHT.
        let same_complex = pos_of("4k3/8/8/8/b7/8/4B1P1/4K3 w - - 0 1");
        // a4: file=0, rank=3 → 0+3=3 odd → light. Both bishops on light → same-color.
        let wb2 = same_complex.pieces_colored(Color::White, PieceKind::Bishop);
        let bb2 = same_complex.pieces_colored(Color::Black, PieceKind::Bishop);
        let both_light =
            (wb2 & Bitboard::LIGHT_SQUARES).any() && (bb2 & Bitboard::LIGHT_SQUARES).any();
        assert!(
            both_light,
            "fixture: both bishops on light squares for same-complex test"
        );
        assert!(
            !is_ocb_with_pawns(&same_complex),
            "same-complex bishops → is_ocb_with_pawns must be false"
        );

        // OCB + rook → false (major piece breaks drawishness).
        // White Be2 (light), Black Bc7 (dark), White rook Ra1, pawns.
        let ocb_with_rook = pos_of("4k3/2b5/8/8/8/8/4B1P1/R3K3 w - - 0 1");
        assert!(
            !is_ocb_with_pawns(&ocb_with_rook),
            "OCB bishops + rook → is_ocb_with_pawns must be false (major piece present)"
        );

        // OCB bishops, no pawns → false.
        let ocb_no_pawns = pos_of("4k3/2b5/8/8/8/8/4B3/4K3 w - - 0 1");
        assert!(
            !is_ocb_with_pawns(&ocb_no_pawns),
            "OCB bishops, no pawns → is_ocb_with_pawns must be false (requires ≥1 pawn)"
        );
    }

    /// OCB disjoint from KBvKB-same-color: a same-complex KBvKB position must
    /// have `is_ocb_with_pawns == false` AND `is_insufficient_material == true`
    /// (the two predicates are disjoint — no double handling).
    ///
    /// TDD state: GREEN against stub (stub returns false; is_insufficient_material
    /// is a private fn, but we test via `evaluate` == 0).
    #[test]
    fn ocb_disjoint_from_kbkb_same_color() {
        use crate::eval::evaluate;
        // KBvKB same-color (both light): White Be2 (light), Black Ba5 (light: 0+4=4 even... wait).
        // a5: file=0, rank=4 → 0+4=4 even → DARK. Not light.
        // Need light: (file+rank) odd. b4: file=1, rank=3 → 1+3=4 even → dark.
        // c5: file=2, rank=4 → 2+4=6 even → dark.
        // d5: file=3, rank=4 → 3+4=7 odd → LIGHT. ✓ Black Bd5.
        let pos = pos_of("4k3/8/8/3b4/8/8/4B3/4K3 w - - 0 1");
        // Verify: White Be2 (light), Black Bd5 (light). Same-color.
        let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
        let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
        assert!(
            (wb & Bitboard::LIGHT_SQUARES).any() && (bb & Bitboard::LIGHT_SQUARES).any(),
            "fixture: both bishops on light squares for KBvKB same-color"
        );
        // OCB predicate must be false (same complex, not opposite).
        assert!(
            !is_ocb_with_pawns(&pos),
            "KBvKB same-color → is_ocb_with_pawns must be false (same complex)"
        );
        // insufficient_material must be true → evaluate returns 0.
        assert_eq!(
            evaluate(&pos),
            0,
            "KBvKB same-color → evaluate must be 0 (insufficient material; \
             the two predicates are disjoint)"
        );
    }

    /// Pawnless-drawish predicate: definitional in-isolation cases (the seam,
    /// not via `endgame_scale`).
    ///
    /// Inclusion cases (expect true — RED against stub which returns false):
    ///   - KNNvK → true (canonical pawnless draw, new coverage beyond M6.A)
    ///   - KBvKN (balanced ≤1-minor-each) → true
    ///   - KNvKN → true
    ///
    /// Rejection cases (expect false — GREEN against stub):
    ///   - KRvK → false (rook clause)
    ///   - KQvKB → false (queen clause)
    ///   - KPvK → false (pawn clause)
    ///
    /// Note: the predicate returns true for some insufficient-material positions
    /// (KBvK, KNvK satisfy "balanced ≤1-minor-each"). This is expected and
    /// documented — the overlap is unobservable in eval (early-return at
    /// `eval.rs:228` precedes `endgame_scale`). Tested in isolation here.
    #[test]
    fn pawnless_drawish_predicate() {
        // KNNvK: White Ke1+Nb1+Nc1, Black Ka8. KNN cannot force mate.
        let knnvk = pos_of("k7/8/8/8/8/8/8/1NNK4 w - - 0 1");
        assert!(
            is_pawnless_drawish(&knnvk),
            "KNNvK → is_pawnless_drawish must be true (canonical pawnless draw)"
        );

        // KBvKN: balanced ≤1-minor-each → true.
        let kbvkn = pos_of("4k3/8/8/8/8/8/8/1B2K1n1 w - - 0 1");
        assert!(
            is_pawnless_drawish(&kbvkn),
            "KBvKN (balanced ≤1-minor-each) → is_pawnless_drawish must be true"
        );

        // KNvKN → true.
        let knvkn = pos_of("4k3/8/8/8/8/8/8/N3K1n1 w - - 0 1");
        assert!(
            is_pawnless_drawish(&knvkn),
            "KNvKN (balanced ≤1-minor-each) → is_pawnless_drawish must be true"
        );

        // KRvK → false (rook present).
        let krvk = pos_of("4k3/8/8/8/8/8/8/R3K3 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&krvk),
            "KRvK → is_pawnless_drawish must be false (rook clause)"
        );

        // KQvKB → false (queen present).
        let kqvkb = pos_of("4k3/8/8/8/8/8/8/Q3K1b1 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&kqvkb),
            "KQvKB → is_pawnless_drawish must be false (queen clause)"
        );

        // KPvK → false (pawn present).
        let kpvk = pos_of("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&kpvk),
            "KPvK → is_pawnless_drawish must be false (pawn clause)"
        );
    }

    /// The won-endgame guard: KBNvK, KBBvK, and KBBvKN must NOT be flagged
    /// drawish (they are forced wins). Pins the narrow accept-list is not a
    /// "no-major sweep".
    ///
    /// TDD state: GREEN against stub (stub returns false, expected is false).
    #[test]
    fn pawnless_drawish_excludes_kbnvk() {
        // KBNvK: forced win. White Ke1+Nb1+Bc1, Black Ka8.
        let kbnvk = pos_of("k7/8/8/8/8/8/8/1BNK4 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&kbnvk),
            "KBNvK → is_pawnless_drawish must be false (forced win, not a draw)"
        );

        // KBBvK: two bishops vs bare king, forced win.
        let kbbvk = pos_of("k7/8/8/8/8/8/8/1BBK4 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&kbbvk),
            "KBBvK → is_pawnless_drawish must be false (two bishops, forced win)"
        );

        // KBBvKN: bishops+knights ≥ 2 on one side (non-KNNvK pattern) → false.
        let kbbvkn = pos_of("4kn2/8/8/8/8/8/8/1BBK4 w - - 0 1");
        assert!(
            !is_pawnless_drawish(&kbbvkn),
            "KBBvKN → is_pawnless_drawish must be false (bishops+knights ≥ 2, non-KNNvK)"
        );
    }

    // -----------------------------------------------------------------------
    // Endgame scale tests.
    // -----------------------------------------------------------------------

    /// At the shipped config, an OCB-with-pawns fixture returns
    /// `EG_SCALE_DEN.min(OCB_WITH_PAWNS_SCALE)` — exactly one condition fires.
    ///
    /// Fixture: White Be2 (light) + pawn g2, Black Bc7 (dark), Kings e1/e8.
    /// No queens, no rooks (OCB condition), no pawnless trigger. hmc=0 (no
    /// 50-move trigger). Only the OCB-with-pawns condition is active.
    ///
    /// **M6.H-dormant:** at ship OCB_WITH_PAWNS_SCALE == EG_SCALE_DEN so the
    /// result is EG_SCALE_DEN. After M6.H sets OCB_WITH_PAWNS_SCALE < DEN,
    /// this fixture returns the tuned scale (discriminating).
    ///
    /// TDD state: GREEN against stub (stub returns EG_SCALE_DEN; expected also
    /// equals EG_SCALE_DEN at ship because OCB_WITH_PAWNS_SCALE==DEN).
    #[test]
    fn endgame_scale_ocb_condition_at_ship_symbolic() {
        let ocb = pos_of("4k3/2b5/8/8/8/8/4B1P1/4K3 w - - 0 1");
        let expected = OCB_WITH_PAWNS_SCALE.min(EG_SCALE_DEN);
        assert_eq!(
            endgame_scale(&ocb),
            expected,
            "OCB-with-pawns position → endgame_scale must be \
             min(OCB_WITH_PAWNS_SCALE, DEN) = {expected}; \
             M6.H-dormant (equals DEN at ship since OCB_WITH_PAWNS_SCALE==DEN)"
        );
    }

    /// At the shipped config, a pawnless-drawish fixture returns
    /// `EG_SCALE_DEN.min(PAWNLESS_DRAW_SCALE)` — exactly one condition fires.
    ///
    /// Fixture: KNNvK (White Ke1+Nb1+Nc1, Black Ka8). Zero pawns, zero queens,
    /// zero rooks, KNN cannot force mate → pawnless-drawish fires. No OCB
    /// (no bishops), hmc=0 (no 50-move trigger). Only the pawnless condition.
    ///
    /// **M6.H-dormant:** at ship PAWNLESS_DRAW_SCALE == EG_SCALE_DEN so the
    /// result is EG_SCALE_DEN. After M6.H, this fixture returns the tuned scale.
    ///
    /// TDD state: GREEN against stub (stub returns EG_SCALE_DEN; expected also
    /// equals EG_SCALE_DEN at ship because PAWNLESS_DRAW_SCALE==DEN).
    #[test]
    fn endgame_scale_pawnless_condition_at_ship_symbolic() {
        let pawnless = pos_of("k7/8/8/8/8/8/8/1NNK4 w - - 0 1");
        let expected = PAWNLESS_DRAW_SCALE.min(EG_SCALE_DEN);
        assert_eq!(
            endgame_scale(&pawnless),
            expected,
            "pawnless-drawish (KNNvK) position → endgame_scale must be \
             min(PAWNLESS_DRAW_SCALE, DEN) = {expected}; \
             M6.H-dormant (equals DEN at ship since PAWNLESS_DRAW_SCALE==DEN)"
        );
    }

    /// At the shipped config, a high halfmove-clock fixture returns the taper
    /// formula value — exactly one condition fires (50-move taper only).
    ///
    /// Fixture: startpos with hmc=90 (above FIFTY_MOVE_TAPER_FROM=80). Many
    /// pawns → no OCB, no pawnless. Only the 50-move taper applies.
    ///
    /// Expected re-derived from `eval::data` taper formula (not a literal):
    ///   span = max(100 - FIFTY_MOVE_TAPER_FROM, 1)
    ///   prog = clamp(90 - FIFTY_MOVE_TAPER_FROM, 0, span)
    ///   tapered = EG_SCALE_DEN - (EG_SCALE_DEN - FIFTY_MOVE_FLOOR) * prog / span
    ///   expected = clamp(tapered, 0, EG_SCALE_DEN).min(EG_SCALE_DEN)
    ///
    /// **M6.H-dormant:** at ship FIFTY_MOVE_FLOOR == EG_SCALE_DEN so
    /// (DEN - DEN)*prog/span == 0 → tapered == DEN → expected == DEN. After M6.H
    /// sets FIFTY_MOVE_FLOOR < DEN, this fixture returns a value < DEN (discriminating).
    ///
    /// TDD state: GREEN against stub (stub returns EG_SCALE_DEN; expected also
    /// equals EG_SCALE_DEN at ship since FIFTY_MOVE_FLOOR==DEN).
    #[test]
    fn endgame_scale_fifty_move_condition_at_ship_symbolic() {
        let high_hmc = pos_of("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 90 45");
        let span = (100 - FIFTY_MOVE_TAPER_FROM).max(1);
        let prog = (90 - FIFTY_MOVE_TAPER_FROM).clamp(0, span);
        let tapered = EG_SCALE_DEN - (EG_SCALE_DEN - FIFTY_MOVE_FLOOR) * prog / span;
        let expected = tapered.clamp(0, EG_SCALE_DEN);
        assert_eq!(
            endgame_scale(&high_hmc),
            expected,
            "high halfmove-clock (hmc=90) → endgame_scale must be {expected} \
             (taper formula re-derived from eval::data); M6.H-dormant: equals \
             DEN at ship since FIFTY_MOVE_FLOOR==EG_SCALE_DEN"
        );
    }

    /// Pure-arithmetic 50-move ramp structural pin: the taper formula is
    /// monotone non-increasing in halfmove_clock and equals DEN at
    /// FIFTY_MOVE_TAPER_FROM and FLOOR at halfmove=100. Does NOT touch
    /// `eval::data` — pins the formula independent of shipped config.
    ///
    /// Tests a *hypothetical* FLOOR < DEN (e.g., floor=16) to make the ramp
    /// non-flat — the M6.H-realistic case. Does not call `endgame_scale`.
    ///
    /// TDD state: GREEN against stub — pure arithmetic, does not call any fn.
    #[test]
    fn endgame_scale_fifty_move_ramp_structural() {
        let den = 64i32;
        let taper_from = 80i32;
        let floor = 16i32; // hypothetical < DEN to make ramp non-trivial

        // Ramp formula (mirrors the impl):
        let span = (100 - taper_from).max(1);
        let ramp = |hmc: i32| -> i32 {
            if hmc > taper_from {
                let prog = (hmc - taper_from).clamp(0, span);
                let tapered = den - (den - floor) * prog / span;
                tapered.clamp(0, den)
            } else {
                den
            }
        };

        // At taper_from: ramp must be DEN (onset).
        assert_eq!(
            ramp(taper_from),
            den,
            "at halfmove_clock == FIFTY_MOVE_TAPER_FROM, ramp must equal DEN"
        );

        // At halfmove 100: ramp must be FLOOR (full taper).
        // prog = (100 - 80).clamp(0, 20) = 20 = span.
        // tapered = 64 - (64-16)*20/20 = 64 - 48 = 16 = floor.
        assert_eq!(
            ramp(100),
            floor,
            "at halfmove_clock == 100, ramp must equal FLOOR={floor}"
        );

        // Monotone non-increasing: ramp(hmc) >= ramp(hmc+1) for hmc in 0..100.
        for hmc in 0i32..100 {
            assert!(
                ramp(hmc) >= ramp(hmc + 1),
                "ramp must be monotone non-increasing: ramp({hmc})={} >= ramp({})={}",
                ramp(hmc),
                hmc + 1,
                ramp(hmc + 1)
            );
        }

        // Clamp: ramp never exceeds DEN and never goes below 0.
        for hmc in 0i32..=100 {
            let v = ramp(hmc);
            assert!(
                v >= 0 && v <= den,
                "ramp({hmc}) = {v} must be in [0, {den}]"
            );
        }
    }

    // Property: `endgame_scale` always returns a value in `[0, EG_SCALE_DEN]`
    // (the clamp invariant). Positions built via FEN strings only — never raw
    // field-poking — so `king_square`/material counts assume a well-formed
    // position (exactly one king per side).
    //
    // M6.H-dormant-by-construction: at ship every scale tunable ==
    // EG_SCALE_DEN so the bound holds trivially for all inputs regardless of
    // taper/clamp correctness. This proptest is the M6.H-revalidation pin
    // (auto-discriminates the instant M6.H sets a tunable < DEN), NOT a live
    // correctness signal. The now-meaningful taper pin is
    // `endgame_scale_fifty_move_ramp_structural` (pure arithmetic,
    // config-independent).
    //
    // TDD state: GREEN against stub (stub returns EG_SCALE_DEN which satisfies
    // 0 <= EG_SCALE_DEN <= EG_SCALE_DEN). M6.H-dormant.
    proptest! {
        #[test]
        fn endgame_scale_never_exceeds_den(
            // Use a handful of known-good FENs so we always have well-formed positions.
            idx in 0usize..5,
            hmc in 0u8..=100u8,
        ) {
            const PROP_FENS: &[&str] = &[
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
                "4k3/2b5/8/8/8/8/4B1P1/4K3 w - - 0 1", // OCB-with-pawns
                "k7/8/8/8/8/8/8/1NNK4 w - - 0 1",        // KNNvK pawnless
                "4k3/8/8/8/8/8/8/1B2K1n1 w - - 0 1",    // KBvKN pawnless
            ];
            // Rebuild with the proptest halfmove_clock by injecting into the FEN.
            // Parse the base FEN, then rewrite the halfmove field.
            let base = PROP_FENS[idx];
            let parts: Vec<&str> = base.split(' ').collect();
            prop_assume!(parts.len() == 6);
            let fen_with_hmc = format!(
                "{} {} {} {} {} {}",
                parts[0], parts[1], parts[2], parts[3], hmc, parts[5]
            );
            let pos = Position::from_fen(&fen_with_hmc).expect("proptest FEN must parse");
            let scale = endgame_scale(&pos);
            prop_assert!(
                (0..=EG_SCALE_DEN).contains(&scale),
                "endgame_scale must be in [0, EG_SCALE_DEN={EG_SCALE_DEN}]; got {scale}"
            );
        }
    }
}
