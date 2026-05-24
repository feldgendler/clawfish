//! The full runtime eval weight set ([`EvalParams`]) + its integer mirror,
//! the flat-core vec views, and the `bench/m6-params.json` artifact.
//!
//! [`EvalParams`] carries the FULL tunable eval surface as runtime fields,
//! mirroring `eval::data` array-for-array. The [`layout`](crate::texel::layout)
//! linear-core groups live here as the live values to be tuned; the
//! excluded/outer fields (the 4 king-attack multipliers, the 100-entry S-curve
//! table, the 3 scale numerators) are carried so a round-trip is total, but
//! they are NOT part of the core vector — they are held at `shipped()` (the
//! S-curve + multipliers are frozen, the scale numerators are outer-swept).
//!
//! [`shipped`] reads the current `eval::data` const values so this struct is
//! the single source of truth (CONN is live / non-zero; every other deferred
//! term is zero — the M6.F production state).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::eval::data;
use crate::texel::TexelError;
use crate::texel::layout::N_CORE;

/// `bench/m6-params.json` schema version. Bump on any field/layout change;
/// [`read_params_file`] rejects a mismatch (ADR-0037 §9 reproducibility).
pub const SCHEMA_VERSION: u32 = 1;

/// The full runtime eval weight set, mirroring `eval::data` field-for-field.
///
/// Linear-core groups (the [`layout`](crate::texel::layout) vector) plus the
/// excluded/outer fields carried for total round-trips. Arrays match the
/// `eval::data` array lengths exactly.
#[derive(Clone, PartialEq, Debug)]
pub struct EvalParams {
    // ----- IsoDblBwd (core) -----
    /// Isolated-pawn MG penalty.
    pub iso_mg: f64,
    /// Isolated-pawn EG penalty.
    pub iso_eg: f64,
    /// Doubled-pawn MG penalty.
    pub dbl_mg: f64,
    /// Doubled-pawn EG penalty.
    pub dbl_eg: f64,
    /// Backward-pawn MG penalty.
    pub bwd_mg: f64,
    /// Backward-pawn EG penalty.
    pub bwd_eg: f64,

    // ----- Conn (core: ranks 2..8) -----
    /// Connected-pawn MG bonus by relative rank (0..8); indices 0,1,? structural.
    pub conn_mg: [f64; 8],
    /// Connected-pawn EG bonus by relative rank (0..8).
    pub conn_eg: [f64; 8],

    // ----- Passed (core) -----
    /// Passed-pawn MG rank bonus (0..8); index 7 structurally 0 (excluded).
    pub passed_mg: [f64; 8],
    /// Passed-pawn EG rank bonus (0..8).
    pub passed_eg: [f64; 8],
    /// Passed-pawn EG free-path delta (0..8).
    pub passed_free_eg_delta: [f64; 8],
    /// Own-king tropism per Chebyshev step (EG).
    pub passed_kdist_own_per_step: f64,
    /// Enemy-king tropism per Chebyshev step (EG).
    pub passed_kdist_enemy_per_step: f64,

    // ----- Mobility (core) -----
    /// Knight mobility MG by popcount (0..9).
    pub knight_mobility_mg: [f64; 9],
    /// Knight mobility EG by popcount (0..9).
    pub knight_mobility_eg: [f64; 9],
    /// Bishop mobility MG by popcount (0..14).
    pub bishop_mobility_mg: [f64; 14],
    /// Bishop mobility EG by popcount (0..14).
    pub bishop_mobility_eg: [f64; 14],
    /// Rook mobility MG by popcount (0..15).
    pub rook_mobility_mg: [f64; 15],
    /// Rook mobility EG by popcount (0..15).
    pub rook_mobility_eg: [f64; 15],
    /// Queen mobility MG by popcount (0..28).
    pub queen_mobility_mg: [f64; 28],
    /// Queen mobility EG by popcount (0..28).
    pub queen_mobility_eg: [f64; 28],

    // ----- KsShieldFile (core) -----
    /// Pawn-shield bonus, king's 2nd rank, MG.
    pub shield_1_mg: f64,
    /// Pawn-shield bonus, king's 2nd rank, EG.
    pub shield_1_eg: f64,
    /// Pawn-shield bonus, king's 3rd rank, MG.
    pub shield_2_mg: f64,
    /// Pawn-shield bonus, king's 3rd rank, EG.
    pub shield_2_eg: f64,
    /// King-file semi-open MG penalty.
    pub ks_kfile_semi_open_mg: f64,
    /// King-file open MG penalty.
    pub ks_kfile_open_mg: f64,
    /// Adjacent-file semi-open MG penalty (per file).
    pub ks_adj_semi_open_mg: f64,
    /// Both adjacent files semi-open MG amplification.
    pub ks_both_adj_semi_open_mg: f64,

    // ----- Outpost (core: ranks 3..6) -----
    /// Knight-outpost MG bonus by relative rank (0..8).
    pub outpost_knight_mg: [f64; 8],
    /// Knight-outpost EG bonus by relative rank (0..8).
    pub outpost_knight_eg: [f64; 8],
    /// Bishop-outpost MG bonus by relative rank (0..8).
    pub outpost_bishop_mg: [f64; 8],
    /// Bishop-outpost EG bonus by relative rank (0..8).
    pub outpost_bishop_eg: [f64; 8],

    // ----- RookFile (core) -----
    /// Rook-on-open-file MG bonus.
    pub rook_open_file_mg: f64,
    /// Rook-on-open-file EG bonus.
    pub rook_open_file_eg: f64,
    /// Rook-on-semi-open-file MG bonus.
    pub rook_semi_open_file_mg: f64,
    /// Rook-on-semi-open-file EG bonus.
    pub rook_semi_open_file_eg: f64,

    // ----- Excluded: king-safety S-curve + attacker multipliers (frozen) -----
    /// King-zone attacker-unit multiplier, knight (frozen at shipped).
    pub king_attack_weight_n: f64,
    /// King-zone attacker-unit multiplier, bishop (frozen at shipped).
    pub king_attack_weight_b: f64,
    /// King-zone attacker-unit multiplier, rook (frozen at shipped).
    pub king_attack_weight_r: f64,
    /// King-zone attacker-unit multiplier, queen (frozen at shipped).
    pub king_attack_weight_q: f64,
    /// 100-entry king-safety S-curve table (frozen at shipped).
    pub king_safety_table: [f64; 100],

    // ----- Outer-swept: endgame-scale numerators (not in core vec) -----
    /// OCB-with-pawns draw-scale numerator (outer-swept).
    pub ocb_with_pawns_scale: f64,
    /// Pawnless drawish endgame scale numerator (outer-swept).
    pub pawnless_draw_scale: f64,
    /// 50-move taper floor numerator (outer-swept).
    pub fifty_move_floor: f64,
}

impl EvalParams {
    /// Reads the current `eval::data` const values — the single source of
    /// truth. CONN is live (non-zero); every other deferred term is zero (the
    /// M6.F production state).
    pub fn shipped() -> Self {
        EvalParams {
            iso_mg: data::ISO_MG as f64,
            iso_eg: data::ISO_EG as f64,
            dbl_mg: data::DBL_MG as f64,
            dbl_eg: data::DBL_EG as f64,
            bwd_mg: data::BWD_MG as f64,
            bwd_eg: data::BWD_EG as f64,

            conn_mg: arr8(&data::CONN_MG),
            conn_eg: arr8(&data::CONN_EG),

            passed_mg: arr8(&data::PASSED_MG),
            passed_eg: arr8(&data::PASSED_EG),
            passed_free_eg_delta: arr8(&data::PASSED_FREE_EG_DELTA),
            passed_kdist_own_per_step: data::PASSED_KDIST_OWN_PER_STEP as f64,
            passed_kdist_enemy_per_step: data::PASSED_KDIST_ENEMY_PER_STEP as f64,

            knight_mobility_mg: arr(&data::KNIGHT_MOBILITY_MG),
            knight_mobility_eg: arr(&data::KNIGHT_MOBILITY_EG),
            bishop_mobility_mg: arr(&data::BISHOP_MOBILITY_MG),
            bishop_mobility_eg: arr(&data::BISHOP_MOBILITY_EG),
            rook_mobility_mg: arr(&data::ROOK_MOBILITY_MG),
            rook_mobility_eg: arr(&data::ROOK_MOBILITY_EG),
            queen_mobility_mg: arr(&data::QUEEN_MOBILITY_MG),
            queen_mobility_eg: arr(&data::QUEEN_MOBILITY_EG),

            shield_1_mg: data::SHIELD_1_MG as f64,
            shield_1_eg: data::SHIELD_1_EG as f64,
            shield_2_mg: data::SHIELD_2_MG as f64,
            shield_2_eg: data::SHIELD_2_EG as f64,
            ks_kfile_semi_open_mg: data::KS_KFILE_SEMI_OPEN_MG as f64,
            ks_kfile_open_mg: data::KS_KFILE_OPEN_MG as f64,
            ks_adj_semi_open_mg: data::KS_ADJ_SEMI_OPEN_MG as f64,
            ks_both_adj_semi_open_mg: data::KS_BOTH_ADJ_SEMI_OPEN_MG as f64,

            outpost_knight_mg: arr8(&data::OUTPOST_KNIGHT_MG),
            outpost_knight_eg: arr8(&data::OUTPOST_KNIGHT_EG),
            outpost_bishop_mg: arr8(&data::OUTPOST_BISHOP_MG),
            outpost_bishop_eg: arr8(&data::OUTPOST_BISHOP_EG),

            rook_open_file_mg: data::ROOK_OPEN_FILE_MG as f64,
            rook_open_file_eg: data::ROOK_OPEN_FILE_EG as f64,
            rook_semi_open_file_mg: data::ROOK_SEMI_OPEN_FILE_MG as f64,
            rook_semi_open_file_eg: data::ROOK_SEMI_OPEN_FILE_EG as f64,

            king_attack_weight_n: data::KING_ATTACK_WEIGHT_N as f64,
            king_attack_weight_b: data::KING_ATTACK_WEIGHT_B as f64,
            king_attack_weight_r: data::KING_ATTACK_WEIGHT_R as f64,
            king_attack_weight_q: data::KING_ATTACK_WEIGHT_Q as f64,
            king_safety_table: arr(&data::KING_SAFETY_TABLE),

            ocb_with_pawns_scale: data::OCB_WITH_PAWNS_SCALE as f64,
            pawnless_draw_scale: data::PAWNLESS_DRAW_SCALE as f64,
            fifty_move_floor: data::FIFTY_MOVE_FLOOR as f64,
        }
    }

