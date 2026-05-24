# ADR-0037 — Texel tuning harness (M6.I): feature-extraction linear model, Adam, frozen-PST reference frame

**Status:** Accepted (M6.I lands the Texel-tuning *harness* — `src/texel/` + `src/bin/texel-tune.rs` — as a standalone consumer that does NOT touch `evaluate`; `evaluate` byte-identical to `M6.F`, bench `1213649` / d4 `90591`. The *tuned weights* ship as a separate diff gated by a mixed-TC SPRT vs `M6.F` per the §9 verdict ladder — the operator-run landing step, the M6.G corpus-materialization precedent.)

## Context

M6.A–F expanded the eval parameter surface and shipped the new terms **score-neutral** (weights zeroed / scale ≡ identity), deferring all calibration to one joint Texel pass — the "extend then tune" law (ADR-0031/0032/0033/0034). M6.G/H/H2 built the game-result-labeled corpus infra and materialized four frozen `lane.bin` lanes. M6.I is that joint pass.

The roadmap (§M6) and the tuning-backlog ("M6.I PST co-tuning") pin the scope precisely:

- M6.I = **Arm A**: tune the **~180 deferred-term weights** (M6.B ISO/DBL/BWD + CONN, M6.C passed-pawn, M6.D mobility, M6.E king-safety, M6.F outpost / rook-file / endgame-scale).
- **Material + the 12 PeSTO PSTs + bishop-pair + mop-up are FROZEN** as the reference frame. Co-tuning the PSTs (Arm B) is a separate, gated, later campaign — it forfeits the verbatim-PeSTO diff-oracle and needs ridge-toward-PeSTO regularization.
- Freezing the base **pins the gauge**: the centipawn scale is set by the vendored PeSTO magnitude, so the `K·score` scale-invariance the literature warns about does not bite (the frozen base dominates the score and does not rescale with the tuned weights).

The roadmap left these ADR questions open: loss function, optimizer, corpus consumption / mixture meta-optimization, tunable-vector serialization. The eval is **linear in the tuned weights** (each weight multiplies a count or indicator) except two constructs — the multiplicative endgame draw-scale and the king-safety attacker-count S-curve index. The whole M6.I optimizer design follows from exploiting that linearity.

## Decision

### 1. Standalone harness, zero `evaluate` touch

- `src/texel/` library + `src/bin/texel-tune.rs` CLI. The engine crate's `evaluate` / search / `eval::data` constants are **not modified** by the harness landing ⇒ `evaluate` byte-identical to `M6.F` ⇒ bench `1213649` / d4 `90591` byte-for-byte (the M6.G/H/H2 infra-landing precedent).
- The tuner consumes the four frozen `lane.bin` lanes via `corpus::store::scan_valid_blocks`, scores positions via the M6.G↔M6.I interface (`static_eval_white`, NOT qsearch), and reuses `corpus::objective::{fit_k, logistic_loss, stratified_objective}` through their `score: &dyn Fn(&CorpusRecord) -> i32` seam.

### 2. Two scorers: a parameterized reference oracle + a fast cached model

A single feature model validated only against the *shipped* (all-deferred-terms-zeroed) eval is a tautology — at shipped weights every deferred term contributes 0, so a feature-count bug is invisible. The harness therefore carries **two** white-POV scorers, validated against each other:

- **`reference_score_white(pos, &EvalParams) -> i32`** — a faithful re-implementation of `evaluate_core` that reads weights from a runtime `EvalParams` struct instead of the `eval::data` consts, reusing the engine's `pub(crate)` detection accessors. It is the correctness oracle: validated `== quiet::static_eval_white(pos)` exactly at shipped `EvalParams` (same detection + same formula + same weights). It handles the full eval including the multiplicative scale (and the mop-up-after-scale split) for the outer sweep.
- **`extract(pos) -> FeatureVec` + `model_score_white(fv, w) -> f64`** — the fast cached path, **linear in the tunable weights at identity scale**. `FeatureVec` carries `{ base_mg, base_eg, mop_up }` (frozen f64 constants), `phase: u8`, and sparse `coeffs: [(index, raw_count)]` where each index's MG-vs-EG parity is fixed by `layout`. The model **mirrors `evaluate_core`'s single blend division exactly** — it accumulates two f64 sums and divides by 24 once at the end, NOT per-term:
  `mg = base_mg + Σ_{k∈MG} wₖ·countₖ`; `eg = base_eg + Σ_{k∈EG} wₖ·countₖ`; `score = (mg·phase + eg·(24−phase))/24 + mop_up`.
  Per-term division (`count·phase/24` summed) would drift several cp across many terms at literature-magnitude weights — the wrong truncation order. Validated `≈ reference_score_white(pos, params_from(w, identity-scale))` over a battery at **arbitrary** `w` — this catches feature-count and folding bugs in the tuned regime without a rebuild.

