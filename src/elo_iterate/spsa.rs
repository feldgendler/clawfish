//! Pure SPSA core: schedule, Rademacher Δ draw, gradient step, encoding.
//!
//! Engine-agnostic, no I/O, fully unit-testable.
//!
//! Θ is held in *encoded units* (centi-K for `Aspiration_K`, centipawns for
//! `Aspiration_Min`/`Aspiration_Max`). The uniform rounding formula is
//! `round(clip(θ))` in all cases, which sidesteps per-parameter unit mismatch.
//!
//! Schedule is the canonical Spall finite-sample SPSA:
//!   α = 0.602, γ = 0.101, A = 0.1·N
//!   c_i  = c_end_i · N^γ        (so c_k_i = c_i/(k+1)^γ → c_end_i at k=N−1)
//!   a    = R_end · c_ref² · (N+A)^α   (c_ref = first param's c_end for anchoring)
//!   a_k  = a / (k+1+A)^α
//!   c_k_i = c_i / (k+1)^γ
//!
//! **Canonical step equation (sign is load-bearing — see §2.3):**
//!   match  = (pair_sum − 1.0) · 2.0   ∈ [−2, +2], positive when θ⁺ wins
//!   θ_i   += a_k · match · Δ_i / (2 · c_k_i)
//!   θ_i    = clip(θ_i, lo_i, hi_i)
//!
//! This is gradient *ascent* on θ⁺'s advantage: θ moves toward θ⁺ when match > 0.

use super::prng::Prng;

/// How a continuous θ float maps to an integer UCI option value.
///
/// Both variants share the same rounding rule — `round(clip(θ))` — because θ is
/// already stored in the engine's native integer units (centi-K or centipawns).
/// The discriminant is informational (for display and `c_end` specification) and
/// does not change the arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Centipawn (cp): θ is an integer cp value, e.g. `Aspiration_Min = 25`.
    IntCp,
    /// Centi-K: θ is K × 100, e.g. `Aspiration_K = 200` means K = 2.00.
    CentiK,
}

/// A single tunable parameter, held as a continuous float in encoded units.
#[derive(Debug, Clone)]
pub(crate) struct SpsaParam {
    /// UCI option name, e.g. `"Aspiration_K"`.
    pub name: String,
    /// Continuous internal state, in encoded units.
    pub theta: f64,
    /// Lower bound (inclusive), in encoded units.
    pub lo: f64,
    /// Upper bound (inclusive), in encoded units.
    pub hi: f64,
    /// Perturbation scale at the last iteration (in encoded units).
    pub c_end: f64,
    /// How to interpret and display θ.
    #[allow(dead_code)]
    // informational discriminant; arithmetic is identical for both variants
    pub encoding: Encoding,
    /// Running accumulator for tail-averaging.
    pub(crate) tail_sum: f64,
    /// Number of samples in the tail accumulator.
    pub(crate) tail_count: u64,
}

impl SpsaParam {
    /// Construct a parameter without any accumulated tail state.
    pub(crate) fn new(
        name: String,
        theta: f64,
        lo: f64,
        hi: f64,
        c_end: f64,
        encoding: Encoding,
    ) -> Self {
        SpsaParam {
            name,
            theta,
            lo,
            hi,
            c_end,
            encoding,
            tail_sum: 0.0,
            tail_count: 0,
        }
    }

    /// Round and clip θ to produce the integer UCI option value.
    ///
    /// Both encodings share the same arithmetic: θ is already in integer units;
    /// clip to [lo, hi] first (prevents rounding past the boundary), then round.
    pub(crate) fn encode_theta(&self, theta: f64) -> i64 {
        let clipped = theta.clamp(self.lo, self.hi);
        clipped.round() as i64
    }

    /// Format the perturbed plus value as a UCI option string token.
    pub(crate) fn plus_value(&self, theta: f64, c_k: f64, delta: f64) -> i64 {
        self.encode_theta(theta + c_k * delta)
    }

    /// Format the perturbed minus value as a UCI option string token.
    pub(crate) fn minus_value(&self, theta: f64, c_k: f64, delta: f64) -> i64 {
        self.encode_theta(theta - c_k * delta)
    }
}

