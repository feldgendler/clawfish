# SPRT — M7.B (qsearch SEE-pruning) vs M5.K — 2026-06-16

**Verdict: SHIP (rung-1 by aggregate CI), user-approved "ship now, tune later".**

- **Candidate:** M7.B (`011b0e2`) — qsearch SEE-pruning (skip `see < 0` non-promo
  captures when not in check; ADR-0040).
- **Baseline:** `M5.K` worktree (`5773091`) — production HEAD behaviour (M6.J eval
  + M5.K search; ≡ `M7.A-infra` since the M7.A SEE evaluator is inert). The only
  behavioural delta candidate↔baseline is the qsearch prune.
- **Shape:** mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` + `--virtual-clock`,
  elo0=0 / elo1=5, alpha=beta=0.05, seed `0xC1ABF15AE10DD00B`, concurrency 6,
  background QoS. 400 games (cap).

## Result

| Metric | Value |
|---|---|
| **Δ Elo (pentanomial CI)** | **+40.13 [+12.97, +67.80]** (200 pairs) |
| ptnml | [12, 42, 64, 52, 30] |
| W-L-D | 138-92-170 (53.5% score) |
| GSPRT | verdict=continue, llr=0.99 (reason=max-games — did not reach the formal H1-accept LLR before the 400-game cap) |

**Ship rationale:** CI-lower **+12.97 > 0** ⇒ **rung-1 ship-by-CI** (ADR-0037 §9;
the M6.I/M6.J "continue-at-cap + positive pentanomial CI" ship precedent). Largest
single-change gain since M6.I; confirms SEE's Elo is in *pruning*, not ordering
(M7.A's ordering split was flat — ADR-0039).

## Per-TC breakdown (the caveat — INVERSE depth profile)

| TC | W-L-D (n) | score | ≈ Elo |
|---|---|---|---|
| 10+0.1 | 42-17-23 (82) | 65.2% | +109 |
| 20+0.2 | 45-23-38 (106) | 60.4% | +73 |
| 40+0.4 | 41-10-51 (102) | 65.2% | +109 |
| **60+0.6** | **10-42-58 (110)** | **35.5%** | **≈ −104** |

Pruning **dominates at fast/mid TC** (the 40% node saving buys large extra depth
when the budget is tight) but **clearly regresses at the slowest TC** (10W vs 42L,
≈4.4σ on the W−L difference — not noise). This is the *inverse* of M6.I's
depth-amplifying profile, and 60+0.6 is the TC most relevant to the eventual
GM-strength goal.

**Hypothesised cause:** `QS_SEE_PRUNE_THRESHOLD = 0` (prune *all* `see < 0`
captures) is too aggressive at depth — at slow TC the engine has the budget to
refute "statically losing" captures that are real tactical shots, so pruning them
costs accuracy. The §10 threshold lever (a negative margin, e.g. `see < −100` —
prune only *clearly*-losing) should recover slow-TC while retaining most of the
fast-TC gain.

**Follow-up:** M7.B.1 — negative-threshold tune (HIGH priority,
`docs/tuning-backlog.md`). Ships now on the +40 aggregate; the slow-TC regression
is tuned as a fast-follow per the user's call.

Run dir: `target/matches/sprt/20260616T043622-M5.K-sprt/`.
