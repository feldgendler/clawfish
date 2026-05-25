//! Material + PST evaluation — tapered MG/EG blend (M6.A).
//!
//! `evaluate(&Position) -> i32` returns a side-to-move-relative score in
//! centipawns. The backing triple `(static_mg_white, static_eg_white, raw_phase)`
//! is maintained incrementally by `make_move` / `unmake_move`; this module
//! provides `eval_state_from_scratch` for construction, parse, and debug
//! round-trip asserts.
//!
//! PST data: PeSTO MG + EG tables, vendored verbatim.
//! Insufficient-material detection: KvK, KvN, KvB, KBvKB-same-color → 0.

use crate::bitboard::Bitboard;
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

pub(crate) mod king_safety;
pub(crate) mod mobility;
pub(crate) mod pawns;
pub(crate) mod tier1;

pub(crate) mod data;
use data::{
    EG_BISHOP_PST, EG_KING_PST, EG_KNIGHT_PST, EG_PAWN_PST, EG_QUEEN_PST, EG_ROOK_PST,
    MG_BISHOP_PST, MG_KING_PST, MG_KNIGHT_PST, MG_PAWN_PST, MG_QUEEN_PST, MG_ROOK_PST,
};

/// MG centipawn material values. Re-exported for use in MVV-LVA move ordering.
pub(crate) use data::MATERIAL;

/// Per-kind phase contribution. Re-exported for use in `update_static_eval_after_make`.
pub(crate) use data::PHASE_DELTA;

// ---------------------------------------------------------------------------
// Compile-time PSQT tables.
//
// PSQT_MG[color][kind][sq] and PSQT_EG[color][kind][sq] are the signed
// contributions to `static_mg_white` / `static_eg_white` respectively.
//   White: +(material[kind] + pst[kind][sq ^ 56])
//   Black: -(material[kind] + pst[kind][sq])
//
// PeSTO arrays use a8-origin indexing; `build_psqt_table` applies the
// LERF ↔ PeSTO flip via `sq ^ 56` for white-piece lookups.
// ---------------------------------------------------------------------------

const MG_TABLES: [[i32; 64]; 6] = [
    MG_PAWN_PST,
    MG_KNIGHT_PST,
    MG_BISHOP_PST,
    MG_ROOK_PST,
    MG_QUEEN_PST,
    MG_KING_PST,
];

const EG_TABLES: [[i32; 64]; 6] = [
    EG_PAWN_PST,
    EG_KNIGHT_PST,
    EG_BISHOP_PST,
    EG_ROOK_PST,
    EG_QUEEN_PST,
    EG_KING_PST,
];

/// `PSQT_MG[color_idx][kind_idx][sq_idx]` — MG piece-square contribution.
pub(crate) const PSQT_MG: [[[i32; 64]; 6]; 2] = build_psqt_table(&MG_TABLES, &data::MATERIAL);

/// `PSQT_EG[color_idx][kind_idx][sq_idx]` — EG piece-square contribution.
pub(crate) const PSQT_EG: [[[i32; 64]; 6]; 2] = build_psqt_table(&EG_TABLES, &data::MATERIAL_EG);

const fn build_psqt_table(tables: &[[i32; 64]; 6], material: &[i32; 6]) -> [[[i32; 64]; 6]; 2] {
    let mut psqt = [[[0i32; 64]; 6]; 2];
    let mut kind = 0usize;
    while kind < 6 {
        let mut sq = 0usize;
        while sq < 64 {
            let mat = material[kind];
            psqt[Color::White as usize][kind][sq] = mat + tables[kind][sq ^ 56];
            psqt[Color::Black as usize][kind][sq] = -(mat + tables[kind][sq]);
            sq += 1;
        }
        kind += 1;
    }
    psqt
}

// Bishop-pair bonus values (MG/EG). FROZEN reference frame (not tuned — the
// M6.I joint Texel pass holds these fixed; ADR-0037 §3). `pub(crate)` so the
// `texel::features` reference oracle reads the same frozen values the engine
// uses — an additive, read-only widening that does not change `evaluate`.
pub(crate) const BISHOP_PAIR_MG: i32 = 30;
pub(crate) const BISHOP_PAIR_EG: i32 = 50;

// Mop-up fires when raw_phase < MOP_UP_PHASE_MAX (covers KQK, KRK, etc.).
const MOP_UP_PHASE_MAX: u8 = 5;
const MOP_UP_MIN_ADVANTAGE_CP: i32 = 100;

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Recompute the eval triple `(mg_white, eg_white, raw_phase)` from scratch.
/// Single pass — used by `Position::refresh_static_eval` and debug round-trip
/// asserts in `make_move` / `unmake_move`.
pub(crate) fn eval_state_from_scratch(pos: &Position) -> (i32, i32, u8) {
    let mut mg = 0i32;
    let mut eg = 0i32;
    let mut phase: u32 = 0;
    for kind in PieceKind::ALL {
        for color in [Color::White, Color::Black] {
            let mut bb = pos.pieces_colored(color, kind);
            while let Some(sq) = bb.pop_lsb() {
                mg += PSQT_MG[color as usize][kind.index()][sq.index() as usize];
                eg += PSQT_EG[color as usize][kind.index()][sq.index() as usize];
                phase += PHASE_DELTA[kind.index()] as u32;
            }
        }
    }
    debug_assert!(
        phase <= 255,
        "phase overflow (only possible with > 255-piece-equivalent material)"
    );
    (mg, eg, phase as u8)
}

/// Returns `true` if the position is an insufficient-material draw.
/// Covers KvK, KvN, KvB, and KBvKB-same-color.
///
/// `pub(crate)` so the `texel::features` reference oracle can reproduce
/// `evaluate_core`'s insufficient-material early-return (additive read-only
/// widening — `evaluate` is unchanged).
pub(crate) fn is_insufficient_material(pos: &Position) -> bool {
    let total_count = pos.occupied_all().count();
    match total_count {
        2 => true,
        3 => {
            let knight_count = pos.pieces(PieceKind::Knight).count();
            let bishop_count = pos.pieces(PieceKind::Bishop).count();
            knight_count == 1 || bishop_count == 1
        }
        4 => is_kbkb_same_color(pos),
        _ => false,
    }
}

/// Returns `true` when the position has exactly 4 pieces, each side has
/// exactly one bishop, and both bishops are on the **same** color complex.
/// The caller guarantee is `pos.occupied_all().count() == 4`.
fn is_kbkb_same_color(pos: &Position) -> bool {
    let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
    let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
    if wb.count() != 1 || bb.count() != 1 {
        return false;
    }
    let wb_on_light = (wb & Bitboard::LIGHT_SQUARES).any();
    let bb_on_light = (bb & Bitboard::LIGHT_SQUARES).any();
    wb_on_light == bb_on_light
}

/// Returns the bishop-pair bonus for white minus the bonus for black.
/// `value` is the per-side bonus (pass `BISHOP_PAIR_MG` or `BISHOP_PAIR_EG`).
/// Returns `+value` if only white has the pair, `-value` if only black has it,
/// `0` if both or neither have it.
pub(crate) fn bishop_pair_term_white(pos: &Position, value: i32) -> i32 {
    let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
    let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
    let white_pair = (wb & Bitboard::LIGHT_SQUARES).any() && (wb & Bitboard::DARK_SQUARES).any();
    let black_pair = (bb & Bitboard::LIGHT_SQUARES).any() && (bb & Bitboard::DARK_SQUARES).any();
    match (white_pair, black_pair) {
        (true, false) => value,
        (false, true) => -value,
        _ => 0,
    }
}

/// Center Manhattan distance — distance from `sq` to the nearest of the four
/// center squares {d4, e4, d5, e5}. Returns a value in `[0, 6]`.
pub(crate) fn center_manhattan_distance(sq: Square) -> i32 {
    let f = sq.file() as i32;
    let r = sq.rank() as i32;
    let fd = (f - 3).abs().min((f - 4).abs());
    let rd = (r - 3).abs().min((r - 4).abs());
    fd + rd
}

/// Chebyshev distance between two squares: `max(|file_diff|, |rank_diff|)`.
/// Returns a value in `[0, 7]`.
pub(crate) fn chebyshev_distance(a: Square, b: Square) -> i32 {
    let fd = (a.file() as i32 - b.file() as i32).abs();
    let rd = (a.rank() as i32 - b.rank() as i32).abs();
    fd.max(rd)
}

/// Mop-up term: drives the losing king to a corner and the winning king close.
/// Returns a white-perspective signed value (+bonus when white winning,
/// -bonus when black winning). Returns 0 when the phase or advantage gate
/// is not met.
///
/// `mg_white` and `eg_white` are the incremental field values (before
/// bishop-pair addends), used to estimate the blended advantage.
pub(crate) fn mop_up_term_white(pos: &Position, mg_white: i32, eg_white: i32) -> i32 {
    if pos.raw_phase() >= MOP_UP_PHASE_MAX {
        return 0;
    }
    // Estimate blended advantage using the same formula as static_eval_white()
    // but without bishop-pair (consistent with the caller's inputs).
    let phase = pos.raw_phase().min(24) as i32;
    let advantage = (mg_white * phase + eg_white * (24 - phase)) / 24;
    if advantage.abs() <= MOP_UP_MIN_ADVANTAGE_CP {
        return 0;
    }

    let white_king = pos.king_square(Color::White);
    let black_king = pos.king_square(Color::Black);

    if advantage > 0 {
        // White winning: drive black king to the corner.
        4 * center_manhattan_distance(black_king)
            + 2 * (14 - chebyshev_distance(white_king, black_king))
    } else {
        // Black winning: drive white king to the corner.
        -(4 * center_manhattan_distance(white_king)
            + 2 * (14 - chebyshev_distance(black_king, white_king)))
    }
}