/// Schedule constants computed once from the run configuration.
///
/// Carries base constants `a` and per-param `c_i` (NOT a per-iteration `R_k`).
/// Each iteration computes `a_k = a / (k+1+A)^α` and `c_k_i = c_i / (k+1)^γ`
/// directly from the closed forms, avoiding the dropped-`(k+1)^{2γ}` trap.
#[derive(Debug, Clone)]
pub(crate) struct SpsaSchedule {
    /// Global apply base constant.
    pub a: f64,
    /// Per-param perturbation base constants (one per param).
    pub c: Vec<f64>,
    /// Spall stability constant A = 0.1·N (or override).
    pub big_a: f64,
    /// Exponent α = 0.602 (Spall finite-sample recommendation).
    pub alpha: f64,
    /// Exponent γ = 0.101 (Spall finite-sample recommendation).
    pub gamma: f64,
    /// Total planned iterations N.
    #[allow(dead_code)]
    // carried for diagnostics/display; schedule arithmetic uses big_a/alpha/gamma
    pub n: u64,
}

/// SPSA standard exponents (Spall 1998 finite-sample recommendations).
pub(crate) const ALPHA: f64 = 0.602;
pub(crate) const GAMMA: f64 = 0.101;

impl SpsaSchedule {
    /// Build the schedule from the operator's end-point specification.
    ///
    /// `r_end` is the desired relative apply factor `a_{N-1}/c_{N-1}²` at the
    /// final iteration. `c_ref_idx` is the index of the reference parameter
    /// whose `c_end` anchors the `a` back-out (typically 0 = first param).
    ///
    /// # Panics
    /// Panics if `params` is empty — a zero-param schedule is a logic error.
    pub(crate) fn new(params: &[SpsaParam], n: u64, r_end: f64, a_override: Option<f64>) -> Self {
        assert!(
            !params.is_empty(),
            "SPSA schedule requires at least one param"
        );
        let alpha = ALPHA;
        let gamma = GAMMA;
        let big_a = a_override.unwrap_or(0.1 * n as f64);

        // c_i = c_end_i · N^γ  so that  c_k_i = c_i/(k+1)^γ → c_end_i  at k=N−1.
        let c: Vec<f64> = params
            .iter()
            .map(|p| p.c_end * (n as f64).powf(gamma))
            .collect();

        // Anchor `a` on the first param's c_end: a = R_end · c_end_ref² · (N+A)^α.
        // (Using the first param is conventional; any param gives consistent R_end
        // at the final iteration for that param's c scale.)
        let c_end_ref = params[0].c_end;
        let a = r_end * c_end_ref * c_end_ref * (n as f64 + big_a).powf(alpha);

        SpsaSchedule {
            a,
            c,
            big_a,
            alpha,
            gamma,
            n,
        }
    }

    /// Gain a_k at iteration k (0-indexed).
    pub(crate) fn a_k(&self, k: u64) -> f64 {
        self.a / (k as f64 + 1.0 + self.big_a).powf(self.alpha)
    }

    /// Perturbation scale c_k_i for param `i` at iteration k.
    pub(crate) fn c_k(&self, param_idx: usize, k: u64) -> f64 {
        self.c[param_idx] / (k as f64 + 1.0).powf(self.gamma)
    }
}

/// Draw a Rademacher ±1 vector from the PRNG.
///
/// `delta[i] = +1.0` when bit 0 of the next u64 is 1, else `−1.0`.
/// The same Prng is consumed by the sequential driver to ensure the
/// Δ stream is `--seed`-deterministic regardless of worker concurrency.
pub(crate) fn draw_rademacher(rng: &mut Prng, n: usize) -> Vec<f64> {
    (0..n)
        .map(|_| if (rng.next_u64() & 1) == 1 { 1.0 } else { -1.0 })
        .collect()
}

/// Map the θ⁺-side pair score sum to the centered match scalar.
///
/// `pair_sum` ∈ {0.0, 0.5, 1.0, 1.5, 2.0} (two games; θ⁺ POV).
/// Returns `match` ∈ [−2, +2]: positive when θ⁺ wins.
pub(crate) fn pair_sum_to_match(pair_sum: f64) -> f64 {
    (pair_sum - 1.0) * 2.0
}

