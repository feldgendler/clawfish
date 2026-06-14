# Tuning backlog

Parameter-tuning campaigns (SPRT or SPSA) deferred from a feature's initial landing. **Listed in suggested order** — pull from the top when the next tuning slot opens.

Each search-feature ADR also carries an "Open SPRT-tunable parameters" section that records the per-feature tunable space at landing time (ADR-0023 NMP, 0024 RFP, 0025 LMR, 0026 FFP, 0027 qsearch correctness). This file is the consolidated **active queue** — items that have been promoted to current tuning candidates because their parent feature shipped with a borderline SPRT signal, or because the per-feature tunable space crosses ADRs.

This file also tracks **deferred features awaiting a precondition** — items rejected at attempt time because the engine wasn't strong enough or deep enough, with a documented trigger condition for revisit.

---

### 2026-06-03 overnight backlog sweep — 0 ships (full plan: [`docs/plans/tuning-overnight-2026-06-03.md`](plans/tuning-overnight-2026-06-03.md))

Walked the whole active queue in order. Dispositions:
- **M5.I / M5.H2 depth gate RE-MEASURED, still not met** — `scripts/depth-probe.sh` at literal 20+0.2 (fresh clock, the 8 midgame `src/bench.rs` FENs): depths `10 11 11 11 12 13 15 16`, **median ≈ 11.5 < 14**. Both stay deferred on fresh empirical basis (was the stale "~8-12" figure).
- **M5.F qsearch-TT last lever (qsearch-local-depth gate) — NO SHIP** (+11.30 [−13.86,+36.57]). Real per-TC lever: fixes 40+0.4 (the bucket M5.F.3 collapsed) but regresses extremes; net flat at `QS_PATHA_SUPPRESS_DEPTH=2`. A threshold sweep (1/3/4) is a low-prior micro-follow-up; patch `bench/sprt/patches/item3-*.patch`. **M5.F now fully explored — close.**
- **M5.G SE — confirmed closed** (no untried lever).
- **ML-aspiration tier 3 (delta-baseline) — NO SHIP but the night's best lead** → promoted to its own item below.
- **sign/monotonicity retune — numerics LANDED** (sign-projection + `--l2-lambda`/`--mono-lambda`/`--sign-project` CLI; tuner-only). Constrained retune was val-loss-neutral (+0.38%); the actual retune+SPRT deferred into the Arm-B/sign-mono campaign (which now has the projected-gradient infra it needed). Corrected-scope lesson: mobility/conn are **centered** tables (legitimately negative at low popcount) — do NOT sign-constrain them; only the scalar penalties/bonuses + the PASSED rank table.
- **Arm-B PST co-tune — sensitivity gate-checked → NO-GO / deprioritized.** `texel-tune sensitivity` on shipped params (16M-record cache) shows the deferred terms (mobility/passed/rook-file/shield) are the **most** sensitive, NOT pinned-near-zero; only sparse dead cells (data-coverage). Gate condition 2 unmet ⇒ frozen-PST double-count bias is small ⇒ Arm-B sinks down the queue (as the gate specifies). Evidence: `bench/tune/2026-06-03-sensitivity-shipped.json`.
- **King-safety S-curve — confirmed closed-negative** (M6.K).

