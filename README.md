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
> mate-distance pruning, PeSTO middlegame piece-square tables, and
> `compute_caps`-driven time management. `bench` UCI command for
> deterministic node-count regression baselines. The rest of the strength
> path (LMR, futility, singular extensions; eval improvements; NNUE)
> is tracked in [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2601 Elo on Stockfish 18 UCI_LimitStrength scale
> at fast-TC-weighted mixed TC** (M5.A end, 2026-05-01). Mixed-TC rating
> estimate via the in-process ELOH harness with Robbins-Monro iteration:
>
> | Metric | Value |
> |---|---|
> | Converged Elo | **2601.45 ± 10.51** |
> | Games | 56 (28 pairs) |
> | W-L-D | 19 / 28 / 9 (41.96% score) |
> | TC mix | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (uniform 4-bucket) |
> | TC sampled | 28 / 0 / 14 / 14 games (early σ-stop fired before TC balance — fast-TC-weighted) |
> | Stop reason | σ-stop |
>
> Apple M4 P-cores (utility QoS), single thread, no pondering, virtual
> clock on clawfish. Methodology + per-TC distribution caveat:
> [`bench/sprt/2026-05-01-m5.a-mixed-tc-rating-estimate.md`](bench/sprt/2026-05-01-m5.a-mixed-tc-rating-estimate.md).
>
> M5.A's mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history-aspiration`
> (M4.D end) — **H1 accepted in 22 games / 11 pairs**, Δ Elo
> **+400.00 [+285.52, +677.51]** (pentanomial 95% CI), pentanomial
> `[0, 0, 0, 4, 7]` (zero losses), W-L-D = 18-0-4. Per-TC: 10+0.1: 3-0-1;
> 20+0.2: 8-0-2; 40+0.4: 5-0-1; 60+0.6: 2-0-0 — all four buckets decisively
> positive, doesn't-regress-anywhere floor satisfied. The +400 gain
> dramatically exceeds the literature prior of +30–70 Elo for NMP; the SPRT
> log attributes this to compounding M4-ordering quality + engine-strength
> tier conditioning. Full log:
> [`bench/sprt/2026-05-01-m5.a-vs-aspiration-mixed-tc.md`](bench/sprt/2026-05-01-m5.a-vs-aspiration-mixed-tc.md).
>
> The pure-logistic SPRT gain (+400 Elo) does NOT translate 1:1 to UCI_Elo
> space (~+300 fast-TC). Stockfish UCI_LimitStrength has a non-linear
> strength curve in 2200–2400 (per the M4.D rating-estimate doc); chained
> clawfish-vs-clawfish SPRT logistic Elo measures relative strength tightly
> but doesn't transfer additively to UCI_Elo space. Anchored M4.D per-TC
> numbers (10+0.1: 2300, 20+0.2: 2466, 40+0.4: 2531, 60+0.6: 2483 — see
> [`bench/sprt/2026-05-01-m4.d-per-tc-rating-estimate.md`](bench/sprt/2026-05-01-m4.d-per-tc-rating-estimate.md))
> + the M5.A mixed-TC anchor 2601 cross-validate at fast-TC: 2300 (M4.D
> 10+0.1) + ~300 (M5.A SPRT score 87.5% at 10+0.1) ≈ 2600. ✓
>
> **Current bench:** `bench: 3355270 nodes <NPS> nps` at default depth 8
> (M5.B end; node count is deterministic, NPS is wallclock-dependent).
> Down from M5.A's 5,345,534 nodes — −37.2% additional reduction from
> reverse futility pruning composing with NMP + M4's TT/killer/history/
> aspiration ordering; down from M3.F's 172,312,700 — ~98% cumulative
> reduction from TT + killer + history + aspiration + NMP + RFP. Run
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
