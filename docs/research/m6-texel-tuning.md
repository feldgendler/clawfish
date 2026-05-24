# M6.I — Texel Tuning Mechanics for HCE

**Scope:** Optimizer choice, K-fitting, regularization, non-linear term handling, and train/validation discipline for the M6.I single joint-Texel pass. Does NOT duplicate corpus depth, sampling, or construction material from `m6.i-texel-corpus-depth.md`, `texel-position-sampling.md`, and `m6-corpus-construction.md`.

**Restriction honored:** No engine source repositories consulted. All sources are CPW articles, TalkChess threads, and blog posts.

---

## 1. Optimizer Choice

### Candidates

| Optimizer | Mechanism | Cost per iteration | Scales with N params? |
|---|---|---|---|
| Classic Texel coordinate descent | ±1 per param, accept if loss improves | O(N) evaluations | Linear — 150 params × 2 = 300 eval calls/pass |
| Full-batch gradient descent + Adam | Closed-form gradient, per-position feature vector, one eval call per position per epoch | O(positions) | Constant in N |
| SPSA | Two simultaneous random perturbations per iteration | O(2) | Constant in N — but noisy |

### Closed-form gradient for linear HCE

When every eval term is **linear in the tunable weights** (PSQT, mobility bonus tables, passed-pawn rank bonuses, etc.), the MSE-of-sigmoid loss has an analytic gradient:

```
∂E/∂w_j = (2/N) · Σ_i [ (σ_i − result_i) · σ_i · (1−σ_i) · K · x_{ij} ]
```

where `σ_i = sigmoid(K · eval(pos_i))` and `x_{ij}` is the feature coefficient for weight `j` at position `i`. The `x_{ij}` are position-specific constants (e.g., number of legal knight moves); they can be **traced once** — computed on a single forward pass and cached — so subsequent gradient passes require no additional eval calls. This is the basis for Andrew Grant's "traced evaluation" in Ethereal. [TalkChess — Texel tuning speed, Grant](https://www.talkchess.com/forum3/viewtopic.php?t=68326)

### Practitioner evidence