/// Apply one SPSA gradient-ascent step.
///
/// Updates `params[i].theta` in place for each param.
///
/// Equation (see §2.3 — sign is load-bearing):
///   `θ_i += a_k · match · Δ_i / (2 · c_k_i)`
///   then clip to `[lo_i, hi_i]`.
///
/// `match_val` is pre-computed from `pair_sum_to_match`.
/// `delta[i]` is the Rademacher ±1 component for param i.
/// `c_k[i]` is the per-param perturbation scale at this iteration.
pub(crate) fn spsa_step(
    params: &mut [SpsaParam],
    a_k: f64,
    c_k: &[f64],
    delta: &[f64],
    match_val: f64,
) {
    debug_assert_eq!(params.len(), c_k.len());
    debug_assert_eq!(params.len(), delta.len());
    for (i, param) in params.iter_mut().enumerate() {
        let gradient_est = match_val * delta[i] / (2.0 * c_k[i]);
        param.theta += a_k * gradient_est;
        param.theta = param.theta.clamp(param.lo, param.hi);
    }
}

/// Accumulate a θ snapshot into the tail-averaging buffer.
///
/// Call after each `spsa_step` for iterations in the tail window.
pub(crate) fn accumulate_tail(params: &mut [SpsaParam]) {
    for param in params.iter_mut() {
        param.tail_sum += param.theta;
        param.tail_count += 1;
    }
}

/// Compute the tail-averaged θ for each parameter.
///
/// Returns the rounded integer value that would be sent as a UCI option,
/// or the current `theta` rounded if no tail samples have been accumulated.
pub(crate) fn tail_averaged_values(params: &[SpsaParam]) -> Vec<i64> {
    params
        .iter()
        .map(|p| {
            if p.tail_count > 0 {
                let avg = p.tail_sum / p.tail_count as f64;
                p.encode_theta(avg)
            } else {
                p.encode_theta(p.theta)
            }
        })
        .collect()
}

/// Format the final output block: one `--engine-option NAME=VALUE` per param,
/// including `Aspiration_Adaptive=true` prepended.
pub(crate) fn format_final_option_block(params: &[SpsaParam], values: &[i64]) -> String {
    debug_assert_eq!(params.len(), values.len());
    let mut out = String::new();
    out.push_str("--engine-option Aspiration_Adaptive=true");
    for (param, val) in params.iter().zip(values.iter()) {
        out.push_str(&format!(" --engine-option {}={}", param.name, val));
    }
    out
}

/// Build the `setoption` lines to send to an engine for a given θ vector.
///
/// Always injects `setoption name Aspiration_Adaptive value true` first,
/// then one `setoption name NAME value VALUE` per param.
pub(crate) fn build_setoption_lines(params: &[SpsaParam], values: &[i64]) -> Vec<String> {
    debug_assert_eq!(params.len(), values.len());
    let mut lines = vec!["setoption name Aspiration_Adaptive value true".to_string()];
    for (param, val) in params.iter().zip(values.iter()) {
        lines.push(format!("setoption name {} value {}", param.name, val));
    }
    lines
}

