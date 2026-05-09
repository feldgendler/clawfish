// #[allow(dead_code)] on each fn: wired by controller in slice E.
#[allow(dead_code)]
pub(crate) fn sample_stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let variance = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    variance.sqrt()
}

#[allow(dead_code)]
pub(crate) fn should_stop(
    estimates: &[f64],
    window: usize,
    target_sigma: f64,
    confirm: usize,
) -> bool {
    if target_sigma == 0.0 {
        return false;
    }
    let len = estimates.len();
    // Need at least `window + confirm - 1` entries so every confirm position
    // has a full window behind it.
    //
    // Index-arithmetic proof: the loop iterates `i ∈ [len-confirm, len-1]`.
    // For each `i`, the slice is `estimates[i+1-window .. i+1]`. The earliest
    // slice (at `i = len-confirm`) starts at `len - confirm + 1 - window`.
    // For this start index to be ≥ 0, we need `len + 1 ≥ window + confirm`,
    // i.e. `len ≥ window + confirm - 1`. The guard below pins exactly this
    // tight bound. With `window=30, confirm=5`, the threshold is 34 — pinned
    // by `should_stop_minimum_data_boundary` and `should_stop_short_estimates_returns_false`.
    if len < window + confirm - 1 {
        return false;
    }
    // Check the last `confirm` positions (indices len-confirm .. len-1 inclusive).
    // Position i uses the slice estimates[i+1-window .. i+1] (length = window).
    for i in (len - confirm)..len {
        let slice = &estimates[i + 1 - window..i + 1];
        if sample_stddev(slice) >= target_sigma {
            return false;
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::super::estimator;
    use super::*;

    struct Xorshift64 {
        state: u64,
    }
    impl Xorshift64 {
        fn new(seed: u64) -> Self {
            Self {
                state: seed.wrapping_mul(0x9E3779B97F4A7C15).max(1),
            }
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.state = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn next_f64(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn sample_stddev_constant_series_zero() {
        let sd = sample_stddev(&[5.0f64, 5.0, 5.0]);
        assert!(
            sd.abs() < 1e-9,
            "constant series should have stddev 0.0, got {sd}"
        );
    }
    #[test]
    fn sample_stddev_two_point_uses_bessel() {
        // [0.0, 2.0]: mean=1.0, sum-sq-dev=2.0, Bessel divisor n-1=1 → stddev=√2.
        let sd = sample_stddev(&[0.0f64, 2.0]);
        let expected = f64::sqrt(2.0);
        assert!(
            (sd - expected).abs() < 1e-9,
            "expected √2 ≈ {expected}, got {sd}"
        );
    }
    #[test]
    fn sample_stddev_short_returns_zero() {
        assert!(
            sample_stddev(&[]).abs() < 1e-9,
            "empty slice should return 0.0"
        );
        assert!(
            sample_stddev(&[42.0]).abs() < 1e-9,
            "single-element slice should return 0.0"
        );
    }
    #[test]
    fn should_stop_disabled_when_target_zero() {
        let estimates: Vec<f64> = vec![2100.0; 50];
        assert!(
            !should_stop(&estimates, 30, 0.0, 5),
            "target_sigma=0.0 must return false (disabled)"
        );
    }
    #[test]
    fn should_stop_fires_when_recent_window_below() {
        let estimates: Vec<f64> = vec![2100.0; 50];
        assert!(
            should_stop(&estimates, 30, 10.0, 5),
            "constant series with target=10 should stop"
        );
    }
    #[test]
    fn should_stop_does_not_fire_with_high_variance() {
        let estimates: Vec<f64> = (0..60)
            .map(|i| if i % 2 == 0 { 2200.0 } else { 2000.0 })
            .collect();
        assert!(
            !should_stop(&estimates, 30, 10.0, 5),
            "high-variance series should not stop"
        );
    }
    #[test]
    fn should_stop_anti_flap_concrete_fixture() {
        // Case 1: alternating throughout — every trailing 30-window has σ ~ 50;
        // should NOT stop with target=10.
        let estimates_alternating: Vec<f64> = (0..35)
            .map(|i| if i % 2 == 0 { 2050.0_f64 } else { 2150.0 })
            .collect();
        assert_eq!(estimates_alternating.len(), 35);
        assert!(
            !should_stop(&estimates_alternating, 30, 10.0, 5),
            "alternating series has high trailing-σ; should not stop"
        );
        // Case 2: flat throughout → σ=0 < target=10 for all 5 confirm positions → fires.
        let estimates_flat: Vec<f64> = vec![2100.0; 35];
        assert!(
            should_stop(&estimates_flat, 30, 10.0, 5),
            "flat series of 35 should stop"
        );
    }
    #[test]
    fn should_stop_short_estimates_returns_false() {
        // window=30, confirm=5 → need at least 34; test with 33.
        let estimates: Vec<f64> = vec![2100.0; 33];
        assert!(
            !should_stop(&estimates, 30, 10.0, 5),
            "too short to confirm; must return false"
        );
    }
    #[test]
    fn bernoulli_back_test_gate() {
        // Bernoulli stream: p=0.760, equilibrium at expected_score(2200, 2000) ≈ 0.760.
        // Initial estimate set 200 Elo above equilibrium so E[S−E] ≈ −0.149 at t=0;
        // the trail drifts DOWN toward 2200 before settling.
        //
        // σ-stopping must fire within [34, 400] games. Lower bound 34 is the
        // minimum-data threshold (window=30 + confirm=5 - 1); with K_0=40 and
        // p=0.760, per-step jitter K·√(p(1−p)) ≈ 17 < target_sigma=30, so the
        // trailing-σ over a 30-window stays below 30 throughout, and the
        // algorithm correctly fires at the minimum sample size. Upper bound
        // 400 is the never-fires safeguard. The test detects:
        //   - never-fires bug (panic on stop_at.expect).
        //   - too-late-fires bug (assertion fails at t > 400).
        //   - sign-flip on update_estimate (post-convergence value check below).
        //   - too-early-fires bug (short-input guard at the bottom).
        let p = 0.760_f64;
        let opp_elo = 2000.0_f64;
        let mut current_estimate = 2400.0_f64; // 200 above equilibrium → directional convergence
        let mut estimates: Vec<f64> = Vec::new();
        let mut rng = Xorshift64::new(0x00DE_DBEE_F123_4567);
        let mut stop_at: Option<usize> = None;
        for t in 0u32..1000 {
            let s = if rng.next_f64() < p { 1.0_f64 } else { 0.0 };
            let k = estimator::compute_k(t, 40.0, 10.0);
            current_estimate = estimator::update_estimate(current_estimate, opp_elo, s, k);
            estimates.push(current_estimate);
            if should_stop(&estimates, 30, 30.0, 5) {
                stop_at = Some(t as usize + 1);
                break;
            }
        }
        let t_stop = stop_at
            .expect("σ-stopping never fired within 1000 games; check estimator or sigma impl");
        assert!(
            (34..=400).contains(&t_stop),
            "σ-stopping fired at t={t_stop}; expected within [34, 400]"
        );

        // Directional-drift check: with σ-stopping firing at the minimum
        // sample size (t≈34), the estimate has drifted only partway toward
        // equilibrium 2200 from initial 2400. We don't require full
        // convergence here — the test's primary purpose is the σ-stopping
        // decision, not convergence depth. We DO require the estimate to
        // be moving DOWN (toward equilibrium) and within ±300 Elo of it,
        // which catches gross update_estimate bugs (e.g. sign flip would
        // push the estimate UP past 2400).
        assert!(
            current_estimate < 2400.0,
            "post-stop estimate {current_estimate:.1} did not drift below initial 2400 — likely sign flip in update_estimate"
        );
        assert!(
            (current_estimate - 2200.0).abs() < 300.0,
            "post-stop estimate {current_estimate:.1} >300 Elo from equilibrium 2200 — likely sign flip or wrong K direction"
        );

        // Short-input guard: should_stop must return false until the data
        // window even fills. window=30 + confirm=5 - 1 = 34. We re-run a
        // parallel iteration up to game 35 and verify should_stop is false
        // at each step ≤ 34. Catches a stub that returns true unconditionally.
        let mut early_estimates = Vec::new();
        let mut early_rng = Xorshift64::new(0x00DE_DBEE_F123_4567);
        let mut early_estimate = 2400.0;
        for tt in 0..35 {
            let s = if early_rng.next_f64() < 0.760 {
                1.0
            } else {
                0.0
            };
            let k = estimator::compute_k(tt as u32, 40.0, 10.0);
            early_estimate = estimator::update_estimate(early_estimate, 2000.0, s, k);
            early_estimates.push(early_estimate);
            // Trail length post-push = tt + 1. should_stop is eligible to
            // fire when len >= window + confirm - 1 = 34, i.e. when tt >= 33.
            // The guard runs only for tt ∈ [0, 32] (len ∈ [1, 33]).
            if tt < 33 {
                assert!(
                    !should_stop(&early_estimates, 30, 30.0, 5),
                    "should_stop must return false for fewer than window+confirm-1=34 entries; fired at tt={tt} (len={})",
                    early_estimates.len()
                );
            }
        }
    }

    // ---- ELOH.B Tier-C targeted tests --------------------------------

    #[test]
    fn sample_stddev_three_point_pins_bessel_divisor() {
        // [1.0, 2.0, 3.0]: mean=2.0, sum-sq-dev=(1+0+1)=2.0, n-1=2 → σ=√1=1.0.
        // Mutant `/` → `*` at the Bessel step:
        //   variance = 2.0 * (3.0 - 1.0) = 4.0, σ = 2.0  (not 1.0).
        // Also catches the `*` variant.
        let sd = sample_stddev(&[1.0_f64, 2.0, 3.0]);
        assert!(
            (sd - 1.0_f64).abs() < 1e-9,
            "expected σ=1.0 for [1,2,3], got {sd}"
        );
    }

    #[test]
    fn should_stop_minimum_data_boundary() {
        // window=3, confirm=2 → need at least 3+2-1=4 entries.
        // With exactly 4 constant entries, `<` guard (correct) does NOT fire
        // (4 < 4 is false), so the confirm loop runs and fires (σ=0 < target).
        // Mutant `<= 4` would fire the guard → return false prematurely.
        let estimates = vec![2100.0_f64; 4];
        assert!(
            should_stop(&estimates, 3, 10.0, 2),
            "exactly window+confirm-1=4 constant entries must fire should_stop"
        );
        // One entry short (3) must still return false.
        let too_short = vec![2100.0_f64; 3];
        assert!(
            !should_stop(&too_short, 3, 10.0, 2),
            "window+confirm-1-1=3 entries must not fire should_stop"
        );
    }

    #[test]
    fn should_stop_slice_window_uses_i_plus_one_minus_window() {
        // Pins that the window slice is `estimates[i+1-window..i+1]` and NOT
        // `estimates[i-1-window..i+1]` (+ → - mutant) or `estimates[i-window..i+1]`
        // (* → identity mutant).
        //
        // Setup: window=2, confirm=1, target=10.
        //   need at least 2+1-1=2 entries.  Use 3 entries: [2100, 2100, 9999].
        //   i = len-1 = 2; correct slice = estimates[2+1-2..3] = estimates[1..3] = [2100,9999]
        //   σ([2100,9999]) >> 10 → NOT below target → should NOT stop.
        //
        //   With + → - mutant: slice = estimates[2-1-2..3]. 2-1-2 = -1 (underflow)
        //   → panic or wrong result. With * mutant (i*1 - window):
        //   slice = estimates[2*1-2..3] = estimates[0..3] = all three → σ still large.
        //
        // Use a fixture that distinguishes the correct vs. wrong slice boundary:
        //   [9999, 2100, 2100]. i=2, window=2.
        //   Correct: estimates[1..3] = [2100, 2100] → σ=0 < 10 → STOPS.
        //   Wrong (if slice started one earlier): [9999, 2100, 2100] → σ large → NOT stop.
        let estimates = vec![9999.0_f64, 2100.0, 2100.0];
        assert!(
            should_stop(&estimates, 2, 10.0, 1),
            "window=2 confirm=1: last 2 entries are constant; should stop"
        );
        // Complementary: if the high-variance entry IS in the window, must not stop.
        let estimates2 = vec![2100.0_f64, 9999.0, 2100.0];
        assert!(
            !should_stop(&estimates2, 2, 10.0, 1),
            "window=2 confirm=1: middle entry 9999 is in the window; must not stop"
        );
    }
}
