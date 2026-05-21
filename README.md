# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength.

## Goals and status

Standard chess only. Variant chess is explicitly out of scope.

**Current strength: ~2648 Elo** on the Stockfish 18 `UCI_LimitStrength` scale
at mixed time control, single-threaded on Apple Silicon (200-game estimate at
the M6.B measurement point). The engine implements a hand-crafted classical
evaluation; NNUE is a planned future milestone, not yet present.

**Roadmap: M6 in progress.** Classical-eval infrastructure is complete; the
next milestone (M6.H) jointly Texel-tunes its cumulative parameter set on a
self-play corpus. Parallel search (M9) and NNUE evaluation (M10) follow. See
[`docs/roadmap.md`](docs/roadmap.md) for the full plan.

### Tier goals

Informal markers for "done enough for now" at each stage:

| # | Goal | Target CCRL Blitz | Status |
|---|---|---|---|
| 1 | Play correct chess at all (legal moves, no protocol bugs) | n/a | **achieved at M2** |
| 2 | Beat the project's owner reliably | ~2000–2400 | **achieved at M3** (~2114 at the M3 anchor) |
| 3 | Beat grandmasters reliably | ~2600–2900 | targeting end-M5 / mid-M6 |
| 4 | Grow out of reach for the best humans | ~3200–3500 | targeting M10 (NNUE) |
| 5 | Compete with the strongest engines | 3600+ | aspirational; requires parallel search + NNUE + a strong training pipeline |

Goals 3–5 use approximate Elo proxies since real GMs and elite engines aren't
directly accessible for routine matches. Methodology and caveats live under
`bench/sprt/` per matching milestone.

## Instructions

### Build

```sh
cargo build --release
```

Primary target is Apple Silicon (ARM64) macOS. Other platforms should build
cleanly but are not regularly exercised.

### Run

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

### Diagnostic suites

Per-position correctness scoring against the canonical public EPD test suites
**WAC** (Win at Chess; 300 tactical positions) and **STS** (Strategic Test
Suite; 1500 positions across 15 strategy themes). Driven by the `epd-suite`
binary or its wrapper:

```sh
scripts/epd-suite.sh run wac
scripts/epd-suite.sh run sts
```

Latest scores: WAC **271/300 (90.3%)**, STS **9620/15000 (64.1%, STS-Elo ~2613)**.
Per-theme breakdown and historical backfills: [`bench/epd-suites.md`](bench/epd-suites.md).

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — current architectural state
- [`docs/roadmap.md`](docs/roadmap.md) — milestone plan and what's next
- [`docs/milestones/`](docs/milestones/) — per-phase retrospectives
- [`docs/decisions/`](docs/decisions/) — ADRs (one file per substantive decision)
- [`docs/reference/`](docs/reference/) — vendored authoritative specs (UCI, FIDE, PGN)

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option.
