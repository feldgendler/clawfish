# ELOH.E — Pentanomial SPRT: Prior Art

**Topic:** Implementing LLR-based SPRT with pentanomial pair statistics inside the in-process harness.

---

## 1. Pentanomial vs Trinomial — Why Pentanomial

**The problem with trinomial over individual games.** When games are played in color-paired batches (engine A plays White in game 1, Black in game 2, same opening), the outcomes of the two games in a pair are negatively correlated: an unbalanced opening that favors White will inflate White's score in game 1 and deflate it in game 2. Treating each game as an independent W/D/L draw (trinomial model) ignores this structure and overestimates variance.

**What pentanomial fixes.** The pentanomial model treats each *pair* as the atomic observation unit, with five possible pair scores: {0, 0.5, 1.0, 1.5, 2.0}. The pair score absorbs the within-pair correlation, so pair outcomes are genuinely i.i.d. (under the model assumption that every pair uses the same opening or that opening effects are exchangeable).

**Variance reduction numbers:**

| Source | Stated reduction |
|---|---|
| Michel Van den Bergh / fishtest issue #348 | ~15% smaller pentanomial variance vs trinomial |
| vdbergh/pentanomial README (simulation) | ~20% fewer games to 95% pass probability (1,809 vs 2,191) |
| Fixed-nodes TalkChess thread (Laskos data) | 6–17% depending on node count / opening balance |

The 15% figure (variance, not game count) is the canonical field consensus. Game-count savings scale as sqrt of variance ratio when using SPRT, giving ~8–9% fewer games in expectation, but the full ~20% savings in the simulation above reflects both the variance reduction and the model's better-calibrated LLR increments.

**Field migration.** Fishtest migrated from trinomial to pentanomial SPRT (and from BayesElo to logistic Elo) around 2018–2019, following Michel Van den Bergh's analysis in fishtest issue #348 and his `vdbergh/pentanomial` reference implementation. The migration is documented in Fishtest Mathematics and the Chess Programming Wiki Match Statistics article.

---

## 2. GSPRT / SPRT-Pentanomial Formulation

### 2.1 Wald bounds (standard for both trinomial and pentanomial)

For α (false-positive rate) and β (false-negative rate):

```
B = log(β / (1 − α))       ← reject H1, accept H0
A = log((1 − β) / α)       ← accept H1, reject H0
```

For α = β = 0.05: B ≈ −2.944, A ≈ 2.944.
When LLR < B → H0 accepted (patch rejected). When LLR > A → H1 accepted (patch passes).

### 2.2 Trinomial GSPRT (per-game; the simplified normal approximation)

This is what most standalone SPRT calculators implement as the "logistic" model and what cutechess-cli / fastchess use under `model=logistic`.

```
# logistic score from Elo (base-10 logistic, standard for chess)
LL(elo) = 1 / (1 + 10^(−elo / 400))

s0 = LL(elo0);  s1 = LL(elo1)

# from W, D, L counts (N = W + D + L games)
w = W/N;  d = D/N;  l = L/N
s   = w + d/2           # observed score rate (MLE under H_true)
m2  = w + d/4           # E[score²] under iid assumption
var = m2 − s²           # per-game score variance
var_s = var / N         # variance of the mean score

LLR = (s1 − s0) * (2*s − s0 − s1) / var_s / 2.0
```

This is the GSPRT normal approximation (treats LLR as a random walk with estimated drift and variance). The reference is the fishtest Match Statistics page and vdbergh/pentanomial's LLRcalc commentary.

### 2.3 Pentanomial GSPRT (per-pair; same normal approximation, different sample unit)

The pentanomial formulation reuses the identical GSPRT structure but replaces per-game scores with per-pair scores:

```
pair score bins (i → score):  0→0.0,  1→0.5,  2→1.0,  3→1.5,  4→2.0

# from pair counts n[0..4]  (N_pairs = sum(n))
s_i   = [0.0, 0.5, 1.0, 1.5, 2.0]
N_p   = sum(n)
s_bar = sum(n[i]*s_i[i] for i) / N_p        # mean pair score
m2    = sum(n[i]*s_i[i]² for i) / N_p       # E[pair_score²]
var   = m2 − s_bar²                          # per-pair score variance
var_s = var / N_p

# convert pair-score hypotheses: pair score ≈ 2 * per-game score
# (because a pair worth 2 points total)
# elo0/elo1 still use the same logistic transformation, but the expected
# pair score under Hi is 2 * LL(elo_i):
s0_pair = 2 * LL(elo0);  s1_pair = 2 * LL(elo1)

LLR = (s1_pair − s0_pair) * (2*s_bar − s0_pair − s1_pair) / var_s / 2.0
```

