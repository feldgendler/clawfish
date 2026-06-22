// #[allow(dead_code)] on each fn: wired by controller in slice E; until
// then, clippy's dead-code lint fires because nothing outside tests calls these.
pub(crate) fn compute_k(t: u32, k0: f64, tau: f64) -> f64 {
    if k0 == 0.0 {
        return 0.0;
    }
    k0 / (1.0 + (t as f64) / tau)
}

pub(crate) fn expected_score(my_elo: f64, opp_elo: f64) -> f64 {
    1.0 / (1.0 + 10_f64.powf((opp_elo - my_elo) / 400.0))
}

pub(crate) fn update_estimate(prior_elo: f64, opp_elo: f64, result: f64, k: f64) -> f64 {
    prior_elo + k * (result - expected_score(prior_elo, opp_elo))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compute_k_at_t_zero_returns_k0() {
        let k = compute_k(0, 40.0, 10.0);
        assert!((k - 40.0).abs() < 1e-3, "expected 40.0, got {k}");
    }
    #[test]
    fn compute_k_at_t_equals_tau_halves() {
        let k = compute_k(10, 40.0, 10.0);
        assert!((k - 20.0).abs() < 1e-3, "expected 20.0, got {k}");
    }
    #[test]
    fn compute_k_decay_at_ten_tau() {
        let k = compute_k(100, 40.0, 10.0);
        let expected = 40.0 / 11.0;
        assert!((k - expected).abs() < 1e-3, "expected {expected}, got {k}");
    }
    #[test]
    fn compute_k_monotone_non_increasing() {
        let ts = [0u32, 5, 10, 20, 50, 100];
        let ks: Vec<f64> = ts.iter().map(|&t| compute_k(t, 40.0, 10.0)).collect();
        for i in 1..ks.len() {
            assert!(
                ks[i] <= ks[i - 1] + 1e-12,
                "K not monotone at index {i}: k[{i}]={} > k[{}]={}",
                ks[i],
                i - 1,
                ks[i - 1]
            );
        }
    }
    #[test]
    fn compute_k_zero_k0_returns_zero() {
        for t in [0u32, 1, 10, 100, 1000] {
            let k = compute_k(t, 0.0, 10.0);
            assert!(k == 0.0, "expected 0.0 for k0=0.0 at t={t}, got {k}");
        }
    }
    #[test]
    fn expected_score_equal_elo_returns_half() {
        let e = expected_score(2000.0, 2000.0);
        assert!((e - 0.5).abs() < 1e-9, "expected 0.5, got {e}");
    }
    #[test]
    fn expected_score_400_above() {
        let e = expected_score(2400.0, 2000.0);
        assert!((e - 0.909).abs() < 1e-3, "expected ≈0.909, got {e}");
    }
    #[test]
    fn expected_score_400_below() {
        let e = expected_score(2000.0, 2400.0);
        assert!((e - 0.091).abs() < 1e-3, "expected ≈0.091, got {e}");
    }
    #[test]
    fn update_win_against_equal() {
        // S=1 vs equal: E=0.5, delta = k*(1-0.5) = k/2.
        let prior = 2000.0;
        let k = 32.0;
        let updated = update_estimate(prior, prior, 1.0, k);
        assert!(
            (updated - (prior + k / 2.0)).abs() < 1e-3,
            "expected {}, got {updated}",
            prior + k / 2.0
        );
    }
    #[test]
    fn update_loss_against_equal() {
        // S=0 vs equal: E=0.5, delta = k*(0-0.5) = -k/2.
        let prior = 2000.0;
        let k = 32.0;
        let updated = update_estimate(prior, prior, 0.0, k);
        assert!(
            (updated - (prior - k / 2.0)).abs() < 1e-3,
            "expected {}, got {updated}",
            prior - k / 2.0
        );
    }
    #[test]
    fn update_draw_against_equal_no_change() {
        // S=0.5 vs equal: E=0.5, delta = k*(0.5-0.5) = 0.
        let prior = 2000.0;
        let updated = update_estimate(prior, prior, 0.5, 32.0);
        assert!(
            (updated - prior).abs() < 1e-9,
            "draw against equal should not change estimate; got {updated}"
        );
    }
    #[test]
    fn update_with_zero_k_freezes_estimate() {
        for &(prior, opp, result) in &[
            (2000.0_f64, 1800.0_f64, 1.0_f64),
            (1500.0, 2100.0, 0.0),
            (2100.0, 2100.0, 0.5),
        ] {
            let updated = update_estimate(prior, opp, result, 0.0);
            assert!(
                (updated - prior).abs() < 1e-9,
                "k=0 should freeze estimate; prior={prior}, got {updated}"
            );
        }
    }
}
