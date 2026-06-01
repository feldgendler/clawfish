# Item 4 (TT replacement: qsearch must not evict negamax cross-generation) — SPRT vs M5.F.1

**Date:** 2026-06-01 (M5.F qsearch items-2/4–8 tuning campaign).
**Decision: NO SHIP (flat)** — CI straddles 0.

## Change

`src/tt.rs` `store()`: a depth-0 (qsearch) entry may not evict a depth≥1
(negamax) entry even across generations (the `old.age() != current_gen` arm
previously allowed it; within-generation was already protected by
`old.depth <= data.depth`). A negamax (full-width) result outranks a
restricted-move qsearch result. Review-approved (blind), 12/12 `cargo mutants
--in-diff` on the diff, **bench-inert** (d4/d7 unchanged: 112020 / 1354640 — the
cross-generation scenario does not arise in single-search bench positions; it
only bites in multi-generation game play with a persistent TT).

## Methodology

Baseline `M5.F.1` tag. Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`,
`--virtual-clock`, full QoS, 400-game cap, elo0=0/elo1=10, seed
`0xC1ABF15AE10DE004`. Run from the item-4 worktree (baseline pre-cached).

## Result

```
sprt: verdict=continue llr=0.08 elo0=0.0 elo1=10.0 pairs=200 ptnml=[10,46,79,57,8]
ci:   elo=+6.08 [-16.44, +28.65] pairs=200
```

**Δ Elo +6.08 [−16.44, +28.65]** (pentanomial 95% CI — straddles 0).
Aggregate ≈ 51.5% (W-L-D ~95-89-216).

## Disposition

**No ship (flat).** Point estimate is mildly positive (+6 Elo) and the per-TC
trend ran faintly positive throughout, but the CI lower bound (−16.44) is < 0, so
it fails the rung-1-by-CI ship rule (ship only if CI-lower > 0). The change is
sound and harmless (it preserves authoritative deep negamax entries slightly
longer), just not a measurable gain at this strength/TC. Production stays
`M5.F.1`. Branch `tune/m5f-item4-ttrepl` retained for record, not merged.

**Re-visit note:** the faint-but-consistent positive lean (and the zero
downside — it's a strictly-more-conservative replacement policy) makes this a
reasonable *free rider* to fold into a future TT-replacement revisit (e.g. a
bucketed-TT or aging-scheme change), where it could be co-validated rather than
needing its own +10-Elo signal to clear the gate solo.
