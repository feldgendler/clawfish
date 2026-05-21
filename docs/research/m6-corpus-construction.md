# M6.G: Corpus Construction for Texel Evaluation Tuning

Prior-art research note for the M6.G milestone. Synthesizes literature on labeled-position corpus construction for HCE Texel tuning.

**Restriction honored:** no engine source code read. All sources are CPW prose, TalkChess threads, blog posts, forum announcements, dataset READMEs, and papers.

---

## 1. The Classic Texel Tuning Corpus Recipe

### 1.1 Origin and framing

Peter Österlund described the method in 2014 TalkChess posts and applied it to Texel 1.03+. The Chess Programming Wiki article ["Texel's Tuning Method"](https://www.chessprogramming.org/Texel's_Tuning_Method) codifies it. The method is logistic regression: fit a sigmoid-mapped eval score to game-result labels (0 / 0.5 / 1) via mean-squared-error minimization over a large position corpus.

### 1.2 The error function

```
E = (1/N) * Σ ( result_i − σ(K · qscore_i) )²
```

where:
- `result_i` ∈ {0, 0.5, 1} — Black win / Draw / White win (always from White's perspective)
- `qscore_i` — quiescence-search score of position _i_ (centipawns, White-positive)
- `σ(x) = 1 / (1 + e^(−x))` — logistic sigmoid
- `K` — scaling constant (see §1.3)

Some implementations use the equivalent base-10 form: `σ(x) = 1 / (1 + 10^(−K·score/400))`, matching the Elo formula's curvature. Both are correct; the only difference is the units absorbed into K. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method); [mACE Chess blog](http://macechess.blogspot.com/2014/03/the-texel-way-of-tuning_10.html)

### 1.3 K-scaling factor

K is computed **once**, before any weight tuning, by minimizing E over the corpus with all weights held at their starting values. It is never updated again. Österlund reported K ≈ 1.13 for Texel's evaluation at the time of writing. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method)

- K represents the slope of the sigmoid at score = 0; it absorbs the arbitrary centipawn scale of the evaluation function.
- If K comes out near 0 (< 0.1), the eval is uncorrelated with the training data — likely a parsing bug or wrong-perspective issue. [TalkChess — Weird Results from Texel Tuning](https://talkchess.com/viewtopic.php?t=83196)
- K must be recomputed if the evaluation function or training corpus change substantially.

### 1.4 Why game-result labels, not engine-score labels

| Label type | Source | Bias risk |
|---|---|---|
| Game result (0/0.5/1) | Actual played game outcome | None from labeling engine |
| Engine score (cp) | Third-party engine analysis | Leaks labeling engine's eval philosophy |

Game-result labels are the Texel method's defining property. Using engine-score labels (e.g., Stockfish analysis) would tune the target engine's weights toward a proxy of Stockfish's evaluation design — circular and leaking. The only ground truth available without a labeling engine is the game result: who actually won. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method); [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)

**Project constraint (ADR-0003 spirit):** clawfish must use game-result labels only. See §3 for the Zurichess c9 label-provenance finding — this directly affects whether the Zurichess dataset is ADR-compliant.

### 1.5 Canonical corpus size

Österlund's original corpus: ~64,000 games at fast TC (1s+0.08s/move) → ~8.8 million positions. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method)

Community-observed norms:

| Dataset | Positions | Parameters tuned | Notes |
|---|---|---|---|
| Zurichess quiet-labeled.epd | 725,000 | ~20–100 | Widely reused starting point |
| Andrew Grant sets | 10M each | 400–750 | D12-resolved PVs |
| Österlund original | 8.8M | ~hundreds | Fast TC self-play |
| Leorik tuning batches | 4M active / 50M pool | ~HCE surface | Rotated between iterations |

For a corpus covering ~50–80 jointly-tuned parameters (clawfish's M6 A–F surface), 1–5 million positions is the practical working range. See §8 for corpus-size vs. quality analysis.

---

## 2. Quiet-Position Extraction Predicates

### 2.1 Why quiet positions

Using non-quiet positions with static eval (instead of qsearch) introduces tactical noise: the label (game result) reflects what actually happened, but the static eval may be wildly wrong because a capture hangs. That noise degrades the regression signal. If qsearch is called at tuning time, quiet-position filtering saves that per-position qsearch cost and enables much faster iteration. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method); [TalkChess — Zurichess quiet-like generator](https://talkchess.com/viewtopic.php?t=71469)

### 2.2 Candidate predicates (survey)

| Predicate | Mechanism | Cost | Precision |
|---|---|---|---|
| A. `!in_check` | Side to move not in check | Trivial | Necessary but not sufficient — position may have hanging pieces |
| B. `|static_eval − qsearch| < threshold` | Run qsearch, compare | High (qsearch per position) | Strongest: directly certifies static == qsearch | 
| C. `last_N_plies_no_captures_checks_promotions` | Look back in PGN move sequence | Requires PGN context | Heuristic; reliable for N ≥ 4 |
| D. Random ply sampling | Sample a ply uniformly from each game | Trivial | Noisy — samples all phases including tactical bursts |
| E. `qsearch PV terminal` | Play out the qsearch PV, use leaf position | High (qsearch + make_move loop) | Strongest guarantees; Lithander approach |

**Predicate B detail:** Common thresholds in the literature — 25–30 cp (xr_a_y, TalkChess 2017), 60 cp (arxiv:2412.17948). Tighter is better for training purity; loosening admits more positions. [TalkChess — Zurichess quiet-like generator](https://talkchess.com/viewtopic.php?t=71469); [Study of the Proper NNUE Dataset, arXiv:2412.17948](https://arxiv.org/pdf/2412.17948)

**Predicate C detail (pgn-extract):** Ferdy's recommendation — `pgn-extract --quiescent N` marks positions where the last N plies contain no checks, captures, or promotions. N=4 covers most tactical bursts. Efficient for large PGN sources (CCRL, Lichess). [TalkChess — Zurichess quiet-like generator](https://talkchess.com/viewtopic.php?t=71469)

**Predicate E (Lithander/Leorik approach):** Instead of filtering out non-quiet positions, make them quiet — call qsearch, play out the PV, store the leaf. Avoids selection bias (the set of positions where static == qsearch is not a random sample of game positions). [TalkChess — Experiments in generating Texel Tuning data](https://talkchess.com/viewtopic.php?t=78536&start=10)

### 2.3 Additional standard filters applied post-predicate

- **Opening skip:** Exclude the first N plies of each game. Book positions are over-represented (same position across many games) and are learned by book, not by eval. Common threshold: first 8–16 plies. Österlund excludes "positions within the opening book." [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method); [TalkChess — Experiments](https://talkchess.com/viewtopic.php?t=78536&start=10) ("ply < 10" excluded)
- **High-score exclusion:** Exclude positions with |eval| > 600 cp — near-resignations; result already determined. [mACE Chess](http://macechess.blogspot.com/2014/03/the-texel-way-of-tuning_10.html)
- **Mate-score exclusion:** Exclude positions where the engine found a forced mate during game play. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method)
- **King-in-check exclusion:** A known gotcha in the Zurichess dataset — ~20,000 positions in quiet.epd where the side to move is in check, despite the set being labeled "quiet." Filtering these is necessary. [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)
- **Material filter (optional):** Some practitioners also filter out positions with large material imbalance (>50–80 cp material delta). Not universal.

### 2.4 One-position-per-game vs. many positions per game

Österlund notes that "taking one position from 8.8 million different games would be theoretically preferable" (decorrelated labels), but used ~140 positions/game from 64K games for practical corpus size. The community settled on a middle ground: cap positions per game at ~10. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel's_Tuning_Method); [TalkChess — Experiments](https://talkchess.com/viewtopic.php?t=78536&start=10) ("randomly selected at most 10 from each game seemed to help significantly")

**Rationale for per-game cap:**
- Positions within a single game are correlated (same opening, same endgame trend, same result label).
- Over-sampling from a single game inflates effective corpus size without adding information.
- A cap forces breadth over depth.

### 2.5 Recommended predicate for M6.G↔M6.I interface contract

**Recommendation: Predicate B** (`!in_check` AND `|static_eval − qsearch| < 30 cp`) applied during corpus construction, with:
- Opening skip: first 8 plies (ply < 8 in 0-indexed half-move count from game start).
- High-score exclusion: |static_eval| > 600 cp dropped.
- Per-game cap: at most 10 positions per game (random sample without replacement).
- FEN dedup: remove exact duplicates before the cap.

**Rationale:**
- Predicate B is the most widely used and gives the strongest guarantee for pure static-eval tuning.
- It pins the quiet definition against M6.I's tuner qsearch — a position that was quiet at corpus-construction time (static == qsearch at then-current weights) may not be quiet at M6.I time with different weights, but that drift is negligible for HCE weights in the ±200 cp range.
- Predicate E (leaf of qsearch PV) is stronger but computationally expensive at scale and changes the position identity (the stored position may differ from the game position).
- Predicate C (pgn-extract lookback) is cheaper and useful as a pre-filter on raw PGN before qsearch validation, but alone is not tight enough for static-eval-only tuning.

**Open question:** The exact qsearch used at corpus-construction time vs M6.I tuner time. If M6.I uses qsearch on each position during tuning (as in the Leorik approach), Predicate A alone would suffice and the quiet-filter is a performance optimization. If M6.I uses static_eval directly (as in the Zurichess dataset approach, where quiet-labeled avoids per-position qsearch at tuning time), Predicate B is mandatory. The M6.I tuner ADR should resolve this; until it does, Predicate B is the conservative safe choice.

---

## 3. Zurichess quiet-labeled.epd: Provenance and Label Semantics

### 3.1 Dataset overview

Created by Alexandru Mosoi (Zurichess engine author), published September 2016 on Bitbucket (`bitbucket.org/zurichess/tuner`). Widely used as a pre-made Texel tuning corpus. [TalkChess — training data to use with Texel Tuning method](https://www.talkchess.com/forum3/viewtopic.php?t=61427)

| File | Content | Size |
|---|---|---|
| `violent.epd` | All sampled positions (quiet + non-quiet) | ~1.5M |
| `quiet.epd` | Positions where qsearch found no winning capture | ~725K |
| `quiet-labeled.epd` | `quiet.epd` positions annotated with c9 labels | ~725K |

### 3.2 Generation pipeline

1. **75,000 games** played between three slightly different versions of Zurichess using the `2moves_v1.pgn` opening book.
2. **20 positions sampled per game** (stochastically during game play) → ~1.5M positions → `violent.epd`.
3. **Quiet filter:** Positions where qsearch found a winning capture removed → `quiet.epd` (~725K).
4. **Labeling:** From each quiet position in `quiet.epd`, **a separate game was played using Stockfish 080916**. The result of that Stockfish-played game is the c9 label. → `quiet-labeled.epd`.

Source confirmation: TalkChess thread directly quotes the Bitbucket README: "from each quiet position a game using Stockfish 080916 was played. The results were stored in quiet-labeled.epd." [TalkChess — training data to use with Texel Tuning method](https://www.talkchess.com/forum3/viewtopic.php?t=61427)

### 3.3 c9 label semantics

**The c9 labels are NOT the original Zurichess self-play game results.** They are the results of Stockfish-played continuation games starting from each quiet position.

- Format: EPD `c9` operand — values `"1-0"`, `"0-1"`, `"1/2-1/2"` (game outcome notation from White's perspective).
- Numeric encoding for MSE: 1-0 → 1.0, 0-1 → 0.0, 1/2-1/2 → 0.5.

### 3.4 ADR-0003 compliance assessment

**The c9 labels are engine-result labels, not game-result labels from the original game.**

This is a critical distinction for clawfish's ADR-0003-by-spirit constraint:

- The labels reflect what **Stockfish 080916** would achieve playing from each position, not what the original Zurichess games produced.
- Stockfish 080916 has its own evaluation design. Positions it systematically wins or draws will get biased labels compared to what the game's original result was.
- This is precisely the "labeling engine bias" that ADR-0003 is designed to avoid: tuning clawfish's weights to reproduce what Stockfish thinks about each position, filtered through Stockfish's play style.

**Evidence of empirical bias:** One TalkChess practitioner compared Zurichess data against a pure-game-result self-play dataset and found "a huge difference in pawn MG/EG value, like 70/130" — the datasets produced materially different weight vectors, consistent with label-source bias. [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)

**Known quality defect:** The `quiet.epd` set contains ~20,000 positions where the side to move is in check — an error in the quiet predicate application. [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)

**Conclusion:** The Zurichess `quiet-labeled.epd` **fails** clawfish's ADR-0003-by-spirit constraint (game-result labels only). It should **not** be used as-is. The unlabeled `quiet.epd` (without c9 labels) could be re-labeled from clawfish self-play game results, but the positions themselves were generated by Zurichess games and may have distributional bias toward positions Zurichess's search tends to explore.

### 3.5 Practical alternative

Use `quiet.epd` (positions only, no labels) as a supplementary source of diverse quiet positions, re-labeled by clawfish self-play game results. Or generate a fresh corpus from CCRL/Lichess game PGNs and clawfish self-play — both with original game-result labels. See §4 and §5.

---

## 4. CCRL and Lichess PGN Sources

### 4.1 CCRL (Computer Chess Rating Lists)

| Property | Value |
|---|---|
| URL | `computerchess.org.uk/ccrl/4040/` |
| Primary TC | 40/15 (40 moves in 15 min; ~5–8 min average game) |
| Secondary TC | 40/2 (blitz); FRC variants |
| Game type | Computer vs. computer |
| Rating range | ~1000–3600 Elo (all engines) |
| PGN availability | Yes — full game archive downloadable |
| License | Not explicitly CC; widely used for research without restriction in practice |
| Evaluation perspective | **Inconsistent** — some games use White-POV evals, others side-to-move POV. Not used for the evals; use only for game results and position extraction. [TalkChess — CCRL PGN database POV](https://talkchess.com/viewtopic.php?t=83216) |

**Scale:** An LCZero/CCRL combined dataset of 2.5M games (40/40 + 40/4) is ~539 MB PGN compressed, or ~11 GB in binary format. [LCZero blog — A Standard Dataset](https://lczero.org/blog/2018/09/a-standard-dataset/)

**Filtering recommendations for tuning corpus:**

| Filter | Rationale |
|---|---|
| Exclude `Termination "Time forfeit"` | Time-forfeit games may not reflect positional truth; result labels are noise |
| TC-class filter: 40/15 only (exclude blitz) | Faster TCs have more blunder noise; slower TCs produce higher-quality game results |
| Rating band: ≥ 2800 Elo both players | Positions are more representative of evaluable middlegames; low-rated games have poor positional decisions |
| Opening-ply skip: first 8 plies | Book positions are over-represented |
| One-sided draws filter: exclude perpetual-check draws | Rare; worth considering for endgame quality |

**Gotcha — evaluation POV:** CCRL PGN files embed engine evaluations in varying perspectives. The result tag (`1-0`, `0-1`, `1/2-1/2`) is always reliable and in standard format. Use only the result tag and move sequence; ignore embedded eval annotations. [TalkChess — CCRL PGN database POV](https://talkchess.com/viewtopic.php?t=83216)

### 4.2 Lichess open database

| Property | Value |
|---|---|
| URL | `database.lichess.org` |
| Format | PGN compressed with Zstandard (`.pgn.zst`) |
| Coverage | Monthly files, non-cumulative, from 2013 to present |
| Total games (standard, as of 2026-05) | ~7.77 billion rated games |
| Compressed size per monthly file | ~10–25 GB (varies; recent months ~20 GB) |
| Total compressed size | ~2 TB |
| License | Creative Commons CC0 (public domain) |
| PGN fields | `WhiteElo`, `BlackElo`, `TimeControl`, `Result`, `Termination` ("Normal" / "Time forfeit" / "Abandoned"), `UTCDate`, `ECO`, etc. |

[Lichess open database](https://database.lichess.org/); [Lichess forum — how to download](https://lichess.org/forum/general-chess-discussion/how-to-download-big-databases-of-lichess)

**Filtering recommendations:**

| Filter | Rationale |
|---|---|
| `Termination "Normal"` only (exclude "Time forfeit", "Abandoned") | Time-forfeit labels corrupt game result; abandoned games have incomplete move sequences |
| `WhiteElo >= 2000 AND BlackElo >= 2000` | Reduces blunder noise from beginners |
| `TimeControl >= 300` (5 min or longer) | Bullet/hyperbullet games have severe time-pressure blunder noise |
| Exclude bullet (`TimeControl < 60`) | Bullet TC games have very high blunder density |
| Opening-ply skip: first 8–12 plies | Book/theory positions |
| Per-month subsampling | Any single recent month contains millions of games — subsample to target corpus size |

**Practical extract:** From one recent Lichess month at 2000+ Elo, 10+ min TC, "Normal" termination, one gets several hundred thousand games — enough positions for a solid training corpus without processing the full 20 GB file.

**Tool:** `pgn-extract` processes Lichess PGN at ~12,000 games/second with `-t` tag filters and `--minmoves`. [Advanced Filtering with pgn-extract](https://bigeatie.com/posts/pgn-extract/)

---

## 5. Self-Play Data Generation for Tuning

### 5.1 Rationale for self-play corpus

External databases (CCRL, Lichess) provide human- or higher-rated-engine-played positions. Self-play from clawfish generates positions from the distribution clawfish's search actually encounters — the "deployment distribution." Positions where clawfish is strong but its eval is miscalibrated are exactly the high-leverage examples for tuning its own weights. [TalkChess — Leorik devlog](https://talkchess.com/viewtopic.php?t=79049&start=420)

The theoretical desideratum ("deployment-distributed data") is that the tuned eval should be calibrated for positions the engine actually sees during search, not positions from a different engine's search trajectories. A tuner trained entirely on Stockfish-calibrated positions may produce weights that are slightly mismatched for clawfish's search depth and pruning profile.

### 5.2 Opening book diversification

**The core problem:** Without diversification, self-play games from a fixed set of openings will converge to a small number of opening trees, badly over-representing early positions.

**Common approaches:**

| Method | Description | Used by |
|---|---|---|
| Polyglot / EPD opening book | Play the first N moves from a book, then search | Most HCE tuners |
| Random plies at game start | Play 2–8 random moves before searching | Integral engine (3–4 random moves), Minic (2–8 plies) |
| Sampled book positions | Pick a random FEN from a curated list and start from there | Self-play data generation literature |

Commonly cited books for opening diversification: `UHO_Lichess_4852_v1`, `2moves_v1.pgn` (Zurichess's book). Any book with broad opening coverage is suitable. [TalkChess — How do NNUEs self train](https://talkchess.com/viewtopic.php?t=83343)

**Opening-skip after the book:** Even with a book, apply opening-ply skip: don't record positions from the first 8–12 moves. Book positions have high positional repetition across games.

### 5.3 TC choice for self-play data generation

| TC | Tradeoff |
|---|---|
| Ultra-fast (< 1s per move) | High throughput, high blunder rate, noisy labels |
| Fast (5–15s per move) | Good balance: moderate quality, reasonable throughput |
| Moderate (1–5 min per move) | High-quality games, low throughput — slow to generate a large corpus |
| Long (> 5 min) | Near-optimal play quality, impractical for corpus generation |

**Recommendation:** Fast TC (5–15 seconds per move, or equivalent increment-based) balances label quality with generation speed for a corpus-generation campaign. At this TC, clawfish plays near its best and game results are meaningful signals.

### 5.4 Self-play label purity

- Labels are the result of clawfish's own game — 0/0.5/1 (Black win / Draw / White win). Zero labeling-engine bias.
- Leorik's approach: 100% self-play, pure game-result labels, no external data, no Stockfish involvement. [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)
- Drawback: self-play generates from positions where clawfish's current search tends to go. If early weights are far off, clawfish's games may not explore positions where the eval is most wrong. This motivates blending self-play with external PGN data.

### 5.5 The held-out self-play validation set

The M6.G roadmap specifies "a held-out deployment-distributed self-play validation set." This means:
- Generate self-play games with the same diversified opening book and TC as the training corpus.
- Hold out a random fraction (10–20%) as a validation corpus.
- **Never tune on it.** Use its MSE (logistic loss) as the tuning stopping criterion for M6.I.
- The validation set must be sampled independently from the training set — separate games, not separate positions from the same games.

---

## 6. Held-Out Validation Split Discipline

### 6.1 Why a held-out set

Texel tuning minimizes MSE on the training corpus. The training MSE will always improve with more iterations, but the held-out validation MSE is the better stopping criterion — it measures generalization, not fitting. Overfitting to training data can produce worse playing strength even as training MSE decreases. [chess4j blog — Automated Parameter Tuning](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)

The Leorik devlog documents this explicitly: "I only update my best coefficients when after a tuning iteration the MSE on the total set has improved." Rotating 4M active positions from a 50M pool enforces diversity and prevents fitting to a fixed subset. [TalkChess — Leorik devlog](https://talkchess.com/viewtopic.php?t=79049&start=420)

### 6.2 Contamination risks

| Risk | Description | Mitigation |
|---|---|---|
| Same-game positions in both splits | If split at position level, positions from the same game appear in train and val | Split at game level — all positions from a game go into one split |
| Opening-transposition leakage | Same opening position appears in many games; occurs in both splits despite game-level split | FEN dedup before splitting; or accept as unavoidable in large corpora |
| Self-play distribution shift | Validation self-play was generated at different weights than training self-play | Generate validation and training self-play in the same campaign run |
| Engine-specific corpus | External corpus (CCRL) may not reflect clawfish's search distribution | Supplement with self-play; measure validation MSE on both sets separately |

### 6.3 Split strategy for M6.G

1. **Split at game level**: group all positions from each game, then assign games to train or validation.
2. **Ratio**: 80% train / 20% validation (matching common practice — chess4j, LCZero standard dataset).
3. **Separate the self-play validation set**: the "deployment-distributed" validation set is generated from separate self-play games, not split from the same run as training self-play.
4. **Monitor training corpus MSE and validation MSE separately** during M6.I tuning; stop when validation MSE stops improving.

### 6.4 Held-out logistic loss as stopping criterion

The stopping criterion for M6.I should be: stop when the validation MSE (or equivalently, logistic loss — MSE on binary labels equals cross-entropy up to a constant when using game results) has not improved for a fixed number of iterations. This is the standard ML early-stopping criterion and avoids the M6.I risk of over-tuning to training noise.

---

## 7. Reproducibility Norms

### 7.1 Corpus manifest

A reproducible corpus needs:

| Artifact | Contents |
|---|---|
| `manifest.json` | Source file names, download URLs, SHA-256 hashes, date downloaded |
| `rng_seeds.txt` | Seeds for shuffle, sampling, game-assignment to train/val |
| `filter_spec.txt` | Exact predicate parameters (quiet threshold, opening-ply skip, per-game cap, |eval| cutoff) |
| `re-run.sh` | Script to reproduce the corpus from the raw sources |

All committed under `bench/` as specified in the M6.G roadmap.

### 7.2 FEN dedup rationale

Opening positions are massively over-represented across games:
- Starting position appears in every game.
- Common 10-move opening trees appear in thousands of games.
- Without dedup, MSE gradient is dominated by these common positions — the tuner fits the opening eval first and may never converge on endgame/middlegame eval.

FEN dedup eliminates exact-position duplicates. In practice, exact duplicates number in the hundreds to low thousands in a well-filtered corpus ([TalkChess — Experiments](https://talkchess.com/viewtopic.php?t=78536&start=10)), so the impact is small for large corpora but important for small (<1M) ones.

**Per-FEN cap alternative:** Instead of strict dedup, cap each unique FEN at N occurrences (e.g., 5). This allows rare positions to keep all occurrences while preventing opening positions from dominating. The per-FEN cap is a softer version of dedup that preserves some repetition signal.

### 7.3 Shuffle and RNG seeding

After filtering and dedup:
1. Shuffle the full corpus with a fixed RNG seed.
2. Assign first 80% to train, last 20% to validation (or use a game-level assignment before shuffling).
3. Record the seed in `rng_seeds.txt`.

A fixed seed enables exact corpus reproduction on any machine, a prerequisite for the M6.G "reproducible snapshot" requirement.

### 7.4 Source hashing

For each raw PGN source file:
- Record SHA-256 hash and file size.
- For Lichess monthly files: record the specific year-month (e.g., `lichess_db_standard_rated_2024-07.pgn.zst`).
- For CCRL: record the archive date/version since CCRL is updated continuously.

---

## 8. Corpus Size vs. Tuning Quality

### 8.1 What the literature says

For ~50–80 jointly-tuned HCE parameters (clawfish M6 A–F surface):

| Corpus size | Observed behavior | Source |
|---|---|---|
| 10,000 | Insufficient — convergence artifacts | TalkChess anecdote |
| 400,000 | Minimal gains reported | TalkChess — Experiments |
| 725,000 | Standard Zurichess set — broadly used, works for ~20–100 params | Multiple practitioners |
| 800,000 | ~25 Elo improvement noted | TalkChess — Experiments |
| 1–2M | Solid working range; diminishing returns begin | Multiple practitioners |
| 2.5M | LCZero standard dataset (80% train, 500K test) | LCZero blog |
| 8.8M | Österlund original; 400-param scale | CPW — Texel |
| 10M | Andrew Grant sets; 750-param scale; ~130 iterations to converge | TalkChess — Some more HCE data |
| > 10M | Saturation for HCE; mainly useful for NNUE | Literature consensus |

[TalkChess — Some more HCE "Texel Tuning" Data](https://talkchess.com/viewtopic.php?t=77502); [TalkChess — Experiments in generating Texel Tuning data](https://talkchess.com/viewtopic.php?t=78536&start=10); [chess4j blog](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)

### 8.2 Diminishing returns analysis

- **Below 500K positions:** Under-coverage — some eval features may not appear enough times to produce a meaningful gradient. King-safety terms and outpost bonuses fire in a small fraction of positions; a 400K corpus may have only ~20–40K positions where a given feature is active.
- **500K–2M positions:** The practical sweet spot for HCE. Each additional million positions adds incrementally to convergence confidence.
- **Beyond 2M positions:** Convergence speed scales roughly with corpus size (larger corpus → slower iteration → same result). Quality of positions and correct quiet predicate matter more than raw size.
- **Saturation:** A forum observation that with the 2.5M Lichess set, training MSE continues to improve past 40 iterations but playing strength degrades — evidence of overfitting to the training distribution. [TalkChess — Some more HCE "Texel Tuning" Data](https://talkchess.com/viewtopic.php?t=77502)

### 8.3 The bias/variance tradeoff for game-result-labeled Texel

**Bias source:** Game results are a very noisy proxy for position evaluation. A queen-up position might be labeled "draw" because the winning side blundered later. This label noise is irreducible — it is the fundamental cost of game-result labels.

**Variance reduction:** More positions average out label noise. A corpus of 5M positions from diverse games has much lower label noise at the evaluation-feature level than 100K positions.

**Implication for M6.G size target:** For ~80 parameters, 1–2M positions provides a good bias/variance balance. The M6.G corpus should target at least 1M positions. 2M is better; 3M is comfortable headroom.

### 8.4 Diversity vs. quantity

Multiple sources agree: position diversity is more important than raw count. [TalkChess — Texel tuning method question](https://talkchess.com/viewtopic.php?t=64189) ("You better worry on the diversity of the training positions.")

- A corpus with 10 positions from 1M different games is better than 1M positions from 10K games (all from the same 10 openings).
- The per-game cap (§2.4) enforces diversity.
- Blending CCRL games (computer-vs-computer, diverse openings, full rating range) with Lichess games (human-played) adds stylistic diversity.

---

## 9. Summary Table: Source × Tradeoff Matrix

| Source | Scale | Label purity | ADR-0003 compliant | Quiet predicate | Key filter needed |
|---|---|---|---|---|---|
| **CCRL 40/15 PGN** | ~1–2M games; ~500 MB PGN | Game result (original) | Yes | Must apply | TC class, time-forfeit exclusion, rating band |
| **Lichess monthly PGN** | ~100M games/month; ~20 GB/month | Game result (original) | Yes | Must apply | Termination=Normal, TC≥300s, Elo≥2000 |
| **Zurichess quiet-labeled.epd** | 725K positions | Stockfish 080916 game results | **NO** | Already applied | Do not use as-is; re-label or use positions only |
| **Zurichess quiet.epd** | 725K positions | No labels | N/A (positions only) | Already applied | ~20K in-check positions present — filter |
| **Clawfish self-play** | Unlimited (generate on demand) | Game result (original) | Yes | Must apply | Opening diversification, fast TC |
| **Andrew Grant sets** | 10M/set | Mixed (see gotcha) | Needs audit | Pre-applied | "Mildly mislabeled" per Grant — FRC/standard mix |

---

## 10. Gotchas and Corner Cases

- **Zurichess c9 = Stockfish labels, not original game results.** This is the primary ADR-0003 hazard. The quiet-labeled.epd is widely recommended in the community for being "generic" and not overfitting to one engine, but from clawfish's perspective, it introduces Stockfish's eval philosophy. (§3)

- **CCRL eval POV inconsistency.** CCRL PGN embeds engine evaluations in inconsistent perspectives (some WPOV, some side-to-move POV). Use only the `Result` tag; ignore embedded eval annotations. (§4.1)

- **Lichess "Time forfeit" Termination tag.** PGN standard defines `Termination "Time forfeit"`. These games must be excluded — the game result does not reflect positional quality. Lichess includes this tag reliably. (§4.2)

- **Zurichess quiet.epd has ~20K in-check positions.** The quiet predicate was applied imperfectly. Filter `!in_check` before using these positions. (§3.4)

- **Same-game split contamination.** Splitting at position level (not game level) puts positions from the same game in both train and validation, inflating validation performance. Always split at game level. (§6.2)

- **Opening-ply skip too short.** If the skip is fewer than 8 plies, book-theory positions dominate and the tuner over-fits the opening. If too long (> 20 plies), you discard significant opening eval signal needed for M6.I. (§2.3)

- **K recomputation.** If corpus or eval change substantially between corpus construction and M6.I tuning, K must be recomputed. Recomputing K at the start of M6.I with the new eval weights is standard practice. (§1.3)

- **Self-play distribution shift.** Validation self-play generated after M6.I weight updates will have a different distribution than training self-play generated before. The held-out set should be frozen after M6.G and never regenerated. (§6.2)

- **Leorik engine-self-play bias.** Tuning on self-play data teaches the engine what positions its search tends to reach — which may reinforce existing weaknesses. Blending with external data (CCRL, Lichess) partially counters this. (§5.4)

- **"40 iterations then Elo degrades" phenomenon.** Over-iterating on a fixed corpus causes overfitting. The held-out validation MSE is the stopping criterion, not a fixed iteration count. (§8.2)

---

## 11. Recommendations for M6.G

### Corpus composition target

| Source | Target positions (after filtering) | Label | Quiet predicate |
|---|---|---|---|
| CCRL 40/15 PGN | 500K–1M | Original game result | B (`|static − qsearch| < 30cp` + `!in_check`) |
| Lichess (2000+ Elo, ≥5 min TC, Normal termination) | 500K–1M | Original game result | B or C (pgn-extract --quiescent 4 as pre-filter, then B) |
| Clawfish self-play (fast TC, diversified book) | 500K–1M | Original game result | B |
| **Total training corpus** | **1.5–3M** | All game-result | — |
| Clawfish self-play validation (separate games) | 200–500K | Original game result | B |

Zurichess `quiet.epd` positions may be used as **unlabeled position seeds** (to find interesting positions to generate self-play from), but the c9 labels must not be used.

### Quiet predicate: M6.G↔M6.I interface contract

**Adopted predicate:**

```
QUIET ⟺ !in_check(pos)
        AND |static_eval(pos) − qsearch(pos)| < 30 cp
```

With additional post-filters:
- Opening skip: ply < 8 (0-indexed from game start).
- |static_eval| > 600 cp: excluded.
- Per-game cap: at most 10 positions per source game (reservoir sample).
- FEN exact-duplicate removal across the full corpus.

This predicate is pinned as the M6.G↔M6.I interface. M6.I must use the same quiet definition when evaluating positions at tuning time (i.e., either call `static_eval` directly on positions pre-certified as quiet, or verify quietness before each static_eval call and skip non-quiet positions).

### Reproducibility artifacts (to commit to `bench/`)

- `bench/corpus/manifest.json` — source file hashes, download dates, filter parameters.
- `bench/corpus/rng_seeds.txt` — shuffle seed, train/val split seed, self-play game seed.
- `bench/corpus/re-run.sh` — full reproduction script.
- `bench/corpus/corpus_stats.txt` — position count by source, label distribution (W/D/L), phase distribution.

---

## Sources

- [Texel's Tuning Method — Chess Programming Wiki](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [Peter Österlund — Chess Programming Wiki](https://www.chessprogramming.org/Peter_%C3%96sterlund)
- [Automated Tuning — Chess Programming Wiki](https://www.chessprogramming.org/Automated_Tuning)
- [TalkChess — training data to use with Texel Tuning method (Zurichess dataset announcement)](https://www.talkchess.com/forum3/viewtopic.php?t=61427)
- [TalkChess — Zurichess quiet-like generator](https://talkchess.com/viewtopic.php?t=71469)
- [TalkChess — Texel tuning Zurichess quiet-like generator page 2](https://talkchess.com/forum3/viewtopic.php?t=71469&start=10)
- [TalkChess — Labeled positions for Texel tuning](https://talkchess.com/viewtopic.php?t=82191)
- [TalkChess — Some more HCE "Texel Tuning" Data](https://talkchess.com/viewtopic.php?t=77502)
- [TalkChess — Experiments in generating Texel Tuning data page 2](https://talkchess.com/viewtopic.php?t=78536&start=10)
- [TalkChess — Weird Results from Texel Tuning](https://talkchess.com/viewtopic.php?t=83196)
- [TalkChess — Evaluation & Tuning in Chess Engines](https://talkchess.com/viewtopic.php?t=74877)
- [TalkChess — Texel tuning method question](https://talkchess.com/viewtopic.php?t=64189)
- [TalkChess — Help with Texel's tuning page 2](https://talkchess.com/viewtopic.php?t=76238&start=10)
- [TalkChess — Leorik devlog page 43](https://talkchess.com/viewtopic.php?t=79049&start=420)
- [TalkChess — CCRL PGN database POV](https://talkchess.com/viewtopic.php?t=83216)
- [TalkChess — How do NNUEs self train](https://talkchess.com/viewtopic.php?t=83343)
- [mACE Chess — The Texel way of tuning](http://macechess.blogspot.com/2014/03/the-texel-way-of-tuning_10.html)
- [ROFCHADE Technical](https://rofchade.nl/?page_id=116)
- [Zurichess tuner downloads — Bitbucket](https://bitbucket.org/zurichess/tuner/downloads/)
- [Zurichess evaluation improvements — Medium/Alexandru Mosoi](https://medium.com/@brtzsnr/hi-all-a73c1b7b7a73)
- [Automated Parameter Tuning in chess4j — jamesswafford.dev](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)
- [Lichess open database](https://database.lichess.org/)
- [Lichess forum — how to download big databases](https://lichess.org/forum/general-chess-discussion/how-to-download-big-databases-of-lichess)
- [Advanced Filtering with pgn-extract — bigeatie.com](https://bigeatie.com/posts/pgn-extract/)
- [LCZero blog — A Standard Dataset](https://lczero.org/blog/2018/09/a-standard-dataset/)
- [Study of the Proper NNUE Dataset — arXiv:2412.17948](https://arxiv.org/html/2412.17948v1)
- [CCRL 40/15 Games](https://ccrl.chessdom.com/ccrl/4040/games.html)
