use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::tt::TtBound;
use crate::{Move, Position};

use super::{AlphaBetaMover, INF, MAX_PLY, SearchClock, SearchContext, SearchLimits, TimeCaps};

// ---------------------------------------------------------------------------
// M7.B — qsearch SEE-pruning
// ---------------------------------------------------------------------------

/// M7.B.2 — depth-conditioned qsearch SEE-prune ramp parameters
/// (`docs/plans/m7.b.2.md`). The threshold is flat `0` (prune every `see < 0`
/// capture, ≡ flat-0 M7.B) at/below `D0`, then ramps linearly more-negative by
/// `SLOPE` cp per ply, clamped at `FLOOR`. Rationale: a `see < 0` capture can be
/// a real tactical sacrifice the engine *would* refute given depth; at shallow
/// (fast-TC) iterations the node saving wins, but at deep (slow-TC) iterations
/// pruning it costs accuracy — so relax the prune only at depth.
pub(crate) const QS_SEE_RAMP_D0: u32 = 12;
pub(crate) const QS_SEE_RAMP_SLOPE: i32 = 16;
pub(crate) const QS_SEE_RAMP_FLOOR: i32 = -64;
/// Fast-out validity (see `qsearch_see_pruneable`): the ramp must be
/// non-positive at every depth so that `victim >= attacker ⇒ see >= 0 ⇒ not
/// pruneable` holds. `-SLOPE * max(0, ·)` clamped to `[FLOOR, 0]` is `<= 0` iff
/// `FLOOR <= 0 && SLOPE >= 0`. Compile-time-pinned (strongest form).
const _: () = assert!(QS_SEE_RAMP_FLOOR <= 0 && QS_SEE_RAMP_SLOPE >= 0);

/// Depth-conditioned SEE-prune threshold for the current ID root iteration.
/// `threshold(d) = clamp(-SLOPE * max(0, d - D0), FLOOR, 0)`: `0` for `d <= D0`
/// (flat-0 ≡ M7.B), ramping to `FLOOR` at `d >= D0 + FLOOR.abs()/SLOPE`.
/// Monotonically non-increasing in `d`; always in `[FLOOR, 0]`.
pub(crate) fn qs_see_prune_threshold(root_depth: u32) -> i32 {
    (-QS_SEE_RAMP_SLOPE * (root_depth.saturating_sub(QS_SEE_RAMP_D0) as i32))
        .clamp(QS_SEE_RAMP_FLOOR, 0)
}

/// Returns `true` iff `mv` (a non-promotion capture: `Capture` | `EnPassant`)
/// should be pruned from quiescence as statically losing relative to
/// `threshold` (the depth-conditioned `qs_see_prune_threshold` value). The
/// caller guarantees `!in_check` and `mv.is_capture() && !mv.is_promotion()`.
///
/// Fast-out: a capture whose victim is worth >= the attacker can never be
/// SEE-negative (worst case the attacker is traded off, netting >= 0), so
/// the full `see()` resolver is skipped for the common winning/equal captures
/// (PxN, RxQ, equal trades, all EP PxP). Only `attacker > victim` captures
/// (QxP-class) pay for the resolver. The fast-out is only valid while
/// `threshold <= 0` (victim>=attacker ⇒ see>=0 ⇒ not < threshold); the ramp is
/// `<= 0` by construction (clamped to `[FLOOR, 0]`, FLOOR<=0), pinned at the
/// const site above and re-asserted here as a debug belt-and-suspenders.
pub(crate) fn qsearch_see_pruneable(pos: &Position, mv: Move, threshold: i32) -> bool {
    use crate::mov::MoveFlag;
    use crate::piece::PieceKind;
    use crate::see::{SEE_VALUE, see};
    debug_assert!(threshold <= 0, "fast-out validity requires threshold <= 0");

    let attacker = SEE_VALUE[pos
        .piece_at(mv.from_square())
        .expect("qsearch_see_pruneable: capture from-square occupied")
        .kind as usize];
    // EP victim is a pawn off-square (the to-square is empty for EP); reading
    // piece_at(to) would be a bug. All other captures take the piece on `to`.
    let victim = match mv.flag() {
        MoveFlag::EnPassant => SEE_VALUE[PieceKind::Pawn as usize],
        _ => {
            SEE_VALUE[pos
                .piece_at(mv.to_square())
                .expect("qsearch_see_pruneable: capture to-square occupied")
                .kind as usize]
        }
    };
    if victim >= attacker {
        return false; // SEE >= 0; never prune (valid while threshold <= 0)
    }
    see(pos, mv) < threshold
}