The factor-of-2 on elo→pair_score is because a pair is worth 2 points maximum, so the expected pair score for a player with Elo advantage Δ against a neutral opponent is 2·LL(Δ). Everything else in the formula is structurally identical to the trinomial version.

**Is this exact GSPRT or a normal approximation?** The vdbergh/pentanomial reference implementation uses the more exact multinomial MLE / GSPRT from Li et al.'s GSPRT paper, which does not reduce to the closed-form above in general. The fishtest production code also uses the exact MLE form with a Siegmund discrete-time overshoot correction. For an in-house harness, the **normal approximation** (formula above) is what tools like cutechess-cli use and is widely considered adequate — the field uses it without issue for pool sizes ≥ 100 pairs. The exact MLE version is better calibrated at small N and with very skewed pair distributions, but the difference is small for typical engine-testing setups.

**Canonical references:**
- Wald (1945), "Sequential Analysis" — original SPRT and Wald bounds.
- Xiaoou Li et al., "Generalized Sequential Probability Ratio Tests for Separate Families of Hypotheses" — the GSPRT MLE form.
- vdbergh/pentanomial `doc/MLE_multinomial.pdf` and `doc/random_walks.pdf` — the specific application to multinomial chess data.
- Chess Programming Wiki, SPRT page — practitioner summary.

---

## 3. What "elo0=0 elo1=10" Means in Fastchess

**Elo convention.** Fastchess (and fishtest) use **logistic Elo** when `model=logistic`:

```
expected_score = 1 / (1 + 10^(−elo_diff / 400))
```

This is standard "FIDE-style" logistic Elo. `elo0=0` means H0: the patch has zero Elo advantage (expected score 0.5). `elo1=10` means H1: the patch has +10 Elo advantage (expected score ≈ 0.5143).

**Alternative model: `model=normalized`** uses normalized Elo (nElo), which scales the Elo difference by the per-game or per-pair score standard deviation. Under the normalized model, the expected test duration is independent of the draw ratio and opening book — useful for multi-TC comparisons. Under the logistic model, the pass/fail *error rates* (α, β) are independent of the draw ratio. The two models are asymptotically equivalent but differ in small-sample behavior and in what "10 Elo" means.

**Which model does our `scripts/sprt.sh` use?** The invocation `fastchess -sprt elo0=0 elo1=10 alpha=0.05 beta=0.05 -report penta=true` omits `model=`. From fastchess 1.8.0-alpha documentation, the `model` parameter appears required in the syntax spec (`model=(normalized|logistic|bayesian)`), but if omitted the apparent default is **logistic**. This is an open question: the fastchess man.md page lists the field as required but does not state a fallback; empirical observations from the community suggest logistic is the default. **Recommendation: explicitly add `model=logistic` to the SPRT invocation in scripts and match the in-process harness to logistic Elo for calibration parity.**

**Version stability.** No documented convention change across fastchess versions was found. The elo0/elo1 semantics have been consistent (logistic Elo difference) since at least fastchess 1.1.

---

## 4. W/D/L → Pentanomial Pair Classification

A pair consists of two games played with reversed colors on the same opening. Let game A = the game where the candidate engine plays White, game B = the game where the candidate plays Black.

| game A outcome (cand=W) | game B outcome (cand=B) | pair score | bin index | label |
|---|---|---|---|---|
| Loss | Loss | 0.0 | 0 | LL |
| Loss | Draw | 0.5 | 1 | LD |
| Loss | Win | 1.0 | 2 | LW (= WL reversed) |
| Draw | Loss | 1.0 | 2 | DL — same bin as LW |
| Draw | Draw | 1.0 | 2 | DD — same bin as LW/DL |
| Win | Loss | 1.0 | 2 | WL — same bin |
| Draw | Win | 1.5 | 3 | DW |
| Win | Draw | 1.5 | 3 | WD — same bin as DW |
| Win | Win | 2.0 | 4 | WW |

