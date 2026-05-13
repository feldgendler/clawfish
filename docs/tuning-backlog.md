# Tuning backlog

Parameter-tuning campaigns (SPRT or SPSA) deferred from a feature's initial landing. **Listed in suggested order** — pull from the top when the next tuning slot opens.

Each search-feature ADR also carries an "Open SPRT-tunable parameters" section that records the per-feature tunable space at landing time (ADR-0023 NMP, 0024 RFP, 0025 LMR, 0026 FFP, 0027 qsearch correctness). This file is the consolidated **active queue** — items that have been promoted to current tuning candidates because their parent feature shipped with a borderline SPRT signal, or because the per-feature tunable space crosses ADRs.

This file also tracks **deferred features awaiting a precondition** — items rejected at attempt time because the engine wasn't strong enough or deep enough, with a documented trigger condition for revisit.

---

### M5.I aspiration third tier — DEFERRED 2026-05-11 (Elo-neutral; no per-TC signal to anchor follow-up tuning)

**Status (2026-05-11).** M5.I v1 (`ASPIRATION_INTERMEDIATE_HALF_WIDTH = 150`, fires at all `depth >= ASPIRATION_MIN_DEPTH = 6`) ran mixed-TC SPRT vs `M5.H1` (seed `0xC1ABF15AE10DD011`).

**Final result at 400 games**:
- Verdict: `continue` (llr=−0.22; LLR between H0/H1 bounds).
- Δ Elo: **+1.74 [−22.00, +25.49]** (pentanomial 95% CI; statistically flat).
- Aggregate score: 50.25% (W=108, L=106, D=186).
- Per-TC (all near 50% within wide noise bands):
  - 10+0.1: 49.5% (Δ Elo ≈ −4) — 92 games
  - 20+0.2: 51.7% (Δ Elo ≈ +12) — 116 games
  - 40+0.4: 48.1% (Δ Elo ≈ −13) — 80 games
  - 60+0.6: 50.9% (Δ Elo ≈ +6) — 112 games

**Decision: revert per plan §11 outcome 2.** Production HEAD remains at `M5.H1`. The intermediate aspiration tier produces **no measurable strength change** at clawfish's current strength/TC profile.