- **Detection reuses the engine's helpers.** New **additive, read-only `pub(crate)` feature accessors** are added to `eval/mobility.rs`, `eval/king_safety.rs`, `eval/tier1.rs`, `eval/pawns.rs` (e.g. `mobility_features`, `shield_open_file_features`, `outpost_features`, `rook_file_features`, pawn iso/dbl/bwd/conn counts) that share detection code with the existing scoring functions. Each scoring function becomes (or is test-pinned equal to) `dot(features, weights)`, so feature counts are correct **by construction**. These accessors are not called by `evaluate` ⇒ `evaluate` byte-identical to `M6.F`, bench unchanged (the M6.G `quiescence_eval_white` read-only-seam precedent). This is a real edit to four eval modules (named here, listed in the plan), not comment-only — the engine-neutrality argument is "additive read-only seam, `evaluate` untouched," not "no eval file changed."
- **Why a model, not the live eval:** `evaluate` reads compile-time `const` weights — it cannot score candidate vectors without a rebuild. The cached model evaluates any `w` as a streamed pass of dot-products (seconds on 8M positions), not millions of full evals.

### 3. Tunable surface and parameterization

| Group | Tunable weights | Treatment |
|---|---|---|
| Pawn ISO/DBL/BWD (MG+EG) | 6 | linear |
| Pawn CONN (MG+EG, ranks 2–7) | 12 | linear, monotonic-smoothed |
| Passed rank / path / king-dist | ~19 | linear (king-tropism per-step coeff is linear given the frozen `PASSED_KDIST_CAP`) |
| Mobility N/B/R/Q (MG+EG) | 132 | linear, monotonic-smoothed per kind |
| King-safety shield + open-file (MG+EG) | 8 | linear (fire in quiet positions) |
| Outpost knight/bishop (MG+EG, ranks 3–5) | 12 | linear |
| Rook open/semi-open (MG+EG) | 4 | linear |
| **Linear-core total** | **~193** | full-batch Adam (matches the backlog "~180") |
| **Frozen reference frame** | material, 12 PSTs, bishop-pair, mop-up, `EG_SCALE_DEN`, `FIFTY_MOVE_TAPER_FROM`, `PASSED_KDIST_CAP` | not tuned (structural / gauge anchor) |
| **Outer-swept (non-linear)** | 3 endgame-scale numerators | coarse coordinate search via the reference scorer (§4) |
| **Deferred (excluded)** | king-safety S-curve table (100) + 4 attacker-multipliers | quiet corpus gives ~no king-attack signal (research §3); frozen at shipped (= zero, off); deferred to a future non-quiet-corpus campaign |

