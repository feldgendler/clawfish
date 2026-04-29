# Rating-estimate cross-validation — M3.F (HEAD) vs Stockfish UCI_Elo=1996

**Date:** 2026-04-29
**Outcome:** 125 W / 66 L / 9 D over 200 games = **64.75% score**. The 1996-Elo hypothesis (derived from the UCI_Elo=1320 anchor) is **falsified** by this cross-check; the true Elo is higher.

## Why this match

Cross-validation of the [primary rating estimate](2026-04-29-rating-estimate.md). If clawfish were really 1996 Elo and Stockfish-at-1996 were really 1996 Elo, the match should be ~50%. Three possible outcomes:

- ~50%: estimate confirmed.
- Clawfish wins: estimate too low (saturation at the 1320 anchor; Stockfish UCI_Elo curve compression).
- Clawfish loses: estimate too high (UCI_Elo's calibration drifts with TC).

## Command

```sh
STOCKFISH_ELO=1996 scripts/sprt.sh rating-estimate
```

## Configuration

Identical to [`2026-04-29-rating-estimate.md`](2026-04-29-rating-estimate.md), except `option.UCI_Elo=1996` instead of 1320. TC, concurrency, adjudication, opening book, all unchanged.

## Result

| Metric | Value |
|---|---|
| Games | 200 |
| Wins (HEAD) | **125** |
| Losses (HEAD) | **66** |
| Draws | **9** |
| Score | **64.75%** |
| Wallclock | 15m46s |

(Result derived by parsing the PGN's `[Result …]` headers against `[White …]` / `[Black …]` to attribute each game to the clawfish side. fastchess's stdout summary line was lost to a `tail -3` truncation in the background command's output capture.)

## Logistic Elo derivation

`w = (W + 0.5×D) / total = (125 + 4.5) / 200 = 0.6475`.

`Elo_diff = -400 × log10((1 - 0.6475) / 0.6475) = -400 × log10(0.5444) = +106`.

**If Stockfish-1996 plays at 1996, clawfish ≈ 2102 Elo** at tc=10+0.1.

This is **inconsistent** with the [primary 1320-anchor estimate of 1996 Elo](2026-04-29-rating-estimate.md). One of them is wrong — and probably both have substantial bias.

## Failure analysis

### Hypothesis 1: Saturation at the 1320 anchor

The primary match scored 98% — near 100%. Logistic Elo at near-saturation has very high variance: tiny changes in score map to big changes in derived Elo. A 99% score implies +800 Elo, 99.5% implies +920 Elo, 99.9% implies +1200 Elo. With only 4 losses anchoring the estimate, ±100 Elo is the *minimum* uncertainty band, and the actual uncertainty is likely larger.

### Hypothesis 2: Stockfish's UCI_Elo is non-linear / TC-biased

Stockfish's strength dial is calibrated at slow TCs (120+1.2 reference, per Stockfish docs). At fast TCs (10+0.1), the dial's effective Elo can drift — typically the strength-limited setting plays *weaker* than its UCI_Elo nominal at fast TC, because the depth/node caps that limit strength fire less often when the engine has less time to begin with.

If Stockfish-at-1996 actually plays at, say, 1900 effective Elo at tc=10+0.1, our +106 derivation would be relative to 1900, giving clawfish ≈ 2006. Closer to the 1996 anchor but still inconsistent in detail.

### Both hypotheses likely contribute

The 1320 anchor produced 98% (saturation). The 1996 anchor produced 65% (well-conditioned but with TC-bias unknown). The cross-check tells us both can't be simultaneously right under linear UCI_Elo assumptions.

## Honest interpretation

**clawfish M3.F end ≈ 2000–2200 Elo at tc=10+0.1**, with substantial uncertainty:

| Anchor | Score | Logistic Elo (assumes linear UCI_Elo) | Reliability |
|---|---|---|---|
| Stockfish UCI_Elo=1320 | 98.0% | ~1996 | Low (saturation) |
| Stockfish UCI_Elo=1996 | 64.75% | ~2102 | Higher (well-conditioned) |

The 2102 estimate is the better point value — it's not saturated — but absolute calibration depends on whether Stockfish-1996 plays at 1996 Elo at this TC. The CCRL 40/4 anchoring of UCI_Elo=1320 is documented; whether the 1996 setting inherits the same anchor cleanly is unclear without a deeper study.

## What this validates and what it doesn't

**Validates:** clawfish is materially stronger than Stockfish UCI_Elo=1320 at tc=10+0.1; *and* materially stronger than Stockfish UCI_Elo=1996 at tc=10+0.1. So clawfish is at least ~2100 Elo at this TC by the 1996 anchor.

**Does not validate:** the precise number. The two anchors disagree, and we don't have a Stockfish-side TC re-calibration to resolve which anchor is reliable.

## What would resolve this

- **Bisection**: pit clawfish against Stockfish at progressively higher UCI_Elo settings (2200, 2400, 2600) until clawfish scores ~50%. The 50%-crossover is clawfish's true Elo at this TC.
- **Slow-TC re-run**: re-run the 1320-anchor match at tc=60+0.6 (closer to Stockfish's calibration reference) to see if the 98% score holds; if it drops, the original derivation was inflated.
- **BayesElo / Ordo regression**: fit a multi-anchor Elo model from several Stockfish-Elo settings.

All three are tooling exercises that don't require additional engine work. Deferred.

## Methodological lesson

The user's suggestion ("match against Stockfish at the hypothesized Elo to confirm") was valuable — it caught the saturation effect that the original 1320-match analysis dismissed as merely "±100 Elo at 200 games." A near-100% score is a much weaker estimate than the formula's CI suggests, and a self-consistency check via a balanced-ish match is the cheapest way to surface that.

Generalizable: **rating estimates from saturated matches need cross-validation against a balanced anchor before being committed to documentation**. Going forward, M4+ rating estimates should target a balanced (~50%) anchor or use multiple Stockfish settings + BayesElo.

## Artifacts

- PGN: `target/matches/sprt/20260429T143034-stockfish-1996-rating-estimate.pgn`
- Log: `target/matches/sprt/20260429T143034-stockfish-1996-rating-estimate.log`