    /// Flat linear-core view (length [`N_CORE`], [`layout`](crate::texel::layout)
    /// order). Excluded/outer fields are NOT included.
    pub fn core_to_vec(&self) -> Vec<f64> {
        let mut v = Vec::with_capacity(N_CORE);

        // IsoDblBwd (0..6): interleaved MG/EG pairs.
        v.extend_from_slice(&[
            self.iso_mg,
            self.iso_eg,
            self.dbl_mg,
            self.dbl_eg,
            self.bwd_mg,
            self.bwd_eg,
        ]);

        // Conn (6..18): MG[2..8] then EG[2..8].
        v.extend_from_slice(&self.conn_mg[2..8]);
        v.extend_from_slice(&self.conn_eg[2..8]);

        // Passed (18..37): MG[2..7], EG[2..8], FREE[2..8], KDIST own/enemy.
        v.extend_from_slice(&self.passed_mg[2..7]);
        v.extend_from_slice(&self.passed_eg[2..8]);
        v.extend_from_slice(&self.passed_free_eg_delta[2..8]);
        v.push(self.passed_kdist_own_per_step);
        v.push(self.passed_kdist_enemy_per_step);

        // Mobility (37..169): each kind MG then EG.
        v.extend_from_slice(&self.knight_mobility_mg);
        v.extend_from_slice(&self.knight_mobility_eg);
        v.extend_from_slice(&self.bishop_mobility_mg);
        v.extend_from_slice(&self.bishop_mobility_eg);
        v.extend_from_slice(&self.rook_mobility_mg);
        v.extend_from_slice(&self.rook_mobility_eg);
        v.extend_from_slice(&self.queen_mobility_mg);
        v.extend_from_slice(&self.queen_mobility_eg);

        // KsShieldFile (169..177).
        v.extend_from_slice(&[
            self.shield_1_mg,
            self.shield_1_eg,
            self.shield_2_mg,
            self.shield_2_eg,
            self.ks_kfile_semi_open_mg,
            self.ks_kfile_open_mg,
            self.ks_adj_semi_open_mg,
            self.ks_both_adj_semi_open_mg,
        ]);

        // Outpost (177..189): KNIGHT_MG[3..6], KNIGHT_EG[3..6], BISHOP_MG[3..6],
        // BISHOP_EG[3..6].
        v.extend_from_slice(&self.outpost_knight_mg[3..6]);
        v.extend_from_slice(&self.outpost_knight_eg[3..6]);
        v.extend_from_slice(&self.outpost_bishop_mg[3..6]);
        v.extend_from_slice(&self.outpost_bishop_eg[3..6]);

        // RookFile (189..193).
        v.extend_from_slice(&[
            self.rook_open_file_mg,
            self.rook_open_file_eg,
            self.rook_semi_open_file_mg,
            self.rook_semi_open_file_eg,
        ]);

        debug_assert_eq!(v.len(), N_CORE);
        v
    }

    /// Returns a copy with the core fields replaced from the flat vec `v`
    /// ([`layout`](crate::texel::layout) order); non-core fields kept from
    /// `self`.
    ///
    /// # Panics
    /// Debug-asserts `v.len() == N_CORE`.
    pub fn with_core(&self, v: &[f64]) -> EvalParams {
        debug_assert_eq!(v.len(), N_CORE, "with_core expects an N_CORE-length vec");
        let mut p = self.clone();
        let mut i = 0usize;
        let mut take = || {
            let x = v[i];
            i += 1;
            x
        };

        p.iso_mg = take();
        p.iso_eg = take();
        p.dbl_mg = take();
        p.dbl_eg = take();
        p.bwd_mg = take();
        p.bwd_eg = take();

        for k in 2..8 {
            p.conn_mg[k] = take();
        }
        for k in 2..8 {
            p.conn_eg[k] = take();
        }

        for k in 2..7 {
            p.passed_mg[k] = take();
        }
        for k in 2..8 {
            p.passed_eg[k] = take();
        }
        for k in 2..8 {
            p.passed_free_eg_delta[k] = take();
        }
        p.passed_kdist_own_per_step = take();
        p.passed_kdist_enemy_per_step = take();

        for k in 0..9 {
            p.knight_mobility_mg[k] = take();
        }
        for k in 0..9 {
            p.knight_mobility_eg[k] = take();
        }
        for k in 0..14 {
            p.bishop_mobility_mg[k] = take();
        }
        for k in 0..14 {
            p.bishop_mobility_eg[k] = take();
        }
        for k in 0..15 {
            p.rook_mobility_mg[k] = take();
        }
        for k in 0..15 {
            p.rook_mobility_eg[k] = take();
        }
        for k in 0..28 {
            p.queen_mobility_mg[k] = take();
        }
        for k in 0..28 {
            p.queen_mobility_eg[k] = take();
        }

        p.shield_1_mg = take();
        p.shield_1_eg = take();
        p.shield_2_mg = take();
        p.shield_2_eg = take();
        p.ks_kfile_semi_open_mg = take();
        p.ks_kfile_open_mg = take();
        p.ks_adj_semi_open_mg = take();
        p.ks_both_adj_semi_open_mg = take();

        for k in 3..6 {
            p.outpost_knight_mg[k] = take();
        }
        for k in 3..6 {
            p.outpost_knight_eg[k] = take();
        }
        for k in 3..6 {
            p.outpost_bishop_mg[k] = take();
        }
        for k in 3..6 {
            p.outpost_bishop_eg[k] = take();
        }

        p.rook_open_file_mg = take();
        p.rook_open_file_eg = take();
        p.rook_semi_open_file_mg = take();
        p.rook_semi_open_file_eg = take();

        debug_assert_eq!(i, N_CORE);
        p
    }

    /// Integer-quantized mirror for `apply` / reference scoring. Core fields are
    /// rounded; non-core fields carry through unchanged.
    pub fn to_ints(&self) -> EvalParamsI32 {
        EvalParamsI32 {
            iso_mg: r(self.iso_mg),
            iso_eg: r(self.iso_eg),
            dbl_mg: r(self.dbl_mg),
            dbl_eg: r(self.dbl_eg),
            bwd_mg: r(self.bwd_mg),
            bwd_eg: r(self.bwd_eg),

            conn_mg: ri8(&self.conn_mg),
            conn_eg: ri8(&self.conn_eg),

            passed_mg: ri8(&self.passed_mg),
            passed_eg: ri8(&self.passed_eg),
            passed_free_eg_delta: ri8(&self.passed_free_eg_delta),
            passed_kdist_own_per_step: r(self.passed_kdist_own_per_step),
            passed_kdist_enemy_per_step: r(self.passed_kdist_enemy_per_step),

            knight_mobility_mg: ri9(&self.knight_mobility_mg),
            knight_mobility_eg: ri9(&self.knight_mobility_eg),
            bishop_mobility_mg: ri14(&self.bishop_mobility_mg),
            bishop_mobility_eg: ri14(&self.bishop_mobility_eg),
            rook_mobility_mg: ri15(&self.rook_mobility_mg),
            rook_mobility_eg: ri15(&self.rook_mobility_eg),
            queen_mobility_mg: ri28(&self.queen_mobility_mg),
            queen_mobility_eg: ri28(&self.queen_mobility_eg),

            shield_1_mg: r(self.shield_1_mg),
            shield_1_eg: r(self.shield_1_eg),
            shield_2_mg: r(self.shield_2_mg),
            shield_2_eg: r(self.shield_2_eg),
            ks_kfile_semi_open_mg: r(self.ks_kfile_semi_open_mg),
            ks_kfile_open_mg: r(self.ks_kfile_open_mg),
            ks_adj_semi_open_mg: r(self.ks_adj_semi_open_mg),
            ks_both_adj_semi_open_mg: r(self.ks_both_adj_semi_open_mg),

            outpost_knight_mg: ri8(&self.outpost_knight_mg),
            outpost_knight_eg: ri8(&self.outpost_knight_eg),
            outpost_bishop_mg: ri8(&self.outpost_bishop_mg),
            outpost_bishop_eg: ri8(&self.outpost_bishop_eg),

            rook_open_file_mg: r(self.rook_open_file_mg),
            rook_open_file_eg: r(self.rook_open_file_eg),
            rook_semi_open_file_mg: r(self.rook_semi_open_file_mg),
            rook_semi_open_file_eg: r(self.rook_semi_open_file_eg),

            king_attack_weight_n: r(self.king_attack_weight_n),
            king_attack_weight_b: r(self.king_attack_weight_b),
            king_attack_weight_r: r(self.king_attack_weight_r),
            king_attack_weight_q: r(self.king_attack_weight_q),
            king_safety_table: ri100(&self.king_safety_table),

            ocb_with_pawns_scale: r(self.ocb_with_pawns_scale),
            pawnless_draw_scale: r(self.pawnless_draw_scale),
            fifty_move_floor: r(self.fifty_move_floor),
        }
    }
}