**King-safety scope.** The roadmap obligation to "re-derive the entire king-safety weight set" is met **partially**: the shield + open/semi-open-file terms (linear, present in quiet positions) **are tuned**; the **attacker-count S-curve table + the 4 per-kind attacker multipliers are excluded** and stay at their shipped (zero) values. The **primary** reason is structural, not corpus signal: the table is indexed by `units = Σ multiplier_kind · attackers`, and the multipliers ship at **zero** (M6.E inert) ⇒ `units ≡ 0` ⇒ only `KING_SAFETY_TABLE[0]` is ever reachable ⇒ the 99 other entries have an identically-zero feature coefficient (dead by construction, independent of the corpus). Making the index live requires non-zero multipliers, which either (a) are *tunable* ⇒ the index is a function of tunable params ⇒ non-linear ⇒ breaks the linear inner solve, or (b) are *frozen at literature* ⇒ `reference_score_white` at `shipped()` (zero multipliers) diverges from the literature-multiplier model ⇒ breaks the §7 faithfulness cross-check — the same non-linearity class as the multiplicative endgame-scale (§4), which is likewise kept out of the gradient core. The **secondary** reason is corpus signal: the quiet filter preferentially removes the sharp king-attack positions (a tactical attack has a large static−qsearch gap ⇒ fails `|static−qsearch|<30`), so the high-danger end of the S-curve is under-observed even where it does fire (research §3, the Blunder mobility-degeneracy lesson). A proper S-curve tune = a non-linear freeze-and-sweep over the multipliers (the standard king-safety approach) + accepting the under-observation + a baseline change to ship non-zero multipliers — deferred to a follow-up. Documented in the retrospective.

### 4. Non-linear terms frozen in the core, swept outside

With the king-safety S-curve excluded (§3), the **only** non-linearity left in M6.I is the endgame draw-scale:

- **Endgame draw-scale** (`blended · scale / EG_SCALE_DEN`) is bilinear in (scale-numerators, term-weights), and `mop_up` is added *after* the scale multiply (`result = blended·scale/DEN + mop_up`). Handled by confining all scale arithmetic to the **reference scorer**: the linear inner Adam solve runs at **scale ≡ identity** (so `base` is a single additive per-position constant and the model stays purely linear); the 3 scale numerators (`OCB_WITH_PAWNS_SCALE`, `PAWNLESS_DRAW_SCALE`, `FIFTY_MOVE_FLOOR`) are then tuned by a coarse outer coordinate sweep that calls `reference_score_white` (which implements the scale + the mop-up-after-scale split faithfully) on the held-out objective, with the additive weights frozen. This is the ADR-0034 §4/§8 resolution made concrete, and it keeps the cached fast-model strictly linear (no scale folding, no two-component base).

Consequence: for M6.I the **reference scorer is itself fully linear in the tunable weights at identity scale** (king-safety S-curve frozen off), so `model_score_white ≈ reference_score_white` is a clean linear-vs-linear agreement check (§7), and the scale is the lone position-dependent multiplier, applied only in the outer sweep.

**Two-stage approximation (stated honestly):** the inner Adam solve optimizes the linear weights at identity scale; the outer sweep then scales the *whole* blend (including the frozen PST base) by `scale/DEN`. The inner weights are **not** re-optimized under the swept scale — so they are not jointly optimal with the chosen scale. This is acceptable because the scale fires only on a small endgame-drawish fraction of the (quiet) corpus, and the scale is a correctness/draw-dampening knob rather than a fitting lever. The verdict-deciding SPRT validates the composed result.

### 5. Optimizer — AdaGrad/Adam on the closed-form gradient

- Loss = mean of `(label − σ(K·score_white(w)))²` (Texel original; `corpus::objective` already implements it). The gradient w.r.t. `wₖ` is closed-form because `score` is linear in `w`:
  `∂L/∂wₖ = mean[ 2(σ−label)·σ(1−σ)·K·featₖ ]`.
- **Primary optimizer: AdaGrad/Adam** on this gradient over cached sparse features. Fastest and most reliable for a few-hundred-param *linear* surface (the loss is smooth, non-convex only through the sigmoid).
- **Coordinate-descent (classic ±1 Texel) retained as a cross-check / fallback** on a small sub-vector — its agreement with the gradient solution is a correctness check on the gradient math.
- **Init = shipped weights** (`EvalParams::shipped()`, read from `eval::data`: CONN live, every other deferred term zero). The tuner cold-starts from the M6.F production eval.
- **Regularization** (prevents the "wrong-shape / degenerate table" failure the per-phase screens and the Blunder report warned about):
  - L2 ridge **toward init (= shipped)** — small λ (≈1e-4–1e-3), swept. Literature defaults are NOT used as init or as the ridge target (that was the unsafe option the M6.E ADR flagged); they remain only as documentation in `eval::data` comments. So a low-signal term cold-starts at zero and stays near zero unless the corpus moves it — it never ships an un-validated literature value by default.
  - Monotonicity smoothing on the structurally-monotonic tables (mobility-by-count, passed-by-rank, CONN-by-rank) — penalize non-monotone adjacent differences. (The safety S-curve is excluded from the tune, §3.)
  - Research note: chess4j found L2 gave no improvement over λ=0; validation early-stopping (§10) is the primary overfitting control, regularization is secondary.

