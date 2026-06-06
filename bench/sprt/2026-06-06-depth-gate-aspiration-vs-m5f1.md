# SPRT — depth-gated adaptive aspiration ([8,12] band) vs `M5.F.1`

**CLOSED NEGATIVE.** Δ Elo **≈ −34.9 over 800 games (2 seeds)**, both single-seed
CIs fully below zero. The depth-gate lever (lever 1 of the delta-baseline
aspiration item) is **refuted**; production HEAD stays `M5.F.1` (the candidate
code at `89e4dad` is a default-OFF runtime feature, bench byte-identical — kept,
not reverted; only the `[8,12]` band is refuted).

**Campaign:** depth-gated adaptive aspiration (`docs/plans/aspiration-depth-gate.md`).
**Baseline:** `M5.F.1` (production HEAD) = `89e4dad` with adaptive OFF (default).
**Candidate:** `89e4dad` + `Aspiration_Adaptive=true`, `Aspiration_AdaptiveMinDepth=8`,
`Aspiration_AdaptiveMaxDepth=12` (K/MIN/MAX at the SPRT'd default 200/25/250).
**Method:** SAME-BINARY self-SPRT (one binary both sides; isolates the setoption
effect with zero toolchain confound). Mixed-TC + virtual-clock,
`10+0.1:20+0.2:40+0.4:60+0.6` equal-weight, elo0=0 elo1=10 α=β=0.05, 400-game
cap each, concurrency 4. Runner: `scripts/sprt-depth-gate.sh`. Bench d7
`1354640` byte-identical (adaptive defaults OFF). Blind final review: APPROVE;
mutants 19/19 on the band gate.

## Seed 1 — `0xC1ABF15AE10F0025` (07:45–10:45 local)

```
sprt: verdict=continue llr=-2.63 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[21,55,77,38,9]
ci:   elo=-35.74 [-60.57, -11.27] pairs=200
```
Per-TC (candidate W-L-D): 10+0.1 19-15-32 (**+4**); **20+0.2 20-54-50 (−34)**;
40+0.4 19-18-65 (**+1**); 60+0.6 25-37-46 (**−12**).

## Seed 2 — `0xC1ABF15AE10F0035` (07:45–10:30 local)

```
sprt: verdict=continue llr=-2.35 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[19,66,60,45,10]
ci:   elo=-33.98 [-59.67, -8.66] pairs=200
```
Per-TC (candidate W-L-D): 10+0.1 34-29-41 (**+5**); 20+0.2 16-32-52 (**−16**);
40+0.4 13-28-55 (**−15**); 60+0.6 22-35-43 (**−13**).

## Combined (800 games)

- Combined ptnml `[40, 121, 137, 83, 19]` (400 pairs), mean pair score **0.900**
  (< 1.0 ⇒ candidate trails). Mean Δ Elo (seed average) **−34.86**.
- Both single-seed CIs exclude zero (−11.27 and −8.66 upper bounds) ⇒ the
  combined CI is unambiguously negative. No combined-confirm ambiguity.

## Reading — the band hypothesis is wrong

Lever 1's premise: confining the adaptive width to a mid-depth band would keep
the ungated candidate's **20+0.2 win (+23/+20, item 5)** while shedding the
extreme-bucket drag (10+0.1 / 60+0.6). The data **refute** this:

- The regression concentrates in **exactly the bucket the band was designed to
  protect** — 20+0.2 went −34 (s1) / −16 (s2), the bucket that *won* ungated.
  The slower buckets (40+0.4, 60+0.6) also turned negative in s2.
- 10+0.1 (where the ungated candidate was flat-negative) is the *only* roughly
  neutral bucket here.

So the adaptive width's benefit is **not** cleanly depth-localizable — gating it
to [8,12] did not "keep the good part," it broke the mechanism. The likely cause
is a discontinuity: at the mid TCs the final ID iterations straddle the band
edge (median depth ~11.5, band max 12), so the first-try window flips between
adaptive and fixed-50 across consecutive iterations, costing re-searches the
ungated (monotone) version avoided. Whatever the exact mechanism, the empirical
verdict is decisive and seed-consistent.

## Disposition

**CLOSE lever 1 — do NOT re-pick band values** (per the M5.I null-/negative-signal
lesson: iterating band edges against a refuted hypothesis is random search). Both
levers of the delta-baseline aspiration item are now exhausted:

- **Lever 2 (SPSA-tune K/MIN/MAX):** ruled out empirically low-signal (2026-06-06
  harness shakedown + calibration).
- **Lever 1 (depth-gate):** refuted here (−35 Elo, both seeds, CI fully negative).

The **ungated** delta-baseline (+13.03 [−3.78, +29.91], rung-2 borderline) remains
the *ceiling* for this mechanism — and gating it backfires — so the only
remaining positive path is the **open user decision** on rung-2 ship-with-note of
the ungated candidate (reproducible from `89e4dad` via `Aspiration_Adaptive=true`
with the default band [6,64] = ungated). The depth-gate infrastructure (`89e4dad`)
is kept as an inert default-OFF runtime feature.

**Cross-refs:** `docs/plans/aspiration-depth-gate.md`; `bench/sprt/2026-06-03-item5-delta-baseline-aspiration-vs-m5f1.md`
(the ungated rung-2 candidate); `docs/tuning-backlog.md` §"Delta-baseline aspiration".
