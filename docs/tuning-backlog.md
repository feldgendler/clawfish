# Tuning backlog

Parameter-tuning campaigns (SPRT or SPSA) deferred from a feature's initial landing. **Listed in suggested order** — pull from the top when the next tuning slot opens.

Each search-feature ADR also carries an "Open SPRT-tunable parameters" section that records the per-feature tunable space at landing time (ADR-0023 NMP, 0024 RFP, 0025 LMR, 0026 FFP, 0027 qsearch correctness). This file is the consolidated **active queue** — items that have been promoted to current tuning candidates because their parent feature shipped with a borderline SPRT signal, or because the per-feature tunable space crosses ADRs.

---

### M5.F qsearch-in-TT — bound semantics, PV suppression, path gating

**Why active.** M5.F vs M5.E mixed-TC SPRT was inconclusive: Δ Elo +13.03 [−10.92, +37.12], `verdict=continue`, with a bimodal per-TC pattern (10+0.1 strong-positive, 60+0.6 slight-negative). Landed as "small-but-not-regression" per plan §11. The post-landing probe-only experiment (`bench/sprt/2026-05-09-m5.f-probe-only-vs-probe-and-store.md`) ruled out per-probe overhead as the cause of the slow-TC slight-regression, leaving Upper-bound looseness and TT-pressure pollution as the leading hypotheses.

**Tunable space.** Ranked by expected leverage on the slow-TC slight-regression (fast-TC gains are already strong; the goal is to recover the slow-TC end without losing the fast-TC end).

**High leverage:**

1. **Allow Exact at non-terminal completed-loop paths (relax Stockfish 45e5e65 rule).** Currently `qsearch_tt_bound_for_completed_node(best, beta)` caps non-terminal completed-loop bounds at Lower (cutoff) or Upper (no cutoff). Loosening the rule for path F only (when `best > alpha_initial && best < beta`) tightens qsearch's TT contribution. Stockfish achieves the same effect via richer path-tagging. Risk: false-Exact propagation. Cost: ~20 LOC + targeted SPRT.
2. **PV-node store suppression.** Currently qsearch has no `is_pv` parameter; PV-adjacent positions get the same store churn as non-PV. Threading `is_pv` and skipping stores on PV nodes is the Stockfish convention. Reduces TT pressure exactly where bound looseness hurts most. Cost: ~50 LOC (signature change + call-site migration) + SPRT.
3. **Path-A (stand-pat cutoff) store gating.** Stand-pat cutoffs are by far the most frequent store path; they're stored as Lower with `best=stand_pat`. Suppressing path-A stores entirely would slash store count by a large fraction without losing much information (the position can be re-stand-patted cheaply on the next visit). Targets TT-pressure-driven slow-TC regression. Cost: ~5 LOC (one early return) + SPRT.

**Medium leverage:**

