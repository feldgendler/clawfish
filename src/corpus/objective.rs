//! Stratified selection objective (research §1.2/§6.4). DEFINITION +
//! computable harness only — the bi-level mixture *selection* runs in M6.H
//! (it needs the inner Texel).
//!
//! Pinned M6.G↔M6.H interface: a quiet-certified position is scored by
//! `quiet::static_eval_white` (NOT qsearch at tune time) — Predicate B was
//! chosen precisely so `static_eval ≈ qsearch` within `QUIET_MARGIN_CP`,
//! resolving research §2.5's open question. Texel `K` fit once over the
//! corpus (1-D MSE-min). Strata: depth-rung (R-TC) + frozen
//! `STRATUM_OUTPOST` (STS theme #3 corpus-context analogue) +
//! `STRATUM_ENDGAME`; the outpost stratum is a FROZEN snapshot stored in
//! `CorpusRecord::strata` (never a live `eval::tier1` call — closes the
//! M6.H re-tune circularity).
//!
//! Implemented by the M6.G `objective` coder slice per
//! `docs/plans/m6.g.md` §3.8.

use crate::Position;

use super::CorpusRecord;

/// Per-stratum + aggregate held-out logistic loss.
#[derive(Clone, Debug, Default)]
pub struct StratObjective {
    /// Aggregate held-out logistic loss over all val records.
    pub aggregate: f64,
    /// `(depth_rung, loss)` — the R-TC TC-stratification.
    pub per_depth_rung: Vec<(u8, f64)>,
    /// Loss restricted to `STRATUM_OUTPOST` records.
    pub outpost: f64,
    /// Loss restricted to `STRATUM_ENDGAME` records.
    pub endgame: f64,
}

/// Frozen-snapshot blind-spot strata bits for a position (computed once at
/// build time, stored in `CorpusRecord::strata`).
pub fn strata_for(_pos: &Position) -> u8 {
    todo!("M6.G objective slice")
}

/// 1-D minimize MSE over the Texel `K` (computed once; M6.H refits per
/// candidate). `score` = White-POV cp for a record.
pub fn fit_k(_recs: &[CorpusRecord], _score: &dyn Fn(&CorpusRecord) -> i32) -> f64 {
    todo!("M6.G objective slice")
}

/// Mean squared error of `σ(K·score)` vs the White-POV label. Clamped so
/// `exp` cannot overflow at extreme scores.
pub fn logistic_loss(
    _recs: &[CorpusRecord],
    _k: f64,
    _score: &dyn Fn(&CorpusRecord) -> i32,
) -> f64 {
    todo!("M6.G objective slice")
}

/// Aggregate + per-stratum held-out objective.
pub fn stratified_objective(
    _val: &[CorpusRecord],
    _k: f64,
    _score: &dyn Fn(&CorpusRecord) -> i32,
) -> StratObjective {
    todo!("M6.G objective slice")
}
