//! Pentanomial-GSPRT machinery for the in-process harness.
//!
//! Math reference: docs/research/eloh.e-pentanomial-sprt.md §2.3 (per-pair
//! GSPRT formula), §5 (post-hoc Δ Elo CI), §6 (pitfalls — load-bearing for
//! the per-pair-not-per-game cadence + discard-incomplete-pair invariants).
//!
//! Logistic Elo only. Normal-approximation GSPRT (not the exact MLE form
//! used by vdbergh/pentanomial). The approximation is what cutechess-cli
//! and fastchess use under `model=logistic` and is well-calibrated for
//! pool sizes ≥ 100 pairs — well within ELOH.E's working range.

/// SPRT bounds + indifference zone configuration. Logistic Elo.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SprtConfig {
    /// H0 Elo gap. Standard chess SPRT uses 0.
    pub elo0: f64,
    /// H1 Elo gap. Standard chess SPRT uses 5–10.
    pub elo1: f64,
    /// False-positive rate. Standard 0.05.
    pub alpha: f64,
    /// False-negative rate. Standard 0.05.
    pub beta: f64,
}

/// Running pentanomial state. Indexed pair counts plus the most recent LLR.
#[derive(Debug, Clone, Default)]
pub(crate) struct SprtState {
    /// Pair counts indexed by pair-score bin (0..=4 → 0.0/0.5/1.0/1.5/2.0).
    pub pair_counts: [u32; 5],
    /// Last computed LLR. Updated by `update_pair` after each completed pair.
    pub llr: f64,
    /// Singleton games discarded because their partner game in a pair did
    /// not complete (e.g. `--max-games` boundary). Audit-only; never feeds
    /// the LLR computation. Pinned by research report §6 pitfall row 2.
    pub discarded_singletons: u32,
}

/// SPRT verdict after a single LLR check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SprtVerdict {
    /// LLR is in the indifference zone (B, A); keep playing.
    Continue,
    /// LLR ≤ B. Patch fails (H0: zero or negative Elo).
    AcceptH0,
    /// LLR ≥ A. Patch passes (H1: positive Elo).
    AcceptH1,
}

/// Wald bounds `(B, A)` for the test. Pure function of (alpha, beta).
///
/// `B = log(β / (1 - α))` (lower bound — accept H0).
/// `A = log((1 - β) / α)` (upper bound — accept H1).
pub(crate) fn wald_bounds(alpha: f64, beta: f64) -> (f64, f64) {
    let b = (beta / (1.0 - alpha)).ln();
    let a = ((1.0 - beta) / alpha).ln();
    (b, a)
}

/// Logistic Elo → expected per-game score in [0, 1].
fn ll(elo: f64) -> f64 {
    1.0 / (1.0 + 10f64.powf(-elo / 400.0))
}

/// Compute LLR from current state and config. Pure function. Per research
/// report §2.3 (pentanomial GSPRT, normal approximation).
///
/// Returns 0.0 when the pair count is 0 or the variance collapses
/// (e.g. all-draw stream); the caller treats both as "indifference zone".
pub(crate) fn compute_llr(state: &SprtState, cfg: &SprtConfig) -> f64 {
    let n: u32 = state.pair_counts.iter().sum();
    if n == 0 {
        return 0.0;
    }
    let n_f = f64::from(n);
    let pair_score = |i: usize| (i as f64) * 0.5;
    let mut sum_ns = 0.0;
    let mut sum_ns2 = 0.0;
    for (i, &count) in state.pair_counts.iter().enumerate() {
        let s = pair_score(i);
        sum_ns += f64::from(count) * s;
        sum_ns2 += f64::from(count) * s * s;
    }
    let mu = sum_ns / n_f;
    let var = sum_ns2 / n_f - mu * mu;
    if var <= 0.0 {
        return 0.0;
    }
    let var_s = var / n_f;
    let s0_pair = 2.0 * ll(cfg.elo0);
    let s1_pair = 2.0 * ll(cfg.elo1);
    (s1_pair - s0_pair) * (2.0 * mu - s0_pair - s1_pair) / var_s / 2.0
}

