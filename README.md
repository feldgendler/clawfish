# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: early.** Fail-soft alpha-beta with quiescence search,
> iterative deepening, MVV-LVA + prior-PV ordering, mate-distance pruning,
> PeSTO middlegame piece-square tables, and `compute_caps`-driven time
> management. `bench` UCI command for deterministic node-count regression
> baselines. No transposition table yet. The rest of the strength path
> (TT, killer/history, PVS, aspiration; eval improvements; NNUE) is tracked in
> [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2114 Elo at tc=10+0.1** (Apple M4 P-cores, single
> thread, no pondering, anchored to Stockfish UCI_Elo / CCRL 40/4
> calibration). ±35 Elo CI from 120-game online iteration. Measured at
> M3.F end. Methodology + caveats: [`bench/sprt/2026-04-29-online-elo-iteration.md`](bench/sprt/2026-04-29-online-elo-iteration.md).
>
> **Current bench:** `bench: 172312700 nodes 11489045 nps` at default depth 7
> (M3.F end). Run `printf 'bench\nquit\n' | ./target/release/clawfish` to
> reproduce.

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

| # | Goal | Status |
|---|---|---|
| 1 | Play correct chess at all (legal moves, no protocol bugs) | **✓ achieved at M2** — RandomMover speaking UCI through fastchess |
| 2 | Beat the project's owner reliably | **✓ achieved at M3** — at ~2114 Elo, comfortably ahead of the owner's casual chess.com level |
| 3 | Beat grandmasters reliably | targeting end-M5 / mid-M6 (~2700-2900 Elo equivalent; proxy: ≥60% vs Stockfish UCI_Elo=2800) |
| 4 | Grow out of reach for the best humans | targeting M9 (NNUE, ~3300+ Elo equivalent) |
| 5 | Compete on par with or beat the strongest engines | aspirational; would require parallel search (M8) + NNUE (M9) plus aggressive M5 pruning + a high-quality training pipeline. Not on the current roadmap as a hard target. |

Goals 3-5 are stated against approximate Elo proxies (since real GMs and
elite engines aren't directly accessible for routine matches). The
proxies and methodology caveats are documented in `bench/sprt/` per
matching milestone.

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