The middle bin (score=1.0, label often written "WL/DD") merges four raw outcomes: candidate loses one game and wins the other in either color arrangement, plus both drawing. This is correct — all produce a pair score of 1.0. The pair score is the candidate's total points in the pair, not the difference.

**Ordering convention.** The standard from fastchess `Ptnml(0-2)` output and Fishtest: bin indices 0–4 correspond to pair scores 0, 0.5, 1.0, 1.5, 2.0. M4.D's result `Ptnml(0-2) = [5, 40, 78, 56, 21]` uses this ordering.

**Color ordering does not matter for the classification** as long as you add the candidate's points from both games. The "game A = White" convention is one choice; "game A = first game in wall-clock order" is another. Either works so long as you are consistent — the pair score is symmetric in the two game results.

---

## 5. Pentanomial CI for Δ Elo

Given N pairs with bin counts `n[0..4]`:

```
s_i  = [0.0, 0.5, 1.0, 1.5, 2.0]   # pair scores
N    = sum(n)

# sample mean and variance of pair score
mu   = sum(n[i] * s_i[i]) / N
m2   = sum(n[i] * s_i[i]^2) / N
sigma2 = m2 − mu^2                   # per-pair variance
SE   = sqrt(sigma2 / N)              # standard error of the mean pair score

# 95% CI on mean pair score (normal approximation, valid for N ≥ ~50)
CI_pair_score = [mu − 1.96*SE, mu + 1.96*SE]

# convert pair score → Elo (inverse logistic, factor-of-2 for pair scale)
# logistic inverse: Elo_diff(s) = 400 * log10(s / (1 − s))
# pair score in [0,2]; normalize to [0,1] first: s_game = mu / 2
Elo_est  = 400 * log10((mu/2) / (1 − mu/2))

# propagate CI bounds through the same inverse transformation
Elo_lo   = 400 * log10((CI_pair_score[0]/2) / (1 − CI_pair_score[0]/2))
Elo_hi   = 400 * log10((CI_pair_score[1]/2) / (1 − CI_pair_score[1]/2))
```

This is the pair-variance estimator that produces the `[+18.18, +65.61]` bracket reported for M4.D. It is a straightforward sample-variance estimator — no jackknife required when pair outcomes are i.i.d.

**Note:** The CI above is for a *fixed* game count (post-hoc Elo estimate with symmetric CI). It is independent of the LLR-based SPRT. For a SPRT run, you report both: the LLR-based accept/reject verdict AND the post-hoc CI on Δ Elo from the accumulated pair counts.

---

## 6. Known Pitfalls

| Pitfall | Description | Mitigation |
|---|---|---|
| LLR overshoot (Wald finite-sample bias) | LLR is a discrete process; it can jump over the bound. The actual α/β realized are slightly higher than the design parameters. | The field accepts this — the overshoot is small (~0.5% at α=β=0.05). The exact Siegmund correction exists but is rarely applied outside fishtest. For in-house testing at our scale, ignore. |
| Unfinished pair at max-games cap | If `--max-games=400` terminates after game 399, the pair started by game 399 has one game missing. | Discard incomplete pairs from the pentanomial counts. Only count pairs where both games completed. Log the discarded single game for audit. Never fabricate the second game outcome. |
| elo0 = elo1 sentinel | Setting elo0=elo1 makes the SPRT degenerate (zero-width indifference zone; LLR immediately hits A or B). This cannot substitute for a "no-SPRT fixed-game-count" path. | Add a separate `--mode=fixed-games` path (no SPRT, just play N pairs and report CI). Use `elo0=elo1=0` only as an error condition to be rejected at parse time. |
| Premature stopping | Stopping on LLR trend before it crosses a bound invalidates the α/β guarantees. The test must run until a bound is crossed or `--max-games` is hit. | The harness must never emit an accept/reject verdict until LLR ∈ (−∞, B] ∪ [A, +∞). Interim LLR values are diagnostics only. |
| Gating bias | SPRT only measures patches that pass — the "you only see what wins" effect. This is inherent to sequential testing, not a bug in the implementation. | Accepted. Document in test records. Run symmetric follow-up tests when a series of accepts accumulates. |
| LLR evaluated per-game vs per-pair | Evaluating LLR after every game (mid-pair) introduces a subtle bias: the pair is not yet complete, so the sample is not i.i.d. at the pair level. | Evaluate LLR only after each completed pair. Never check after an odd game. |
| Mixed-TC note | For mixed-TC SPRT, game outcomes remain i.i.d. as argued in ELOH.D (TC is part of the "game definition"), so the same LLR formula applies without modification. The CI computation is also unchanged. If you want a *per-TC* Elo estimate, use the per-TC subsets independently — but the SPRT verdict uses all pairs together. |