- **Andrew Grant (Ethereal):** Uses batch gradient descent with Adam. Compared to AdaGrad: "Adam seemed miles above AdaGrad for HalfKP problems." With Adam (LR=0.001, batch=16384), queen-value recovery runs at ~+3 cp/epoch vs ~+1 cp/epoch under AdaGrad. Convergence in "less than 10,000 iterations, no more than a couple minutes on a 32-thread machine." [TalkChess — Some more HCE Texel Tuning Data](https://talkchess.com/viewtopic.php?t=77502)
- **Jon Dart (Arasan):** Batch gradient descent + Adam. 750 parameters × 10M positions converge in ~130 iterations, ~1.5–2 h on 24-core. [TalkChess — Texel tuning speed](https://www.talkchess.com/forum3/viewtopic.php?t=68326)
- **Joost Buijs (Rebel):** Plain gradient descent. ~400 terms × 7.5M positions, ~100–200 iterations, ~2 h on a 6950X. [TalkChess — Texel tuning speed](https://www.talkchess.com/forum3/viewtopic.php?t=68326)
- **Invictus (Edsel Apostol):** SGD with Adam. [TalkChess — Some more HCE Texel Tuning Data](https://talkchess.com/viewtopic.php?t=77502)
- **Weiss:** Adam, added on Grant's suggestion. [TalkChess — Tuning parameters](https://talkchess.com/viewtopic.php?t=83977)
- **Classic coordinate descent (CPW canonical):** Österlund's original; terminates in finite time but "VERY slow" in practice — hours to days at the scale we need. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method); [chess4j blog](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)
- **SPSA:** Generic, no gradient needed, O(2) per step — but "very hyperparameter-dependent" and can "forget less important parameters" due to sign-only gradient estimates. Not preferred over Adam when the gradient is analytically available. [TalkChess — A hybrid of SPSA and local optimization](https://talkchess.com/viewtopic.php?t=77420); [CPW — SPSA](https://www.chessprogramming.org/SPSA)

### Gotchas

- **Adam default LR too small:** With LR=0.001, some implementors reported ~1 cp/100 epochs. Root cause traced to wrong K, not the optimizer — once K was corrected, convergence jumped to normal rate. [TalkChess — Some more HCE Texel Tuning Data](https://talkchess.com/viewtopic.php?t=77502)
- **Non-convexity:** The MSE-of-sigmoid loss is smooth but non-convex in the weights (K introduces curvature). Coordinate descent and Adam both find local optima; in practice the landscape is benign enough for HCE that practitioners report no multi-modal trapping issues.
- **Feature sparsity:** At most positions, most feature coefficients `x_{ij}` are zero. Using a sparse feature representation per position is "the biggest improvement" for large-corpus tuning throughput. [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)

### Recommendation

Use **full-batch gradient descent with Adam** (LR ≈ 0.001, β₁=0.9, β₂=0.999). Trace features once per corpus load. This is the consensus choice among modern HCE practitioners; coordinate descent is for prototyping only at this parameter scale.

---

## 2. K Sigmoid Fitting

### The standard procedure

1. **Fit K once, before any weight optimization**, by 1-D minimization of MSE over the corpus with all weights held at their starting values. Österlund uses this explicitly: "Compute the K that minimizes E. K is never changed again by the algorithm." [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
2. The 1-D minimization over K is unimodal (MSE is convex in K for fixed weights) — **ternary search or golden-section search** converge in O(log(range/ε)) evaluations and are the standard approach. [TalkChess — Texel tuning method question p.4](https://www.talkchess.com/forum/viewtopic.php?t=64189&start=36)
3. Freeze K and run all weight optimization passes against the frozen K.

### When to refit K

| Event | Refit K? | Rationale |
|---|---|---|
| Initial setup | Yes — mandatory | K is undefined until fitted |
| Weight optimization iterations | No | K frozen |
| Corpus replaced (new source, new filter, new mix) | Yes | K is corpus-specific; CCRL corpus produced K≈0.24, Lichess K≈1.09 in reported experiments |
| Eval function structure changes substantially | Yes | Centipawn scale shifts with new terms; K absorbs the scale |
| Same corpus, iterated tuning passes | No, unless convergence stalls | K refit optional diagnostically if loss plateau suggests wrong scale |

- Österlund: K was 1.13, never changed. [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- Apostol: initial tuning stalled because K was wrong; fixing K "2 to 3 times higher" restored convergence. [TalkChess — Some more HCE Texel Tuning Data](https://talkchess.com/viewtopic.php?t=77502)

### K and centipawn scale

- K is a unitless scaling constant that maps centipawn scores onto the sigmoid's active range.
- A typical HCE with pawn ≈ 100 cp and K ≈ 1.0 maps ±300 cp to sigmoid ≈ [0.05, 0.95] — reasonable dynamic range.
- K < 0.1 (near-zero) means the eval scores are nearly uncorrelated with game results — indicates a parsing bug, wrong eval perspective, or mismatched position/result pairing. [TalkChess — Weird Results from Texel Tuning](https://talkchess.com/viewtopic.php?t=83196)
- Different corpora yield different K. A mixed Lichess+CCRL corpus for clawfish should be fitted fresh.

---

## 3. Regularization for Sparse and Monotonic Weight Tables

### The degeneracy problem

The Blunder (algerbrex) experiments found that mobility table weights were driven to 0 or 1 by the optimizer: "the mobility parameters…are driven from the current values to one or zero." This happened despite mobility being "worth ~50–60 Elo." The root cause is sparse training signal: quiet positions where a specific piece type has many legal moves appear in a small fraction of the corpus, giving the optimizer little gradient signal for high-index mobility table entries. [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)

The king-safety table is worse: quiet positions (by definition, without tactical threats) have **systematically weak king attacks**. The high-attack-count entries of the king-safety S-curve receive almost no gradient signal from a quiet-position corpus — a structural blind spot the quiet filter itself creates.

### Mitigations practitioners use

| Technique | Mechanism | Published source |
|---|---|---|
| **L1 regularization** | Penalty ∝ \|w_j\| — drives sparse entries toward zero rather than extremes | Zurichess (Mosoi): "for Zurichess I added L1" [Zurichess Medium blog](https://medium.com/@brtzsnr/hi-all-a73c1b7b7a73) |
| **L2 ridge toward zero** | Penalty ∝ w_j² | CPW [Automated Tuning](https://www.chessprogramming.org/Automated_Tuning) |
| **L2 ridge toward init (prior)** | Penalty ∝ (w_j − w_j^init)² | First-principles extension; prevents degenerate-from-reasonable |
| **Hard bounds on individual params** | Clamp/reject values outside plausible range | "you should probably never have passed pawn award that is negative!" [TalkChess — Texel tuning questions](https://talkchess.com/viewtopic.php?t=77964) |
| **Section-based tuning** | Tune king safety in isolation; combine after | Apostol: "better when tuning just a few parameters at a time (e.g. just king safety)" [TalkChess](https://talkchess.com/viewtopic.php?t=77502) |
| **Literature-init + minimal perturbation** | Start from literature values; L2 toward init | General practice; reduces effective DoF for low-signal entries |

### Monotonicity and table smoothing

- Mobility tables should be monotonically non-decreasing. This is a known-correct prior.
- Post-tuning monotonicity projection after each update is defensible but not well-documented in the HCE literature.
- **Cleanest approach:** L2 toward init for the tables, combined with hard bounds (min 0, max reasonable), prevents extreme values without explicit monotonicity logic.

### For king-safety specifically

- High-index S-curve entries (attack count ≥ 5–6) appear in almost no quiet positions.
- Practitioners either: (a) **exclude king-safety table entries from the joint gradient pass and tune them separately** via a gauntlet or coordinate sweep; (b) freeze the table shape and tune only an overall king-safety scalar multiplier; (c) apply strong L2 ridge toward init so sparse entries barely move.
- No consensus. Options (a)/(c) are the simplest to implement correctly. [TalkChess — Evaluation & Tuning in Chess Engines](https://talkchess.com/viewtopic.php?t=74877)

---

## 4. Handling Non-Linear Eval Terms

### Two constructs in clawfish M6.F

**A. Endgame draw-scale (bilinear):** `final = blended × scale / DEN`. If `scale` is a linear function of piece counts, `final` is bilinear in (scale params, term weights). A single Adam pass would need the product rule — no longer linear-gradient in either set alone.

**B. King-safety attacker-count S-curve (index-nonlinear):** `idx = Σ_k multiplier_k × count_k(pos)`. If the `multiplier_k` are tunable, the index is itself a function of tunable params, making `table[idx]` non-differentiable in the multipliers.

### What practitioners do

| Approach | Who / source | Applicability |
|---|---|---|
| **Freeze structural/non-linear sub-params; tune linear part only** | Grant: framework "only works for linear terms"; king-safety/complexity excluded | Most common for gradient tuners |
| **Compute gradient for non-linear terms explicitly** | Dart (Arasan): "non-trivial but doable" — never detailed | Rare; custom hand-derivation |
| **Coordinate-sweep outer loop over the few non-linear params** | mACE: GA for king-safety table only (separate pass) [mACE](http://macechess.blogspot.com/2013/10/king-safety-tuning.html) | Practical: ~5–10 params × coarse grid |
| **Tune non-linear sub-terms in isolation after joint linear pass** | Apostol section-by-section [TalkChess](https://talkchess.com/viewtopic.php?t=77502) | Alternating passes; slow |

### Recommendation

**Freeze the non-linear sub-parameters (per-kind attacker multipliers; draw-scale coefficients) at fixed values for the joint pass. Tune only the linear table entries/weights in the Adam gradient pass.** After convergence, run a small coordinate sweep (±step on each of the ~5–10 scalar params, linear weights frozen) to check for gross miscalibration. This is what the community actually does; the alternative (full non-linear gradients) is documented as "non-trivial" and never publicly detailed.

---

## 5. Train/Validation Split and Early Stopping

### Is a held-out split standard for HCE Texel?

Mixed evidence:

| Practitioner / engine | Validation split? | Stopping criterion |
|---|---|---|
| Österlund (canonical) | No — all positions train | No improvement on training loss across a full cycle |
| chess4j (Swafford) | Yes — 80/20; early stop on val MSE | Learning-curve divergence [chess4j](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/) |
| Leorik (Jahn) | Yes — 4M active of 50M; MSE on full set | "update best coefficients only when MSE on the total set improves" [TalkChess](https://talkchess.com/viewtopic.php?t=79049&start=420) |
| TalkChess (2.5M Lichess) | Not described | Overfit past ~40 iters: "training MSE improves…playing strength degrades" [TalkChess](https://talkchess.com/viewtopic.php?t=77502) |
| algerbrex (Blunder) | No | Iteration count + gauntlet |

### Key observations

- **Overfitting occurs on a fixed corpus** (the 2.5M case: loss down, strength down). Held-out validation catches it; training-loss-only does not.
- **chess4j reports L2 regularization showed NO improvement** ("lambda=0 outperformed all alternatives") — suggesting validation + early stopping may matter more than regularization for overall overfitting. [chess4j](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)
- **Game-level split is mandatory** (see `texel-position-sampling.md` §6) — position-level split contaminates val with same-game same-label positions.
- **Patience / best-checkpoint restore:** Jahn's "update only when total-set MSE improves" is a best-checkpoint pattern. Formal patience (stop after N non-improving epochs) is standard ML practice.

### The integer-quantization floor

HCE weights are integers. Once `round(w_float)` is identical to the previous epoch's rounded weights across all params, further training-loss reduction is **deployment-irrelevant** — the engine's eval is unchanged. This is the natural hard stop for an integer-weight HCE tuner. (Not stated explicitly in any chess source, but follows from the deployment target.)

---

## Recommendations for clawfish M6.I

### Optimizer
- **Full-batch Adam** (LR=0.001, β₁=0.9, β₂=0.999, ε=1e-8). Trace feature coefficients once per corpus load; reuse across epochs. Sparse feature vectors. Full corpus in RAM if it fits, else mini-batches ≥16K.

### K-fit cadence
- Fit K once via ternary/golden search over a sane range before any weight update; freeze for all Adam iterations. Refit only on corpus/mix change (so per mixture candidate) or eval structure change. K < 0.1 on initial fit ⇒ stop and investigate (bug signal).

### Regularization
- Linear tables (mobility, passed-pawn, CONN): L2 ridge toward **init**, small λ (≈1e-4–1e-3). Hard bounds where a sign is known.
- King-safety: high-index entries are essentially unidentifiable from a quiet corpus — **exclude them or anchor strongly to init**. (clawfish M6.I: exclude the S-curve + attacker multipliers from the linear tune; tune only shield + open-file, which appear in quiet positions.)
- Note chess4j's finding: don't over-invest in regularization; validation early-stopping is the primary overfitting control.

### Non-linear terms
- Freeze per-kind attacker multipliers + draw-scale coefficients for the joint Adam pass. Optional coarse outer coordinate sweep afterward, linear weights frozen.

### Train/validation + stopping
- Game-level 80/20 split (assign whole games before splitting). Track train + val MSE per epoch; patience = 5 on val MSE; restore best-val weights. Apply the integer-quantization floor as a hard stop. Don't rely on a fixed iteration count.

---

## Sources

- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [CPW — Automated Tuning](https://www.chessprogramming.org/Automated_Tuning)
- [CPW — SPSA](https://www.chessprogramming.org/SPSA)
- [TalkChess — Some more HCE Texel Tuning Data (Grant)](https://talkchess.com/viewtopic.php?t=77502)
- [TalkChess — Texel tuning speed](https://www.talkchess.com/forum3/viewtopic.php?t=68326)
- [TalkChess — Evaluation & Tuning in Chess Engines](https://talkchess.com/viewtopic.php?t=74877)
- [TalkChess — Experiments in generating Texel Tuning data p.2](https://talkchess.com/viewtopic.php?t=78536&start=10)
- [TalkChess — Texel tuning method question p.4](https://www.talkchess.com/forum/viewtopic.php?t=64189&start=36)
- [TalkChess — Weird Results from Texel Tuning](https://talkchess.com/viewtopic.php?t=83196)
- [TalkChess — Tuning parameters](https://talkchess.com/viewtopic.php?t=83977)
- [TalkChess — A hybrid of SPSA and local optimization](https://talkchess.com/viewtopic.php?t=77420)
- [TalkChess — Leorik devlog p.43](https://talkchess.com/viewtopic.php?t=79049&start=420)
- [TalkChess — Texel tuning questions](https://talkchess.com/viewtopic.php?t=77964)
- [chess4j — Automated Parameter Tuning](https://jamesswafford.dev/automated-parameter-tuning-in-chess4j/)
- [mACE Chess — King safety tuning](http://macechess.blogspot.com/2013/10/king-safety-tuning.html)
- [Zurichess evaluation improvements (Mosoi)](https://medium.com/@brtzsnr/hi-all-a73c1b7b7a73)
