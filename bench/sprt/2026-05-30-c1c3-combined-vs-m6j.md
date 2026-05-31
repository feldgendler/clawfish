# C1+C3 combined (M5.F.3 + M5.F.1) — combined-confirmation SPRT vs M6.J

**Date:** 2026-05-30 (overnight tuning campaign).
**Decision: DO NOT SHIP THE COMBINATION — destructive interaction.** Ship C1 alone.

## Why this run

C1 (suppress Path-A qsearch store) and C3 (Exact at completed-loop qsearch) each
shipped individually vs M6.J at **+37.49 Elo** (CI-lower>0). Both touch qsearch
TT contents, so before committing both they were combined (M6.J + C1 + C3, d4
bench `111_992`) and re-confirmed jointly vs M6.J. The complementary individual
per-TC profiles (C1 strong fast/mid, C3 strong slow) suggested additive
composition. **They do not compose.**

## Methodology

Baseline `M6.J`. Mixed-TC, `--virtual-clock`, full QoS, 400-game cap,
elo0=0/elo1=10, seed `0xC1ABF15AE10DDF09`.
Out dir: `target/matches/sprt/20260530T081854-M6.J-sprt/`.

## Result

```
sprt: verdict=continue llr=0.25 pairs=200 ptnml=[14,54,61,49,22]
ci:   elo=+9.56 [-17.18, +36.41] pairs=200
```

**Δ Elo +9.56 [−17.18, +36.41]** — CI lower < 0 ⇒ does NOT confirm. Aggregate
W-L-D = 108-97-195 ≈ 51.4% (vs ~55.5% for each component alone).

### Per-TC — the destructive interaction is concentrated and severe

| TC | W | L | D | N | score | C1-alone | C3-alone |
|---|---|---|---|---|---|---|---|
| 10+0.1 | 72 | 14 | 42 | 128 | **72.7%** | 65.2% | 54.5% |
| 20+0.2 | 16 | 21 | 47 | 84 | 47.0% | 44.2% | 50.0% |
| 40+0.4 | 3 | 35 | 48 | 86 | **31.4%** | 60.9% | 50.4% |
| 60+0.6 | 17 | 27 | 58 | 102 | 45.1% | 51.6% | 66.3% |

The 40+0.4 bucket collapses to **31.4% (W=3 L=35)** under the combination, even
though *both* components were positive there individually — a strong, non-noise
signal (86 games). Meanwhile 10+0.1 is amplified to 72.7%. The combination is a
hugely amplified bimodal: the two qsearch-TT changes interact such that the
net effect is depth/TC-dependent and, at mid-TC, destructive. (Mechanism
hypothesis: both reduce qsearch nodes and reshape TT contents; combined, the
Exact re-probe cutoffs (C3) over positions whose Path-A entries are now absent
(C1) appear to prune differently at the depths reached around 40+0.4, but this
is unconfirmed — would need a node/depth bisection.)

## Disposition

- **Ship C1 alone** (M5.F.3) — independently validated +37.49 [+12.46, +62.92],
  safe in isolation. New production base.
- **Defer C3** (M5.F.1) — independently validated +37.49 [+15.71, +59.58] but
  does NOT compose with C1. Re-queued in `docs/tuning-backlog.md` with this
  interaction data. Options for a future session: (a) ship C3 *instead of* C1
  (coin-flip on standalone merit; C3 has the cleaner per-TC, no negative
  bucket); (b) bisect the interaction (which TC/depth, which code path) and
  re-tune so they compose; (c) re-run the combined with a fresh seed to rule out
  a seed artifact in the 40+0.4 cluster (W=3 L=35 is extreme but worth a
  confirm). Patches preserved: `bench/sprt/patches/c3-m5f1-qsearch-exact.patch`,
  `bench/sprt/patches/c1c3-combined.patch`.
- **Lesson:** two independently-SPRT-validated changes to the same subsystem
  (qsearch TT) cannot be assumed additive — the combined-confirmation SPRT
  earned its keep here. Always combined-confirm same-subsystem multi-ships.

## Seed-confirm (40+0.4 only, fresh seed `0x…F0B`, 200 games)

To test whether the catastrophic 40+0.4 bucket (W=3 L=35 = 31.4%) was a seed
artifact (H0), the combined was re-run at **40+0.4 only** with a fresh seed.
Out dir: `target/matches/sprt/20260531T000632-M6.J-sprt/`.

```
sprt: verdict=continue llr=-2.51 pairs=100 ptnml=[6,42,34,15,3]
ci:   elo=-57.86 [-90.15, -26.53] pairs=100
```

**Δ Elo −57.86 [−90.15, −26.53]** at 40+0.4 — CI entirely below zero.
(Mid-run this read ~46% at 91 games, a high-variance midpoint; it converged
down to ~41% / −58 Elo by 200.)

**Conclusion — H0 is dead, the interaction is REAL.** Both seeds show a strong
negative at 40+0.4 (F09 ≈ −130 Elo; F0B = −58 Elo). The original's *magnitude*
was seed-amplified, but the *sign and significance* are robust: C1+C3 are
significantly worse than M6.J at mid-TC. The two changes do not compose.
**Ship a single change.** (H1 — accuracy-vs-speed crossover — remains the
leading mechanistic explanation; a fixed-depth match could confirm it but is no
longer decision-relevant: the combination is dead either way.)
