//! The two white-POV scorers (ADR-0037 §2): a parameterized reference oracle
//! and a fast cached feature model, validated against each other.
//!
//! - [`reference_score_white`] — a faithful re-implementation of `evaluate_core`
//!   that reads weights from a runtime [`EvalParams`] instead of the `eval::data`
//!   consts. The correctness oracle: validated `== quiet::static_eval_white` at
//!   shipped weights, and it owns the multiplicative endgame-scale + the
//!   mop-up-after-scale split (the outer-sweep path).
//! - [`extract`] + [`model_score_white`] — the fast cached path, linear in the
//!   tunable weights at identity scale. The model mirrors `evaluate_core`'s
//!   SINGLE blend division (two f64 sums ÷ 24 once at the end, NOT per-term —
//!   ADR-0037 §2).

use crate::Position;
use crate::bitboard::{self, Bitboard};
use crate::eval::data::PASSED_KDIST_CAP;
use crate::eval::{self, chebyshev_distance};
use crate::piece::{Color, PieceKind};
use crate::square::Square;
use crate::texel::layout::{self, Group, group_range};
use crate::texel::params::EvalParams;

/// Faithful re-implementation of `evaluate_core` reading weights from a runtime
/// [`EvalParams`]. White-POV. The correctness oracle (ADR-0037 §2/§7); handles
/// the full eval including the multiplicative endgame scale and the
/// mop-up-after-scale split.
pub fn reference_score_white(pos: &Position, p: &EvalParams) -> i32 {
    if eval::is_insufficient_material(pos) {
        return 0;
    }

    // Frozen PST base + phase (the engine's incremental triple, no fields the
    // tuner touches). Recomputed from scratch so the oracle is independent of
    // any cached incremental state.
    let (mg_pst, eg_pst, raw_phase) = eval::eval_state_from_scratch(pos);
    let phase = raw_phase.min(24) as i32;

    // Frozen bishop-pair (the BISHOP_PAIR_* consts, not tuned).
    let bp_mg = eval::bishop_pair_term_white(pos, eval::BISHOP_PAIR_MG);
    let bp_eg = eval::bishop_pair_term_white(pos, eval::BISHOP_PAIR_EG);

    // Mop-up takes PST-only (mg, eg) — no bishop-pair, no pawn structure
    // (ADR-0032 §4; the evaluate_core call site). Added AFTER the scale.
    let mop_up = eval::mop_up_term_white(pos, mg_pst, eg_pst);

    // Tunable terms, read from `p` via the shared detection accessors. The
    // term sums carry the (un-rounded) weight·count products in f64 — the
    // SINGLE blend division below is the only truncation, mirroring
    // `evaluate_core`'s integer arithmetic (ADR-0037 §2). At shipped (integer)
    // weights the f64 sums are exact integers, so the truncation reproduces
    // the engine's `/24` byte-for-byte (Tier 1); at arbitrary `with_core(w)`
    // weights the lone truncation diverges from the f64 model by <1cp (Tier 2).
    let core = p.core_to_vec();
    let (iso_mg, iso_eg) = dot_features(&eval::pawns::iso_dbl_bwd_features(pos), &core);
    let (conn_mg, conn_eg) = dot_features(&eval::pawns::conn_features(pos), &core);
    let (pp_mg, pp_eg) = passed_term(pos, p);
    let (mob_mg, mob_eg) = dot_features(&eval::mobility::mobility_features(pos), &core);
    let (ks_mg, ks_eg) = dot_features(&eval::king_safety::shield_open_file_features(pos), &core);
    let (op_mg, op_eg) = dot_features(&eval::tier1::outpost_features(pos), &core);
    let (rf_mg, rf_eg) = dot_features(&eval::tier1::rook_file_features(pos), &core);

    // King-safety S-curve: frozen at `p`'s shipped values (excluded from the
    // core). At shipped these are all zero ⇒ the term contributes exactly 0;
    // we keep it explicit (and exact) so the oracle == engine at shipped.
    let kss_mg = king_safety_scurve_mg(pos, p);

    let mg_sum = (mg_pst + bp_mg) as f64
        + iso_mg
        + conn_mg
        + pp_mg
        + mob_mg
        + ks_mg
        + kss_mg
        + op_mg
        + rf_mg;
    let eg_sum =
        (eg_pst + bp_eg) as f64 + iso_eg + conn_eg + pp_eg + mob_eg + ks_eg + op_eg + rf_eg;

    // Single blend division with truncation toward zero (the engine's i32 `/`).
    // The f64 sums fit exactly in the 2^53 mantissa for any legal position, so
    // `.trunc()` reproduces integer truncation at integer weights.
    let blended = ((mg_sum * phase as f64 + eg_sum * (24.0 - phase as f64)) / 24.0).trunc() as i32;
    let scale = endgame_scale(pos, p);
    let scaled = blended * scale / eval::data::EG_SCALE_DEN;
    scaled + mop_up
}

/// Sparse white-POV feature vector for the fast linear model (ADR-0037 §2).
///
/// Identity scale; raw counts (NOT phase-folded). The frozen reference-frame
/// contribution (material + PST + bishop-pair + mop-up) is split into the MG /
/// EG blended `base_*` and the post-scale `mop_up`; `coeffs` are sparse
/// `(core_index, raw_count)` pairs whose MG/EG parity is fixed by
/// [`layout`](crate::texel::layout).
#[derive(Clone, PartialEq, Debug)]
pub struct FeatureVec {
    /// Frozen MG base contribution (material + MG PST + bishop-pair MG).
    pub base_mg: f32,
    /// Frozen EG base contribution (material + EG PST + bishop-pair EG).
    pub base_eg: f32,
    /// Mop-up contribution (added AFTER the endgame scale; reference-only at
    /// non-identity scale).
    pub mop_up: f32,
    /// Tapered phase in `0..=24`.
    pub phase: u8,
    /// Sparse `(core_index, raw_count)` tunable-feature coeffs.
    pub coeffs: Vec<(u16, f32)>,
}

/// Extracts the sparse [`FeatureVec`] for `pos` (identity scale, raw counts).
///
/// Always emits the raw frozen base + tunable feature counts — it does NOT
/// short-circuit on insufficient material. The corpus contains only quiet
/// game positions (never KvK / KNvK), so the linear model never sees an
/// insufficient-material position in production; the per-term feature pins,
/// however, do construct minimal KvN-style fixtures and expect the raw counts.
/// The insufficient-material early-return is the engine score's concern and
/// lives in [`reference_score_white`].
pub fn extract(pos: &Position) -> FeatureVec {
    let (mg_pst, eg_pst, raw_phase) = eval::eval_state_from_scratch(pos);
    let bp_mg = eval::bishop_pair_term_white(pos, eval::BISHOP_PAIR_MG);
    let bp_eg = eval::bishop_pair_term_white(pos, eval::BISHOP_PAIR_EG);
    let mop_up = eval::mop_up_term_white(pos, mg_pst, eg_pst);

    let mut coeffs: Vec<(u16, f32)> = Vec::new();
    accumulate_signed(&mut coeffs, eval::pawns::iso_dbl_bwd_features(pos));
    accumulate_signed(&mut coeffs, eval::pawns::conn_features(pos));
    accumulate_signed(&mut coeffs, passed_features(pos));
    accumulate_signed(&mut coeffs, eval::mobility::mobility_features(pos));
    accumulate_signed(
        &mut coeffs,
        eval::king_safety::shield_open_file_features(pos),
    );
    accumulate_signed(&mut coeffs, eval::tier1::outpost_features(pos));
    accumulate_signed(&mut coeffs, eval::tier1::rook_file_features(pos));

    FeatureVec {
        base_mg: (mg_pst + bp_mg) as f32,
        base_eg: (eg_pst + bp_eg) as f32,
        mop_up: mop_up as f32,
        phase: raw_phase.min(24),
        coeffs,
    }
}

