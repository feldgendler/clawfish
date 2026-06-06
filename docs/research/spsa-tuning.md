# SPSA Tuning for Chess Engine Parameters

Research for the aspiration-window volatility model tuner (3 parameters: `K`, `MIN`, `MAX`).

**Sources**: Spall (1998) implementation overview; Chess Programming Wiki SPSA article; Kocsis, Szepesvári & Winands (2005/2006) RSPSA paper; Stockfish's tuning method (Kiiski 2011); Fishtest documentation; vdbergh spsa_simul README; TalkChess threads (t=40662, t=63632, t=78896); Bayesian stats paper (arXiv 2205.15602).

**Restriction honored**: no engine source repos browsed; all material from prose documentation.

---

## 1. The SPSA Algorithm

### Core recursion

At each iteration k (0-indexed), SPSA updates a p-dimensional parameter vector θ:

```
Δ_k   = random Rademacher vector (each component ±1 with p = 0.5)
θ⁺_k  = θ_k + c_k · Δ_k
θ⁻_k  = θ_k − c_k · Δ_k
ĝ_k   = [J(θ⁺_k) − J(θ⁻_k)] / (2 · c_k · Δ_k)   ← component-wise division
θ_{k+1} = θ_k − a_k · ĝ_k
```

- J(·) is the objective (minimize loss / maximize win rate).
- ĝ_k is the **simultaneous-perturbation gradient estimate**: one scalar difference divided component-wise by the p-component Δ_k vector.
- Total measurements: always **2 per iteration**, regardless of dimension p. This is the central efficiency advantage over finite-difference SA (which requires 2p measurements).

### Chess adaptation (Kiiski 2011)

The objective J is the result of a **2-game mini-match** between an engine configured with θ⁺ and one with θ⁻:

```
match(θ⁺, θ⁻) ∈ {−1, 0, +1}   (loss / draw / win for the θ⁺ side)
```

The Chessprogramming wiki documents a ±2 range variant where each game returns ±1 (not ±0.5 for a draw). Either convention works; the key is that it is a noisy estimate of the relative strength difference between θ⁺ and θ⁻.

The parameter update in chess SPSA notation uses the **relative apply factor** R_k = a_k / c_k²:

```
θ_{k+1} = θ_k + R_k · c_k · match(θ⁺_k, θ⁻_k) / Δ_k
```

This is algebraically equivalent to the standard form with a_k = R_k · c_k².

---

## 2. Hyperparameter Schedules

### Gain sequences

```
a_k = a / (k + 1 + A)^α
c_k = c / (k + 1)^γ
```

where k is 0-indexed iteration count and A, a, c, α, γ are scalar constants.

### Recommended values

| Parameter | Asymptotic optimal | Finite-sample preferred | Notes |
|---|---|---|---|
| α | 1 | 0.602 | Spall: lowest value guaranteeing convergence in finite samples |
| γ | 1/6 ≈ 0.167 | 0.101 | Same rationale |
| A | — | 0.1 × N_total | Stability constant; delays large early steps |

- The **asymptotic-optimal** pair (α=1, γ=1/6) maximizes convergence rate as k→∞.
- The **finite-sample** pair (α=0.602, γ=0.101) is Spall's practical recommendation when total iterations are bounded. The constraint α > γ must hold, otherwise magnitudes blow up.
- A = 10% of the planned total iterations is Spall's rule of thumb. It acts as an offset that prevents the gain from being excessively large in the first few iterations.

### Choosing a and c

| Parameter | Rule of thumb | Chess-specific guidance |
|---|---|---|
| c | ≈ standard deviation of measurement noise | For eval parameters: c_end ≈ 4 cp (centipawns); for search params, adjust to get a few-Elo gap between θ⁺ and θ⁻ |
| a | Calibrate so first-iteration step size matches the desired initial change in θ | Work backwards: choose desired Δθ₀, solve a from a/(1+A)^α ≈ Δθ₀/‖ĝ₀‖ |

