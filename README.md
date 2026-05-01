# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: early.** Fail-soft alpha-beta with quiescence search,
> iterative deepening, transposition table (Zobrist-keyed, 16 MiB default),
> killer-aware move ordering (TT move + MVV-LVA captures + 2 killer slots
> per ply + history-rated quiets), butterfly history heuristic
> (`[side][from][to]` i16; `+= depth*depth` bonus + matching malus),
> two-tier asymmetric aspiration windows (±50 cp first try; depth ≥ 6),
> mate-distance pruning, PeSTO middlegame piece-square tables, and
> `compute_caps`-driven time management. `bench` UCI command for
> deterministic node-count regression baselines. The rest of the strength
> path (PVS, NMP, LMR, futility; eval improvements; NNUE) is tracked in
> [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: 2300–2531 Elo on Stockfish 18 UCI_LimitStrength scale,
> varying with TC.** Direct rating estimates from the in-process ELOH
> harness (Robbins-Monro iteration of Stockfish's `UCI_Elo` to parity):
>
> | TC | Elo | σ | Games | W-L-D |
> |---|---|---|---|---|
> | 10+0.1 | 2300 | ±12 | 54 | 24-22-8 |
> | 20+0.2 | 2466 | ±8 | 64 | 35-20-9 |
> | 40+0.4 | 2531 | ±9 | 78 | 32-36-10 |
> | 60+0.6 | 2483 | ±13 | 70 | 26-34-10 |
>
> Apple M4 P-cores (utility QoS), single thread, no pondering, virtual
> clock on clawfish. Methodology + per-trajectory consistency checks:
> [`bench/sprt/2026-05-01-m4.d-per-tc-rating-estimate.md`](bench/sprt/2026-05-01-m4.d-per-tc-rating-estimate.md).
>
> The TC-to-Elo curve is not monotonic at the slow end (60+0.6 < 40+0.4
> by ~48 Elo, within combined ~2σ noise) — Stockfish's UCI_LimitStrength
> reduction mechanism saturates with TC differently than full Stockfish,
> so per-TC numbers are not directly comparable across TCs. **These
> direct measurements are substantially below the previously-reported
> chained estimate of ~2962 mixed-TC** (M3.F's 2114 anchor + M4 SPRT Δs
> summed). The discrepancy is explained by Stockfish UCI_Elo
> non-linearity in the 2200-2400 transition zone (flagged in the M3.F
> anchor doc): chaining clawfish-vs-clawfish Δ-SPRTs onto an anchor below
> the zone compounds calibration error. The within-project Δ-SPRTs
> (M4.A–M4.D) remain trustworthy as relative measurements; the
> load-bearing M4.D SPRT vs `baseline/alpha-beta-tt-killer-history`
> stands at 138-90-172 in 400 games / 200 pairs, Δ +41.9 Elo with
> pentanomial 95% CI [+18.2, +65.6], `Ptnml(0-2) = [5, 40, 78, 56, 21]`.
> Latest SPRT methodology + caveats:
> [`bench/sprt/2026-04-30-m4.d-vs-history-mixed-tc.md`](bench/sprt/2026-04-30-m4.d-vs-history-mixed-tc.md).
>
> **Current bench:** `bench: 15863206 nodes <NPS> nps` at default depth 7
> (M4.D end; node count is deterministic, NPS is wallclock-dependent).
> Down from M4.C's 17,650,332 nodes — ~10% additional reduction from
> aspiration windows tightening the iterative-deepening outer loop;
> down from M3.F's 172,312,700 — ~91% cumulative reduction from
> TT + killer + history + aspiration. Run
> `printf 'bench\nquit\n' | ./target/release/clawfish` to reproduce
> the node count.

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