**Important lesson from the SPRT run.** Mid-SPRT samples produced a dramatic-looking bimodal pattern at 168 games (10+0.1 +41 Elo, 20+0.2 −71 Elo). That signal was **sample-variance noise** — at full N=92-116 per TC bucket, all per-TC scores converged to ~50%. **Do not iterate on per-TC SPRT signals at <200 games.** Per-TC CIs are ±10-12% at N=80; the "TC-bimodal" failure pattern from M5.G v1 / M5.H2 needs the full mixed-TC run to confirm. (M5.G v1's actual bimodal pattern at 400 games was: 10+0.1 +66, 60+0.6 +5, 20+0.2 −60, 40+0.4 −44 — strong magnitudes that survived to convergence. M5.I had no such surviving signal.)

**Diagnostic suites (post-SPRT, head vs M5.H1 baseline)**:
- WAC: 274/300 vs M5.H1 baseline 267 (+7, within wallclock noise).
- STS: 8872/15000 vs M5.H1 baseline 8822 (+50, within ±68 noise per prior M5-phase deltas).
- Stockfish UCI_Elo=2641 at 10+0.1: Δ Elo **−107.54 [−156.89, −62.08]** (200 games). Note: TC-specific point estimate at fast TC; not directly comparable to M5.B/M5.C `UCI_LimitStrength` numbers.

The modest tactical lean (WAC +7, STS +50) is consistent with the SPRT's +1.74 Elo mean but cannot be distinguished from wallclock noise. No actionable signal.

**Revisit conditions** (analogous to M5.H2's deferral):
- Clawfish reaches **depth ≥ 14 at typical TCs** (currently ~depth 8-12 at mixed-TC). At deeper search, score-continuity improves and the intermediate-tier mechanism's literature payoff (Crafty / Meesha / RobboLito reports of +10-20 Elo) is more plausibly realizable.
- OR: the engine's ordering quality improves substantially (e.g., post-M9 NNUE) so that tier-1 failure rates drop and tier-2 firings become rarer-but-more-targeted.

**Tunables preserved for future revisit** (in case a future iteration reaches the revisit conditions):
- `ASPIRATION_INTERMEDIATE_HALF_WIDTH` (v1 was 150 — Crafty-proportional): try `100`, `200`, `250` if revisiting.
- **Depth-gated tier 2** (untested in v1; potential v2 if revisiting): `ASPIRATION_INTERMEDIATE_MAX_DEPTH` constant capping where tier 2 fires. Without per-TC signal to anchor the cap value, no specific candidate is motivated.
- **Asymmetric intermediate widths** (untested): decouple fail-high vs fail-low widths.
- **Score-volatility-adaptive width**: width as a function of `|score(d-1) − score(d-2)|` (the cheap delta-baseline from the ML-tuned-aspiration backlog item §"ML-tuned aspiration window sizing").

**Why v2/v3 were not attempted at landing time.** Plan §11 outcome 2 + user confirmation (2026-05-11): with v1 showing no per-TC signal, follow-up variants (v2 depth-gating, v3 width retune, v4 asymmetric) lack empirical anchoring. The M5.G v1/v2 retune precedent (v2 succeeded because v1 had a clear-but-bimodal signal at full N) does not apply here — v1's signal is Elo-zero at full N. Iterating against a null-signal baseline is essentially random search through the tunable space. Defer to a future M-stage where the engine's strength/depth profile provides a clearer signal substrate.

**Cross-references.** `docs/plans/m5.i.md`; `docs/research/m5-aspiration-third-tier.md`; `docs/research/m4-aspiration-windows.md` §13; `bench/sprt/2026-05-11-m5.i-v1-vs-m5h1-mixed-tc.md`; `docs/milestones/m5.i.md` (retrospective). The M5.H2 deferral structure (ADR-0030 §11) is the workflow analog: attempted, Elo-neutral / flat at current strength, deferred with revisit conditions documented.

---

### M5.H2 lazy quiet sort — REVISIT when typical-TC search reaches depth 14+

**Status.** Rejected 2026-05-11 across four implementation variants (v1-v4); see ADR-0030 §11 and `docs/milestones/m5.h2.md`. Production reverted to M5.H1 v2 thin-wrapper baseline. v4 working code preserved on branch `experiment/m5h2-v4` at origin (commit `2701d23`, branched from M5.H1 baseline `M5.H1`).

**Why deferred, not abandoned.** The literature signal (research §15.6: +5–15 Elo from post-captures-search history-score timing) DID manifest at 40+0.4 (+65 Elo decisive) in the M5.H2 v1/v2 SPRT. The failure mode was the per-TC bimodal pattern: at fast TCs where clawfish reaches only depth 8–12, the post-search history table is too sparse and noisy to outperform pre-search history (−95 Elo at 20+0.2). The literature signal is real but conditional on the engine reaching depths where history accumulates enough signal-to-noise.

**Trigger condition for revisit.** Three precondition checks should ALL pass before re-attempting M5.H2:

1. **Typical-TC depth reach ≥ 14.** Measure: median ID depth reached at 20+0.2 from a 20-position mixed corpus. Currently clawfish reaches depth 10–12; the M5.H2 v1/v2 SPRT bucket showed +65 Elo decisive at 40+0.4 where median depth is ~14. The break-even is roughly there.
2. **History table accumulation rate per game ≥ 4000 entries.** Measure: instrument the history table to count nonzero entries at end of game; sample 10 games at 20+0.2. M5.H2 v1/v2 failure was diagnosed (research §3.1) as "history too sparse" at fast TC; the bound here is project-empirical (Stockfish-derived literature suggests ~4000 as the saturation point).
3. **No higher-leverage feature is also pending.** If M6 eval improvements are in flight, the depth gain from those would shift the engine through threshold 1 above; revisit M5.H2 only after those have shipped and re-measured.

**Recommended approach at revisit.**

- **Start from the `experiment/m5h2-v4` branch** (`git fetch && git checkout experiment/m5h2-v4 -- src/search.rs tests/uci_integration.rs proptest-regressions/search.txt`, or just cherry-pick the v4 commit and resolve any drift), not from scratch. v4 has the depth-gated single-Vec in-place design with `LAZY_QUIET_SORT_MIN_DEPTH=6`. The eager-sort-at-shallow cost was the failure mode at clawfish's current depth profile; that cost shrinks proportionally to the fraction-of-shallow-nodes when depth reach increases. If depth-12+ is reached at 20+0.2, the shallow fraction drops and v4's tradeoff changes sign.
- **Re-tune the threshold via SPRT campaign.** v4 used threshold=6 (matching SE_MIN_DEPTH). At higher depth reach, 8 or 10 might dominate. Sweep [4, 6, 8, 10, 14] at 200 games each.
- **Alternative: selection sort on quiet sub-slice** (research §3.2). Reads fresh history at each yield, O(N²) per node. Not attempted in M5.H2 v1-v4 — orthogonal design point worth a quick prototype before committing to depth-gated v4.
- **Independent research note** at `docs/research/m5-staged-movegen-allocation.md` covers prior art on allocation patterns, selection-sort, depth-gating thresholds, and binned history sort (research §3.4).

**Expected impact at revisit.** If preconditions hold, recovery of the +65 Elo signal observed at 40+0.4 in M5.H2 v1/v2, applied across all TC buckets. Total: +30–65 Elo conditional on threshold tuning. Time budget at revisit: ~6 hours (cherry-pick v4 from `experiment/m5h2-v4`, re-build, re-test, SPRT sweep across thresholds, validate winner).

**Hard rejection criteria.** If the revisit shows the same bimodal pattern even at depth ≥ 14 reach, abandon M5.H2 permanently; the engine's history-score noise floor is structurally incompatible with lazy-after-search sort, and no further investment is warranted.

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

**Validation methodology.** Mixed-TC SPRT against `M5.F` per the M5 convention. Items 1–3 are independent enough to SPRT separately; items 4–6 may interact (test in combinations once 1–3 land). Items 7–8 are diagnostic and may inform but not gate.

**Cross-references.** ADR-0028 (the "Open SPRT-tunable parameters" section is empty — this file supersedes it for now); `bench/sprt/2026-05-09-m5.f-vs-m5e-mixed-tc.md`; `bench/sprt/2026-05-09-m5.f-probe-only-vs-probe-and-store.md`; `docs/milestones/m5.f.md`.

---

### M5.G singular-extensions — verification-window boundary mutants + tunable-space sweep

**Why active.** M5.G's `cargo mutants --in-diff` campaign caught 58/60 viable mutants (96.7%). The two missed mutants are **both at `src/search.rs:1770:24`** — the `s_beta - 1` expression in the verification call's alpha argument:
1. `replace - with +` → window becomes `(s_beta + 1, s_beta)` (alpha > beta, inverted/degenerate).
2. `replace - with /` → window becomes `(s_beta, s_beta)` (alpha == beta, zero-width).

Neither is observably distinct from the original `(s_beta - 1, s_beta)` null window on the M5.G integration-test fixtures: the test corpus drives `verif_score` either far below `s_beta` (fail-low — both windows agree, extension fires) or far above `s_beta` (fail-high — both windows agree, no extension). The boundary case `verif_score == s_beta` is the only place where the windows differentiate, and no fixture engineers an exact-equality boundary.

**Tunable space and follow-up tests.**

1. ~~**`negamax_se_extension_at_singular_beta_boundary` test** (boundary-engineering fixture).~~ **DONE 2026-05-14.** Landed at `src/search.rs::tests::negamax_se_extension_at_singular_beta_boundary`. Verified via `cargo mutants --file 'src/search.rs' --re 'search\.rs:(1759|1775)'` — all four mutants caught (the two `s_beta - 1` arithmetic mutants plus the two `verif_score < s_beta` comparison mutants `< → ==` and `< → >`). Fixture uses pre-stored Exact TT entries at every non-excluded child's zobrist to deterministically land `verif_score = s_beta` at the verification frame, exploiting the fail-soft cutoff's "alpha ≥ beta fires on first move regardless of score" behavior to distinguish production from the `+` / `/` mutants.
2. **`SE_MARGIN_PER_DEPTH = 2` retune** (Stockfish-historical default). Current value is 1 (Xiphos / Ethereal). A wider margin tightens the verification's eligibility (fewer extensions, less verification cost) at the price of missing some genuinely-singular nodes. SPRT should resolve. Cost: 0 LOC + mixed-TC SPRT.
3. **`SE_MIN_DEPTH = 6` retune** (Xiphos default). Current value is 8 (literature majority). Lowering enables SE on shallower nodes, increasing firing rate. Cost: 0 LOC + mixed-TC SPRT.
4. **Lower+Exact gate at clause 6.** Currently Lower-only. At non-PV nodes Exact entries are rare but extend SE's eligibility surface. Cost: ~5 LOC + SPRT.
5. **Multi-cut on verification fail-high.** When ≥ N moves beat `s_beta` at verification, return `s_beta` as a fail-high cutoff (turning the lost SE bet into a pruning win). Cost: ~30 LOC + SPRT.
6. **Double extensions** (extend by +2 when verification fails low strongly, e.g. `verif_score < s_beta - 50`). Cost: ~20 LOC + SPRT.
7. **PV-SE** (extend SE eligibility to PV nodes). Cost: ~5 LOC + SPRT.
8. **Propagated `singular_ext_active` flag** through `search_child` (Stockfish-style stack tracking) — only relevant if SPRT shows verification-subtree NPS regression at deep TC. Cost: ~80 LOC (parameter migration through `search_child`) + SPRT.

**Validation methodology.** Mixed-TC SPRT against `M5.G` per the M5 convention. Item 1 is a test-only addition (no Elo impact). Items 2–8 each independent; SPRT each separately.

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

## M5.H2 SEE-split capture stage (good vs bad captures)

**Pulled from M5.H1 plan §1 / ADR-0030 §8.** Split the captures stage into "good captures" (SEE ≥ 0) yielded immediately after TT and "bad captures" (SEE < 0) yielded last, after quiets. Per CPW Static Exchange Evaluation + Move Ordering pages.

**Prerequisites.** SEE infrastructure (no current ETA in roadmap; M9+ candidate alongside NNUE training). Without SEE, the MVV-LVA-only capture sort is the recommended starting point per research §6.2.

**Estimated Elo gain.** Hard to disentangle from SEE itself; captures-split is one of several SEE consumers. Tuning-backlog entry rather than roadmap-promised.

**Estimated size.** ~50–100 LOC inside `MoveStager` once SEE exists.

---

*Active queue ends here. New items append above this line.*