The Fishtest/Stockfish convention specifies the **final** (not initial) values:
- R_end (= R at last iteration) ≈ 0.002 for eval-class parameters.
- c_end ≈ 4 cp (= 8 in Stockfish's internal half-pawn scale).

For search parameters (where the objective is noisier), c_end must be larger — tuning is empirical; use the spsa_simul tool to calibrate.

### Per-parameter scaling

When parameters have different units or sensitivities, give each its own c_i (but keep a single global R). This is equivalent to diagonal quasi-Newton preconditioning: each axis's c_i encodes the axis scale, so the global R becomes a scalar learning rate in a normalized space.

---

## 3. Objective-Function Design

### The mini-match

- Each iteration plays **2 games**: θ⁺ vs θ⁻ (one as white, one as black from the same opening position).
- The match result is a single number aggregated across both games: e.g. +2 (win+win), +1 (win+draw), 0 (split or double-draw), −1 (loss+draw), −2 (loss+loss).
- This is the only objective evaluation consumed per iteration.

### Signal-to-noise

Game results are extremely noisy. The draw rate for closely-matched engines is typically 30–60%; even a W/L is subject to opening-luck variance. This is why:
1. c must be large enough that θ⁺ and θ⁻ play measurably differently (a few Elo gap is the target).
2. Many thousands of iterations are needed; the algorithm is counting on the gradient signal averaging out over many iterations, not on any single measurement being reliable.

### Reporting

The literature does not mandate a "report the running average vs. the final point" convention for chess SPSA. In practice:
- Stockfish/Fishtest reports **final θ** after N iterations.
- For noisy runs, tail-averaging (average of the last T iterates) can reduce variance. Polyak-Ruppert averaging has optimal asymptotic rate but is rarely applied explicitly in chess practice.
- The Fishtest precision/confidence framework (vdbergh spsa_simul) plans the budget around ending within ε Elo of the optimum at confidence p%, then reads the final θ.

---

## 4. Variance Reduction

### Common random numbers (CRN)

Both the θ⁺ game and the θ⁻ game use the **same opening position** (drawn from the same book entry). Because position quality is a major source of game-outcome variance, using the same opening for both games cancels this noise source from the gradient estimate.

Standard practice: play 2 games per iteration from the same book position, with colors reversed (θ⁺ plays white in game 1, black in game 2; θ⁻ plays black in game 1, white in game 2). This is the **paired-opening / color-swap** pattern, which is a direct application of CRN to the chess gradient estimator.

### Antithetic variates

Introduced for chess SPSA by Kocsis, Szepesvári & Winands (2005 ACG conference) as part of RSPSA. The idea: for each Δ used in one iteration, the negated vector −Δ is used in a second iteration. Because E[ĝ(Δ)] and E[ĝ(−Δ)] are both unbiased gradient estimates but are negatively correlated in the variance component, combining them reduces gradient-estimate variance.

### Why this matters

Game results are Bernoulli-like (W/D/L) with a very high noise-to-signal ratio. Without variance reduction:
- A typical game result has ≈ 0.5 std-dev of noise.
- The Elo gap between two reasonably close parameter settings is ≈ 1–5 Elo.
- Raw signal-to-noise per game ≈ 0.1–0.5.

CRN (paired openings) is estimated to roughly halve the variance of the gradient estimate by removing positional noise. Antithetic variates further reduce variance on the Δ-perturbation axis.

Both techniques are orthogonal and can be combined.

---

## 5. Convergence and Stopping

### The fundamental chess-SPSA problem

SPSA's decreasing gain sequences are designed to achieve asymptotic convergence. In chess practice, this budget is never reached — 30,000–100,000 iterations are a practical ceiling, far below the number where formal convergence theory kicks in. The Stockfish team explicitly documented this: "The method doesn't converge and needs to be stopped at a 'suitable moment.'"

In practice:
- Parameters approach the optimum for the first phase of the run.
- Then the gradient signal becomes too small relative to remaining noise: "random walk" dominates.
- Strength starts degrading if the run continues too far.

### Practical stopping approaches

| Approach | Notes |
|---|---|
| Fixed iteration count | Most common. Choose N to land within target precision at target confidence (use spsa_simul to calibrate N from a guessed loss-function shape). |
| Visual trajectory monitoring | Plot each θ_i vs. iteration. Stable plateau = good. Wild oscillation late = random walk. Stop when plateau is clear. |
| Hold-out SPRT | After every B iterations, run a short SPRT between current θ and starting θ to check net improvement. Not standard but practical for small parameter sets. |
| Restart | If parameters diverge or exceed bounds, restart from the best-seen θ with halved a. |

### spsa_simul budget planning (vdbergh)

The simulator takes an estimated loss-function shape (Elo vs. parameter offset) and returns the number of games needed to achieve precision ε (Elo from optimum) with probability p (confidence). Default targets: ε = 0.5 Elo, p = 95%.

- c is kept **constant** (not decaying) in the vdbergh variant, a significant departure from the standard schedule. This is deliberate: it models the practical chess setting where convergence is never achieved and the target is a fixed precision bound.
- The parameters computed by the simulator are the constant c and constant R = a/c² that best achieve the precision target over the planned game budget.

---

## 6. SPSA vs. CLOP vs. Bayesian Optimization

| Criterion | SPSA | CLOP | Bayesian (e.g. Optuna/BoTorch) |
|---|---|---|---|
| Measurements per iteration | 2 (fixed, regardless of p) | O(1) but with internal model fitting | 1 per candidate, with surrogate model |
| Scalability in p | O(2) — excellent; 10–100+ params practical | Quadratic: model has 1 + p(p+3)/2 coefficients; practical limit ≈ 5–10 params | Depends on GP kernel; typically 5–20 params |
| Theoretical basis | Stochastic gradient descent with convergence guarantees | Local quadratic regression + confident discarding | Gaussian process upper-confidence bound |
| Convergence | Asymptotic; slow for noisy objectives | Can be slow to converge | Often finds good values faster with few params |
| Game budget | 30k–100k games typical for eval tune | Polynomial in params but often fewer games for few params | Paper reports good values at ~2250 games (45 iters × 50 games) for small p |
| Integer params | Continuous internal state + round for engine call | Native integer support in the CLOP tool | Depends on implementation |
| Distributed execution | Trivially parallelizable (each game pair is independent) | Less natural; model fitting is sequential | Batch BO supports parallel evaluation |
| Why Fishtest chose SPSA | Engineering fit with distributed system; CLOP was hard to integrate at scale | — | — |

### Recommendation by parameter count

- **p = 1–5**: All three methods work. CLOP or Bayesian BO often find the optimum in fewer total games due to more efficient local modeling. Use SPSA if you already have the infrastructure, or prefer the simplicity.
- **p = 5–20**: SPSA advantage grows. CLOP's quadratic model becomes expensive; Bayesian GP degrades.
- **p > 20**: SPSA is the dominant choice in the chess community.

For our 3-parameter aspiration tune, CLOP or Bayesian BO would be competitively efficient — but SPSA's integration with the existing `elo_iterate` harness (2-game match infrastructure, SPRT tooling) makes it the practical choice for clawfish.

---

## 7. Integer and Clamped Parameters

### The standard pattern

SPSA maintains θ as **continuous-valued floats internally**. For each engine call, θ⁺ and θ⁻ are **rounded to integers** before passing as UCI option values:

```
engine_value_plus  = round(θ⁺_k)
engine_value_minus = round(θ⁻_k)
```

The continuous θ is updated by the standard SPSA step, preserving fractional state across iterations. This is important: if θ were snapped to an integer after each update, the gradient signal from small per-step changes (O(R_k · c_k)) would be lost to quantization.

### Gotcha: c must exceed the integer grid

If c_k < 0.5, then θ⁺ and θ⁻ round to the same integer — both engines get identical parameters and the gradient estimate is zero. Rule: ensure c_k_initial ≥ 1 (at least one unit of the parameter's integer scale) throughout the run.

The Fishtest documentation explicitly warns: "the c value must be above 1" for small-valued integer parameters.

### Box constraints (min/max clipping)

Fishtest applies per-parameter box constraints:
- The θ float is clipped to [min, max] after each update.
- The perturbed values θ⁺ and θ⁻ are also clipped before being passed to the engine.
- Wide bounds are preferred; narrow bounds create boundary artifacts because SPSA's projection onto the constraint boundary interacts poorly with the gradient estimate (the projected gradient is biased near a binding constraint).

### Theoretical note on discrete SPSA

The theoretical literature (arXiv 1311.0042 and related) establishes that the round-then-evaluate / update-as-continuous pattern converges to the discrete optimum under mild conditions. Probabilistic projection (treating the continuous θ as a mixing parameter between ⌊θ⌋ and ⌈θ⌉) is theoretically cleaner but rarely used in practice because round() is simpler and convergence is sufficient.

---

## 8. Gotchas and Corner Cases

| Issue | Description | Mitigation |
|---|---|---|
| Wrong sign convention | If the match function sign is flipped (loss returns +1, win returns −1), θ diverges away from the optimum indefinitely. | Validate with a toy run: fix one parameter known to improve and verify θ moves toward it. |
| c too small | θ⁺ and θ⁻ round to the same integer; zero gradient signal; parameters barely move. | Ensure c_end ≥ 1 unit; increase if parameters seem stationary after many iterations. |
| c too large | θ⁺ and θ⁻ differ by more than the interesting parameter range; gradient estimates are noisy and biased. | Use spsa_simul to calibrate. c should produce a gap of ~1–5 Elo between θ⁺ and θ⁻. |
| A too small | First iterations have huge step sizes; parameters immediately overshoot. | A ≥ 0.05 × N total; 10% is standard. |
| a too large | Same as A too small; oscillation dominates. | Cross-validate: run spsa_simul with guessed loss-function shape. |
| Local optima | SPSA converges to local, not global, optima. The chess loss function is non-convex. | Try multiple restarts from different starting points. Or accept: for 3 params with bounded ranges, a grid-scan of a few SPRT probes can verify you're near the global optimum. |
| "Poisoned parameters" | Badly misconfigured learning rate corrupts parameters and they get worse during the run. | Monitor Elo via periodic short SPRTs vs the starting point. If Elo trends down, abort. |
| No stopping criterion | SPSA has no built-in convergence test. Random-walk phase wastes games and can worsen parameters. | Pre-plan N with spsa_simul. Monitor trajectory plots. Stop when stable or at planned N. |
| Interaction between params | Two independently SPRT-validated changes to the same subsystem may not compose. | After SPSA converges, confirm the final θ vs starting θ with a full SPRT, not just the tuning run. |
| Time-control dependence | Search-parameter optima can be TC-specific. | Run at the TC closest to your SPRT standard. For depth-amplifying effects, use the mixed-TC SPRT after tuning. |

---

## 9. Recommended Starting Configuration for the 3-Parameter Aspiration Tune

The aspiration volatility model: `half = clamp(K · |score(d−1) − score(d−2)|, MIN, MAX)`.

Parameters: K (real-valued multiplier, likely 0.1–3.0), MIN (integer centipawns, likely 10–50), MAX (integer centipawns, likely 50–300).

### Gain sequence

Use the standard Spall finite-sample schedule:

```
α = 0.602
γ = 0.101
A = 0.1 × N_total
```

For a first run of N = 20,000 iterations (= 40,000 games): A = 2,000.

### Per-parameter c_end and R_end

| Parameter | Suggested c_end | Reasoning |
|---|---|---|
| K | 0.1–0.3 | Fractional multiplier; a change of 0.2 in K should produce a few-Elo gap. Verify with a quick SPRT of K ± 0.2 before committing. |
| MIN | 2–5 cp | Integer; c_end must be ≥ 1; 2–5 cp should produce a measurable gap for a MIN in the 10–50 range. |
| MAX | 5–15 cp | Integer; MAX has wider range, so larger c needed. |
| R_end (global) | 0.002 | Standard for search parameters per Stockfish/Fishtest convention. |

### Calibrating c before launching

For each parameter, do a sanity-check SPRT: fix all but one parameter and run (val + c_end) vs (val − c_end) for ~200 games. Target: ~2–5 Elo gap (large enough to give gradient signal, small enough to not overshoot the basin).

### Game count (N)

With p = 3 parameters:
- Per iteration: 2 games.
- Suggested first run: 10,000–20,000 iterations = 20,000–40,000 games.
- The vdbergh spsa_simul can produce a calibrated N given an Elo-vs-parameter estimate. Input: estimated Elo loss at half-range from optimum (try 20 Elo as a starting guess for each param).

### Opening book

Use the same book positions already used by the `elo_iterate` SPRT harness. Each iteration: draw one book position, play both games from it (θ⁺ as white / θ⁻ as black, then θ⁻ as white / θ⁺ as black). This is the CRN variance-reduction pattern.

### Post-tuning validation

After SPSA terminates, round the final θ to integers (or nearest sensible values) and run a standard mixed-TC SPRT vs the pre-tuning baseline. The SPSA run itself does not constitute SPRT evidence.

### Open question

- The Fishtest convention specifies c_end and R_end for eval-class parameters (4 cp, 0.002). For **search** parameters — particularly K, which is a real-valued multiplier rather than a centipawn count — the appropriate c scale is not documented in prose sources. The calibration procedure (sanity-check SPRT of ±c_end before launch) is the only reliable resolution without reading engine source.

---

## Sources

- [SPSA — Chess Programming Wiki](https://www.chessprogramming.org/SPSA)
- [Stockfish's Tuning Method — Chess Programming Wiki](https://www.chessprogramming.org/Stockfish%27s_Tuning_Method)
- [CLOP — Chess Programming Wiki](https://www.chessprogramming.org/CLOP)
- [Automated Tuning — Chess Programming Wiki](https://www.chessprogramming.org/Automated_Tuning)
- [Aspiration Windows — Chess Programming Wiki](https://www.chessprogramming.org/Aspiration_Windows)
- [Simultaneous Perturbation Stochastic Approximation — Wikipedia](https://en.wikipedia.org/wiki/Simultaneous_perturbation_stochastic_approximation)
- [Spall, "Overview of the Simultaneous Perturbation Method," Johns Hopkins APL Technical Digest, 1998](https://www.jhuapl.edu/SPSA/PDF-SPSA/Spall_An_Overview.PDF)
- [Kocsis, Szepesvári, Winands, "Universal Parameter Optimisation in Games Based on SPSA," Machine Learning 2006](https://link.springer.com/article/10.1007/s10994-006-6888-8)
- [Kocsis, Szepesvári, Winands, "RSPSA: Enhanced Parameter Optimization in Games," LNCS 2006](https://link.springer.com/chapter/10.1007/11922155_4)
- [Fishtest Mathematics documentation](https://official-stockfish.github.io/docs/fishtest-wiki/Fishtest-Mathematics.html)
- [Fishtest — Creating my first test](https://official-stockfish.github.io/docs/fishtest-wiki/Creating-my-first-test.html)
- [zamar/spsa — SPSA Tuner for Stockfish](https://github.com/zamar/spsa)
- [vdbergh/spsa_simul — Multi-threaded SPSA simulator](https://github.com/vdbergh/spsa_simul)
- [TalkChess — Stockfish's tuning method (t=40662)](https://www.talkchess.com/forum3/viewtopic.php?t=40662)
- [TalkChess — SPSA problems (t=63632)](https://talkchess.com/forum3/viewtopic.php?t=63632)
- [TalkChess — Tuning search parameters (t=78896)](https://talkchess.com/viewtopic.php?t=78896)
- [Fishtest SPSA improvements issue #535](https://github.com/glinscott/fishtest/issues/535)
- [Bayesian statistics approach to chess engines optimization (arXiv 2205.15602)](https://arxiv.org/abs/2205.15602)
- [AlphaZero, RL, and SPSA — cosmo blog](https://cosmo.tardis.ac/files/2026-02-12-az-rl-and-spsa.html)
- [CLOP paper — Coulom 2011](https://www.remi-coulom.fr/CLOP/CLOP.pdf)
