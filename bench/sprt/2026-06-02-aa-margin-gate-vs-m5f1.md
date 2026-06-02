# A(a) — margin-gated Path-A store (recompose M5.F.3 w/ M5.F.1) — SPRT vs M5.F.1

**Date:** 2026-06-02 (overnight tuning campaign, item A(a)).
**Decision: NO SHIP** — CI straddles 0 (lower bound < 0), and the 40+0.4 bucket
collapses again. The two qsearch-TT changes do not recompose via this lever.

## Change

`src/search.rs` Path A (stand-pat fail-high): make the `Lower` store
value-conditional. Decisive fail-high (`sp - beta >= QS_PATHA_KEEP_MARGIN=100`)
stores `Lower` as before; marginal fail-high (`< 100`) suppresses the store and
returns `sp` (the M5.F.3 TT-pressure rationale, restricted to the low-value
high-volume entries — keeping the decisive entries whose loss under
*unconditional* M5.F.3 was the leading suspect for the slow-TC collapse against
M5.F.1's completed-loop `Exact`).

≤15 LOC + 3 unit tests (decisive/marginal/boundary). Blind final-review: no
further substantive issues. Bench d4 `112_020 → 111_993` (deterministic; at
shallow bench depths almost all Path-A fail-highs are marginal, so the gate
suppresses most of them — ≈ unconditional M5.F.3 at d4). Patch:
`bench/sprt/patches/aa-margin-gate.patch`.

## Methodology

Baseline `M5.F.1` tag. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS (concurrency 6), 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DE0A0`. Out dir: `target/matches/sprt/20260602T031919-M5.F.1-sprt/`.

## Result

```
sprt: verdict=continue llr=-0.76 pairs=200 ptnml=[16,52,74,41,17]
ci:   elo=-7.82 [-33.43, +17.71] pairs=200
```

**Δ Elo −7.82 [−33.43, +17.71]** — CI lower < 0 ⇒ NO SHIP (fails rung-1-by-CI).
Aggregate W-L-D = 106-115-179 ≈ 49.1%.

### Per-TC — the 40+0.4 collapse reproduces

| TC | W | L | D | N | score |
|---|---|---|---|---|---|
| 10+0.1 | 32 | 18 | 34 | 84 | **58.3%** |
| 20+0.2 | 29 | 35 | 54 | 118 | 47.5% |
| 40+0.4 | 18 | 37 | 29 | 84 | **39.9%** |
| 60+0.6 | 27 | 25 | 62 | 114 | 50.9% |

Same bimodal shape as the unconditional M6.J+C1+C3 combine (strong at 10+0.1,
collapse at 40+0.4). The margin gate retains the fast-TC lean but does **not**
fix the mid-TC interaction — so the decisive-entry-loss hypothesis is not the
(sole) mechanism; the 40+0.4 regression is robust to keeping the high-value
Path-A entries.

## Disposition

- **NO SHIP; revert.** Production stays `M5.F.1`.
- **A(a) closes the "recompose" option (b) NEGATIVE for the margin lever.** The
  margin gate is one of several conceivable gates; the persistence of the 40+0.4
  collapse across (i) unconditional suppression and (ii) margin-gated
  suppression strengthens the **substitutes, not complements** reading of
  M5.F.1 vs M5.F.3. A qsearch-local-depth gate remains untried but is lower-prior
  now. The backlog's option (a) (ship M5.F.3 *instead of* M5.F.1) is unaffected,
  but trades down per ELOH.D (M5.F.1 is depth-amplifying).
- Patch preserved: `bench/sprt/patches/aa-margin-gate.patch`. Not merged.