/// Shared blend: PST + bishop-pair + pawn-structure + mop-up, white-perspective,
/// then STM flip. Pawn structure enters only here (ADR-0032 §4), not in the
/// incremental accessor or the mop-up estimate.
///
/// Mop-up uses PST-only `(mg, eg)` inputs — bishop-pair and pawn structure are
/// deliberately excluded from the mop-up advantage estimate (ADR-0031 / ADR-0032
/// §4). This is the M6.A precedent preserved verbatim.
fn evaluate_core(pos: &Position, pe: pawns::PawnEval) -> i32 {
    use crate::search::MATE_IN_MAX_PLY;

    if is_insufficient_material(pos) {
        return 0;
    }

    let mg = pos.static_mg_white();
    let eg = pos.static_eg_white();
    let phase = pos.raw_phase().min(24) as i32;

    let bp_mg = bishop_pair_term_white(pos, BISHOP_PAIR_MG);
    let bp_eg = bishop_pair_term_white(pos, BISHOP_PAIR_EG);
    // Mop-up takes PST-only (mg, eg) — no bishop-pair, no pawn structure
    // (ADR-0032 §4; M6.A precedent). The mop_up_addend_excludes_pawn_structure
    // regression test pins this call site.
    let mop_up = mop_up_term_white(pos, mg, eg);

    // M6.B ships the pawn-structure eval with the **connected-pawn term only**
    // active: ISO/DBL/BWD weights are zeroed in `eval::data` (kept as named
    // M6.F-tunable constants), CONN keeps its literature-default table. A
    // per-term SPRT screen + confirmation vs `M6.A` (single-TC 10+0.1; see
    // docs/milestones/m6.b.md + ADR-0032 §7) showed each term individually
    // positive-to-neutral (ISO +82.7, DBL +31.4, BWD −7.5, CONN +103.1) but
    // every multi-term subset collapses via a connectivity-axis double-count
    // (ISO×CONN −197.9 H0; all-four −99.9; ISO+DBL+CONN +9.9 neutral) —
    // ISO and CONN are opposite-signed measurements of the same axis with
    // mismatched scale, coherent only after the joint Texel co-calibration
    // that ADR-0032 §6 always assigned to M6.F. CONN-only is the largest
    // confirmed gain *and* structurally interaction-immune (one term). `pe`
    // still computes ISO/DBL/BWD (× zero weight) so the structure is
    // M6.F-ready, and caches `pe.passed` for M6.C. **M6.F**: re-introduce
    // ISO/DBL/BWD via joint Texel against the CONN-only baseline.
    const PAWN_STRUCTURE_IN_EVAL: bool = true;
    let (pe_mg, pe_eg) = if PAWN_STRUCTURE_IN_EVAL {
        (pe.mg, pe.eg)
    } else {
        (0, 0)
    };

    // M6.C ships the passed-pawn term (rank bonus + EG path discriminator +
    // EG king-tropism) **wired but with all weights zeroed** in `eval::data`
    // — the score-neutral landing. The three-screen mixed-TC SPRT ladder vs
    // `M6.B` proved the literature defaults have a scale-invariant structural
    // mismatch with this engine (all-three Δ −21.74 / 60+0.6 KDIST collapse;
    // {RANK+PATH} fast-TC over-magnitude; {RANK+PATH}/2 plateau migrating the
    // failure — the M6.B `(ISO+CONN)/2` signature); the entire passed-pawn
    // weight set defers to the M6.F joint Texel reshape (ADR-0032 §7; see the
    // authoritative provenance block in `eval::data` + docs/milestones/m6.c.md).
    // Computed **live** from `pe.passed` (M6.B cache): king-distance/path
    // depend on non-pawn state, so the term is not pawn-only and is never
    // cached (ADR-0032 §3). It enters the blend numerator only — never
    // `static_eval_white()` nor the mop-up estimate (D7 / ADR-0032 §4). The
    // term math stays live at zero weight (M6.F-ready); the screen zeroed the
    // data constants, not this flag (the M6.B `PAWN_STRUCTURE_IN_EVAL`
    // precedent).
    const PASSED_PAWNS_IN_EVAL: bool = true;
    let (pp_mg, pp_eg) = if PASSED_PAWNS_IN_EVAL {
        pawns::passed_pawn_term_white(pos, &pe.passed)
    } else {
        (0, 0)
    };

    // M6.D piece mobility (N/B/R/Q attack-count ∩ mobility area, per-kind
    // MG/EG tables). Live literature defaults (Stockfish HCE) pending the
    // landing-gate SPRT vs `M6.C` (plan §10/§11). Computed live (not pawn-only
    // ⇒ never cached). Enters the blend numerator ONLY — never
    // `static_eval_white()` (the RFP/NMP/FFP pruning input) nor the mop-up
    // estimate (ADR-0032 §4; pinned by `static_accessor_excludes_mobility` +
    // `mop_up_addend_excludes_mobility_vs_m6c`). Term math stays live; the §11
    // contingency zeros per-kind `eval::data` arrays, not this flag (M6.B/C).
    const MOBILITY_IN_EVAL: bool = true;
    let (mob_mg, mob_eg) = if MOBILITY_IN_EVAL {
        mobility::mobility_term_white(pos)
    } else {
        (0, 0)
    };

    // M6.E king safety (zone attacker S-curve + castled pawn-shield +
    // MG-only open/semi-open-file penalty). **Shipped score-neutral** — every
    // king-safety weight zeroed in `eval::data` ⇒ term ≡ (0,0) ⇒ `evaluate`
    // byte-identical to `M6.D` (ADR-0033 §8; research §8 HIGH transfer risk;
    // the M6.C/M6.D inert-landing precedent). Computed live (not pawn-only ⇒
    // never cached — ADR-0033 §6, supersedes ADR-0032 §3). Enters the blend
    // numerator ONLY — never `static_eval_white()` (the RFP/NMP/FFP input) nor the
    // mop-up estimate (ADR-0032 §4; pinned by `static_accessor_excludes_king_
    // safety` + `mop_up_addend_excludes_king_safety_vs_m6d`). Term math stays
    // live at zero weight; M6.F joint Texel re-derives the whole set.
    const KING_SAFETY_IN_EVAL: bool = true;
    let (ks_mg, ks_eg) = if KING_SAFETY_IN_EVAL {
        king_safety::king_safety_term_white(pos)
    } else {
        (0, 0)
    };

    // M6.F Tier-1 HCE: outposts + rook-on-open/semi-open-file (additive (mg,eg))
    // + endgame draw-scale (multiplicative on the blended value). **Shipped
    // score-neutral** — all additive weights zeroed + scale ≡ identity
    // (EG_SCALE_DEN/EG_SCALE_DEN) ⇒ `evaluate` byte-identical to `M6.E`
    // (ADR-0034 §2; the M6.C/M6.D/M6.E inert-landing precedent). Computed live
    // (not pawn-only ⇒ never cached). Additive terms enter the blend numerator
    // ONLY; the scale multiplies the blended value BEFORE `+ mop_up` (mop-up is
    // the unscaled conversion nudge — ADR-0034 §5). Never `static_eval_white()`
    // (RFP/NMP/FFP input) nor the mop-up estimate (ADR-0032 §4; pinned by
    // `static_accessor_excludes_tier1` + `mop_up_addend_excludes_tier1_vs_m6e`
    // + `endgame_scale_excludes_static_accessor_vs_m6e`). Term math stays live;
    // M6.I joint Texel re-derives the whole set.
    const TIER1_IN_EVAL: bool = true;
    let ((op_mg, op_eg), (rf_mg, rf_eg), scale) = if TIER1_IN_EVAL {
        (
            tier1::outpost_term_white(pos),
            tier1::rook_file_term_white(pos),
            tier1::endgame_scale(pos),
        )
    } else {
        ((0, 0), (0, 0), data::EG_SCALE_DEN)
    };

    let blended = ((mg + bp_mg + pe_mg + pp_mg + mob_mg + ks_mg + op_mg + rf_mg) * phase
        + (eg + bp_eg + pe_eg + pp_eg + mob_eg + ks_eg + op_eg + rf_eg) * (24 - phase))
        / 24;
    let scaled = blended * scale / data::EG_SCALE_DEN;
    let result_white = scaled + mop_up;

    debug_assert!(
        result_white.abs() < MATE_IN_MAX_PLY - 1,
        "eval score {result_white} outside centipawn band"
    );

    if pos.side_to_move() == Color::White {
        result_white
    } else {
        -result_white
    }
}

/// Side-to-move-relative score in centipawns.
///
/// Returns 0 for insufficient-material positions (KvK, KvN, KvB,
/// KBvKB-same-color). Otherwise blends MG/EG by phase, applies bishop-pair,
/// pawn-structure, and mop-up addends, then negates for Black.
///
/// Pawn structure is recomputed from scratch on every call. For the hot search
/// path use [`evaluate_cached`].
pub fn evaluate(pos: &Position) -> i32 {
    evaluate_core(pos, pawns::pawn_eval(pos))
}

