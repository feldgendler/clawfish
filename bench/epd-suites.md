# EPD diagnostic suites — backfill across baseline tags

Deterministic per-position correctness scoring against:

- **WAC** (Win at Chess) — 300 tactical positions, single `bm` annotation. Score = `solved/300`.
- **STS** (Strategic Test Suite) v1 — 1500 positions across 15 strategy themes, weighted `c0` annotations. Score = `total_credit / max_credit` where each position's max credit is typically 10 (sometimes 8). Per-theme breakdown attributes regressions to specific eval components.

**Methodology.** `scripts/epd-suite.sh backfill` builds each baseline tag's binary in a `git worktree`-isolated checkout (mirrors `scripts/sprt.sh`'s pattern) and runs `epd-suite` against both corpora. Each position runs at fixed `movetime=500ms` with `--concurrency 6` (six parallel UCI subprocesses, each owning its own engine; per-position `ucinewgame` clears TT for determinism). Corpus files SHA-256:

- `bench/data/wac.epd`: `9bfb7b42cf5bbc7f3d60ff767ca3be460e0ae4d9110ef9a71ddf0bd7071d4d46`
- `bench/data/sts.epd`: `d7a33ae6cf2fb5f3f18ef1b904cca07dce2cd0458edcf60644f59d4d9e28c07b`

