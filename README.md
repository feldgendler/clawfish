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
> **Current strength: ~2962 Elo on a mixed game (uniform over tc ∈
> {10+0.1, 20+0.2, 40+0.4, 60+0.6})** (chained estimate: M3.F's ~2114
> anchor at tc=10+0.1 + M4.A's measured Δ +282 + M4.B's measured Δ +488
> + M4.C's measured Δ +36 at tc=60+0.6 + M4.D's measured Δ +42 at
> mixed-TC). Apple M4 P-cores, single thread, no pondering, ±~160 Elo
> combined uncertainty. The chained estimate sits in the upper end of
> the "fast-TC amplifies ordering gains" band; actual strength against
> calibrated opponents at slow TC may be 100–200 Elo lower. M4.D's
> mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history` is the
> load-bearing M4.D measurement: 138-90-172 in 400 games / 200 pairs,
> Δ +41.9 Elo with pentanomial 95% CI [+18.2, +65.6], `Ptnml(0-2) =
> [5, 40, 78, 56, 21]`. The fast-TC-only run at threshold=6 (+65.9 ±
> 26.5 Elo, run 3 of the M4.D fast-TC SPRT log) cross-validates the
> 10+0.1 bucket within ~1σ.
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
