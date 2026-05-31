# C2 (M5.G-2): SE_MARGIN_PER_DEPTH 1→2 — SPRT vs M6.J

**Date:** 2026-05-30 (overnight tuning campaign).
**Decision: DO NOT SHIP (revert).** CI straddles zero; bimodal per-TC.

## Change

`src/search.rs:525` `SE_MARGIN_PER_DEPTH = 1 → 2` (Stockfish-historical default;
wider margin tightens singular-extension eligibility). Confirmed live: d7 bench
`1357063→1334671` (fewer extensions); d4 bench unchanged at `112497` because SE
fires only at `depth ≥ SE_MIN_DEPTH=6` (bench runs at depth 4). SE boundary test
made margin-agnostic (`tt_score_raw = s_beta_value + depth*SE_MARGIN_PER_DEPTH`).

## Methodology

Baseline `M6.J`. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS, 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DDF05`. Out dir: `target/matches/sprt/20260530T040134-M6.J-sprt/`.

## Result

```
sprt: verdict=continue llr=1.04 elo0=0.0 elo1=10.0 pairs=200 ptnml=[9,46,74,54,17]
ci:   elo=+20.87 [-3.30, +45.24] pairs=200
```

**Δ Elo +20.87 [−3.30, +45.24]** — point estimate positive but CI includes 0, so
not a ship under the rung-1-by-CI rule (ADR-0037 §9). verdict=continue (LLR
between bounds), NOT H0-reject, so it is *inconclusive*, not a confirmed regress.

### Per-TC (diagnostic; bimodal)

| TC | W | L | D | N | score |
|---|---|---|---|---|---|
| 10+0.1 | 17 | 36 | 47 | 100 | 40.5% |
| 20+0.2 | 39 | 20 | 39 | 98 | 59.7% |
| 40+0.4 | 44 | 8 | 40 | 92 | 69.6% |
| 60+0.6 | 10 | 22 | 78 | 110 | 44.5% |

Sharply bimodal: very strong at mid-TC (20+0.2, 40+0.4), negative at both
extremes (10+0.1, 60+0.6). This is the M5.G-v1 / M5.H2 bimodal-instability
signature — the kind of pattern that does not robustly generalize. Combined with
the zero-straddling aggregate CI, the disciplined disposition is revert.

## Disposition

Revert; `SE_MARGIN_PER_DEPTH` stays `1`. The SE-margin axis is not a clear win at
current strength. A future revisit could try `SE_MARGIN=2` *gated to mid-depth*,
or co-tune with `SE_MIN_DEPTH`, but no single-knob value clears the gate now.
Patch preserved: `bench/sprt/patches/c2-m5g2-se-margin-2.patch`.