/// M5.E #4 — qsearch ply-ceiling short-circuit predicate. Returns `true`
/// when qsearch should return `evaluate(pos)` immediately instead of
/// recursing further, as a defense-in-depth against pathological forced-
/// quiet chains under the M5.E #1 single-reply extension.
///
/// Only the `!in_check` arm is guarded. At in-check + ply ceiling the
/// existing evasion arm runs as before. Returning a fabricated mate-in-ply
/// score (`-(MATE - ply)`) would propagate a false mate through
/// `score_to_uci`, `score_to_tt`, and mate-distance pruning, and the
/// in-check ceiling case is structurally near-impossible (a chain of
/// forced check evasions tall enough to hit MAX_PLY does not occur in
/// real positions). ADR-0027 §4.
///
/// Extracted from the inline qsearch guard for mutation-coverage
/// discrimination per the M3.D `negate_window` precedent: at the only
/// callable ply value (`MAX_PLY - 1`) the qsearch fall-through path also
/// returns stand-pat on typical fixtures, so cargo-mutants cannot
/// distinguish the inline arithmetic. The helper's named function and
/// unit tests give mutation testing a discriminating surface.
#[inline]
pub(crate) fn qsearch_short_circuit_at_ply_ceiling(ply: u32, pos: &Position) -> bool {
    ply >= MAX_PLY as u32 - 1 && !crate::movegen::in_check(pos)
}

/// Classify the bound for a completed qsearch result (three-way, M5.F.1).
///
/// `alpha_entry` is the node's window alpha at entry (after mate-distance
/// pruning, BEFORE the stand-pat raise) — i.e. the caller-facing lower bound.
///
/// - `best >= beta`: `Lower` (fail-high cutoff).
/// - `alpha_entry < best < beta`: `Exact` (improved the entry alpha without a
///   cutoff — a fully-searched fail-soft value).
/// - otherwise (`best <= alpha_entry`): `Upper` (fail-low).
///
/// **Why `Exact` is sound here, despite Stockfish 45e5e65.** That commit keeps
/// qsearch to Lower/Upper because a qsearch `Exact` (restricted to captures /
/// evasions, not all legal moves) could short-circuit a future *negamax* probe
/// with a value that isn't a true all-moves minimax. In *this* engine that
/// path does not exist: `negamax` delegates to `qsearch` at `depth == 0`
/// (`search.rs` step ~1480) **before** its TT-cutoff probe, and that probe only
/// cuts when `entry.depth >= depth` — unreachable for a depth-0 qsearch entry
/// at any `depth >= 1`. So a qsearch entry never triggers a negamax cutoff
/// (negamax only reads its `best_move` for ordering). The only consumer of a
/// qsearch `Exact` is qsearch's own re-probe, which returns it unconditionally;
/// that is correct because a completed fail-soft loop with `alpha_entry < best
/// < beta` searched every capture full-window (no PVS null-window) and the
/// move that set `best` returned its exact value — window-independent within
/// qsearch's own (captures + stand-pat) value model. M5.F.1 tuning campaign vs
/// M6.J (tuning-backlog M5.F item 1).
///
/// Terminal cases (handled at their own call sites with a forced
/// `Exact` bound, NOT this helper): true stalemate (returns 0) and mate
/// at horizon (returns `-(MATE - ply)`).
///
/// Precondition: `best != -INF`. The `-INF` seed at the in-check arm
/// either recurses (in which case any executed evasion's negated child
/// score satisfies `score > -INF`, so `best > -INF` after the first
/// completed iteration) or hits the empty-evasion terminal which
/// returns `-(MATE - ply)` directly via path D — that path stores
/// before reaching the post-loop classifier and never invokes this
/// helper. Pinned by debug_assert.
pub(crate) fn qsearch_tt_bound_for_completed_node(
    best: i32,
    alpha_entry: i32,
    beta: i32,
) -> TtBound {
    debug_assert!(
        best != -INF,
        "qsearch_tt_bound_for_completed_node: best == -INF unreachable at production sites"
    );
    if best >= beta {
        TtBound::Lower
    } else if best > alpha_entry {
        TtBound::Exact
    } else {
        TtBound::Upper
    }
}

