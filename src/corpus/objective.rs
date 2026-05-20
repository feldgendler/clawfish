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
use crate::bitboard::{self, Bitboard};
use crate::piece::{Color, PieceKind};

use super::{CorpusRecord, STRATUM_ENDGAME, STRATUM_OUTPOST};

// ---------------------------------------------------------------------------
// Mop-up threshold (structural constant from eval.rs — NOT a tuned weight).
// Source: `src/eval.rs` line ~91: `const MOP_UP_PHASE_MAX: u8 = 5;`
// Endgame stratum = positions where raw_phase < this value.
// Copied here as a structural constant; if eval.rs changes this value the
// corpus stratum definition drifts intentionally (it is the same semantic
// boundary).
// ---------------------------------------------------------------------------
const MOP_UP_PHASE_MAX: u8 = 5;

// ---------------------------------------------------------------------------
// Frozen-snapshot outpost predicate (M6.G§3.8 "frozen, not live").
//
// This is a FROZEN COPY of the algorithm from `eval::tier1::outpost_squares`
// + the rank gate from `eval::tier1::outpost_term_white`, snapshotted at
// M6.F (2026-05-19). M6.H must NOT change this snapshot — the stratum is
// fixed at corpus-build time. If the live outpost predicate in eval::tier1
// is later re-tuned by M6.H, `strata_for` must still reflect the M6.F
// corpus-build semantics, not the new eval semantics.
//
// Correctness note from CLAUDE.md / ADR-0034: the front-span-complement
// (`!*_attack_front_spans(enemy_pawns)`) is a valid hole test ONLY in the
// enemy half; the rank gate `(3..=5)` is the load-bearing gate that
// constrains the outpost to the enemy half.
// ---------------------------------------------------------------------------

/// Relative rank from `side`'s perspective (0 = own back rank, 7 = enemy back rank).
/// Frozen copy of `eval::tier1::relative_rank` — do NOT delegate to the live fn.
fn frozen_relative_rank(side: Color, sq: crate::square::Square) -> u8 {
    match side {
        Color::White => sq.rank(),
        Color::Black => 7 - sq.rank(),
    }
}