/// Field-for-field integer mirror of [`EvalParams`] — the serialized +
/// `apply` artifact.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct EvalParamsI32 {
    /// See [`EvalParams::iso_mg`].
    pub iso_mg: i32,
    /// See [`EvalParams::iso_eg`].
    pub iso_eg: i32,
    /// See [`EvalParams::dbl_mg`].
    pub dbl_mg: i32,
    /// See [`EvalParams::dbl_eg`].
    pub dbl_eg: i32,
    /// See [`EvalParams::bwd_mg`].
    pub bwd_mg: i32,
    /// See [`EvalParams::bwd_eg`].
    pub bwd_eg: i32,

    /// See [`EvalParams::conn_mg`].
    pub conn_mg: [i32; 8],
    /// See [`EvalParams::conn_eg`].
    pub conn_eg: [i32; 8],

    /// See [`EvalParams::passed_mg`].
    pub passed_mg: [i32; 8],
    /// See [`EvalParams::passed_eg`].
    pub passed_eg: [i32; 8],
    /// See [`EvalParams::passed_free_eg_delta`].
    pub passed_free_eg_delta: [i32; 8],
    /// See [`EvalParams::passed_kdist_own_per_step`].
    pub passed_kdist_own_per_step: i32,
    /// See [`EvalParams::passed_kdist_enemy_per_step`].
    pub passed_kdist_enemy_per_step: i32,

    /// See [`EvalParams::knight_mobility_mg`].
    pub knight_mobility_mg: [i32; 9],
    /// See [`EvalParams::knight_mobility_eg`].
    pub knight_mobility_eg: [i32; 9],
    /// See [`EvalParams::bishop_mobility_mg`].
    pub bishop_mobility_mg: [i32; 14],
    /// See [`EvalParams::bishop_mobility_eg`].
    pub bishop_mobility_eg: [i32; 14],
    /// See [`EvalParams::rook_mobility_mg`].
    pub rook_mobility_mg: [i32; 15],
    /// See [`EvalParams::rook_mobility_eg`].
    pub rook_mobility_eg: [i32; 15],
    /// See [`EvalParams::queen_mobility_mg`].
    pub queen_mobility_mg: [i32; 28],
    /// See [`EvalParams::queen_mobility_eg`].
    pub queen_mobility_eg: [i32; 28],

    /// See [`EvalParams::shield_1_mg`].
    pub shield_1_mg: i32,
    /// See [`EvalParams::shield_1_eg`].
    pub shield_1_eg: i32,
    /// See [`EvalParams::shield_2_mg`].
    pub shield_2_mg: i32,
    /// See [`EvalParams::shield_2_eg`].
    pub shield_2_eg: i32,
    /// See [`EvalParams::ks_kfile_semi_open_mg`].
    pub ks_kfile_semi_open_mg: i32,
    /// See [`EvalParams::ks_kfile_open_mg`].
    pub ks_kfile_open_mg: i32,
    /// See [`EvalParams::ks_adj_semi_open_mg`].
    pub ks_adj_semi_open_mg: i32,
    /// See [`EvalParams::ks_both_adj_semi_open_mg`].
    pub ks_both_adj_semi_open_mg: i32,

    /// See [`EvalParams::outpost_knight_mg`].
    pub outpost_knight_mg: [i32; 8],
    /// See [`EvalParams::outpost_knight_eg`].
    pub outpost_knight_eg: [i32; 8],
    /// See [`EvalParams::outpost_bishop_mg`].
    pub outpost_bishop_mg: [i32; 8],
    /// See [`EvalParams::outpost_bishop_eg`].
    pub outpost_bishop_eg: [i32; 8],

    /// See [`EvalParams::rook_open_file_mg`].
    pub rook_open_file_mg: i32,
    /// See [`EvalParams::rook_open_file_eg`].
    pub rook_open_file_eg: i32,
    /// See [`EvalParams::rook_semi_open_file_mg`].
    pub rook_semi_open_file_mg: i32,
    /// See [`EvalParams::rook_semi_open_file_eg`].
    pub rook_semi_open_file_eg: i32,

    /// See [`EvalParams::king_attack_weight_n`].
    pub king_attack_weight_n: i32,
    /// See [`EvalParams::king_attack_weight_b`].
    pub king_attack_weight_b: i32,
    /// See [`EvalParams::king_attack_weight_r`].
    pub king_attack_weight_r: i32,
    /// See [`EvalParams::king_attack_weight_q`].
    pub king_attack_weight_q: i32,
    /// See [`EvalParams::king_safety_table`]. Serialized as a length-100
    /// sequence (serde's array `impl` stops at 32; ADR-0037 §9 schema).
    #[serde(with = "table100")]
    pub king_safety_table: [i32; 100],

    /// See [`EvalParams::ocb_with_pawns_scale`].
    pub ocb_with_pawns_scale: i32,
    /// See [`EvalParams::pawnless_draw_scale`].
    pub pawnless_draw_scale: i32,
    /// See [`EvalParams::fifty_move_floor`].
    pub fifty_move_floor: i32,
}

/// Optimizer stopping constants, recorded in the params file (ADR-0037 §10).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct StoppingConfig {
    /// Held-out plateau patience (epochs without improvement before stop).
    pub patience: u32,
    /// Deterministic max-iteration backstop.
    pub max_iter: u64,
    /// Evaluate the held-out objective every this many iterations.
    pub eval_every: u64,
    /// Integer-quantization floor: stop once int-rounded weights are stable for
    /// this many consecutive checks.
    pub quant_stable_checks: u32,
}

/// One parameter's diagnostic sensitivity entry (recorded in all outcomes,
/// ADR-0037 §9).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ParamSensitivity {
    /// Human-readable parameter name (e.g. `"knight_mobility_mg[4]"`).
    pub name: String,
    /// Loss gradient at the endpoint.
    pub grad: f64,
    /// Approximate SPRT-margin sensitivity (±10% perturbation loss delta).
    pub sprt_margin_sens: f64,
}

/// `bench/m6-params.json` — the full tuner output artifact (ADR-0037 §9).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ParamsFile {
    /// Schema version; [`read_params_file`] rejects a mismatch.
    pub schema_version: u32,
    /// The tuned integer weights.
    pub params: EvalParamsI32,
    /// Fitted Texel scaling constant K.
    pub k: f64,
    /// Selected lane mixture proportions (4 sources, ADR-0037 §8).
    pub mix: [f64; 4],
    /// Stopping constants used.
    pub stopping: StoppingConfig,
    /// Per-parameter sensitivity table.
    pub sensitivity: Vec<ParamSensitivity>,
    /// Consumed lane sha256s (provenance cross-check, M6.G discipline).
    pub lane_sha256: [String; 4],
    /// Tuning RNG seed.
    pub seed: u64,
}