### 6. K-fitting and the gauge

- K fit by golden-section (`objective::fit_k`) **per mixture candidate**, frozen during that candidate's inner weight solve (refit once after, optional). K is fit on integer-rounded scores (deployment-faithful).
- The frozen PeSTO base pins the gauge; no separate gauge constraint needed for Arm A.

### 7. Cross-check faithfulness (the load-bearing correctness gate)

A silent divergence between the tuner's model and `evaluate` would surface only as an SPRT failure, and the user does not read code (CLAUDE.md profile). The two-scorer design (§2) makes the guard three-tier — critically, it validates in the **tuned-weight regime**, not only at shipped weights where every deferred term is inert:

- **Tier 1 — reference == engine, at shipped weights:** over a large corpus-position battery (including hand-built EP / promotion / castling / scale-boundary fixtures), assert `reference_score_white(pos, shipped()) == quiet::static_eval_white(pos)` exactly (or ≤1cp). Pins the reference oracle to the real engine eval.
- **Tier 2 — fast model == reference, at arbitrary weights:** assert `model_score_white(extract(pos), w) ≈ reference_score_white(pos, params_from(w, identity-scale))` over the battery at **random and literature-magnitude** `w` (≤ a small float-vs-int tolerance). This is the regime the shipped output lives in; it catches feature-count and phase-folding bugs the shipped-weight check cannot.
- **Tier 3 — scale-boundary + post-apply golden:** assert `reference_score_white` reproduces `evaluate`'s scale arithmetic at hand-built OCB-with-pawns / pawnless / `hmc∈{80,81,100}` fixtures with non-identity scale numerators; and after `apply` writes tuned integer weights and the engine rebuilds, assert the rebuilt `evaluate` matches the tuned model on the battery (the operator-side final bracket).

### 8. Mixture meta-optimization (bi-level)

- **Inner:** Adam weight solve for a candidate lane-mix (per-source reweighting of the four lanes), K refit per candidate.
- **Outer:** coarse simplex (Nelder–Mead-style) over lane proportions, selected on the **stratified held-out objective** (`objective::stratified_objective` — aggregate loss + per-source + outpost/endgame strata), NOT SPRT. TC-mix is prior-pinned to the deployment profile.
- **SPRT only confirms the chosen winner** — never inside the search loop.

### 9. Output, write-back, verdict ladder

- Tuner writes `bench/m6-params.json` (the tuned vector + K + chosen mixture) + a per-parameter sensitivity table (loss derivative at the endpoint + ±10% perturbation) recorded **in all outcomes** for diagnosability + a re-run script.
- **Write-back via codegen:** `texel-tune apply --params bench/m6-params.json` rewrites marker-delimited (`// TEXEL-TUNABLE-BEGIN/END: <group>`) `const` regions in `src/eval/data.rs` (structural region replacement, not fragile regex), then rebuild + the §7 golden test. The tuned weights ship hardcoded like PeSTO (the project's "no feature flags / baselines from historical commits" ethos).
- **Verdict ladder** (mixed-TC SPRT vs `M6.F`, the operator-run gate):
  1. **H1-accept** (CI lower ≥ 5): ship tuned, tag `M6.I` at tuned build.
  2. **Small-but-not-regression** (mean ≥ 5 ∧ CI lower > −10): ship tuned, retrospective notes inconclusive SPRT (M5.F outcome-2 precedent).
  3. **Marginal-positive** (mean ≥ 0 ∧ CI lower > −10): ship tuned with retrospective caveat (M6.I-specific rung — joint retune can land small-positive even when each weight moved correctly).
  4. **Regression / wide-CI negative** (mean < 0 ∨ CI lower ≤ −10): **revert** — M6 ships at `M6.F`, `M6.I` tag placed at `M6.F`'s commit, retrospective documents the failed attempt + recommends an SPSA follow-up. ("Revert" = the M6.F shipped state, i.e. deferred terms stay zeroed / scale ≡ identity; there are no literature values in production to revert *to* — the roadmap's "literature defaults" phrasing predates the score-neutral-landing disposition.)

