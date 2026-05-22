# Texel Position Sampling: Dataset Design for HCE Tuning

**Research date:** 2026-05-22
**Commissioned for:** M6.I single joint-Texel pass
**Engine context:** clawfish — game-result labels, sigmoid-of-eval MSE objective, `static_eval_white` (NOT qsearch) at tune time, `PER_GAME_CAP=10`, `OPENING_SKIP_PLIES=8`, `QUIET_MARGIN_CP=30`, `HIGH_SCORE_CP=600`, FEN-dedup across corpus

---

## Executive Summary

The raw positions-per-game ratios in our corpus (~75 for Lichess, ~136 for CCRL) are high but standard: Österlund's canonical Texel dataset extracted ALL ~140 positions/game from 64,000 games. The community has converged on a per-game cap as the primary mitigation, and the most-cited practitioner figure is **10 positions per game** (Andrew Grant / Ethereal, corroborated by the Blunder thread). Our `PER_GAME_CAP=10` is exactly at the community consensus.

Our `OPENING_SKIP_PLIES=8` is below the dominant practitioner value of 16 half-moves; the CPW/Österlund method uses opening-book exclusion only. `QUIET_MARGIN_CP=30` is the most commonly cited threshold. FEN-dedup across the corpus is sound practice; the literature does it implicitly via "no duplicate positions" clauses.

With `PER_GAME_CAP=10` over ~41K games (26,628 Lichess + 14,713 CCRL), the filtered-and-deduped corpus will yield roughly **200–350K** positions — healthy for a few-hundred-parameter HCE by the 64K-games-≥400-params rule. The binding constraint is game diversity (number of independent games), not raw position count.

**The one actionable open question is whether the game-length asymmetry between CCRL (~136 plies) and Lichess (~74 plies) needs correction.** Under a fixed cap-10, CCRL positions are drawn from a denser game trajectory, which slightly overrepresents long middlegames and endgames relative to Lichess. No literature source quantifies this bias specifically; see Section 6 for the analysis and recommendation.

---

## 1. Positions per Game: Full Extraction vs Subsampling

### What Österlund (CPW canonical) actually did

- Extracted **ALL** positions from 64,000 self-play games at fast TC (1s+0.08s).
- Typical game length ~140 positions → ~8.8 million total.
- Filtered: remove opening-book positions + mate-score positions (but NOT quiet-filtered at the position level; this came later).
- Acknowledged the dependency: "since more than one position is picked from each game (in fact about 140 positions per game on average), the result is invalid because there is a dependence between terms." He reframed it as a weighted least-squares problem where the weights reflect how often that position type appears in games.
- **Preferred ideal:** "one position each from 8.8 million different games" — but obtaining that many games was cost-prohibitive, so full extraction from 64K games was the pragmatic choice.
- Statistical justification: "I believe 64,000 independent events is more than enough to estimate 400 parameters anyway."

Sources: [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)

### What practitioners do — subsampling to a per-game cap

| Who / engine | Per-game cap | Game count | Total positions | Notes |
|---|---|---|---|---|
| Peter Österlund (Texel) | None (all ~140) | 64,000 | ~8.8 M | Acknowledged dependency; preferred ideal was 1/game/8.8M games |
| Andrew Grant (Ethereal) | 10 random | ~1,000,000 | ~10 M | D12 PV endpoint search; random 10 per game explicitly stated |
| Algerbrex (Blunder) | 10 random | ~475,000 | ~1.3 M | "Rather than just grabbing as many valid FENs as possible, I randomly selected at most 10 from each game" — significant quality improvement noted |
| Zurichess (Alexandru Moșoi) | 20 random (sampled from millions evaluated mid-game) | 75,000 self-play | 725K quiet | CCRL corpus → 475K games → 761K quiet; ~1.6 quiet/game after filter |
| CCRL/quiet generator (TalkChess) | None (all, opening/end filtered) | 475,200 (filtered CCRL) | 761,612 quiet | 16 half-move opening skip; "endgame−6" ply cap |
| 1M training positions (TalkChess) | Implicit (dedup) | ~8,000+ OTB GM | 1,000,000 unique | First 4 full moves excluded |

