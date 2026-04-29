# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: early.** Fail-soft alpha-beta with quiescence search,
> iterative deepening, transposition table (Zobrist-keyed, 16 MiB default),
> killer-aware move ordering (TT move + MVV-LVA captures + 2 killer slots
> per ply + remaining quiets), mate-distance pruning, PeSTO middlegame
> piece-square tables, and `compute_caps`-driven time management. `bench`
> UCI command for deterministic node-count regression baselines. The rest
> of the strength path (history, PVS, aspiration; eval improvements; NNUE)
> is tracked in [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2884 Elo at tc=10+0.1** (chained estimate: M3.F's
> ~2114 anchor + M4.A's measured Δ +282 + M4.B's measured Δ +488).
> Apple M4 P-cores, single thread, no pondering, ±~140 Elo combined
> uncertainty. The chained estimate sits in the upper end of the
> "fast-TC amplifies ordering gains" band; actual strength against
> calibrated opponents at slow TC may be 100–200 Elo lower. Direct
> rating-estimate run deferred to M4.D close.
> Latest SPRT methodology + caveats: [`bench/sprt/2026-04-30-m4.b-vs-tt.md`](bench/sprt/2026-04-30-m4.b-vs-tt.md).
>
> **Current bench:** `bench: 22237579 nodes <NPS> nps` at default depth 7
> (M4.B end; node count is deterministic, NPS is wallclock-dependent).
> Down from M4.A's 39,964,046 nodes — ~44% reduction from killer ordering;
> down from M3.F's 172,312,700 — ~87% cumulative reduction from TT +
> killer. Run `printf 'bench\nquit\n' | ./target/release/clawfish` to
> reproduce the node count.

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
