# SPRT — item 5: delta-baseline (volatility-responsive) aspiration width vs `M5.F.1`

**Campaign:** overnight 2026-06-03 (`docs/plans/tuning-overnight-2026-06-03.md`), item 5.
**Baseline:** `M5.F.1` (current production HEAD).
**Candidate:** M5.F.1 + delta-baseline first-try aspiration half-width —
`half = clamp(ASPIRATION_DELTA_K·|score(d-1) − score(d-2)|, MIN, MAX)` with
`K=2, MIN=25, MAX=250`, fallback to fixed `ASPIRATION_HALF_WIDTH=50` when
score(d-2) unavailable. Tracks score(d-2) in the ID loop (`prev_prev_score`,
shifted only on iteration completion). ML-aspiration tier 3 (the cheap
delta-baseline); a *different mechanism* from tier 2's monotone narrowing
(2026-06-02 B, no-ship): this *widens* on volatility, *narrows* on stability.
**Bench:** d4 `112020` (unchanged — aspiration fires at depth ≥6) / d7 `1326598`
(was `1354640`; net node savings from tighter windows on stable nodes).
**Review:** blind final-review **approve** (d-2 tracking verified correct on
ordering / abort-gating / off-by-one; no overflow; no degenerate window).

## Seed 1 — `0xC1ABF15AE10F0005` (run ~05:04–06:57 CEST)

```
sprt: verdict=continue llr=1.22 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[5,44,86,53,12]
ci:   elo=+20.00 [-1.71, +41.87] pairs=200
```

**Δ Elo +20.00 [−1.71, +41.87]** — llr=1.22 (drifting toward H1, not crossed).
Per-TC (candidate W-L-D):

| TC | W | L | D | net | reading |
|---|---|---|---|---|---|
| 10+0.1 | 19 | 23 | 46 | −4 | flat-neg (too shallow for a stable d-1/d-2 delta) |
| **20+0.2** | **40** | **17** | **59** | **+23** | **strongly positive** — depth ~11-12, volatility signal most useful |
| **40+0.4** | **27** | **16** | **39** | **+11** | positive |
| 60+0.6 | 21 | 28 | 65 | −7 | flat-neg (deep enough that fixed-50 already fine) |

**The mechanism story is coherent:** the delta-baseline helps exactly at the
mid-TCs where clawfish's depth-reach makes the |Δscore| volatility signal
informative, and is inert-to-slightly-negative at the extremes. Far more
believable than item 3's single-bucket spike.

## Decision — borderline; SEED-CONFIRM launched (not shipped unilaterally)

- **Campaign strict rule (CI-lower > 0):** fails (−1.71) ⇒ no ship *by that rule*.
- **ADR-0037 §9 verdict ladder:** mean +20 ≥ 5 ∧ CI-lower −1.71 > −10 ⇒
  **rung-2 "ship with note."** The two rules disagree on this borderline.
- This is the textbook case for the project's **combined-confirm discipline**
  (CLAUDE.md: "always combined-confirm"). A second independent seed resolves it:
  if seed 2 also leans ~+15-20, the combined ~800-game CI-lower clears 0
  (unambiguous ship); if seed 2 is flat/negative, the +20 was seed-luck (discard).
- **Seed-confirm SPRT launched:** seed `0xC1ABF15AE10F0015`, same config, vs
  `M5.F.1`. Result + combined verdict appended below.
- **Not shipped to production unilaterally** on a single-seed borderline; the
  ship/discard decision is deferred to the combined evidence (and surfaced to
  the user as the night's headline open item).

## Seed 2 — `0xC1ABF15AE10F0015` (run ~06:58–08:53 CEST)

```
sprt: verdict=continue llr=0.06 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=200 ptnml=[15,47,72,48,18]
ci:   elo=+6.08 [-19.58, +31.80] pairs=200
```
Per-TC: 10+0.1 W19-L30-D35 (−11); **20+0.2 W33-L13-D48 (+20)**; 40+0.4 W34-L37-D51 (−3); 60+0.6 W21-L20-D59 (+1).

Weaker than seed 1 (+6.08 vs +20.00) — the +20 was partly seed-luck. **But the
20+0.2 win replicates** (seed 1 +23, seed 2 +20): the mid-TC mechanism is robust.

## Combined verdict (pooled 400 pairs / 800 games)

Pooled ptnml = `[20, 91, 158, 101, 30]`.
**Δ Elo +13.03 [−3.78, +29.91]** (pentanomial, normal approx).

- **Strict campaign rule (CI-lower > 0):** fails (−3.78) ⇒ **NO SHIP. Reverted.**
- **ADR-0037 §9 ladder:** mean +13.03 ≥ 5 ∧ CI-lower −3.78 > −10 ⇒ **rung-2
  "ship with note"** would apply. The two rules still disagree even after the
  combine; the disciplined default under the committed campaign rule is revert,
  and reverting is the reversible choice (the user can opt into the rung-2 ship
  by re-applying the patch).

## The actionable signal — 20+0.2 is robustly positive

Both seeds show a large, consistent **20+0.2** win (+23 / +20) while 10+0.1 is
negative both times and the slow buckets are flat. The delta-baseline helps
exactly where clawfish's depth-reach (~11-12 at 20+0.2) makes the |Δscore|
volatility signal informative, and is inert-to-harmful at the extremes. **Top
follow-up:** a **TC- or depth-gated** delta-baseline (apply the volatility width
only in the mid-depth band; keep fixed-50 at shallow/deep) to capture the
20+0.2 win without the extreme-bucket drag — plus an **SPSA tune of K/MIN/MAX**
(hand-picked here: K=2, MIN=25, MAX=250). This is the closest the search layer
has come to a ship across three campaigns; it earns a dedicated micro-campaign.

Patch: `bench/sprt/patches/item5-delta-baseline-aspiration.patch`.