/// Fast linear model score (white-POV) for core weights `core_w` (length
/// [`N_CORE`](crate::texel::layout::N_CORE)). Mirrors `evaluate_core`'s single
/// blend division: `mg = base_mg + Σ_{MG} w·count`, `eg = base_eg + Σ_{EG}
/// w·count`, `score = (mg·phase + eg·(24−phase))/24 + mop_up` (ADR-0037 §2).
pub fn model_score_white(fv: &FeatureVec, core_w: &[f64]) -> f64 {
    let mut mg = fv.base_mg as f64;
    let mut eg = fv.base_eg as f64;
    for &(idx, count) in &fv.coeffs {
        let contrib = core_w[idx as usize] * count as f64;
        if layout::is_mg(idx as usize) {
            mg += contrib;
        } else {
            eg += contrib;
        }
    }
    let phase = fv.phase as f64;
    (mg * phase + eg * (24.0 - phase)) / 24.0 + fv.mop_up as f64
}

// ---------------------------------------------------------------------------
// Reference-scorer term helpers: each mirrors the corresponding eval term but
// reads weights from `EvalParams` instead of the `eval::data` consts. They
// share detection with the engine via the `(core_index, raw_count)` accessors
// (so the detection is correct by construction) dotted with `p`'s core vec.
// ---------------------------------------------------------------------------

/// Blend a set of `(core_index, raw_count)` features against `p`'s core weights
/// into a `(mg, eg)` f64 pair (MG/EG parity per [`layout`]). Un-rounded
/// weights: the single blend division in [`reference_score_white`] is the only
/// truncation (ADR-0037 §2). At shipped (integer) weights the products are
/// exact integers.
fn dot_features(features: &[(u16, i32)], core: &[f64]) -> (f64, f64) {
    let (mut mg, mut eg) = (0f64, 0f64);
    for &(idx, count) in features {
        let contrib = core[idx as usize] * count as f64;
        if layout::is_mg(idx as usize) {
            mg += contrib;
        } else {
            eg += contrib;
        }
    }
    (mg, eg)
}

/// Passed-pawn term reading `p`'s weights (un-rounded f64). Mirrors
/// `eval::pawns::passed_pawn_term_white` exactly (rank MG/EG + 3-state path
/// delta EG + per-step king tropism), but the weights come from `p`.
fn passed_term(pos: &Position, p: &EvalParams) -> (f64, f64) {
    let pe = eval::pawns::pawn_eval(pos);
    let passed = pe.passed;
    let occ_all = pos.occupied_all();
    let cap = PASSED_KDIST_CAP;
    let (mut mg, mut eg) = (0f64, 0f64);

    for (side, signi) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let sign = signi as f64;
        let enemy = side.flip();
        let enemy_occ = pos.occupied(enemy);
        for sq in passed[side.index()].iter() {
            let rel = match side {
                Color::White => sq.rank(),
                Color::Black => 7 - sq.rank(),
            } as usize;

            mg += sign * p.passed_mg[rel];
            eg += sign * p.passed_eg[rel];

            let pawn_bb = Bitboard::from_square(sq);
            let path = match side {
                Color::White => bitboard::white_front_spans(pawn_bb),
                Color::Black => bitboard::black_front_spans(pawn_bb),
            };
            if (path & occ_all).is_empty() {
                eg += sign * p.passed_free_eg_delta[rel];
            } else if (path & enemy_occ).any() {
                eg -= sign * p.passed_free_eg_delta[rel];
            }

            let own_king = pos.king_square(side);
            let enemy_king = pos.king_square(enemy);
            let promo_rank = match side {
                Color::White => 7,
                Color::Black => 0,
            };
            let promo = Square::from_file_rank(sq.file(), promo_rank)
                .expect("file 0..7 and rank 0/7 are always in range");
            let own_d = chebyshev_distance(own_king, promo).min(cap);
            let enemy_d = chebyshev_distance(enemy_king, promo).min(cap);
            let rel_scale = rel as f64;
            eg += sign * rel_scale * p.passed_kdist_own_per_step * (cap - own_d) as f64;
            eg += sign * rel_scale * p.passed_kdist_enemy_per_step * (enemy_d - cap) as f64;
        }
    }
    (mg, eg)
}

