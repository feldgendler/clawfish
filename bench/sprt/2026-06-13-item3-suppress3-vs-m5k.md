# SPRT — item 3 sweep: `QS_PATHA_SUPPRESS_DEPTH = 3` vs `M5.K`

**Campaign:** side-backlog micro-sweep 2026-06-13.
**Baseline:** `M5.K` (bench d7 `1326598`).
**Candidate:** `M5.K` + item-3 patch with `QS_PATHA_SUPPRESS_DEPTH = 3` (suppress Path-A stand-pat `Lower` stores at `qs_depth >= 3`; keep frames 0–2). Bench d7 `1325261`.
**Harness:** mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` + `--virtual-clock`, conc 6, elo1=10, 400-game cap. Seed `…F1003`.

## Result — single-seed positive, but NOT shipped (see `=1` doc for the seed-split context)

```
sprt: verdict=continue llr=2.53 pairs=200 ptnml=[7,35,81,59,18]
ci:   elo=+40.13 [+16.91, +63.72] pairs=200    (W129-L83-D187 ≈ 55.8%)
```

**Δ Elo +40.13 [+16.91, +63.72]** on seed 1 — looked rung-1-ship-worthy, mirroring `=1`'s seed-1 +41.89. **No confirmation seed was run for `=3`** (wall-clock budget — only the best candidate `=1` was confirmed). Given `=1`'s seed-2 contradiction (−25.23), `=3`'s single-seed +40 is treated as **the same likely-variance false positive** and is **not shipped**. The whole item-3 lever is closed (see `docs/tuning-backlog.md`).

## Per-TC breakdown (candidate W-L-D, seed 1)

| TC | W | L | D | reading |
|---|---|---|---|---|
| 10+0.1 | 36 | 26 | 44 | + |
| 20+0.2 | 32 | 19 | 61 | + |
| 40+0.4 | 33 | 8 | 41 | **strong +** |
| 60+0.6 | 28 | 30 | 42 | ≈ flat/− |

Note 60+0.6 is the weakest bucket here too (consistent with `=1`: the lever does its worst at slow TC).

## Reading

`=3` (keep frames 0–2, suppress deeper) lands at essentially the same seed-1 magnitude as `=1` (suppress all but root) — evidence the exact threshold is second-order and the apparent gain is a property of *enabling* Path-A deep-suppression at all, not the cutoff. But since that apparent gain did not survive a confirmation seed on `=1`, the second-order threshold distinction is moot. NOT shipped.
