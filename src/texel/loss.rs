//! Sigmoid/MSE Texel objective + closed-form gradient + L2/monotonicity reg
//! (ADR-0037 §5).
//!
//! Loss = mean of `(label − σ(K·score_white(w)))²`. Because `score` is linear
//! in `w` the gradient is closed-form:
//! `∂L/∂wₖ += 2(σ−label)·σ(1−σ)·K·featₖ` + the L2-toward-init term
//! `2·l2_lambda·(wₖ−initₖ)` + the monotonicity-penalty grad on the
//! structurally-monotonic table groups.
//!

use std::path::Path;

use crate::corpus::Label;
use crate::texel::dataset::{CachedRec, stream_chunks};
use crate::texel::layout::{self, Group};

/// Regularization config (ADR-0037 §5). `init` is the shipped core vec — the
/// L2 ridge pulls toward production, never toward literature values.
pub struct Reg {
    /// L2 ridge strength (pull toward [`Reg::init`]).
    pub l2_lambda: f64,
    /// L2 target = the shipped core vec.
    pub init: Vec<f64>,
    /// Monotonicity-smoothing strength on the monotonic table groups.
    pub mono_lambda: f64,
}

/// Logistic sigmoid, clamped to avoid `exp` overflow at extreme scores (mirrors
/// [`crate::corpus::objective`]'s clamp so the two losses agree exactly).
#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x.clamp(-60.0, 60.0)).exp())
}

/// Accumulates loss and the closed-form gradient over `chunk` at weights `w`
/// and scaling `k`, adding the regularization grad. Returns `(sum_loss,
/// count)` — `sum_loss` is the SUMMED data SSE plus the regularization penalty
/// (so `sum_loss / count` is the mean total loss), `count` is the record count.
///
/// Per record `s = base + Σ coeff·w[idx]`, `σ = sigmoid(k·s)`, `err = σ − label`;
/// the data SSE accumulates `err²`. The gradient (mean over the chunk, plus the
/// per-call regularization grad) is closed-form because `s` is linear in `w`:
/// `∂(mean SSE)/∂w[idx] += 2·err·σ·(1−σ)·k·coeff / n`. The L2-toward-init term
/// adds `2·l2_lambda·(w[idx]−init[idx])`; the monotonicity penalty adds its grad
/// on the monotone table groups (ADR-0037 §5).
pub fn loss_and_grad(
    chunk: &[CachedRec],
    w: &[f64],
    k: f64,
    reg: &Reg,
    grad: &mut [f64],
) -> (f64, usize) {
    let n = chunk.len();
    let mut sse = 0.0f64;
    let inv_n = if n == 0 { 0.0 } else { 1.0 / n as f64 };

    for rec in chunk {
        let mut s = rec.base as f64;
        for &(idx, c) in rec.coeffs.iter() {
            s += c as f64 * w[idx as usize];
        }
        let sig = sigmoid(k * s);
        let label = Label::from_u8(rec.label)
            .expect("valid label byte")
            .as_f64();
        let err = sig - label;
        sse += err * err;
        // d(err²)/ds = 2·err·σ·(1−σ)·k; chain through coeff for each weight,
        // then take the per-record contribution to the MEAN-loss gradient.
        let ds = 2.0 * err * sig * (1.0 - sig) * k * inv_n;
        for &(idx, c) in rec.coeffs.iter() {
            grad[idx as usize] += ds * c as f64;
        }
    }

    // Regularization penalty + its gradient. The penalty is added to the
    // returned scalar UN-divided so `(sse + penalty)/count` is the mean total
    // loss; the gradient is scaled by `1/n` to stay consistent with that mean
    // (the FD-from-returned-loss check differentiates `loss/count`).
    let reg_penalty = reg_loss_and_grad(w, reg, inv_n, grad);

    (sse + reg_penalty, n)
}

/// Adds the regularization gradient (scaled by `inv_n` to match the mean-loss
/// convention) into `grad` and returns the UN-divided regularization penalty.
/// L2-toward-init + monotonicity on the structurally-monotone table groups
/// (mobility-by-count, passed-by-rank, conn-by-rank).
fn reg_loss_and_grad(w: &[f64], reg: &Reg, inv_n: f64, grad: &mut [f64]) -> f64 {
    let mut penalty = 0.0f64;

    if reg.l2_lambda != 0.0 {
        for (i, &wi) in w.iter().enumerate() {
            let d = wi - reg.init[i];
            penalty += reg.l2_lambda * d * d;
            grad[i] += 2.0 * reg.l2_lambda * d * inv_n;
        }
    }

    if reg.mono_lambda != 0.0 {
        for seq in monotone_sequences() {
            for pair in seq.windows(2) {
                let (lo, hi) = (pair[0], pair[1]);
                // Penalize a DECREASE along the monotone direction: the table
                // should be non-decreasing in rank / count.
                let diff = w[hi] - w[lo];
                if diff < 0.0 {
                    penalty += reg.mono_lambda * diff * diff;
                    let g = 2.0 * reg.mono_lambda * diff * inv_n;
                    grad[hi] += g;
                    grad[lo] -= g;
                }
            }
        }
    }

    penalty
}

