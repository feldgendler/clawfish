//! The pinned quiet predicate (M6.G↔M6.H interface contract).
//!
//! `QUIET ⟺ !in_check(pos) ∧ |static_eval_white − qsearch_white| <
//! QUIET_MARGIN_CP`. `static_eval_white` = white-POV `eval::evaluate`;
//! `qsearch_white` = `search::quiescence_eval_white*` (the M6.G seam).
//!
//! Implemented by the M6.G `filter+quiet` coder slice per
//! `docs/plans/m6.g.md` §3.4.

use crate::Position;

/// White-POV static eval (centipawns). `eval::evaluate` is
/// side-to-move-relative; negate for Black so the corpus is White-POV
/// (Texel needs White-positive).
pub fn static_eval_white(_pos: &Position) -> i32 {
    todo!("M6.G quiet slice")
}

/// The pinned predicate. `static_eval` / `qscore` are both White-POV cp;
/// pass them in so the caller can reuse a per-worker searcher for `qscore`
/// (R6/R7 — no per-position searcher allocation).
pub fn is_quiet(_pos: &Position, _static_eval: i32, _qscore: i32) -> bool {
    todo!("M6.G quiet slice")
}
