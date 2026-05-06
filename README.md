# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: early.** Fail-soft alpha-beta with quiescence search,
> iterative deepening, transposition table (Zobrist-keyed, 16 MiB default),
> killer-aware move ordering (TT move + MVV-LVA captures + 2 killer slots
> per ply + history-rated quiets), butterfly history heuristic
> (`[side][from][to]` i16; `+= depth*depth` bonus + matching malus),
> two-tier asymmetric aspiration windows (±50 cp first try; depth ≥ 6),
> null-move pruning (`R = 2 + depth/6`; seven-condition gate; mate-cap),
> reverse futility pruning (`margin = 100*depth`; depth ≤ 6; non-PV,
> non-check; no TT store),
> late move reductions (log-log formula `R = floor(0.99 + ln(d)·ln(qi)/3.14)`
> clamped to `0..=(d−2)`; quiet-only, non-PV, non-check, depth ≥ 3,
> quiet_index ≥ 2; killers and high-history quiets exempt; full-depth re-search
> on `reduced_score > alpha`; reduced-only TT-store suppression),
> mate-distance pruning, PeSTO middlegame piece-square tables, and
> `compute_caps`-driven time management. `bench` UCI command for
> deterministic node-count regression baselines. The rest of the strength
> path (frontier futility, qsearch correctness + TT, singular extensions;
> eval improvements; NNUE) is tracked in [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2664 Elo on Stockfish 18 UCI_LimitStrength scale
> at uniform mixed TC** (M5.D, 2026-05-06; estimated 2663.95 ± 47.75,
> statistically indistinguishable from M5.C's 2657 ± 16.49 anchor).
> M5.D's mixed-TC SPRT vs `baseline/m5c-lmr` was **inconclusive** at v1
> parameters (Δ Elo −6.95 [−32.50, +18.53] at 400 games; per-TC
> bimodal: positive at fast TC, negative at slow TC); `baseline/m5d-ffp`
> NOT tagged. Mixed-TC rating estimate via the in-process ELOH harness
> with Robbins-Monro iteration:
>
> | Metric | Value |
> |---|---|
> | Estimated Elo (M5.D) | **~2664 ± 48** (anchor 2657 + Δ +6.95 [−40.64, +54.80]) |
> | Anchor (M5.C) | 2657.44 ± 16.49 |
> | Games | 200 (100 pairs) |
> | W-L-D | 80 / 76 / 44 (51.0% score) |
> | TC mix | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (uniform 4-bucket) |
> | TC sampled | 44 / 50 / 52 / 54 games (frozen K + disabled σ-stop; full 200 games complete) |
> | Stop reason | max-games |
>
> Apple M4 P-cores (utility QoS), single thread, no pondering, virtual
> clock on clawfish. M5.D rating-estimate methodology + per-TC pattern:
> [`bench/sprt/2026-05-06-m5.d-mixed-tc-rating-estimate.md`](bench/sprt/2026-05-06-m5.d-mixed-tc-rating-estimate.md).
> M5.C anchor methodology: [`bench/sprt/2026-05-05-m5.c-mixed-tc-rating-estimate.md`](bench/sprt/2026-05-05-m5.c-mixed-tc-rating-estimate.md).
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
> **Current bench:** `bench: 1414405 nodes <NPS> nps` at default depth 7
> (M5.D end; node count is deterministic, NPS is wallclock-dependent).
> Down from M5.C's 1,651,610 nodes — −14.4% additional reduction from
> frontier futility pruning composing with LMR + RFP + NMP + M4's
> TT/killer/history/aspiration ordering; down from M3.F's 172,312,700 —
> ~99% cumulative reduction from TT + killer + history + aspiration + NMP
> + RFP + LMR + FFP. Run `printf 'bench\nquit\n' | ./target/release/clawfish`
> to reproduce the node count.

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
