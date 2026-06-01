# Item 6 (TT-move ordering filter relaxation) — SPRT vs M5.F.1

**Date:** 2026-05-31 (M5.F qsearch items-2/4–8 tuning campaign).
**Decision: REVERT** (CI lower bound < 0; not H1; verdict=continue at cap).

## Change

`src/search.rs` qsearch step-7 ordering: relax ADR-0028 §7 so a legal but
filtered-out (quiet / under-promo) TT move at a `!in_check` node is searched
(prepended for ordering) rather than dropped. Legality = membership in the
already-generated full legal list `ml`. Placed past the step-6 terminal returns
(so `moves_vec` is non-empty), which structurally prevents a pure-quiet long
chain. Bench: d4 112020 (unchanged), d7 1354640→1334940 (fewer nodes on the
bench corpus — better ordering there).

## Methodology

Baseline `M5.F.1` tag (current production HEAD; `1bef931` is search-identical).
Mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`, `--virtual-clock`, full QoS,
400-game cap, elo0=0/elo1=10, seed `0xC1ABF15AE10DE016`. (An initial run, seed
`…DE006`, was killed at ~89 games for a laptop suspend; this is the clean re-run.)

## Result

```
sprt: verdict=continue llr=-1.65 elo0=0.0 elo1=10.0 pairs=200 ptnml=[23,54,62,50,11]
ci:   elo=-24.36 [-50.84, +1.84] pairs=200
```

**Δ Elo −24.36 [−50.84, +1.84]** (pentanomial 95% CI). Aggregate W-L-D ≈
102-130-158 (~46.4%).

## Disposition

**Revert.** CI lower bound −50.84 < 0 (and verdict not H1) → fails the
ship rule. Searching a quiet TT move inside qsearch dilutes qsearch's
capture/tactical focus: qsearch is meant to resolve forcing sequences to a quiet
leaf, and injecting a non-forcing move yields a misleading horizon value (the
side gets a "free" quiet improvement the opponent can answer with their own
quiet that qsearch then won't continue). The bench's fewer-nodes signal (better
ordering on those fixtures) did **not** translate to game strength. ADR-0028 §7
(filter-gated TT-move ordering, Andrew Grant long-chain protection) stands —
the relaxation is rejected. Production stays `M5.F.1`. Branch
`tune/m5f-item6-ttmove` retained for record, not merged.
