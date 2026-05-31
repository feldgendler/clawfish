# C3 (M5.F-1): Exact at completed-loop qsearch nodes — SPRT vs M6.J

**Date:** 2026-05-30 (overnight tuning campaign).
**Decision: SHIP** (rung-1-by-CI per ADR-0037 §9 — CI lower bound > 0).

## Change

`qsearch_tt_bound_for_completed_node` becomes a three-way classifier: stores
`Exact` when `alpha_entry < best < beta` (improved entry alpha, no beta cutoff),
relaxing the M5.F Stockfish-45e5e65 Lower/Upper-only rule. `alpha_entry` is the
post-MDP / pre-stand-pat window alpha; paths C (single-reply) and F (main loop)
pass it. **Sound in this engine** (blind-review-verified): negamax delegates to
qsearch at depth 0 BEFORE its TT-cutoff probe, so a depth-0 qsearch entry never
triggers a negamax cutoff — the only consumer of a qsearch-Exact is qsearch's
own re-probe, where a completed fail-soft loop value with `alpha_entry < best <
beta` (full-window capture search) is window-independent. Bench: d4
`112497→112020` (via qsearch's own re-probe cutoffs), d7 `1354640`.

## Methodology

Baseline `M6.J`. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS, 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DDF07`. Out dir: `target/matches/sprt/20260530T061435-M6.J-sprt/`.

## Result

```
sprt: verdict=continue llr=2.65 elo0=0.0 elo1=10.0 pairs=200 ptnml=[4,39,80,64,13]
ci:   elo=+37.49 [+15.71, +59.58] pairs=200
```

**Δ Elo +37.49 [+15.71, +59.58]** (pentanomial 95% CI). Aggregate W-L-D =
111-68-221 ≈ 55.4%.

### Per-TC (diagnostic)

| TC | W | L | D | N | score |
|---|---|---|---|---|---|
| 10+0.1 | 28 | 20 | 40 | 88 | 54.5% |
| 20+0.2 | 21 | 21 | 52 | 94 | 50.0% |
| 40+0.4 | 23 | 22 | 69 | 114 | 50.4% |
| 60+0.6 | 39 | 5 | 60 | 104 | 66.3% |

Strongest at the **slow** TC (60+0.6 66.3%, near-zero losses) — a depth-amplifying
profile, **complementary to C1** (which was strongest at fast/mid TC). Same
point estimate as C1 (+37.49, coincidental) but a tighter CI (more draws). The
slow-TC strength is consistent with the mechanism: at deeper search, qsearch
transpositions are more frequent, so Exact re-probe cutoffs save more.

## Disposition

Ship. Patch: `bench/sprt/patches/c3-m5f1-qsearch-exact.patch`. C1 and C3 both
ship and both touch qsearch + the E51 bench pin → combine + combined-confirmation
SPRT (C1+C3 vs M6.J) before commit (the complementary per-TC profiles suggest
additive composition, but interaction must be confirmed, not assumed).