/// The monotone index sequences within the structurally-monotone groups: each
/// inner `Vec` is a run of core indices that should be non-decreasing (mobility
/// MG/EG blocks per kind, CONN MG/EG rank runs, passed MG/EG rank runs). A
/// decrease along a run is penalized (ADR-0037 §5).
fn monotone_sequences() -> Vec<Vec<usize>> {
    let mut seqs: Vec<Vec<usize>> = Vec::new();

    // Mobility: each kind's MG block and EG block is a popcount run.
    for g in [Group::MobN, Group::MobB, Group::MobR, Group::MobQ] {
        let r = layout::group_range(g);
        let half = (r.end - r.start) / 2;
        seqs.push((r.start..r.start + half).collect());
        seqs.push((r.start + half..r.end).collect());
    }

    // CONN: MG[2..8] then EG[2..8] — two rank runs.
    let conn = layout::group_range(Group::Conn);
    let conn_half = (conn.end - conn.start) / 2;
    seqs.push((conn.start..conn.start + conn_half).collect());
    seqs.push((conn.start + conn_half..conn.end).collect());

    // Passed: MG[2..7] (5) then EG[2..8] (6) — two rank runs (the per-step
    // king-tropism + free-path-delta tail is not a monotone rank table).
    let passed = layout::group_range(Group::Passed);
    seqs.push((passed.start..passed.start + 5).collect());
    seqs.push((passed.start + 5..passed.start + 11).collect());

    seqs
}