---

## References

- [Chess Programming Wiki — Sequential Probability Ratio Test](https://www.chessprogramming.org/Sequential_Probability_Ratio_Test)
- [Chess Programming Wiki — Match Statistics](https://www.chessprogramming.org/Match_Statistics)
- [vdbergh/pentanomial — SPRT for pentanomial frequencies](https://github.com/vdbergh/pentanomial)
- [vdbergh/simul — multi-threaded pentanomial simulator](https://github.com/vdbergh/simul)
- [fishtest issue #348 — Games in a game pair are highly correlated](https://github.com/glinscott/fishtest/issues/348)
- [Fishtest Mathematics — Statistical Methods in Fishtest](https://official-stockfish.github.io/docs/fishtest-wiki/Fishtest-Mathematics.html)
- [TalkChess — Fixed nodes games and the pentanomial model](https://www.talkchess.com/forum3/viewtopic.php?t=69407)
- [TalkChess — cutechess-cli SPRT: What do elo0 and elo1 mean?](https://talkchess.com/viewtopic.php?t=78272)
- [fastchess man.md](https://github.com/Disservin/fastchess/blob/master/man.md)
- [dogeystamp — Chess engine pt. 3: Elo, and rigorous SPRT testing](https://www.dogeystamp.com/chess3/)
- [dannyhammer — Sequential Probability Ratio Test](https://dannyhammer.github.io/engine-testing-guide/sprt.html)
- Wald, A. (1945). "Sequential Tests of Statistical Hypotheses." *Annals of Mathematical Statistics* 16(2), 117–186.
- Li, Xiaoou et al. "Generalized Sequential Probability Ratio Tests for Separate Families of Hypotheses." [Columbia PDF](https://sites.stat.columbia.edu/jcliu/paper/GSPRT_SQA3.pdf)

---

## TL;DR for the Implementer

- **Pentanomial LLR uses pair scores (0/0.5/1.0/1.5/2.0) as the sample unit.** Compute the mean pair score `mu` and per-pair variance `sigma2` from the bin counts, then plug into the standard GSPRT normal-approximation formula: `LLR = (s1_pair − s0_pair) * (2*mu − s0_pair − s1_pair) / (sigma2/N) / 2`, where `s_i_pair = 2 * LL(elo_i)` and `LL(elo) = 1/(1 + 10^(−elo/400))`.
- **Wald bounds are `B = log(β/(1−α))` and `A = log((1−β)/α)`.** For α=β=0.05: B ≈ −2.944, A ≈ 2.944. Check LLR after each completed pair (never mid-pair).
- **fastchess `elo0=0 elo1=10` with no explicit `model=` uses logistic Elo** (strongly implied default). Match the harness to logistic Elo for back-test calibration. Add `model=logistic` explicitly to the fastchess invocation to remove ambiguity.
- **Pair classification:** add candidate's points from both color games; five bins by total pair score. The middle bin (score=1.0) is the merge of WL, LW, and DD — all three collapse to index 2.
- **For the Δ Elo 95% CI** (the `[+18.18, +65.61]` style output): compute `SE = sqrt(sigma2/N)`, form `[mu±1.96·SE]`, then map both endpoints through the inverse logistic `400·log10((x/2)/(1−x/2))`.
- **Three implementation gotchas:** (a) evaluate LLR only after complete pairs; (b) discard any incomplete pair at the `--max-games` boundary — never force-complete it; (c) add a `--mode=fixed-games` code path distinct from SPRT so callers wanting "just run 400 games and report" never set `elo0=elo1`.