/// Serializes `p` to pretty JSON and writes it to `path`.
pub fn write_params_file(p: &ParamsFile, path: &Path) -> Result<(), TexelError> {
    let json = serde_json::to_string_pretty(p).map_err(|e| TexelError::Json(e.to_string()))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Reads + parses `bench/m6-params.json`. Rejects a [`SCHEMA_VERSION`] mismatch
/// with [`TexelError::Parse`] (ADR-0037 §9 reproducibility).
pub fn read_params_file(path: &Path) -> Result<ParamsFile, TexelError> {
    let bytes = std::fs::read(path)?;
    let pf: ParamsFile =
        serde_json::from_slice(&bytes).map_err(|e| TexelError::Json(e.to_string()))?;
    if pf.schema_version != SCHEMA_VERSION {
        return Err(TexelError::Parse(format!(
            "params schema_version {} != supported {SCHEMA_VERSION}",
            pf.schema_version
        )));
    }
    Ok(pf)
}

/// Rewrites the marker-delimited (`// TEXEL-TUNABLE-BEGIN/END: <group>`) `const`
/// regions in `data_rs` from `p` (structural region replacement; ADR-0037 §9).
///
/// Inside every marked region each `const NAME: TYPE = <expr>;` has its RHS
/// rewritten from `p` (keyed by const name), preserving the const name, type,
/// doc comments, and attributes so the file still compiles. Bytes OUTSIDE the
/// markers (material, the 12 PSTs, the frozen structural constants) are
/// byte-identical. A group may span MULTIPLE non-contiguous regions (the `scale`
/// group is split into the OCB/PAWNLESS region and the FIFTY_MOVE_FLOOR region);
/// every region is rewritten. The rewrite is a deterministic function of `p`, so
/// re-applying the same params is idempotent.
pub fn apply_to_data_rs(p: &EvalParamsI32, data_rs: &Path) -> Result<(), TexelError> {
    let src = std::fs::read_to_string(data_rs)?;
    let values = const_value_map(p);
    let mut out = String::with_capacity(src.len());

    let mut cursor = 0usize;
    while let Some(begin_rel) = src[cursor..].find("// TEXEL-TUNABLE-BEGIN:") {
        let begin = cursor + begin_rel;
        // The marker line ends at the next newline (the marker itself is frozen).
        let begin_eol = src[begin..]
            .find('\n')
            .map(|n| begin + n + 1)
            .ok_or_else(|| TexelError::Parse("BEGIN marker without newline".to_string()))?;
        // Everything up to and including the BEGIN marker line is copied verbatim.
        out.push_str(&src[cursor..begin_eol]);

        // Find the matching END marker line.
        let group = src[begin..begin_eol]
            .trim_start()
            .strip_prefix("// TEXEL-TUNABLE-BEGIN:")
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let end_needle = format!("// TEXEL-TUNABLE-END: {group}");
        let end_rel = src[begin_eol..].find(&end_needle).ok_or_else(|| {
            TexelError::Parse(format!("BEGIN marker for {group:?} has no matching END"))
        })?;
        let end = begin_eol + end_rel;

        // Rewrite the const RHS values inside the region body.
        let body = &src[begin_eol..end];
        out.push_str(&rewrite_region_body(body, &values)?);

        // Resume copying from the END marker line (the END marker is frozen).
        cursor = end;
    }
    out.push_str(&src[cursor..]);

    std::fs::write(data_rs, out)?;
    Ok(())
}

/// Rewrites each `const NAME: TYPE = <expr>;` RHS in `body` using `values`
/// (keyed by const name). A const whose name is not in `values` is left
/// untouched (defensive — only tunable consts live in marked regions).
fn rewrite_region_body(
    body: &str,
    values: &std::collections::HashMap<&'static str, String>,
) -> Result<String, TexelError> {
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("const ") {
        let kw = cursor + rel;
        out.push_str(&body[cursor..kw]);
        // Parse `const NAME:` — NAME is up to the colon.
        let after_kw = kw + "const ".len();
        let colon_rel = body[after_kw..].find(':').ok_or_else(|| {
            TexelError::Parse("const declaration without a type colon".to_string())
        })?;
        let name = body[after_kw..after_kw + colon_rel].trim().to_string();
        // The RHS runs from the `=` after the type to the terminating `;`.
        let eq_rel = body[after_kw..]
            .find('=')
            .ok_or_else(|| TexelError::Parse(format!("const {name} without an = initializer")))?;
        let eq = after_kw + eq_rel;
        // The statement-terminating `;` is the first `;` at bracket depth 0 after
        // the `=` — an array-repeat initializer like `[0; 8]` carries an inner
        // `;` that must NOT be mistaken for the terminator.
        let semi = {
            let mut depth = 0i32;
            let bytes = body.as_bytes();
            let mut j = eq + 1;
            loop {
                if j >= body.len() {
                    return Err(TexelError::Parse(format!(
                        "const {name} not terminated by ;"
                    )));
                }
                match bytes[j] {
                    b'[' | b'(' | b'{' => depth += 1,
                    b']' | b')' | b'}' => depth -= 1,
                    b';' if depth == 0 => break j,
                    _ => {}
                }
                j += 1;
            }
        };

        // Copy `const NAME: TYPE = ` verbatim (through the `= `), then the new
        // value, then the `;` and any trailing same-line comment.
        out.push_str(&body[kw..eq + 1]); // includes the `=`
        if let Some(val) = values.get(name.as_str()) {
            out.push(' ');
            out.push_str(val);
        } else {
            // Untouched const: keep the original RHS.
            out.push_str(&body[eq + 1..semi]);
        }
        out.push(';');
        cursor = semi + 1;
    }
    out.push_str(&body[cursor..]);
    Ok(out)
}

/// Maps each tunable `const` name to its canonical RHS value string from `p`
/// (scalars as integers; arrays as `[v0, v1, …]`). The names + element counts
/// mirror `eval::data` exactly so the rewritten file compiles.
fn const_value_map(p: &EvalParamsI32) -> std::collections::HashMap<&'static str, String> {
    let mut m: std::collections::HashMap<&'static str, String> = std::collections::HashMap::new();
    let scalar = |v: i32| v.to_string();
    let array = |a: &[i32]| {
        let inner = a.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
        format!("[{inner}]")
    };

    m.insert("ISO_MG", scalar(p.iso_mg));
    m.insert("ISO_EG", scalar(p.iso_eg));
    m.insert("DBL_MG", scalar(p.dbl_mg));
    m.insert("DBL_EG", scalar(p.dbl_eg));
    m.insert("BWD_MG", scalar(p.bwd_mg));
    m.insert("BWD_EG", scalar(p.bwd_eg));

    m.insert("CONN_MG", array(&p.conn_mg));
    m.insert("CONN_EG", array(&p.conn_eg));

    m.insert("PASSED_MG", array(&p.passed_mg));
    m.insert("PASSED_EG", array(&p.passed_eg));
    m.insert("PASSED_FREE_EG_DELTA", array(&p.passed_free_eg_delta));
    m.insert(
        "PASSED_KDIST_OWN_PER_STEP",
        scalar(p.passed_kdist_own_per_step),
    );
    m.insert(
        "PASSED_KDIST_ENEMY_PER_STEP",
        scalar(p.passed_kdist_enemy_per_step),
    );

    m.insert("KNIGHT_MOBILITY_MG", array(&p.knight_mobility_mg));
    m.insert("KNIGHT_MOBILITY_EG", array(&p.knight_mobility_eg));
    m.insert("BISHOP_MOBILITY_MG", array(&p.bishop_mobility_mg));
    m.insert("BISHOP_MOBILITY_EG", array(&p.bishop_mobility_eg));
    m.insert("ROOK_MOBILITY_MG", array(&p.rook_mobility_mg));
    m.insert("ROOK_MOBILITY_EG", array(&p.rook_mobility_eg));
    m.insert("QUEEN_MOBILITY_MG", array(&p.queen_mobility_mg));
    m.insert("QUEEN_MOBILITY_EG", array(&p.queen_mobility_eg));

    m.insert("SHIELD_1_MG", scalar(p.shield_1_mg));
    m.insert("SHIELD_1_EG", scalar(p.shield_1_eg));
    m.insert("SHIELD_2_MG", scalar(p.shield_2_mg));
    m.insert("SHIELD_2_EG", scalar(p.shield_2_eg));
    m.insert("KS_KFILE_SEMI_OPEN_MG", scalar(p.ks_kfile_semi_open_mg));
    m.insert("KS_KFILE_OPEN_MG", scalar(p.ks_kfile_open_mg));
    m.insert("KS_ADJ_SEMI_OPEN_MG", scalar(p.ks_adj_semi_open_mg));
    m.insert(
        "KS_BOTH_ADJ_SEMI_OPEN_MG",
        scalar(p.ks_both_adj_semi_open_mg),
    );

    m.insert("OUTPOST_KNIGHT_MG", array(&p.outpost_knight_mg));
    m.insert("OUTPOST_KNIGHT_EG", array(&p.outpost_knight_eg));
    m.insert("OUTPOST_BISHOP_MG", array(&p.outpost_bishop_mg));
    m.insert("OUTPOST_BISHOP_EG", array(&p.outpost_bishop_eg));

    m.insert("ROOK_OPEN_FILE_MG", scalar(p.rook_open_file_mg));
    m.insert("ROOK_OPEN_FILE_EG", scalar(p.rook_open_file_eg));
    m.insert("ROOK_SEMI_OPEN_FILE_MG", scalar(p.rook_semi_open_file_mg));
    m.insert("ROOK_SEMI_OPEN_FILE_EG", scalar(p.rook_semi_open_file_eg));

    m.insert("OCB_WITH_PAWNS_SCALE", scalar(p.ocb_with_pawns_scale));
    m.insert("PAWNLESS_DRAW_SCALE", scalar(p.pawnless_draw_scale));
    m.insert("FIFTY_MOVE_FLOOR", scalar(p.fifty_move_floor));

    m
}

/// Serde adapter for `[i32; 100]` (the S-curve table): serde's built-in array
/// impls stop at length 32, so it (de)serializes as a length-100 sequence and
/// rejects any other length.
mod table100 {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(t: &[i32; 100], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_tuple(100)?;
        for v in t {
            seq.serialize_element(v)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[i32; 100], D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = [i32; 100];
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a sequence of 100 i32 values")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = [0i32; 100];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                if seq.next_element::<i32>()?.is_some() {
                    return Err(A::Error::invalid_length(101, &self));
                }
                Ok(out)
            }
        }
        d.deserialize_tuple(100, V)
    }
}

// ---------------------------------------------------------------------------
// Small array-conversion helpers (kept local; no abstraction surface).
// ---------------------------------------------------------------------------

fn arr8(src: &[i32; 8]) -> [f64; 8] {
    arr(src)
}

fn arr<const N: usize>(src: &[i32; N]) -> [f64; N] {
    std::array::from_fn(|i| src[i] as f64)
}

fn r(x: f64) -> i32 {
    x.round() as i32
}

fn ri8(src: &[f64; 8]) -> [i32; 8] {
    ri(src)
}

fn ri9(src: &[f64; 9]) -> [i32; 9] {
    ri(src)
}

fn ri14(src: &[f64; 14]) -> [i32; 14] {
    ri(src)
}

fn ri15(src: &[f64; 15]) -> [i32; 15] {
    ri(src)
}

fn ri28(src: &[f64; 28]) -> [i32; 28] {
    ri(src)
}

fn ri100(src: &[f64; 100]) -> [i32; 100] {
    ri(src)
}