/// Classify a (game-A score, game-B score) pair into a pair-score bin
/// (0..=4 → 0.0/0.5/1.0/1.5/2.0). Per research report §4 truth table.
/// Inputs are per-side scores in candidate-POV (1.0/0.5/0.0).
///
/// Inputs outside `{0.0, 0.5, 1.0}` are clamped via the rounded-doubled
/// sum then bounded to `[0, 4]`; the harness only ever calls this with
/// the three valid scores, so the clamp is purely defensive.
#[allow(dead_code)] // controller uses update_pair(pair_sum) instead of calling classify_pair_score directly
pub(crate) fn classify_pair_score(game_a: f64, game_b: f64) -> usize {
    let total = game_a + game_b;
    let scaled = (total * 2.0).round() as i64;
    scaled.clamp(0, 4) as usize
}

/// Append a new pair to the state, recompute LLR, and return the verdict.
/// `pair_score` is the candidate's *total* score across the pair (0.0–2.0).
/// Caller must never call this with a singleton (use `discard_singleton`
/// for the audit-only count).
pub(crate) fn update_pair(state: &mut SprtState, cfg: &SprtConfig, pair_score: f64) -> SprtVerdict {
    let bin = ((pair_score * 2.0).round() as i64).clamp(0, 4) as usize;
    state.pair_counts[bin] = state.pair_counts[bin].saturating_add(1);
    state.llr = compute_llr(state, cfg);
    let (b, a) = wald_bounds(cfg.alpha, cfg.beta);
    if state.llr <= b {
        SprtVerdict::AcceptH0
    } else if state.llr >= a {
        SprtVerdict::AcceptH1
    } else {
        SprtVerdict::Continue
    }
}

/// Increment the audit-only singleton counter. Used at run end when an
/// in-flight pair never completes (worker failure or `--max-games` cap).
pub(crate) fn discard_singleton(state: &mut SprtState) {
    state.discarded_singletons = state.discarded_singletons.saturating_add(1);
}

