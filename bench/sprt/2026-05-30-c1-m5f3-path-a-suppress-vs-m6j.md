# C1 (M5.F-3): suppress Path-A qsearch stand-pat TT store — SPRT vs M6.J

**Date:** 2026-05-30 (overnight tuning campaign, `docs/plans/tuning-m5f-m5g-overnight.md`).
**Decision: SHIP** (rung-1-by-CI per ADR-0037 §9 — CI lower bound > 0).

## Change

`src/search.rs` qsearch Path A (stand-pat fail-high): previously stored a Lower
TT entry (`score=stand_pat, depth=0, best_move=0`); now returns `sp` without
storing. Stand-pat is `evaluate_cached(pos)`, recomputed cheaply on re-visit, so
the store (the single most frequent qsearch store path) was near-pure TT
pressure. Blind review confirmed the entry was **value-identical** (only node
counts change, never returned scores). Bench re-pin: d4 `112497→112467`, d7 full
`1357063→1354385` (deterministic ×2).

## Methodology

Baseline `M6.J` tag (behavior-identical to current HEAD `266c314`, M6.K removal).
Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`, `--virtual-clock`, full QoS,
400-game cap, elo0=0/elo1=10, seed `0xC1ABF15AE10DDF03`.
Out dir: `target/matches/sprt/20260530T015944-M6.J-sprt/`.

## Result

```
converged: elo=827.98 sigma=4.38 games=400 reason=max-games   (elo= is harness online tracker, ignore)
sprt: verdict=continue llr=2.01 elo0=0.0 elo1=10.0 alpha=0.05 beta=0.05 pairs=200 ptnml=[12,33,76,58,21]
ci:   elo=+37.49 [+12.46, +62.92] pairs=200
```

**Δ Elo +37.49 [+12.46, +62.92]** (pentanomial 95% CI). Aggregate W-L-D =
142-98-160 ≈ 55.5%.

### Per-TC (diagnostic; per-bucket N<200 → noisy, NOT a gate per M5.I caveat)

| TC | W | L | D | N | score |
|---|---|---|---|---|---|
| 10+0.1 | 49 | 21 | 22 | 92 | 65.2% |
| 20+0.2 | 27 | 39 | 38 | 104 | 44.2% |
| 40+0.4 | 42 | 18 | 50 | 110 | 60.9% |
| 60+0.6 | 24 | 21 | 49 | 94 | 51.6% |

Broadly positive; strongest at 10+0.1 and 40+0.4. The 20+0.2 dip is within
per-TC noise (±~10% at N=104) and does not move the aggregate off its
robustly-positive CI. The original hypothesis (recover the M5.F slow-TC
regression) is met at 60+0.6 (51.6%, was slightly negative under M5.F) and the
win is broader than just slow-TC.

## Disposition

Ship. Patch: `bench/sprt/patches/c1-m5f3-path-a-suppress.patch`. To be committed
at campaign end (combined-confirmed if other candidates also ship).
