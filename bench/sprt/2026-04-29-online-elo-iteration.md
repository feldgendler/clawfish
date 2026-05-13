# Online Elo iteration — M3.F (HEAD) self-paced bisection vs Stockfish

**Date:** 2026-04-29
**Outcome:** Final converged estimate **~2114 Elo at tc=10+0.1**, anchored to Stockfish UCI_Elo (CCRL 40/4 calibrated). 95% CI ~±35 Elo from the iterator's tail variance.

## Why this run

The earlier batched-anchor estimates ([1320 anchor](2026-04-29-rating-estimate.md), [1996 anchor](2026-04-29-rating-estimate-cross-validation.md), 2102 anchor, 2225 anchor, 2171 anchor) gave inconsistent point estimates (1996, 2102, 2189, 2158, 2145) — the spread was driven by:

- **Saturation at the 1320 anchor** (98% score → high-variance logistic Elo).
- **Stockfish's UCI_Elo curve non-linearity** (sharp transition zone in 2200-2400 → log-Elo derivation depends on which side of the transition the anchor sits).
- **Hardware/scheduling noise** — both engines competed for cores at concurrency=6, leading to non-deterministic per-game compute allocation.

The online iterator (chess.com-style: each batch nudges the estimate via Elo formula, Stockfish reconfigured between batches) converges past these issues by self-correcting toward 50% as the estimate approaches truth. Combined with **clawfish-pinned-to-P-cores** (Apple M4: 4 P + 6 E; clawfish gets `taskpolicy -c utility` QoS hint), the per-game noise drops sharply.

## Command

```sh
scripts/elo-iterate.sh 2180 30 4
```

(Initial estimate 2180 from the prior batched-anchor work; 30 batches × 4 games each = 120 games; concurrency=4 = one P-core per clawfish instance.)

## Method

- **Robbins-Monro decaying K-factor**: `K_t = max(8, round(40 / (1 + t/10)))`. K=40 for first ~10 batches (fast initial convergence), decaying to K~10 at batch 30 (stable plateau).
- **Per-batch update**: `ΔR = K × (avg_score - 0.5)` over the 4 games in the batch.
- **Stockfish's UCI_Elo set to `round(current_estimate)` each batch.**
- **clawfish on P-cores** (utility QoS); Stockfish on default scheduler (no QoS hint). Justification in `docs/tooling-backlog.md` "Hardware-invariant TC" entry — at our operating point (UCI_Elo ~2100), Stockfish's depth cap fits within 10+0.1 even on E-cores, so its calibrated effective strength is preserved.
- **Symmetric TC = 10+0.1** for both engines (asymmetric TC unsupported by fastchess; future custom harness will allow Stockfish-with-unlimited-TC for cleaner anchoring).
- **Adjudication**: same as `scripts/sprt.sh` — `-resign movecount=3 score=600 -draw movenumber=34 movecount=8 score=20 -maxmoves 200`.

## Trajectory

Estimate started at 2180, descended to a plateau around 2100-2115 Elo over batches 15-29:

| Batch | K | Estimate |
|---|---|---|
| 0 | 40 | 2180 (seed) |
| 5 | 27 | 2160 |
| 10 | 20 | 2128 |
| 15 | 16 | 2110 |
| 20 | 13 | 2101 |
| 25 | 11 | 2104 |
| **29** | **10** | **2114** |

Tail-10-batch range: 2098–2114 Elo (16 Elo spread). σ ≈ 5 Elo over the tail. 95% CI ~±35 Elo via standard estimator (`σ_total ≈ K × σ_score / sqrt(2N)` for batched updates).

## Final estimate

**clawfish M3.F end ≈ 2100-2150 Elo at tc=10+0.1**, point estimate **~2114** with 95% CI ~±35 Elo.

## Comparison to batched anchors

| Method | Result | Note |
|---|---|---|
| Saturated 1320 anchor | 1996 | Unreliable (98% score, logistic saturation) |
| 1996 anchor (logistic) | 2102 | Well-conditioned but Stockfish UCI_Elo non-linearity |
| 2102 anchor (logistic) | 2189 | Same |
| 2171 anchor (logistic) | 2145 | Same |
| 2225 anchor (logistic) | 2158 | Same |
| **Online iterator (P-core pinned)** | **2114** | Self-correcting, P-core clean |

The online iterator's number is the most trustworthy because:
1. Self-corrects toward 50% — no saturation issues.
2. Many anchor settings get sampled — averages over Stockfish UCI_Elo non-linearity.
3. Clawfish runs at consistent (full P-core) speed each batch — no per-batch hardware noise.

## Caveats

- **TC-specific**: estimate at `tc=10+0.1`. Slower TC would likely shift the number; CCRL 40/4 standard would probably show clawfish ~50-150 Elo higher.
- **Anchor-pool-specific**: Stockfish UCI_Elo is anchored to CCRL 40/4 calibration. Inherits any drift in that calibration (see `docs/tooling-backlog.md` discussion of pool stability).
- **No transposition table**: clawfish at M3.F end has no TT. M4.A's TT will materially shift this number upward (typical TT-vs-no-TT delta is +50-150 Elo).
- **Within-project deltas remain trustworthy** even if the absolute number drifts. The point of the `M3.F` tag (created at this commit) is to give M4+ SPRTs a frozen reference point that doesn't depend on absolute Elo accuracy.

## Methodology lessons

The path to a converged estimate took several iterations. Lessons captured in `docs/tooling-backlog.md`:

1. **Single-anchor logistic Elo at saturated scores is unreliable.** Always cross-validate with a balanced anchor.
2. **Stockfish's UCI_Elo curve is non-linear.** The 2200-2400 transition zone makes single-anchor estimates noisy.
3. **Online iteration with K-decay converges past these.** Each batch self-corrects.
4. **CPU-time-based TC (or `go nodes`, with caveats) would eliminate hardware noise.** The custom harness path lays this out for M4+.
5. **Adaptive batch sizing**: coarse batches when far from 50%, finer batches at convergence — saves total games for the same final precision.

## Artifacts

- Per-batch summary: `target/matches/sprt/20260429T160956-elo-iterate.summary`
- Per-batch PGNs: `target/matches/sprt/20260429T160956-elo-iterate/batch-{0..29}.pgn`

(Raw artifacts gitignored under `target/`. This summary is the committed record.)