/// Hot-path eval: same result as [`evaluate`] but with pawn structure served
/// from the search-owned pawn hash. Pure accelerator — `evaluate_cached(p,
/// h) == evaluate(p)` for all `p` and any hash state (ADR-0032 §5).
///
/// `pub(crate)` rather than `pub` because `PawnHashTable` is crate-private
/// (search-owned, not public API) — a `pub fn` taking a `pub(crate)` type is
/// a privacy-leak lint error. No external consumer exists (only qsearch).
pub(crate) fn evaluate_cached(pos: &Position, ph: &mut pawns::PawnHashTable) -> i32 {
    evaluate_core(pos, ph.get(pos))
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
    // A — starting position phase is exactly 24.
    // -----------------------------------------------------------------------

    /// At the starting position all pieces are present:
    ///   4 knights × 1 + 4 bishops × 1 + 4 rooks × 2 + 2 queens × 4 = 24.
    /// Pawns and kings contribute 0. raw_phase must be exactly 24.
    #[test]
    fn eval_starting_position_phase_is_24() {
        let pos = Position::starting_position();
        assert_eq!(
            pos.raw_phase(),
            24,
            "starting position raw_phase must be 24 (4N×1 + 4B×1 + 4R×2 + 2Q×4 = 24)"
        );
    }

    // -----------------------------------------------------------------------
    // A — e-pawn missing → −67 cp (value preserved from M3.A).
    // -----------------------------------------------------------------------

    /// Starting position minus white's e2 pawn → −67 for white to move (pawn
    /// down). At starting raw_phase 24, mg_phase=24 and eg_phase=0, so the
    /// blend reduces to the MG-only value. Bishop pair is symmetric at the
    /// starting position (both sides have it) → net 0 addend. Therefore the
    /// blended eval at full material equals the MG-only eval from M3.A: −67.
    ///
    /// Derivation: e2 is sq=12 (LERF). White MG lookup is
    /// `+(MATERIAL[Pawn] + MG_PAWN_PST[12^56])` = `+(82 + MG_PAWN_PST[52])`.
    /// MG_PAWN_PST[52] is PeSTO row 6, col 4 (e-file) = −15 (from
    /// `docs/research/m3-eval-material-pst.md` "MG Pawn PST" row
    /// `−35, −1, −20, −23, −15, 24, 38, −22`). Total = +(82 + (−15)) = +67.
    /// Removed pawn means −67 white-perspective, and at phase 24 (full MG)
    /// the blend equals the MG score. Bishop pair cancels (both sides retain
    /// symmetric pairs). Net: −67.
    #[test]
    fn eval_one_e_pawn_missing_blends_to_minus_67_white_to_move() {
        // White is missing the e2 pawn (down one pawn). FEN built from
        // startpos by removing the e2 pawn from rank 2.
        let pos_white_pawn_missing =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1")
                .expect("FEN must parse");
        // Pins the fixture invariant the derivation relies on.  Pawns contribute
        // 0 to phase, so removing one pawn leaves raw_phase = 24 (full MG).
        assert_eq!(
            pos_white_pawn_missing.raw_phase(),
            24,
            "fixture invariant: raw_phase must be 24 (pawns have PHASE_DELTA=0)"
        );
        assert_eq!(
            evaluate(&pos_white_pawn_missing),
            -50,
            "white missing e2 pawn: evaluate must be -50 cp. \
            Through M6.F this was -67 (material+PST only: MG material 82 + \
            PST[e2^56=52]=-15 → net 67, pawn down). At the M6.I tuned ship the \
            now-live positional terms (mobility, etc.) net the missing-e2 \
            asymmetry to -50; sign still correct (white down a pawn)."
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
    // A — KBvKB same-color → 0; opposite-color → non-zero.
    // -----------------------------------------------------------------------

    /// White bishop on e2 (sq=12, light: file=4, rank=1, 4+1=5 odd → light).
    /// Black bishop on c4 (sq=26, light: file=2, rank=3, 2+3=5 odd → light).
    /// Both on light → same-color → insufficient material → 0.
    #[test]
    fn eval_kbvkb_same_color_returns_zero_light() {
        let pos = Position::from_fen("4k3/8/8/8/2b5/8/4B3/4K3 w - - 0 1")
            .expect("KBvKB same-color (light) FEN must parse");
        assert_eq!(
            evaluate(&pos),
            0,
            "KBvKB with both bishops on light squares must evaluate to 0 (insufficient material)"
        );
    }

    /// White bishop on a1 (sq=0, dark: file=0, rank=0, 0+0=0 even → dark).
    /// Black bishop on h8 (sq=63, dark: file=7, rank=7, 7+7=14 even → dark).
    /// Both on dark → same-color → insufficient material → 0.
    #[test]
    fn eval_kbvkb_same_color_returns_zero_dark() {
        // Kings on e1/e8; bishops on a1(dark) and h8(dark).
        // Rank 8 `4k2b`: e8=k, h8=b. Rank 1 `B3K3`: a1=B, e1=K.
        let pos = Position::from_fen("4k2b/8/8/8/8/8/8/B3K3 w - - 0 1")
            .expect("KBvKB same-color (dark) FEN must parse");
        assert_eq!(
            evaluate(&pos),
            0,
            "KBvKB with both bishops on dark squares must evaluate to 0 (insufficient material)"
        );
    }

    /// White bishop on e2 (light). Black bishop on c1 (sq=2, dark: file=2,
    /// rank=0, 2+0=2 even → dark). Opposite colors → NOT insufficient material
    /// → non-zero evaluation (material balance is 0 but PST imbalance is real).
    #[test]
    fn eval_kbvkb_opposite_color_does_not_return_zero() {
        // White Be2 (light), Black Bc1 (dark). Kings on e1/e8.
        let pos = Position::from_fen("4k3/8/8/8/8/8/4B3/2b1K3 w - - 0 1")
            .expect("KBvKB opposite-color FEN must parse");
        assert_ne!(
            evaluate(&pos),
            0,
            "KBvKB with bishops on opposite colors must NOT return 0 — \
            this is not an insufficient-material draw under FIDE"
        );
    }

    /// KBN vs K is a forced WIN (not a draw), and has total_count == 4 — the
    /// branch that calls `is_kbkb_same_color`. White K e1 + N b1 + B c1 (c1
    /// is dark: file=2, rank=0, sum=2 even), Black K e8. White has exactly
    /// one bishop; black has zero bishops.
    ///
    /// Mutation coverage for `is_kbkb_same_color`'s early-return guard
    /// `wb.count() != 1 || bb.count() != 1` (survivor at src/eval.rs:139
    /// under cargo-mutants `|| -> &&`). Original: `false || true` → returns
    /// false → NOT insufficient → evaluate != 0 (white up B+N). The `&&`
    /// mutant evaluates `false && true` → false → does not early-return,
    /// then `wb_on_light(false) == bb_on_light(false)` → returns true →
    /// evaluate would wrongly return 0 for a winning KBN-vs-K position.
    #[test]
    fn eval_kbn_vs_k_not_insufficient_material() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/1NB1K3 w - - 0 1")
            .expect("KBN vs K FEN must parse");
        assert_eq!(
            pos.occupied_all().count(),
            4,
            "fixture invariant: KBNvK must have exactly 4 pieces (reaches the \
             is_kbkb_same_color branch)"
        );
        assert_ne!(
            evaluate(&pos),
            0,
            "KBN vs K is a forced win, NOT an insufficient-material draw — \
             evaluate must be non-zero"
        );
    }

    // -----------------------------------------------------------------------
    // A — PSQT_MG anchor values.
    // -----------------------------------------------------------------------

    /// A2 is sq=8 (LERF). White MG lookup: +(MATERIAL[Pawn] + MG_PAWN_PST[8^56])
    /// = +(82 + MG_PAWN_PST[48]). PeSTO row 6 (rank-2 from White's perspective),
    /// col 0 (a-file) = -35. Total = +(82 + (-35)) = +47.
    #[test]
    fn psqt_mg_white_pawn_a2_anchor() {
        assert_eq!(
            PSQT_MG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            47,
            "PSQT_MG[White][Pawn][A2] must be 47 (82 material + (-35) MG PST)"
        );
    }

    /// A7 is sq=48. Black MG lookup: -(MATERIAL[Pawn] + MG_PAWN_PST[48])
    /// = -(82 + (-35)) = -47. Pins that Black uses `table[sq]` directly (no
    /// XOR), and that the value is the negation of the symmetric white A2 entry.
    #[test]
    fn psqt_mg_black_pawn_a7_anchor() {
        assert_eq!(
            PSQT_MG[Color::Black as usize][PieceKind::Pawn as usize][Square::A7.index() as usize],
            -47,
            "PSQT_MG[Black][Pawn][A7] must be -47 (negation of White A2 entry)"
        );
    }

    /// E1 is sq=4. White MG lookup: +(MATERIAL[King] + MG_KING_PST[4^56])
    /// = +(0 + MG_KING_PST[60]). PeSTO MG king PST row 7 (rank-1), col 4
    /// (e-file) = 8. Pins (a) king material is 0, (b) MG king PST is included.
    #[test]
    fn psqt_mg_white_king_e1_anchor() {
        assert_eq!(
            PSQT_MG[Color::White as usize][PieceKind::King as usize][Square::E1.index() as usize],
            8,
            "PSQT_MG[White][King][E1] must be 8 (0 material + 8 MG PST)"
        );
    }

    // -----------------------------------------------------------------------
    // A — PSQT_EG anchor values.
    // -----------------------------------------------------------------------

    /// A2 is sq=8 (LERF). White EG lookup: +(MATERIAL_EG[Pawn] + EG_PAWN_PST[8^56])
    /// = +(94 + EG_PAWN_PST[48]). PeSTO EG pawn PST row 6 (rank-2 from
    /// White's perspective), col 0 (a-file) = 13. Total = +(94 + 13) = +107.
    ///
    /// PeSTO EG pawn PST row 6 (a8-origin index 48..55):
    ///   `13, 8, 8, 10, 13, 0, 2, -7`; index 48 (col 0) = 13.
    #[test]
    fn psqt_eg_white_pawn_a2_anchor() {
        assert_eq!(
            PSQT_EG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            107,
            "PSQT_EG[White][Pawn][A2] must be 107 (94 EG material + 13 EG PST)"
        );
    }

    /// A7 is sq=48. Black EG lookup: -(MATERIAL_EG[Pawn] + EG_PAWN_PST[48])
    /// = -(94 + 13) = -107. Symmetric with white A2.
    #[test]
    fn psqt_eg_black_pawn_a7_anchor() {
        assert_eq!(
            PSQT_EG[Color::Black as usize][PieceKind::Pawn as usize][Square::A7.index() as usize],
            -107,
            "PSQT_EG[Black][Pawn][A7] must be -107 (negation of White EG A2 entry)"
        );
    }

    /// E1 is sq=4. White EG lookup: +(MATERIAL_EG[King] + EG_KING_PST[4^56])
    /// = +(0 + EG_KING_PST[60]). PeSTO EG king PST row 7 (rank-1), col 4
    /// (e-file) = -28. Total = +(0 + (-28)) = -28.
    ///
    /// PeSTO EG king PST row 7 (a8-origin index 56..63):
    ///   `-53, -34, -21, -11, -28, -14, -24, -43`; index 60 (col 4) = -28.
    ///
    /// The large negative EG king values in the back rank reflect the EG king
    /// PST's preference for central squares — the king should be active in
    /// the endgame, not tucked in a corner.
    #[test]
    fn psqt_eg_white_king_e1_anchor() {
        assert_eq!(
            PSQT_EG[Color::White as usize][PieceKind::King as usize][Square::E1.index() as usize],
            -28,
            "PSQT_EG[White][King][E1] must be -28 (0 EG material + (-28) EG king PST)"
        );
    }

    // -----------------------------------------------------------------------
    // B — PSQT_MG symmetry.
    // -----------------------------------------------------------------------

    /// For all (kind, sq), `PSQT_MG[White][kind][sq] == -PSQT_MG[Black][kind][sq ^ 56]`.
    #[test]
    fn psqt_mg_symmetry_at_indices() {
        assert_ne!(
            PSQT_MG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            0,
            "PSQT_MG must contain non-zero data — symmetry test would pass vacuously otherwise"
        );

        for kind in PieceKind::ALL {
            for sq in Square::all() {
                let sq_idx = sq.index() as usize;
                let sq_flip = (sq.index() ^ 56) as usize;
                let white_val = PSQT_MG[Color::White as usize][kind.index()][sq_idx];
                let black_val = PSQT_MG[Color::Black as usize][kind.index()][sq_flip];
                assert_eq!(
                    white_val, -black_val,
                    "PSQT_MG symmetry violated for {:?} at sq={sq}: \
                    PSQT_MG[White][{:?}][{sq_idx}]={white_val} != -PSQT_MG[Black][{:?}][{sq_flip}]={black_val}",
                    kind, kind, kind,
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // B — PSQT_EG symmetry.
    // -----------------------------------------------------------------------

    /// For all (kind, sq), `PSQT_EG[White][kind][sq] == -PSQT_EG[Black][kind][sq ^ 56]`.
    #[test]
    fn psqt_eg_symmetry_at_indices() {
        assert_ne!(
            PSQT_EG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize],
            0,
            "PSQT_EG must contain non-zero data — symmetry test would pass vacuously otherwise"
        );

        for kind in PieceKind::ALL {
            for sq in Square::all() {
                let sq_idx = sq.index() as usize;
                let sq_flip = (sq.index() ^ 56) as usize;
                let white_val = PSQT_EG[Color::White as usize][kind.index()][sq_idx];
                let black_val = PSQT_EG[Color::Black as usize][kind.index()][sq_flip];
                assert_eq!(
                    white_val, -black_val,
                    "PSQT_EG symmetry violated for {:?} at sq={sq}: \
                    PSQT_EG[White][{:?}][{sq_idx}]={white_val} != -PSQT_EG[Black][{:?}][{sq_flip}]={black_val}",
                    kind, kind, kind,
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // B — MG and EG tables are distinct (not copies of each other).
    // -----------------------------------------------------------------------

    /// A stub that sets PSQT_MG == PSQT_EG would pass all symmetry tests above.
    /// One spot-check that the pawn tables differ at a known entry closes the gap.
    #[test]
    fn psqt_mg_and_eg_pawn_tables_differ() {
        let mg_a2 =
            PSQT_MG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize];
        let eg_a2 =
            PSQT_EG[Color::White as usize][PieceKind::Pawn as usize][Square::A2.index() as usize];
        assert_ne!(
            mg_a2, eg_a2,
            "PSQT_MG[White][Pawn][A2] ({mg_a2}) and PSQT_EG[White][Pawn][A2] ({eg_a2}) \
            must differ — they come from distinct PeSTO tables (MG material 82, EG material 94)"
        );
    }

    // -----------------------------------------------------------------------
    // Bishop pair predicate tests.
    // -----------------------------------------------------------------------

    /// At the starting position both sides have bishop pairs (c1+f1 cover
    /// dark+light for white; c8+f8 cover dark+light for black). bishop_pair_term_white
    /// returns 0 when both sides have the pair (symmetric → no net bonus).
    #[test]
    fn bishop_pair_predicate_starting_position() {
        let pos = Position::starting_position();
        assert_eq!(
            bishop_pair_term_white(&pos, BISHOP_PAIR_MG),
            0,
            "bishop_pair_term_white must be 0 at starting position (both sides have the pair)"
        );
        assert_eq!(
            bishop_pair_term_white(&pos, BISHOP_PAIR_EG),
            0,
            "bishop_pair_term_white EG must be 0 at starting position"
        );
    }

    /// Position with only white having a bishop pair (remove one black bishop).
    /// White: c1(dark)+f1(light). Black: one bishop removed → no pair.
    /// bishop_pair_term_white returns +value.
    #[test]
    fn bishop_pair_predicate_only_white() {
        // White Ke1, Bc1, Bf1. Black Ke8, Bc8 only (f8 bishop removed).
        let pos = Position::from_fen("2b1k3/8/8/8/8/8/8/2B1KB2 w - - 0 1")
            .expect("bishop pair only white FEN must parse");
        assert_eq!(
            bishop_pair_term_white(&pos, BISHOP_PAIR_MG),
            BISHOP_PAIR_MG,
            "bishop_pair_term_white must be +BISHOP_PAIR_MG when only white has the pair"
        );
    }

    /// Position with only black having a bishop pair.
    /// bishop_pair_term_white returns -value.
    #[test]
    fn bishop_pair_predicate_only_black() {
        // White Ke1, Bc1 only (f1 bishop removed). Black Ke8, Bc8, Bf8.
        let pos = Position::from_fen("2b1kb2/8/8/8/8/8/8/2B1K3 w - - 0 1")
            .expect("bishop pair only black FEN must parse");
        assert_eq!(
            bishop_pair_term_white(&pos, BISHOP_PAIR_MG),
            -BISHOP_PAIR_MG,
            "bishop_pair_term_white must be -BISHOP_PAIR_MG when only black has the pair"
        );
    }

    /// Position where neither side has a bishop pair.
    /// bishop_pair_term_white returns 0.
    #[test]
    fn bishop_pair_predicate_neither() {
        // White Ke1, Bc1. Black Ke8, Bc8. Each side has exactly one bishop (dark).
        let pos = Position::from_fen("2b1k3/8/8/8/8/8/8/2B1K3 w - - 0 1")
            .expect("bishop pair neither FEN must parse");
        assert_eq!(
            bishop_pair_term_white(&pos, BISHOP_PAIR_MG),
            0,
            "bishop_pair_term_white must be 0 when neither side has the pair"
        );
    }

    // -----------------------------------------------------------------------
    // Mop-up term tests.
    // -----------------------------------------------------------------------

    /// KQK with white winning, black king cornered and white king close.
    /// White Kh1, Qh3, Black Kh3? No — need a position where black king is
    /// cornered and kings are close. Use: White Ka6 + Qa8, Black Ka8 →
    /// but kings can't be adjacent. Use White Kf6 + Qa1, Black Ka8:
    /// raw_phase=4 (queen only). BUT MOP_UP_PHASE_MAX=5, so phase<5 fires.
    ///
    /// Simpler: White Kg1 + Qa4, Black Ka8. White wins big, black king on corner.
    /// raw_phase = 4 (queen) < 5. advantage > 100. Mop-up fires and is positive.
    #[test]
    fn mop_up_bonus_kqk_white_winning_corner() {
        // White Kg1 (g1=sq6), Qa4 (a4=sq24), Black Ka8 (a8=sq56).
        // White to move (side irrelevant for mop-up direction, which depends on mg+eg sign).
        let pos = Position::from_fen("k7/8/8/8/Q7/8/8/6K1 w - - 0 1")
            .expect("KQK white winning FEN must parse");
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert!(
            bonus > 0,
            "mop_up_term_white must be positive when white has KQK advantage (bonus={bonus})"
        );
        assert!(
            bonus >= 14,
            "mop_up bonus must be at least 14 (minimum formula value); got {bonus}"
        );
        assert!(
            bonus <= 50,
            "mop_up bonus must be at most 50 (maximum formula value); got {bonus}"
        );
    }

    /// KQK with black winning, white king cornered.
    /// Black Ka8, Black Qa4, White Kg1 (cornered at g1).
    /// mop_up_term_white must be negative (black winning → penalty for white).
    #[test]
    fn mop_up_bonus_kqk_black_winning_corner() {
        // Black Ka8 (a8=sq56), Black Qa4 (a4=sq24), White Kg1 (g1=sq6, cornered).
        let pos = Position::from_fen("k7/8/8/8/q7/8/8/6K1 b - - 0 1")
            .expect("KQK black winning FEN must parse");
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert!(
            bonus < 0,
            "mop_up_term_white must be negative when black has KQK advantage (bonus={bonus})"
        );
        assert!(
            bonus >= -50,
            "mop_up penalty must not exceed -50 in magnitude; got {bonus}"
        );
        // Exact-arithmetic pin of the black-winning branch (mutation coverage
        // for the `4*CMD + 2*(14-cheb)` sub-expression — survivors at
        // src/eval.rs:210 under cargo-mutants `* -> +`, `* -> /`, `- -> /`).
        // White K g1 is the losing king: CMD(g1) = min(|6-3|,|6-4|) +
        // min(|0-3|,|0-4|) = 2 + 3 = 5. Black K a8; chebyshev(a8,g1) =
        // max(|0-6|,|7-0|) = 7. bonus = -(4*5 + 2*(14-7)) = -(20+14) = -34.
        assert_eq!(
            bonus, -34,
            "mop_up_term_white black-winning branch exact value: \
             -(4*CMD(g1)=5 + 2*(14 - cheb(a8,g1)=7)) = -34; got {bonus}"
        );
    }

    /// Phase gate: when raw_phase >= MOP_UP_PHASE_MAX, mop_up_term_white returns 0
    /// regardless of material advantage.
    #[test]
    fn mop_up_phase_gate() {
        // Four queens total (2 per side): raw_phase = 4 × 4 = 16 >= 5. Despite
        // large material imbalance, phase gate fires and mop_up returns 0.
        let pos = Position::from_fen("qk1q4/8/8/8/8/8/8/QK1Q4 w - - 0 1")
            .expect("middlegame FEN must parse");
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        assert!(
            pos.raw_phase() >= MOP_UP_PHASE_MAX,
            "fixture invariant: raw_phase={} must be >= MOP_UP_PHASE_MAX={}",
            pos.raw_phase(),
            MOP_UP_PHASE_MAX
        );
        assert_eq!(
            mop_up_term_white(&pos, mg, eg),
            0,
            "mop_up_term_white must return 0 when raw_phase >= MOP_UP_PHASE_MAX"
        );
    }

    /// Advantage gate: when |advantage| <= MOP_UP_MIN_ADVANTAGE_CP, mop_up returns 0.
    /// Construct a bare-KvK position (raw_phase=0, advantage≈0).
    #[test]
    fn mop_up_advantage_gate() {
        // KvK: raw_phase=0 < 5, but advantage=0 ≤ 100 → returns 0.
        let pos = Position::from_fen("8/8/8/4k3/8/8/8/4K3 w - - 0 1").expect("KvK FEN must parse");
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        assert_eq!(
            mop_up_term_white(&pos, mg, eg),
            0,
            "mop_up_term_white must return 0 when advantage <= MOP_UP_MIN_ADVANTAGE_CP"
        );
    }

    /// mop_up_term_white is positive when white wins and negative when black wins,
    /// even for the same structural position type (KRK).
    #[test]
    fn mop_up_direction_white_vs_black() {
        // White Ke1 + Ra1, Black Ke8 — white winning KRK.
        let white_wins = Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w - - 0 1")
            .expect("KRK white winning FEN must parse");
        let mg_w = white_wins.static_mg_white();
        let eg_w = white_wins.static_eg_white();
        let bonus_white = mop_up_term_white(&white_wins, mg_w, eg_w);

        // White Ke1, Black Ke8 + Ra8 — black winning KRK.
        let black_wins = Position::from_fen("r3k3/8/8/8/8/8/8/4K3 w - - 0 1")
            .expect("KRK black winning FEN must parse");
        let mg_b = black_wins.static_mg_white();
        let eg_b = black_wins.static_eg_white();
        let bonus_black = mop_up_term_white(&black_wins, mg_b, eg_b);

        assert!(
            bonus_white > 0,
            "mop_up_term_white must be positive in KRK-white-winning; got {bonus_white}"
        );
        assert!(
            bonus_black < 0,
            "mop_up_term_white must be negative in KRK-black-winning; got {bonus_black}"
        );
    }

    /// Pin Risk 4: in KQK with black winning, the mop-up term (which drives the
    /// losing white king to a corner) must dominate the EG king PST's positive
    /// central bias for the white king. Concretely:
    ///
    /// Variant A: white king on d4 (centralized, EG king PST gives a positive
    ///   bonus to `static_eg_white`), black king nearby with queen.
    /// Variant B: white king on a1 (cornered, EG king PST gives large negative).
    ///
    /// `evaluate(variant_B) < evaluate(variant_A)` from white's perspective
    /// means the cornered position is more losing for white — which is the
    /// correct ordering (the cornered king is closer to being mated).
    ///
    /// If this test fails, the mop-up scale is too small relative to the EG
    /// king PST gradient and needs revision before SPRT.
    #[test]
    fn mop_up_kqk_losing_king_centralized_worse_than_cornered() {
        // White to move so evaluate() == static_eval_white (no side-to-move flip).
        // Black queen on b7 (off the d-file/diagonals so the white king is NOT
        // in check in either variant — keeps these static-eval fixtures from
        // depicting an impossible side-to-move-in-check position), black king
        // on e6, white king on d4 (centralized).
        let variant_a = Position::from_fen("8/1q6/4k3/8/3K4/8/8/8 w - - 0 1")
            .expect("KQK variant A (white king centralized) FEN must parse");
        // Same black Qb7 + Ke6; white king on a1 (cornered).
        let variant_b = Position::from_fen("8/1q6/4k3/8/8/8/8/K7 w - - 0 1")
            .expect("KQK variant B (white king cornered) FEN must parse");

        // Sister assertion: pin that mop_up_term_white itself is more negative
        // for the cornered variant.  This is the load-bearing invariant: even if
        // evaluate() ordering happens to hold for other reasons, the mop-up term
        // must contribute a larger penalty when CMD(losing king) is larger.
        // CMD(a1)=6 > CMD(d4)=0, so mop_up_b must be strictly less than mop_up_a.
        let mop_up_a = mop_up_term_white(
            &variant_a,
            variant_a.static_mg_white(),
            variant_a.static_eg_white(),
        );
        let mop_up_b = mop_up_term_white(
            &variant_b,
            variant_b.static_mg_white(),
            variant_b.static_eg_white(),
        );
        assert!(
            mop_up_b < mop_up_a,
            "mop-up term must be more negative for the cornered white king (CMD=6) \
            than for the centralized white king (CMD=0): \
            mop_up(centralized)={mop_up_a}, mop_up(cornered)={mop_up_b}; \
            expected mop_up_cornered < mop_up_centralized"
        );

        let eval_a = evaluate(&variant_a);
        let eval_b = evaluate(&variant_b);

        assert!(
            eval_b < eval_a,
            "mop-up must make the cornered-white-king position more losing for white: \
            eval(centralized)={eval_a}, eval(cornered)={eval_b}; \
            expected eval_cornered < eval_centralized"
        );
    }

    // -----------------------------------------------------------------------
    // Distance helper tests.
    // -----------------------------------------------------------------------

    /// center_manhattan_distance: min(|f-3|,|f-4|) + min(|r-3|,|r-4|).
    /// Known values from definition: corners are 6, center squares are 0.
    #[test]
    fn center_manhattan_distance_known_squares() {
        // a1: file=0, rank=0. min(|0-3|,|0-4|)+min(|0-3|,|0-4|) = min(3,4)+min(3,4) = 3+3 = 6.
        assert_eq!(
            center_manhattan_distance(Square::A1),
            6,
            "CMD(a1) must be 6"
        );
        // d4: file=3, rank=3. min(|3-3|,|3-4|)+min(|3-3|,|3-4|) = min(0,1)+min(0,1) = 0+0 = 0.
        assert_eq!(
            center_manhattan_distance(Square::D4),
            0,
            "CMD(d4) must be 0"
        );
        // e4: file=4, rank=3. min(|4-3|,|4-4|)+min(|3-3|,|3-4|) = min(1,0)+min(0,1) = 0+0 = 0.
        assert_eq!(
            center_manhattan_distance(Square::E4),
            0,
            "CMD(e4) must be 0"
        );
        // h1: file=7, rank=0. min(|7-3|,|7-4|)+min(|0-3|,|0-4|) = min(4,3)+min(3,4) = 3+3 = 6.
        assert_eq!(
            center_manhattan_distance(Square::H1),
            6,
            "CMD(h1) must be 6"
        );
        // d5: file=3, rank=4. min(|3-3|,|3-4|)+min(|4-3|,|4-4|) = 0+0 = 0.
        assert_eq!(
            center_manhattan_distance(Square::D5),
            0,
            "CMD(d5) must be 0"
        );
        // a8: file=0, rank=7. min(3,4)+min(|7-3|,|7-4|) = 3+min(4,3) = 3+3 = 6.
        assert_eq!(
            center_manhattan_distance(Square::A8),
            6,
            "CMD(a8) must be 6"
        );
        // h8: file=7, rank=7. min(4,3)+min(4,3) = 3+3 = 6.
        assert_eq!(
            center_manhattan_distance(Square::H8),
            6,
            "CMD(h8) must be 6"
        );
    }

    /// chebyshev_distance: max(|fa-fb|, |ra-rb|).
    #[test]
    fn chebyshev_distance_known_pairs() {
        // a1 vs h8: max(|0-7|,|0-7|) = max(7,7) = 7.
        assert_eq!(
            chebyshev_distance(Square::A1, Square::H8),
            7,
            "chebyshev(a1, h8) must be 7"
        );
        // a1 vs b1: max(|0-1|,|0-0|) = max(1,0) = 1.
        assert_eq!(
            chebyshev_distance(Square::A1, Square::B1),
            1,
            "chebyshev(a1, b1) must be 1"
        );
        // a1 vs a8: max(|0-0|,|0-7|) = max(0,7) = 7.
        assert_eq!(
            chebyshev_distance(Square::A1, Square::A8),
            7,
            "chebyshev(a1, a8) must be 7"
        );
        // Pairs with BOTH ranks non-zero — mutation coverage for the rank
        // term (survivor at src/eval.rs:177 under `- -> +`). With a1 (rank 0)
        // in every pair above, `0 - x` and `0 + x` have equal abs, so the
        // sign of the rank subtraction is untestable. b2 vs c4: file diff
        // |1-2|=1, rank diff |1-3|=2 → max=2 (a `+` mutant gives |1+3|=4).
        assert_eq!(
            chebyshev_distance(Square::B2, Square::C4),
            2,
            "chebyshev(b2, c4) must be 2 (rank diff |1-3|=2 dominates file diff 1)"
        );
        // g7 vs b3: file diff |6-1|=5, rank diff |6-2|=4 → max=5 (a `+`
        // mutant gives rank |6+2|=8 → max would be 8).
        assert_eq!(
            chebyshev_distance(Square::G7, Square::B3),
            5,
            "chebyshev(g7, b3) must be 5 (file diff 5 dominates rank diff |6-2|=4)"
        );
    }

    // -----------------------------------------------------------------------
    // Phase clamp tests.
    // -----------------------------------------------------------------------

    /// Construct a position with raw_phase > 24 (promoted queen adds to phase).
    /// evaluate() must not produce an obviously wrong or panicking result — the
    /// blend's `min(raw_phase, 24)` clamp is the load-bearing guard.
    ///
    /// Position: starting material + extra white queen (simulating promotion
    /// having occurred). raw_phase = 4 (starting queen) + 4 (extra queen) = 8
    /// for the non-standard piece count; we'll use a position where only queens
    /// are on the board to keep it simple.
    ///
    /// Simpler: 3 white queens (2 promoted) on a8-origin board, raw_phase = 12.
    /// This is > 24 only with enough queens. Use 7 queens: 7×4=28 > 24.
    ///
    /// For a feasible test: we want raw_phase > 24. That needs 7+ queens.
    /// Use a simpler check: after a series of promotions on the same position
    /// via a known FEN with extra pieces. Actually test via direct accessor path.
    ///
    /// Simpler approach: test that static_eval_white() clamps correctly when
    /// raw_phase > 24 (see accessor_clamp test), and for promote-then-evaluate,
    /// use a FEN with 7 white queens (5 promoted) + kings only:
    /// raw_phase = 7×4 = 28 > 24. Result must be finite and < MATE threshold.
    #[test]
    fn eval_promoted_queen_position_phase_clamped() {
        // 7 white queens + white king + black king. raw_phase = 7×4 = 28 > 24.
        // The blend must not explode: `min(28,24)=24` → pure MG blend.
        let pos = Position::from_fen("QQQQ4/QQQ5/8/8/8/8/8/4K1k1 w - - 0 1")
            .expect("multi-queen FEN must parse");
        let result = evaluate(&pos);
        // The result must be a finite centipawn value, not a mate score.
        // MATE_IN_MAX_PLY - 1 ≈ 29935; any valid eval is well below.
        assert!(
            result.abs() < 30_000,
            "evaluate with raw_phase > 24 must produce a finite centipawn result; got {result}"
        );
        assert!(
            pos.raw_phase() > 24,
            "fixture invariant: raw_phase={} must be > 24 for this test",
            pos.raw_phase()
        );
    }

    /// Direct test of the Position::static_eval_white() accessor's clamp.
    /// Construct a position where raw_phase > 24, manually set the triple,
    /// then verify the blended accessor returns the clamped result.
    ///
    /// When raw_phase > 24, min(raw_phase,24) = 24, so (24−24)=0 → EG term
    /// drops out, and the result equals mg (before the /24 scaling):
    ///   accessor = (mg × 24 + eg × 0) / 24 = mg.
    #[test]
    fn static_eval_white_accessor_clamp_at_overflowed_phase() {
        // Use a multi-queen position where raw_phase > 24.
        let pos = Position::from_fen("QQQQ4/QQQ5/8/8/8/8/8/4K1k1 w - - 0 1")
            .expect("multi-queen FEN must parse");
        let raw = pos.raw_phase();
        assert!(raw > 24, "fixture invariant: raw_phase={raw} must be > 24");
        // At phase > 24, min(phase,24)=24 → eg_phase=0 → accessor == mg.
        let mg = pos.static_mg_white();
        let expected_blend = mg; // (mg*24 + eg*0) / 24 = mg
        assert_eq!(
            pos.static_eval_white(),
            expected_blend,
            "static_eval_white() with raw_phase={raw} > 24 must clamp: \
            expected mg={mg} (EG term drops out), got {}",
            pos.static_eval_white()
        );
    }

    // -----------------------------------------------------------------------
    // M6.B Slice D — eval integration (evaluate_cached purity, regressions).
    // -----------------------------------------------------------------------

    use crate::eval::pawns::PawnHashTable;
    use proptest::prelude::*;

    /// D6 proptest corpus (single source of truth — referenced by both the
    /// `evaluate_cached_equals_evaluate` proptest and the corpus-invariant
    /// guard below). Hoisted to module scope so a future FEN edit cannot
    /// silently leave the proptest passer-free without the guard test failing.
    const SEED_FENS: &[&str] = &[
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    ];

    /// Corpus-invariant guard (test-suite-review should-fix #1): the D6
    /// argument for the live passed-pawn term is structural (pure fn of
    /// `pos`+`pe.passed`, identical `evaluate_core` call site in both
    /// `evaluate` and `evaluate_cached`), so it does not *depend* on a
    /// passer being present — but the proptest should still exercise a
    /// non-empty passed set at a walked node for defense-in-depth. The plan's
    /// example claim that `SEED_FENS[2]` has a "b5 passer" is wrong (b5 is
    /// blocked by black c7 on the adjacent file); coverage actually comes
    /// from `SEED_FENS[3]`'s genuine white d7 passer. Pin it so a corpus
    /// edit that removes all passers fails loudly here.
    #[test]
    fn d6_proptest_corpus_exercises_a_passer() {
        let any_passer = SEED_FENS.iter().any(|fen| {
            let pos = Position::from_fen(fen).expect("seed FEN parses");
            let pe = crate::eval::pawns::pawn_eval(&pos);
            !pe.passed[Color::White.index()].is_empty()
                || !pe.passed[Color::Black.index()].is_empty()
        });
        assert!(
            any_passer,
            "the evaluate_cached_equals_evaluate proptest corpus must contain \
             at least one position with a passed pawn (SEED_FENS[3] = the \
             rnbq1k1r/pp1Pbppp/... white d7 passer); a corpus edit that \
             removed every passer would silently un-exercise the live \
             passed-pawn term in the D6 property"
        );
    }

    proptest! {
        /// Random-position × random-hash-state determinism (pins ADR-0032
        /// §5 / plan D6): `evaluate_cached(p, h) == evaluate(p)` for any `p`
        /// and any hash population. The hash is pre-warmed by probing along
        /// a short pseudo-random legal walk so the property exercises hit,
        /// miss, and collision-eviction states — not just a cold table.
        /// Stub: fails with `unimplemented!` until Slice D — correct
        /// against the contract, the test-first gate.
        #[test]
        fn evaluate_cached_equals_evaluate(seed in 0u64..u64::MAX) {
            // SEED_FENS is the module-scope const (single source of truth;
            // pinned non-empty-of-passers by `d6_proptest_corpus_exercises_a_passer`).
            let fen_idx = (seed as usize) % SEED_FENS.len();
            let mut pos = Position::from_fen(SEED_FENS[fen_idx]).expect("seed FEN");

            let mut ph = PawnHashTable::new();
            // Randomly decide whether to pre-warm or leave the hash cold.
            if seed & 1 == 0 {
                let _ = crate::eval::evaluate_cached(&pos, &mut ph);
            }

            // Walk a few plies, asserting equality at every node + clearing
            // the hash mid-walk on some seeds (covers cleared-then-reused).
            let mut state = seed;
            for ply in 0..4u32 {
                prop_assert_eq!(
                    crate::eval::evaluate_cached(&pos, &mut ph),
                    crate::eval::evaluate(&pos),
                    "evaluate_cached must equal evaluate at ply {}", ply
                );
                if (seed >> 3) & 1 == 1 && ply == 1 {
                    ph.clear();
                }
                let mut ml = crate::movegen::MoveList::new();
                crate::movegen::generate_moves(&pos, &mut ml);
                if ml.is_empty() {
                    break;
                }
                state = state.wrapping_add(0x9E3779B97F4A7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
                z ^= z >> 31;
                let mv = ml.as_slice()[(z as usize) % ml.len()];
                pos.make_move(mv);
            }
        }
    }

    /// `evaluate` is unchanged on a pawn-free position (regression guard
    /// against accidental non-pawn-core drift). Through M6.F this pinned the
    /// value to the M6.A baseline (525), on the premise that the only post-M6.A
    /// eval terms (pawn structure) cannot contribute on a pawn-free board, so
    /// `evaluate` must equal the M6.A non-pawn core exactly.
    ///
    /// That premise ENDED at the M6.I tuned ship: mobility and
    /// rook-(semi)open-file are non-pawn terms that DO fire on a pawn-free
    /// board, so the anchor moved 525 → 567. It remains a deterministic
    /// regression anchor for the shipped `evaluate` on this exact KRvK fixture.
    #[test]
    fn eval_pawn_free_krk_eval_anchor() {
        // White Ke1 + Ra1, Black Ke8 — KRK, white winning. Pawn-free.
        let pos =
            Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w - - 0 1").expect("KRK white winning FEN");
        // Deterministic value of evaluate() on this exact fixture at the M6.I
        // tuned ship (was 525 at M6.A–M6.F; mobility + rook-open-file now add on
        // the pawn-free board). A change here flags non-pawn-core eval drift.
        const KRK_EVAL_ANCHOR: i32 = 567;
        assert_eq!(
            evaluate(&pos),
            KRK_EVAL_ANCHOR,
            "pawn-free KRvK evaluate() regression anchor (M6.I tuned ship)"
        );
    }

    /// Startpos evaluates to 0 (symmetric pawn structure cancels in the
    /// white-minus-black pawn accumulation, on top of the already-symmetric
    /// material/PST). Distinct from `eval_starting_position_is_zero` above
    /// in that it is the post-M6.B contract: even WITH the pawn-structure
    /// term added, the perfectly symmetric start must still be exactly 0.
    #[test]
    fn eval_startpos_zero_with_pawn_structure() {
        let pos = Position::starting_position();
        assert_eq!(
            evaluate(&pos),
            0,
            "startpos must still be exactly 0 once pawn structure is added \
             (white and black pawn structure are mirror-identical)"
        );
    }

    /// Insufficient-material short-circuit still returns 0 even though the
    /// position (KvK) has no pawns and pawn structure would be (0,0) anyway
    /// — pins that the `is_insufficient_material` early return is preserved
    /// by the Slice-D refactor and not bypassed by a pawn-eval path.
    #[test]
    fn eval_insufficient_material_still_zero_post_m6b() {
        let pos = Position::from_fen("8/8/8/4k3/8/8/8/4K3 w - - 0 1").expect("KvK FEN");
        assert_eq!(
            evaluate(&pos),
            0,
            "KvK must still short-circuit to 0 after the M6.B refactor"
        );
    }

    /// Mop-up addend regression (pins §6 / ADR-0032 §4): pawn structure must
    /// NOT enter the mop-up advantage estimate. On a mop-up-eligible fixture
    /// that *also* has pawns, `mop_up_term_white(pos, static_mg_white,
    /// static_eg_white)` (PST-only inputs) must be byte-identical to its
    /// M6.A value. We assert it directly on a KQ-vs-K+pawn fixture by
    /// recomputing the term with the PST-only triple and comparing to the
    /// hand-derived M6.A formula value — independent of any pawn-eval code.
    ///
    /// Fixture: White Kg1 + Qa4, Black Ka8 + black pawn h7 (so the position
    /// has a pawn, exercising the "pawn structure must be excluded" path).
    /// raw_phase: queen 4 + pawn 0 = 4 < MOP_UP_PHASE_MAX(5) → mop-up eligible.
    /// The mop-up estimate uses static_mg_white/static_eg_white (PST-only,
    /// no pawn-structure addend). White winning (Q vs lone K + 1 pawn) →
    /// mop_up > 0, computed as 4·CMD(losing king a8)=4·6=24 + 2·(14 −
    /// cheb(g1,a8)=7) = 24 + 14 = 38.
    ///
    /// CMD(a8) = min(|0-3|,|0-4|) + min(|7-3|,|7-4|) = 3 + 3 = 6.
    /// cheb(g1,a8) = max(|6-0|,|0-7|) = 7.  bonus = 4·6 + 2·(14−7) = 38.
    #[test]
    fn mop_up_addend_excludes_pawn_structure_vs_m6a() {
        let pos = Position::from_fen("k7/7p/8/8/Q7/8/8/6K1 w - - 0 1")
            .expect("KQ vs K+pawn mop-up fixture");
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert_eq!(
            bonus, 38,
            "mop-up addend must equal its M6.A formula value \
             (4·CMD(a8)=24 + 2·(14−cheb(g1,a8)=7)=14 → 38); pawn structure \
             must be excluded from the mop-up advantage estimate"
        );
    }

    // -----------------------------------------------------------------------
    // M6.C — passed-pawn integration.
    //
    // These run against CURRENT behaviour: `passed_pawn_term_white` is NOT
    // yet wired into `evaluate_core` (Slice B does that). They are GREEN now
    // and must STAY green after the impl phase — they pin the no-passer
    // baseline, the insufficient-material short-circuit, the D6 cached==
    // uncached invariant (structurally, via the existing proptest), the
    // extreme-passers mate-band, and the D7 mop-up exclusion.
    // -----------------------------------------------------------------------

    /// Startpos has no passers (every pawn is blocked by an opposing pawn on
    /// its own or an adjacent file) and is colour-symmetric, so `evaluate`
    /// must still be exactly 0 once the passed-pawn term is added — the
    /// passed term must not perturb the no-passer baseline. Green now (term
    /// not yet wired) and green after (no passers ⇒ term is (0,0); symmetry
    /// otherwise unchanged).
    #[test]
    fn evaluate_startpos_zero_with_passed_term() {
        let pos = Position::starting_position();
        assert_eq!(
            evaluate(&pos),
            0,
            "startpos must still be exactly 0 with the passed-pawn term \
             (no passers, white/black structure mirror-identical)"
        );
    }

    /// The `is_insufficient_material` short-circuit precedes any passed-pawn
    /// computation (it returns before `evaluate_core`'s blend). KvK → 0,
    /// unaffected by the passed term in any phase.
    #[test]
    fn evaluate_insufficient_material_still_zero() {
        let pos = Position::from_fen("8/8/8/4k3/8/8/8/4K3 w - - 0 1").expect("KvK FEN");
        assert_eq!(
            evaluate(&pos),
            0,
            "KvK insufficient-material short-circuit precedes the passed-pawn term"
        );
    }

    /// Adversarial many-passer band check (pins the plan §5 derived
    /// worst-case bound). 8 white passers on rank 7 (rel-rank 6) with no
    /// black pawns: `evaluate` must not trip the `evaluate_core` mate-band
    /// `debug_assert!` and must land within `±(MATE_IN_MAX_PLY−1)`. Green
    /// now (term not wired) and green after: §5 bounds `|Σ pp_eg| ≤ 8·426 =
    /// 3408`, an order of magnitude below the band, stacked on a
    /// material/PST sum the M6.B core already keeps well inside it.
    #[test]
    fn passed_pawn_term_extreme_passers_in_band() {
        use crate::search::MATE_IN_MAX_PLY;
        let pos = Position::from_fen("4k3/PPPPPPPP/8/8/8/8/8/4K3 w - - 0 1")
            .expect("8 white passers, no black pawns");
        let score = evaluate(&pos);
        assert!(
            score.abs() < MATE_IN_MAX_PLY - 1,
            "8-passer extreme position must stay within the centipawn band; got {score}"
        );
    }

    /// Mop-up addend regression — pins D7 / ADR-0032 §4: the live
    /// passed-pawn term must NOT enter the mop-up advantage estimate. This
    /// reproduces the `mop_up_addend_excludes_pawn_structure_vs_m6a`
    /// pinned-literal discipline (the M6.B precedent at the top of this
    /// section) with the `_vs_m6b` suffix — M6.C's baseline is `M6.B`.
    ///
    /// Fixture: White Kg1 + Qa4 + Pb5, Black Ka8. The white b5 pawn is a
    /// passer (no black pawns) — so the position exercises the "the passed
    /// term must be excluded from mop-up" path. raw_phase = queen 4 + pawn 0
    /// = 4 < MOP_UP_PHASE_MAX(5) → mop-up eligible; White Q+P vs lone K →
    /// advantage ≫ 100, White winning → mop_up > 0.
    ///
    /// mop_up uses `mop_up_term_white(pos, static_mg_white, static_eg_white)`
    /// — PST-only inputs, so the live passed-pawn term provably cannot
    /// perturb the addend. Hand-derived longhand (king geometry identical to
    /// the M6.A precedent, so the literal is the same 38):
    ///   CMD(a8) = min(|0−3|,|0−4|) + min(|7−3|,|7−4|) = 3 + 3 = 6.
    ///   cheb(g1,a8) = max(|6−0|,|0−7|) = 7.
    ///   bonus = 4·CMD(a8) + 2·(14 − cheb(g1,a8)) = 4·6 + 2·(14−7)
    ///         = 24 + 14 = 38.
    #[test]
    fn mop_up_addend_excludes_passed_pawns_vs_m6b() {
        let pos = Position::from_fen("k7/8/8/1P6/Q7/8/8/6K1 w - - 0 1")
            .expect("KQ+P vs K mop-up fixture with a white passer");
        // Fixture invariant: the white b5 pawn is a passer (no black pawns) —
        // this is what makes the test exercise the passed-term exclusion.
        assert!(
            !crate::eval::pawns::pawn_eval(&pos).passed[Color::White.index()].is_empty(),
            "fixture invariant: white must have a passer (b5) present"
        );
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert_eq!(
            bonus, 38,
            "mop-up addend must equal its hand-derived formula value \
             (4·CMD(a8)=24 + 2·(14−cheb(g1,a8)=7)=14 → 38); the live \
             passed-pawn term must be excluded from the mop-up advantage \
             estimate (PST-only inputs)"
        );
    }

    // -----------------------------------------------------------------------
    // M6.D boundary regression tests (ADR-0032 §4).
    // -----------------------------------------------------------------------

    /// Regression pin: the `static_mg_white()` / `static_eg_white()` PST-only
    /// accessors used by RFP/NMP/FFP must NOT include the mobility term.
    ///
    /// On a midgame fixture with N/B/R/Q on the board the mobility term is
    /// non-zero. We assert that `pos.static_mg_white()` and `pos.static_eg_white()`
    /// equal `eval_state_from_scratch(&pos)`'s `(mg, eg)` — the PST-only triple —
    /// regardless of the mobility term. This is a regression pin, not a TDD-red
    /// test: the accessor is PST-only by construction and must remain so.
    ///
    /// TDD state: GREEN now (accessor is PST-only by construction) and stays
    /// green after the impl (mobility enters only the blend numerator, not the
    /// accessor — ADR-0032 §4). Weight-free.
    ///
    /// The fixture invariant is a weight-free *structural* presence check
    /// (mobile pieces are on the board): the non-zero/value discriminator is
    /// **M6.F-dormant** at the score-neutral zeroed mobility weights and
    /// auto-revalidates when M6.F re-introduces non-zero weights (the M6.C
    /// per-test-docstring convention).
    #[test]
    fn static_accessor_excludes_mobility() {
        // Midgame position with N/B/R/Q for both sides (mobility≠0 fixture).
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("midgame fixture must parse");
        // Weight-free structural fixture invariant: the fixture must contain
        // at least one mobile white piece (N/B/R/Q) — the boundary the test
        // pins. The non-zero-value discriminator is M6.F-dormant at the
        // score-neutral zeroed mobility weights (mirrors
        // `mop_up_addend_excludes_passed_pawns_vs_m6b`'s structural precedent).
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Knight).any()
                || pos.pieces_colored(Color::White, PieceKind::Bishop).any()
                || pos.pieces_colored(Color::White, PieceKind::Rook).any()
                || pos.pieces_colored(Color::White, PieceKind::Queen).any(),
            "fixture invariant: ≥1 mobile white piece (N/B/R/Q) must be present \
             (the boundary the test pins; value discriminator is M6.F-dormant \
             at the score-neutral zeroed weights)"
        );
        let (from_scratch_mg, from_scratch_eg, _) = eval_state_from_scratch(&pos);
        assert_eq!(
            pos.static_mg_white(),
            from_scratch_mg,
            "static_mg_white() must equal the PST-only from-scratch mg \
             (mobility must NOT enter this accessor)"
        );
        assert_eq!(
            pos.static_eg_white(),
            from_scratch_eg,
            "static_eg_white() must equal the PST-only from-scratch eg \
             (mobility must NOT enter this accessor)"
        );
    }

    /// Mop-up addend regression — pins ADR-0032 §4 for M6.D: the mobility
    /// term must NOT enter the mop-up advantage estimate. Mirrors
    /// `mop_up_addend_excludes_passed_pawns_vs_m6b` verbatim in shape.
    ///
    /// Fixture: White Kg1 + Qa4 (raw_phase = queen 4 < MOP_UP_PHASE_MAX 5 →
    /// mop-up eligible), Black Ka8. No pawns. KQ-vs-K.
    ///
    /// Fixture invariant assertion FIRST: a mobile white queen is structurally
    /// present (the boundary the test pins). The non-zero-value discriminator
    /// is **M6.F-dormant** at the score-neutral zeroed mobility weights and
    /// auto-revalidates when M6.F re-introduces non-zero weights (the M6.C
    /// per-test-docstring convention).
    ///
    /// mop_up uses PST-only `(mg, eg)` inputs. Hand-derived longhand:
    ///   White winning: drive black king to corner (black king on a8, white on g1).
    ///   CMD(a8) = min(|0−3|,|0−4|) + min(|7−3|,|7−4|) = 3 + 3 = 6.
    ///   cheb(g1,a8) = max(|6−0|,|0−7|) = 7.
    ///   bonus = 4·CMD(a8) + 2·(14 − cheb(g1,a8)) = 4·6 + 2·(14−7)
    ///         = 24 + 14 = 38.
    #[test]
    fn mop_up_addend_excludes_mobility_vs_m6c() {
        let pos =
            Position::from_fen("k7/8/8/8/Q7/8/8/6K1 w - - 0 1").expect("KQ vs K mop-up fixture");
        // Weight-free structural fixture invariant: a mobile white queen must
        // be present (the boundary the test pins; value discriminator is
        // M6.F-dormant at the score-neutral zeroed mobility weights). Mirrors
        // `mop_up_addend_excludes_passed_pawns_vs_m6b`'s structural precedent.
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Queen).any(),
            "fixture invariant: a mobile white queen must be present (the \
             boundary the test pins; value discriminator is M6.F-dormant at \
             the score-neutral zeroed weights)"
        );
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert_eq!(
            bonus, 38,
            "mop-up addend must equal its hand-derived formula value \
             (4·CMD(a8)=24 + 2·(14−cheb(g1,a8)=7)=14 → 38); the mobility \
             term must be excluded from the mop-up advantage estimate \
             (PST-only inputs)"
        );
    }

    // -----------------------------------------------------------------------
    // M6.E boundary regression tests (ADR-0033 §6, mirrors the M6.D pair).
    // -----------------------------------------------------------------------

    /// Regression pin: `static_mg_white()` / `static_eg_white()` PST-only
    /// accessors used by RFP/NMP/FFP must NOT include the king-safety term.
    ///
    /// Fixture: a position with a castled white king (g1, file ≥ 5) + ≥2
    /// enemy mobile pieces + an enemy queen, so the king-safety sub-components
    /// (S-curve, shield, open-file) are all exercised — the structural boundary
    /// the test pins. The non-zero-value discriminator (king-safety ≠ (0,0)) is
    /// **M6.F-dormant** at the score-neutral zeroed weights and auto-revalidates
    /// when M6.F sets live weights (the M6.D `static_accessor_excludes_mobility`
    /// precedent verbatim in shape; suffix `_mobility` → `_king_safety`).
    ///
    /// TDD state: GREEN now (accessor is PST-only by construction) and stays
    /// green after the impl (king safety enters only the blend numerator, not
    /// the accessor — ADR-0033 §6). Weight-free.
    #[test]
    fn static_accessor_excludes_king_safety() {
        // Midgame position: White Kg1 (castled-side) + mobile pieces; Black has
        // a queen + mobile pieces attacking the white king zone.
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        )
        .expect("midgame fixture must parse");
        // Weight-free structural fixture invariant: the fixture must contain
        // ≥1 mobile white piece (N/B/R/Q) and an enemy queen (the boundary
        // the test pins; value discriminator is M6.F-dormant at the score-
        // neutral zeroed king-safety weights).
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Knight).any()
                || pos.pieces_colored(Color::White, PieceKind::Bishop).any()
                || pos.pieces_colored(Color::White, PieceKind::Rook).any()
                || pos.pieces_colored(Color::White, PieceKind::Queen).any(),
            "fixture invariant: ≥1 mobile white piece (N/B/R/Q) must be present"
        );
        assert!(
            pos.pieces_colored(Color::Black, PieceKind::Queen).any(),
            "fixture invariant: enemy queen must be present (king-safety exercises \
             the S-curve path with the attacker gate)"
        );
        let (from_scratch_mg, from_scratch_eg, _) = eval_state_from_scratch(&pos);
        assert_eq!(
            pos.static_mg_white(),
            from_scratch_mg,
            "static_mg_white() must equal the PST-only from-scratch mg \
             (king safety must NOT enter this accessor)"
        );
        assert_eq!(
            pos.static_eg_white(),
            from_scratch_eg,
            "static_eg_white() must equal the PST-only from-scratch eg \
             (king safety must NOT enter this accessor)"
        );
    }

    /// Mop-up addend regression — pins ADR-0033 §6 for M6.E: the king-safety
    /// term must NOT enter the mop-up advantage estimate. Mirrors
    /// `mop_up_addend_excludes_mobility_vs_m6c` verbatim in shape; suffix
    /// `_vs_m6c` → `_vs_m6d` (M6.E's baseline is `M6.D`).
    ///
    /// Fixture: White Kh1 + Qa4, Black Ke8. raw_phase = queen 4 <
    /// MOP_UP_PHASE_MAX 5 → mop-up eligible. White winning hugely (queen up) →
    /// advantage ≫ 100. White Kh1 on file 7 (≥ 5 → castled-side gate passes
    /// for white king safety). White queen is 1 enemy mobile piece for black's
    /// king safety (gate fires at < 2 — S-curve = 0 from data; other
    /// sub-components also 0 at zeroed weights). The structural boundary:
    /// king-safety code runs but produces (0,0); mop-up must use PST-only
    /// inputs regardless.
    ///
    /// mop_up_term_white uses PST-only `(mg, eg)` inputs. Hand-derived longhand:
    ///   White winning: drive black king (e8) to corner.
    ///   CMD(e8) = min(|4−3|,|4−4|) + min(|7−3|,|7−4|) = 0 + 3 = 3.
    ///   cheb(h1, e8) = max(|7−4|, |0−7|) = max(3, 7) = 7.
    ///   bonus = 4·CMD(e8) + 2·(14 − cheb(h1,e8)) = 4·3 + 2·(14−7)
    ///         = 12 + 14 = 26.
    #[test]
    fn mop_up_addend_excludes_king_safety_vs_m6d() {
        // FEN: White Kh1 + Qa4, Black Ke8.
        // raw_phase = queen(4) = 4 < MOP_UP_PHASE_MAX(5) → mop-up eligible.
        // White king on h1 (file 7 ≥ 5 → castled-side gate passes for white
        // king safety). White queen = 1 enemy mobile piece for black king safety
        // (attacker gate fires at < 2 ⇒ S-curve = 0 from data). All other
        // sub-components 0 at zeroed weights ⇒ king_safety_term_white ≡ (0,0).
        let pos = Position::from_fen("4k3/8/8/8/Q7/8/8/7K w - - 0 1")
            .expect("KQ vs K mop-up fixture (White Kh1 + Qa4, Black Ke8)");
        // Weight-free structural fixture invariant: white queen present (exercises
        // king-safety code path); white king on castled-side file (h1, file 7 ≥ 5).
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Queen).any(),
            "fixture invariant: a mobile white queen must be present (exercises \
             king-safety code path for black king; value discriminator is \
             M6.F-dormant at the score-neutral zeroed weights)"
        );
        assert!(
            pos.king_square(Color::White).file() >= 5,
            "fixture invariant: white king must be on a castled-side file (≥ f, \
             exercises the white shield gate)"
        );
        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert_eq!(
            bonus, 26,
            "mop-up addend must equal its hand-derived formula value \
             (4·CMD(e8)=12 + 2·(14−cheb(h1,e8)=7)=14 → 26); the king-safety \
             term must be excluded from the mop-up advantage estimate \
             (PST-only inputs)"
        );
    }

    // -----------------------------------------------------------------------
    // M6.F boundary regression tests (ADR-0034 §5; mirrors M6.E pair verbatim
    // in shape; suffix `_vs_m6d` → `_vs_m6e`).
    // -----------------------------------------------------------------------

    /// Regression pin: `static_mg_white()` / `static_eg_white()` PST-only
    /// accessors used by RFP/NMP/FFP must NOT include the Tier-1 additive terms
    /// (outpost, rook-file) OR the endgame scale.
    ///
    /// Fixture invariant: an outpost-eligible knight + a rook on a genuinely
    /// open file (the two additive-term inputs exercised). Note: this fixture
    /// intentionally has a rook present, which means it is NOT an OCB-with-pawns
    /// position (OCB requires no rooks or queens). The OCB-scale-vs-accessor
    /// exclusion is pinned by `endgame_scale_excludes_static_accessor_vs_m6e`.
    ///
    /// At zeroed weights the non-zero-value discriminator is **M6.I-dormant**
    /// (both PST-only and term values are 0) — auto-revalidates when M6.I sets
    /// non-zero weights (the M6.E `static_accessor_excludes_king_safety`
    /// precedent verbatim in shape).
    ///
    /// TDD state: GREEN now (accessor is PST-only by construction; Tier-1 is
    /// not wired into `evaluate_core` yet and even when wired does not touch
    /// the accessor — ADR-0034 §5).
    #[test]
    fn static_accessor_excludes_tier1() {
        // Outpost-eligible: White Nd5 pawn-supported by c4+e4, unchallengeable
        // (no black c/e pawns). Open-rook-file: White Ra1 on a-file, no pawns
        // on a-file for either side.
        // Black Ka8, Black Bc7 (dark bishop present for the bishop fixture
        // invariant below — not OCB eligible since a rook is also present).
        // FEN: k7/2b5/8/3N4/2P1P3/8/4B1P1/R3K3 w - - 0 1
        //   rank8: k at a8. rank7: b at c7. rank5: N at d5. rank4: P at c4, P at e4.
        //   rank2: B at e2, P at g2. rank1: R at a1, K at e1.
        let pos = Position::from_fen("k7/2b5/8/3N4/2P1P3/8/4B1P1/R3K3 w - - 0 1")
            .expect("fixture FEN must parse");

        // Structural fixture invariants (weight-free):
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Knight).any(),
            "fixture invariant: white knight present (outpost-eligible)"
        );
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Rook).any(),
            "fixture invariant: white rook present (rook-file boundary)"
        );
        // Rook is present → cannot be OCB (OCB requires no rooks/queens).
        assert!(
            pos.pieces(PieceKind::Rook).any(),
            "fixture invariant: rook present confirms this is not OCB-eligible \
             (OCB-scale boundary is pinned by endgame_scale_excludes_static_accessor_vs_m6e)"
        );

        let (from_scratch_mg, from_scratch_eg, _) = eval_state_from_scratch(&pos);
        assert_eq!(
            pos.static_mg_white(),
            from_scratch_mg,
            "static_mg_white() must equal PST-only from-scratch mg \
             (Tier-1 additive terms AND scale must NOT enter this accessor)"
        );
        assert_eq!(
            pos.static_eg_white(),
            from_scratch_eg,
            "static_eg_white() must equal PST-only from-scratch eg \
             (Tier-1 additive terms AND scale must NOT enter this accessor)"
        );
    }

    /// Mop-up addend regression — pins ADR-0034 §5 for M6.F: neither the Tier-1
    /// additive terms nor the endgame scale must enter the mop-up advantage
    /// estimate. Mirrors `mop_up_addend_excludes_king_safety_vs_m6d` verbatim
    /// in shape; suffix `_vs_m6d` → `_vs_m6e` (M6.F's baseline is `M6.E`).
    ///
    /// Fixture: White Kh1 + Ra4 (rook on open a-file — exercises the Tier-1
    /// rook-file term), Black Ka8. raw_phase = rook(2) = 2 < MOP_UP_PHASE_MAX(5)
    /// → mop-up eligible. White winning (rook up) → mop_up > 0. Note: a queen
    /// alone would also give raw_phase < 5, but a queen does NOT trigger
    /// `rook_file_term_white` (only a rook does — should-fix #5 from review).
    ///
    /// Hand-derived from THIS fixture's king geometry (not the king_safety baseline):
    ///   White winning: drive black king (a8) to corner.
    ///   CMD(a8) = min(|0−3|,|0−4|) + min(|7−3|,|7−4|) = 3 + 3 = 6.
    ///   cheb(h1, a8) = max(|7−0|, |0−7|) = max(7, 7) = 7.
    ///   bonus = 4·CMD(a8) + 2·(14 − cheb(h1,a8)) = 4·6 + 2·(14−7)
    ///         = 24 + 14 = 38.
    ///
    /// TDD state: GREEN now (mop-up uses PST-only inputs by construction).
    #[test]
    fn mop_up_addend_excludes_tier1_vs_m6e() {
        // White Kh1; White Ra4 (rook on the a-file, no pawns anywhere → an
        // OPEN file, so this fixture genuinely exercises `rook_file_term_white`
        // — the M6.F Tier-1 boundary input this test pins out of the mop-up
        // estimate); Black Ka8. raw_phase = rook(2) = 2 < 5 → mop-up fires.
        // FEN: k7/8/8/8/R7/8/8/7K w - - 0 1
        let pos =
            Position::from_fen("k7/8/8/8/R7/8/8/7K w - - 0 1").expect("fixture FEN must parse");

        // Structural fixture invariants:
        assert!(
            pos.pieces_colored(Color::White, PieceKind::Rook).any(),
            "fixture invariant: white rook present (exercises `rook_file_term_white`; \
             value discriminator is M6.I-dormant at zeroed weights)"
        );
        assert!(
            pos.king_square(Color::White).file() >= 5,
            "fixture invariant: white king on castled-side file (h1, file 7 ≥ 5)"
        );
        // raw_phase check: rook(2) < MOP_UP_PHASE_MAX(5) → mop-up must fire.
        assert!(
            pos.raw_phase() < MOP_UP_PHASE_MAX,
            "fixture invariant: raw_phase={} must be < MOP_UP_PHASE_MAX={} \
             (mop-up must fire for this boundary regression to be non-trivial)",
            pos.raw_phase(),
            MOP_UP_PHASE_MAX
        );

        let mg = pos.static_mg_white();
        let eg = pos.static_eg_white();
        let bonus = mop_up_term_white(&pos, mg, eg);
        assert_eq!(
            bonus, 38,
            "mop-up addend must equal its hand-derived formula value \
             (4·CMD(a8)=24 + 2·(14−cheb(h1,a8)=7)=14 → 38); neither Tier-1 \
             additive terms nor the endgame scale must enter the mop-up estimate \
             (PST-only inputs)"
        );
    }

    /// Scale-specific boundary regression: the endgame scale multiplies only
    /// the blend numerator — it must NEVER enter the `static_eval_white()`
    /// RFP/NMP/FFP pruning input. This is the **first** M6.F-specific boundary
    /// test (a multiplicative construct is the first that *could* leak into the
    /// pruning eval — ADR-0034 §5 / plan §4).
    ///
    /// Fixture: a deliberately OCB-with-pawns position. `static_mg_white()` /
    /// `static_eg_white()` must equal the PST-only from-scratch `(mg, eg)` —
    /// the scale never touches the accessor.
    ///
    /// TDD state: GREEN now (accessor is PST-only by construction; even after
    /// wiring, `endgame_scale` applies only to the blend numerator in
    /// `evaluate_core` — ADR-0034 §5). Weight-free.
    #[test]
    fn endgame_scale_excludes_static_accessor_vs_m6e() {
        // OCB-with-pawns fixture: White Be2 (light) + pawn g2, Black Bc7 (dark).
        // Kings on e1 / e8. No queens, no rooks.
        let pos = Position::from_fen("4k3/2b5/8/8/8/8/4B1P1/4K3 w - - 0 1")
            .expect("fixture FEN must parse");

        // Structural fixture invariant: OCB-eligible (both sides have exactly
        // one bishop on opposite complexes; pawns present; no majors).
        let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
        let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
        assert!(
            wb.count() == 1 && bb.count() == 1,
            "fixture invariant: each side has exactly one bishop"
        );
        assert!(
            pos.pieces(PieceKind::Pawn).any(),
            "fixture invariant: at least one pawn present (OCB-with-pawns boundary)"
        );
        assert!(
            !pos.pieces(PieceKind::Queen).any() && !pos.pieces(PieceKind::Rook).any(),
            "fixture invariant: no queens/rooks (OCB-drawishness condition)"
        );

        let (from_scratch_mg, from_scratch_eg, _) = eval_state_from_scratch(&pos);
        assert_eq!(
            pos.static_mg_white(),
            from_scratch_mg,
            "static_mg_white() must equal PST-only from-scratch mg \
             (endgame scale must NOT enter the static accessor — ADR-0034 §5)"
        );
        assert_eq!(
            pos.static_eg_white(),
            from_scratch_eg,
            "static_eg_white() must equal PST-only from-scratch eg \
             (endgame scale must NOT enter the static accessor — ADR-0034 §5)"
        );
    }
}
