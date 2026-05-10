# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: early.** Fail-soft alpha-beta with quiescence search,
> iterative deepening, transposition table (Zobrist-keyed, 16 MiB default;
> qsearch participates per ADR-0028), killer-aware move ordering (TT move
> + MVV-LVA captures + 2 killer slots per ply + history-rated quiets),
> butterfly history heuristic
> (`[side][from][to]` i16; `+= depth*depth` bonus + matching malus),
> two-tier asymmetric aspiration windows (±50 cp first try; depth ≥ 6),
> null-move pruning (`R = 2 + depth/6`; seven-condition gate; mate-cap),
> reverse futility pruning (`margin = 100*depth`; depth ≤ 6; non-PV,
> non-check; no TT store),
> late move reductions (log-log formula `R = floor(0.99 + ln(d)·ln(qi)/3.14)`
> clamped to `0..=(d−2)`; quiet-only, non-PV, non-check, depth ≥ 3,
> quiet_index ≥ 2; killers and high-history quiets exempt; full-depth re-search
> on `reduced_score > alpha`; reduced-only TT-store suppression),
> frontier futility pruning at depth=1 (Heinz 1998 frontier-only),
> singular extensions (`SE_MIN_DEPTH=6` Xiphos default; non-PV interior
> nodes with TT-Lower entry of sufficient depth; verification at
> `(depth−1)/2` zero-window excluding TT move; +1 ply on the TT move when
> verification fails low),
> mate-distance pruning, PeSTO middlegame piece-square tables, and
> `compute_caps`-driven time management. `bench` UCI command for
> deterministic node-count regression baselines. The rest of the strength
> path (staged movegen; eval improvements; NNUE) is tracked in
> [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2639 ± 44 Elo on Stockfish 18 UCI_LimitStrength
> scale at uniform mixed TC** (M5.G v2 rating-anchor, 2026-05-10; rating
> estimate Δ Elo **+3.47 [−40.84, +47.90]** vs Stockfish-2641 anchor;
> CI overlaps M5.F's 2636, M5.E's 2622, M5.D-v2's 2641 — the chain is
> statistically flat across M5.D-v2 → M5.E → M5.F → M5.G).
>
> **M5.G (singular extensions)** landed at v2 retune `SE_MIN_DEPTH=6`
> (after v1's literature default `SE_MIN_DEPTH=8` SPRT-failed at outcome 3
> and v3's `SE_MARGIN_PER_DEPTH=2` at outcome 4). v2's mixed-TC SPRT vs
> `baseline/m5f-qsearch-in-tt`: **Δ Elo +23.49 [+0.65, +46.53]** at 400
> games (verdict=continue, plan §11 outcome 2 small-but-not-regression).
> Per-TC: 10+0.1 51.2%, 20+0.2 51.0%, **40+0.4 64.6% (decisive +104 Elo
> locally)**, 60+0.6 48.9%. Bench: **1,147,614 nodes (+7.0% vs M5.F)** —
> first M5 sub-phase to grow bench rather than shrink it; SE adds work
> (verification searches at depths 6–7) but the SPRT shows search-quality
> gain outweighs the per-node cost.
>
> **Diagnostic suites at the v2-landed binary (clean re-run, sequential
> per the new "RUN ALONE" methodology rule)**: WAC **278/300 (92.7%)** —
> +11 vs M5.F's 267 (well above ±2 wallclock-noise). STS **9239/15000
> (61.6%, est. 2499 STS-Elo)** — +417 credit vs M5.F's 8822 (well above
> ±68 wallclock-noise). Decisive tactical+strategic positive — STS-Elo
> jumps +123 from M5.F.
>
> **M5.F (qsearch-in-TT)** landed 2026-05-09 as "small-but-not-regression"
> with mixed-TC SPRT Δ Elo **+13.03 [−10.92, +37.12]**. **M5.E** was a
> correctness-only landing (4 narrow `qsearch` corrections; SPRT
> inconclusive). `baseline/m5g-singular` tagged at the M5.G v2 landing.
> Mixed-TC rating estimate via the in-process ELOH harness with frozen-K +
> disabled σ-stop:
>
> | Metric | Value |
> |---|---|
> | Estimated Elo (M5.G v2) | **~2639 ± 44** (anchor 2641 + Δ +3.47 [−40.84, +47.90]) |
> | Anchor (M5.F) | ~2636 ± 43 |
> | Games | 200 (100 pairs) + 200-game independent confirmation |
> | W-L-D (primary run) | 85 / 83 / 32 (50.5% score) |
> | TC mix | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (uniform 4-bucket) |
> | Per-TC scores | 10+0.1: 32.9%; 20+0.2: 43.2%; 40+0.4: 57.8%; 60+0.6: 62.5% |
> | Stop reason | max-games |
>
> Apple M4 P-cores (utility QoS), single thread, no pondering, virtual
> clock on clawfish. M5.G rating-estimate methodology:
> [`bench/sprt/2026-05-10-m5.g-mixed-tc-rating-estimate.md`](bench/sprt/2026-05-10-m5.g-mixed-tc-rating-estimate.md).
> M5.G SPRT-vs-`baseline/m5f-qsearch-in-tt` (v2-landed) log:
> [`bench/sprt/2026-05-10-m5.g-v2-min-depth-6-vs-m5f-mixed-tc.md`](bench/sprt/2026-05-10-m5.g-v2-min-depth-6-vs-m5f-mixed-tc.md).
> Full M5.G retune campaign (v1 outcome 3, v2 outcome 2 LANDED, v3 outcome 4):
> [`bench/m5.md`](bench/m5.md) M5.G section.
>
> M5.C's mixed-TC SPRT vs `baseline/m5b-rfp` (M5.B end) — **H1 accepted in
> 144 games / 72 pairs**, Δ Elo **+145.47 [+100.12, +196.09]** (pentanomial
> 95% CI). All four TC buckets positive with strong slow-TC amplification
> (40+0.4: 81.9%; 60+0.6: 78.9%; 20+0.2: 58.75%; 10+0.1: 58.3%) — typical
> of LMR's depth-bounded selectivity. Full log:
> [`bench/sprt/2026-05-05-m5.c-vs-m5b-mixed-tc.md`](bench/sprt/2026-05-05-m5.c-vs-m5b-mixed-tc.md).
>
> M5.B's mixed-TC SPRT vs `baseline/m5a-nmp` (M5.A end) — W-L-D
> **94/32/74** in 200 games / 100 pairs, score 65.5%, **logistic Elo +111**.
> Pentanomial-GSPRT did not formally cross the `elo1=5` H1 Wald bound (the
> chosen bound was too narrow given the actual gain magnitude — RFP
> standalone was expected to be a +20–50 Elo addition on top of NMP, but
> the actual gain on this engine's M4-ordering substrate is ~2× that);
> H1 was treated as accepted on score-based decisioning. Full log:
> [`bench/sprt/2026-05-02-m5.b-vs-m5a-mixed-tc.md`](bench/sprt/2026-05-02-m5.b-vs-m5a-mixed-tc.md).
>
> M5.A's mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history-aspiration`
> (M4.D end) — **H1 accepted in 22 games / 11 pairs**, Δ Elo **+400.00
> [+285.52, +677.51]** (pentanomial 95% CI). Full log:
> [`bench/sprt/2026-05-01-m5.a-vs-aspiration-mixed-tc.md`](bench/sprt/2026-05-01-m5.a-vs-aspiration-mixed-tc.md).
>
> Pure-logistic SPRT gains do NOT translate 1:1 to UCI_Elo space.
> Stockfish UCI_LimitStrength has a non-linear strength curve at
> 2200–2700 (per the M4.D + M5.A + M5.B + M5.C rating-estimate docs); chained
> clawfish-vs-clawfish SPRT logistic Elo measures relative strength
> tightly but doesn't transfer additively to UCI_Elo space.
>
> **Current bench:** `bench: 1147614 nodes <NPS> nps` at default depth 7
> (M5.G v2 end; +7.0% vs M5.F's 1,072,309 — SE adds verification searches
> at depths 6–7 in the bench corpus). Node count is deterministic; NPS is
> wallclock-dependent. Down from M3.F's 172,312,700 — ~99.3% cumulative
> reduction across SE + qsearch-in-TT + NMP + RFP + LMR + FFP + M4's
> TT/killer/history/aspiration. Run
> `printf 'bench\nquit\n' | ./target/release/clawfish` to reproduce.

## Build

```sh
cargo build --release
```

Primary target is Apple Silicon (ARM64) macOS. Other platforms should build
cleanly but are not regularly exercised.

## Run

clawfish speaks the [UCI protocol](https://backscattering.de/chess/uci/) over
stdin/stdout. Point any UCI-compatible chess GUI (Banksia GUI — `brew install
banksiagui` on macOS — En Croissant, Arena) at the binary:

```sh
./target/release/clawfish
```

Or pipe UCI commands directly:

```sh
printf 'uci\nposition startpos\ngo movetime 100\nquit\n' \
  | ./target/release/clawfish
```

## Diagnostic suites

Per-position correctness scoring against the canonical public EPD test suites
**WAC** (Win at Chess; 300 tactical positions) and **STS** (Strategic Test Suite;
1500 positions across 15 strategy themes, weighted multi-move scoring).
Complementary to SPRT — SPRT measures relative game-playing strength
stochastically over a game distribution; WAC/STS measure absolute correctness
on a fixed corpus deterministically. WAC catches tactical regressions; STS
attributes positional regressions to specific eval components.

Vendored corpora at `bench/data/wac.epd` and `bench/data/sts.epd`. The
`epd-suite` binary drives the engine over UCI:

```sh
cargo build --release
./target/release/epd-suite \
    --engine target/release/clawfish \
    --suite wac \
    --epd bench/data/wac.epd \
    --movetime 1000 \
    --concurrency 6
```

Or via the wrapper for the typical recipes:

```sh
scripts/epd-suite.sh run wac           # HEAD on WAC
scripts/epd-suite.sh run sts           # HEAD on STS
scripts/epd-suite.sh backfill          # iterate baseline tags (worktree-isolated builds)
```

**HEAD (M5.G v2):** WAC **278/300 (92.7%)**, STS **9239/15000 (61.6% → STS-Elo ~2499)** at `--movetime 1000`. Per-baseline backfill across 12 tags + per-theme STS breakdown at [`bench/epd-suites.md`](bench/epd-suites.md). Tactical (WAC) **peak now at M5.G v2** (278/300, eclipsing M5.A NMP's previous 270/300); SE's TT-move extension on tactical hinges produces the +11-position bump above M5.F. Strategic (STS) credit also peaks at M5.G v2 (9239/15000); SE benefits the deep-search themes (Bishop vs Knight, Knight Outposts) the most. STS-Elo systematically underestimates game-playing strength by ~140 Elo (the ~2499 STS-Elo lags the ~2639 mixed-TC rating estimate); the relative ranking across tags is the load-bearing signal.

## Scope

Standard chess only. Variant chess is explicitly out of scope.

## Goals

The project's casual progression, in tiers. Each is informal — not a strict
commitment, just a way to think about what "done enough for now" means at
each stage.

| # | Goal | Target CCRL Blitz | Status |
|---|---|---|---|
| 1 | Play correct chess at all (legal moves, no protocol bugs) | n/a (random play sits below CCRL's listing floor) | **✓ achieved at M2** — RandomMover speaking UCI through fastchess |
| 2 | Beat the project's owner reliably | ~2000-2400 | **✓ achieved at M3** — measured ~2114 at our anchor; ~2100-2300 CCRL Blitz equivalent estimated, comfortably ahead of the owner's casual chess.com level |
| 3 | Beat grandmasters reliably | ~2600-2900 | targeting end-M5 / mid-M6 (proxy: ≥60% vs Stockfish UCI_Elo=2800) |
| 4 | Grow out of reach for the best humans | ~3200-3500 | targeting M9 (NNUE) |
| 5 | Compete on par with or beat the strongest engines | 3600+ | aspirational; would require parallel search (M8) + NNUE (M9) plus aggressive M5 pruning + a high-quality training pipeline. Not on the current roadmap as a hard target. |

Goals 3-5 are stated against approximate Elo proxies (since real GMs
and elite engines aren't directly accessible for routine matches).
CCRL Blitz is chosen as the reference scale because the project tests
at fast TC; CCRL 40/4 numbers would be modestly higher (engines without
TT suffer at fast TC more than at slow TC). The proxies and
methodology caveats are documented in `bench/sprt/` per matching
milestone.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — current architectural state
- [`docs/roadmap.md`](docs/roadmap.md) — milestone plan and what's next
- [`docs/decisions/`](docs/decisions/) — ADRs (one file per substantive decision)
- [`docs/reference/`](docs/reference/) — vendored authoritative specs (UCI, FIDE, PGN)

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option.
