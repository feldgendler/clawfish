# EPD diagnostic suites — backfill across baseline tags

Deterministic per-position correctness scoring against:

- **WAC** (Win at Chess) — 300 tactical positions, single `bm` annotation. Score = `solved/300`.
- **STS** (Strategic Test Suite) v1 — 1500 positions across 15 strategy themes, weighted `c0` annotations. Score = `total_credit / max_credit` where each position's max credit is typically 10 (sometimes 8). Per-theme breakdown attributes regressions to specific eval components.

**Methodology.** `scripts/epd-suite.sh backfill` builds each baseline tag's binary in a `git worktree`-isolated checkout (mirrors `scripts/sprt.sh`'s pattern) and runs `epd-suite` against both corpora. Each position runs at fixed `movetime=500ms` with `--concurrency 6` (six parallel UCI subprocesses, each owning its own engine; per-position `ucinewgame` clears TT for determinism). Corpus files SHA-256:

- `bench/data/wac.epd`: `9bfb7b42cf5bbc7f3d60ff767ca3be460e0ae4d9110ef9a71ddf0bd7071d4d46`
- `bench/data/sts.epd`: `d7a33ae6cf2fb5f3f18ef1b904cca07dce2cd0458edcf60644f59d4d9e28c07b`

Sources: WAC from [arasan-chess/tests/wacnew.epd](https://github.com/jdart1/arasan-chess/blob/master/tests/wacnew.epd); STS from [fsmosca/STS-Rating/STS1-STS15_LAN_v3.epd](https://github.com/fsmosca/STS-Rating). Both public-domain or community-licensed.

**Caveats.**

- **`baseline/material-greedy` is M3.A** (depth-1 GreedyMover). It accepts `go movetime` syntactically but doesn't budget time — it returns a depth-1 move instantly regardless of TC. The row is meaningful as "what this engine does at its native search shape" but is not TC-comparable to M3.E+ rows.
- **`baseline/alpha-beta-no-tt` is the M3.F commit** (post-M3.E iterative deepening + time management; pre-M4.A TT). The "no-tt" suffix is historical: the tag was created at the moment before the TT was added.
- **M5.E qsearch shape change.** M3.D through M5.D-v2 use the *uncorrected* qsearch; M5.E onward uses the corrected qsearch (single-reply extension, true-stalemate detection, stalemate-conditional under-promo, MAX_PLY ceiling guard). The 16-position bench corpus didn't surface the corner cases, but WAC's 300 tactical positions and STS's 1500 strategic positions are broader corpora — small step changes between M5.D-v2 and M5.E rows may reflect this rather than direct strength signal.
- **STS-Elo regression** uses Swaminathan Natarajan's published formula `Elo ≈ 44.523 · score_pct − 242.85`, where `score_pct` is the engine's percentage of max credit (e.g. a 58.9% scoring engine maps to `44.523 × 58.9 − 242.85 = 2380` STS-Elo). The formula is calibrated for the CCRL band 2000-2800; extrapolation outside degrades. STS systematically underestimates game-playing strength by ~200-300 Elo relative to game-based ratings because it scores strategic correctness only, with no credit for tactical sharpness. The relative ranking across baseline tags is the load-bearing signal; the absolute Elo number is advisory. Source: [STS site](https://sites.google.com/site/strategictestsuite/) and [Chess Programming Wiki](https://www.chessprogramming.org/Strategic_Test_Suite).
- **`baseline/random-mover` is excluded** — score would be ~0/300 by construction (random move from legal moves).

## Results — 2026-05-09

Run config: `--movetime 500 --concurrency 6 --hash 16`. Apple M4 P-cores. All 11 viable baseline tags + HEAD.

| Milestone | Baseline tag | WAC (solved/300) | WAC % | STS (credit/15000) | STS % | STS-Elo est. |
|---|---|---:|---:|---:|---:|---:|
| M3.A (depth-1 GreedyMover) | `material-greedy` | 44 | 14.7% | 2903 | 19.4% | 620 |
| M3.F (pre-TT alpha-beta) | `alpha-beta-no-tt` | 226 | 75.3% | 7433 | 49.6% | 1965 |
| M4.A (TT) | `alpha-beta-tt` | 242 | 80.7% | 7915 | 52.8% | 2107 |
| M4.B (+killers) | `alpha-beta-tt-killer` | 250 | 83.3% | 8114 | 54.1% | 2165 |
| M4.C (+history) | `alpha-beta-tt-killer-history` | 253 | 84.3% | 8126 | 54.2% | 2169 |
| M4.D (+aspiration) | `alpha-beta-tt-killer-history-aspiration` | 253 | 84.3% | 8188 | 54.6% | 2186 |
| M5.A (+NMP) | `m5a-nmp` | **270** | **90.0%** | 8454 | 56.4% | 2266 |
| M5.B (+RFP) | `m5b-rfp` | 268 | 89.3% | 8674 | 57.8% | 2329 |
| M5.C (+LMR) | `m5c-lmr` | 267 | 89.0% | **8803** | **58.7%** | **2369** |
| M5.D (+FFP) | `m5d-ffp` | 263 | 87.7% | 8773 | 58.5% | 2360 |
| M5.E (qsearch correctness) | `m5e-qsearch-correctness` | 263 | 87.7% | 8764 | 58.4% | 2356 |
| HEAD (≡ M5.E engine) | — | 265 | 88.3% | 8832 | 58.9% | 2380 |

**Bold** marks per-suite peak. HEAD's run differs slightly from the M5.E tag's run (+2 WAC, +68 STS credit) within wallclock-budget variance — the engine code is byte-identical from the M5.E tag onward; the only commits added are tooling.

### Observations

**Tactical (WAC) trajectory.** Strong monotonic gain through M3.F → M5.A (+44 to +270 = +226 positions), then plateau-with-mild-recession across M5.B-E. The recession from M5.A's peak (270 → 263 at M5.E) is consistent with each subsequent feature's pruning aggressiveness: RFP, LMR, FFP each accept some risk on tactical lines for a search-speed gain whose Elo benefit dominates the per-position miss rate. Notably, M5.A's NMP delivered the largest single tactical jump in the M5 series (+17 from M4.D → M5.A), which is expected — null-move pruning at depth ≥ 3 with the seven-condition gate dramatically deepens tactical search by skipping unproductive sidelines.

**Strategic (STS) trajectory.** Smoother monotonic gain from M3.A's 19.4% all the way to M5.C's 58.7% peak, with slight recession at M5.D-E. The M3.F → M4.A jump (+482 credit) is the largest single-feature improvement — TT lookups dramatically deepen the search budget. M5.C's LMR added the most M5-era strategic credit (+129 vs M5.B), consistent with LMR's depth-bound selectivity helping positional play more than tactics. M5.D's FFP traded a small STS credit (-30) for the SPRT-validated +53 Elo gain — the speed/depth dividend compensates for the per-position miss rate.

**M5.E qsearch-correctness change is invisible at this corpus.** STS −9 credit, WAC unchanged (using the m5e-tag binary; HEAD's run differs by wallclock noise). M5.E's corner cases (single-reply quiet extension, true-stalemate detection, stalemate-conditional under-promo, MAX_PLY ceiling) are concentrated in endgame patterns that don't dominate the WAC tactical or STS strategic corpora. The M5.E SPRT was inconclusive too (Δ Elo −14.77 [−35.99, +6.33]) — these results corroborate that the M5.E patches are correctness-only on the test surface available to us.

**STS-Elo vs game-based Elo.** HEAD's 58.9% maps to ~2380 STS-Elo, vs the M5.E mixed-TC rating estimate of ~2622 Elo (Stockfish UCI_LimitStrength scale, 2026-05-08). The ~240 Elo gap is consistent with STS's well-known systematic underestimation of game-playing strength: STS scores strategic correctness only and gives no credit for tactical sharpness, so engines with strong tactical search (especially clawfish post-NMP/LMR) outperform their STS-Elo in actual games. The relative ranking across baseline tags is the load-bearing signal — every M3.A→M5.A→M5.C uptick is real.

### Per-theme STS breakdown (HEAD)

15 themes × 100 positions × max-10 each = 1000 max credit per theme.

| # | Theme | Credit / 1000 | % |
|---|---|---:|---:|
| 1 | Undermine | 648 | 64.8% |
| 2 | Open Files and Diagonals | 511 | 51.1% |
| 3 | Knight Outposts/Repositioning/Centralization | 722 | 72.2% |
| 4 | Square Vacancy | 560 | 56.0% |
| 5 | Bishop vs Knight | **737** | **73.7%** |
| 6 | Recapturing | 676 | 67.6% |
| 7 | Offer of Simplification | 568 | 56.8% |
| 8 | AKPC (Advancement of King-side Pawns / Center) | 462 | 46.2% |
| 9 | Advancement of a/b/c pawns | 501 | 50.1% |
| 10 | Simplification | 661 | 66.1% |
| 11 | King Activity | 423 | 42.3% |
| 12 | Center Control | 577 | 57.7% |
| 13 | Pawn Play in the Center | 600 | 60.0% |
| 14 | 7th Rank | 674 | 67.4% |
| 15 | AT (Advanced Tactics) | 512 | 51.2% |

Strongest themes: **Bishop vs Knight** (73.7%) and **Knight Outposts** (72.2%) — both heavily search-driven (the eval doesn't have explicit minor-piece imbalance terms; the engine must compute them dynamically). Weakest: **King Activity** (42.3%) and **AKPC** (46.2%) — both are pawn-structure / king-safety patterns where PeSTO MG-only PSTs have known gaps (no tapered eval; no king-shelter or pawn-storm terms; no minor-piece coordination evaluation). These themes will be the natural empirical anchor for M6 eval improvements: each new eval term should defend itself via per-theme score change on the relevant group.

Full per-theme tables for older baselines: `target/epd-suites/backfill/<tag>-sts.txt`.

## Reproducing

```sh
scripts/epd-suite.sh backfill                       # all baseline tags
EPD_MOVETIME=500 EPD_CONCURRENCY=6 \
    scripts/epd-suite.sh backfill                   # explicit knobs
EPD_LIMIT=20 scripts/epd-suite.sh backfill <tag>    # smoke a single tag
```

Output lands in `target/epd-suites/<TS>/`:
- `<slug>-wac.{stdout,stderr,txt}` — full per-position trace + summary.
- `<slug>-sts.{stdout,stderr,txt}` — same for STS.
- `INDEX` — one-line digest per (tag, suite).