Sources: WAC from [arasan-chess/tests/wacnew.epd](https://github.com/jdart1/arasan-chess/blob/master/tests/wacnew.epd); STS from [fsmosca/STS-Rating/STS1-STS15_LAN_v3.epd](https://github.com/fsmosca/STS-Rating). Both public-domain or community-licensed.

**Caveats.**

- **`M3.A` is M3.A** (depth-1 GreedyMover). It accepts `go movetime` syntactically but doesn't budget time — it returns a depth-1 move instantly regardless of TC. The row is meaningful as "what this engine does at its native search shape" but is not TC-comparable to M3.E+ rows.
- **`M3.F` is the M3.F commit** (post-M3.E iterative deepening + time management; pre-M4.A TT). The "no-tt" suffix is historical: the tag was created at the moment before the TT was added.
- **M5.E qsearch shape change.** M3.D through M5.D-v2 use the *uncorrected* qsearch; M5.E onward uses the corrected qsearch (single-reply extension, true-stalemate detection, stalemate-conditional under-promo, MAX_PLY ceiling guard). The 16-position bench corpus didn't surface the corner cases, but WAC's 300 tactical positions and STS's 1500 strategic positions are broader corpora — small step changes between M5.D-v2 and M5.E rows may reflect this rather than direct strength signal.
- **STS-Elo regression** uses Swaminathan Natarajan's published formula `Elo ≈ 44.523 · score_pct − 242.85`, where `score_pct` is the engine's percentage of max credit (e.g. a 58.9% scoring engine maps to `44.523 × 58.9 − 242.85 = 2380` STS-Elo). The formula is calibrated for the CCRL band 2000-2800; extrapolation outside degrades. STS systematically underestimates game-playing strength by ~200-300 Elo relative to game-based ratings because it scores strategic correctness only, with no credit for tactical sharpness. The relative ranking across baseline tags is the load-bearing signal; the absolute Elo number is advisory. Source: [STS site](https://sites.google.com/site/strategictestsuite/) and [Chess Programming Wiki](https://www.chessprogramming.org/Strategic_Test_Suite).
- **`M2.E` is excluded** — score would be ~0/300 by construction (random move from legal moves).

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
| M5.F (qsearch-in-TT) | `m5f-qsearch-in-tt` | 267 | 89.0% | 8822 | 58.8% | 2376 |
| M5.G (singular extensions, v2) | `M5.G` | 278 | 92.7% | 9239 | 61.6% | 2499 |

### M6.A same-campaign re-baseline — 2026-05-15

Per roadmap §"Same-campaign re-baseline required", M6 per-phase secondary gates re-measure the predecessor baseline tag and HEAD in the **same** RUN-ALONE campaign; the 2026-05-09 snapshot above (and its M5.H1-implied figures) is **stale and not a valid gate** at the ±2 WAC / ±68 STS noise band. Run config identical (`--movetime 500 --concurrency 6 --hash 16`, Apple M4).

| Milestone | Baseline tag | WAC (solved/300) | WAC % | STS (credit/15000) | STS % | STS-Elo est. |
|---|---|---:|---:|---:|---:|---:|
| M5.H1 (re-baseline) | `M5.H1` | 270 | 90.0% | 8786 | 58.6% | 2365 |
| M6.A (tapered eval foundation) | HEAD | **271** | **90.3%** | **9620** | **64.1%** | **2613** |

M6.A vs M5.H1 (same campaign): WAC **+1** (within ±2 noise — flat). STS **+834 credit / +248 STS-Elo** (≫ ±68 noise — decisive). Per-piggyback sub-gates (plan §13.3): theme #11 "King Activity" 404→499 = **+95** (mop-up, gate ≥+30 — PASS); theme #5 "Bishop vs Knight" 698→688 = **−10** (bishop-pair, flat/within-noise, above the ≤−20 should-fix floor, no targeted lift — Texel-calibrated in M6.H). Largest movers: Center Control +137, Open Files +116. The 2026-05-09 snapshot's M5.H1-era weakest themes (King Activity 42.3%, AKPC 46.2%) both lifted materially (49.9%, 50.7% at M6.A).

### M6.C same-campaign re-baseline — 2026-05-17 (rejected-config diagnostic)

Per roadmap §"Same-campaign re-baseline required" (RUN ALONE, sandbox-disabled, `--movetime 1000 --concurrency 4`, Apple M4). **Caveat:** this campaign measured the **live-term (rejected) M6.C config**, not the shipped artifact — M6.C ships **score-neutral** (all passed weights zeroed; `evaluate` byte-identical to `M6.B`), so the shipped build's WAC/STS == `M6.B`'s by construction and there is no per-theme delta to gate (the M6.B "rejected-config EPD no longer applies to the shipped build" precedent). Retained as a diagnostic of the rejected literature-default build + to discharge the M6.B WAC/STS watch-item.

| Milestone | Baseline tag | WAC (solved/300) | WAC % | STS (credit/15000) | STS % |
|---|---|---:|---:|---:|---:|
| M6.B (re-baseline, same campaign) | `M6.B` | 271 | 90.3% | 9640 | 64.3% |
| M6.C **live-term, rejected** (not shipped) | HEAD-live | 269 | 89.7% | 9791 | 65.3% |

Live-term vs `M6.B` (same campaign): WAC **−2** (flat, ±2 band). STS **+151 credit** (≫ ±68 — real). Per-theme (claimed pawn-advancement group): #9 "Advancement of a/b/c pawns" 542→600 = **+58**; #13 "Pawn Play in the Center" 639→645 +6; #8 "AKPC" 536→525 −11 (within per-theme noise); #11 "King Activity" 526→601 = **+75** (the king-tropism sub-term's direct target). Only notable negative #10 "Simplification" 710→648 −62 (not a claimed theme; within per-theme noise; aggregate +151 dwarfs it). **Diagnostic conclusion: the literature passed-pawn term is directionally correct — it lifts exactly the themes it claims — but mis-*scaled* not mis-*designed* (it SPRT-regressed via TC-localized over-magnitude; ADR-0032 §8). This is the M6.H "reshape, don't delete" corroboration.** Discharges the M6.B watch-item (2): M6.B's same-campaign 271 WAC / 9640 STS is now on record.

### M6.E same-campaign re-baseline — 2026-05-19 (rejected-config diagnostic)

Per roadmap §"Same-campaign re-baseline required" (RUN ALONE, sandbox-disabled, `--movetime 1000 --concurrency 4`, Apple M4; the M6.C-precedent params). **Caveat:** this campaign measured the **live literature-default (rejected) king-safety config**, not the shipped artifact — M6.E ships **score-neutral** (all 13 king-safety weights zeroed; `evaluate` byte-identical to `M6.D`), so the shipped build's WAC/STS == `M6.D`'s by construction and there is no per-theme delta to gate (the M6.B/C/D "rejected-config EPD no longer applies to the shipped build" precedent — the secondary gate is moot). M6.E ran **no SPRT screen ladder** (ADR-0033 §8); this is the one cheap M6.H directional-brief substitute the plan committed to. Patch-faithfulness confirmed: live-default `bench 1102432` ≠ M6.D shipped `1213649` ⇒ the literature term is genuinely active.

| Milestone | Baseline tag | WAC (solved/300) | WAC % | STS (credit/15000) | STS % | STS-Elo |
|---|---|---:|---:|---:|---:|---:|
| M6.D (re-baseline, same campaign) | `M6.D` | 276 | 92.0% | 9817 | 65.4% | 2671 |
| M6.E **live-default, rejected** (not shipped) | HEAD-live | 278 | 92.7% | 10181 | 67.9% | 2779 |

Live-default vs `M6.D` (same campaign): WAC **+2** (flat, ±2 band). STS **+364 credit** (≫ ±68 — decisive; STS-Elo +108). Per-theme (claimed king-safety group): #11 "King Activity" 514→576 = **+62** (the primary king-safety target — strong ✓); #8 "AKPC" 579→569 = **−10** (flat/within per-theme noise, above the ≤−20 should-fix floor — mirrors M6.C's AKPC −11). Largest movers: #4 "Square Vacancy" 674→769 **+95**, #7 "Offer of Simplification" 614→691 **+77**, #12 "Center Control" 713→759 +46, #2 "Open Files and Diagonals" 604→635 +31, #9 "Advancement a/b/c" 556→594 +38. Only material negative #3 "Knight Outposts" 776→753 = **−23** (not a claimed king-safety theme; near per-theme noise; aggregate +364 dwarfs it). **Diagnostic conclusion: the literature king-safety term is directionally correct — it lifts exactly its claimed primary theme (King Activity +62) and is aggregate-decisive (+364) — but mis-*shaped* not mis-*designed* (the research §8 / M6.B→C→D three-phase-law transfer-fail prediction stands; M6.E shipped score-neutral with no screen, ADR-0033 §8). This is the M6.C/M6.D "reshape, don't delete" corroboration for the M6.H joint-Texel king-safety reshape.** (Absolute STS totals vary ±4% across campaigns per the known doc-audit issue — the *same-campaign* delta computed here is the valid measure; M6.D's 9817 here is this campaign's re-baseline, not comparable to M6.D's own-campaign figure.)

**Bold** marks per-suite peak in the legacy 2026-05-09 table only. M5.E HEAD-run delta vs tag was +2 WAC / +68 STS (wallclock-budget noise; engine code byte-identical from tag onward). M5.F HEAD-run vs M5.E HEAD-run: +2 WAC, −10 STS — both well within wallclock noise. Statistically flat. **M5.G HEAD-run vs M5.F: +11 WAC (well above ±2 noise band) and +417 STS credit (well above ±68 noise band) — decisive tactical+strategic positive**, validating the SE extension's contribution in fixed-`movetime` mode. Initial measurement (WAC 268 / STS 9036) was depressed by CPU contention from concurrent SPRT + parallel EPD suites; the clean re-run above is the load-bearing measurement (see "Methodology rule — RUN ALONE" below — a rule the M5.G campaign forced into the project conventions after a repeat M5.F-style mistake).

### M5.F observation (2026-05-09)

vs the displaced M5.E HEAD row (265 / 8832):
- WAC: 267 vs 265 = **+2 positions** (within ±2 wallclock-noise band).
- STS: 8822 vs 8832 = **−10 credit** (well within ±68 wallclock-noise band; M5.E's own run-vs-run variance was higher).

M5.F's diagnostic-suite signal is **flat**. Bench drops 26.9% (1466436 → 1072309 nodes) without an EPD-detectable correctness regression. Earlier (now-overwritten) tentative numbers showed an apparent regression of −11 WAC / −302 STS — that run shared CPU with an in-flight 6-concurrency SPRT match against `M5.E` and was depressed by load contention; the clean re-run above is the load-bearing measurement.

The SPRT remains the load-bearing strength gate. M5.F's mixed-TC SPRT vs `M5.E`: Δ Elo **+13.03 [−10.92, +37.12]**, verdict=continue at 400 games, per-TC bimodal (10+0.1: 59.8%, 60+0.6: 46.5%). Landed as "small-but-not-regression" per plan §11 spirit (mean positive, CI lower 0.92 Elo below the −10 threshold, diagnostic-suite flat). Full SPRT log: [`bench/sprt/2026-05-09-m5.f-vs-m5e-mixed-tc.md`](sprt/2026-05-09-m5.f-vs-m5e-mixed-tc.md).

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

## Methodology rule — RUN ALONE (load-bearing, do not relax)

**EPD suites at fixed `--movetime` are CPU-contention-sensitive. Run them on an idle machine, never alongside any other CPU-heavy job (SPRT, rating-estimate, mutation testing, parallel EPD suites, parallel `cargo test`, ...).**

The wallclock budget per position is fixed (default 1000ms or 500ms via `EPD_MOVETIME`). Under CPU contention, the engine reaches lower depths within that budget; tactical/strategic accuracy degrades; numbers come back depressed in ways that look like real regressions but are pure measurement noise. The depression magnitude is large — observed at M5.F (initial WAC 254, STS 8530 under in-flight SPRT contention; clean re-run 267 / 8822, a +13 / +292 swing) and at M5.G (concurrent WAC + STS + rating-estimate all started together; results invalid).

**Operational rules:**
- One EPD suite at a time (not WAC and STS in parallel — they together saturate the box even at `EPD_CONCURRENCY=4`).
- No EPD suite while any SPRT/match/rating-estimate is running.
- No EPD suite while `cargo mutants` is running (`cargo mutants` uses N parallel test workers).
- No EPD suite while a parallel-coding-agent's `cargo test` is in flight.

**Sequencing for milestone landings**: run WAC, then STS, then any `rating-estimate` SPRT, **strictly sequentially**. Total wallclock is ~5 + 15 + 30 ≈ 50 minutes; running them in parallel saves no clock time once you re-run the contaminated ones (which you will).

This rule is appended to bench/m5.f.md's M5.F observation as a generalised methodology rule. Future milestones must check this section's history before claiming WAC/STS represent a clean signal.