// ---------------------------------------------------------------------------
// M6.G corpus seam — quiescence eval accessor (additive, behavior-neutral).
//
// Reads the *existing* `AlphaBetaMover::qsearch` with a full window at ply 0.
// Introduces no new search/eval logic and touches no existing code path, so
// `evaluate`/negamax/qsearch and the deterministic `bench` signature are
// byte-identical (the M6.G "no engine build / no bench" clause targets
// strength/bench-affecting change). White-POV (Texel needs White-positive);
// `qsearch` returns a side-to-move-relative score, negated for Black here.
//
// Consumed by `corpus::quiet` (M6.G) and downstream by M6.I, the
// tuning-backlog "PST co-tuning" Arm B, future SPSA campaigns, and M11
// NNUE data-prep — a deliberately public data-infra seam.
// ---------------------------------------------------------------------------

/// Opaque per-worker quiescence-eval handle. Wraps the crate-private
/// searcher so the M6.G seam does not leak engine internals (the
/// `evaluate_cached` `pub(crate)`-type-leak precedent). Construct once per
/// worker thread and reuse across positions — do **not** allocate per
/// position (R6/R7: avoids per-position searcher/TT/pawn-hash allocation).
pub struct QSearcher {
    inner: AlphaBetaMover,
    /// Reused per-call so each `eval_white` invocation does not allocate
    /// a fresh `Arc<AtomicBool>` (≈6M allocations elided on a 3M-record
    /// corpus build). The flag stays `false` for the lifetime of this
    /// `QSearcher` — `eval_white` never sets `stop`, and there is no
    /// other concurrent owner.
    stop: Arc<AtomicBool>,
}

impl Default for QSearcher {
    fn default() -> Self {
        Self::new()
    }
}

impl QSearcher {
    /// New reusable handle.
    pub fn new() -> Self {
        QSearcher {
            inner: AlphaBetaMover::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// White-POV quiescence-search score (centipawns) of `pos`.
    /// `evaluate`/qsearch logic is unchanged; this only *reads* it. White-POV
    /// because Texel needs White-positive (`qsearch` is side-to-move-relative,
    /// negated for Black here).
    pub fn eval_white(&mut self, pos: &Position) -> i32 {
        let caps = TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        };
        let ctx = SearchContext {
            stop: Arc::clone(&self.stop),
            caps,
            virtual_clock: false,
            limits: SearchLimits::default(),
            history: Vec::new(),
            tt: None,
        };
        let clock = SearchClock::start_for(false, caps);
        let mut work = *pos;
        let stm_score = self.inner.qsearch(&mut work, -INF, INF, 0, &ctx, &clock);
        if pos.side_to_move() == crate::Color::White {
            stm_score
        } else {
            -stm_score
        }
    }
}

/// White-POV quiescence-search score (centipawns) of `pos`. Convenience
/// wrapper that allocates a throwaway [`QSearcher`]; for bulk corpus use
/// prefer a per-worker-reused `QSearcher`.
pub fn quiescence_eval_white(pos: &Position) -> i32 {
    QSearcher::new().eval_white(pos)
}
