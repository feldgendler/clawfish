# SPRT — item 3 sweep: `QS_PATHA_SUPPRESS_DEPTH = 1` vs `M5.K` (2 seeds)

**Campaign:** side-backlog micro-sweep 2026-06-13 (the last untried M5.F lever).
**Baseline:** `M5.K` (current production = M5.K search + M6.J eval; bench d7 `1326598`).
**Candidate:** `M5.K` + the item-3 patch (`bench/sprt/patches/item3-qsearch-local-depth-gate.patch`) with `QS_PATHA_SUPPRESS_DEPTH = 1` — suppress the Path-A stand-pat fail-high `Lower` store on **every** qsearch frame except the root (`qs_depth >= 1`); keep only the `qs_depth == 0` store. The most-aggressive point of the threshold sweep.
**Bench:** d7 `1324069` (vs baseline `1326598` — tiny; gate fires only on the rare deep frames it suppresses).
**Harness:** mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` + `--virtual-clock`, conc 6, elo1=10, 400-game cap. Engines reniced to +15 mid-campaign (per user; no QoS clamp) — does not affect outcomes (virtual clock).

## Result — NO SHIP (seed-split)

| Seed | Δ Elo (pentanomial CI) | W–L–D | verdict |
|---|---|---|---|
| `…F1001` (seed 1) | **+41.89 [+16.20, +68.06]** | 149-100-150 | CI-lower > 0 |
| `…F2001` (seed 2) | **−25.23 [−55.47, +4.63]** | 111-141-147 | CI-lower < 0, point estimate negative |

**The two seeds disagree by ~67 Elo with opposite signs.** This fails the 2-seed-confirm rule (ship requires CI-lower > 0 on **both** seeds). The +41.89 on seed 1 was **seed variance**, not a real effect. Production stays `M5.K`.

## Per-TC breakdown (candidate W-L-D)

| TC | seed 1 | seed 2 | reading |
|---|---|---|---|
| 10+0.1 | W55-L8-D33 (**strong +**) | W52-L19-D39 (**+**) | both: helps at fast TC |
| 20+0.2 | W43-L21-D36 (+) | W18-L38-D40 (−) | flips |
| 40+0.4 | W30-L28-D50 (flat) | W36-L25-D39 (+) | flips |
| 60+0.6 | W21-L44-D31 (**−**) | W6-L59-D29 (**catastrophic −**) | both: hurts at slow TC |

## Reading

The lever is **fast-TC-amplifying and slow-TC-regressing** — both seeds agree it *helps* at 10+0.1 and *hurts* at 60+0.6. The net sign is decided by the seed-dependent middle buckets (20+0.2, 40+0.4), which is why one seed reads +42 and the other −25. This is the **same character as M5.F.3** (the unconditional Path-A stand-pat suppression: "fast-TC-amplifying", per the M5.F backlog), which makes sense — the item-3 gate is just a depth-conditional M5.F.3. Against `M5.K` (which carries M5.F.1's *depth*-amplifying qsearch-`Exact`), layering a fast-TC-amplifying Path-A suppression produces a seed-unstable wash.

Per the ELOH.D mandate (value depth/slow-TC strength), a lever that consistently *regresses* 60+0.6 is structurally the wrong direction even if the mixed-TC net occasionally reads positive.

## Lesson

**A single-seed mixed-TC `CI-lower > 0` is NOT sufficient to ship a search micro-lever.** Seed 1 here was a textbook false positive — it cleared the rung-1 bar (+41.89 [+16.20, +68.06]) and would have shipped on a one-seed rule. The 2-seed confirm caught it. This re-affirms the delta-baseline-aspiration precedent (which already mandated ≥2-seed confirm for borderline search changes); extend it to *any* search micro-lever, including ones that look like a clean rung-1 on the first seed.
