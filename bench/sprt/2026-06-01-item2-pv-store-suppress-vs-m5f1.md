# Item 2 (PV-node loose-store suppression) — SPRT vs M5.F.1

**Date:** 2026-05-31→06-01 (M5.F qsearch items-2/4–8 tuning campaign).
**Decision: REVERT** (CI entirely below 0).

## Change

`src/search.rs`: thread `is_pv` into qsearch (propagated to all qsearch children);
in `qsearch_store_and_return`, suppress a Lower/Upper store when `is_pv` but KEEP
`Exact` stores (the refined adaptation that avoids cannibalising M5.F.1's
completed-loop Exact lever on PV nodes). Reconsiders ADR-0028 §6. Bench: d4
112020→112306, d7→1354942 (small; PV qsearch nodes are a minor fraction).
Review-approved (blind), full-lib `cargo mutants --in-diff` clean on the guard.

## Methodology

Baseline `M5.F.1` tag. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS, 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DE002`. Run survived a laptop suspend mid-flight and resumed to
completion (harness is suspend-tolerant: virtual clock + suspend-excluding
`Instant` accounting — no spurious forfeit).

## Result

```
sprt: verdict=continue llr=-2.38 elo0=0.0 elo1=10.0 pairs=200 ptnml=[16,66,64,46,8]
ci:   elo=-31.35 [-55.91, -7.10] pairs=200
```

**Δ Elo −31.35 [−55.91, −7.10]** (pentanomial 95% CI — entirely below 0).
Aggregate ≈ 44% (W-L-D ~90-128-182).

## Disposition

**Revert.** CI lower bound −55.91 < 0 (and the *upper* bound −7.10 is also < 0,
so the regression is unambiguous, not marginal). The code is sound
(review-approved, mutation-clean) — it simply does not help: suppressing the
loose (Lower/Upper) qsearch stores on PV-leaf subtrees removes TT entries that
were net-useful, even though M5.F.1's `Exact` stores were preserved. ADR-0028 §6
(no `is_pv` store-gating in qsearch) stands. Production stays `M5.F.1`. Branch
`tune/m5f-item2-pvstore` retained for record, not merged.