/// Format one TSV trajectory row for `spsa-trajectory.tsv`.
///
/// Columns: `k  <theta_i...>  <plus_i...>  <minus_i...>  match  a_k  <c_k_i...>  pair_score`
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn format_trajectory_row(
    k: u64,
    thetas: &[f64],
    plus_vals: &[i64],
    minus_vals: &[i64],
    match_val: f64,
    a_k: f64,
    c_k: &[f64],
    pair_score: f64,
) -> String {
    let mut fields = vec![k.to_string()];
    for t in thetas {
        fields.push(format!("{t:.4}"));
    }
    for p in plus_vals {
        fields.push(p.to_string());
    }
    for m in minus_vals {
        fields.push(m.to_string());
    }
    fields.push(format!("{match_val:.4}"));
    fields.push(format!("{a_k:.6}"));
    for c in c_k {
        fields.push(format!("{c:.4}"));
    }
    fields.push(format!("{pair_score:.4}"));
    fields.join("\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn aspiration_params() -> Vec<SpsaParam> {
        vec![
            SpsaParam::new(
                "Aspiration_K".into(),
                200.0,
                0.0,
                1000.0,
                20.0,
                Encoding::CentiK,
            ),
            SpsaParam::new(
                "Aspiration_Min".into(),
                25.0,
                0.0,
                1000.0,
                4.0,
                Encoding::IntCp,
            ),
            SpsaParam::new(
                "Aspiration_Max".into(),
                250.0,
                0.0,
                2000.0,
                12.0,
                Encoding::IntCp,
            ),
        ]
    }

    // -----------------------------------------------------------------------
    // 2.6.a — Schedule math: pin a_k / c_k_i at k=0, N/2, N-1
    // -----------------------------------------------------------------------

    #[test]
    fn schedule_math_pins_at_three_k_points() {
        let n: u64 = 1000;
        let r_end = 0.002_f64;
        let params = aspiration_params();
        let sched = SpsaSchedule::new(&params, n, r_end, None);

        // Hand-computed values for N=1000, R_end=0.002, A=0.1*N=100,
        // α=0.602, γ=0.101:
        //   c_end_ref = 20.0 (Aspiration_K)
        //   a = R_end · c_end_ref² · (N+A)^α = 0.002 · 400 · 1100^0.602

        let big_a = 0.1 * n as f64; // 100.0
        assert!((sched.big_a - big_a).abs() < 1e-9, "A = 0.1·N");

        // c_i for each param
        let n_pow_gamma = (n as f64).powf(GAMMA);
        let c0 = 20.0 * n_pow_gamma;
        let c1 = 4.0 * n_pow_gamma;
        let c2 = 12.0 * n_pow_gamma;
        assert!((sched.c[0] - c0).abs() < 1e-9, "c[0] mismatch");
        assert!((sched.c[1] - c1).abs() < 1e-9, "c[1] mismatch");
        assert!((sched.c[2] - c2).abs() < 1e-9, "c[2] mismatch");

        // a back-out
        let expected_a = r_end * 20.0_f64 * 20.0 * (n as f64 + big_a).powf(ALPHA);
        assert!((sched.a - expected_a).abs() < 1e-9, "a mismatch");

        // Check at three k-points.
        for k in [0u64, n / 2, n - 1] {
            let a_k = sched.a_k(k);
            let expected_a_k = sched.a / (k as f64 + 1.0 + big_a).powf(ALPHA);
            assert!(
                (a_k - expected_a_k).abs() < 1e-12,
                "a_k at k={k}: got {a_k}, expected {expected_a_k}"
            );

            for (i, param) in params.iter().enumerate() {
                let c_k_i = sched.c_k(i, k);
                let expected_c_k_i = sched.c[i] / (k as f64 + 1.0).powf(GAMMA);
                assert!(
                    (c_k_i - expected_c_k_i).abs() < 1e-12,
                    "c_k[{i}] at k={k}: got {c_k_i}, expected {expected_c_k_i}"
                );

                // c_k_i → c_end_i at k = N-1
                if k == n - 1 {
                    assert!(
                        (c_k_i - param.c_end).abs() < 1e-9,
                        "c_k[{i}] at k=N-1 must equal c_end={}, got {c_k_i}",
                        param.c_end
                    );
                }
            }
        }

        // a_{N-1} / c_{N-1}^2 ≈ R_end for the reference param (param 0).
        let a_last = sched.a_k(n - 1);
        let c_last_0 = sched.c_k(0, n - 1);
        let r_last = a_last / (c_last_0 * c_last_0);
        assert!(
            (r_last - r_end).abs() < 1e-9,
            "a_{{N-1}}/c_{{N-1}}^2 must ≈ R_end={r_end}, got {r_last}"
        );
    }

    /// Ensure c_k_i ≥ 1 for the K param throughout the run for the recommended
    /// configuration (c_end=20, N=1000). If this fails, θ⁺ and θ⁻ round to the
    /// same integer and the K-gradient is always zero.
    #[test]
    fn schedule_c_k_never_below_one_for_k_param() {
        let n: u64 = 1000;
        let params = aspiration_params();
        let sched = SpsaSchedule::new(&params, n, 0.002, None);

        for k in 0..n {
            let c_k_0 = sched.c_k(0, k);
            assert!(
                c_k_0 >= 1.0,
                "c_k[K] must be ≥ 1 at k={k} (got {c_k_0}); \
                 c_end={} may be too small for this N",
                params[0].c_end
            );
        }
    }

    /// Full spsa_step arithmetic pinned at k=0, N/2, N-1 against hand-computed θ deltas.
    #[test]
    fn schedule_spsa_step_arithmetic_at_three_k_points() {
        let n: u64 = 100;
        let r_end = 0.002_f64;
        let mut params = aspiration_params();
        let sched = SpsaSchedule::new(&params, n, r_end, None);

        let delta = [1.0_f64, -1.0, 1.0]; // fixed Rademacher vector
        let match_val = 1.0_f64; // θ⁺ wins one, draws one

        for k in [0u64, n / 2, n - 1] {
            // Reset thetas
            params[0].theta = 200.0;
            params[1].theta = 25.0;
            params[2].theta = 250.0;
            let a_k = sched.a_k(k);
            let c_k: Vec<f64> = (0..params.len()).map(|i| sched.c_k(i, k)).collect();

            spsa_step(&mut params, a_k, &c_k, &delta, match_val);

            // Verify each param moved by a_k * match * Δ_i / (2 * c_k_i)
            let expected: Vec<f64> = (0..3)
                .map(|i| {
                    let orig = match i {
                        0 => 200.0,
                        1 => 25.0,
                        _ => 250.0,
                    };
                    let step = a_k * match_val * delta[i] / (2.0 * c_k[i]);
                    orig + step
                })
                .collect();

            for (i, (p, e)) in params.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (p.theta - e).abs() < 1e-12,
                    "theta[{i}] at k={k}: got {}, expected {e}",
                    p.theta
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // 2.6.b — spsa_step on a mocked quadratic objective (determinism gate)
    // -----------------------------------------------------------------------

    #[test]
    fn spsa_step_converges_toward_known_optimum_on_quadratic() {
        // Mocked 1-D quadratic: J(θ) = −(θ − opt)²
        // For θ⁺ = θ + c·Δ vs θ⁻ = θ − c·Δ, match > 0 iff θ⁺ is closer to opt.
        // With Δ=+1: match > 0 iff θ < opt; θ should increase toward opt.

        let opt = 300.0_f64;
        let n: u64 = 200;
        let mut params = vec![SpsaParam::new(
            "TestParam".into(),
            100.0,
            0.0,
            600.0,
            20.0,
            Encoding::IntCp,
        )];
        let sched = SpsaSchedule::new(&params, n, 0.002, None);
        let mut rng = Prng::new(42);

        let initial_theta = params[0].theta;
        for k in 0..n {
            let a_k = sched.a_k(k);
            let c_k_0 = sched.c_k(0, k);
            let c_k = vec![c_k_0];
            let delta = draw_rademacher(&mut rng, 1);

            // Mock objective: J(θ) = −(θ − opt)²; J(θ⁺) − J(θ⁻) computed analytically.
            let theta_plus = (params[0].theta + c_k_0 * delta[0]).clamp(0.0, 600.0);
            let theta_minus = (params[0].theta - c_k_0 * delta[0]).clamp(0.0, 600.0);
            let j_plus = -(theta_plus - opt).powi(2);
            let j_minus = -(theta_minus - opt).powi(2);
            // pair_sum in [0, 2] where 2 = θ⁺ wins both, 1 = split, 0 = θ⁺ loses both.
            // Model as: if j_plus > j_minus, pair_sum = 1.5 (net +0.5); else 0.5 (net -0.5).
            let pair_sum = if j_plus > j_minus { 1.5_f64 } else { 0.5 };
            let match_val = pair_sum_to_match(pair_sum);

            spsa_step(&mut params, a_k, &c_k, &delta, match_val);
        }

        // After 200 iterations, θ should have moved substantially toward opt.
        let moved_toward_opt = (params[0].theta - opt).abs() < (initial_theta - opt).abs();
        assert!(
            moved_toward_opt,
            "θ should move toward opt={opt}; started at {initial_theta}, ended at {}",
            params[0].theta
        );
    }

    /// Same seed → bit-reproducible θ trajectory.
    #[test]
    fn spsa_step_trajectory_is_bit_reproducible_with_same_seed() {
        let n: u64 = 50;
        let run = |seed: u64| {
            let mut params = vec![SpsaParam::new(
                "P".into(),
                100.0,
                0.0,
                600.0,
                10.0,
                Encoding::IntCp,
            )];
            let sched = SpsaSchedule::new(&params, n, 0.002, None);
            let mut rng = Prng::new(seed);
            let mut trajectory = Vec::with_capacity(n as usize);
            for k in 0..n {
                let a_k = sched.a_k(k);
                let c_k = vec![sched.c_k(0, k)];
                let delta = draw_rademacher(&mut rng, 1);
                let pair_sum = if rng.next_u64() & 1 == 0 {
                    1.5_f64
                } else {
                    0.5
                };
                let match_val = pair_sum_to_match(pair_sum);
                spsa_step(&mut params, a_k, &c_k, &delta, match_val);
                trajectory.push(params[0].theta.to_bits());
            }
            trajectory
        };

        let t1 = run(99);
        let t2 = run(99);
        assert_eq!(t1, t2, "same seed must produce bit-identical trajectories");

        let t3 = run(100);
        assert_ne!(t1, t3, "distinct seeds must produce distinct trajectories");
    }

    // -----------------------------------------------------------------------
    // 2.6.c — Sign convention
    // -----------------------------------------------------------------------

    /// When increasing θ strictly helps θ⁺ win (i.e. J is monotone increasing),
    /// θ should increase over many iterations. This guards against the flipped-sign
    /// divergence described in research §8.
    #[test]
    fn sign_convention_theta_increases_when_higher_value_always_wins() {
        // Mock: θ⁺ always beats θ⁻ regardless of Δ direction.
        // When Δ = +1: θ⁺ = θ + c, which is higher → θ⁺ wins → match > 0.
        //   gradient estimate = match · Δ / (2c) > 0 → θ increases (correct).
        // When Δ = −1: θ⁺ = θ − c, which is lower → θ⁺ loses → match < 0.
        //   gradient estimate = match · Δ / (2c) = (neg)·(neg)/(pos) > 0 → θ increases (correct).
        // So regardless of Δ, θ should drift upward.

        let n: u64 = 50;
        let mut params = vec![SpsaParam::new(
            "P".into(),
            200.0,
            0.0,
            1000.0,
            10.0,
            Encoding::IntCp,
        )];
        let sched = SpsaSchedule::new(&params, n, 0.002, None);
        let mut rng = Prng::new(7);

        for k in 0..n {
            let a_k = sched.a_k(k);
            let c_k = vec![sched.c_k(0, k)];
            let delta = draw_rademacher(&mut rng, 1);

            // θ⁺ > θ⁻ iff Δ > 0; higher value always wins.
            // So match = +1 when Δ=+1, match = -1 when Δ=-1.
            let match_val = if delta[0] > 0.0 { 1.0_f64 } else { -1.0 };

            spsa_step(&mut params, a_k, &c_k, &delta, match_val);
        }

        assert!(
            params[0].theta > 200.0,
            "θ must increase when higher values always win; got {} (started at 200)",
            params[0].theta
        );
    }

    // -----------------------------------------------------------------------
    // 2.6.d — Rademacher draw determinism
    // -----------------------------------------------------------------------

    #[test]
    fn rademacher_same_seed_identical_sequence() {
        let mut rng_a = Prng::new(12345);
        let mut rng_b = Prng::new(12345);
        let d_a = draw_rademacher(&mut rng_a, 10);
        let d_b = draw_rademacher(&mut rng_b, 10);
        assert_eq!(d_a, d_b, "same seed → identical Rademacher sequence");
    }

    #[test]
    fn rademacher_distinct_seeds_differ() {
        let mut rng_a = Prng::new(1);
        let mut rng_b = Prng::new(2);
        let d_a = draw_rademacher(&mut rng_a, 20);
        let d_b = draw_rademacher(&mut rng_b, 20);
        assert_ne!(d_a, d_b, "distinct seeds → distinct Rademacher sequences");
    }

    #[test]
    fn rademacher_values_are_plus_or_minus_one() {
        let mut rng = Prng::new(42);
        let d = draw_rademacher(&mut rng, 1000);
        for (i, &v) in d.iter().enumerate() {
            assert!(v == 1.0 || v == -1.0, "delta[{i}] = {v} is not ±1");
        }
    }

    // -----------------------------------------------------------------------
    // 2.6.e — Perturb/round/clip
    // -----------------------------------------------------------------------

    #[test]
    fn encode_theta_clips_before_rounding() {
        let p = SpsaParam::new("P".into(), 0.0, 10.0, 100.0, 5.0, Encoding::IntCp);
        // θ = 9.7 < lo=10; should clip to 10, round to 10.
        assert_eq!(p.encode_theta(9.7), 10);
        // θ = 100.4 > hi=100; should clip to 100, round to 100.
        assert_eq!(p.encode_theta(100.4), 100);
        // θ = 105.0 > hi=100; clips to 100.
        assert_eq!(p.encode_theta(105.0), 100);
    }

    #[test]
    fn centi_k_encoding_round_trips() {
        let p = SpsaParam::new(
            "Aspiration_K".into(),
            200.0,
            0.0,
            1000.0,
            20.0,
            Encoding::CentiK,
        );
        assert_eq!(p.encode_theta(200.0), 200, "θ=200 → 200");
        assert_eq!(p.encode_theta(215.0), 215, "θ=215 → 215");
        assert_eq!(p.encode_theta(199.5), 200, "θ=199.5 → 200 (round-half-up)");
        assert_eq!(p.encode_theta(200.4), 200, "θ=200.4 → 200");
    }

    #[test]
    fn cp_encoding_round_trips() {
        let p = SpsaParam::new(
            "Aspiration_Min".into(),
            25.0,
            0.0,
            1000.0,
            4.0,
            Encoding::IntCp,
        );
        assert_eq!(p.encode_theta(25.0), 25);
        assert_eq!(p.encode_theta(25.6), 26);
        assert_eq!(p.encode_theta(24.4), 24);
    }

    /// θ⁺ and θ⁻ must never be equal when c_k_i ≥ 1 (integer grid guard).
    #[test]
    fn perturb_plus_minus_differ_when_c_k_at_least_one() {
        let p = SpsaParam::new("P".into(), 100.0, 0.0, 1000.0, 10.0, Encoding::IntCp);
        // With c_k = 2.0 and Δ = +1, plus = round(102) = 102, minus = round(98) = 98.
        let plus = p.plus_value(100.0, 2.0, 1.0);
        let minus = p.minus_value(100.0, 2.0, 1.0);
        assert_ne!(plus, minus, "plus and minus must differ when c_k ≥ 1");
        assert!(plus > minus, "plus > minus when Δ=+1");
    }

    /// When θ is near the lower box boundary, the perturbed values clip correctly.
    #[test]
    fn perturb_clips_at_box_boundary() {
        let p = SpsaParam::new("P".into(), 2.0, 0.0, 1000.0, 10.0, Encoding::IntCp);
        // minus = round(clip(2.0 - 5.0)) = round(clip(-3.0)) = round(0.0) = 0.
        let minus = p.minus_value(2.0, 5.0, 1.0);
        assert_eq!(minus, 0, "minus must clip to 0 at the lower boundary");
        // plus = round(clip(2.0 + 5.0)) = round(7.0) = 7.
        let plus = p.plus_value(2.0, 5.0, 1.0);
        assert_eq!(plus, 7);
    }

    /// Pin minus_value's exact arithmetic (no clipping in range) — kills the
    /// →0, `-`→`/`, and `*`→`+` mutants, which otherwise produce 0, 50, 97.
    #[test]
    fn minus_value_exact_arithmetic_no_clip() {
        let p = SpsaParam::new("P".into(), 100.0, 0.0, 1000.0, 10.0, Encoding::IntCp);
        // correct: round(clip(100.0 - 2.0 * 1.0)) = 98
        assert_eq!(p.minus_value(100.0, 2.0, 1.0), 98);
        // Δ=-1 branch: round(clip(100.0 - 2.0 * -1.0)) = 102
        assert_eq!(p.minus_value(100.0, 2.0, -1.0), 102);
        // symmetry: plus_value pins the same arithmetic on the + side
        assert_eq!(p.plus_value(100.0, 2.0, 1.0), 102);
        assert_eq!(p.plus_value(100.0, 2.0, -1.0), 98);
    }

    // -----------------------------------------------------------------------
    // 2.6.f — Objective mapping
    // -----------------------------------------------------------------------

    #[test]
    fn pair_sum_maps_to_match_correctly() {
        // θ⁺ loses both games (score 0 each = pair_sum 0.0) → match = −2
        assert!((pair_sum_to_match(0.0) - (-2.0)).abs() < 1e-12, "0.0 → −2");
        // θ⁺ loses one, draws one (0 + 0.5 = 0.5) → match = −1
        assert!((pair_sum_to_match(0.5) - (-1.0)).abs() < 1e-12, "0.5 → −1");
        // θ⁺ draws both (0.5 + 0.5 = 1.0) → match = 0
        assert!((pair_sum_to_match(1.0) - 0.0).abs() < 1e-12, "1.0 → 0");
        // θ⁺ wins one, draws one (1.0 + 0.5 = 1.5) → match = +1
        assert!((pair_sum_to_match(1.5) - 1.0).abs() < 1e-12, "1.5 → +1");
        // θ⁺ wins both (1.0 + 1.0 = 2.0) → match = +2
        assert!((pair_sum_to_match(2.0) - 2.0).abs() < 1e-12, "2.0 → +2");
    }

    // -----------------------------------------------------------------------
    // 2.6.a addendum: full spsa_step arithmetic cross-check
    // -----------------------------------------------------------------------

    #[test]
    fn spsa_step_arithmetic_cross_check() {
        // With known a_k, c_k, Δ, match: verify exact θ update.
        let a_k = 0.1_f64;
        let c_k = vec![5.0_f64, 3.0];
        let delta = vec![1.0_f64, -1.0];
        let match_val = 2.0_f64; // θ⁺ won both

        let mut params = vec![
            SpsaParam::new("P0".into(), 100.0, 0.0, 1000.0, 5.0, Encoding::IntCp),
            SpsaParam::new("P1".into(), 50.0, 0.0, 1000.0, 3.0, Encoding::IntCp),
        ];

        spsa_step(&mut params, a_k, &c_k, &delta, match_val);

        // θ₀ += 0.1 * 2.0 * 1.0 / (2 * 5.0) = 0.1 * 2 / 10 = 0.02
        assert!(
            (params[0].theta - 100.02).abs() < 1e-12,
            "θ₀ update mismatch"
        );
        // θ₁ += 0.1 * 2.0 * (−1.0) / (2 * 3.0) = 0.1 * (−2) / 6 = −0.03333...
        let expected_1 = 50.0 - 1.0 / 30.0;
        assert!(
            (params[1].theta - expected_1).abs() < 1e-12,
            "θ₁ update mismatch"
        );
    }

    // -----------------------------------------------------------------------
    // Tail averaging
    // -----------------------------------------------------------------------

    #[test]
    fn tail_averaging_returns_mean_of_accumulated_thetas() {
        let mut params = vec![SpsaParam::new(
            "P".into(),
            100.0,
            0.0,
            500.0,
            10.0,
            Encoding::IntCp,
        )];

        // Simulate accumulating three different θ values.
        params[0].theta = 120.0;
        accumulate_tail(&mut params);
        params[0].theta = 130.0;
        accumulate_tail(&mut params);
        params[0].theta = 110.0;
        accumulate_tail(&mut params);

        let avg = tail_averaged_values(&params);
        // mean of {120, 130, 110} = 120.0 → rounds to 120.
        assert_eq!(avg[0], 120, "tail average must round mean to nearest int");
    }

    #[test]
    fn tail_averaged_values_with_no_accumulation_uses_current_theta() {
        let params = vec![SpsaParam::new(
            "P".into(),
            175.0,
            0.0,
            500.0,
            10.0,
            Encoding::IntCp,
        )];
        let avg = tail_averaged_values(&params);
        assert_eq!(avg[0], 175);
    }

    // -----------------------------------------------------------------------
    // setoption / option block formatting
    // -----------------------------------------------------------------------

    #[test]
    fn build_setoption_lines_injects_adaptive_first() {
        let params = vec![SpsaParam::new(
            "Aspiration_K".into(),
            200.0,
            0.0,
            1000.0,
            20.0,
            Encoding::CentiK,
        )];
        let lines = build_setoption_lines(&params, &[210]);
        assert_eq!(lines[0], "setoption name Aspiration_Adaptive value true");
        assert_eq!(lines[1], "setoption name Aspiration_K value 210");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn format_final_option_block_structure() {
        let params = vec![
            SpsaParam::new(
                "Aspiration_K".into(),
                200.0,
                0.0,
                1000.0,
                20.0,
                Encoding::CentiK,
            ),
            SpsaParam::new(
                "Aspiration_Min".into(),
                25.0,
                0.0,
                1000.0,
                4.0,
                Encoding::IntCp,
            ),
        ];
        let block = format_final_option_block(&params, &[205, 28]);
        assert!(
            block.contains("Aspiration_Adaptive=true"),
            "must inject Adaptive=true"
        );
        assert!(block.contains("Aspiration_K=205"));
        assert!(block.contains("Aspiration_Min=28"));
    }

    #[test]
    fn format_trajectory_row_tab_separated() {
        let row = format_trajectory_row(0, &[100.0], &[105], &[95], 1.0, 0.05, &[10.0], 1.5);
        let cols: Vec<&str> = row.split('\t').collect();
        // k, theta0, plus0, minus0, match, a_k, c_k0, pair_score = 8 columns
        assert_eq!(cols.len(), 8, "expected 8 TSV columns, got: {row:?}");
        assert_eq!(cols[0], "0", "first col is k");
    }
}