fn ri<const N: usize>(src: &[f64; N]) -> [i32; N] {
    std::array::from_fn(|i| r(src[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_core_vec_has_n_core_len() {
        assert_eq!(EvalParams::shipped().core_to_vec().len(), N_CORE);
    }

    #[test]
    fn core_vec_roundtrip() {
        let base = EvalParams::shipped();
        let v = base.core_to_vec();
        let rebuilt = base.with_core(&v);
        assert_eq!(base, rebuilt);
    }

    #[test]
    fn conn_is_live_other_core_groups_zero_at_shipped() {
        let s = EvalParams::shipped();
        // CONN is the one live group (M6.F production state).
        assert!(
            s.conn_mg[2..8].iter().any(|&x| x != 0.0),
            "CONN_MG should be live"
        );
        assert!(
            s.conn_eg[2..8].iter().any(|&x| x != 0.0),
            "CONN_EG should be live"
        );
        // Everything else in the core is zero.
        assert_eq!(s.iso_mg, 0.0);
        assert!(s.passed_mg.iter().all(|&x| x == 0.0));
        assert!(s.knight_mobility_mg.iter().all(|&x| x == 0.0));
        assert_eq!(s.shield_1_mg, 0.0);
        assert!(s.outpost_knight_mg.iter().all(|&x| x == 0.0));
        assert_eq!(s.rook_open_file_mg, 0.0);
    }

    #[test]
    fn with_core_keeps_non_core_fields() {
        let s = EvalParams::shipped();
        let mut v = s.core_to_vec();
        // Perturb a core entry.
        v[0] = 42.0;
        let p = s.with_core(&v);
        // Non-core (scale numerators, S-curve, multipliers) unchanged.
        assert_eq!(p.ocb_with_pawns_scale, s.ocb_with_pawns_scale);
        assert_eq!(p.king_safety_table, s.king_safety_table);
        assert_eq!(p.king_attack_weight_q, s.king_attack_weight_q);
        // Core entry applied.
        assert_eq!(p.iso_mg, 42.0);
    }

    #[test]
    fn to_ints_rounds_core_carries_non_core() {
        let mut s = EvalParams::shipped();
        s.iso_mg = 2.6;
        s.knight_mobility_mg[3] = -1.4;
        let ints = s.to_ints();
        assert_eq!(ints.iso_mg, 3);
        assert_eq!(ints.knight_mobility_mg[3], -1);
        // Non-core carried (scale numerators at shipped identity = 64).
        assert_eq!(
            ints.ocb_with_pawns_scale,
            s.ocb_with_pawns_scale.round() as i32
        );
    }

    // -----------------------------------------------------------------------
    // Single-source-of-truth pin: shipped() == eval::data consts.
    // -----------------------------------------------------------------------

    /// `EvalParams::shipped()` reads the LIVE `eval::data` consts field-for-
    /// field. A const edit (or a transcription bug in `shipped()`) fails here
    /// — `shipped()` is the single source of truth (plan §params.rs).
    #[test]
    fn shipped_params_match_data_consts() {
        let s = EvalParams::shipped();
        // Scalars.
        assert_eq!(s.iso_mg, data::ISO_MG as f64);
        assert_eq!(s.iso_eg, data::ISO_EG as f64);
        assert_eq!(s.dbl_mg, data::DBL_MG as f64);
        assert_eq!(s.bwd_eg, data::BWD_EG as f64);
        assert_eq!(
            s.passed_kdist_own_per_step,
            data::PASSED_KDIST_OWN_PER_STEP as f64
        );
        assert_eq!(s.rook_open_file_mg, data::ROOK_OPEN_FILE_MG as f64);
        assert_eq!(s.shield_1_mg, data::SHIELD_1_MG as f64);
        // Arrays (spot-check full equality on the live + several zeroed ones).
        for k in 0..8 {
            assert_eq!(s.conn_mg[k], data::CONN_MG[k] as f64, "conn_mg[{k}]");
            assert_eq!(s.conn_eg[k], data::CONN_EG[k] as f64, "conn_eg[{k}]");
            assert_eq!(s.passed_mg[k], data::PASSED_MG[k] as f64, "passed_mg[{k}]");
            assert_eq!(
                s.outpost_knight_mg[k],
                data::OUTPOST_KNIGHT_MG[k] as f64,
                "outpost_knight_mg[{k}]"
            );
        }
        for k in 0..9 {
            assert_eq!(
                s.knight_mobility_mg[k],
                data::KNIGHT_MOBILITY_MG[k] as f64,
                "knight_mobility_mg[{k}]"
            );
        }
        for k in 0..28 {
            assert_eq!(
                s.queen_mobility_eg[k],
                data::QUEEN_MOBILITY_EG[k] as f64,
                "queen_mobility_eg[{k}]"
            );
        }
        // Non-core (excluded / outer) fields.
        assert_eq!(s.king_attack_weight_q, data::KING_ATTACK_WEIGHT_Q as f64);
        assert_eq!(s.king_safety_table[0], data::KING_SAFETY_TABLE[0] as f64);
        assert_eq!(s.ocb_with_pawns_scale, data::OCB_WITH_PAWNS_SCALE as f64);
        assert_eq!(s.fifty_move_floor, data::FIFTY_MOVE_FLOOR as f64);
    }

    // -----------------------------------------------------------------------
    // Core-index map covers all of 0..N_CORE with no overlap.
    //
    // The layout-side invariant is pinned by
    // `layout::tests::layout_ranges_contiguous_no_overlap_cover_n_core`. Here
    // we pin the params-side contract: every core vec slot is touched exactly
    // once by `with_core` (the `take()` cursor reaches N_CORE), and a perturbed
    // unique value at each slot round-trips back to the SAME slot — i.e. no two
    // core fields alias the same vec index.
    // -----------------------------------------------------------------------

    /// Each of the `N_CORE` vec slots maps to a distinct `EvalParams` field:
    /// writing a unique value `1000 + i` at slot `i` via `with_core`, then
    /// reading the core vec back, recovers `1000 + i` at slot `i` for every `i`.
    /// A field aliasing two slots (or a missed slot) breaks the identity.
    #[test]
    fn core_index_map_covers_all_core_no_overlap() {
        let base = EvalParams::shipped();
        let unique: Vec<f64> = (0..N_CORE).map(|i| 1000.0 + i as f64).collect();
        let p = base.with_core(&unique);
        let back = p.core_to_vec();
        assert_eq!(back.len(), N_CORE);
        for i in 0..N_CORE {
            assert_eq!(
                back[i], unique[i],
                "core slot {i} did not round-trip — field aliasing or gap"
            );
        }
    }

    // -----------------------------------------------------------------------
    // JSON round-trips.
    // -----------------------------------------------------------------------

    /// `ParamsFile` serializes + deserializes losslessly (incl. the length-100
    /// S-curve table via the custom serde adapter).
    #[test]
    fn params_json_roundtrip() {
        let mut ints = EvalParams::shipped().to_ints();
        // Perturb a few fields (incl. a high S-curve index past serde's 32 cap).
        ints.iso_mg = -7;
        ints.knight_mobility_mg[4] = 13;
        ints.king_safety_table[99] = 42;
        let pf = ParamsFile {
            schema_version: SCHEMA_VERSION,
            params: ints,
            k: 0.012_345,
            mix: [0.25, 0.25, 0.25, 0.25],
            stopping: StoppingConfig {
                patience: 5,
                max_iter: 10_000,
                eval_every: 100,
                quant_stable_checks: 3,
            },
            sensitivity: vec![ParamSensitivity {
                name: "iso_mg".to_string(),
                grad: -0.5,
                sprt_margin_sens: 0.1,
            }],
            lane_sha256: [
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
            seed: 0xDEAD_BEEF,
        };
        let json = serde_json::to_string(&pf).expect("serialize");
        let back: ParamsFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back, pf,
            "ParamsFile must round-trip through JSON losslessly"
        );
        assert_eq!(
            back.params.king_safety_table[99], 42,
            "len-100 table slot 99"
        );
    }

    /// JSON round-trips an `EvalParamsI32` whose array fields carry DISTINCT,
    /// NON-ZERO, index-DEPENDENT values (`f(i) = i − 3` and friends), built by
    /// `to_ints()` from a float `EvalParams`. Asserts field-for-field equality.
    ///
    /// This is the value-discriminating complement to `params_json_roundtrip`
    /// (which round-trips the mostly-zero shipped defaults): a constant-array
    /// mutation of the `riN`/`ri8`/`ri100` conversion helpers (`replace … with
    /// [0; N]` / `[1; N]` / `[-1; N]`) cannot reproduce these index-dependent
    /// values, and the serde array (de)ser of the populated fields is exercised
    /// across both the ≤32 derive arrays and the length-100 `table100` adapter.
    #[test]
    fn params_json_roundtrip_nonzero_arrays() {
        // Populate the float params with distinct, index-dependent values so the
        // integer mirror is non-constant per array.
        let mut p = EvalParams::shipped();
        let fill = |a: &mut [f64], base: i32| {
            for (i, slot) in a.iter_mut().enumerate() {
                *slot = (base + i as i32) as f64;
            }
        };
        fill(&mut p.knight_mobility_mg, -4);
        fill(&mut p.knight_mobility_eg, 2);
        fill(&mut p.bishop_mobility_mg, -7);
        fill(&mut p.bishop_mobility_eg, 5);
        fill(&mut p.rook_mobility_mg, -8);
        fill(&mut p.rook_mobility_eg, 3);
        fill(&mut p.queen_mobility_mg, -14);
        fill(&mut p.queen_mobility_eg, 6);
        fill(&mut p.conn_mg, -3);
        fill(&mut p.conn_eg, 4);
        fill(&mut p.passed_mg, -2);
        fill(&mut p.passed_eg, 1);
        fill(&mut p.passed_free_eg_delta, -5);
        fill(&mut p.outpost_knight_mg, -1);
        fill(&mut p.outpost_knight_eg, 2);
        fill(&mut p.outpost_bishop_mg, -3);
        fill(&mut p.outpost_bishop_eg, 4);
        // The length-100 king-safety table (past serde's 32-array derive cap).
        fill(&mut p.king_safety_table, -50);

        let ints = p.to_ints();

        // Independently-computed expected integer arrays: `fill(a, base)` sets
        // `a[i] = base + i`, and `(base + i) as f64` rounds back to `base + i`,
        // so the integer mirror MUST be exactly `[base, base+1, …]`. Comparing
        // against these hand-derived arrays — NOT against a `to_ints()`
        // re-derivation — breaks the self-consistency tautology: a constant-array
        // mutant of the `riN`/`ri8`/`ri100` helper (`[0;N]`/`[1;N]`/`[-1;N]`)
        // changes `to_ints()` AND would still "round-trip", but it cannot match
        // these independent index-dependent expectations.
        let expect =
            |base: i32, n: usize| -> Vec<i32> { (0..n).map(|i| base + i as i32).collect() };
        assert_eq!(
            ints.knight_mobility_mg.to_vec(),
            expect(-4, 9),
            "ri9 knight_mg"
        );
        assert_eq!(
            ints.knight_mobility_eg.to_vec(),
            expect(2, 9),
            "ri9 knight_eg"
        );
        assert_eq!(
            ints.bishop_mobility_mg.to_vec(),
            expect(-7, 14),
            "ri14 bishop_mg"
        );
        assert_eq!(
            ints.bishop_mobility_eg.to_vec(),
            expect(5, 14),
            "ri14 bishop_eg"
        );
        assert_eq!(
            ints.rook_mobility_mg.to_vec(),
            expect(-8, 15),
            "ri15 rook_mg"
        );
        assert_eq!(
            ints.rook_mobility_eg.to_vec(),
            expect(3, 15),
            "ri15 rook_eg"
        );
        assert_eq!(
            ints.queen_mobility_mg.to_vec(),
            expect(-14, 28),
            "ri28 queen_mg"
        );
        assert_eq!(
            ints.queen_mobility_eg.to_vec(),
            expect(6, 28),
            "ri28 queen_eg"
        );
        assert_eq!(ints.conn_mg.to_vec(), expect(-3, 8), "ri8 conn_mg");
        assert_eq!(ints.conn_eg.to_vec(), expect(4, 8), "ri8 conn_eg");
        assert_eq!(ints.passed_mg.to_vec(), expect(-2, 8), "ri8 passed_mg");
        assert_eq!(ints.passed_eg.to_vec(), expect(1, 8), "ri8 passed_eg");
        assert_eq!(
            ints.passed_free_eg_delta.to_vec(),
            expect(-5, 8),
            "ri8 passed_free"
        );
        assert_eq!(
            ints.outpost_knight_mg.to_vec(),
            expect(-1, 8),
            "ri8 outpost_n_mg"
        );
        assert_eq!(
            ints.outpost_knight_eg.to_vec(),
            expect(2, 8),
            "ri8 outpost_n_eg"
        );
        assert_eq!(
            ints.outpost_bishop_mg.to_vec(),
            expect(-3, 8),
            "ri8 outpost_b_mg"
        );
        assert_eq!(
            ints.outpost_bishop_eg.to_vec(),
            expect(4, 8),
            "ri8 outpost_b_eg"
        );
        assert_eq!(
            ints.king_safety_table.to_vec(),
            expect(-50, 100),
            "ri100 king_safety"
        );

        let pf = ParamsFile {
            schema_version: SCHEMA_VERSION,
            params: ints.clone(),
            k: 0.020_5,
            mix: [0.4, 0.3, 0.2, 0.1],
            stopping: StoppingConfig {
                patience: 7,
                max_iter: 5_000,
                eval_every: 50,
                quant_stable_checks: 2,
            },
            sensitivity: Vec::new(),
            lane_sha256: [
                "w".to_string(),
                "x".to_string(),
                "y".to_string(),
                "z".to_string(),
            ],
            seed: 0x1234_5678,
        };

        let json = serde_json::to_string(&pf).expect("serialize");
        let back: ParamsFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, pf, "non-zero array params must round-trip losslessly");

        // Field-for-field spot-checks on the index-dependent contents so a
        // constant-array deser/convert mutant is unambiguously distinguishable.
        assert_eq!(back.params.knight_mobility_mg, ints.knight_mobility_mg);
        assert_eq!(back.params.knight_mobility_eg, ints.knight_mobility_eg);
        assert_eq!(back.params.bishop_mobility_mg, ints.bishop_mobility_mg);
        assert_eq!(back.params.bishop_mobility_eg, ints.bishop_mobility_eg);
        assert_eq!(back.params.rook_mobility_mg, ints.rook_mobility_mg);
        assert_eq!(back.params.rook_mobility_eg, ints.rook_mobility_eg);
        assert_eq!(back.params.queen_mobility_mg, ints.queen_mobility_mg);
        assert_eq!(back.params.queen_mobility_eg, ints.queen_mobility_eg);
        assert_eq!(back.params.conn_mg, ints.conn_mg);
        assert_eq!(back.params.conn_eg, ints.conn_eg);
        assert_eq!(back.params.passed_mg, ints.passed_mg);
        assert_eq!(back.params.passed_eg, ints.passed_eg);
        assert_eq!(back.params.passed_free_eg_delta, ints.passed_free_eg_delta);
        assert_eq!(back.params.outpost_knight_mg, ints.outpost_knight_mg);
        assert_eq!(back.params.outpost_knight_eg, ints.outpost_knight_eg);
        assert_eq!(back.params.outpost_bishop_mg, ints.outpost_bishop_mg);
        assert_eq!(back.params.outpost_bishop_eg, ints.outpost_bishop_eg);
        // The full length-100 table deserializes element-for-element to the
        // independent index-dependent expectation (kills a constant-array
        // `table100` deser mutant directly, not via a self-consistent compare).
        assert_eq!(back.params.king_safety_table.to_vec(), expect(-50, 100));
        for (i, &v) in back.params.king_safety_table.iter().enumerate() {
            assert_eq!(
                v,
                -50 + i as i32,
                "king_safety_table[{i}] index-dependent value"
            );
        }
    }

    /// `write_params_file` (pretty JSON) + `read_params_file` round-trips a
    /// `ParamsFile` losslessly — pins that the pretty-printed on-disk artifact
    /// reads back identically (the canonical deployment artifact, ADR-0037 §9).
    #[test]
    fn write_then_read_params_file_roundtrip() {
        let dir = unique_temp_dir("params-write-rt");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("m6-params.json");
        let mut ints = EvalParams::shipped().to_ints();
        ints.iso_mg = -7;
        ints.knight_mobility_mg[4] = 13;
        ints.king_safety_table[99] = 42;
        let pf = ParamsFile {
            schema_version: SCHEMA_VERSION,
            params: ints,
            k: 0.012_345_678,
            mix: [0.25, 0.25, 0.25, 0.25],
            stopping: StoppingConfig {
                patience: 5,
                max_iter: 10_000,
                eval_every: 100,
                quant_stable_checks: 3,
            },
            sensitivity: vec![ParamSensitivity {
                name: "iso_mg".to_string(),
                grad: -0.5,
                sprt_margin_sens: 0.1,
            }],
            lane_sha256: [
                "aa".to_string(),
                "bb".to_string(),
                "cc".to_string(),
                "dd".to_string(),
            ],
            seed: 0xDEAD_BEEF,
        };
        write_params_file(&pf, &path).expect("write");
        // Verify pretty-printed (human-readable) format was written.
        let raw = std::fs::read_to_string(&path).expect("read raw");
        assert!(
            raw.contains('\n'),
            "write_params_file must produce pretty-printed JSON (has newlines)"
        );
        let back = read_params_file(&path).expect("read back");
        assert_eq!(
            back, pf,
            "write_params_file + read_params_file must round-trip"
        );
        assert_eq!(
            back.k.to_bits(),
            pf.k.to_bits(),
            "k must be bit-identical through the JSON round-trip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `read_params_file` rejects a schema-version mismatch with
    /// `TexelError::Parse` (ADR-0037 §9 reproducibility).
    #[test]
    fn read_params_file_rejects_schema_mismatch() {
        let mut pf = ParamsFile {
            schema_version: SCHEMA_VERSION + 99,
            params: EvalParams::shipped().to_ints(),
            k: 0.01,
            mix: [0.25; 4],
            stopping: StoppingConfig {
                patience: 5,
                max_iter: 1,
                eval_every: 1,
                quant_stable_checks: 1,
            },
            sensitivity: Vec::new(),
            lane_sha256: [
                "".to_string(),
                "".to_string(),
                "".to_string(),
                "".to_string(),
            ],
            seed: 0,
        };
        let dir = unique_temp_dir("params-schema");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("m6-params.json");
        // Bad schema → Parse error.
        std::fs::write(&path, serde_json::to_string(&pf).unwrap()).unwrap();
        let err = read_params_file(&path).expect_err("schema mismatch must error");
        assert!(
            matches!(err, TexelError::Parse(_)),
            "expected TexelError::Parse on schema mismatch, got {err:?}"
        );
        // Correct schema → Ok (control).
        pf.schema_version = SCHEMA_VERSION;
        std::fs::write(&path, serde_json::to_string(&pf).unwrap()).unwrap();
        assert!(
            read_params_file(&path).is_ok(),
            "matching schema must parse"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // apply codegen: marker-region replacement.
    // -----------------------------------------------------------------------

    /// A minimal fixture mirroring `data.rs`'s TEXEL-TUNABLE marker structure:
    /// frozen (un-marked) blocks bracketing two tunable marker regions. `apply`
    /// must rewrite ONLY the bytes between matching BEGIN/END markers.
    const FIXTURE_DATA_RS: &str = "\
// frozen header — must stay byte-identical
pub(crate) const MATERIAL: [i32; 6] = [82, 337, 365, 477, 1025, 0];

// TEXEL-TUNABLE-BEGIN: iso_dbl_bwd
pub(crate) const ISO_MG: i32 = 0;
pub(crate) const ISO_EG: i32 = 0;
pub(crate) const DBL_MG: i32 = 0;
pub(crate) const DBL_EG: i32 = 0;
pub(crate) const BWD_MG: i32 = 0;
pub(crate) const BWD_EG: i32 = 0;
// TEXEL-TUNABLE-END: iso_dbl_bwd

// frozen middle — a PST block that must stay byte-identical
pub(crate) const MG_PAWN_PST: [i32; 4] = [1, 2, 3, 4];

// TEXEL-TUNABLE-BEGIN: rook_file
pub(crate) const ROOK_OPEN_FILE_MG: i32 = 0;
pub(crate) const ROOK_OPEN_FILE_EG: i32 = 0;
pub(crate) const ROOK_SEMI_OPEN_FILE_MG: i32 = 0;
pub(crate) const ROOK_SEMI_OPEN_FILE_EG: i32 = 0;
// TEXEL-TUNABLE-END: rook_file

// frozen footer — must stay byte-identical
pub(crate) const EG_SCALE_DEN: i32 = 64;
";

    /// The byte span strictly between a `BEGIN: <group>` marker line and its
    /// `END: <group>` marker line (the region `apply` is allowed to touch).
    fn between_markers(src: &str, group: &str) -> String {
        let begin = format!("// TEXEL-TUNABLE-BEGIN: {group}");
        let end = format!("// TEXEL-TUNABLE-END: {group}");
        let b = src.find(&begin).expect("begin marker") + begin.len();
        let e = src[b..].find(&end).expect("end marker") + b;
        src[b..e].to_string()
    }

    /// The bytes OUTSIDE every tunable region (everything not strictly between a
    /// matched BEGIN/END pair). These must be byte-identical after `apply`.
    fn frozen_regions(src: &str) -> Vec<String> {
        // The frozen prologue, the inter-region middle, and the epilogue.
        let mut out = Vec::new();
        let mut cursor = 0usize;
        for group in ["iso_dbl_bwd", "rook_file"] {
            let begin = format!("// TEXEL-TUNABLE-BEGIN: {group}");
            let end = format!("// TEXEL-TUNABLE-END: {group}");
            let b = src[cursor..].find(&begin).expect("begin") + cursor;
            // Everything from cursor up to AND INCLUDING the begin marker line
            // is frozen; everything from the end marker line onward continues.
            out.push(src[cursor..b + begin.len()].to_string());
            let region_end = src[b..].find(&end).expect("end") + b;
            cursor = region_end; // the END marker line itself is frozen
        }
        out.push(src[cursor..].to_string());
        out
    }

    /// `apply` rewrites ONLY the marker-delimited regions (incl. CONN-style
    /// tunable blocks): the frozen / PST blocks stay byte-identical, the marked
    /// regions change, and a second `apply` of the same params is idempotent.
    #[test]
    fn apply_rewrites_marked_regions_only() {
        let dir = unique_temp_dir("apply-marked");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("data.rs");
        std::fs::write(&path, FIXTURE_DATA_RS).expect("write fixture");

        let original = FIXTURE_DATA_RS.to_string();
        let frozen_before = frozen_regions(&original);

        // Tuned params: set ROOK_OPEN_FILE_MG to a non-zero value so the
        // rook_file region MUST change.
        let mut ints = EvalParams::shipped().to_ints();
        ints.rook_open_file_mg = 21;
        ints.iso_mg = -9;

        apply_to_data_rs(&ints, &path).expect("apply");
        let after = std::fs::read_to_string(&path).expect("read after apply");

        // Frozen regions byte-identical.
        let frozen_after = frozen_regions(&after);
        assert_eq!(
            frozen_after, frozen_before,
            "apply must NOT touch frozen / PST blocks outside the markers"
        );

        // Marked regions actually changed (the new value must appear).
        let rook_region = between_markers(&after, "rook_file");
        assert!(
            rook_region.contains("21"),
            "rook_file region must carry the tuned ROOK_OPEN_FILE_MG=21; got:\n{rook_region}"
        );
        let iso_region = between_markers(&after, "iso_dbl_bwd");
        assert!(
            iso_region.contains("-9"),
            "iso_dbl_bwd region must carry the tuned ISO_MG=-9; got:\n{iso_region}"
        );

        // Idempotent: applying the same params again is a no-op.
        let snapshot = std::fs::read_to_string(&path).unwrap();
        apply_to_data_rs(&ints, &path).expect("re-apply");
        let after2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after2, snapshot,
            "apply must be idempotent — re-applying the same params changes nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Golden: after `apply` writes tuned integer weights, re-parsing those
    /// const values reproduces the tuned `EvalParamsI32`. (Unit-level: parse
    /// the rewritten marked regions back to integers and compare — pins that
    /// `apply` writes the values it was given, in the right slots.)
    #[test]
    fn apply_then_reparse_matches_tuned_model() {
        let dir = unique_temp_dir("apply-reparse");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("data.rs");
        std::fs::write(&path, FIXTURE_DATA_RS).expect("write fixture");

        let mut ints = EvalParams::shipped().to_ints();
        ints.iso_mg = -11;
        ints.iso_eg = -7;
        ints.dbl_mg = -5;
        ints.rook_open_file_mg = 23;
        ints.rook_semi_open_file_eg = 4;

        apply_to_data_rs(&ints, &path).expect("apply");
        let after = std::fs::read_to_string(&path).expect("read");

        // Re-parse: extract each `const NAME: i32 = VALUE;` from the rewritten
        // regions and assert it equals the tuned integer we wrote.
        let parse_const = |src: &str, name: &str| -> i32 {
            let needle = format!("const {name}: i32 = ");
            let start = src
                .find(&needle)
                .unwrap_or_else(|| panic!("const {name} not found in rewritten data.rs"))
                + needle.len();
            let tail = &src[start..];
            let endrel = tail.find(';').expect("const terminated by ;");
            tail[..endrel].trim().parse::<i32>().unwrap_or_else(|e| {
                panic!("const {name} value not an i32: {:?} ({e})", &tail[..endrel])
            })
        };
        assert_eq!(parse_const(&after, "ISO_MG"), ints.iso_mg);
        assert_eq!(parse_const(&after, "ISO_EG"), ints.iso_eg);
        assert_eq!(parse_const(&after, "DBL_MG"), ints.dbl_mg);
        assert_eq!(
            parse_const(&after, "ROOK_OPEN_FILE_MG"),
            ints.rook_open_file_mg
        );
        assert_eq!(
            parse_const(&after, "ROOK_SEMI_OPEN_FILE_EG"),
            ints.rook_semi_open_file_eg
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fixture whose marked region carries BOTH an array-repeat initializer
    /// (`[v; N]`, with an inner `;` at bracket depth 1) and a regular array
    /// (`[1, 2, 3]`), plus a following tunable scalar that MUST stay intact. The
    /// array-repeat const is NOT tunable (`PADDING` is not a real eval const), so
    /// `rewrite_region_body` copies its RHS verbatim — but only if the
    /// bracket-depth `;`-tracking correctly skips the inner `;`. A broken depth
    /// guard would terminate the const at the inner `;`, corrupting the file.
    const FIXTURE_ARRAY_REPEAT: &str = "\
// frozen header
pub(crate) const MATERIAL: [i32; 3] = [82, 337, 365];

// TEXEL-TUNABLE-BEGIN: iso_dbl_bwd
pub(crate) const PADDING: [i32; 8] = [0; 8];
pub(crate) const MARKER: [i32; 3] = [1, 2, 3];
pub(crate) const ISO_MG: i32 = 0;
pub(crate) const ISO_EG: i32 = 0;
// TEXEL-TUNABLE-END: iso_dbl_bwd

// frozen footer
pub(crate) const EG_SCALE_DEN: i32 = 64;
";

    /// `apply` correctly handles an array-repeat initializer (`[v; N]`) inside a
    /// marked region: the inner `;` does NOT prematurely terminate the const, the
    /// non-tunable array-repeat/regular-array consts pass through byte-identical,
    /// the following tunable scalar (`ISO_MG`) is rewritten to its target, and a
    /// second apply is idempotent. Pins the bracket-depth `[`/`]` match arms, the
    /// `depth == 0` guard, and the `depth += / -=` tracking (category-D
    /// survivors). A depth mutant produces invalid Rust or a wrong value here.
    #[test]
    fn apply_handles_array_repeat_initializer_in_region() {
        let dir = unique_temp_dir("apply-array-repeat");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("data.rs");
        std::fs::write(&path, FIXTURE_ARRAY_REPEAT).expect("write fixture");

        let mut ints = EvalParams::shipped().to_ints();
        ints.iso_mg = 17; // non-zero target so the rewrite MUST fire
        ints.iso_eg = -4;

        apply_to_data_rs(&ints, &path).expect("apply");
        let after = std::fs::read_to_string(&path).expect("read after apply");

        // The array-repeat const passed through verbatim — the inner `;` was NOT
        // mistaken for the terminator (a depth bug would mangle this line).
        assert!(
            after.contains("pub(crate) const PADDING: [i32; 8] = [0; 8];"),
            "array-repeat const must survive byte-identical; got:\n{after}"
        );
        // The regular array const is untouched (not tunable).
        assert!(
            after.contains("pub(crate) const MARKER: [i32; 3] = [1, 2, 3];"),
            "regular-array const must survive byte-identical; got:\n{after}"
        );
        // The following tunable scalar was rewritten to its target value — i.e.
        // the cursor advanced PAST the array-repeat to the real const.
        let parse_scalar = |src: &str, name: &str| -> i32 {
            let needle = format!("const {name}: i32 = ");
            let start = src
                .find(&needle)
                .unwrap_or_else(|| panic!("const {name} not found:\n{src}"))
                + needle.len();
            let tail = &src[start..];
            let end = tail.find(';').expect("terminator");
            tail[..end].trim().parse::<i32>().unwrap_or_else(|e| {
                panic!("const {name} value not an i32: {:?} ({e})", &tail[..end])
            })
        };
        assert_eq!(parse_scalar(&after, "ISO_MG"), 17);
        assert_eq!(parse_scalar(&after, "ISO_EG"), -4);

        // The whole file must still reparse: the frozen footer is intact.
        assert!(
            after.contains("pub(crate) const EG_SCALE_DEN: i32 = 64;"),
            "frozen footer must remain intact"
        );

        // Idempotent.
        let snapshot = std::fs::read_to_string(&path).unwrap();
        apply_to_data_rs(&ints, &path).expect("re-apply");
        let after2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after2, snapshot, "apply must be idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `apply` rewrites a TUNABLE const whose fixture RHS is an array-repeat
    /// (`[0; 8]`, an inner `;` at bracket depth 1): the RHS is replaced by the
    /// full target tuple AND no leftover ` 8]` garbage trails it — the depth
    /// tracking found the STATEMENT-terminating `;` (depth 0), not the inner one.
    /// A FOLLOWING tunable scalar (`CONN_EG`-via-ISO here) and the frozen footer
    /// stay intact. This is the load-bearing category-D test: when the target
    /// const is itself an array-repeat, a broken `depth == 0` guard / `[`/`]`
    /// depth tracking terminates at the inner `;` and corrupts the output, which
    /// the EXACT-string assertion below detects.
    ///
    /// (The real `data.rs` array consts are explicit `[v0,…]` tuples, not
    /// array-repeats; but `apply` keys on the const NAME and rewrites whatever
    /// RHS span it finds, so a fixture array-repeat under a real const name
    /// exercises the depth tracking on the rewrite path.)
    #[test]
    fn apply_rewrites_tunable_array_repeat_in_region() {
        let fixture = "\
// TEXEL-TUNABLE-BEGIN: iso_dbl_bwd
pub(crate) const ISO_MG: [i32; 8] = [0; 8];
pub(crate) const ISO_EG: i32 = 0;
// TEXEL-TUNABLE-END: iso_dbl_bwd

// frozen footer
pub(crate) const EG_SCALE_DEN: i32 = 64;
";
        let dir = unique_temp_dir("apply-tunable-array-repeat");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("data.rs");
        std::fs::write(&path, fixture).expect("write fixture");

        // ISO_MG maps to a scalar value string, but the fixture's ISO_MG RHS is
        // an array-repeat with an inner `;` — the rewrite must replace the whole
        // ` [0; 8]` span (through bracket depth 0), leaving no ` 8]` tail.
        let mut ints = EvalParams::shipped().to_ints();
        ints.iso_mg = 42;
        ints.iso_eg = -3;

        apply_to_data_rs(&ints, &path).expect("apply");
        let after = std::fs::read_to_string(&path).expect("read");

        // EXACT region content — any trailing ` 8]` garbage (the inner-`;`
        // mis-termination) breaks this byte-for-byte equality.
        let expected = "\
// TEXEL-TUNABLE-BEGIN: iso_dbl_bwd
pub(crate) const ISO_MG: [i32; 8] = 42;
pub(crate) const ISO_EG: i32 = -3;
// TEXEL-TUNABLE-END: iso_dbl_bwd

// frozen footer
pub(crate) const EG_SCALE_DEN: i32 = 64;
";
        // Byte-for-byte: a mis-terminated repeat (inner `;`) leaves a trailing
        // ` 8];` after the rewritten value, breaking this exact equality.
        assert_eq!(
            after, expected,
            "array-repeat RHS rewrite must consume the inner `;` (depth>0) and \
             terminate at the statement `;` (depth 0); got:\n{after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `apply_to_data_rs` on a temp copy of the real `src/eval/data.rs` with
    /// non-zero values: the rewritten file parses back to those values, and a
    /// second apply is idempotent. Exercises the split `scale` group (two
    /// non-contiguous regions), trailing `// lit` comments containing `=` and
    /// `;`, and `#[allow(dead_code)]`/`#[rustfmt::skip]` attributes inside
    /// marked regions.
    #[test]
    fn apply_rewrites_real_data_rs_marker_structure() {
        // Locate the real data.rs relative to this source file's CARGO_MANIFEST_DIR.
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let data_rs = std::path::PathBuf::from(&manifest).join("src/eval/data.rs");
        if !data_rs.exists() {
            // Guard: the test only runs when the source tree is present (it is in
            // normal `cargo test` invocations; skip gracefully otherwise).
            return;
        }

        let dir = unique_temp_dir("real-data-rs");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let tmp = dir.join("data.rs");
        std::fs::copy(&data_rs, &tmp).expect("copy data.rs");

        // Build a params with all core fields set to non-zero, index-dependent
        // values so the rewrite actually changes the file and the values are
        // distinguishable (no accidental pass on zero identity).
        let mut p = EvalParams::shipped();
        p.iso_mg = -11.0;
        p.iso_eg = -7.0;
        p.dbl_mg = -5.0;
        p.dbl_eg = -3.0;
        p.bwd_mg = -4.0;
        p.bwd_eg = -2.0;
        for k in 0..8 {
            p.conn_mg[k] = (k as f64) - 3.0;
            p.conn_eg[k] = (k as f64) + 1.0;
        }
        p.rook_open_file_mg = 23.0;
        p.rook_open_file_eg = 11.0;
        p.rook_semi_open_file_mg = 9.0;
        p.rook_semi_open_file_eg = 4.0;
        p.ocb_with_pawns_scale = 48.0;
        p.pawnless_draw_scale = 32.0;
        p.fifty_move_floor = 10.0;
        let ints = p.to_ints();

        apply_to_data_rs(&ints, &tmp).expect("apply to real data.rs");

        // The file must still be parseable: read it back and verify the consts
        // we set appear as the exact integer values we wrote.
        let after = std::fs::read_to_string(&tmp).expect("read after apply");

        let parse_i32 = |src: &str, name: &str| -> i32 {
            let needle = format!("const {name}: i32 = ");
            let start = src
                .find(&needle)
                .unwrap_or_else(|| panic!("const {name} not found in rewritten real data.rs"))
                + needle.len();
            let tail = &src[start..];
            let end = tail.find(';').expect("terminated by ;");
            tail[..end]
                .trim()
                .parse::<i32>()
                .unwrap_or_else(|e| panic!("const {name} not an i32: {e}"))
        };

        assert_eq!(parse_i32(&after, "ISO_MG"), ints.iso_mg);
        assert_eq!(parse_i32(&after, "ISO_EG"), ints.iso_eg);
        assert_eq!(parse_i32(&after, "DBL_MG"), ints.dbl_mg);
        assert_eq!(parse_i32(&after, "BWD_EG"), ints.bwd_eg);
        assert_eq!(
            parse_i32(&after, "ROOK_OPEN_FILE_MG"),
            ints.rook_open_file_mg
        );
        assert_eq!(
            parse_i32(&after, "ROOK_SEMI_OPEN_FILE_EG"),
            ints.rook_semi_open_file_eg
        );
        assert_eq!(
            parse_i32(&after, "OCB_WITH_PAWNS_SCALE"),
            ints.ocb_with_pawns_scale
        );
        assert_eq!(
            parse_i32(&after, "PAWNLESS_DRAW_SCALE"),
            ints.pawnless_draw_scale
        );
        assert_eq!(parse_i32(&after, "FIFTY_MOVE_FLOOR"), ints.fifty_move_floor);

        // Idempotent: re-applying the same params is a no-op.
        let snapshot = after.clone();
        apply_to_data_rs(&ints, &tmp).expect("re-apply");
        let after2 = std::fs::read_to_string(&tmp).expect("read after re-apply");
        assert_eq!(after2, snapshot, "apply must be idempotent on real data.rs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unique per-test temp dir (avoids cross-test collisions; CI-parallel-safe).
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("clawfish-texel-{tag}-{pid}-{n}"))
    }
}