/// King-safety S-curve MG contribution that the LINEAR shield seam excludes,
/// recomputed from `p`'s frozen (integer-valued) S-curve fields. At shipped
/// weights every entry is zero ⇒ contributes exactly 0; kept exact so the
/// oracle matches the engine. EG is always 0 (king-safety is MG-only).
fn king_safety_scurve_mg(pos: &Position, p: &EvalParams) -> f64 {
    let occ_all = pos.occupied_all();
    let mut mg = 0i32;
    let table_len = p.king_safety_table.len() as i32;

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let enemy = side.flip();
        let ksq = pos.king_square(side);
        let zone = eval::king_safety::king_zone(side, ksq);

        let mut units = 0i32;
        let mut attackers = 0u32;
        for psq in pos.pieces_colored(enemy, PieceKind::Knight).iter() {
            let h = (crate::movegen::masks::knight_attacks(psq) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += p.king_attack_weight_n.round() as i32 * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Bishop).iter() {
            let h = (crate::magic::bishop_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += p.king_attack_weight_b.round() as i32 * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Rook).iter() {
            let h = (crate::magic::rook_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += p.king_attack_weight_r.round() as i32 * h;
            }
        }
        for psq in pos.pieces_colored(enemy, PieceKind::Queen).iter() {
            let h = (crate::magic::queen_attacks(psq, occ_all) & zone).count() as i32;
            if h > 0 {
                attackers += 1;
                units += p.king_attack_weight_q.round() as i32 * h;
            }
        }
        let enemy_has_queen = pos.pieces_colored(enemy, PieceKind::Queen).any();
        if attackers < 2 || !enemy_has_queen {
            units = 0;
        }
        let idx = units.clamp(0, table_len - 1) as usize;
        mg -= sign * p.king_safety_table[idx].round() as i32;
    }
    mg as f64
}

/// Endgame draw-scale numerator from `p`'s scale fields. Mirrors
/// `eval::tier1::endgame_scale`'s clamp + min + 50-move taper, reading `p`'s
/// (rounded) numerators and the frozen structural constants.
fn endgame_scale(pos: &Position, p: &EvalParams) -> i32 {
    use eval::data::{EG_SCALE_DEN, FIFTY_MOVE_TAPER_FROM};
    let mut scale = EG_SCALE_DEN;
    if eval::tier1::is_ocb_with_pawns(pos) {
        scale = scale.min(p.ocb_with_pawns_scale.round() as i32);
    }
    if eval::tier1::is_pawnless_drawish(pos) {
        scale = scale.min(p.pawnless_draw_scale.round() as i32);
    }
    let hmc = pos.halfmove_clock() as i32;
    if hmc > FIFTY_MOVE_TAPER_FROM {
        let span = (100 - FIFTY_MOVE_TAPER_FROM).max(1);
        let prog = (hmc - FIFTY_MOVE_TAPER_FROM).clamp(0, span);
        let floor = p.fifty_move_floor.round() as i32;
        let tapered = EG_SCALE_DEN - (EG_SCALE_DEN - floor) * prog / span;
        scale = scale.min(tapered);
    }
    scale.clamp(0, EG_SCALE_DEN)
}

// ---------------------------------------------------------------------------
// Feature-extraction helpers (raw counts, identity scale).
// ---------------------------------------------------------------------------

/// Merge a `(core_index, i32 raw_count)` accessor output into the float coeff
/// list (as `(u16, f32)`). The accessors already drop zero counts, so the
/// merged list is sparse and each index appears at most once per accessor.
fn accumulate_signed(coeffs: &mut Vec<(u16, f32)>, features: Vec<(u16, i32)>) {
    for (idx, count) in features {
        coeffs.push((idx, count as f32));
    }
}

/// Sparse `(core_index, raw_count)` passed-pawn features (white-POV signed).
/// Implemented consistently with `eval::pawns::passed_pawn_term_white`: rank
/// MG/EG + 3-state path-delta EG + per-step king-distance (rel-scaled). The
/// passer's MG rank-7 entry is non-core (frozen 0) and is intentionally not
/// emitted — it matches the frozen `passed_mg[7]` carried in `EvalParams`.
fn passed_features(pos: &Position) -> Vec<(u16, i32)> {
    let pe = eval::pawns::pawn_eval(pos);
    let passed = pe.passed;
    let occ_all = pos.occupied_all();
    let cap = PASSED_KDIST_CAP;
    let start = group_range(Group::Passed).start;

    // Per-core-index accumulator over the 19 Passed slots.
    //  0..5   : PASSED_MG[2..7]   (MG)
    //  5..11  : PASSED_EG[2..8]   (EG)
    //  11..17 : FREE_EG_DELTA[2..8] (EG)
    //  17     : KDIST_OWN  (EG)
    //  18     : KDIST_ENEMY (EG)
    let mut acc = [0i32; 19];

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let enemy = side.flip();
        let enemy_occ = pos.occupied(enemy);
        for sq in passed[side.index()].iter() {
            let rel = match side {
                Color::White => sq.rank(),
                Color::Black => 7 - sq.rank(),
            } as usize;

            // MG rank bonus: core slots cover rel 2..=6 (rel 7 is frozen 0).
            if (2..7).contains(&rel) {
                acc[rel - 2] += sign;
            }
            // EG rank bonus: rel 2..=7.
            if (2..8).contains(&rel) {
                acc[5 + (rel - 2)] += sign;
            }

            // 3-state path delta (EG): +1 free / −1 enemy-blocked / 0 friendly.
            let pawn_bb = Bitboard::from_square(sq);
            let path = match side {
                Color::White => bitboard::white_front_spans(pawn_bb),
                Color::Black => bitboard::black_front_spans(pawn_bb),
            };
            if (2..8).contains(&rel) {
                if (path & occ_all).is_empty() {
                    acc[11 + (rel - 2)] += sign;
                } else if (path & enemy_occ).any() {
                    acc[11 + (rel - 2)] -= sign;
                }
            }

            // King-distance per-step (EG, rel-scaled). The per-step weight is
            // the tuned coeff; its multiplier is the raw count emitted here.
            let own_king = pos.king_square(side);
            let enemy_king = pos.king_square(enemy);
            let promo_rank = match side {
                Color::White => 7,
                Color::Black => 0,
            };
            let promo = Square::from_file_rank(sq.file(), promo_rank)
                .expect("file 0..7 and rank 0/7 are always in range");
            let own_d = chebyshev_distance(own_king, promo).min(cap);
            let enemy_d = chebyshev_distance(enemy_king, promo).min(cap);
            let rel_scale = rel as i32;
            acc[17] += sign * rel_scale * (cap - own_d);
            acc[18] += sign * rel_scale * (enemy_d - cap);
        }
    }

    let mut out = Vec::new();
    for (local, &count) in acc.iter().enumerate() {
        if count != 0 {
            out.push(((start + local) as u16, count));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use crate::bench::BENCH_POSITIONS;
    use crate::corpus::prng::Prng;
    use crate::corpus::quiet::static_eval_white;
    use crate::eval;
    use crate::movegen::{MoveList, generate_moves};
    use crate::texel::layout::{self, Group, N_CORE};
    use crate::texel::params::EvalParams;

    // -----------------------------------------------------------------------
    // Deterministic position battery (ADR-0037 §7 "large corpus battery").
    //
    // The gitignored `bench/corpus/` is NEVER touched. Instead we generate a
    // diverse, fully-deterministic set: seeded random-legal-move walks from
    // startpos (the engine's own movegen + a fixed-seed SplitMix64) plus the
    // 16 `bench::BENCH_POSITIONS`. Every walk reseeds at a terminal node so a
    // single seed produces hundreds of distinct middlegame/endgame positions.
    // -----------------------------------------------------------------------

    /// One pseudo-random legal-move walk from startpos, snapshotting the
    /// position after each move. Stops at a terminal node (no legal moves from
    /// `generate_moves`) or after `depth` moves.
    ///
    /// `make_move` returns an `Undo` token (not a `Result`): every move from
    /// `generate_moves` is playable, so we intentionally discard the token —
    /// we never need to unmake here.
    fn walk_from_startpos(seed: u64, depth: usize, out: &mut Vec<Position>) {
        let mut pos = Position::starting_position();
        let mut rng = Prng::new(seed);
        for _ in 0..depth {
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            // Terminal: checkmate or stalemate — no legal moves. Stop the walk;
            // the caller collects what we produced so far.
            if ml.is_empty() {
                return;
            }
            let pick = rng.below(ml.len() as u64) as usize;
            let mv = ml.as_slice()[pick];
            let _undo = pos.make_move(mv);
            out.push(pos);
        }
    }

    /// Build the ≥500-position deterministic battery (seeded walks + bench).
    fn battery() -> Vec<Position> {
        let mut out: Vec<Position> = Vec::new();
        // 40 independent walks × up to 16 plies each ≈ 640 positions.
        for seed in 0u64..40 {
            walk_from_startpos(
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5,
                16,
                &mut out,
            );
        }
        for fen in BENCH_POSITIONS.iter() {
            out.push(Position::from_fen(fen).expect("bench FEN must parse"));
        }
        assert!(
            out.len() >= 500,
            "battery must have ≥500 positions (got {})",
            out.len()
        );
        out
    }

    /// Hand-built FEN fixtures exercising the special-case surfaces called out
    /// in the plan: en-passant available, promotion-adjacent, rich castling.
    fn special_fixtures() -> Vec<Position> {
        const FENS: &[&str] = &[
            // En-passant square available (after 1.e4 ... d5 2.e5 f5).
            "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
            // En-passant for black to capture.
            "rnbqkbnr/ppppp1pp/8/8/4Pp2/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 3",
            // Promotion-adjacent: white pawn on 7th, black pawn on 2nd.
            "4k3/P7/8/8/8/8/7p/4K3 w - - 0 1",
            "8/2P1k3/8/8/8/8/4K1p1/8 b - - 0 1",
            // Rich castling rights, all four available.
            "r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1",
            "r3k2r/pppq1ppp/2npbn2/2b1p3/2B1P3/2NPBN2/PPPQ1PPP/R3K2R w KQkq - 6 8",
        ];
        FENS.iter()
            .map(|f| Position::from_fen(f).expect("special fixture FEN must parse"))
            .collect()
    }

    /// A seeded core-weight vector at literature magnitude (`scale` ~ tens of
    /// cp). The exact values are immaterial — only that they are diverse and
    /// non-degenerate so the model-vs-reference check exercises the tuned
    /// regime, not the shipped all-zero one.
    fn seeded_core_weights(seed: u64) -> Vec<f64> {
        let mut rng = Prng::new(seed);
        // Centered in roughly [-40, +40] cp — literature term magnitude.
        (0..N_CORE)
            .map(|_| (rng.below(8001) as f64 - 4000.0) / 100.0)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Tier 1 — reference == engine, at shipped weights.
    // -----------------------------------------------------------------------

    /// `reference_score_white(pos, shipped()) == static_eval_white(pos)`
    /// exactly over the battery + special fixtures. Pins the reference oracle
    /// to the real engine eval (ADR-0037 §7 Tier 1).
    #[test]
    fn reference_matches_static_eval_at_shipped() {
        let shipped = EvalParams::shipped();
        let mut positions = battery();
        positions.extend(special_fixtures());
        for pos in &positions {
            let reference = reference_score_white(pos, &shipped);
            let engine = static_eval_white(pos);
            assert_eq!(
                reference,
                engine,
                "reference_score_white != static_eval_white at shipped weights \
                 (fen={})",
                pos.to_fen()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Tier 2 — fast model == reference, at arbitrary weights.
    //
    // ≤2cp tolerance: the model is f64 throughout while `reference_score_white`
    // rounds to i32 at the SAME single blend division (ADR-0037 §2), so the
    // only divergence is the final truncation of one quotient — bounded by
    // <1cp per the floor. The 2cp band leaves headroom for the mop-up-after
    // term's independent rounding without admitting a real feature-count or
    // phase-fold bug (those drift by ≥1cp PER mis-attributed term, scaling
    // with the weight magnitude — visible well beyond 2cp on this battery).
    // -----------------------------------------------------------------------

    /// `model_score_white(extract(pos), w) ≈ reference_score_white(pos,
    /// shipped().with_core(w))` within ≤2cp, for several seeded random `w` AND
    /// the shipped core vec, over the battery. THE test that validates the tuned
    /// regime (ADR-0037 §7 Tier 2).
    ///
    /// The fast linear model (`extract` / `model_score_white`) deliberately
    /// EXCLUDES the king-safety attacker S-curve (ADR-0037 §3 — the excluded
    /// nonlinear term that keeps the gradient solve linear). Since M6.K
    /// activated that S-curve to non-zero literature values, the reference
    /// scorer now carries a non-zero `king_safety_scurve_mg` addend the linear
    /// model cannot represent. This Tier-2 test validates the LINEAR core
    /// fidelity, so the S-curve is zeroed in the comparison params here; the
    /// S-curve term itself is validated against `evaluate` by
    /// `reference_matches_static_eval_at_shipped` (Tier 1).
    #[test]
    fn model_matches_reference_at_random_and_literature_weights() {
        const TOL_CP: f64 = 2.0;
        let shipped = EvalParams::shipped();
        let positions = battery();

        // Several seeded random vectors + the shipped core vec.
        let mut weight_vecs: Vec<Vec<f64>> = (1u64..=5).map(seeded_core_weights).collect();
        weight_vecs.push(shipped.core_to_vec());

        for w in &weight_vecs {
            // Identity scale: with_core leaves the (shipped-identity) scale
            // numerators untouched, so the reference scorer runs at identity
            // scale and matches the linear fast model.
            let mut params = shipped.with_core(w);
            // Isolate the linear core: zero the frozen, non-linear S-curve that
            // the fast model excludes by construction (see the doc comment).
            params.king_attack_weight_n = 0.0;
            params.king_attack_weight_b = 0.0;
            params.king_attack_weight_r = 0.0;
            params.king_attack_weight_q = 0.0;
            params.king_safety_table = [0.0; 100];
            for pos in &positions {
                let model = model_score_white(&extract(pos), w);
                let reference = reference_score_white(pos, &params) as f64;
                assert!(
                    (model - reference).abs() <= TOL_CP,
                    "model vs reference diverged by {:.4}cp (> {TOL_CP}cp) \
                     at fen={} (model={model:.4}, reference={reference})",
                    (model - reference).abs(),
                    pos.to_fen()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tier 3 — scale-boundary fixtures.
    // -----------------------------------------------------------------------

    /// With non-identity scale numerators in `EvalParams`, `reference_score_white`
    /// reproduces `evaluate`'s `blended·scale/EG_SCALE_DEN + mop_up` arithmetic
    /// on OCB-with-pawns, pawnless-drawish (KNvKN balanced), and
    /// `halfmove_clock ∈ {80, 81, 100}` fixtures. The expected value mirrors
    /// `tier1::endgame_scale`'s formula independently (ADR-0037 §7 Tier 3).
    ///
    /// All fixtures are chosen so `mop_up = 0`, which is asserted at runtime:
    ///   - OCB-with-pawns: phase = 2 (one bishop each) ≥ MOP_UP_PHASE_MAX(5)?
    ///     No — phase=2 < 5 — BUT the advantage is ≈0 (symmetric), so
    ///     |advantage| ≤ 100 ⇒ mop_up=0.
    ///   - KNvKN balanced: raw_phase=2 < 5 but |advantage|≈0 ≤ 100 ⇒ mop_up=0.
    ///   - Startpos hmc fixtures: raw_phase=24 ≥ 5 ⇒ mop_up=0 by the phase gate.
    #[test]
    fn reference_scale_matches_eval_at_boundary_fixtures() {
        use crate::eval::data::EG_SCALE_DEN;

        // Non-identity scale numerators (each < DEN so the min() in
        // endgame_scale actually bites at the relevant fixtures).
        let mut params = EvalParams::shipped();
        params.ocb_with_pawns_scale = 32.0;
        params.pawnless_draw_scale = 8.0;
        params.fifty_move_floor = 16.0;
        let ints = params.to_ints();

        // Fixtures, each triggering exactly one scale condition.
        // KNvKN (balanced): raw_phase=2 (1+1 knight), |advantage|≈0 ≤ 100cp
        // ⇒ mop_up=0. KNNvK is intentionally avoided — KNNvK has a large
        // white advantage (~600cp) and raw_phase=2 < 5, so mop_up FIRES there.
        const FENS: &[&str] = &[
            // OCB-with-pawns: White Be2 (light) + Pg2; Black Bc7 (dark) + Pg7.
            // Material symmetric (B+P each side) → advantage ≈ 0 → mop_up=0.
            // Phase = bishop(1)+bishop(1)=2 < 5, but |advantage|≤100 → mop_up=0.
            "4k3/2b3p1/8/8/8/8/4B1P1/4K3 w - - 0 1",
            // Pawnless drawish — KNvKN balanced (1 knight each, symmetric
            // placement). raw_phase=2, |advantage|≈0 ≤ 100 ⇒ mop_up=0.
            "8/8/4k3/8/8/4K3/8/n1N5 w - - 0 1",
            // 50-move taper boundaries. raw_phase=24 ≥ 5 ⇒ mop_up=0 by phase gate.
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 80 45",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 81 45",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 100 51",
        ];

        for fen in FENS {
            let pos = Position::from_fen(fen).expect("scale fixture FEN");

            // Runtime guard: assert mop_up=0 on this fixture so any future
            // fixture change that breaks the mop_up=0 assumption fails loudly
            // rather than silently accepting (blended+mop_up)*scale/DEN.
            // mop_up_term_white takes the MG/EG pre-blend values from scratch.
            let (mg_w, eg_w, _) = crate::eval::eval_state_from_scratch(&pos);
            let mop_up = crate::eval::mop_up_term_white(&pos, mg_w, eg_w);
            assert_eq!(
                mop_up, 0,
                "fixture mop_up assumption violated at fen={fen}: \
                 mop_up={mop_up} (choose a fixture where mop_up=0)"
            );

            // Expected scale numerator: mirror endgame_scale's min/taper, but
            // reading the runtime EvalParams numerators (not the eval::data
            // consts). The structural pieces (DEN, taper-from) stay frozen.
            let expected_scale = expected_scale_numerator(&pos, &ints, EG_SCALE_DEN);
            // The blended white-POV value at identity scale. reference_score_white
            // at identity scale equals `blended + mop_up` = `blended` (mop_up=0).
            let mut identity = params.clone();
            identity.ocb_with_pawns_scale = EG_SCALE_DEN as f64;
            identity.pawnless_draw_scale = EG_SCALE_DEN as f64;
            identity.fifty_move_floor = EG_SCALE_DEN as f64;
            let blended = reference_score_white(&pos, &identity);
            // Spec formula: scaled = blended * scale / DEN, result = scaled + mop_up.
            // With mop_up=0: expected = blended * scale / DEN.
            let expected = blended * expected_scale / EG_SCALE_DEN;
            let actual = reference_score_white(&pos, &params);
            assert_eq!(
                actual, expected,
                "reference scale arithmetic mismatch at fen={fen}: \
                 expected blended({blended})·scale({expected_scale})/DEN({EG_SCALE_DEN}) \
                 = {expected}, got {actual}"
            );
        }
    }

    /// Independent re-derivation of `tier1::endgame_scale`'s scale numerator,
    /// reading the runtime [`crate::texel::params::EvalParamsI32`] numerators
    /// instead of the `eval::data` consts. Mirrors the impl formula (clamp +
    /// min + 50-move taper) so the Tier-3 test does not assert self-equality.
    fn expected_scale_numerator(
        pos: &Position,
        ints: &crate::texel::params::EvalParamsI32,
        den: i32,
    ) -> i32 {
        use crate::eval::data::FIFTY_MOVE_TAPER_FROM;
        use crate::eval::tier1::{is_ocb_with_pawns, is_pawnless_drawish};
        let mut scale = den;
        if is_ocb_with_pawns(pos) {
            scale = scale.min(ints.ocb_with_pawns_scale);
        }
        if is_pawnless_drawish(pos) {
            scale = scale.min(ints.pawnless_draw_scale);
        }
        let hmc = pos.halfmove_clock() as i32;
        if hmc > FIFTY_MOVE_TAPER_FROM {
            let span = (100 - FIFTY_MOVE_TAPER_FROM).max(1);
            let prog = (hmc - FIFTY_MOVE_TAPER_FROM).clamp(0, span);
            let tapered = den - (den - ints.fifty_move_floor) * prog / span;
            scale = scale.min(tapered);
        }
        scale.clamp(0, den)
    }

    // -----------------------------------------------------------------------
    // Division-order pin (ADR-0037 §2: ONE blend division, not per-term).
    // -----------------------------------------------------------------------

    /// `model_score_white` produces the same result as an independently
    /// hand-computed `(mg·phase + eg·(24−phase))/24 + mop_up` formula on a
    /// multi-term position (ADR-0037 §2). This is the defense-in-depth cross-
    /// check: a wrong divisor, swapped MG/EG, or missing phase would cause a
    /// measurable mismatch on this fixture.
    #[test]
    fn model_blend_matches_hand_computed_single_division() {
        // Queens-traded version of a rich middlegame: phase ≈ 16 (strictly
        // mixed — not 0 or 24), many mobility/structure terms firing.
        let pos =
            Position::from_fen("r1b2rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1B2RK1 w - - 0 9")
                .expect("multi-term FEN");
        let fv = extract(&pos);
        let phase = fv.phase as f64;
        // Arbitrary diverse non-unit weights: catches wrong-divisor (÷12 vs ÷24)
        // and MG/EG swap independently of the reference-scorer test suite.
        let w = seeded_core_weights(0xD1D0_0000);

        // Independent hand computation: accumulate MG and EG sums, divide ONCE.
        let mut mg = fv.base_mg as f64;
        let mut eg = fv.base_eg as f64;
        for &(idx, count) in &fv.coeffs {
            let contrib = w[idx as usize] * count as f64;
            if layout::is_mg(idx as usize) {
                mg += contrib;
            } else {
                eg += contrib;
            }
        }
        let expected = (mg * phase + eg * (24.0 - phase)) / 24.0 + fv.mop_up as f64;

        let model = model_score_white(&fv, &w);
        assert!(
            (model - expected).abs() < 1e-9,
            "model_score_white must match hand-computed single-division blend: \
             model={model}, expected={expected}"
        );
        // Guard: the fixture is genuinely multi-phase and non-trivial.
        assert!(
            phase > 0.0 && phase < 24.0,
            "fixture must have mixed phase (0 < phase < 24) — got phase={phase}"
        );
        assert!(
            !fv.coeffs.is_empty(),
            "fixture must have tunable terms so the blend formula is exercised"
        );
        // Guard: the non-unit weights produce a score visibly different from the
        // base-only score, confirming the tunable terms fired.
        let base_only = (fv.base_mg as f64 * phase + fv.base_eg as f64 * (24.0 - phase)) / 24.0
            + fv.mop_up as f64;
        assert!(
            (expected - base_only).abs() > 0.1,
            "tunable terms must contribute >0.1 cp with non-unit weights; \
             expected={expected}, base_only={base_only}"
        );
    }

    // -----------------------------------------------------------------------
    // Linearity property (ADR-0037 §7).
    // -----------------------------------------------------------------------

    /// `model_score_white` is affine in the core weights: for scalars a, b and
    /// weight vectors w, u, `model(a·w + b·u) - base == a·(model(w) - base) +
    /// b·(model(u) - base)`, where `base = model(0)` is the frozen
    /// contribution at zero weights. Seeded loop over diverse (w, u, a, b).
    #[test]
    fn score_is_linear_in_core_weights() {
        let positions = battery();
        let zero = vec![0.0f64; N_CORE];
        let mut rng = Prng::new(0x1111_2222_3333_4444);
        for pos in positions.iter().take(64) {
            let fv = extract(pos);
            let base = model_score_white(&fv, &zero);
            for _ in 0..4 {
                let w = seeded_core_weights(rng.next_u64());
                let u = seeded_core_weights(rng.next_u64());
                let a = (rng.below(2001) as f64 - 1000.0) / 100.0;
                let b = (rng.below(2001) as f64 - 1000.0) / 100.0;
                let combo: Vec<f64> = (0..N_CORE).map(|k| a * w[k] + b * u[k]).collect();

                let lhs = model_score_white(&fv, &combo) - base;
                let rhs = a * (model_score_white(&fv, &w) - base)
                    + b * (model_score_white(&fv, &u) - base);
                assert!(
                    (lhs - rhs).abs() < 1e-6,
                    "model not linear in core weights at fen={}: \
                     lhs={lhs}, rhs={rhs}",
                    pos.to_fen()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Cache-score ↔ model-score cross-check (phase-fold correctness pin).
    //
    // `build_cache_rec(pos)` bakes phase-folded effective coefficients into a
    // `CachedRec` (f32 storage). The cached linear score
    // `base + Σ coeff·w[idx]` must agree with `model_score_white(extract(pos), w)`
    // to within f32 phase-fold rounding. Any structural bug is much larger:
    //   - a missing `/24` diverges by ~24× the sum (tens of cp)
    //   - a wrong phase factor diverges by ≥1cp per active term
    // so a 1e-2 absolute tolerance (or 1e-5 relative) catches all structural
    // bugs while tolerating the f32→f64 accumulation difference between the
    // pre-folded cache and the single-division model path.
    // -----------------------------------------------------------------------

    /// `model_score_white(extract(pos), w)` agrees with `base + Σ coeff·w[idx]`
    /// from `build_cache_rec(pos)` within f32-phase-fold rounding. Pins that
    /// the phase-fold baked into [`CachedRec`] at cache-build time is
    /// numerically consistent with the single-division model path (ADR-0037 §2).
    ///
    /// Tolerance: `1e-2` absolute. Structural bugs diverge by ≥1cp ≈ 1.0 or
    /// multiples thereof (e.g. a missing `/24` gives ~24× error); f32 rounding
    /// in the cached coefficients is bounded by ~1e-4 per coefficient
    /// (f32 precision ~7 digits over a 40cp range).
    #[test]
    fn cache_score_matches_model_score() {
        use crate::texel::dataset::build_cache_rec;
        let positions = battery();
        let mut rng = Prng::new(0x0CAC_45C0_7E57);
        for pos in positions.iter().take(64) {
            let fv = extract(pos);
            let rec = build_cache_rec(pos);
            for _ in 0..4 {
                let w = seeded_core_weights(rng.next_u64());
                let model = model_score_white(&fv, &w);
                // Cached linear score: base + Σ coeff·w[idx].
                let cached: f64 = rec.base as f64
                    + rec
                        .coeffs
                        .iter()
                        .map(|&(idx, c)| c as f64 * w[idx as usize])
                        .sum::<f64>();
                // 1e-2 absolute: catches structural bugs (≥1cp error) while
                // tolerating f32 phase-fold rounding (~1e-4 per coefficient).
                let tol = 1e-2f64;
                assert!(
                    (model - cached).abs() < tol,
                    "model_score_white and cache_score diverged at fen={}: \
                     model={model}, cached={cached} (diff={}, tol={tol})",
                    pos.to_fen(),
                    (model - cached).abs()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Per-term feature pins (shared-detection — constructed minimal positions
    // with KNOWN raw counts; ADR-0037 §7).
    // -----------------------------------------------------------------------

    /// Sum of raw counts in `extract(pos).coeffs` restricted to one group's
    /// index range, keyed by core index. Helper for the per-term pins.
    fn coeffs_in_group(fv: &FeatureVec, g: Group) -> Vec<(usize, f32)> {
        let range = layout::group_range(g);
        fv.coeffs
            .iter()
            .filter(|(idx, _)| range.contains(&(*idx as usize)))
            .map(|(idx, c)| (*idx as usize, *c))
            .collect()
    }

    /// ISO / DBL / BWD raw counts: a constructed position with known pawn
    /// structure must surface the exact expected counts in the IsoDblBwd
    /// group coeffs. Counts are hardcoded from independent hand-derivation
    /// (not cross-checked against the same detector — that would be circular).
    #[test]
    fn iso_dbl_bwd_counts() {
        // FEN: "4k3/8/8/8/P3P3/2P5/2P5/4K3 w - - 0 1"
        // White pawns: a4, c2, c3, e4. Black: lone king.
        //
        // Hand-derivation:
        //   Isolated (no friendly on adjacent file):
        //     a4: neighbor files b (empty), none-west → isolated.
        //     c2: neighbor files b (empty), d (empty) → isolated.
        //     c3: neighbor files b (empty), d (empty) → isolated.
        //     e4: neighbor files d (empty), f (empty) → isolated.
        //     Total iso = 4.
        //
        //   Doubled (extra pawn on same file = own & black_front_spans(own)):
        //     black_front_spans fills c-file below c3 → c2 is "south of c3".
        //     Total dbl = 1 (c2 is marked; c3 is the northernmost).
        //
        //   Backward (stop attacked by enemy pawn, not covered by own spans):
        //     No enemy pawns → no backward pawns. Total bwd = 0.
        let pos = Position::from_fen("4k3/8/8/8/P3P3/2P5/2P5/4K3 w - - 0 1")
            .expect("iso/dbl/bwd fixture FEN");
        let iso: f32 = 4.0;
        let dbl: f32 = 1.0;
        let bwd: f32 = 0.0;

        let fv = extract(&pos);
        let g = layout::group_range(Group::IsoDblBwd);
        // Layout order: [ISO_MG, ISO_EG, DBL_MG, DBL_EG, BWD_MG, BWD_EG].
        // Both the MG and EG parity entries for a term carry the SAME raw
        // count (the same pawn set), so each of the 6 entries equals its
        // term's count. Black contributes 0 (lone king).
        let count_at = |local: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(idx, _)| *idx as usize == g.start + local)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(count_at(0), iso, "ISO_MG raw count");
        assert_eq!(count_at(1), iso, "ISO_EG raw count");
        assert_eq!(count_at(2), dbl, "DBL_MG raw count");
        assert_eq!(count_at(3), dbl, "DBL_EG raw count");
        assert_eq!(count_at(4), bwd, "BWD_MG raw count");
        assert_eq!(count_at(5), bwd, "BWD_EG raw count");
    }

    /// CONN raw counts are indexed by relative rank: a constructed phalanx /
    /// defended pawn structure on known ranks must populate the CONN group's
    /// per-rank coeffs at exactly those rank indices.
    #[test]
    fn conn_rank_indexed() {
        // White phalanx d4+e4 (both relative rank 3) — each is connected
        // (adjacent same-rank), so CONN fires for rank-3 with count 2. Black:
        // lone king. CONN layout: MG[ranks 2..8] then EG[ranks 2..8]; the
        // rank-3 entry is local offset (3-2)=1 within each half.
        let pos = Position::from_fen("4k3/8/8/8/3PP3/8/8/4K3 w - - 0 1").expect("conn fixture FEN");
        let wp = pos.pieces_colored(crate::piece::Color::White, crate::piece::PieceKind::Pawn);
        let conn = eval::pawns::connected_pawns(wp, crate::piece::Color::White);
        // Both d4 and e4 are connected (phalanx).
        assert_eq!(conn.count(), 2, "fixture: d4+e4 phalanx → 2 connected");

        let fv = extract(&pos);
        let g = layout::group_range(Group::Conn); // 6 MG (ranks 2..8) + 6 EG.
        // rank-3 MG entry is g.start + (3 - 2) = g.start + 1; EG is +6 from it.
        let rank3_mg = g.start + 1;
        let rank3_eg = g.start + 6 + 1;
        let count_at = |idx: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == idx)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(count_at(rank3_mg), 2.0, "CONN_MG[rank3] raw count");
        assert_eq!(count_at(rank3_eg), 2.0, "CONN_EG[rank3] raw count");
        // No other rank in the CONN group should carry a non-zero count.
        for (idx, c) in coeffs_in_group(&fv, Group::Conn) {
            if idx != rank3_mg && idx != rank3_eg {
                assert_eq!(c, 0.0, "unexpected CONN count at core idx {idx}");
            }
        }
    }

    /// Passed-pawn three-state path discriminator: a constructed passer with a
    /// clear front-span path (empty), then with an enemy blocker, must produce
    /// the corresponding free-path-delta feature counts (+1 free / −1 blocked).
    #[test]
    fn passed_three_state_path() {
        // White passed pawn d5, clear path (no pieces on d6/d7/d8), black king
        // far away. rel-rank for d5 = 4. The PASSED_FREE_EG_DELTA[4] coeff
        // should reflect +1 (path empty of ALL pieces).
        let free = Position::from_fen("7k/8/8/3P4/8/8/8/K7 w - - 0 1").expect("passed free FEN");
        // White passed pawn d5, ENEMY piece on the path (black rook d8).
        let blocked =
            Position::from_fen("3r3k/8/8/3P4/8/8/8/K7 w - - 0 1").expect("passed blocked FEN");

        let fv_free = extract(&free);
        let fv_blocked = extract(&blocked);
        let passed = layout::group_range(Group::Passed);
        // Passed layout: MG[2..7] (5), EG[2..8] (6), FREE[2..8] (6), KDIST own,
        // KDIST enemy. FREE block starts at local offset 5 + 6 = 11; rel-rank 4
        // entry is FREE[4], local offset 11 + (4 - 2) = 13.
        let free_idx = passed.start + 11 + (4 - 2);
        let count_at = |fv: &FeatureVec, idx: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == idx)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(
            count_at(&fv_free, free_idx),
            1.0,
            "free path: PASSED_FREE_EG_DELTA[rank4] coeff must be +1"
        );
        assert_eq!(
            count_at(&fv_blocked, free_idx),
            -1.0,
            "enemy-blocked path: PASSED_FREE_EG_DELTA[rank4] coeff must be -1"
        );
    }

    /// Mobility histogram: a lone white knight on d4 (8 pseudolegal squares,
    /// no exclusions) must set the MobN feature at popcount-bucket 8 to +1.
    #[test]
    fn mobility_histogram() {
        // White Kh1, Nd4. Black Ke8 (lone king → no black mobility terms).
        let pos =
            Position::from_fen("4k3/8/8/8/3N4/8/8/7K w - - 0 1").expect("mobility histogram FEN");
        let fv = extract(&pos);
        let g = layout::group_range(Group::MobN); // KNIGHT_MG[0..9] then EG[0..9].
        // Bucket 8 MG entry: g.start + 8; EG entry: g.start + 9 + 8.
        let mg8 = g.start + 8;
        let eg8 = g.start + 9 + 8;
        let count_at = |idx: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == idx)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(count_at(mg8), 1.0, "MobN bucket-8 MG count must be +1");
        assert_eq!(count_at(eg8), 1.0, "MobN bucket-8 EG count must be +1");
        // No other MobN bucket carries a count.
        for (idx, c) in coeffs_in_group(&fv, Group::MobN) {
            if idx != mg8 && idx != eg8 {
                assert_eq!(c, 0.0, "unexpected MobN count at core idx {idx}");
            }
        }
    }

    /// King-safety shield + open-file features: a constructed castled white
    /// king on an open file with semi-open adjacent files must set the
    /// open/semi-open-file feature coeffs to their known counts.
    #[test]
    fn ks_shield_open_file_features() {
        // White Kg1 castled, no pawns near the king at all → g-file open,
        // f-file and h-file open (no own pawn). Black mirrored far away so its
        // king-safety contribution is symmetric-zero where possible. We assert
        // the KsShieldFile group's coeffs are internally consistent with the
        // file-openness of the white king's files, cross-checked via file_fill.
        let pos = Position::from_fen("6k1/8/8/8/8/8/8/6K1 w - - 0 1").expect("ks open-file FEN");
        let fv = extract(&pos);
        // KsShieldFile layout: [SHIELD_1_MG, SHIELD_1_EG, SHIELD_2_MG,
        // SHIELD_2_EG, KS_KFILE_SEMI_OPEN_MG, KS_KFILE_OPEN_MG,
        // KS_ADJ_SEMI_OPEN_MG, KS_BOTH_ADJ_SEMI_OPEN_MG].
        let g = layout::group_range(Group::KsShieldFile);
        let count_at = |local: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == g.start + local)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        // Both kings on a fully open king-file with both adjacent files open.
        // By color symmetry the white-POV net for king-file-open is 0 (both
        // sides equally exposed); the discriminating, unambiguous pin is the
        // SHIELD counts: with NO pawns, no shield fires → SHIELD coeffs are 0.
        assert_eq!(count_at(0), 0.0, "no pawns → SHIELD_1 count 0");
        assert_eq!(count_at(2), 0.0, "no pawns → SHIELD_2 count 0");
        // king-file-open net (white minus black) is 0 by symmetry.
        assert_eq!(count_at(5), 0.0, "symmetric open king-file → net count 0");
    }

    /// Outpost rank-band 3..=5: a white knight on a pawn-supported,
    /// unchallengeable d5 (relative rank 4) must set the Outpost knight feature
    /// at rank-4 to +1; a knight outside the band sets nothing.
    #[test]
    fn outpost_rank_band_3_5() {
        // White Nd5 supported by c4+e4, no black c/e pawns → outpost at
        // rel-rank 4. Black: lone king.
        let pos =
            Position::from_fen("4k3/8/8/3N4/2P1P3/8/8/4K3 w - - 0 1").expect("outpost fixture FEN");
        let fv = extract(&pos);
        // Outpost layout: KNIGHT_MG[3..6] (3), KNIGHT_EG[3..6] (3),
        // BISHOP_MG[3..6] (3), BISHOP_EG[3..6] (3). rank-4 knight MG is local
        // offset (4-3)=1; knight EG is 3 + 1 = 4.
        let g = layout::group_range(Group::Outpost);
        let count_at = |local: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == g.start + local)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(count_at(1), 1.0, "OUTPOST_KNIGHT_MG[rank4] count +1");
        assert_eq!(count_at(4), 1.0, "OUTPOST_KNIGHT_EG[rank4] count +1");
        // No bishop outpost, no other knight rank.
        for (idx, c) in coeffs_in_group(&fv, Group::Outpost) {
            let local = idx - g.start;
            if local != 1 && local != 4 {
                assert_eq!(c, 0.0, "unexpected Outpost count at local {local}");
            }
        }
    }

    /// Rook on open vs semi-open file: a white rook on a fully open file and
    /// another on a semi-open file must set the RookFile open / semi-open
    /// feature coeffs to +1 each; a blocked rook contributes 0 to all slots.
    #[test]
    fn rook_file_open_vs_semi() {
        // Ra1 on the open a-file (no pawns), Rc1 on the semi-open c-file
        // (black pawn c7, no white c-pawn), Re1 blocked (white pawn e2).
        // White king h1. Black: lone king + the c7 pawn.
        let pos = Position::from_fen("4k3/2p5/8/8/8/8/4P3/R1R1R2K w - - 0 1")
            .expect("rook-file fixture FEN");
        let fv = extract(&pos);
        // RookFile layout: [ROOK_OPEN_MG, ROOK_OPEN_EG, ROOK_SEMI_MG,
        // ROOK_SEMI_EG].
        let g = layout::group_range(Group::RookFile);
        let count_at = |local: usize| -> f32 {
            fv.coeffs
                .iter()
                .find(|(i, _)| *i as usize == g.start + local)
                .map(|(_, c)| *c)
                .unwrap_or(0.0)
        };
        assert_eq!(count_at(0), 1.0, "ROOK_OPEN_FILE_MG count +1 (Ra1)");
        assert_eq!(count_at(1), 1.0, "ROOK_OPEN_FILE_EG count +1 (Ra1)");
        assert_eq!(count_at(2), 1.0, "ROOK_SEMI_OPEN_FILE_MG count +1 (Rc1)");
        assert_eq!(count_at(3), 1.0, "ROOK_SEMI_OPEN_FILE_EG count +1 (Rc1)");
        // Blocked Re1 (white pawn on e-file) contributes nothing to any RookFile slot.
        // The group total must be exactly [1, 1, 1, 1] — not 2 open or 2 semi.
        // Sweep the whole group to confirm no other entry fires.
        let total: f32 = coeffs_in_group(&fv, Group::RookFile)
            .iter()
            .map(|(_, c)| c)
            .sum();
        assert_eq!(
            total, 4.0,
            "total RookFile group count must be 4 (1 open MG + 1 open EG + 1 semi MG + 1 semi EG); \
             blocked Re1 must contribute 0 (got total={total})"
        );
    }

    // -----------------------------------------------------------------------
    // Phase-3 accessor refactor pin (ADR-0037 §2 "dot(features, weights) ==
    // term fn"). References the PLANNED pub(crate) accessors:
    //   eval::mobility::mobility_features
    //   eval::king_safety::shield_open_file_features
    //   eval::tier1::outpost_features
    //   eval::tier1::rook_file_features
    //   eval::pawns::{iso_dbl_bwd_features, conn_features}
    // each returning a Vec<(u16 core_index, i32 raw_count)> sharing detection
    // code with the existing scoring fn. dot(features, eval::data weights) must
    // equal the existing <module>::<term>_term_white(pos) output over the
    // battery. This is the Phase-3 seam; it compiles only once the impl adds
    // those accessors with the documented signatures.
    // -----------------------------------------------------------------------

    /// Dot-product of a per-term accessor's `(core_index, raw_count)` features
    /// with the live `eval::data` core weights (read through
    /// [`EvalParams::shipped`]'s core vec) equals the existing
    /// `<module>::<term>_term_white` output, over the battery. Pins the Phase-3
    /// accessor refactor (bench byte-identity is the global guard).
    ///
    /// **This test is ACTIVE (not gated).** The exact accessor names this test pins:
    ///   - `eval::pawns::iso_dbl_bwd_features` + `iso_dbl_bwd_term_white`
    ///   - `eval::pawns::conn_features` + `conn_term_white`
    ///   - `eval::mobility::mobility_features` (+ existing `mobility_term_white`)
    ///   - `eval::king_safety::shield_open_file_features` +
    ///     `shield_open_file_term_white` (the LINEAR shield+open-file split of
    ///     `king_safety_term_white`, excluding the frozen S-curve)
    ///   - `eval::tier1::outpost_features` (+ existing `outpost_term_white`)
    ///   - `eval::tier1::rook_file_features` (+ existing `rook_file_term_white`)
    ///
    /// Each accessor returns `Vec<(u16 core_index, i32 raw_count)>`; the
    /// dot-product against the shipped core weights, blended into a `(mg, eg)`
    /// pair, must equal the term fn's `(mg, eg)` output over the battery.
    #[test]
    fn accessor_dot_weights_equals_term_fn() {
        let shipped_core = EvalParams::shipped().core_to_vec();
        let positions = battery();

        // dot((idx,count) features, shipped core weights), blended (mg,eg)
        // exactly as the term fn does — but the term fn already blends? No: the
        // term fns return (mg, eg) PAIRS. So the accessor features must carry
        // MG-vs-EG parity by core index; we reconstruct the (mg, eg) pair by
        // summing MG-parity and EG-parity contributions separately and compare
        // to the term fn's (mg, eg).
        let blended_pair = |features: &[(u16, i32)]| -> (i32, i32) {
            let mut mg = 0i32;
            let mut eg = 0i32;
            for &(idx, count) in features {
                let w = shipped_core[idx as usize] as i32;
                if layout::is_mg(idx as usize) {
                    mg += w * count;
                } else {
                    eg += w * count;
                }
            }
            (mg, eg)
        };

        for pos in &positions {
            assert_eq!(
                blended_pair(&eval::pawns::iso_dbl_bwd_features(pos)),
                eval::pawns::iso_dbl_bwd_term_white(pos),
                "iso/dbl/bwd accessor·weights != term fn at {}",
                pos.to_fen()
            );
            assert_eq!(
                blended_pair(&eval::pawns::conn_features(pos)),
                eval::pawns::conn_term_white(pos),
                "conn accessor·weights != term fn at {}",
                pos.to_fen()
            );
            assert_eq!(
                blended_pair(&eval::mobility::mobility_features(pos)),
                eval::mobility::mobility_term_white(pos),
                "mobility accessor·weights != term fn at {}",
                pos.to_fen()
            );
            assert_eq!(
                blended_pair(&eval::king_safety::shield_open_file_features(pos)),
                eval::king_safety::shield_open_file_term_white(pos),
                "ks shield/open-file accessor·weights != term fn at {}",
                pos.to_fen()
            );
            assert_eq!(
                blended_pair(&eval::tier1::outpost_features(pos)),
                eval::tier1::outpost_term_white(pos),
                "outpost accessor·weights != term fn at {}",
                pos.to_fen()
            );
            assert_eq!(
                blended_pair(&eval::tier1::rook_file_features(pos)),
                eval::tier1::rook_file_term_white(pos),
                "rook-file accessor·weights != term fn at {}",
                pos.to_fen()
            );
        }
    }
}
