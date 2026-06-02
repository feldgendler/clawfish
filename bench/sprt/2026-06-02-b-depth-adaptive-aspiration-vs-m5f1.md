# B — depth-adaptive aspiration first-try width — SPRT vs M5.F.1

**Date:** 2026-06-02 (overnight tuning campaign, item B).
**Decision: NO SHIP** — CI straddles 0; flat-to-slightly-negative, no
depth-amplifying signal.

## Change

Replace the fixed `ASPIRATION_HALF_WIDTH=50` first-try aspiration half-width with
a depth-adaptive one:
`half_width(depth) = max(MIN, BASE − SLOPE·(depth − ASPIRATION_MIN_DEPTH))`,
defaults `BASE=50, SLOPE=4, MIN=16` (so depth 6 = 50 unchanged, narrowing to 16
by depth ~14.5). Tuning-backlog "ML-tuned aspiration window sizing" tier 2
(depth-adaptive parametric). ~35 LOC; new `aspiration_half_width` fn + 4 unit
tests; 3 existing aspiration_window tests updated. Blind final-review: no further
substantive issues. Mutants --in-diff: 0 real survivors. Bench d4 unchanged
`112_020` (aspiration fires at depth ≥ 6); d7 `1_354_640 → 1_350_273`.

## Methodology

Baseline `M5.F.1`. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS (concurrency 6), 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DE0B0`. Out dir: `target/matches/sprt/20260602T063245-M5.F.1-sprt/`.

## Result

```
sprt: verdict=continue llr=-1.07 pairs=200 ptnml=[9,59,76,46,10]
ci:   elo=-9.56 [-32.51, +13.32] pairs=200
```

**Δ Elo −9.56 [−32.51, +13.32]** — CI lower < 0 ⇒ NO SHIP. Aggregate
W-L-D = 86-97-217 ≈ 48.6%.

### Per-TC — noisy/bimodal, no depth-amplifying trend

| TC | W | L | D | N | score |
|---|---|---|---|---|---|
| 10+0.1 | 13 | 30 | 45 | 88 | **40.3%** |
| 20+0.2 | 39 | 24 | 33 | 96 | **57.8%** |
| 40+0.4 | 11 | 29 | 70 | 110 | 41.8% |
| 60+0.6 | 23 | 14 | 69 | 106 | **54.2%** |

No coherent depth trend (a depth-adaptive mechanism should, if anything, help
the deeper buckets). 10+0.1 — where the width is closest to the unchanged base —
is the worst bucket, arguing the narrowing itself is net-neutral-to-harmful at
current strength rather than a tuning-magnitude issue.

## Disposition

- **NO SHIP; revert.** Production stays `M5.F.1`.
- **Tier 2 (depth-adaptive parametric) closes flat** at current strength/TC.
  A gentler-slope or floor-only variant is conceivable but **unmotivated** — the
  per-TC pattern shows no direction to tune toward (the result is flat, not
  "right idea, wrong magnitude"). Consistent with the backlog's small-ceiling
  estimate (+2–4 Elo) being below the CI-lower>0 bar; here the point estimate is
  even mildly negative.
- Revisit conditions unchanged: the tier escalates only post-M11/NNUE or if the
  fixed schedule saturates with measurable headroom (`docs/tuning-backlog.md`).
- Patch preserved at `bench/sprt/patches/b-depth-adaptive-aspiration.patch`.