### 10. Operational robustness (R1–R7 + R-TC)

The tuner is a long laptop crunch job (suspend / renice / offline). The roadmap's hard requirements map to:

- **R1 crash-safe:** atomic checkpoint (write-temp → fsync → rename); max loss = one optimizer step.
- **R2 (corpus correctness):** N/A to the tuner — the corpus is already frozen and game-terminal-only (M6.G R2). The tuner only reads.
- **R3 resumable, bit-identical:** checkpoint {params, best-params, Adam moments, iter, RNG state, K, early-stop counters, val history} ⇒ resume bit-identical to uninterrupted modulo one lost step.
- **R6 bounded memory:** features cached to a disk file in pass 1; tuning streams the cache in bounded chunks per epoch ⇒ peak heap does NOT scale with corpus size. The feature cache is a deterministic, reproducible artifact.
- **R7 all-cores, renice-friendly:** parallel feature extraction + parallel gradient accumulation; graceful on SIGINT/SIGTERM (flush checkpoint, exit), safe on SIGKILL via R1.
- **R-TC / VirtualClock:** the SPRT step uses `--virtual-clock` (the M6.G mandate); the tuner itself is compute-only.
- **Stopping (roadmap optimizer-stopping sub-protocol):** stop on the **stratified held-out objective** (patience, restore-best), plus the **integer-centipawn quantization floor** (once integer-rounded weights stabilize, stop), plus a deterministic max-iter backstop. Never stop on training loss. All stopping constants recorded in `bench/m6-params.json`.

## Consequences

- **Positive:** the harness lands inert (byte-identical to `M6.F`, no SPRT for the landing); the full 8M-position tune runs in minutes (linear model + cached features); reproducible (seeds + feature cache + re-run script); the verdict ladder makes the ship/revert decision principled; Arm B (PST co-tune) is unblocked by reusing this harness.
- **Negative / accepted:**
  - The king-safety S-curve table + attacker multipliers are **not tuned** (excluded — quiet corpus gives no signal); only shield + open-file are. The S-curve re-derivation is deferred to a future non-quiet-corpus campaign. Documented, not papered.
  - The reference scorer + fast model duplicate the eval's *scoring* arithmetic (detection is shared via the new `pub(crate)` accessors); mitigated by the three-tier cross-check that validates in the tuned regime.
  - The endgame-scale (3 numerators) gets a coarse outer sweep via the reference scorer, not joint gradient — a deliberate simplification preserving inner-loop linearity.
  - **Mixture meta-optimization is a secondary, optional `texel-tune mixture` subcommand**, not the default `tune` path: the research notes rate the source mix second-order ("ingest/rebalance only if SPRT is unconvincing"). The default tune runs a fixed (uniform or `--mix`-specified) mix and reports the per-source held-out loss so the operator can escalate to the simplex only if the first SPRT is marginal.
- **The land decision is operator-gated** by the SPRT (cannot run in an interactive session — ~10h mixed-TC). The harness + tuned `bench/m6-params.json` + apply tool are delivered; the operator runs the SPRT and applies the verdict ladder.

## References

- Roadmap §M6 (M6.I scope detail, verdict ladder, operational robustness, mixture sub-protocol).
- Tuning-backlog "M6.I PST co-tuning" (Arm A / Arm B split, frozen-PST rationale, gauge pinning).
- `docs/research/m6.i-texel-corpus-depth.md`, `docs/research/texel-position-sampling.md`, `docs/research/m6-corpus-construction.md`, `docs/research/m6-texel-tuning.md` (optimizer/regularization mechanics).
- ADR-0031/0032/0033/0034 (the deferred-weight terms), ADR-0035/0036 (corpus infra).
- `src/corpus/objective.rs` (`fit_k`, `logistic_loss`, `stratified_objective`), `src/corpus/mod.rs` (interface constants, `CorpusRecord`).