/// Frozen snapshot of `eval::tier1::outpost_squares` (M6.F, 2026-05-19).
/// Returns the set of squares that are (a) pawn-supported and (b)
/// unchallengeable by an enemy pawn for the given `side`.
/// NEVER delegates to the live `eval::tier1` seam.
fn frozen_outpost_squares(pos: &Position, side: Color) -> Bitboard {
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

/// Returns `true` if the position has at least one knight or bishop on an
/// outpost square in the enemy-half rank band (relative ranks 3..=5).
/// Frozen snapshot of the M6.F scoring predicate — NEVER calls live eval::tier1.
fn frozen_has_outpost_candidate(pos: &Position) -> bool {
    for side in [Color::White, Color::Black] {
        let outpost_sqs = frozen_outpost_squares(pos, side);
        for sq in (pos.pieces_colored(side, PieceKind::Knight) & outpost_sqs).iter() {
            if (3..=5).contains(&(frozen_relative_rank(side, sq) as usize)) {
                return true;
            }
        }
        for sq in (pos.pieces_colored(side, PieceKind::Bishop) & outpost_sqs).iter() {
            if (3..=5).contains(&(frozen_relative_rank(side, sq) as usize)) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

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
    /// Loss restricted to `STRATUM_BOOK_OPENING` records (the M6.H
    /// meta-tunable stratum — book-seeded games carry this bit so the
    /// outer simplex can compare against the unset complement).
    pub book_opening: f64,
    /// `(source_byte, loss)` — per-source held-out loss. The corpus-mix
    /// (self-play vs CCRL vs Lichess) ratio is another M6.H meta-tunable
    /// axis (ADR-0035 §9 / plan §3.8); this entry lets the outer simplex
    /// read the per-source signal and move the mix accordingly. Sorted
    /// by source byte for stable iteration.
    pub per_source: Vec<(u8, f64)>,
}

/// Frozen-snapshot blind-spot strata bits for a position (computed once at
/// build time, stored in `CorpusRecord::strata`).
///
/// `STRATUM_OUTPOST`: set if any knight/bishop occupies a pawn-supported
/// unchallengeable square in the enemy-half rank band — frozen M6.F snapshot,
/// NEVER a live `eval::tier1` call.
///
/// `STRATUM_ENDGAME`: set if `pos.raw_phase() < MOP_UP_PHASE_MAX` (the same
/// structural threshold eval.rs uses for mop-up — sourced from `src/eval.rs`
/// `const MOP_UP_PHASE_MAX: u8 = 5`).
pub fn strata_for(pos: &Position) -> u8 {
    let mut bits = 0u8;
    if frozen_has_outpost_candidate(pos) {
        bits |= STRATUM_OUTPOST;
    }
    if pos.raw_phase() < MOP_UP_PHASE_MAX {
        bits |= STRATUM_ENDGAME;
    }
    bits
}

/// Logistic sigmoid, clamped to avoid `exp` overflow at extreme scores.
/// Clamps `x` to `[-60, 60]` before computing `1 / (1 + exp(-x))`.
#[inline]
fn sigmoid(x: f64) -> f64 {
    // exp(-60) ≈ 8.8e-27 and exp(60) ≈ 1.1e26 — both finite, but we clamp
    // for safety and to avoid loss of numerical precision.
    let x_clamped = x.clamp(-60.0, 60.0);
    1.0 / (1.0 + (-x_clamped).exp())
}

/// Mean squared error of `σ(K·score_white)` vs the White-POV label.
/// Returns a finite value for any finite K and any score (extreme scores
/// are clamped inside `sigmoid`). Returns 0.0 for an empty slice.
pub fn logistic_loss(recs: &[CorpusRecord], k: f64, score: &dyn Fn(&CorpusRecord) -> i32) -> f64 {
    if recs.is_empty() {
        return 0.0;
    }
    let sum: f64 = recs
        .iter()
        .map(|r| {
            let s = k * score(r) as f64;
            let pred = sigmoid(s);
            let target = r.label.as_f64();
            let diff = target - pred;
            diff * diff
        })
        .sum();
    sum / recs.len() as f64
}

/// 1-D minimize MSE over the Texel `K ∈ (0, 2]` by golden-section search.
/// `score` = White-POV cp for a record. Returns the K that minimises
/// `logistic_loss(recs, K, score)`. For an empty slice returns 1.0
/// (arbitrary; no tuning possible).
pub fn fit_k(recs: &[CorpusRecord], score: &dyn Fn(&CorpusRecord) -> i32) -> f64 {
    if recs.is_empty() {
        return 1.0;
    }
    // Golden-section search on K ∈ [LO, HI].
    // The MSE is convex in K (sigmoid is monotone → MSE is unimodal),
    // so golden-section finds the global minimum.
    const LO: f64 = 1e-6;
    const HI: f64 = 2.0;
    const ITERS: usize = 60;
    const PHI: f64 = 0.618_033_988_749_895; // 1/φ

    let mut a = LO;
    let mut b = HI;
    // Two interior evaluation points.
    let mut c = b - PHI * (b - a);
    let mut d = a + PHI * (b - a);
    let mut fc = logistic_loss(recs, c, score);
    let mut fd = logistic_loss(recs, d, score);

    for _ in 0..ITERS {
        if fc < fd {
            b = d;
            d = c;
            fd = fc;
            c = b - PHI * (b - a);
            fc = logistic_loss(recs, c, score);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + PHI * (b - a);
            fd = logistic_loss(recs, d, score);
        }
    }
    // Return the midpoint of the converged bracket.
    (a + b) / 2.0
}

/// Aggregate + per-stratum held-out objective. Computes:
/// - Aggregate logistic loss over all records.
/// - Per-depth-rung losses (sorted by rung).
/// - Loss restricted to `STRATUM_OUTPOST` records.
/// - Loss restricted to `STRATUM_ENDGAME` records.
pub fn stratified_objective(
    val: &[CorpusRecord],
    k: f64,
    score: &dyn Fn(&CorpusRecord) -> i32,
) -> StratObjective {
    let aggregate = logistic_loss(val, k, score);

    // Per-depth-rung: collect unique rungs, then compute per-rung loss.
    let mut rungs: Vec<u8> = val.iter().map(|r| r.depth_rung).collect();
    rungs.sort_unstable();
    rungs.dedup();
    let per_depth_rung: Vec<(u8, f64)> = rungs
        .into_iter()
        .map(|rung| {
            let stratum: Vec<&CorpusRecord> = val.iter().filter(|r| r.depth_rung == rung).collect();
            let loss = logistic_loss_refs(&stratum, k, score);
            (rung, loss)
        })
        .collect();

    let outpost_recs: Vec<&CorpusRecord> = val
        .iter()
        .filter(|r| r.strata & STRATUM_OUTPOST != 0)
        .collect();
    let outpost = logistic_loss_refs(&outpost_recs, k, score);

    let endgame_recs: Vec<&CorpusRecord> = val
        .iter()
        .filter(|r| r.strata & STRATUM_ENDGAME != 0)
        .collect();
    let endgame = logistic_loss_refs(&endgame_recs, k, score);

    let book_recs: Vec<&CorpusRecord> = val
        .iter()
        .filter(|r| r.strata & super::STRATUM_BOOK_OPENING != 0)
        .collect();
    let book_opening = logistic_loss_refs(&book_recs, k, score);

    // Per-source loss (M6.H corpus-mix meta-tunable axis).
    let mut source_bytes: Vec<u8> = val.iter().map(|r| r.source.as_u8()).collect();
    source_bytes.sort_unstable();
    source_bytes.dedup();
    let per_source: Vec<(u8, f64)> = source_bytes
        .into_iter()
        .map(|sb| {
            let stratum: Vec<&CorpusRecord> =
                val.iter().filter(|r| r.source.as_u8() == sb).collect();
            (sb, logistic_loss_refs(&stratum, k, score))
        })
        .collect();

    StratObjective {
        aggregate,
        per_depth_rung,
        outpost,
        endgame,
        book_opening,
        per_source,
    }
}

/// Like `logistic_loss` but over a slice of references.
fn logistic_loss_refs(recs: &[&CorpusRecord], k: f64, score: &dyn Fn(&CorpusRecord) -> i32) -> f64 {
    if recs.is_empty() {
        return 0.0;
    }
    let sum: f64 = recs
        .iter()
        .map(|r| {
            let s = k * score(r) as f64;
            let pred = sigmoid(s);
            let target = r.label.as_f64();
            let diff = target - pred;
            diff * diff
        })
        .sum();
    sum / recs.len() as f64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use crate::corpus::{
        CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, STRATUM_ENDGAME, STRATUM_OUTPOST, Source,
    };

    // Helper: construct a minimal CorpusRecord.
    fn make_rec(
        fen: &str,
        label: Label,
        score_cp: i32,
        strata: u8,
        depth_rung: u8,
    ) -> CorpusRecord {
        // We embed the score_cp in the fen field for testing convenience —
        // tests use a closure that parses it back. Actually, we just stash it
        // in game_id (unused in objective tests) and use a lookup closure.
        // But better: store in a way the test score closure can recover.
        // Since we can't add fields, we'll use a real FEN and a closure that
        // ignores it. Tests that need a specific score pass a closure directly.
        CorpusRecord {
            fen: fen.to_string(),
            label,
            source: Source::SelfPlay,
            game_id: score_cp as u64, // repurposed as score carrier for tests
            ply: 10,
            depth_rung,
            strata,
        }
    }

    // Score closure that reads `game_id` as a signed i32 cp score.
    fn score_from_game_id(r: &CorpusRecord) -> i32 {
        r.game_id as i32
    }

    // ---------------------------------------------------------------------------
    // fit_k_recovers_known_k_on_monotone_fixture
    //
    // Synthetic corpus where the label distribution at each score is calibrated
    // to the Texel sigmoid at K_true. Uses small centipawn scores (±1, ±2) so
    // K_true=0.5 is in the non-saturating region of the sigmoid within K∈(0,2].
    //
    // Fixture design: at score s, generate N_REPS records using the three-label
    // decomposition that makes the average label = sigmoid(K_true * s):
    //   p >= 0.5: nw = round((2p-1)*N) WhiteWin, nd = N-nw Draws
    //   p < 0.5:  nb = round((1-2p)*N) BlackWin, nd = N-nb Draws
    // With N=500 per score point and 5 score points, the quantization noise
    // averages away → MSE has a genuine interior minimum at K ≈ K_true.
    // ---------------------------------------------------------------------------
    #[test]
    fn fit_k_recovers_known_k_on_monotone_fixture() {
        const K_TRUE: f64 = 0.5;
        // Small cp scores: K_TRUE * score ∈ [-1, 1] → sigmoid in [0.27, 0.73]
        // (well within the non-saturating regime for all K ∈ (0, 2]).
        const SCORES: &[i32] = &[-2, -1, 0, 1, 2];
        const N_REPS: usize = 500;

        let mut recs: Vec<CorpusRecord> = Vec::new();
        for &s in SCORES {
            let p = sigmoid(K_TRUE * s as f64);
            // Three-label decomposition: average label = p.
            // p >= 0.5: nw = round((2p-1)*N), nd = N-nw, nb = 0
            // p < 0.5:  nb = round((1-2p)*N), nd = N-nb, nw = 0
            let nw = ((2.0 * p - 1.0).max(0.0) * N_REPS as f64).round() as usize;
            let nb = ((1.0 - 2.0 * p).max(0.0) * N_REPS as f64).round() as usize;
            let nd = N_REPS - nw - nb;
            for _ in 0..nw {
                recs.push(make_rec("x", Label::WhiteWin, s, 0, DEPTH_RUNG_EXTERNAL));
            }
            for _ in 0..nd {
                recs.push(make_rec("x", Label::Draw, s, 0, DEPTH_RUNG_EXTERNAL));
            }
            for _ in 0..nb {
                recs.push(make_rec("x", Label::BlackWin, s, 0, DEPTH_RUNG_EXTERNAL));
            }
        }

        let k_fit = fit_k(&recs, &score_from_game_id);
        // Tolerance: 1e-2 (three-label quantization noise)
        assert!(
            (k_fit - K_TRUE).abs() < 1e-2,
            "fit_k recovered K={k_fit:.6}, expected ≈ K_true={K_TRUE:.4} (tol 1e-2)"
        );
    }

    // ---------------------------------------------------------------------------
    // score_closure_is_static_eval_white
    //
    // Documents and exercises that the pinned scoring closure is static_eval_white
    // (not qsearch) at tune time. The harness is designed around a Fn closure
    // that can be static_eval_white-shaped.
    // ---------------------------------------------------------------------------
    #[test]
    fn score_closure_is_static_eval_white() {
        // The pinned tuning closure is `quiet::static_eval_white` — NOT a
        // qsearch call at tune time. The quiet-certificate (Predicate B,
        // `|static_eval − qsearch| < QUIET_MARGIN_CP`) makes the two scores
        // close BY CONSTRUCTION on a quiet corpus, so static-eval is the
        // cheap, correct tuning score. This test exercises both closures
        // on a diverse fixture and asserts the fit Ks land in the same
        // ball-park — if they diverge, the quiet predicate isn't doing
        // its job.
        use crate::bench::BENCH_POSITIONS;
        use crate::search::QSearcher;

        // Build a fixture of ~16 positions from the bench corpus. Assign
        // plausible labels by sign of the white-POV static eval (an
        // imperfect proxy, but it gives non-degenerate K-fit data).
        let recs: Vec<CorpusRecord> = BENCH_POSITIONS
            .iter()
            .enumerate()
            .map(|(i, fen)| {
                let p = Position::from_fen(fen).expect("BENCH_POSITIONS FEN");
                let stm = crate::eval::evaluate(&p);
                let white_pov = match p.side_to_move() {
                    Color::White => stm,
                    Color::Black => -stm,
                };
                let label = if white_pov > 50 {
                    Label::WhiteWin
                } else if white_pov < -50 {
                    Label::BlackWin
                } else {
                    Label::Draw
                };
                CorpusRecord {
                    fen: fen.to_string(),
                    label,
                    source: Source::SelfPlay,
                    game_id: i as u64,
                    ply: 10,
                    depth_rung: DEPTH_RUNG_EXTERNAL,
                    strata: 0,
                }
            })
            .collect();
        assert!(
            recs.len() >= 10,
            "fixture should have ≥ 10 diverse positions; got {}",
            recs.len()
        );

        // Closure A — the pinned production closure: white-POV static eval.
        let static_closure = |r: &CorpusRecord| -> i32 {
            let p = Position::from_fen(&r.fen).expect("valid FEN");
            let stm = crate::eval::evaluate(&p);
            match p.side_to_move() {
                Color::White => stm,
                Color::Black => -stm,
            }
        };

        // Closure B — the qsearch-at-tune-time alternative (NOT pinned).
        // A per-call QSearcher is wasteful but fine for a ≤16-entry test.
        let qsearch_closure = |r: &CorpusRecord| -> i32 {
            let p = Position::from_fen(&r.fen).expect("valid FEN");
            QSearcher::new().eval_white(&p)
        };

        let k_static = fit_k(&recs, &static_closure);
        let k_qsearch = fit_k(&recs, &qsearch_closure);

        assert!(
            k_static.is_finite() && k_static > 0.0,
            "fit_k via static_eval must be finite + positive; got {k_static}"
        );
        assert!(
            k_qsearch.is_finite() && k_qsearch > 0.0,
            "fit_k via qsearch must be finite + positive; got {k_qsearch}"
        );

        // The quiet-certificate predicts `static_eval ≈ qsearch` ⇒ both Ks
        // should land in the same ball-park. Allow a generous 50% relative
        // tolerance: a divergence beyond that suggests the quiet predicate
        // (or this fixture) admits too many non-quiet positions.
        let lo = k_static.min(k_qsearch);
        let hi = k_static.max(k_qsearch);
        assert!(
            hi <= 1.5 * lo,
            "k_static={k_static:.4} vs k_qsearch={k_qsearch:.4} diverge > 50% \
             — the quiet predicate is allowing non-quiet positions through"
        );

        let loss_static = logistic_loss(&recs, k_static, &static_closure);
        assert!(
            loss_static.is_finite() && loss_static >= 0.0,
            "logistic_loss must be finite + non-negative"
        );
        let obj = stratified_objective(&recs, k_static, &static_closure);
        assert!(obj.aggregate.is_finite());
    }

    // ---------------------------------------------------------------------------
    // logistic_loss_matches_closed_form_tiny
    //
    // 3-4 records, hand-computed expected loss.
    // ---------------------------------------------------------------------------
    #[test]
    fn logistic_loss_matches_closed_form_tiny() {
        // Three records with known scores and labels.
        // score=0 → sigmoid(K*0) = 0.5 for any K.
        // score=100 with K=0.01 → sigmoid(1.0) ≈ 0.7310586
        // score=-100 with K=0.01 → sigmoid(-1.0) ≈ 0.2689414
        let k = 0.01f64;
        let recs = vec![
            make_rec("a", Label::Draw, 0, 0, DEPTH_RUNG_EXTERNAL), // pred=0.5, target=0.5, err²=0
            make_rec("b", Label::WhiteWin, 100, 0, DEPTH_RUNG_EXTERNAL), // pred≈0.7311, target=1.0, err²≈(0.2689)²
            make_rec("c", Label::BlackWin, -100, 0, DEPTH_RUNG_EXTERNAL), // pred≈0.2689, target=0.0, err²≈(0.2689)²
        ];

        let s0 = sigmoid(k * 0.0);
        let s100 = sigmoid(k * 100.0);
        let s_neg100 = sigmoid(k * -100.0);

        let expected = ((0.5 - s0).powi(2) + (1.0 - s100).powi(2) + (0.0 - s_neg100).powi(2)) / 3.0;

        let actual = logistic_loss(&recs, k, &score_from_game_id);
        assert!(
            (actual - expected).abs() < 1e-12,
            "logistic_loss mismatch: actual={actual:.10e} expected={expected:.10e}"
        );
    }

    // ---------------------------------------------------------------------------
    // loss_finite_at_extreme_scores
    //
    // Scores like ±100000 must not produce NaN or inf.
    // ---------------------------------------------------------------------------
    #[test]
    fn loss_finite_at_extreme_scores() {
        let extremes = [
            100_000i32,
            -100_000,
            1_000_000,
            -1_000_000,
            i32::MAX,
            i32::MIN,
        ];
        for &sc in &extremes {
            let recs = vec![
                make_rec("x", Label::WhiteWin, sc, 0, DEPTH_RUNG_EXTERNAL),
                make_rec("y", Label::BlackWin, sc, 0, DEPTH_RUNG_EXTERNAL),
            ];
            let loss = logistic_loss(&recs, 1.0, &score_from_game_id);
            assert!(
                loss.is_finite() && !loss.is_nan(),
                "logistic_loss at score={sc} must be finite (got {loss})"
            );
        }
        // Also verify sigmoid itself is always in [0, 1].
        for &sc in &extremes {
            let s = sigmoid(1.0 * sc as f64);
            assert!(
                (0.0..=1.0).contains(&s),
                "sigmoid({sc}) = {s} must be in [0,1]"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // stratified_eq_weighted_sum_of_strata
    //
    // The aggregate loss must equal the weighted average of per-stratum losses
    // when strata partition the dataset (non-overlapping).
    // ---------------------------------------------------------------------------
    #[test]
    fn stratified_eq_weighted_sum_of_strata() {
        // Create records that are either STRATUM_OUTPOST or STRATUM_ENDGAME
        // (not both, not neither) so the strata partition the dataset.
        let k = 0.5;
        let recs = vec![
            make_rec(
                "a",
                Label::WhiteWin,
                50,
                STRATUM_OUTPOST,
                DEPTH_RUNG_EXTERNAL,
            ),
            make_rec("b", Label::Draw, 10, STRATUM_OUTPOST, DEPTH_RUNG_EXTERNAL),
            make_rec(
                "c",
                Label::BlackWin,
                -80,
                STRATUM_ENDGAME,
                DEPTH_RUNG_EXTERNAL,
            ),
            make_rec("d", Label::Draw, 0, STRATUM_ENDGAME, DEPTH_RUNG_EXTERNAL),
        ];

        let obj = stratified_objective(&recs, k, &score_from_game_id);

        // Compute reference losses directly.
        let outpost_recs: Vec<CorpusRecord> = recs
            .iter()
            .filter(|r| r.strata & STRATUM_OUTPOST != 0)
            .cloned()
            .collect();
        let endgame_recs: Vec<CorpusRecord> = recs
            .iter()
            .filter(|r| r.strata & STRATUM_ENDGAME != 0)
            .cloned()
            .collect();

        let ref_outpost = logistic_loss(&outpost_recs, k, &score_from_game_id);
        let ref_endgame = logistic_loss(&endgame_recs, k, &score_from_game_id);
        let ref_agg = logistic_loss(&recs, k, &score_from_game_id);

        assert!(
            (obj.outpost - ref_outpost).abs() < 1e-12,
            "outpost loss mismatch: obj.outpost={} ref={ref_outpost}",
            obj.outpost
        );
        assert!(
            (obj.endgame - ref_endgame).abs() < 1e-12,
            "endgame loss mismatch: obj.endgame={} ref={ref_endgame}",
            obj.endgame
        );
        assert!(
            (obj.aggregate - ref_agg).abs() < 1e-12,
            "aggregate loss mismatch: obj.aggregate={} ref={ref_agg}",
            obj.aggregate
        );

        // Weighted average check: since strata partition (2 outpost, 2 endgame),
        // aggregate = (2/4)*outpost + (2/4)*endgame.
        let weighted = 0.5 * ref_outpost + 0.5 * ref_endgame;
        assert!(
            (obj.aggregate - weighted).abs() < 1e-12,
            "aggregate must equal weighted average of strata when they partition: {} vs {weighted}",
            obj.aggregate
        );
    }

    // ---------------------------------------------------------------------------
    // outpost_stratum_is_frozen_snapshot_not_live_call
    //
    // Verify that strata_for uses a frozen copy of the outpost predicate
    // (by construction/comment + positive/negative fixtures).
    // ---------------------------------------------------------------------------
    #[test]
    fn outpost_stratum_is_frozen_snapshot_not_live_call() {
        // Positive fixture: White Nd5, White pawns c4+e4, no black pawns.
        // d5 is pawn-supported by c4+e4 and unchallengeable → outpost.
        // Nd5 is on relative rank 4 for White (rank=4 in 0-indexed) → in 3..=5.
        // STRATUM_OUTPOST must be set.
        let pos_outpost =
            Position::from_fen("4k3/8/8/3N4/2P1P3/8/8/4K3 w - - 0 1").expect("fixture FEN");
        let bits = strata_for(&pos_outpost);
        assert!(
            bits & STRATUM_OUTPOST != 0,
            "White Nd5 on pawn-supported/unchallengeable d5 (rel-rank 4) \
             → STRATUM_OUTPOST must be set; got bits={bits:#04x}"
        );

        // Negative fixture: White Nd5, no supporting pawns → not pawn-supported
        // → STRATUM_OUTPOST must NOT be set.
        let pos_no_outpost =
            Position::from_fen("4k3/8/8/3N4/8/8/8/4K3 w - - 0 1").expect("fixture FEN");
        let bits_no = strata_for(&pos_no_outpost);
        assert!(
            bits_no & STRATUM_OUTPOST == 0,
            "White Nd5 without supporting pawns → STRATUM_OUTPOST must NOT be set; \
             got bits={bits_no:#04x}"
        );

        // Negative fixture: challengeable by black pawn (e7 → black_attack_front_spans covers d5).
        let pos_challenged =
            Position::from_fen("4k3/4p3/8/3N4/2P1P3/8/8/4K3 w - - 0 1").expect("fixture FEN");
        let bits_challenged = strata_for(&pos_challenged);
        assert!(
            bits_challenged & STRATUM_OUTPOST == 0,
            "White Nd5 challenged by black pawn e7 → STRATUM_OUTPOST must NOT be set; \
             got bits={bits_challenged:#04x}"
        );

        // This module uses a FROZEN snapshot — the frozen_outpost_squares fn
        // and frozen_relative_rank fn are internal to this module and do NOT
        // call eval::tier1::outpost_squares. Mutating the live eval::tier1
        // functions cannot affect an already-computed CorpusRecord::strata
        // field (stored at build time). This invariant is structural; the
        // positive/negative fixtures above pin the predicate semantics.
    }

    // ---------------------------------------------------------------------------
    // frozen_outpost_squares_byte_equals_live_at_m6f_snapshot
    //
    // The corpus stratum definition is a FROZEN snapshot of M6.F's
    // `eval::tier1::outpost_squares`. At M6.F-snapshot time the frozen and
    // live impls must agree byte-for-byte over every position; if they
    // disagree NOW, the snapshot already drifted (the structural argument
    // doesn't cover that backward direction — only "M6.H can't break a
    // built record"). After M6.H this test will start failing on positions
    // where the LIVE predicate has been re-tuned and the frozen snapshot
    // intentionally lags; until then this is the equality pin.
    // ---------------------------------------------------------------------------
    #[test]
    fn frozen_outpost_squares_byte_equals_live_at_m6f_snapshot() {
        use crate::bench::BENCH_POSITIONS;
        use crate::eval::tier1;

        for fen in BENCH_POSITIONS.iter() {
            let pos = Position::from_fen(fen).expect("BENCH_POSITIONS parse");
            for side in [Color::White, Color::Black] {
                let frozen = frozen_outpost_squares(&pos, side);
                let live = tier1::outpost_squares(&pos, side);
                assert_eq!(
                    frozen.0, live.0,
                    "frozen outpost_squares ≠ live at M6.F snapshot \
                     (fen={fen}, side={side:?}, frozen={:#x}, live={:#x})",
                    frozen.0, live.0
                );
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Additional: endgame stratum threshold correctness.
    // ---------------------------------------------------------------------------
    #[test]
    fn endgame_stratum_fires_below_mop_up_threshold() {
        // Startpos has raw_phase = 24 (full material), which is >= MOP_UP_PHASE_MAX=5
        // → STRATUM_ENDGAME must NOT be set.
        let pos_start = Position::starting_position();
        let bits = strata_for(&pos_start);
        assert!(
            bits & STRATUM_ENDGAME == 0,
            "startpos raw_phase=24 ≥ MOP_UP_PHASE_MAX=5 → STRATUM_ENDGAME must be 0; \
             got bits={bits:#04x}"
        );

        // KvK endgame (bare kings): raw_phase = 0 < 5 → STRATUM_ENDGAME must be set.
        // Kings only, no other pieces.
        let pos_kvk = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN");
        let bits_kvk = strata_for(&pos_kvk);
        assert!(
            bits_kvk & STRATUM_ENDGAME != 0,
            "KvK raw_phase=0 < MOP_UP_PHASE_MAX=5 → STRATUM_ENDGAME must be set; \
             got bits={bits_kvk:#04x}"
        );
    }
}