**Net:** three consecutive 0-ship campaigns (06-01/02/03) ⇒ the search layer is at a local optimum at current strength/TC. Remaining headroom is in the eval surface (gated on Arm-B's now-deprioritized gate) and NNUE (M11).

---

### Delta-baseline aspiration, TC/depth-gated + SPSA — NEW (2026-06-03; the closest-to-ship lead)

**Status.** The 2026-06-03 ML-aspiration tier-3 delta-baseline (`half = clamp(K·|score(d-1)−score(d-2)|, MIN, MAX)`, hand-picked `K=2, MIN=25, MAX=250`) SPRT'd **combined +13.03 [−3.78, +29.91] over 800 games (2 seeds)** vs `M5.F.1` — no-ship by the strict CI-lower>0 rule, but **rung-2 "ship with note"** by ADR-0037 §9, and **robustly positive at 20+0.2 across both seeds** (+23 / +20) while flat-to-negative at the extremes (10+0.1 too shallow for a stable d-1/d-2 delta; 60+0.6 deep enough that fixed-50 already suffices). The mechanism is sound and the mid-TC win is real; the net is dragged to borderline by the extreme buckets.

**Update 2026-06-06 — SPSA harness build underway; delta-baseline mechanism landed as a default-OFF runtime feature (Unit 1).** Per the approved plan [`docs/plans/spsa-harness.md`](plans/spsa-harness.md), the delta-baseline mechanism is now an engine feature gated behind four new UCI options — `Aspiration_Adaptive` (check, **default false** → byte-identical fixed-±50, bench `112020`/`1354640` unchanged), `Aspiration_K` (centi-K spin, default 200 = K 2.00), `Aspiration_Min` (default 25), `Aspiration_Max` (default 250). **The ungated +13.03 candidate is now reproducible from the shipped binary** via `setoption name Aspiration_Adaptive value true` with the default K/Min/Max (they equal the hand-pick), so the `item5-*.patch` is functionally superseded — but **retained** pending the user's open rung-2 decision below (not deleted while that decision is live). Unit 2 (the SPSA loop on `elo-iterate`, to tune `K/MIN/MAX` ± a depth-gate band) is in progress; once it lands, the SPSA-tuned `Aspiration_*` values get a confirmatory mixed-TC SPRT vs `M5.F.1` per the validation methodology below. The two levers (depth-gate, SPSA-tune) are unchanged; the harness is their shared prerequisite.

**Update 2026-06-06 (later) — SPSA harness LANDED + validated; SPSA-tune lever found EMPIRICALLY LOW-SIGNAL.** Unit 2 shipped (`9d734d9`: `src/elo_iterate/spsa.rs` core + `--spsa*` CLI + `PlaySpsaPair` driver; 329 tests, fmt/clippy clean, mutants 73/80 caught — 7 survivors all equivalent-over-domain; bench `1354640` byte-identical). Two end-to-end validation runs on the live binaries (harness mechanics confirmed correct: exit 0, box projection holds, Spall `a_k`/`c_k` schedule cools as specified, `Aspiration_Adaptive=true` injected for both perturbed engines):
- **Shakedown** (30 iters, 2 games/iter, `c_end`=20/4/12, 2+0.02): `match` column ≈ **0.0 on nearly every iteration** (the θ± color-swap pair just splits 1-1) ⇒ ~zero gradient ⇒ θ moved <0.5/param over 30 iters.
- **Calibration** (15 iters, 6 games/iter, `c_end` ×3 = 60/12/36, 2+0.02): `match` becomes non-zero but **sign-unstable / noise-dominated** (−0.67,−1,−1,+1,0,−0.33,+0.33,0,−0.33,0,+1.67,−0.67,0,−0.33,0; mean ≈ −0.09). Net θ drift still tiny (K 200→200.17, Min 25→23, Max 250→249.6).

**Conclusion:** aspiration is a node-*efficiency* knob, not a move-*quality* knob, so per-iteration game outcomes carry almost no signal about K/MIN/MAX. Extracting the sub-Elo gradient would need the research-doc's ~20k-iter regime (×2 games × a representative TC like 20+0.2 ≈ **15–30h wall-clock**) — beyond a single overnight slot, for a candidate whose *total* ceiling is ~+13 Elo. **The harness is a sound, reusable deliverable** (it will pay off on higher-signal targets — eval-weight or time-management SPSA where per-game outcomes move more), **but SPSA-tuning *these three params* is not a good compute trade.** ⇒ The **depth-gate lever (lever 1) is the better remaining move** for this item, and the **rung-2 ship-with-note on the ungated candidate** (now reproducible from the shipped binary, no patch needed) is the cheap alternative. Surfaced to the user as an open decision (below). Validation-run data: `$TMPDIR/spsa-{shakedown,calib}` (transient; reproduce via the `elo-iterate --spsa` invocations recorded in `src/elo_iterate.rs::end_to_end_spsa_smoke_*`).

**Update 2026-06-06 (later still) — LEVER 1 (depth-gate) CLOSED NEGATIVE; BOTH LEVERS NOW EXHAUSTED.** Implemented the depth-gate as a tunable adaptive band `[adaptive_min_depth, adaptive_max_depth]` on `AspirationParams` (full workflow loop: plan-review → test-suite-review → final-review APPROVE; commit `89e4dad`, default-OFF runtime feature via `Aspiration_AdaptiveMinDepth`/`MaxDepth` UCI spins, bench `1354640` byte-identical, mutants 19/19 on the band gate). SPRT'd the `[8,12]`-banded candidate vs `M5.F.1` (same-binary self-SPRT, mixed-TC + virtual-clock, 2 seeds × 400 games): **Δ Elo ≈ −34.9 [both single-seed CIs fully negative: −35.74 [−60.57,−11.27] and −33.98 [−59.67,−8.66]]**. **The band hypothesis is refuted:** the regression concentrates in **20+0.2** (−34 / −16) — the exact bucket the band was meant to *protect* (it won +23/+20 ungated). Gating the adaptive width did not "keep the good part"; it broke the mechanism (likely a band-edge discontinuity at the mid-TC median depth ~11.5 ≈ band-max 12, flipping consecutive first-try windows between adaptive and fixed-50). **Closed — no band re-pick** (M5.I lesson). Data: `bench/sprt/2026-06-06-depth-gate-aspiration-vs-m5f1.md`; runner `scripts/sprt-depth-gate.sh`. ⇒ **The ungated +13.03 rung-2 candidate is the confirmed ceiling for this mechanism, and gating it backfires.** The *only* remaining positive path for the whole delta-baseline aspiration item is the **open user decision** (below): rung-2 ship-with-note the **ungated** candidate (`Aspiration_Adaptive=true` + default band [6,64], reproducible from `89e4dad`), or leave it default-OFF and move on (the search layer is now at a confirmed local optimum across 4 consecutive 0-ship campaigns + both aspiration levers).

**Why promoted (closest the search layer has come to a ship in three campaigns).** Two levers to capture the 20+0.2 win without the extreme-bucket drag:
1. **TC- or depth-gate the volatility width** — apply the delta-baseline only in the mid-depth band (e.g. `ASPIRATION_MIN_DEPTH ≤ depth ≤ ~12`), keep fixed-50 at shallow/deep. The 10+0.1 and 60+0.6 losses suggest the width should *not* deviate from 50 at the depth extremes.
2. **SPSA-tune `K / MIN / MAX`** (hand-picked, never optimized). A proper tune of the three params — ideally jointly with the depth-gate band — is the natural next step. Needs the SPSA harness (a small extension of `elo-iterate`; same dependency as the ML-aspiration items below).

**Validation.** Mixed-TC + virtual-clock SPRT vs `M5.F.1`. Decision: combined ≥2-seed confirm at CI-lower>0 (or rung-2 with an explicit ship-with-note). **Baseline = `M5.F.1`.** Patch: `bench/sprt/patches/item5-delta-baseline-aspiration.patch`; data: `bench/sprt/2026-06-03-item5-delta-baseline-aspiration-vs-m5f1.md`. Cross-ref the "ML-tuned aspiration window sizing" item below (this is its tier-3 made concrete + a mid-band gate).

**Open user decision (from the 2026-06-03 campaign):** ~~whether to take the rung-2 ship-with-note on the *ungated* +13.03 candidate now (patch preserved), vs waiting for the gated/SPSA-tuned version.~~ **RESOLVED 2026-06-06 — SHIPPED as `M5.K`** (`55c8fae`). After both improvement levers failed (SPSA low-signal; depth-gate refuted −34.9 Elo), the user chose the **rung-2 ship-with-note on the ungated candidate**: `Aspiration_Adaptive` flipped ON by default, bench d4 `112020` / d7 `1326598`, new production HEAD (search). The whole delta-baseline aspiration item is now **CLOSED — shipped at its confirmed ceiling.** See `docs/milestones/m5.k.md`. The `item5-*.patch` is fully superseded (the mechanism is the shipped default) and may be deleted.

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
- OR: the engine's ordering quality improves substantially (e.g., post-M11 NNUE) so that tier-1 failure rates drop and tier-2 firings become rarer-but-more-targeted.

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

**STATUS (2026-06-13 — item-3 threshold sweep vs `M5.K`, the last untried M5.F lever: CLOSED NO-SHIP, seed-split). M5.F is now fully exhausted.**
The 2026-06-03 item-3 qsearch-local-depth gate (suppress Path-A stand-pat `Lower` stores at `qs_depth >= QS_PATHA_SUPPRESS_DEPTH`) was net-flat vs `M5.F.1` at threshold 2 (+11.30 [−13.86, +36.57]); the deferred follow-up was a threshold sweep (1/3/4). Run 2026-06-13 **against current production `M5.K`** (not `M5.F.1` — M5.K = M5.F.1 + adaptive aspiration, so the qsearch-TT interaction differs), mixed-TC + virtual-clock, elo1=10, 400-game cap:
  - `=1` seed 1 (`…F1001`): **+41.89 [+16.20, +68.06]** — looked rung-1-ship-worthy.
  - `=3` seed 1 (`…F1003`): **+40.13 [+16.91, +63.72]** — same magnitude (threshold is second-order).
  - `=4` seed 1 (`…F1004`): **~52.3% / 348 games, flat** (run crashed `rc=1` EngineExit at game 348 — UCI-handshake desync, not build-stacking; partial data conclusive, not re-run).
  - **`=1` CONFIRMATION seed 2 (`…F2001`): −25.23 [−55.47, +4.63]** — *contradicts* seed 1 by ~67 Elo, opposite sign. **NO SHIP (seed-split).** Both seeds agree the lever **helps at 10+0.1 and regresses 60+0.6** (seed1 60+0.6 W21-L44; seed2 W6-L59) — it is **fast-TC-amplifying / slow-TC-regressing** (the M5.F.3 signature: item-3 is just a depth-conditional M5.F.3), with the net sign decided by the seed-dependent middle buckets. Layering a fast-TC-amplifying Path-A suppression onto `M5.K`'s depth-amplifying qsearch-`Exact` (M5.F.1) yields a seed-unstable wash; per ELOH.D a consistent 60+0.6 regression is the wrong direction regardless. Production stays `M5.K`/`M6.J`. Data: `bench/sprt/2026-06-13-item3-suppress{1,3,4}-vs-m5k.md`; patch unchanged (`bench/sprt/patches/item3-qsearch-local-depth-gate.patch`).
  - **Lesson: a single-seed mixed-TC `CI-lower > 0` is NOT sufficient to ship a search micro-lever.** Seed 1's +41.89 cleared the rung-1 bar and would have shipped on a one-seed rule; the 2-seed confirm caught the false positive. Extend the delta-baseline ≥2-seed-confirm requirement to *any* search micro-lever, even ones that look like a clean rung-1 on the first seed. **⇒ M5.F item 3 (and the whole M5.F qsearch-TT tunable space) is now CLOSED — every lever explored, nothing further ships at current strength.**

**STATUS (2026-05-31 overnight qsearch-TT tuning campaign — `docs/plans/tuning-m5f-m5g-overnight.md`):**
- **Item 1 (Exact at completed-loop, "M5.F.1") — SHIPPED.** Δ Elo **+37.49
  [+15.71, +59.58]** vs `M6.J`, mixed-TC + virtual-clock, 400 games. Three-way
  `qsearch_tt_bound_for_completed_node` (Exact when `alpha_entry < best < beta`);
  sound here because negamax delegates to qsearch at depth 0 before its TT
  cutoff, so a depth-0 qsearch-Exact is only ever consumed by qsearch's own
  (window-independent) re-probe. `bench/sprt/2026-05-30-c3-m5f1-*.md`.
- **Item 3 (Path-A stand-pat store gating, "M5.F.3") — VALIDATED but DEFERRED.**
  Δ Elo **+37.49 [+12.46, +62.92]** vs `M6.J` *standalone*, BUT it does **not
  compose with M5.F.1**: the combined C1+C3 went flat (+9.56 [−17.18, +36.41]),
  with a real, seed-confirmed destructive interaction at 40+0.4 (combined −58 Elo
  there across two seeds). Both are qsearch-TT changes that trade accuracy for
  speed; combined, the speed saturates while the inaccuracies compound (H1, the
  leading hypothesis). **Re-queued:** a future session could (a) ship M5.F.3
  *instead of* M5.F.1 (it was fast-TC-amplifying; M5.F.1 is depth-amplifying, so
  M5.F.1 was preferred per the ELOH.D mandate), or (b) bisect the interaction and
  re-tune so they compose. Patch: `bench/sprt/patches/c1-m5f3-path-a-suppress.patch`;
  data: `bench/sprt/2026-05-30-c1-m5f3-*.md` + `bench/sprt/2026-05-30-c1c3-combined-*.md`.
  - **Option (b) attempted 2026-06-02 (campaign item A(a)) — NO SHIP for the
    margin-gate lever.** Made the Path-A `Lower` store value-conditional (store
    only when *decisive*, `sp − beta ≥ 100`; suppress *marginal* fail-highs),
    hypothesizing the slow-TC collapse came from losing high-value re-probe
    entries. SPRT vs `M5.F.1` (mixed-TC + virtual-clock, 400 games, seed `…E0A0`):
    **Δ Elo −7.82 [−33.43, +17.71]**, CI-lower < 0 → no ship, and **40+0.4
    collapses again (39.9%)** — the same bucket as the unconditional combine. The
    margin gate does not fix the interaction ⇒ the **substitutes-not-complements**
    reading is reinforced. A qsearch-local-depth gate remains untried but is
    lower-prior now. `bench/sprt/2026-06-02-aa-margin-gate-vs-m5f1.md`;
    `bench/sprt/patches/aa-margin-gate.patch`. Option (a) (ship M5.F.3 instead)
    is unaffected but trades down per ELOH.D.
- **Items 2, 4–8 — ALL CLOSED, 0 ships (2026-06-01 campaign — `docs/plans/tuning-m5f-qsearch-items-2-4-8.md`).** Run against the `M5.F.1` tag (current production), mixed-TC + virtual-clock, 400-game cap, elo1=10:
  - **Item 2 (PV-node loose-store suppression) — REVERT.** Δ Elo **−31.35 [−55.91, −7.10]** (CI fully <0). Implemented the *refined* form (suppress Lower/Upper on PV nodes, KEEP `Exact` so M5.F.1's lever survives); review-approved + mutation-clean, but suppressing the loose PV-leaf stores removes net-useful TT entries. ADR-0028 §6 (no `is_pv` store-gating) stands. `bench/sprt/2026-06-01-item2-pv-store-suppress-vs-m5f1.md`.
  - **Item 4 (qsearch must not evict negamax cross-generation) — NO SHIP (flat).** Δ Elo **+6.08 [−16.44, +28.65]** (straddles 0). The backlog's "depth=−1 sentinel" premise was stale (`depth` is `u8`; negamax never stores depth 0), so the real lever was the cross-generation case. Sound + bench-inert + faintly-positive; **re-visit as a free-rider in a future TT-replacement change** (bucketed-TT / aging) rather than solo. `bench/sprt/2026-06-01-item4-ttrepl-crossgen-vs-m5f1.md`.
  - **Item 5 (probe gating by qsearch ply) — CLOSED on bench canary (no SPRT).** Gating the probe on deep frames *strictly increases* nodes (lost cutoffs): d4 +12% @threshold 4, +4.4% @8. A TT probe is one cache-read, its cutoff saves a subtree → no favorable mechanism; Elo ceiling ~0 (neutral as threshold→∞). Not worth an SPRT slot.
  - **Item 6 (TT-move ordering filter relaxation) — REVERT.** Δ Elo **−24.36 [−50.84, +1.84]** (CI-lower <0). Searching a legal quiet TT move inside qsearch dilutes its tactical focus (a non-forcing move yields a misleading horizon value). ADR-0028 §7 (filter-gated TT-move ordering, long-chain protection) stands. `bench/sprt/2026-05-31-item6-ttmove-filter-relax-vs-m5f1.md`.
  - **Item 7 (per-path enable/disable) — CLOSED analytically.** Paths B/C/D/E are correctness-mandated (stalemate/mate/single-reply/false-stalemate-guard) → non-removable regardless of frequency; A/F are load-bearing. No code-size lever exists.
  - **Item 8 (Hash-size interaction) — CLOSED analytically.** Not a candidate-vs-baseline differential (harness runs both engines at `Hash=64`); 64 MiB = 4M entries isn't the bottleneck. The only lever is the engine's *default* (16 MiB), which only applies without a GUI override and carries a **mobile-footprint tradeoff** — a separate config decision, not part of this campaign. Optional `Hash=128`-vs-`64` self-SPRT available on request.

  **Net lesson:** the M5.F qsearch-TT subsystem is at a local optimum at the current strength/TC. qsearch wants *more* accurate TT data + tight tactical focus, not less — adding `Exact` (M5.F.1) helped; injecting non-forcing moves (item 6) or trimming TT participation (items 2, 5) regress. (The per-item `tune/m5f-item*` experiment branches were deleted 2026-06-07 — the record is this entry + the per-item SPRT result docs under `bench/sprt/`, not branches.) **Production stays `M5.F.1`.**

- ~~Items 2, 4–8 below remain untried.~~ (the original tunable-space list is retained below as the historical record; each is tagged with its disposition.)

**Why active.** M5.F vs M5.E mixed-TC SPRT was inconclusive: Δ Elo +13.03 [−10.92, +37.12], `verdict=continue`, with a bimodal per-TC pattern (10+0.1 strong-positive, 60+0.6 slight-negative). Landed as "small-but-not-regression" per plan §11. The post-landing probe-only experiment (`bench/sprt/2026-05-09-m5.f-probe-only-vs-probe-and-store.md`) ruled out per-probe overhead as the cause of the slow-TC slight-regression, leaving Upper-bound looseness and TT-pressure pollution as the leading hypotheses.

**Tunable space.** Ranked by expected leverage on the slow-TC slight-regression (fast-TC gains are already strong; the goal is to recover the slow-TC end without losing the fast-TC end).

**High leverage:**

1. **Allow Exact at non-terminal completed-loop paths (relax Stockfish 45e5e65 rule).** Currently `qsearch_tt_bound_for_completed_node(best, beta)` caps non-terminal completed-loop bounds at Lower (cutoff) or Upper (no cutoff). Loosening the rule for path F only (when `best > alpha_initial && best < beta`) tightens qsearch's TT contribution. Stockfish achieves the same effect via richer path-tagging. Risk: false-Exact propagation. Cost: ~20 LOC + targeted SPRT.
2. **PV-node store suppression.** **[REJECTED 2026-06-01: −31.35 Elo, CI fully <0.]** Currently qsearch has no `is_pv` parameter; PV-adjacent positions get the same store churn as non-PV. Threading `is_pv` and skipping stores on PV nodes is the Stockfish convention. Reduces TT pressure exactly where bound looseness hurts most. Cost: ~50 LOC (signature change + call-site migration) + SPRT.
3. **Path-A (stand-pat cutoff) store gating.** Stand-pat cutoffs are by far the most frequent store path; they're stored as Lower with `best=stand_pat`. Suppressing path-A stores entirely would slash store count by a large fraction without losing much information (the position can be re-stand-patted cheaply on the next visit). Targets TT-pressure-driven slow-TC regression. Cost: ~5 LOC (one early return) + SPRT.

**Medium leverage:**

4. **Depth field convention (`-1` instead of `0`).** **[NO SHIP 2026-06-01: flat +6.08 [−16.44, +28.65]; premise stale (`depth` is `u8`, negamax never stores depth 0) — implemented as cross-gen negamax-eviction block; re-visit as a free-rider in a future TT-replacement change.]** Qsearch entries currently store at `depth=0`, indistinguishable from negamax `depth=0` under depth-preferred replacement. Using `-1` (or a sentinel like `DEPTH_QS`) lets replacement disambiguate — e.g., never let a qsearch entry displace a negamax entry of any depth, while still allowing q→q displacement. Cost: ~30 LOC across `tt.rs` replacement logic + SPRT.
5. **Probe gating by qsearch ply.** **[CLOSED 2026-06-01: bench canary — strictly adds nodes (d4 +12%@thr4, +4.4%@thr8); no favorable mechanism, ~0 Elo ceiling; no SPRT.]** Currently every qsearch frame probes. Skipping probes for the first N frames (where the TT data was just written by the negamax probe immediately above and a re-probe is redundant) or the last N frames (deep qsearch where TT data is stale) might avoid wasted work. Cost: ~5 LOC + sweep.
6. **TT-move ordering filter relaxation.** **[REJECTED 2026-06-01: −24.36 Elo, CI-lower <0 — searching quiet TT moves dilutes qsearch's tactical focus.]** Currently filter-gated via `moves_vec` membership (Andrew Grant's long-chain protection). Relaxing to "try the TT move even when not in `moves_vec`, with a make/legality check" recovers ordering wins at the cost of an extra make/unmake on misses. Cost: ~15 LOC + SPRT.

**Low leverage (~1–2 Elo each):**

7. **Per-path enable/disable for paths B/C/D/E.** **[CLOSED 2026-06-01 analytically: B/C/D/E are correctness-mandated (stalemate/mate/single-reply/false-stalemate-guard) → non-removable regardless of frequency; no code-size lever.]** Corner-case stores that fire rarely. Disabling individually has small per-path effect; mostly a code-size argument once SPRT-confirmed inert.
8. **Hash-size interaction sweep.** **[CLOSED 2026-06-01 analytically; optional probe RUN 2026-06-13 — single-seed positive, NOT actioned.]** Qsearch entries roughly double TT pressure. The optional `Hash=128`-vs-`64` self-SPRT (one `M5.K` binary both sides, only the UCI `Hash` option differs) was run: **Δ Elo +27.85 [+2.04, +53.98]** (mixed-TC + virtual-clock, 400 games, seed `…F0128`), mildly depth-amplifying (gain concentrates at 60+0.6 where more search → more TT pressure). So 128 > 64 *as a self-SPRT*, **but no engine change**: CI-lower (+2.04) is barely positive on a single seed (needs a confirm seed before treating as real), and the **default-`Hash` decision (currently 16 MiB) is a separate config tradeoff** — desktop strength vs the downstream mobile-footprint floor — not a qsearch-TT tuning lever. GUIs/harnesses override `Hash` anyway (the real item3 SPRTs ran at the engine default), so the default only matters for bare-CLI/embedded use. Documented as a single-seed signal; revisit if/when a default-`Hash` config decision is opened. Data: `bench/sprt/2026-06-13-hash128-vs-64.md`. Cost was 0 LOC (UCI option already exists).

**Validation methodology.** Mixed-TC SPRT against `M5.F` per the M5 convention. Items 1–3 are independent enough to SPRT separately; items 4–6 may interact (test in combinations once 1–3 land). Items 7–8 are diagnostic and may inform but not gate.

**Cross-references.** ADR-0028 (the "Open SPRT-tunable parameters" section is empty — this file supersedes it for now); `bench/sprt/2026-05-09-m5.f-vs-m5e-mixed-tc.md`; `bench/sprt/2026-05-09-m5.f-probe-only-vs-probe-and-store.md`; `docs/milestones/m5.f.md`.

---

### M5.G singular-extensions — verification-window boundary mutants + tunable-space sweep

**STATUS (2026-05-31 tuning campaign):**
- **Item 2 (`SE_MARGIN_PER_DEPTH = 2`) — REVERTED (flat).** SPRT vs `M6.J`
  (mixed-TC + virtual-clock, 400 games): Δ Elo **+20.87 [−3.30, +45.24]** — point
  estimate positive but CI straddles 0, and sharply bimodal per-TC (strong at
  mid-TC, negative at the extremes — the M5.G-v1/M5.H2 instability signature).
  Not a ship; `SE_MARGIN_PER_DEPTH` stays `1`. `bench/sprt/2026-05-30-c2-m5g2-*.md`.
  A future revisit could try `SE_MARGIN=2` *gated to mid-depth* or co-tuned with
  `SE_MIN_DEPTH`, but no single-knob value clears the gate now.
- **Item 3 (`SE_MIN_DEPTH 8→6`) — ALREADY SHIPPED / STALE.** The constant is
  already `6` (the M5.G v2 retune shipped it; see the `M5.G` tag annotation).
  This item was written pre-v2 and is moot; dropped.
- **Items 4–7 — ALL CLOSED, 0 ships; item 8 closed analytically (2026-06-01/02 campaign — [`docs/plans/tuning-m5g-se-items-4-8.md`](plans/tuning-m5g-se-items-4-8.md)).** Run against the `M5.F.1` tag (current production), mixed-TC + virtual-clock, 400-game cap, elo1=10. Evaluated in highest→lowest probability order; each independently SPRT'd off `main`@`82c94db`. **Net: nothing shipped — the SE subsystem is at a local optimum at the current strength/TC** (extending *more*, pruning *more*, and widening *eligibility* all regress or go flat).
  - **Item 6 (double extensions, `DOUBLE_EXT_MARGIN=50` + consecutive-cap) — REVERT.** Δ Elo **≈ −46** decisive over 91 games (W-L-D 17-29-45 ≈ 43%, consistent throughout; the run crashed at 91 on a self-inflicted UCI-handshake desync from stacking builds on the live SPRT, but the signal was unambiguous so no re-run).
  - **Item 5 (multi-cut on verification fail-high) — NO SHIP (flat).** Δ Elo **+1.74 [−21.76, +25.25]** (continue@400, ptnml [10,49,85,41,15]). Sound *heuristic* form: return the actual `verif_score` when `verif_score >= beta`, **no TT store** (RFP precedent), not the backlog's fabricated-`s_beta`+store form (which a plan-review caught as unsound). Prunes hard (d10 −30% nodes) but the speed gain exactly offsets the heuristic errors. ADR-0029 alt (g) stays deferred.
  - **Item 4 (Lower+Exact eligibility gate) — NO SHIP.** Δ Elo **−15.65 [−41.21, +9.76]** (CI-lower <0). Admitting Exact entries at clause 6 adds +15% d10 verification cost that doesn't pay off (Exact-at-non-PV survives to the SE block only in the narrow `depth−SE_TT_DEPTH_DELTA ≤ tt_depth < depth` window). ADR-0029 §1 Lower-only gate stands.
  - **Item 7 (PV-SE) — NO SHIP.** Δ Elo **−21.74 [−47.60, +3.87]** (CI-lower <0). Dropping clause 3 (`!is_pv`) extends SE to PV nodes; near-zero bench delta (PV nodes rarely carry Lower entries, and clause 6 still requires Lower) but mildly negative in play — verification cost on hot nodes without payoff. ADR-0029 §1 clause 3 / §8 stand.
  - **Item 8 (propagated `singular_ext_active` flag) — CLOSED analytically (no SPRT).** Perf/NPS-mitigation only, no standalone strength mechanism. The instrument the backlog implied (`se_extensions` density) is `#[cfg(test)]`-only and counts the wrong thing; the correct signal is a release-NPS comparison, which shows **no deep-TC verification-subtree NPS regression** (`M5.F.1` holds 5.50 Mnps@d10 / 5.20 Mnps@d12). With the ADR-0029 §5 geometric chain bound (≤3) and no regression observed across M5.G→M6.J→M5.F.1, the 80-LOC stack-flag has nothing to recover. Re-open only if a future change surfaces a measured deep-TC NPS regression.

  **Process lessons (full detail in the plan's CLOSED section):** (a) no engine-spawning/heavy-CPU work during a live SPRT (the C-6 crash); (b) `cargo mutants` hangs under the command sandbox on whole-function-replacement mutants — use `--timeout` + `-- --lib` + sandbox-off, or the manual apply-test-revert technique; (c) loosening any eligibility clause ⇒ grep for the stale `…does_not_fire…` integration test (both C-4 and C-7 had one).

~~Items 4–8 below remain untried.~~ (the tunable-space list is retained as the historical record; each is tagged with its disposition.)

**Why active.** M5.G's `cargo mutants --in-diff` campaign caught 58/60 viable mutants (96.7%). The two missed mutants are **both at `src/search.rs:1770:24`** — the `s_beta - 1` expression in the verification call's alpha argument:
1. `replace - with +` → window becomes `(s_beta + 1, s_beta)` (alpha > beta, inverted/degenerate).
2. `replace - with /` → window becomes `(s_beta, s_beta)` (alpha == beta, zero-width).

Neither is observably distinct from the original `(s_beta - 1, s_beta)` null window on the M5.G integration-test fixtures: the test corpus drives `verif_score` either far below `s_beta` (fail-low — both windows agree, extension fires) or far above `s_beta` (fail-high — both windows agree, no extension). The boundary case `verif_score == s_beta` is the only place where the windows differentiate, and no fixture engineers an exact-equality boundary.

**Tunable space and follow-up tests.**

1. ~~**`negamax_se_extension_at_singular_beta_boundary` test** (boundary-engineering fixture).~~ **DONE 2026-05-14.** Landed at `src/search.rs::tests::negamax_se_extension_at_singular_beta_boundary`. Verified via `cargo mutants --file 'src/search.rs' --re 'search\.rs:(1759|1775)'` — all four mutants caught (the two `s_beta - 1` arithmetic mutants plus the two `verif_score < s_beta` comparison mutants `< → ==` and `< → >`). Fixture uses pre-stored Exact TT entries at every non-excluded child's zobrist to deterministically land `verif_score = s_beta` at the verification frame, exploiting the fail-soft cutoff's "alpha ≥ beta fires on first move regardless of score" behavior to distinguish production from the `+` / `/` mutants.
2. **`SE_MARGIN_PER_DEPTH = 2` retune** (Stockfish-historical default). Current value is 1 (Xiphos / Ethereal). A wider margin tightens the verification's eligibility (fewer extensions, less verification cost) at the price of missing some genuinely-singular nodes. SPRT should resolve. Cost: 0 LOC + mixed-TC SPRT.
3. **`SE_MIN_DEPTH = 6` retune** (Xiphos default). Current value is 8 (literature majority). Lowering enables SE on shallower nodes, increasing firing rate. Cost: 0 LOC + mixed-TC SPRT.
4. **Lower+Exact gate at clause 6.** **[NO SHIP 2026-06-02: −15.65 [−41.21, +9.76], CI-lower <0; +15% d10 verification cost without payoff.]** Currently Lower-only. At non-PV nodes Exact entries are rare but extend SE's eligibility surface. Cost: ~5 LOC + SPRT.
5. **Multi-cut on verification fail-high.** **[NO SHIP 2026-06-02: flat +1.74 [−21.76, +25.25]; sound form = return actual `verif_score` when `>= beta`, NO store — the backlog's "return `s_beta` + store" form was plan-review-rejected as unsound (depth-specific bound over a TT-move-excluded game).]** Turning the lost SE bet into a pruning win. Cost: ~30 LOC + SPRT.
6. **Double extensions** (extend by +2 when verification fails low strongly, e.g. `verif_score < s_beta - 50`). **[REVERT 2026-06-01: ≈ −46 Elo over 91 games; double-extending regresses at current strength. Needs a consecutive-double-ext cap to avoid node explosion — implemented + canary-clean, but the change itself loses.]** Cost: ~20 LOC + SPRT.
7. **PV-SE** (extend SE eligibility to PV nodes). **[NO SHIP 2026-06-02: −21.74 [−47.60, +3.87], CI-lower <0; verification cost on hot PV nodes without payoff.]** Cost: ~5 LOC + SPRT.
8. **Propagated `singular_ext_active` flag** through `search_child` (Stockfish-style stack tracking). **[CLOSED analytically 2026-06-02: no standalone strength mechanism; release-NPS canary shows no deep-TC verification-subtree regression (5.50 Mnps@d10 / 5.20@d12) + §5 geometric chain bound ≤3 ⇒ nothing to recover. Re-open only on a measured deep-TC NPS regression.]** Only relevant if SPRT shows verification-subtree NPS regression at deep TC. Cost: ~80 LOC (parameter migration through `search_child`) + SPRT.

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
- NNUE (M11) re-trains the eval from scratch; an ML aspiration model trained on PeSTO eval would need re-training post-NNUE.

**When to land.** Four-tier escalation, each tier proceeding only if the previous shows ≥ +5 Elo of remaining headroom worth chasing:

1. **Fixed schedule (M4.D)** — ±50 default, width-tuned via mixed-TC SPRT. Ships at M4.D close.
2. **TC- or depth-adaptive parametric (post-M5, pre-M11)** — `base · max(min_factor, 1 - α·(depth - threshold))`. SPSA-tuned. Cheapest validation of the depth/TC interaction motif. **ATTEMPTED 2026-06-02 (campaign item B) — NO SHIP (flat).** Linear depth-adaptive first-try width (`BASE=50, SLOPE=4, MIN=16`; depth 6 = 50 unchanged, narrowing to 16 by depth ~14.5), hand-picked defaults (no SPSA harness). SPRT vs `M5.F.1` (mixed-TC + virtual-clock, 400 games, seed `…E0B0`): **Δ Elo −9.56 [−32.51, +13.32]**, CI-lower < 0. Per-TC noisy/bimodal with **no depth-amplifying trend** (10+0.1, where width ≈ base, was the *worst* bucket) ⇒ the narrowing itself is net-neutral-to-harmful at current strength, not a tuning-magnitude issue. Consistent with the ~+2–4 Elo ceiling being below the CI bar. A gentler-slope variant is unmotivated (no per-TC direction to tune toward). `bench/sprt/2026-06-02-b-depth-adaptive-aspiration-vs-m5f1.md`; `bench/sprt/patches/b-depth-adaptive-aspiration.patch`. Tier 3/4 (delta-baseline / MLP) stay gated on tier-2 showing ≥+5 Elo headroom — which it did **not** — so they sink further down the queue (revisit post-M11/NNUE).
3. **Cheap delta-baseline (post-M11 or earlier if (2) saturates)** — `window = clamp(k · |score(d-1) − score(d-2)|, MIN, MAX)`. Three SPSA-tunable params.
4. **MLP (post-M11, only if (2)+(3) saturate)** — full feature set + hidden layer.

**Estimated size.** Adaptive parametric: ~30-50 LOC + SPSA harness reuse. Cheap delta-baseline: ~50 LOC + SPSA harness reuse. ML model: ~500-800 LOC (feature extraction + inference + offline-training pipeline as a separate Python or Rust binary), plus the corpus + label-generation infrastructure (potentially shared with Texel tuning or NNUE data prep).

---

## M5.H2 SEE-split capture stage (good vs bad captures)

**Pulled from M5.H1 plan §1 / ADR-0030 §8.** Split the captures stage into "good captures" (SEE ≥ 0) yielded immediately after TT and "bad captures" (SEE < 0) yielded last, after quiets. Per CPW Static Exchange Evaluation + Move Ordering pages.

**Prerequisites.** SEE infrastructure (no current ETA in roadmap; M11+ candidate alongside NNUE training). Without SEE, the MVV-LVA-only capture sort is the recommended starting point per research §6.2.

**Estimated Elo gain.** Hard to disentangle from SEE itself; captures-split is one of several SEE consumers. Tuning-backlog entry rather than roadmap-promised.

**Estimated size.** ~50–100 LOC inside `MoveStager` once SEE exists.

---

### M6.I sign/monotonicity-constrained deferred-term retune — CLOSED NO-SHIP 2026-06-07 (−64.13 Elo; the corpus genuinely wants the unconstrained shapes)

**Update 2026-06-07 — CLOSED NO-SHIP. The constrained retune REGRESSES −64.13 Elo vs production.** Ran the deferred retune+SPRT half (the sign-projection numerics had landed 2026-06-03; see below). Recipe per the locked `docs/plans/tuning-overnight-2026-06-03.md` §item-7: rebuilt the 16M-record flat cache, warm-started from shipped (now M6.J, not M6.I), `texel-tune tune --sign-project --mono-lambda 5e-3`. The constrained refit **improved held-out val-loss −0.35%** (0.142272 vs shipped 0.142766 — the corrected sign table, with mobility/conn left unconstrained, does NOT make the fit worse; the 06-03 "+0.38%" came from the over-broad first table that mangled the centered mobility terms) **and correctly fixed the documented sign violations** (`ISO_MG +4→0`; `PASSED_MG [0,0,-28,-14,…]→[0,0,0,0,5,45,48,0]`, now rank-monotone). But the SPRT vs `M5.K` (current production = M5.K search + M6.J eval; mixed-TC + virtual-clock, elo1=5, seed `…F0007`, 400 games) was **decisively negative: Δ Elo −64.13 [−92.00, −37.04]** (pentanomial, 200 pairs, verdict=continue at cap), and **depth-amplifying** — flat at 10+0.1 (~50%) / 20+0.2 (~52%), collapsing to ~35% at 40+0.4 and **~27% at 60+0.6**. Likely driver: the refit inflated `PASSED_EG` to `[0,0,21,66,104,89,60,59]` (roughly doubled + pushed onto the back ranks), and deep search reaches the endgames where passed-pawn EG eval dominates. **Production HEAD unchanged: M6.J eval / M5.K search, bench d4 `112020` / d7 `1326598`.** Data: `bench/sprt/2026-06-07-signmono-constrained-eval-vs-m5k.md`; constrained vector retained at `bench/tune/2026-06-07-signmono-constrained.json`.

**Lessons.** (1) **Val-loss is a poor Elo predictor — reaffirmed with the sign flipped:** a 0.35% val-loss *improvement* bought a 64 Elo *loss* (cf. M6.J's cold/warm 2-µ-loss = +41 Elo). (2) **The counterintuitive M6.I/M6.J sign shapes are not cleanup targets via a constrained refit** — forcing principled signs and recompensating produced a decisively worse-playing eval; the corpus genuinely wants the unconstrained shapes (this is the item's own "abandon if … the corpus genuinely wants the unconstrained shapes" criterion, now realized at SPRT rather than val-loss). (3) **Attribution: RESOLVED — refit-drift, NOT the sign clamps** (control run 2026-06-07, `bench/sprt/2026-06-07-unconstrained-refit-control-vs-m5k.md`). Reran the identical recipe *minus* the constraints (same paired seed `…F0007`): the **unconstrained** refit scored **−68.63 [−94.92, −43.09]** — statistically identical to the constrained −64.13 (marginally worse, despite a tighter val-loss 0.142186). So **a fresh full corpus refit regresses ~−65 Elo vs M6.J regardless of sign/mono constraints**; the clamps are exonerated. The unconstrained refit moreover **reproduced and amplified** the counterintuitive shapes (`ISO_MG=+7`, `PASSED_MG=[0,0,-41,-32,1,39,43,0]`) ⇒ **the corpus genuinely wants the prior-violating signs**, so the sign-shape "cleanup" is moot (the data prefers the violations; they are not what regresses play). The real failure is that **M6.J's specific tuned vector is not reproducible by a naive warm-start refit on the re-materialized corpus** — val-loss improves, play craters; the shared inflated `PASSED_EG` drives the common 60+0.6 collapse in both refits. The **durable deliverable** remains the sign-projection optimizer numerics (`edf6246`), reusable by any future constrained tune. **Practical takeaway for any future eval re-tune: do not expect a corpus Texel refit to hold M6.J's strength — the gap between the val-loss optimum and the Elo optimum is large and not crossable by warm-start refitting; a different mechanism (NNUE / SPSA-on-games) is needed.**

**Cross-ref:** the Arm-B PST co-tune item (above) — the natural merge target for any future eval-weight re-opening — is itself sensitivity-gate-checked NO-GO/deprioritized (2026-06-03), so the whole eval-weight surface is now closed pre-NNUE. King Activity / AKPC residual gaps + this sign-shape question all defer to M11/NNUE.

---

_Original deferral note (2026-05-25), retained for the record:_

**Status.** M6.I shipped (SPRT H1, +93.86 Elo vs `M6.F`) with the deferred-term vector that the unconstrained Texel pass produced. Several individual terms settled at counterintuitive, prior-violating values that the joint game-result fit nonetheless validated in aggregate:

- `ISO_MG = +5` — isolated-pawn MG is a small *bonus*, not the expected penalty (`ISO_EG = -1`, correct sign).
- `PASSED_MG = [0,0,-30,-17,25,36,40,0]` — **negative MG for early-rank passers** (ranks 2–3), and the passed bonus is not EG-dominant at rank 6 (eg 37 < mg 40).

These survive because the M6.I tune used only ridge-toward-shipped + light monotonicity smoothing, not hard sign/shape constraints, and the corpus is self-play-heavy (correlated-feature imprinting is plausible). They are minor, weakly-identified terms; the ship is sound. The eval-term test suite dropped the now-violated positional-prior assertions (ISO ≤ 0; passed EG-dominance/monotonicity) — see `docs/milestones/m6.i.md` "Test updates".

**Why deferred, not done now.** Re-tuning under sign/monotonicity constraints needs its own SPRT (any weight change vs the shipped `M6.I` is a strength claim). It is not free, and the expected delta is small (these are minor terms). Worth doing only when a tuning slot opens and ideally folded into the larger Arm-B PST co-tune below (which already re-opens the whole eval-weight surface).

**Recommended approach at promotion.** Warm-start from the shipped `M6.I` vector. Add per-term sign constraints (penalty terms ≤ 0; passed/connected bonuses ≥ 0 and rank-monotone) as projected-gradient clamps or strong one-sided ridge. Pick the constraint strength by held-out validation loss; SPRT the winner vs `M6.I`. **Baseline = the `M6.I` tag.** Abandon if (a) constrained validation loss is materially worse (the corpus genuinely wants the unconstrained shapes), or (b) M11/NNUE lands first (obsoletes classical weights).

**Update 2026-06-02 (campaign item C — examined, NOT run; folded into Arm-B per user decision).** Inspected the Texel harness (`src/bin/texel-tune.rs`, `src/texel/{optimizer,loss}.rs`):
- `tune` warm-starts from `EvalParams::shipped()` ✓.
- `Reg` already supports **L2 ridge toward shipped** (`l2_lambda`) + **monotonicity** (`mono_lambda`); both wired into `loss_and_grad` but **hardcoded to 0.0 in `cmd_tune`** (no CLI flag — ~6 LOC to expose).
- **No hard sign constraint** (projected-gradient clamp) exists, and the L2 ridge "pulls toward production, never toward literature" — so it pulls toward the *wrong-signed* shipped value (`ISO_MG=+5`), i.e. does **not** fix the sign violations that are this item's defining target.

⇒ Doing this item *faithfully* (sign **and** monotonicity) needs **new constrained-optimizer numerics** (sign-projection + tests + review), not a config run. The monotonicity half is cheap; the sign half is not. Given the small expected delta + the 2026-06-02 campaign's two same-night no-ships (A(a), B) indicating a local optimum at current strength, the user chose to **defer and fold this into the Arm-B PST co-tune** (both re-open the eval-weight surface; Arm-B can add sign/monotonicity projection once). When promoted: implement sign-projection in `optimizer.rs` + expose `--l2-lambda`/`--mono-lambda` in `cmd_tune`, warm-start from shipped, select constraint strength by held-out val-loss, SPRT vs current production.

**Cross-references.** `docs/milestones/m6.i.md` ("Counterintuitive term shapes"); ADR-0037 §5 (ridge-toward-shipped + monotonicity, the regularization that was *not* hard-constraining); the Arm-B item below (natural merge target — both re-open the eval-weight surface against the corpus); `docs/plans/tuning-overnight-2026-06-02.md` (the campaign + C scoping discovery).

---

### M6.I PST co-tuning (joint full-eval retune) — DEFERRED until M6.I lands; gated on M6.I sensitivity diagnostics

**Status.** Not yet attemptable. M6.I as scoped (roadmap §M6, ADR-0032 §8 / ADR-0033 §7-8) is a Texel pass over the **deferred-term surface only** (~180 weights: M6.B ISO/DBL/BWD + CONN, M6.C passed-pawn, M6.D mobility, M6.E king-safety, **plus the small M6.F Tier-1 feature-weight additions** — outpost rank-table + rook open/semi-open-file + endgame-scaling coefficients, on the order of ~15–25 more weights) with the vendored PeSTO material + 12 PSTs (768 entries) **held frozen as the reference frame**. This entry captures the *next* campaign: relaxing that freeze and co-tuning the PSTs jointly with the deferred terms.

**Why deferred, not abandoned — the bias is real and empirically confirmed.** PeSTO is *Piece-Square-Tables-Only*: it was Texel-tuned as a complete standalone eval (material + PST, nothing else). Every PST entry therefore has the smeared-out average of pawn-structure / mobility / king-safety value baked into it — exactly the factors M6.B–E now model explicitly (and the M6.F outpost / rook-file terms add a further centralization-overlap axis against the frozen N/B/R PSTs). Frozen-PST + an explicit term ⇒ systematic double-count, and the per-phase screens **measured this directly**: M6.D found `ROOK_EG`/`QUEEN_EG` over-magnitude *against the frozen PeSTO PSTs*; M6.E research verdict was "PeSTO MG king PST already prices ~30–50 cp of castled-king safety." Frozen-PST tuning can only ever *shrink the new term toward zero* to kill the double-count; it is structurally forbidden from doing the correct thing (deflate the over-loaded PST entry, let the explicit term carry the signal). So M6.I-as-scoped has a known, one-directional, **recoverable** bias: it underweights the M6.B–F terms relative to their structurally-ideal values. This campaign is how that residual is recovered.

**The principled formulation — one nested experiment, not two disconnected runs.** "Frozen" and "co-tuned" are the two endpoints of a single ridge-regularization parameter λ on each PST entry's deviation from its vendored PeSTO value:

- λ → ∞ : PSTs pinned ⇒ *exactly M6.I as scoped* (call this Arm A — the control, the warm start, the fallback, the interim/often-final shipped product).
- λ → 0 : PSTs fully free ⇒ full co-tune (Arm B — high-variance, ~970 strongly-correlated (ill-conditioned, not rank-deficient) params: the ~950 PST+deferred-term surface plus the ~15–25 M6.F Tier-1 feature weights).

Run order is forced (not a preference): **A first** — it is M6.I's committed deliverable, M6 *closes* on it, and B's entire headline result is the effect size **B − A**, which is uninterpretable without A as the control. Then, if gated in, walk λ downward from ∞, select λ by **held-out validation loss**, confirm the winner by SPRT. Frozen-PST is thereby reframed as the *prior mean*; λ is how strongly the corpus must argue before it may overrule PeSTO. This caps B's overfitting continuously instead of all-or-nothing.

**Trigger / go-no-go gate.** ALL must hold before promoting this campaign:

1. **M6.I (Arm A) has landed and is tagged** (`M6.I` tag exists; baseline for B is the `M6.I` tag, **not** `M6.F` and **not** `M6.E`).
2. **A's mandated per-parameter sensitivity table shows material residual double-count** — i.e., A landed at verdict 3/4, OR the table shows M6.B–F term weights pinned near zero by PST↔structural-term ill-conditioning (strong correlation, not exact collinearity). If A landed a clean verdict-1 with healthy non-zero term weights, the frozen-PST bias was small ⇒ this campaign sinks down the queue (low expected upside).
3. **A corpus + train/validation-split + regularized-Texel harness exists.** ~970 strongly-correlated params against ≤1.58-bit, position-noisy game-result labels is the classical underparameterized-*but-ill-conditioned* regime (NOT the LLM overparameterized-but-generalizing regime — no implicit-regularization / rich-label rescue here). **The hazard is conditioning × label-noise, not rank-deficiency:** the system is *identified* (the per-phase screens recovered clean per-term Elo — impossible under exact collinearity), but estimate variance along an eigenvector of XᵀX scales like σ²/λᵢ; the PST↔structural-term directions have *small-but-nonzero* λᵢ (strongly correlated, not degenerate) while the game-result-label σ² is large ⇒ **large-but-finite** variance along them ⇒ the optimizer lowers the proxy (logistic loss) while Elo does not follow (proxy-overfit). They are *shallow* (low-curvature) directions, not flat; the optimum is a poorly-determined *point*, not a manifold. Ridge-toward-PeSTO is exactly the conditioning fix — it lifts the small λᵢ. Requires: ridge penalty toward frozen PeSTO, held-out validation for λ-selection and final model selection, a larger/cleaner corpus than Arm A needs.

**Recommended approach at promotion.** Warm-start the optimizer from A's converged vector (cold-start B is precisely the ill-conditioned / proxy-overfitting failure mode). Unfreeze the 768 PST entries under ridge-toward-PeSTO. Sweep λ; pick by held-out validation loss; SPRT the winner vs `M6.I`. **Higher success bar than A's verdict ladder:** co-tuning forfeits the verbatim-vendored-PeSTO diff-oracle (our only external bug/provenance check on the PST block — disproportionately load-bearing here since the eval is not inspected line-by-line by design, CLAUDE.md user profile). B must therefore beat A by a clear, robust margin that *also* justifies losing that oracle — not merely CI-lower > 0.

**Expected impact.** Recovery of the under-weighting bias the frozen-PST screens left on the table across M6.B–F. Magnitude unknown until A's sensitivity table quantifies the pinned-near-zero residual; plausibly modest (the per-phase screens suggest the dominant double-counts are a handful of axes: pawn-shield × PeSTO king MG PST, slider mobility EG × PeSTO slider PST, CONN × ISO, and the M6.F outpost/rook-file × PeSTO N/B/R centralization overlap). Bounded upside, real provenance/variance cost — hence gated, not queued.

**Hard rejection criteria.** Abandon permanently if: (a) the λ-sweep's held-out-validation-optimal λ is ≈ ∞ (data does not want the PSTs to move — frozen was right); or (b) the best λ beats A on validation loss but SPRT-regresses or is flat vs `M6.I` (proxy-overfit confirmed, the regime warning realized); or (c) M11/NNUE has landed first — NNUE re-trains the eval from scratch and obsoletes all classical PSTs, zeroing this campaign's value. Priority therefore decays as M11 approaches; this is a *pre-M11* opportunity only.

**Cross-references.** Roadmap §M6 (M6.I scope detail, frozen-PST commitment; M6.G corpus scope detail; M6.F Tier-1-feature scope detail); ADR-0032 §8 / ADR-0033 §7-8 ("re-derive … against our PeSTO PSTs" — the freeze, whose rationale this entry records explicitly); `src/eval/data.rs` (`MATERIAL`/PST arrays — the frozen reference frame); the M6.D/M6.E per-phase screens (empirical double-count evidence). Conceptual lineage: the gauge-freedom / K-anchoring discussion (centipawn scale is set by the vendored PeSTO magnitude, pinned by freezing K against the M6.F baseline; co-tuning would re-gauge — out of scope for A, in scope for B under the ridge-toward-PeSTO prior).

**Corpus dependency (added at M6.G landing, 2026-05-20; amended 2026-05-21 for the four-source taxonomy):** the corpus + held-out split + train/val split this campaign requires is **`bench/corpus/`** — the operator-materialized artifact produced under ADR-0035. Specifically Arm B reads `bench/corpus/manifest.json` (campaign knobs + `corpus_sha256`) + `bench/corpus/filter_spec.txt` (the pinned M6.G↔M6.I interface constants: `QUIET_MARGIN_CP`, `OPENING_SKIP_PLIES`, `HIGH_SCORE_CP`, `PER_GAME_CAP`, `FEN_LEAKAGE_TAU`) + the binary shard log (`shard.bin` + `val.bin`, gitignored per ADR-0035 §8 — operator-materialized via `bench/corpus/re-run.sh`). The scoring closure pinned by the M6.G→M6.I interface is `corpus::quiet::static_eval_white` on quiet-certified positions (NOT qsearch at tune time). For an Arm B campaign the operator runs one `corpus selfplay` campaign per `--opening-mode={book,random}`; external sources arrive via M6.H's `corpus fetch --source={ccrl,lichess}` (on-demand streaming + early-termination + in-RAM resume — see Roadmap §M6 M6.H scope detail) or via operator-staged `corpus ingest-pgn` against already-on-disk PGN files. The `Source` accept-list = `{SelfPlayOnBook, SelfPlayOffBook, Ccrl, LichessOpen}` (Zurichess `c9` REJECTED by the ADR-0003 audit per research §3 — Stockfish-080916 played-game results); the on-book / off-book share is a training-time per-source reweighting axis at the outer optimizer level, mechanically identical to the CCRL / Lichess proportion (ADR-0035 §10).

---

### M6.I king-safety attacker S-curve — SPSA-deflate + SPRT (post-M6.I; the one M6 term Texel left off)

**Status.** **CLOSED NEGATIVE at M6.K (2026-05-29) — the attacker S-curve was REMOVED; not pursued further pre-NNUE.** Both mixed-TC SPRT probes vs `M6.J` regressed: stage 1 (g=1 literature) **Δ Elo −44.54 [−72.13, −17.50]**, stage 2 (g=0.5 deflate) **~−48** (same class — deflation didn't salvage it). The M6.E double-count vs the PeSTO king PST is real and persists at half magnitude (optimum g≈0), so the S-curve was deleted (eval-neutral vs `M6.J`). See ADR-0038 + `docs/milestones/m6.k.md`. King Activity / AKPC remain the largest residual classical-eval gaps but are now deferred to NNUE (M11), which re-learns king safety from scratch. Re-addable from git + ADR-0038 if ever revisited. The structural/semantic rationale below is retained as the record.

_Original deferral note (M6.I harness landing, 2026-05-24):_ M6.I tuned the *linear* king-safety terms (pawn-shield + open/semi-open-file) but **excluded the attacker-count S-curve** (the 100-entry `KING_SAFETY_TABLE` + the 4 per-kind `KING_ATTACK_WEIGHT_*` multipliers) — it shipped at zero (off), identical to M6.F. ADR-0037 §3 / `docs/milestones/m6.i.md`.

**Why Texel-on-quiet-positions can't do it (so a different tool is needed).**
- **Structural:** the term is non-linear — `score = TABLE[Σ multiplier·attackers]`. M6.I's speed comes from a linear feature-cached model; a non-linear term doesn't fit it, and with the multipliers frozen at their shipped zero the table index freezes to 0 (only `TABLE[0]` reachable). Making the index live needs non-zero multipliers, which either break linearity (tunable) or break the M6.I faithfulness cross-check (frozen-at-literature ≠ the shipped-zero `static_eval`).
- **Semantic:** the S-curve predicts a *future* tactical collapse; the corpus is deliberately *quiet* (no imminent tactics) and scored by `static_eval`, so the data is selected to exclude exactly this term's signal, and the game-result label is a weak target for a momentary attack count.
- Conclusion: king danger's value is realized in **games**, so tune it on games (SPSA + SPRT), not on a quiet-position static regression. This is standard engine practice (mACE tuned its king-safety table by GA over play; `docs/research/m6-texel-tuning.md` §4 = freeze the non-linear params and sweep them outside the linear core).

**Tunable space (compact — do NOT tune 100 table entries; no signal, guaranteed overfit).** Keep the literature CPW S-curve *shape*; tune the ~5 knobs that scale it:
- `KING_ATTACK_WEIGHT_{N,B,R,Q}` (4) — literature 2/2/3/4.
- A global gain `g ∈ (0,1]` on the table output (the deflation lever for the PeSTO double-count) — or, equivalently, scale the literature `KING_SAFETY_TABLE` by `g` at apply time.
- Optionally the gate (`<2 attackers ∨ no queen`) thresholds — leave fixed first.

**Approach.**
1. **Cheap probe (1 SPRT, ~hours):** turn on the literature S-curve at full magnitude (multipliers 2/2/3/4, literature table, `g=1`) and mixed-TC SPRT vs the `M6.I` tag. This directly tests the M6.E HIGH-transfer-risk worry — does the PeSTO MG king PST's already-priced ~30–50 cp of castled-king safety make the literature term a **double-count**?
   - Gains → ship, done.
   - Flat / regresses → the double-count is real → step 2.
2. **SPSA-deflate:** tune `g` (and, if needed, the 4 multipliers) by SPSA against SPRT. The deflation lets the explicit term carry king danger while shrinking enough not to re-count what the PST already encodes. ~5 params → a handful of SPRT-equivalent campaigns. (SPSA harness is a small extension of `elo-iterate`; or a manual coordinate grid over `g ∈ {0.25,0.5,0.75,1.0}` × a coarse multiplier sweep.)

**Validation.** Mixed-TC SPRT vs the `M6.I` tag (post-M6.I, so king safety co-exists with the tuned mobility/pawn weights it interacts with). Apply via a small extension of `texel-tune apply` (or hand-edit the marked `eval::data` king-safety regions — the markers exist).

**Trigger / go-no-go.** Promote after the M6.I SPRT resolves and `M6.I` is tagged. Skip if M11/NNUE is imminent.

**Opportunity-cost caveat (priority decays as M11 approaches).** King safety is exactly the nonlinear pattern NNUE (M11) learns natively and obsoletes — the same decay logic as the "M6.I PST co-tuning" Arm B. Worth the cheap on/off SPRT + a light SPSA deflation *if it pays*; **not** worth heavy hand-tuning of the curve shape. A pre-M11 opportunity only.

**Cross-references.** ADR-0037 §3 (the exclusion + its structural/semantic reasons) / §4 (the non-linear-params-outside-the-linear-core pattern); `docs/milestones/m6.i.md` (king-safety scope + lessons); ADR-0033 / `docs/research/m6-king-safety.md` (M6.E's HIGH transfer-risk verdict — the double-count this campaign measures); `docs/research/m6-texel-tuning.md` §3-§4 (freeze-and-sweep for non-linear terms; quiet-corpus blind spot).

---

*Active queue ends here. New items append above this line.*

---

## Done

### ~~Corpus-mixture meta-tune (post-M6.I) — RE-RUN PRECONDITIONS~~ — DELIVERED by M6.J 2026-05-29

The bi-level corpus-mixture meta-tune (aborted 2026-05-26 post-mortem at [`../milestones/meta-tune-postmortem.md`](../milestones/meta-tune-postmortem.md)) was re-run as part of M6.J. **Shipped:** mix `[0.32, 0.15, 0.23, 0.31]` at val_loss 0.142749, K = 0.005690 (cold-start from zero weights, not the warm-start the original entry called for — exposed that warm-start near-optimum and cold-start global optimum were 2 µ-loss apart but **+41 Elo** apart in play). Algorithmic root cause beyond the postmortem's three was a fourth: the M6.I "coarse N-M-style" simplex was **reflect-only**, which structurally stalls on flat surfaces (any time the reflection is rejected, the simplex is unchanged and the next iteration recomputes the same number). M6.J's `b1c4b74` replaced it with the full textbook Nelder–Mead (reflect / expand / contract / shrink) + softmax R³ reparameterization (removes the `clamp + renormalize` discontinuity at the simplex boundary) + Kelley sufficient-decrease restart (guards McKinnon-1998 degeneracy). The cold-start retune **converged at iter 8/30 via `obj_spread < EPS_F = 1e-6`** — the convergence stop, not the iter cap. SPRT vs `M6.I`: verdict=continue at 400-game cap, pentanomial CI **+41.01 Elo [+18.26, +64.12]** ⇒ rung-1 ship by CI. See [`../milestones/m6.j.md`](../milestones/m6.j.md).
