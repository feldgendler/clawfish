# SPRT — item 3: qsearch-local-depth gate on Path-A stores vs `M5.F.1`

**Campaign:** overnight 2026-06-03 (`docs/plans/tuning-overnight-2026-06-03.md`), item 3.
**Date:** 2026-06-03 (run ~02:44–04:49 CEST).
**Baseline:** `M5.F.1` (current production HEAD).
**Candidate:** M5.F.1 + qsearch-local-depth gate — suppress Path-A stand-pat
fail-high `Lower` stores on deep qsearch frames (`qs_depth >=
QS_PATHA_SUPPRESS_DEPTH = 2`); keep shallow-frame stores. The last untried
M5.F.3-recompose lever (backlog M5.F item 3, option b). Threads a `qs_depth`
counter through `fn qsearch`.
**Bench:** d4 `112017` / d7 `1353237` (vs baseline `112020` / `1354640` — tiny
shift; gate fires only on rare deep qsearch frames).
**Seed:** `0xC1ABF15AE10F0003`. Mixed-TC + virtual-clock + full QoS, elo1=10,
400-game cap.

## Result — NO SHIP

```
sprt: verdict=continue llr=0.38 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[15,44,68,59,14]
ci:   elo=+11.30 [-13.86, +36.57] pairs=200
```

**Δ Elo +11.30 [−13.86, +36.57]** — CI-lower < 0 ⇒ **no ship** (decision rule:
ship only on SPRT H1 or pentanomial CI-lower > 0 at the 400-game cap). Production
stays `M5.F.1`.

## Per-TC breakdown (candidate W-L-D)

| TC | W | L | D | net | ~in-bucket |
|---|---|---|---|---|---|
| 10+0.1 | 23 | 35 | 44 | −12 | ≈ −40 Elo |
| 20+0.2 | 23 | 20 | 53 | +3 | ≈ +10 Elo |
| **40+0.4** | **48** | **18** | **40** | **+30** | **≈ +50 Elo** |
| 60+0.6 | 21 | 29 | 46 | −8 | ≈ −30 Elo |

## Reading

The depth-gate is a **real lever that rebalances the per-TC profile**, not inert:
- **It fixes the 40+0.4 bucket** (W48-L18-D40, strongly positive) — exactly the
  bucket where *unconditional* M5.F.3 collapsed (−58 Elo, the 2026-05-30
  combined-C1C3 result). Gating-by-local-depth preserves the shallow high-value
  Path-A stores that unconditional suppression wrongly killed, recovering the
  mid-slow bucket.
- **But it regresses the extremes** (10+0.1 and 60+0.6), netting flat.

So the qs_depth gate trades buckets rather than lifting all of them. The
hypothesis ("shallow stores are the valuable ones") is *partially* vindicated
(the 40+0.4 recovery), but threshold 2 doesn't clear the gate. The
**substitutes-not-complements** reading (M5.F.1 and M5.F.3 both reshape qsearch
TT behavior; combined the speed saturates while inaccuracies compound) stands at
the net level.

## Follow-up (deferred, not run tonight)

A `QS_PATHA_SUPPRESS_DEPTH` sweep (try 1, 3, 4) might rebalance the bucket trade —
the threshold clearly moves the per-TC distribution. Lower-prior than a fresh
lever; logged for a future micro-campaign, not pursued in this 3-slot night.
Patch preserved: `bench/sprt/patches/item3-qsearch-local-depth-gate.patch`.
