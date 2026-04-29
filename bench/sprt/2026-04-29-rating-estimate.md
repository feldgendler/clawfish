# Rating estimate — M3.F (HEAD) vs Stockfish UCI_Elo=1320

**Date:** 2026-04-29
**Outcome:** 196 W / 4 L / 0 D over 200 games = **98.0% score** → **~1996 Elo** at tc=10+0.1.

## Command

```sh
scripts/sprt.sh rating-estimate
```

## Configuration

| Parameter | Value |
|---|---|
| HEAD | M3.F end |
| Baseline | Stockfish 18 (Homebrew) with `option.UCI_LimitStrength=true option.UCI_Elo=1320` |
| TC | `10+0.1` |
| Concurrency | 6 |
| Adjudication | `-resign movecount=3 score=600`; `-draw movenumber=34 movecount=8 score=20`; `-maxmoves 200` |
| Reporting | `-report penta=true` |
| Opening book | None |

## Result

```
Results of clawfish-head vs stockfish-1320 (10+0.1, NULL - 1t, NULL - 16MB):
Elo: 676.08 +/- 341.85, nElo: 1203.55 +/- 48.15
LOS: 100.00 %, DrawRatio: 4.00 %, PairsRatio: inf
Games: 200, Wins: 196, Losses: 4, Draws: 0, Points: 196.0 (98.00 %)
Ptnml(0-2): [0, 0, 4, 0, 96], WL/DD Ratio: inf
```

| Metric | Value |
|---|---|
| Games | 200 |
| Wins (HEAD) | 196 |
| Losses (HEAD) | 4 |
| Draws | 0 |
| Score | 98.0% |
| Pentanomial | `[0, 0, 4, 0, 96]` (4 pairs were 1-1; 96 were 2-0 to HEAD) |
| fastchess Elo | +676.08 ±341.85 |
| Wallclock | 13m6s |

## Logistic Elo derivation

Standard formula per CPW "Match Statistics":

```
Elo_diff = -400 × log10((1 - w) / w)
```

where `w = (W + 0.5×D) / total = 196/200 = 0.98`.

`Elo_diff = -400 × log10(0.02 / 0.98) = -400 × (-1.6902) = +676`.

**Clawfish M3.F end ≈ 1320 + 676 = ~1996 Elo at tc=10+0.1**, anchored to the Stockfish UCI_Elo=1320 reference (CCRL 40/4 calibrated).

## Caveats

- **TC-specific**: this estimate is at `tc=10+0.1`. At slower TCs the gap typically narrows (both engines get more time, but the weaker side benefits relatively more). Re-running at e.g. `tc=60+0.6` would likely produce a lower point estimate.
- **CCRL anchoring**: Stockfish's UCI_Elo is calibrated against the CCRL 40/4 rating list. Clawfish's derived estimate inherits the same anchor; it is not directly comparable to FIDE human ratings.
- **±100 Elo uncertainty**: per research §4, 200 games gives roughly ±70–100 Elo at 95% confidence. The fastchess `Elo: 676.08 +/- 341.85` reports a much wider Elo CI because of the 100% LOS / extreme score: when one side wins ~all games, the Elo's confidence band is intrinsically wide (the few losses anchor a noisy estimate of "how often does it lose").
- **No CI-bearing estimate yet**: BayesElo or Ordo would give a tighter CI; deferred to M4+ when SPRT-bounded changes accumulate enough data for a multi-anchor regression.

## Artifacts

- PGN: `target/matches/sprt/20260429T140833-stockfish-1320-rating-estimate.pgn`
- Log: `target/matches/sprt/20260429T140833-stockfish-1320-rating-estimate.log`

(Raw artifacts gitignored under `target/`. This summary is the committed record.)
