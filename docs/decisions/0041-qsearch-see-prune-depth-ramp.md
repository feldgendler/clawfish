# ADR-0041 — Depth-conditioned qsearch SEE-prune threshold (ramp)

**Status:** Accepted — **SHIPPED 2026-06-17** as the new production search HEAD,
superseding ADR-0040's flat-0 threshold (eval unchanged = M6.J). **2-seed
H1-ACCEPT** vs `M5.K`: `…D00B` **+92.53 [+67.62, +118.41]** (llr 3.03, 392g),
`…D01B` **+123.93 [+94.75, +154.90]** (llr 3.19, 260g) — both reached the H1
boundary before the cap; rung-1. Dominates both M7.B (flat-0) and the M7.B.1 flat
−50 at **every** TC, 2-seed-consistent: 60+0.6 recovered (−104 → +56), 20+0.2 held
(+140, no flat-−50 collapse), 40+0.4 no d13 band-edge dip (+125). The M7.B.1
slow-TC-regression deliverable. Byte-identical to M7.B at bench (d4 `45788` / d7
`662085` — ramp inert ≤ D0). Full record: `bench/sprt/2026-06-17-m7b2-ramp-vs-m5k.md`,
`docs/plans/m7.b.2.md`.

## Context

M7.B (ADR-0040) shipped a **flat** SEE-prune threshold of `0` (skip every
non-promotion `see < 0` capture in qsearch when not in check). SPRT vs `M5.K`:
aggregate **+40.13**, but an **inverse depth profile** — strongly positive at
fast/mid TC (10+0.1 ≈+109, 20+0.2 ≈+73, 40+0.4 ≈+109) and **≈−104 at 60+0.6**,
the slowest and most strength-relevant TC.

M7.B.1 swept flat negative thresholds. `−50` was 2-seed-confirmed to **recover
60+0.6** (−104 → +28/+47) but to **collapse 20+0.2** (+73 → −66/−57) — both
effects real, not noise. Conclusion: **a flat threshold can only trade one TC
bucket for another**; aggregate nets ~+28 (statistically tied with thr-0's +40)
but cannot win both ends.

## Decision

Make the threshold a **smooth ramp on the current ID *root* iteration depth**:

```
qs_see_prune_threshold(d) = clamp(-SLOPE * max(0, d - D0), FLOOR, 0)
   D0 = 12,  SLOPE = 16,  FLOOR = -64
   ⇒  d≤12: 0 | 13: -16 | 14: -32 | 15: -48 | 16+: -64
```

`AlphaBetaMover.root_depth` is published at the top of each ID iteration and read
once per qsearch frame to compute the threshold.

### Why root depth is the right conditioning variable

The engine experiences time control only *through the ID depth it reaches* (fast
TC ⇒ ~d9; slow TC ⇒ ~d15). The prune's harm is a function of depth, not the
clock: a `see < 0` capture can be a real tactical sacrifice the engine *would*
refute given depth. qsearch runs at the leaves of **every** ID iteration, so:
- **Fast TC** runs only shallow iterations (d≤9) → threshold stays 0 → full M7.B
  fast-TC node-saving benefit retained.
- **Slow TC** runs iterations 1…15; the deep ones — which burn most of the clock
  and produce the played move — prune conservatively → slow-TC accuracy restored.

### Why a smooth ramp, not a band (the load-bearing design choice)

The M5.K aspiration depth-gate `[8,12]` band scored **−34.9 Elo**, the regression
landing in the very 20+0.2 bucket it meant to protect — attributed to a
**band-edge discontinuity** at the mid-TC median depth (~11.5 ≈ band-max 12). The
ramp has no cliff (consecutive depths differ by `SLOPE` cp), and `D0=12` sits
*above* the 20+0.2 median, so that bucket stays in the flat-0 region M7.B already
won. This is the explicit, structural defense against repeating that failure.

### Defaults targeted from the M7.B/M7.B.1 per-TC data

Measured median ID depths: 20+0.2 ≈ 11.5, 60+0.6 ≈ 15 (~3.5-ply gap the ramp
exploits). 20+0.2 wants aggressive (thr-0 +73, thr-50 −66) → ramp keeps it ~0 at
d≤12; 60+0.6 wants conservative (thr-0 −104, thr-50 +28) → ramp gives ≈−48 at
d15. The hand-picked consts are the SPRT candidate; UCI tunability is deferred
(per the M5.K lesson, SPSA is low-signal for this knob class — a manual sweep is
the tool if a retune is wanted).

## Structural properties

- `qs_see_prune_threshold(d) == 0` for all `d ≤ D0` ⇒ **byte-identical to M7.B
  (flat-0) at every iteration ≤ D0**; the ramp can only change behavior at d≥13.
  ⇒ standard bench (d4/d7, ≤ d7) is inert; the ramp is **not** a bench-visible
  change, so a dedicated depth-driven test (`qsearch_see_ramp_*`) replaces bench
  as the canary.
- `root_depth` defaults to `0` (constructor + reset in `qsearch_for_test` /
  `negamax_for_test`) ⇒ any non-ID-loop caller (eval `stm_score` helper, the
  for-test entries) uses threshold 0 ≡ M7.B. The default-0 guarantee is
  code-backed, not invariant-backed.
- Fast-out validity (`victim ≥ attacker ⇒ see ≥ 0 ⇒ not pruneable`) requires
  `threshold ≤ 0`. The ramp is `≤ 0` by construction; pinned by a **compile-time**
  `assert!(FLOOR ≤ 0 && SLOPE ≥ 0)` at the const site plus a consumer-side
  `debug_assert`.

## Consequences / risks

- **M5.K precedent** is the primary risk; mitigated by smooth ramp + D0 above the
  20+0.2 median + the superset-of-M7.B property bounding downside at TCs M7.B won.
- **Median-depth overlap:** if 20+0.2 and 60+0.6 ID depths overlap more than the
  ~3.5-ply estimate, the ramp reduces to a flat intermediate threshold (the SPRT
  per-TC, esp. 40+0.4 at the d13 onset, will show this; fallback: raise D0 /
  steepen SLOPE).
- **Sparse marginal band:** only captures with `see ∈ (FLOOR, 0)` change
  disposition; with the coarse `SEE_VALUE` set this is a thin band per-position,
  but it is empirically Elo-relevant (the flat thr-0↔thr-50 delta *is* that band).
- The "engine refutes given depth" mechanism is a hypothesis; the SPRT is the
  arbiter.

## Mutation note

The `< threshold` vs `<= threshold` boundary in `qsearch_see_pruneable` is an
**equivalent mutant** at every reachable threshold {0, −16, −32, −48, −64}: no
`attacker > victim` exchange in the integer `SEE_VALUE` set nets exactly any of
them (achievable losing values −28, −112, −140, −255, … are none of the SLOPE
multiples). Documented in `.cargo/mutants.toml` + the §5.3 search.rs comment.

## Open SPRT-tunable parameters

`QS_SEE_RAMP_D0` (12), `QS_SEE_RAMP_SLOPE` (16), `QS_SEE_RAMP_FLOOR` (−64). Sweep
candidates if the first SPRT is promising-but-imperfect: D0 ∈ {10, 14}, FLOOR ∈
{−48, −80}, SLOPE ∈ {12, 24}.