/// Fits the Texel scaling constant K by golden-section over a feature-backed
/// score closure on the cache, at weights `w` (ADR-0037 §6 — fit once on init).
///
/// Mirrors [`crate::corpus::objective::fit_k`]'s golden-section over `K ∈ (0,
/// 2]`, scoring each cached record by the deployment-faithful integer-rounded
/// `round(base + Σ coeff·w[idx])` (the cache stream is read once per loss eval).
pub fn fit_k_features(cache: &Path, w: &[f64]) -> f64 {
    const LO: f64 = 1e-6;
    const HI: f64 = 2.0;
    const ITERS: usize = 60;
    const PHI: f64 = 0.618_033_988_749_895; // 1/φ

    // Materialize integer-rounded cached scores + labels once (the score does
    // not depend on K, so the golden-section reuses them across iterations).
    let mut scores: Vec<i32> = Vec::new();
    let mut labels: Vec<f64> = Vec::new();
    for chunk in stream_chunks(cache, 65_536) {
        for rec in &chunk {
            let mut s = rec.base as f64;
            for &(idx, c) in rec.coeffs.iter() {
                s += c as f64 * w[idx as usize];
            }
            scores.push(s.round() as i32);
            labels.push(
                Label::from_u8(rec.label)
                    .expect("valid label byte")
                    .as_f64(),
            );
        }
    }
    if scores.is_empty() {
        return 1.0;
    }

    let loss = |k: f64| -> f64 {
        let sum: f64 = scores
            .iter()
            .zip(labels.iter())
            .map(|(&s, &lab)| {
                let pred = sigmoid(k * s as f64);
                let d = lab - pred;
                d * d
            })
            .sum();
        sum / scores.len() as f64
    };

    let mut a = LO;
    let mut b = HI;
    let mut c = b - PHI * (b - a);
    let mut d = a + PHI * (b - a);
    let mut fc = loss(c);
    let mut fd = loss(d);
    for _ in 0..ITERS {
        if fc < fd {
            b = d;
            d = c;
            fd = fc;
            c = b - PHI * (b - a);
            fc = loss(c);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + PHI * (b - a);
            fd = loss(d);
        }
    }
    (a + b) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::objective::{self, logistic_loss};
    use crate::corpus::prng::Prng;
    use crate::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};
    use crate::texel::layout::{self, N_CORE};
    use std::io::Write;

    // -----------------------------------------------------------------------
    // Synthetic cached-record model (the cache-score contract under test).
    //
    // `CachedRec` carries `base: f32` (the frozen reference-frame contribution,
    // already phase-blended at identity scale) + sparse `coeffs: [(idx, c)]`.
    // The cached linear score is `score(rec, w) = base + Σ_k coeff_k · w[idx_k]`
    // and the loss-and-gradient operate on it as a pure linear model — the
    // phase fold is BAKED INTO the cached `coeff` at build time (`CachedRec` has
    // no phase field, unlike `FeatureVec`). So `featₖ = coeff_k` directly, which
    // is exactly what the ADR-0037 §5 gradient `∂L/∂wₖ += 2(σ−label)σ(1−σ)K·featₖ`
    // consumes. The FD check below validates the analytic gradient against this
    // linear score definition.
    // -----------------------------------------------------------------------

    /// White-POV cached-model score: `base + Σ coeff·w[idx]` (the linear model
    /// the closed-form gradient differentiates).
    fn cache_score(rec: &CachedRec, w: &[f64]) -> f64 {
        let mut s = rec.base as f64;
        for &(idx, c) in rec.coeffs.iter() {
            s += c as f64 * w[idx as usize];
        }
        s
    }

    fn sigmoid(x: f64) -> f64 {
        1.0 / (1.0 + (-x.clamp(-60.0, 60.0)).exp())
    }

    /// MSE Texel loss over a chunk at weights `w`, scaling `k`, NO regularization
    /// (the unregularized oracle for `loss_matches_objective_logistic_loss`).
    fn data_loss(chunk: &[CachedRec], w: &[f64], k: f64) -> f64 {
        if chunk.is_empty() {
            return 0.0;
        }
        let sum: f64 = chunk
            .iter()
            .map(|r| {
                let pred = sigmoid(k * cache_score(r, w));
                let target = Label::from_u8(r.label).expect("valid label").as_f64();
                (target - pred).powi(2)
            })
            .sum();
        sum / chunk.len() as f64
    }

    /// Build a synthetic chunk of cached records with sparse, deterministic
    /// coeffs spanning multiple groups so the gradient touches every group.
    fn synthetic_chunk(seed: u64, n: usize) -> Vec<CachedRec> {
        let mut rng = Prng::new(seed);
        (0..n)
            .map(|i| {
                // 4–8 sparse coeffs at distinct random core indices.
                let k = 4 + (rng.below(5) as usize);
                let mut seen = std::collections::BTreeSet::new();
                while seen.len() < k {
                    seen.insert(rng.below(N_CORE as u64) as u16);
                }
                let coeffs: Vec<(u16, f32)> = seen
                    .into_iter()
                    .map(|idx| {
                        let c = (rng.below(7) as i64 - 3) as f32; // [-3, +3]
                        (idx, c)
                    })
                    .collect();
                let label = match rng.below(3) {
                    0 => Label::WhiteWin,
                    1 => Label::Draw,
                    _ => Label::BlackWin,
                };
                CachedRec {
                    base: (rng.below(401) as f64 - 200.0) as f32, // [-200, +200] cp
                    coeffs: coeffs.into_boxed_slice(),
                    label: label.as_u8(),
                    source: Source::SelfPlayOffBook.as_u8(),
                    strata: 0,
                    depth_rung: DEPTH_RUNG_EXTERNAL,
                    game_id: i as u64,
                }
            })
            .collect()
    }

    fn no_reg() -> Reg {
        Reg {
            l2_lambda: 0.0,
            init: vec![0.0; N_CORE],
            mono_lambda: 0.0,
        }
    }

    // -----------------------------------------------------------------------
    // THE gradient gate: analytic vs central-difference, all groups.
    // -----------------------------------------------------------------------

    /// `loss_and_grad`'s analytic gradient matches a central finite-difference
    /// of the same loss across ALL core indices, on a synthetic cached chunk
    /// (ADR-0037 §5 — the gradient gate). Unregularized so the loss closure is
    /// exactly `data_loss`.
    #[test]
    fn closed_form_gradient_matches_finite_difference() {
        let chunk = synthetic_chunk(0xA11CE, 200);
        let k = 0.01;
        let reg = no_reg();
        let mut rng = Prng::new(0x6AAD);
        // A non-trivial weight point (not all-zero — the sigmoid slope varies).
        let w: Vec<f64> = (0..N_CORE)
            .map(|_| (rng.below(2001) as f64 - 1000.0) / 500.0)
            .collect();

        let mut grad = vec![0.0f64; N_CORE];
        let (loss, count) = loss_and_grad(&chunk, &w, k, &reg, &mut grad);
        assert_eq!(
            count,
            chunk.len(),
            "loss_and_grad count must equal chunk len"
        );
        assert!(
            (loss / count as f64 - data_loss(&chunk, &w, k)).abs() < 1e-9,
            "returned summed loss / count must equal the MSE oracle"
        );

        // Central difference on every coordinate.
        const H: f64 = 1e-5;
        // loss_and_grad returns the SUMMED loss; the per-record-mean gradient
        // the impl returns is dL_mean/dw. Compare against the mean-loss FD.
        let mean_loss = |w: &[f64]| -> f64 { data_loss(&chunk, w, k) };
        for idx in 0..N_CORE {
            let mut wp = w.clone();
            let mut wm = w.clone();
            wp[idx] += H;
            wm[idx] -= H;
            let fd = (mean_loss(&wp) - mean_loss(&wm)) / (2.0 * H);
            assert!(
                (grad[idx] - fd).abs() < 1e-4,
                "gradient mismatch at core idx {idx} (group {:?}): \
                 analytic={}, fd={fd}",
                layout::group_of(idx),
                grad[idx]
            );
        }
    }

    /// With non-zero `l2_lambda` (init = shipped core) AND `mono_lambda`, the
    /// analytic gradient still matches central differences of the TOTAL loss
    /// (data + reg). The total loss is recovered from `loss_and_grad`'s own
    /// returned scalar at each perturbed point, so this test does not need to
    /// hard-code the monotonicity-penalty shape (ADR-0037 §5).
    #[test]
    fn l2_and_monotonicity_gradients_match_finite_difference() {
        use crate::texel::params::EvalParams;
        let chunk = synthetic_chunk(0xB0B, 150);
        let k = 0.01;
        let reg = Reg {
            l2_lambda: 5e-3,
            init: EvalParams::shipped().core_to_vec(),
            mono_lambda: 1e-2,
        };
        let mut rng = Prng::new(0xC0FFEE);
        let w: Vec<f64> = (0..N_CORE)
            .map(|_| (rng.below(2001) as f64 - 1000.0) / 500.0)
            .collect();

        // Helper that returns the MEAN total loss at a point via loss_and_grad
        // itself (loss is returned as a summed value; divide by count).
        let total_mean = |w: &[f64]| -> f64 {
            let mut g = vec![0.0f64; N_CORE];
            let (loss, count) = loss_and_grad(&chunk, w, k, &reg, &mut g);
            loss / count as f64
        };

        let mut grad = vec![0.0f64; N_CORE];
        let _ = loss_and_grad(&chunk, &w, k, &reg, &mut grad);

        const H: f64 = 1e-5;
        // Spot-check a representative index in each group (full sweep is the
        // unregularized test above; here we add the reg terms across groups).
        let probe: Vec<usize> = [
            layout::Group::IsoDblBwd,
            layout::Group::Conn,
            layout::Group::Passed,
            layout::Group::MobN,
            layout::Group::MobQ,
            layout::Group::KsShieldFile,
            layout::Group::Outpost,
            layout::Group::RookFile,
        ]
        .iter()
        .map(|&g| layout::group_range(g).start)
        .collect();

        for &idx in &probe {
            let mut wp = w.clone();
            let mut wm = w.clone();
            wp[idx] += H;
            wm[idx] -= H;
            let fd = (total_mean(&wp) - total_mean(&wm)) / (2.0 * H);
            assert!(
                (grad[idx] - fd).abs() < 1e-3,
                "reg gradient mismatch at core idx {idx} (group {:?}): \
                 analytic={}, fd={fd}",
                layout::group_of(idx),
                grad[idx]
            );
        }
    }

    /// A single zero-coeff `base=0`, `Draw`-labeled record: the data term is
    /// exactly 0 (σ(k·0)=0.5=label ⇒ err=0; no coeffs ⇒ no data gradient), so
    /// `loss_and_grad` isolates the regularization penalty + gradient with
    /// `inv_n = 1`. Used by `reg_loss_and_grad_pins_*` to read the reg term
    /// straight off the returned scalar / `grad`.
    fn data_inert_unit_chunk() -> Vec<CachedRec> {
        vec![CachedRec {
            base: 0.0,
            coeffs: Vec::new().into_boxed_slice(),
            label: Label::Draw.as_u8(),
            source: Source::SelfPlayOffBook.as_u8(),
            strata: 0,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            game_id: 0,
        }]
    }

    /// VALUE-PINNED L2 reg: with `mono_lambda = 0` and `w` differing from `init`
    /// at exactly two indices, the returned penalty AND each gradient component
    /// equal hand-derived numbers. The data term is inert (see
    /// `data_inert_unit_chunk`), so `loss == penalty` and `grad` is the pure L2
    /// gradient at `inv_n = 1`.
    ///
    /// Penalty `= l2·Σ(w−init)²`; grad `= 2·l2·(w−init)·inv_n` (ADR-0037 §5).
    /// This BREAKS the gradient↔loss self-consistency tautology of
    /// `l2_and_monotonicity_gradients_match_finite_difference`: a mutant that
    /// flips `+`→`-`, `*`→`/`, or the sign of the L2 term changes the absolute
    /// numbers pinned here even though it might stay FD-self-consistent.
    #[test]
    fn reg_loss_and_grad_pins_l2_values() {
        let chunk = data_inert_unit_chunk();
        let k = 0.01;

        let init = vec![1.0f64; N_CORE];
        let mut w = init.clone();
        w[0] = 4.0; // d = +3
        w[1] = -1.0; // d = -2
        let reg = Reg {
            l2_lambda: 0.5,
            init: init.clone(),
            mono_lambda: 0.0,
        };

        let mut grad = vec![0.0f64; N_CORE];
        let (loss, count) = loss_and_grad(&chunk, &w, k, &reg, &mut grad);
        assert_eq!(count, 1);

        // penalty = 0.5·(3² + (−2)²) = 0.5·13 = 6.5; data term is 0.
        assert!(
            (loss - 6.5).abs() < 1e-12,
            "L2 penalty must be 0.5·(9+4)=6.5, got {loss}"
        );
        // grad[i] = 2·0.5·d·1 = d.
        assert!(
            (grad[0] - 3.0).abs() < 1e-12,
            "grad[0] must be +3, got {}",
            grad[0]
        );
        assert!(
            (grad[1] - (-2.0)).abs() < 1e-12,
            "grad[1] must be −2, got {}",
            grad[1]
        );
        // Every other index has d=0 ⇒ zero L2 gradient.
        for (i, &g) in grad.iter().enumerate() {
            if i != 0 && i != 1 {
                assert_eq!(g, 0.0, "grad[{i}] must be 0 (init==w there)");
            }
        }
    }

    /// VALUE-PINNED monotonicity reg: with `l2_lambda = 0` and a single
    /// decreasing run injected into the CONN-MG monotone group (`w[6]=5, w[7]=1`,
    /// all else 0), the returned penalty AND each touched gradient component
    /// equal hand-derived numbers. The data term is inert, so `loss == penalty`
    /// and `grad` is the pure monotonicity gradient at `inv_n = 1`.
    ///
    /// CONN-MG run is indices `[6,7,8,9,10,11]`. Violating pairs:
    /// (6,7) diff=1−5=−4 ⇒ pen += 0.25·16, grad[7]+=2·0.25·(−4), grad[6]−=…;
    /// (7,8) diff=0−1=−1 ⇒ pen += 0.25·1,  grad[8]+=2·0.25·(−1), grad[7]−=…;
    /// (8,9)…(10,11) diff=0 ⇒ skipped (`diff < 0` is false). Pins the
    /// monotonicity `+=` penalty, the `*` factors, and the `grad[hi]+= /
    /// grad[lo]-=` sign split. (The `< → <=` guard mutant is equivalent — the
    /// diff==0 contribution is exactly 0 — and is excluded in `mutants.toml`.)
    #[test]
    fn reg_loss_and_grad_pins_monotonicity_values() {
        let chunk = data_inert_unit_chunk();
        let k = 0.01;

        let mut w = vec![0.0f64; N_CORE];
        w[6] = 5.0;
        w[7] = 1.0;
        let reg = Reg {
            l2_lambda: 0.0,
            init: vec![0.0; N_CORE],
            mono_lambda: 0.25,
        };

        // Pin the assumed CONN-MG run so a layout drift fails loudly here.
        assert_eq!(
            monotone_sequences()[8],
            vec![6, 7, 8, 9, 10, 11],
            "CONN-MG monotone run drifted; the hand-derived values below assume [6..12]"
        );

        let mut grad = vec![0.0f64; N_CORE];
        let (loss, count) = loss_and_grad(&chunk, &w, k, &reg, &mut grad);
        assert_eq!(count, 1);

        // penalty = 0.25·(4² + 1²) = 0.25·17 = 4.25.
        assert!(
            (loss - 4.25).abs() < 1e-12,
            "monotonicity penalty must be 0.25·(16+1)=4.25, got {loss}"
        );
        // grad[6] = −(2·0.25·−4) = +2.0
        // grad[7] = (2·0.25·−4) + −(2·0.25·−1) = −2.0 + 0.5 = −1.5
        // grad[8] = (2·0.25·−1) = −0.5
        assert!(
            (grad[6] - 2.0).abs() < 1e-12,
            "grad[6] must be +2.0, got {}",
            grad[6]
        );
        assert!(
            (grad[7] - (-1.5)).abs() < 1e-12,
            "grad[7] must be −1.5, got {}",
            grad[7]
        );
        assert!(
            (grad[8] - (-0.5)).abs() < 1e-12,
            "grad[8] must be −0.5, got {}",
            grad[8]
        );
        for (i, &g) in grad.iter().enumerate() {
            if i != 6 && i != 7 && i != 8 {
                assert_eq!(g, 0.0, "grad[{i}] must be 0 (no violating pair touches it)");
            }
        }
    }

    /// VALUE-PINNED: `monotone_sequences` returns the EXACT layout-index runs for
    /// the structurally-monotone tables (mobility MG/EG per kind, CONN MG/EG
    /// rank runs, passed MG/EG rank runs), hand-derived from `texel::layout`.
    /// Asserts the full `Vec<Vec<usize>>` (not just lengths), killing the
    /// index-arithmetic survivors (`/`→`%`, `-`→`/`, `+`→`-`, the `/ 2` halving).
    #[test]
    fn monotone_sequences_returns_exact_index_groups() {
        let expected: Vec<Vec<usize>> = vec![
            // MobN 37..55, half=9: MG[37..46], EG[46..55].
            (37..46).collect(),
            (46..55).collect(),
            // MobB 55..83, half=14: MG[55..69], EG[69..83].
            (55..69).collect(),
            (69..83).collect(),
            // MobR 83..113, half=15: MG[83..98], EG[98..113].
            (83..98).collect(),
            (98..113).collect(),
            // MobQ 113..169, half=28: MG[113..141], EG[141..169].
            (113..141).collect(),
            (141..169).collect(),
            // Conn 6..18, half=6: MG[6..12], EG[12..18].
            (6..12).collect(),
            (12..18).collect(),
            // Passed 18..37: MG[18..23] (5), EG[23..29] (6).
            (18..23).collect(),
            (23..29).collect(),
        ];
        assert_eq!(
            monotone_sequences(),
            expected,
            "monotone_sequences must return the exact layout-index runs"
        );
    }

    /// `loss_and_grad`'s (mean) loss equals `corpus::objective::logistic_loss`
    /// when the cached score is fed back as the score closure. Uses
    /// integer-valued cached scores (so the int-vs-float quantization the
    /// deployment K-fit assumes is exercised) and reuses the corpus oracle.
    #[test]
    fn loss_matches_objective_logistic_loss_at_integer_scores() {
        // Integer cached scores: zero coeffs (so cache_score == base, an
        // integer-valued f32), and labels by sign of base.
        let bases = [-200i32, -50, 0, 30, 150, 400];
        let chunk: Vec<CachedRec> = bases
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                let label = if b > 50 {
                    Label::WhiteWin
                } else if b < -50 {
                    Label::BlackWin
                } else {
                    Label::Draw
                };
                CachedRec {
                    base: b as f32,
                    coeffs: Vec::new().into_boxed_slice(),
                    label: label.as_u8(),
                    source: Source::SelfPlayOffBook.as_u8(),
                    strata: 0,
                    depth_rung: DEPTH_RUNG_EXTERNAL,
                    game_id: i as u64,
                }
            })
            .collect();
        let k = 0.0123;
        let w = vec![0.0f64; N_CORE];
        let reg = no_reg();

        // The corpus oracle: build matching CorpusRecords whose score closure
        // returns the same integer cached score.
        let recs: Vec<CorpusRecord> = chunk
            .iter()
            .map(|r| CorpusRecord {
                fen: "8/8/8/8/8/8/8/8 w - - 0 1".to_string(),
                label: Label::from_u8(r.label).unwrap(),
                source: Source::SelfPlayOffBook,
                game_id: r.base as i64 as u64, // carry the integer score
                ply: 0,
                depth_rung: DEPTH_RUNG_EXTERNAL,
                strata: 0,
            })
            .collect();
        let oracle = logistic_loss(&recs, k, &|r: &CorpusRecord| r.game_id as i64 as i32);

        let mut grad = vec![0.0f64; N_CORE];
        let (loss, count) = loss_and_grad(&chunk, &w, k, &reg, &mut grad);
        let mean = loss / count as f64;
        assert!(
            (mean - oracle).abs() < 1e-12,
            "loss_and_grad mean loss ({mean}) must equal objective::logistic_loss ({oracle})"
        );
    }

    // -----------------------------------------------------------------------
    // fit_k_features: golden-section K fit on a feature-cache stream.
    // -----------------------------------------------------------------------

    /// The `dataset` layout-contract hash, recomputed here so a directly-written
    /// cache header passes `read_header`'s layout check (the same `DefaultHasher`
    /// over `N_CORE` + per-index `is_mg`). Recomputed rather than imported
    /// because the producer is private; if the layout contract changes, this and
    /// the real cache writer move together (both hash the same inputs).
    fn layout_contract_hash() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        layout::N_CORE.hash(&mut h);
        for i in 0..layout::N_CORE {
            layout::is_mg(i).hash(&mut h);
        }
        h.finish()
    }

    /// Writes a feature cache with the given records, mirroring
    /// `dataset::write_cache_file` framing so `fit_k_features`'s `stream_chunks`
    /// reads it back. Header: `MAGIC | N | layout_hash | 4×(sha_len, sha)`;
    /// record: `base:f32 | n_coeff:u32 | n×(idx:u16, coeff:f32) | label | source
    /// | strata | depth_rung | game_id:u64` (all little-endian).
    fn write_test_cache(path: &Path, recs: &[CachedRec]) {
        const MAGIC: u32 = 0x5458_4331;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&(recs.len() as u64).to_le_bytes());
        buf.extend_from_slice(&layout_contract_hash().to_le_bytes());
        for sha in ["", "", "", ""] {
            buf.extend_from_slice(&(sha.len() as u32).to_le_bytes());
            buf.extend_from_slice(sha.as_bytes());
        }
        for r in recs {
            buf.extend_from_slice(&r.base.to_le_bytes());
            buf.extend_from_slice(&(r.coeffs.len() as u32).to_le_bytes());
            for &(idx, coeff) in r.coeffs.iter() {
                buf.extend_from_slice(&idx.to_le_bytes());
                buf.extend_from_slice(&coeff.to_le_bytes());
            }
            buf.extend_from_slice(&[r.label, r.source, r.strata, r.depth_rung]);
            buf.extend_from_slice(&r.game_id.to_le_bytes());
        }
        let mut f = std::fs::File::create(path).expect("create cache");
        f.write_all(&buf).expect("write cache");
    }

    fn int_score_rec(base: i32, label: Label, game_id: u64) -> CachedRec {
        coeff_score_rec(base, &[], label, game_id)
    }

    fn coeff_score_rec(base: i32, coeffs: &[(u16, f32)], label: Label, game_id: u64) -> CachedRec {
        CachedRec {
            base: base as f32,
            coeffs: coeffs.to_vec().into_boxed_slice(),
            label: label.as_u8(),
            source: Source::SelfPlayOffBook.as_u8(),
            strata: 0,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            game_id,
        }
    }

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-texel-loss-{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// `fit_k_features` over a feature cache with zero `w` (so each record's
    /// integer score is `round(base)`) returns the SAME K as
    /// `corpus::objective::fit_k` on the equivalent integer scores, AND that K
    /// achieves a STRICTLY LOWER sigmoid-MSE than the golden-section bracket
    /// endpoints (`K→0⁺` and `K=2`). The fixture is a non-flat, clearly-unimodal
    /// MSE-vs-K with its minimum well inside `(0, 2]`. A golden-section mutant
    /// that returns an endpoint or mis-converges produces a wrong K or a loss no
    /// better than the endpoints → caught.
    #[test]
    fn fit_k_features_matches_objective_fit_k() {
        // Scores well-separated by label so the MSE-vs-K minimum is interior:
        // strong White scores → win, mirror Black scores → loss, near-zero →
        // draw. Optimal K is the small slope that best matches the sigmoid.
        let recs = vec![
            int_score_rec(250, Label::WhiteWin, 0),
            int_score_rec(180, Label::WhiteWin, 1),
            int_score_rec(90, Label::WhiteWin, 2),
            int_score_rec(20, Label::Draw, 3),
            int_score_rec(0, Label::Draw, 4),
            int_score_rec(-20, Label::Draw, 5),
            int_score_rec(-90, Label::BlackWin, 6),
            int_score_rec(-180, Label::BlackWin, 7),
            int_score_rec(-250, Label::BlackWin, 8),
        ];

        let dir = unique_temp_dir("fit-k");
        let cache = dir.join("features.cache");
        write_test_cache(&cache, &recs);

        let w = vec![0.0f64; N_CORE];
        let k_features = fit_k_features(&cache, &w);

        // The corpus oracle over the SAME integer scores (carried in game_id's
        // low bits via base; here base is already integer-valued).
        let corpus_recs: Vec<CorpusRecord> = recs
            .iter()
            .map(|r| CorpusRecord {
                fen: "8/8/8/8/8/8/8/8 w - - 0 1".to_string(),
                label: Label::from_u8(r.label).unwrap(),
                source: Source::SelfPlayOffBook,
                game_id: r.base as i64 as u64,
                ply: 0,
                depth_rung: DEPTH_RUNG_EXTERNAL,
                strata: 0,
            })
            .collect();
        let score = |r: &CorpusRecord| r.game_id as i64 as i32;
        let k_oracle = objective::fit_k(&corpus_recs, &score);

        assert!(
            (k_features - k_oracle).abs() < 1e-6,
            "fit_k_features ({k_features}) must match objective::fit_k ({k_oracle}) \
             on the equivalent integer scores"
        );

        // The minimum is interior: the fitted K beats both bracket endpoints.
        let mse_at = |k: f64| objective::logistic_loss(&corpus_recs, k, &score);
        let loss_fit = mse_at(k_features);
        let loss_lo = mse_at(1e-6);
        let loss_hi = mse_at(2.0);
        assert!(
            loss_fit < loss_lo && loss_fit < loss_hi,
            "fitted-K loss ({loss_fit}) must be strictly below both bracket \
             endpoints (LO={loss_lo}, HI={loss_hi}) — minimum is interior"
        );
        // And the K is genuinely interior (not pinned at an endpoint).
        assert!(
            k_features > 1e-3 && k_features < 1.9,
            "fitted K ({k_features}) must be interior, not at a bracket endpoint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `fit_k_features` scores each record by the full `round(base + Σ
    /// coeff·w[idx])` — NOT just `base`. With NON-ZERO coeffs AND non-zero `w`,
    /// the fitted K equals `objective::fit_k` over the SAME hand-computed integer
    /// scores. This exercises (and pins) the score-accumulation `s += c·w[idx]`:
    /// a `+=`→`-=` or `*`→`+` mutant changes each record's integer score, hence
    /// the fitted K, away from the oracle. Zero-coeff fixtures (above) cannot
    /// catch these because `c·w == 0` collapses the loop body.
    #[test]
    fn fit_k_features_uses_coeff_weighted_score() {
        struct Spec {
            base: i32,
            coeffs: &'static [(u16, f32)],
            label: Label,
        }
        // w nonzero at two indices; each record's coeffs reference them so the
        // integer score genuinely depends on `base + Σ c·w` (and `c·w ≠ c+w`).
        let mut w = vec![0.0f64; N_CORE];
        w[10] = 30.0;
        w[40] = -12.0;
        // Score = base + 30·c10 − 12·c40.
        let specs = [
            Spec {
                base: 40,
                coeffs: &[(10, 6.0), (40, 1.0)],
                label: Label::WhiteWin,
            }, // 208
            Spec {
                base: 10,
                coeffs: &[(10, 4.0)],
                label: Label::WhiteWin,
            }, // 130
            Spec {
                base: 0,
                coeffs: &[(40, 3.0)],
                label: Label::Draw,
            }, // −36
            Spec {
                base: 5,
                coeffs: &[(10, 1.0), (40, 2.0)],
                label: Label::Draw,
            }, // 11
            Spec {
                base: -30,
                coeffs: &[(40, 5.0)],
                label: Label::BlackWin,
            }, // −90
            Spec {
                base: -50,
                coeffs: &[(10, -4.0), (40, 1.0)],
                label: Label::BlackWin,
            }, // −182
        ];
        let recs: Vec<CachedRec> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| coeff_score_rec(s.base, s.coeffs, s.label, i as u64))
            .collect();

        let dir = unique_temp_dir("fit-k-coeff");
        let cache = dir.join("features.cache");
        write_test_cache(&cache, &recs);

        let k_features = fit_k_features(&cache, &w);

        // Hand-compute the integer scores the SAME way fit_k_features does:
        // round(base + Σ coeff·w[idx]).
        let int_scores: Vec<i32> = specs
            .iter()
            .map(|s| {
                let acc = s.base as f64
                    + s.coeffs
                        .iter()
                        .map(|&(idx, c)| c as f64 * w[idx as usize])
                        .sum::<f64>();
                acc.round() as i32
            })
            .collect();
        // Sanity: the coeff term genuinely moved each score off `base`.
        for (i, s) in specs.iter().enumerate() {
            assert_ne!(
                int_scores[i], s.base,
                "record {i} score must depend on coeff·w, not just base"
            );
        }

        let corpus_recs: Vec<CorpusRecord> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| CorpusRecord {
                fen: "8/8/8/8/8/8/8/8 w - - 0 1".to_string(),
                label: s.label,
                source: Source::SelfPlayOffBook,
                game_id: int_scores[i] as i64 as u64,
                ply: 0,
                depth_rung: DEPTH_RUNG_EXTERNAL,
                strata: 0,
            })
            .collect();
        let score = |r: &CorpusRecord| r.game_id as i64 as i32;
        let k_oracle = objective::fit_k(&corpus_recs, &score);

        assert!(
            (k_features - k_oracle).abs() < 1e-6,
            "fit_k_features ({k_features}) over coeff-weighted scores must match \
             objective::fit_k ({k_oracle}) on the hand-computed integer scores"
        );
        assert!(
            k_features > 1e-3 && k_features < 1.9,
            "fitted K ({k_features}) must be interior"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `fit_k_features` returns the documented `1.0` fallback on an empty cache
    /// (no records ⇒ no tuning possible).
    #[test]
    fn fit_k_features_empty_cache_returns_one() {
        let dir = unique_temp_dir("fit-k-empty");
        let cache = dir.join("features.cache");
        write_test_cache(&cache, &[]);
        let w = vec![0.0f64; N_CORE];
        assert_eq!(fit_k_features(&cache, &w), 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
