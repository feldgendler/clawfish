# Unconstrained-refit CONTROL vs `M5.K` — attribution for the sign/mono regression

**Date:** 2026-06-07. **Purpose:** isolate whether the constrained retune's −64.13 Elo
(`bench/sprt/2026-06-07-signmono-constrained-eval-vs-m5k.md`) came from the **sign/mono
clamps** or from **generic refit-drift**. Method: rerun the *identical* recipe **minus**
`--sign-project --mono-lambda`, SPRT vs the same baseline with the **same paired seed**
so the two candidate runs are directly comparable.

**Baseline:** `M5.K` (production: M5.K search + M6.J eval). **Candidate (U):** M5.K search +
*unconstrained* full Texel refit (warm-start from shipped M6.J, 10000 iters).
**Seed:** `0xC1ABF15AE10F0007` (same as the constrained run). Mixed-TC + virtual-clock,
elo1=5, elo0=0, alpha=beta=0.05, 400 games.

## Verdict — refit-drift confirmed

| Candidate | val_loss (vs shipped 0.142766) | Δ Elo vs production |
|---|---|---|
| **C** — constrained (`--sign-project --mono-lambda 5e-3`) | 0.142272 (−0.35%) | **−64.13 [−92.00, −37.04]** |
| **U** — unconstrained (control) | 0.142186 (−0.41%) | **−68.63 [−94.92, −43.09]** |

The two are **statistically indistinguishable** (CIs overlap almost entirely; U is if
anything slightly *worse* despite the tighter val-loss). **The regression is refit-drift,
not the sign constraints.** A fresh full refit on the rebuilt corpus cache lands at a
worse-*playing* optimum than the carefully-tuned M6.J **regardless of whether the signs are
constrained** — and improving held-out val-loss (both C and U beat shipped) does not rescue
it. The sign/mono clamps are exonerated as the cause.

```
U: sprt: verdict=continue llr=-2.18 elo0=0.0 elo1=5.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[30,63,70,29,8]
U: ci:   elo=-68.63 [-94.92, -43.09] pairs=200
```

Aggregate U: W=85 L=163 D=151 over 400 games (40.1%).

## Per-TC (U)

| TC | W | L | D | score |
|---|---|---|---|---|
| 10+0.1 | 24 | 37 | 41 | ~43.6% |
| 20+0.2 | 13 | 39 | 38 | ~35.6% |
| 40+0.4 | 37 | 35 | 30 | ~51.0% |
| 60+0.6 | 11 | 52 | 43 | **~30.7%** |

The per-TC *distribution* differs from C (C cratered at 40+0.4 / 60+0.6 and was flat at the
fast TCs; U is weak at 10+0.1 / 20+0.2, ~even at 40+0.4, and worst at 60+0.6) — a genuine
eval-behavior difference between the two refits, visible because the paired seed fixes the
opening/TC streams. But the **60+0.6 collapse is common to both** (C ~27%, U ~31%),
consistent with the **shared inflated `PASSED_EG`** that both refits produced — the
deep-TC endgame term is where a casual refit does the most damage.

## The corpus genuinely wants the "wrong" signs

U, free of constraints, **reproduced and amplified** the very shapes M6.I flagged as
counterintuitive:

- `ISO_MG = +7` — isolated-pawn MG *bonus*, larger than shipped's +4 (and far from the
  "expected" penalty ≤ 0).
- `PASSED_MG = [0,0,-41,-32,1,39,43,0]` — early-rank passers *more* negative than shipped
  (`-28,-14`).
- `PASSED_EG = [0,0,44,83,108,94,64,0]` — inflated, like C's.

So the unconstrained optimum on this corpus *is* the prior-violating shape. Forcing the
"principled" signs (C) cost essentially nothing relative to the refit-drift baseline (U) —
the sign question is moot: **the data wants these signs, and they are not what regresses
play.** What regresses play is re-deriving the whole vector from the corpus at all.

## Conclusion

- **Attribution: refit-drift.** Both refits (constrained −64, unconstrained −69) regress
  by the same ~−65 Elo vs M6.J. The sign/mono clamps are not the culprit.
- **Reaffirms the M6.J cold/warm-start lesson, hard:** a corpus refit that *improves*
  held-out val-loss can *lose* ~65 Elo in play. M6.J's specific tuned vector is not
  reproducible by a naive warm-start refit on the (re-materialized) corpus — val-loss is
  not a usable ship signal here.
- **Sign-shape question: closed as moot.** The corpus wants the counterintuitive shapes;
  constraining them is neither necessary (data prefers the violations) nor the source of
  the regression.
- **Production HEAD unchanged: M6.J eval / M5.K search, bench d4 `112020` / d7 `1326598`.**

## Artifacts

- U vector: `bench/tune/2026-06-07-unconstrained-refit.json` (retained); U bench d4
  `110290` / d7 `1326255`.
- C vector: `bench/tune/2026-06-07-signmono-constrained.json` (retained).
- Match output: newest `target/matches/sprt/*-M5.K-sprt/`.
- Cache `bench/tune-cache.bin` deleted (2.4 GB, rebuildable in minutes).