/// Post-hoc 95% CI on Δ Elo from accumulated pair counts. Per research
/// report §5: pair-variance SE, normal-approximation CI on the mean pair
/// score, inverse-logistic transformation to Elo. Returns
/// `(elo_lo, elo_est, elo_hi)`.
///
/// NaN-safe at `N < 2` or degenerate variance: returns `(NaN, NaN, NaN)`;
/// the caller must guard the print path.
pub(crate) fn pentanomial_ci(state: &SprtState) -> (f64, f64, f64) {
    let n: u32 = state.pair_counts.iter().sum();
    if n < 2 {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let n_f = f64::from(n);
    let pair_score = |i: usize| (i as f64) * 0.5;
    let mut sum_ns = 0.0;
    let mut sum_ns2 = 0.0;
    for (i, &count) in state.pair_counts.iter().enumerate() {
        let s = pair_score(i);
        sum_ns += f64::from(count) * s;
        sum_ns2 += f64::from(count) * s * s;
    }
    let mu = sum_ns / n_f;
    let var = sum_ns2 / n_f - mu * mu;
    // var ≤ 0 (zero variance from a degenerate single-bin distribution, or
    // negative from f64 round-off near zero) → CI is undefined.
    if var <= 0.0 || var.is_nan() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let se = (var / n_f).sqrt();
    let ci_lo = mu - 1.96 * se;
    let ci_hi = mu + 1.96 * se;
    let to_elo = |pair_mu: f64| -> f64 {
        let s_game = pair_mu / 2.0;
        // Saturate the inverse-logistic at the open-interval boundary.
        // 0 < s_game < 1 always holds for non-degenerate runs; only
        // pathological 0%/100%-score pair distributions hit the limit.
        if s_game <= 0.0 {
            return f64::NEG_INFINITY;
        }
        if s_game >= 1.0 {
            return f64::INFINITY;
        }
        400.0 * (s_game / (1.0 - s_game)).log10()
    };
    (to_elo(ci_lo), to_elo(mu), to_elo(ci_hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wald_bounds_at_alpha_beta_05() {
        let (b, a) = wald_bounds(0.05, 0.05);
        let expected_b = (0.05_f64 / 0.95).ln();
        let expected_a = (0.95_f64 / 0.05).ln();
        assert!(
            (b - expected_b).abs() < 1e-9,
            "B mismatch: {b} vs {expected_b}"
        );
        assert!(
            (a - expected_a).abs() < 1e-9,
            "A mismatch: {a} vs {expected_a}"
        );
        assert!((b - (-2.944438979_f64)).abs() < 1e-6);
        assert!((a - 2.944438979_f64).abs() < 1e-6);
    }

    #[test]
    fn wald_bounds_asymmetric_alpha_beta() {
        let (b, a) = wald_bounds(0.01, 0.05);
        let expected_b = (0.05_f64 / 0.99).ln();
        let expected_a = (0.95_f64 / 0.01).ln();
        assert!((b - expected_b).abs() < 1e-9);
        assert!((a - expected_a).abs() < 1e-9);
    }

    #[test]
    fn compute_llr_zero_at_indifference_midpoint() {
        // Empty state → degenerate guard returns 0.
        let cfg_zero = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let state = SprtState::default();
        assert_eq!(compute_llr(&state, &cfg_zero), 0.0);

        // Exact midpoint: with elo0=-10 and elo1=+10, s0_pair + s1_pair = 2.0 exactly
        // (logistic is symmetric around Elo=0). 50/50 mix of bin 1 (0.5) and bin 3 (1.5)
        // gives mu = 1.0 exactly, so (2*mu - s0_pair - s1_pair) = 0 → LLR = 0.
        let cfg = SprtConfig {
            elo0: -10.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        s.pair_counts[1] = 50;
        s.pair_counts[3] = 50;
        let llr = compute_llr(&s, &cfg);
        assert!(
            llr.abs() < 1e-9,
            "LLR at exact indifference midpoint must be ~0; got {llr}"
        );
    }

    #[test]
    fn compute_llr_positive_when_sample_favors_h1() {
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        s.pair_counts[3] = 50;
        s.pair_counts[4] = 50;
        let llr = compute_llr(&s, &cfg);
        assert!(
            llr > 0.0,
            "LLR must be positive when samples favor H1; got {llr}"
        );
    }

    #[test]
    fn compute_llr_negative_when_sample_favors_h0() {
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        s.pair_counts[0] = 50;
        s.pair_counts[1] = 50;
        let llr = compute_llr(&s, &cfg);
        assert!(
            llr < 0.0,
            "LLR must be negative when samples favor H0; got {llr}"
        );
    }

    #[test]
    fn compute_llr_zero_variance_returns_zero() {
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        s.pair_counts[2] = 100;
        assert_eq!(compute_llr(&s, &cfg), 0.0);
    }

    #[test]
    fn compute_llr_pinned_value() {
        // Pinned hand-computed value catches a factor-of-2 error in s_i_pair
        // that the midpoint test cannot catch (its LLR=0 property is
        // invariant under any consistent rescaling).
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let s = SprtState {
            pair_counts: [10, 10, 30, 25, 25],
            ..SprtState::default()
        };
        // mu = (0+5+30+37.5+50)/100 = 1.225
        // m2 = (0+2.5+30+56.25+100)/100 = 1.8875
        // sigma2 = 1.8875 - 1.225² = 0.386875
        // s0_pair = 1.0; s1_pair ≈ 1.028766
        // LLR ≈ 0.028766 * 0.421234 / (0.386875/100) / 2 ≈ 1.566
        let llr = compute_llr(&s, &cfg);
        assert!(
            (llr - 1.566).abs() < 0.01,
            "compute_llr_pinned_value: expected ≈ 1.566, got {llr}"
        );
    }

    #[test]
    fn classify_pair_score_truth_table() {
        // 9-row table of (gameA, gameB) ∈ {0.0, 0.5, 1.0}².
        assert_eq!(classify_pair_score(0.0, 0.0), 0);
        assert_eq!(classify_pair_score(0.0, 0.5), 1);
        assert_eq!(classify_pair_score(0.0, 1.0), 2);
        assert_eq!(classify_pair_score(0.5, 0.0), 1);
        assert_eq!(classify_pair_score(0.5, 0.5), 2);
        assert_eq!(classify_pair_score(0.5, 1.0), 3);
        assert_eq!(classify_pair_score(1.0, 0.0), 2);
        assert_eq!(classify_pair_score(1.0, 0.5), 3);
        assert_eq!(classify_pair_score(1.0, 1.0), 4);
    }

    #[test]
    fn pentanomial_ci_hand_computed_example() {
        // M4.D-shape input.
        let s = SprtState {
            pair_counts: [5, 40, 78, 56, 21],
            ..SprtState::default()
        };
        // mu = 224/200 = 1.12; m2 = 298/200 = 1.49; sigma2 = 0.2356.
        // SE = sqrt(0.2356/200) ≈ 0.03432.
        // CI_pair ≈ [1.0527, 1.1873].
        // Elo_est = 400·log10(0.56/0.44) ≈ +41.85.
        // Elo_lo  ≈ +18.33; Elo_hi ≈ +65.85.
        let (lo, est, hi) = pentanomial_ci(&s);
        assert!((est - 41.85).abs() < 0.5, "Elo_est ≈ +41.85; got {est}");
        assert!((lo - 18.33).abs() < 0.5, "Elo_lo ≈ +18.33; got {lo}");
        assert!((hi - 65.85).abs() < 0.5, "Elo_hi ≈ +65.85; got {hi}");
    }

    #[test]
    fn pentanomial_ci_minimum_sample_returns_nan() {
        let s = SprtState {
            pair_counts: [0, 0, 1, 0, 0],
            ..SprtState::default()
        };
        let (lo, est, hi) = pentanomial_ci(&s);
        assert!(
            lo.is_nan() && est.is_nan() && hi.is_nan(),
            "pentanomial_ci at N<2 must return NaN tuple; got ({lo}, {est}, {hi})"
        );
    }

    #[test]
    fn pentanomial_ci_zero_variance_returns_nan() {
        // 100 pairs but all in the middle bin → variance is exactly zero.
        let mut s = SprtState::default();
        s.pair_counts[2] = 100;
        let (lo, est, hi) = pentanomial_ci(&s);
        assert!(
            lo.is_nan() && est.is_nan() && hi.is_nan(),
            "pentanomial_ci at zero variance must return NaN tuple"
        );
    }

    #[test]
    fn pentanomial_ci_two_pairs_non_zero_variance_returns_valid_ci() {
        // n=2 pairs: one LL (bin-0, score 0.0) and one WW (bin-4, score 2.0).
        // n=2 satisfies `n >= 2`, so the function must NOT return NaN.
        // Pins the `< → <=` mutation on `if n < 2`: the mutant would
        // early-return NaN for n=2 (since 2 <= 2 is true), but the correct
        // code proceeds past the guard and computes a valid CI.
        //
        // mu = (0.0 + 2.0) / 2 = 1.0.
        // sum_ns2 = (0.0 + 4.0) / 2 = 2.0. var = 2.0 - 1.0 = 1.0.
        // SE = sqrt(1.0/2) ≈ 0.707. CI pair ≈ [1.0 - 1.96*0.707, 1.0 + 1.96*0.707].
        let mut s = SprtState::default();
        s.pair_counts[0] = 1; // LL: score 0.0
        s.pair_counts[4] = 1; // WW: score 2.0
        let (lo, est, hi) = pentanomial_ci(&s);
        assert!(
            !lo.is_nan() && !est.is_nan() && !hi.is_nan(),
            "pentanomial_ci at n=2 with non-zero variance must return a valid (non-NaN) CI; \
             got ({lo}, {est}, {hi})"
        );
        // est ≈ 0.0 (uniform draw rate at this exact mu=1.0 → Elo_est = 0 because
        // pair_score(mu=1.0) maps to p=0.5 → Elo=0).
        assert!(
            est.is_finite(),
            "Elo estimate must be finite for balanced LL+WW sample; got {est}"
        );
    }

    #[test]
    fn update_pair_continue_then_h1() {
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        // Stream: alternate bin-3 (1.5) and bin-4 (2.0) — strongly favors H1.
        let mut accepted = false;
        for i in 0..2000 {
            let score = if i % 2 == 0 { 1.5 } else { 2.0 };
            let v = update_pair(&mut s, &cfg, score);
            if v == SprtVerdict::AcceptH1 {
                accepted = true;
                break;
            }
            assert_ne!(v, SprtVerdict::AcceptH0);
        }
        assert!(accepted, "stream favoring H1 must accept H1 within cap");
    }

    #[test]
    fn update_pair_h0_path() {
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut s = SprtState::default();
        // Stream of 0.0 / 0.5 outcomes — favors H0.
        let mut rejected = false;
        for i in 0..2000 {
            let score = if i % 2 == 0 { 0.0 } else { 0.5 };
            let v = update_pair(&mut s, &cfg, score);
            if v == SprtVerdict::AcceptH0 {
                rejected = true;
                break;
            }
            assert_ne!(v, SprtVerdict::AcceptH1);
        }
        assert!(rejected, "stream favoring H0 must accept H0 within cap");
    }

    #[test]
    fn discard_singleton_increments_only_audit_counter() {
        let mut s = SprtState::default();
        for _ in 0..5 {
            discard_singleton(&mut s);
        }
        assert_eq!(s.discarded_singletons, 5);
        assert_eq!(s.pair_counts, [0; 5]);
        assert_eq!(s.llr, 0.0);
    }

    // -----------------------------------------------------------------
    // §6.3 back-test gate — synthetic Bernoulli-pair stream
    // -----------------------------------------------------------------

    /// Convert a u64 to a uniform f64 in [0, 1).
    fn u64_to_f64(x: u64) -> f64 {
        (x >> 11) as f64 / ((1u64 << 53) as f64)
    }

    #[test]
    fn sprt_back_test_h1_accept_at_known_elo_gap() {
        // Synthetic no-draw stream: each game is W with p ≈ 0.572 (= +50 Elo gap).
        // The plan's parenthetical claim that no-draw streams converge faster is
        // wrong: no-draw pair-variance is ~2× draw-heavy variance (each game is
        // Bernoulli at variance p(1-p) — pure 0/1, no 0.5 outcome to dampen var).
        // At +20 Elo the asymptotic LLR/pair is ~+0.0012, requiring ~2400 pairs
        // to reach +2.944 — too tight for the 2000-pair cap. +50 Elo gives
        // ~+0.0038 LLR/pair → ~770 pairs asymptotic, well inside the cap.
        // (The draw-heavy variants below test the realistic-distribution path.)
        let mut prng = super::super::prng::Prng::new(0xC1AB_F15A_E10D_5757);
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut state = SprtState::default();
        let p = 1.0 / (1.0 + 10f64.powf(-50.0 / 400.0));
        for _ in 0..2000 {
            let g_a = if u64_to_f64(prng.next_u64()) < p {
                1.0
            } else {
                0.0
            };
            let g_b = if u64_to_f64(prng.next_u64()) < p {
                1.0
            } else {
                0.0
            };
            let pair_score = g_a + g_b;
            let verdict = update_pair(&mut state, &cfg, pair_score);
            if verdict == SprtVerdict::AcceptH1 {
                return;
            }
            assert_ne!(
                verdict,
                SprtVerdict::AcceptH0,
                "SPRT must not falsely reject at +50 Elo true gap"
            );
        }
        panic!("SPRT did not accept H1 within 2000 pairs at +50 Elo true gap");
    }

    #[test]
    fn sprt_back_test_h0_reject_at_zero_elo_gap() {
        // True -30 Elo gap (well inside H0) — see sibling H1 test for why
        // the plan's "true 0 Elo" recommendation cannot converge in the
        // no-draw stream within 2000 pairs (asymptotic LLR/pair at 0 Elo
        // is only ~-0.00041 → ~7100 pairs needed). At -30 Elo the
        // asymptotic LLR/pair is ~-0.0058 → ~510 pairs.
        let mut prng = super::super::prng::Prng::new(0xC1AB_F15A_E10D_5758);
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut state = SprtState::default();
        let p = 1.0 / (1.0 + 10f64.powf(30.0 / 400.0));
        for _ in 0..2000 {
            let g_a = if u64_to_f64(prng.next_u64()) < p {
                1.0
            } else {
                0.0
            };
            let g_b = if u64_to_f64(prng.next_u64()) < p {
                1.0
            } else {
                0.0
            };
            let pair_score = g_a + g_b;
            let verdict = update_pair(&mut state, &cfg, pair_score);
            if verdict == SprtVerdict::AcceptH0 {
                return;
            }
            assert_ne!(
                verdict,
                SprtVerdict::AcceptH1,
                "SPRT must not falsely accept H1 at -30 Elo true gap"
            );
        }
        panic!("SPRT did not accept H0 within 2000 pairs at -30 Elo true gap");
    }

    #[test]
    fn sprt_back_test_drawheavy_h1_accept_at_known_elo_gap() {
        // Three-way per-game outcome: W=0.30, D=0.50, L=0.20 → ≈ +35 Elo.
        // Realistic 50% draw rate keeps the middle pair-score bins populated,
        // so a buggy trinomial-over-games implementation would mis-estimate
        // variance. Pentanomial code must converge to AcceptH1.
        let mut prng = super::super::prng::Prng::new(0xC1AB_F15A_E10D_DA01);
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut state = SprtState::default();
        let draw_game = |rng: &mut super::super::prng::Prng| -> f64 {
            let r = u64_to_f64(rng.next_u64());
            if r < 0.30 {
                1.0
            } else if r < 0.80 {
                0.5
            } else {
                0.0
            }
        };
        for _ in 0..2000 {
            let g_a = draw_game(&mut prng);
            let g_b = draw_game(&mut prng);
            let pair_score = g_a + g_b;
            let verdict = update_pair(&mut state, &cfg, pair_score);
            if verdict == SprtVerdict::AcceptH1 {
                return;
            }
            assert_ne!(
                verdict,
                SprtVerdict::AcceptH0,
                "SPRT must not falsely reject at +35 Elo true gap"
            );
        }
        panic!("SPRT did not accept H1 within 2000 pairs at +35 Elo true gap (draw-heavy)");
    }

    #[test]
    fn sprt_back_test_drawheavy_h0_reject_at_zero_elo_gap() {
        // W=0.25, D=0.50, L=0.25 → 0 Elo gap, 50% draw rate.
        let mut prng = super::super::prng::Prng::new(0xC1AB_F15A_E10D_DA02);
        let cfg = SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let mut state = SprtState::default();
        let draw_game = |rng: &mut super::super::prng::Prng| -> f64 {
            let r = u64_to_f64(rng.next_u64());
            if r < 0.25 {
                1.0
            } else if r < 0.75 {
                0.5
            } else {
                0.0
            }
        };
        for _ in 0..2000 {
            let g_a = draw_game(&mut prng);
            let g_b = draw_game(&mut prng);
            let pair_score = g_a + g_b;
            let verdict = update_pair(&mut state, &cfg, pair_score);
            if verdict == SprtVerdict::AcceptH0 {
                return;
            }
            assert_ne!(
                verdict,
                SprtVerdict::AcceptH1,
                "SPRT must not falsely accept H1 at 0 Elo true gap (draw-heavy)"
            );
        }
        panic!("SPRT did not accept H0 within 2000 pairs at 0 Elo true gap (draw-heavy)");
    }
}
