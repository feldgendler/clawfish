# Sign/monotonicity-constrained eval retune vs `M5.K` — NO SHIP (−64.13 Elo)

**Date:** 2026-06-07 (overnight slot).
**Tuning-backlog item:** "M6.I sign/monotonicity-constrained deferred-term retune" (the
constrained-Texel-retune half of the Arm-B / sign-mono cluster).
**Baseline:** `M5.K` tag (current production: M5.K search layer + M6.J eval).
**Candidate:** working tree = M5.K search + a sign/monotonicity-constrained full Texel
refit of the eval weights (warm-start from shipped M6.J).
**Seed:** `0xC1ABF15AE10F0007`. Mixed-TC + virtual-clock, elo1=5 (eval-term gate),
elo0=0, alpha=beta=0.05, 400-game cap.

## Verdict

**NO SHIP — decisive regression. Δ Elo = −64.13 [−92.00, −37.04]** (pentanomial 95%
CI, 200 pairs). `verdict=continue` (LLR=−1.80; did not cross the SPRT bound at the
400-game cap, but the CI is fully negative and well below elo0=0). Production HEAD
unchanged: **M6.J eval / M5.K search, bench d4 `112020` / d7 `1326598`.**

```
sprt: verdict=continue llr=-1.80 elo0=0.0 elo1=5.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[32,62,66,27,13]
ci:   elo=-64.13 [-92.00, -37.04] pairs=200
```

Aggregate: W=93 L=166 D=141 over 400 games (40.9%).

## Per-TC — depth-amplifying regression

| TC | W | L | D | games | score |
|---|---|---|---|---|---|
| 10+0.1 | 24 | 23 | 55 | 102 | ~50.5% (flat) |
| 20+0.2 | 36 | 32 | 22 | 90 | ~52.2% (flat/+) |
| 40+0.4 | 24 | 54 | 24 | 102 | ~35.3% |
| 60+0.6 | 9 | 57 | 40 | 106 | **~27.4%** |

The regression is **monotone in depth**: flat at the fast TCs, catastrophic at 60+0.6.
This is the inverse of the ELOH.D "depth-amplifying gain is best" pattern — here a
depth-amplifying *loss*. The likely driver is the refit's EG terms: the constrained
refit inflated `PASSED_EG` from `[0,0,15,37,38,35,5,0]` to `[0,0,21,66,104,89,60,59]`
(roughly doubled, and pushed onto the back ranks). Deep search reaches endgames where
passed-pawn EG eval dominates, so an over-inflated/over-extended `PASSED_EG` curve is
exactly what would crater 60+0.6 while leaving 10+0.1 (which rarely resolves to those
endgames) untouched.

## The setup

1. **Cache:** rebuilt the 16M-record flat feature cache from the four `bench/corpus/`
   lanes (`texel-tune cache`, per-source 4M each). Baseline shipped val-loss on this
   cache = **0.142766** (K=0.005448, frozen from shipped).
2. **Constrained retune:** `texel-tune tune --sign-project --mono-lambda 5e-3`
   (the locked `docs/plans/tuning-overnight-2026-06-03.md` §item-7 recipe), warm-start
   from `EvalParams::shipped()` (= M6.J). Ran the full 10000-iter cap; val_loss =
   **0.142272** — i.e. **−0.35% BELOW shipped** (the constrained refit *improved*
   held-out loss). Vector saved at `bench/tune/2026-06-07-signmono-constrained.json`.
   - NB: this contradicts the 06-03 note's "+0.38% worse." That figure came from the
     over-broad first sign table that mangled the *centered* mobility/connectivity
     tables; with the corrected table (mobility/conn left unconstrained) the constrained
     refit improves val-loss.
3. **Sign violations corrected** (the item's defining target):
   - `ISO_MG`: `+4 → 0` (the wrong-signed isolated-pawn *bonus* clamped non-positive).
   - `PASSED_MG`: `[0,0,-28,-14,23,36,40,0] → [0,0,0,0,5,45,48,0]` (negative early-rank
     passers zeroed; table now rank-monotone).
   - `ISO_EG` stayed correctly negative (`-1 → -2`).
4. Applied, rebuilt (candidate bench d4 `118612` / d7 `1391585`), SPRT'd.

## Lessons

- **Val-loss is a poor Elo predictor — reaffirmed, hard.** Held-out val-loss improved
  0.35%, yet play regressed 64 Elo. This is the M6.J cold/warm-start lesson again (2 µ-loss
  apart → +41 Elo apart), now with the sign flipped: a val-loss *improvement* bought a
  large Elo *loss*. Do not trust a constrained-refit's val-loss as a ship signal.
- **The counterintuitive M6.I/M6.J sign shapes are NOT cleanup targets — at least not via
  a constrained refit.** Forcing `ISO_MG≥…→0` and monotone non-negative `PASSED_MG`,
  then letting the optimizer recompensate, produced a decisively worse-playing eval.
- **Attribution: RESOLVED — refit-drift, not the sign clamps** (control run 2026-06-07,
  `bench/sprt/2026-06-07-unconstrained-refit-control-vs-m5k.md`). Reran the identical recipe
  *minus* `--sign-project --mono-lambda`, same paired seed `…F0007`: the unconstrained refit
  (U) scored **−68.63 [−94.92, −43.09]** — statistically identical to this constrained run's
  −64.13 (U if anything marginally worse, despite a tighter val-loss 0.142186). So **a fresh
  full refit regresses ~−65 Elo vs M6.J regardless of the sign constraints**; the clamps are
  exonerated. Better still, U — unconstrained — *reproduced and amplified* the
  counterintuitive shapes (`ISO_MG=+7`, `PASSED_MG=[0,0,-41,-32,1,39,43,0]`), proving **the
  corpus genuinely wants the prior-violating signs**. The sign-shape cleanup is moot: the
  data prefers the violations, and they are not what regresses play — re-deriving the vector
  from the corpus at all is. The shared inflated `PASSED_EG` in both C and U drives the
  common 60+0.6 collapse.

## Artifacts

- Constrained vector: `bench/tune/2026-06-07-signmono-constrained.json` (retained).
- Feature cache: `bench/tune-cache.bin` (2.4 GB, gitignored/transient; rebuildable via
  `texel-tune cache` in ~minutes — delete to reclaim space, or keep for the optional
  control run).
- Match output: `target/matches/sprt/20260607T035840-M5.K-sprt/`.
- Run log: `$TMPDIR/sprt-signmono.log` (transient).