Sources:
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [TalkChess — Evaluation & Tuning in Chess Engines (Grant)](https://talkchess.com/viewtopic.php?t=74877&start=20)
- [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)
- [TalkChess — training data to use with Texel Tuning method](https://www.talkchess.com/forum3/viewtopic.php?t=61427)
- [TalkChess — Texel tuning Zurichess quiet like generator](https://talkchess.com/viewtopic.php?t=71469)
- [TalkChess — 1 Million Training Positions for Texel Tuning](https://talkchess.com/viewtopic.php?t=81586)

### Takeaways

- There is no single standard. A spectrum exists: Österlund-style full extraction (all positions, rely on correlated weighting); practitioner cap-10 (Grant, Blunder); Zurichess-style "20 sampled from millions evaluated."
- The cap-10 value appears independently in the two most-cited practitioner write-ups (Grant and Blunder). No one has published a controlled ablation of cap-5 vs cap-10 vs cap-20; it is an empirical rule of thumb.
- "More is not always better" once you have sufficient diversity: Blunder's author found that capping at 10 "improved quality significantly compared to exhaustive extraction approaches."
- Österlund explicitly preferred "one per game from many games" over "many per game from few games" — the per-game cap is the tractable approximation of that ideal.

---

## 2. Autocorrelation and Effective Sample Size

### The mechanism

Positions from the same game share:
- Material balance (slowly drifting over the game).
- The **game-result label** — all positions in a game carry the same win/loss/draw outcome. This is the dominant correlation source.
- Pawn structure and piece placement trajectories.
- Opening line until at least ply ~20–30.

### Österlund's framing

He acknowledged the dependence explicitly and reframed it: multiple positions from the same game function as a **weighted** contribution — positions of type T appear proportionally to how often T appears in real games. This is a feature, not a bug, if you want the evaluation to be calibrated to game-frequency. But it means the statistical unit of observation is the **game**, not the position.

### Quantitative estimate

No published formula in the chess tuning literature. The general effective-sample-size result from autocorrelated data is:

```
N_eff ≈ N / (1 + 2 · sum_k ρ_k)
```

For positions from the same game, ρ at lag 1 (adjacent plies) is high (shared material, label). By the time a cap of 10 is applied with random sampling spread across the game's post-opening positions, the within-game autocorrelation is reduced substantially compared to extracting 140 consecutive positions.

### Mitigations in order of effectiveness

| Mitigation | Mechanism | Status in clawfish corpus |
|---|---|---|
| Per-game cap (≤10) | Limits correlated contribution per game | Applied: `PER_GAME_CAP=10` |
| Random (reservoir) sampling per game | Spreads the 10 positions across the game trajectory instead of consecutive blocks | Applied: seeded reservoir sampling in `corpus build` |
| Opening-ply skip | Removes positions where the label is most decoupled from the board state | Applied: `OPENING_SKIP_PLIES=8` |
| FEN dedup across corpus | Removes positions shared by many games (early opening moves especially) | Applied: full-corpus FEN dedup in `corpus build` |
| High-score clamp | Removes positions in won/lost games where positions are forced (near-certain labels) | Applied: `HIGH_SCORE_CP=600` |

Sources:
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)

### Gotcha: the dedup count tells you something

The Blunder experiments found "approximately 200–300 duplicate positions removed from a ~1.3M FEN corpus" (~0.02% rate). Our full-corpus FEN dedup across 2M+ raw positions (with many games sharing identical opening positions) should remove more: the starting position plus common first ~8 plies are the obvious cluster. Our `OPENING_SKIP_PLIES=8` pre-empts most of this before dedup runs.

---

## 3. Result-Label Noise in Opening Positions

### The problem

The game-result label applied to an opening position (e.g., ply 10, a routine Sicilian tabiya) is the outcome of a game decided perhaps 80 moves later. For a human blitz game with many tactical errors, the connection is even weaker than for engine games.

### Quantitative signal estimate

No formal measurement in the Texel literature. The signal reasoning:
- An opening position with eval near 0 will be in a game where one player eventually won. The label carries information (not random) but with very high variance.
- A late-middlegame position where eval is +300 for White is much more predictive — the label variance is lower.
- Österlund's high-score clamp (>600cp removed) eliminates the forced/late positions but doesn't address early-game noise.

### How practitioners handle it

| Mitigation | Who / source | Value used |
|---|---|---|
| Opening-book skip (book moves excluded) | Österlund (canonical Texel) | Book-depth only (~10–16 plies in fast games) |
| Fixed ply skip | CCRL quiet generator, Algerbrex/Blunder | 16 half-moves (= 8 full moves) |
| Fixed ply skip | OTB-GM dataset (TalkChess) | 8 half-moves (= 4 full moves) |
| Fixed ply skip | clawfish corpus | 8 half-moves (= `OPENING_SKIP_PLIES=8`) |
| Skip last N plies | Multiple sources | Skip "last 6 moves" / "ply > 200" |
| High-score clamp | Österlund, standard | > 600 cp |

Sources:
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [TalkChess — Texel tuning Zurichess quiet like generator p.1](https://talkchess.com/viewtopic.php?t=71469)
- [TalkChess — Experiments in generating Texel Tuning data p.1](https://talkchess.com/viewtopic.php?t=78536)
- [TalkChess — 1 Million Training Positions for Texel Tuning](https://talkchess.com/viewtopic.php?t=81586)

### Assessment for `OPENING_SKIP_PLIES=8`

8 half-moves is at the low end of the practitioner range (CCRL quiet generator uses 16, Blunder uses 16). With 8, we include positions at plies 8–16 which may still be opening-book territory in human games. The dominant risk is not Elo regression but loss of learning efficiency — those noisy positions contribute less signal per position. With a cap of 10, we lose at most ~8 positions per game from this effect (the first 8 that would have been in the draw), plus the positions we include from plies 8–15 are noisier than if we had used skip=16.

**Open question / flag:** The literature consensus is skip=16 for engine-game CCRL data. For human blitz (Lichess), the book is shorter (players diverge from theory earlier), so skip=8 may be adequate. No published ablation exists.

---

## 4. Game Count vs Position Count: Binding Constraint

### Österlund's rule

"64,000 independent events is more than enough to estimate 400 parameters."

This states the **game** is the statistical unit, not the position. The binding constraint is **game diversity**.

### Statistical reasoning

- A regression problem with P parameters needs many more than P independent observations for reliable estimation. The standard rule of thumb in regression: 10× to 20× the parameter count in independent observations.
- For P = 400 parameters: 4,000–8,000 independent game events would be the rough floor.
- For P = a few hundred (clawfish M6.I range, roughly 150–500 tunable weights): floors are similarly 1,500–10,000 games.
- 41,341 games (clawfish corpus) is **well above** this floor by 5×–25× depending on the exact parameter count.

### The "more is (usually) better" rule

- Österlund: one position per game from 8.8M games would be ideal — uncorrelated positions, each from a fresh game.
- Grant (Ethereal): 1M games × 10 positions/game = 10M positions, from 1M independent game events. This is the high end of common practice.
- Our ~41K games × cap-10 = ~410K positions before filter, ~200–350K after filter + dedup. This is at the modest end of practice — workable, not excess.

### Is 41K games enough?

- For a few-hundred-parameter HCE: **yes**, with reasonable confidence. Österlund uses 64K games for 400 parameters.
- The risk is not that 41K games is "too few" in an absolute sense, but that diversity may be lower if games cluster (e.g., many Lichess games from the same popular opening lines).
- FEN-dedup handles the exact-duplicate case. The residual risk is near-duplicate positions from the same popular opening family (different games, same position type) — this is handled by the optimizer learning a stable weight for that position type, not a single-game label.

### Practical floor for our parameter surface

Our M6.I surface: CONN (1 param), ISO/DBL/BWD (3), passed-pawn rank/path/distance tables (many), N/B/R/Q mobility tables (4×2 per-kind tables), king-safety (S-curve + per-kind + shield + open-file), outpost/rook-file/EG-scaling (handful). Rough total: 100–400 parameters (exact depends on table dimensions). Even at 400 parameters, 41K games × cap-10 with the game as statistical unit is well within range.

Sources:
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [TalkChess — training data to use with Texel Tuning method](https://www.talkchess.com/forum3/viewtopic.php?t=61427)

---

## 5. Quiet Filtering and Deduplication

### Quiet margin — standard values

| Value | Who uses it | Notes |
|---|---|---|
| 30 cp (|static − qsearch| ≤ 30) | CCRL quiet generator (TalkChess 2020), clawfish | Most common cited threshold |
| 25 cp | Algerbrex/Blunder (v2 refinement) | Slight tightening |
| 60 cp | NNUE dataset paper (2024) | Looser; NNUE needs more positions |
| Full qsearch resolution (no margin) | Österlund original, Ethereal (D12 PV endpoint) | Different approach: endpoint is by definition quiet |

### High-score clamp

- >600 cp: Österlund, widely followed.
- Clawfish uses `HIGH_SCORE_CP=600` — matches canonical.

### In-check filter

- Standard: drop positions where the side to move is in check.
- Clawfish applies this. Note: Zurichess `quiet-labeled.epd` was later found to contain ~20K positions where the side to move is in check — a known data quality gap in that dataset.

### Deduplication

- Implicit "no duplicate positions" clause in most practitioner datasets.
- The Blunder experiments removed only ~200–300 duplicates from 1.3M FENs (0.02%) — very low rate from self-play data.
- Clawfish applies full-corpus FEN dedup **before** the per-game cap step; this is important for handling opening positions that appear identically across many games.
- **Order matters:** our pipeline runs filter → dedup → per-game cap. If dedup runs after cap, the dedup signal is smaller. Running dedup first is slightly more conservative (removes a position from one game's bucket before the cap is evaluated), but the practical difference is small given the 0.02% duplicate rate observed in similar corpora.

### Quiet filtering with `static_eval` (not qsearch)

Our corpus uses `static_eval_white` for the quiet-margin check and records positions for `static_eval_white` scoring at tune time (not qsearch). This is consistent with the approach described in the TalkChess "prefiltered quiet positions to avoid doing qsearch" pattern — we prefilter the positions so that the margin check is trusted.

Sources:
- [TalkChess — Texel tuning Zurichess quiet like generator p.1](https://talkchess.com/viewtopic.php?t=71469)
- [TalkChess — Experiments in generating Texel Tuning data p.1](https://talkchess.com/viewtopic.php?t=78536)
- [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)

---

## 6. Source Mix and Game-Length Bias

### CCRL vs Lichess — concrete numbers

| Source | Games | Avg plies/game | Cap-10 positions | Raw positions if uncapped |
|---|---|---|---|---|
| Lichess (≥2000, blitz/rapid) | 26,628 | ~74 | ≤266,280 | ~1,970,472 |
| CCRL (engine games) | 14,713 | ~135 | ≤147,130 | ~1,986,255 |
| Clawfish self-play | ~12 | variable | ≤120 | small |

With `PER_GAME_CAP=10`:
- CCRL contributes fewer positions than Lichess (14,713 vs 26,628 games) but each CCRL game has ~1.8× the eligible raw positions.
- Under cap-10, the contribution is proportional to game count, not game length — a 135-ply game and a 74-ply game each yield at most 10 positions. This **removes the raw game-length bias** in terms of position count contribution.

### What cap-10 does NOT correct

- **Positional distribution within a game.** A CCRL game's 10 randomly sampled positions are drawn from a pool of ~135 post-opening eligible positions. A Lichess game's 10 are drawn from ~74. The CCRL positions therefore sample **denser trajectories** — more positions per unit of game-time — and so have slightly higher within-game correlation between samples.
- **Phase distribution.** Engine games (CCRL) are longer partly because engines play out drawn endgames longer and resign/adjudicate later. This means the "endgame" phase is proportionally over-represented in CCRL relative to Lichess at any given cap.
- This is a second-order effect. Its practical Elo impact is unquantified in any source found.

### Human games vs engine games: quality for HCE tuning

| Aspect | Human games (Lichess) | Engine games (CCRL) |
|---|---|---|
| Label noise | Higher — human blunders decouple result from position | Lower — engines play stronger moves, results are more position-correlated |
| Positional diversity | High — humans play many opening systems, including unusual ones | Lower — engines converge on theoretically best openings |
| Endgame accuracy | Low — humans blunder in endings | High — engines play accurate endings |
| Elo range effect | Lichess ≥2000 still contains significant blunder rate | CCRL engines are very strong |

**The Blunder (algerbrex) data point:** Human games (2700+ GM OTB) produced **−153 Elo vs the Zurichess engine-game dataset**. JDart (on TalkChess) cautioned that "human games contain blunders, making them potentially unsuitable for engines approaching 2400+ strength." Our Lichess source is Elo≥2000 blitz/rapid — a lower-quality human game pool than 2700+ GM games. This increases label noise from human sources.

**However:** clawfish is tuning its own `static_eval_white` against game-result labels. The noise in the label (human blunders) is absorbed into the sigmoid's MSE — noisy labels inflate the loss but don't bias the gradient as long as the noise is symmetric. The optimizer will converge more slowly, not to a wrong answer.

### Opening duplication in human games

Popular openings (e.g., e4/e5, d4/d5, Sicilian) appear in many Lichess games with identical or nearly-identical positions in the first 8–16 plies. Our `OPENING_SKIP_PLIES=8` removes plies 0–7; plies 8–15 (full moves 4–7) still overlap heavily across games in main-line openings. **FEN dedup removes exact duplicates but not the near-duplicate "Ruy Lopez ply 12 family" effect.** This is an unavoidable property of human game databases. It means early-middlegame evaluation for popular opening structures is over-represented relative to later-game evaluation.

The practical consequence: the optimizer may be slightly biased toward weights that fit common opening-phase middlegame structures, potentially at the expense of rarer endgame structure fitting. This is a known limitation accepted across the field.

Sources:
- [TalkChess — Experiments in generating Texel Tuning data p.1](https://talkchess.com/viewtopic.php?t=78536)
- [TalkChess — Evaluation & Tuning in Chess Engines (Grant)](https://talkchess.com/viewtopic.php?t=74877&start=20)

---

## 7. Reference Datasets — Concrete Numbers

| Dataset | Source engine / games | Games | Positions | Pos/game | Label type | ADR-0003 status |
|---|---|---|---|---|---|---|
| Zurichess `quiet-labeled.epd` | 3× zurichess self-play; Stockfish 080916 played a game from each quiet position to generate result | 75,000 | 725,000 | ~10 (after quiet filter from 20 sampled) | **Stockfish-played game results, NOT original game outcomes** | **REJECTED** — labels are Stockfish-engine-played, not original zurichess self-play results (ADR-0003 audit) |
| Ethereal E12.33-STD | Ethereal self-play, 1s+0.01s, D12 PV endpoint | ~1,000,000 | ~10,000,000 | 10 (random) | Ethereal game results | Accepted for reference; engine labels from Ethereal games |
| Ethereal E12.52-STD | Ethereal self-play, 4s+0.04s, D12 PV endpoint | ~1,000,000 | ~10,000,000 | 10 (random) | Ethereal game results | Same provenance |
| Österlund / CPW canonical | CCRL + CEGT + internet games, fast TC self-play | 64,000 | ~8,800,000 | ~140 (all positions) | CCRL/CEGT game results | Accepted; original game results |
| CCRL quiet (TalkChess 71469) | CCRL 40/40, filtered 2600+ rating | 475,200 | 761,612 (quiet) | ~1.6 (after quiet filter) | CCRL game results | Accepted; original game results |
| OTB GM (TalkChess 81586) | OTB games, all players Elo≥2500 | ~8,000+ | 1,000,000 | variable | OTB human game results | Accepted; original game results |
| Lichess Elite (CPW/engines) | Lichess high-rated player games | millions | millions | variable | Original Lichess game results | Accepted; original game results |
| clawfish M6.G corpus | CCRL + Lichess ≥2000 + self-play | 41,353 | ~200–350K (est., post-filter) | ≤10 (cap applied) | Original CCRL + Lichess + self-play game results | Accepted by design |

**Note on Zurichess label provenance (critical for ADR-0003):** The `quiet-labeled.epd` c9 labels were generated by playing a fresh game from each quiet position using Stockfish 080916. The label is the result of that Stockfish-played game, not the result of the original zurichess self-play game. This means the labels are Stockfish-engine-originated, not original-game-result labels. ADR-0003 audited and rejected this for clawfish. Sources confirming this label provenance:

- [TalkChess — training data to use with Texel Tuning method](https://www.talkchess.com/forum3/viewtopic.php?t=61427): "From each quiet position a game using Stockfish 080916 was played, and the results were stored in quiet-labeled.epd."
- [Zurichess Medium blog](https://medium.com/@brtzsnr/hi-all-a73c1b7b7a73)

Sources (general):
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [TalkChess — Some more HCE "Texel Tuning" Data](https://talkchess.com/viewtopic.php?t=77502)
- [TalkChess — Eval tuning data](https://talkchess.com/viewtopic.php?t=78958)

---

## 8. Recommendations for M6.I

### Should PER_GAME_CAP=10 stay, move, or be swept?

**Recommendation: keep `PER_GAME_CAP=10`.**

- It is the community consensus value (Grant/Ethereal, Blunder/algerbrex both independently land on 10).
- No published ablation justifies moving it up or down.
- A sweep (cap=5, cap=10, cap=20) would be informative but is not a prerequisite: the signal is second-order compared to weight initialization and optimizer hyperparameters.
- If M6.I SPRT shows poor convergence or unexpected degradation, a cap sweep is the first dataset knob to pull.

### Is 41K games enough or should we ingest more?

**Recommendation: 41K games is sufficient for M6.I. Ingest more only if SPRT results are unconvincing.**

- Österlund's rule: 64K games for 400 parameters. We have ~41K games for a similar or smaller parameter count. We are 65% of his "more than enough" threshold by game count.
- Position count at cap-10 post-filter: ~200–350K estimated. This is firmly in the range used by successful HCE tuning campaigns (zurichess: 725K; CCRL quiet: 761K; but those are all-positions-extracted, not cap-10 from many games).
- The primary risk from having fewer games is **lower diversity**, not insufficient statistics per se. The FEN-dedup + reservoir sampling already maximizes diversity from the available games.
- If M6.I SPRT lands (H1 accept), 41K games was enough. If SPRT is flat or regresses, the corpus should be examined before blaming game count — dataset quality issues (wrong K, bad label parsing, margin bug) have historically caused more failures than dataset size in the tuning literature.

### Does the Lichess/CCRL length asymmetry need correcting?

**Recommendation: no correction needed for M6.I; document as a known second-order effect.**

- `PER_GAME_CAP=10` already equalizes the raw position-count contribution per game, regardless of game length.
- The residual effect (CCRL's 10 positions are drawn from a denser pool → slightly higher within-game correlation) is second-order and unquantified in the literature.
- A correction would require either (a) a smaller cap for CCRL games (cap=5 CCRL, cap=10 Lichess) or (b) a per-source weight. Neither is standard practice and both introduce new hyperparameters.
- If a CCRL/Lichess phase bias is suspected post-M6.I (e.g., eval for engine-style long endgames is miscalibrated vs human-style positions), the right fix is to add more human games, not rebalance the existing corpus.

### OPENING_SKIP_PLIES=8 — acceptable or should it increase?

**Recommendation: keep 8 for M6.I; note 16 as the literature consensus for a future corpus update.**

- 8 half-moves is at the low end of documented practice (16 is the most cited value for CCRL/engine game data).
- For human blitz (Lichess ≥2000), divergence from theory happens earlier than in CCRL engine games, so 8 may be adequate.
- The cost of skip=8 vs skip=16 is more label noise for plies 8–15; this increases optimizer gradient variance but does not bias the result directionally.
- Increasing to 16 for a future corpus rebuild would be a marginal improvement, not a correctness fix.

### K calibration note

The K (sigmoid temperature) constant should be fitted to the clawfish corpus specifically (not borrowed from Zurichess or Ethereal). The K value differs by dataset: CCRL corpus produced K≈0.24; Lichess produced K≈1.09; Zurichess K≈0.058 (normalized differently). A mis-fitted K is one of the most common causes of Texel tuning failure documented in the literature.

Source: [TalkChess — Texel tuning Zurichess quiet like generator p.2](https://talkchess.com/forum3/viewtopic.php?t=71469&start=10)

---

## 9. Open Questions

1. **OPENING_SKIP_PLIES ablation.** Is 8 vs 16 a measurable Elo difference for clawfish specifically? No published ablation for a mixed Lichess+CCRL corpus exists. Flag for a future corpus sweep if M6.I SPRT results are marginal.

2. **CCRL-length distribution.** The 10 positions sampled per CCRL game are drawn from ~135 eligible plies; the 10 from Lichess are drawn from ~74. Does this introduce a meaningful phase-distribution skew in the corpus? No quantitative evidence found in the literature; this is a known second-order concern without an accepted mitigation.

3. **Human blunder noise floor.** Lichess ≥2000 blitz includes significantly more blundering than CCRL engine games. No published comparison of HCE tuning quality using Lichess ≥2000 specifically (most human-game experiments used 2700+ GM OTB data). If M6.I shows worse convergence than expected, adding more CCRL games and fewer Lichess games is the first targeted experiment.

4. **FEN dedup before vs after cap.** Our pipeline order is filter → dedup → cap. An alternative is filter → cap → dedup. For the small dedup rate (~0.02%), the ordering effect is negligible in practice, but it has not been verified empirically against our specific corpus.

---

*All sources cited inline above. No engine source code consulted per ADR-0003.*