4. **Depth field convention (`-1` instead of `0`).** Qsearch entries currently store at `depth=0`, indistinguishable from negamax `depth=0` under depth-preferred replacement. Using `-1` (or a sentinel like `DEPTH_QS`) lets replacement disambiguate — e.g., never let a qsearch entry displace a negamax entry of any depth, while still allowing q→q displacement. Cost: ~30 LOC across `tt.rs` replacement logic + SPRT.
5. **Probe gating by qsearch ply.** Currently every qsearch frame probes. Skipping probes for the first N frames (where the TT data was just written by the negamax probe immediately above and a re-probe is redundant) or the last N frames (deep qsearch where TT data is stale) might avoid wasted work. Cost: ~5 LOC + sweep.
6. **TT-move ordering filter relaxation.** Currently filter-gated via `moves_vec` membership (Andrew Grant's long-chain protection). Relaxing to "try the TT move even when not in `moves_vec`, with a make/legality check" recovers ordering wins at the cost of an extra make/unmake on misses. Cost: ~15 LOC + SPRT.

**Low leverage (~1–2 Elo each):**

7. **Per-path enable/disable for paths B/C/D/E.** Corner-case stores that fire rarely. Disabling individually has small per-path effect; mostly a code-size argument once SPRT-confirmed inert.
8. **Hash-size interaction sweep.** Qsearch entries roughly double TT pressure. The default `Hash` may now be undersized; tuning the harness's `Hash` setting upward could shift the equilibrium. Cost: 0 LOC (UCI option already exists) + harness-config sweep.

**Validation methodology.** Mixed-TC SPRT against `baseline/m5f-qsearch-in-tt` per the M5 convention. Items 1–3 are independent enough to SPRT separately; items 4–6 may interact (test in combinations once 1–3 land). Items 7–8 are diagnostic and may inform but not gate.

**Cross-references.** ADR-0028 (the "Open SPRT-tunable parameters" section is empty — this file supersedes it for now); `bench/sprt/2026-05-09-m5.f-vs-m5e-mixed-tc.md`; `bench/sprt/2026-05-09-m5.f-probe-only-vs-probe-and-store.md`; `docs/milestones/m5.f.md`.

---

### M5.G singular-extensions — verification-window boundary mutants + tunable-space sweep

**Why active.** M5.G's `cargo mutants --in-diff` campaign caught 58/60 viable mutants (96.7%). The two missed mutants are **both at `src/search.rs:1770:24`** — the `s_beta - 1` expression in the verification call's alpha argument:
1. `replace - with +` → window becomes `(s_beta + 1, s_beta)` (alpha > beta, inverted/degenerate).
2. `replace - with /` → window becomes `(s_beta, s_beta)` (alpha == beta, zero-width).

Neither is observably distinct from the original `(s_beta - 1, s_beta)` null window on the M5.G integration-test fixtures: the test corpus drives `verif_score` either far below `s_beta` (fail-low — both windows agree, extension fires) or far above `s_beta` (fail-high — both windows agree, no extension). The boundary case `verif_score == s_beta` is the only place where the windows differentiate, and no fixture engineers an exact-equality boundary.

**Tunable space and follow-up tests.**

1. **`negamax_se_extension_at_singular_beta_boundary` test** (boundary-engineering fixture). Synthesize a TT entry whose `score` and a hand-tuned position make the depth-3 verification's max-child-score land exactly at `s_beta`. Assert `verif_score == s_beta` boundary case → `se_extensions == 0` (strict `<` rejects equality). This kills both missed mutants and the `<` → `<=` mutant from the test-suite review pass-2 should-fix. Cost: ~30 LOC + fixture engineering.
2. **`SE_MARGIN_PER_DEPTH = 2` retune** (Stockfish-historical default). Current value is 1 (Xiphos / Ethereal). A wider margin tightens the verification's eligibility (fewer extensions, less verification cost) at the price of missing some genuinely-singular nodes. SPRT should resolve. Cost: 0 LOC + mixed-TC SPRT.
3. **`SE_MIN_DEPTH = 6` retune** (Xiphos default). Current value is 8 (literature majority). Lowering enables SE on shallower nodes, increasing firing rate. Cost: 0 LOC + mixed-TC SPRT.
4. **Lower+Exact gate at clause 6.** Currently Lower-only. At non-PV nodes Exact entries are rare but extend SE's eligibility surface. Cost: ~5 LOC + SPRT.
5. **Multi-cut on verification fail-high.** When ≥ N moves beat `s_beta` at verification, return `s_beta` as a fail-high cutoff (turning the lost SE bet into a pruning win). Cost: ~30 LOC + SPRT.
6. **Double extensions** (extend by +2 when verification fails low strongly, e.g. `verif_score < s_beta - 50`). Cost: ~20 LOC + SPRT.
7. **PV-SE** (extend SE eligibility to PV nodes). Cost: ~5 LOC + SPRT.
8. **Propagated `singular_ext_active` flag** through `search_child` (Stockfish-style stack tracking) — only relevant if SPRT shows verification-subtree NPS regression at deep TC. Cost: ~80 LOC (parameter migration through `search_child`) + SPRT.

**Validation methodology.** Mixed-TC SPRT against `baseline/m5g-singular` per the M5 convention. Item 1 is a test-only addition (no Elo impact). Items 2–8 each independent; SPRT each separately.

**Cross-references.** ADR-0029 §11; `bench/sprt/2026-05-09-m5.g-vs-m5f-mixed-tc.md` (landing); `docs/milestones/m5.g.md`.

---

### ML-tuned aspiration window sizing

**Purpose.** Replace M4.D's fixed `±25 → ±100 → full` widening schedule with a learned policy that predicts the optimal first-try window from cheap features, maximizing expected node savings.

**Motivation.** Aspiration is a bet on score continuity across ID iterations: a successful narrow-window search saves nodes (up to ~2× under perfect ordering), a failed one costs a re-search. The optimal window width depends on position-specific volatility, which is observable from cheap features but not captured by a fixed schedule. Surfaced during the M4.D walkthrough (2026-04-30 chat) as a deferred follow-up if the fixed schedule under-performs or saturates after SPSA tuning.

**Approach.**

1. **Feature set** (all cheap, all available without extra search work):
   - Iteration depth and depth threshold delta.
   - Prior score and prior score delta (`score(d-1) − score(d-2)`); the delta is the single strongest feature.
   - EWMA of past `K` score deltas.
   - Game-tree ply number.
   - Total material on each side (proxy for game phase; once M6's tapered eval lands, use the proper phase tag instead).
   - Branching factor at the root (move-list size).
   - First-move-stability flag from the prior iteration.
   - TT hit rate at root or near-root.
2. **Architecture.** Single hidden layer MLP, ~16-32 units, ReLU. Total params low thousands. Output: continuous window width (regression) or softmax over a bucket grid (`{25, 50, 100, 200, full}`). Inference ~ns; called once per ID iteration.
3. **Training data.** Per-position label generated by running the search with a grid of candidate windows and taking the argmin node count. Corpus: ~100K positions sampled from CCRL games + tactics suites (the latter to cover failure modes that self-play under-represents). Cost estimate: ~50-200 CPU-hours at d≤12.

**Cheap baseline first.** Before training a model, validate that the dominant signal isn't already captured by a one-line heuristic:

```
window = clamp(k * |score(d-1) − score(d-2)|, MIN, MAX)
```

Three SPSA-tunable params. Empirically this baseline captures 60-80% of the ML model's gain in published work on similar problems. If the baseline + SPSA SPRTs neutral or marginal vs M4.D's fixed schedule, the ML approach is unlikely to clear `elo1=5`.

**TC-adaptive (or depth-adaptive) intermediate tier.** Between the cheap delta-baseline and a full MLP, a parameterized adaptive width that scales with search depth or estimated time-per-move is a natural stepping stone (surfaced during the M4.D walkthrough on 2026-04-30 alongside the mixed-TC SPRT discussion). Functional shape: `half_width(depth) = base · max(min_factor, 1 - α·(depth - threshold))` — two parameters (`base`, `α`) plus the existing `threshold`. Depth-adaptive is cleaner than time-adaptive (depth is a discrete observable in the ID outer loop; no coupling to `compute_caps` time-management state). Validation: mixed-TC SPRT against M4.D's fixed-width baseline (the same campaign methodology M4.D establishes), with adaptive variants entering the per-TC regression curve to surface where adaptation helps. Expected gain: small (+2-4 Elo over fixed-width) but cheap to implement (~30 LOC + parameter tune); validates whether the depth/TC interaction motif is real before investing in delta-EWMA or MLP variants.

**Why not now.**

- M4.D's gate is the fixed schedule passing mixed-TC SPRT; building ML training infra is a multi-week scope expansion to a phase that should land in days.
- Prerequisites missing: tapered eval for proper phase signal (M6), SPSA / CLOP harness for the cheap baseline, position corpus, offline label-generation pipeline.
- Elo headroom small at this layer: ~+3-5 Elo realistic ceiling for ML over hand-tuned, vs +50-80 Elo for NMP, +80-100 Elo for LMR, +400+ Elo for NNUE. Opportunity cost is poor.
- NNUE (M9) re-trains the eval from scratch; an ML aspiration model trained on PeSTO eval would need re-training post-NNUE.

**When to land.** Four-tier escalation, each tier proceeding only if the previous shows ≥ +5 Elo of remaining headroom worth chasing:

1. **Fixed schedule (M4.D)** — ±50 default, width-tuned via mixed-TC SPRT. Ships at M4.D close.
2. **TC- or depth-adaptive parametric (post-M5, pre-M9)** — `base · max(min_factor, 1 - α·(depth - threshold))`. SPSA-tuned. Cheapest validation of the depth/TC interaction motif.
3. **Cheap delta-baseline (post-M9 or earlier if (2) saturates)** — `window = clamp(k · |score(d-1) − score(d-2)|, MIN, MAX)`. Three SPSA-tunable params.
4. **MLP (post-M9, only if (2)+(3) saturate)** — full feature set + hidden layer.

**Estimated size.** Adaptive parametric: ~30-50 LOC + SPSA harness reuse. Cheap delta-baseline: ~50 LOC + SPSA harness reuse. ML model: ~500-800 LOC (feature extraction + inference + offline-training pipeline as a separate Python or Rust binary), plus the corpus + label-generation infrastructure (potentially shared with Texel tuning or NNUE data prep).

---

*Active queue ends here. New items append above this line.*
