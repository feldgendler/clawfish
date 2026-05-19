# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength. Classical evaluation first; NNUE planned.

> **Status: M6 in progress (M6.E landed 2026-05-19 — king-safety infra,
> score-neutral; all king-safety weights deferred to M6.H. Production HEAD = `M6.E`,
> eval byte-identical to `M6.D` / `M6.C` / `M6.B`, strength unchanged by construction).** Fail-soft
> alpha-beta with quiescence search, iterative deepening, transposition
> table (Zobrist-keyed, 16 MiB default; qsearch participates per ADR-0028),
> killer-aware move ordering (TT move + MVV-LVA captures + 2 killer slots
> per ply + history-rated quiets) via a staged `MoveStager` iterator (M5.H1),
> butterfly history heuristic (`[side][from][to]` i16; `+= depth*depth`
> bonus + matching malus), two-tier asymmetric aspiration windows
> (±50 cp first try; depth ≥ 6),
> null-move pruning (`R = 2 + depth/6`; seven-condition gate; mate-cap),
> reverse futility pruning (`margin = 100*depth`; depth ≤ 6; non-PV,
> non-check; no TT store),
> late move reductions (log-log `R = floor(0.99 + ln(d)·ln(qi)/3.14)`
> clamped to `0..=(d−2)`; quiet-only, non-PV, non-check, depth ≥ 3,
> quiet_index ≥ 2; killers and high-history quiets exempt; full-depth
> re-search on `reduced_score > alpha`),
> frontier futility pruning at depth=1 (Heinz 1998),
> singular extensions (`SE_MIN_DEPTH=6`; verification at `(depth−1)/2`
> zero-window excluding the TT move; +1 ply when it fails low),
> mate-distance pruning,
> **tapered PeSTO MG+EG piece-square evaluation** blended by a material
> phase tag, with a **bishop-pair** term, **KBvKB-same-color**
> insufficient-material detection, and a **KQK/KRK mop-up**
> corner-attractor (M6.A; ADR-0031, supersedes ADR-0014 §1/§5),
> the **pawn-structure infrastructure** — 4 MiB search-owned pawn hash,
> pawn-only Zobrist substream, isolated/doubled/backward/connected
> predicates, passed-pawn detection — shipping the **connected-pawn
> term only** (`PAWN_STRUCTURE_IN_EVAL=true`; ISO/DBL/BWD weights zeroed
> in `eval::data`; M6.B; ADR-0032 §7: the all-four literature defaults
> SPRT-regressed −100 vs `M6.A`, a per-term subset screen exposed a
> catastrophic ISO×CONN connectivity-axis double-count, CONN-only is
> the largest interaction-immune gain at **+45 Elo**; M6.H re-introduces
> ISO/DBL/BWD via joint Texel + rescales CONN), the **passed-pawn term**
> — per-passer rank bonus + EG path discriminator + EG king-tropism,
> computed live, never cached — shipped **score-neutral** (M6.C;
> ADR-0032 §8: a three-config screen ladder proved the literature
> defaults a scale-invariant structural mismatch with this engine, so
> every passed weight is zeroed and the whole set defers to M6.H's
> reshape; the term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.B`),
> the **piece-mobility term** — N/B/R/Q pseudolegal-attack count ∩
> mobility area, per-kind MG/EG tables, computed live, never cached —
> also shipped **score-neutral** (M6.D; no ADR, roadmap-committed: the
> landing-gate + a full per-kind screen proved the Stockfish-HCE
> literature defaults a scale-invariant structural mismatch with our
> PeSTO PSTs — all-four −131.62, ×0.5 co-scale −220.18 *worsened*; every
> mobility weight zeroed, whole set defers to M6.H's per-kind reshape;
> the term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.C`),
> the **king-safety term** — king-zone attacker S-curve + castled
> pawn-shield + MG-only open/semi-open-file penalty, computed live,
> never cached — also shipped **score-neutral** (M6.E; ADR-0033:
> research transfer-risk verdict HIGH + the M6.B→C→D three-phase law +
> king-safety SPRT-noisiness + coupled-by-design components ⇒ **no SPRT
> screen ladder** (the owned M6.C/M6.D divergence), every king-safety
> weight zeroed, whole set defers to M6.H's reshape; ADR-0033 §6
> supersedes ADR-0032 §3's pawn-shield-cache reservation; the term ≡
> (0,0) ⇒ `evaluate` byte-identical to `M6.D`),
> and `compute_caps`-driven time management. `bench` UCI command for
> deterministic node-count regression baselines. The remaining strength
> path (the M6.F Tier-1 HCE features — outposts, rook-on-file, endgame
> draw-scaling — added score-neutral/inert before the M6.H joint Texel
> tuning pass over the cumulative M6.B–F surface; then NNUE) is tracked in
> [`docs/roadmap.md`](docs/roadmap.md).
>
> **Current strength: ~2648 Elo on the Stockfish 18 UCI_LimitStrength
> scale at uniform mixed TC** (M6.B CONN-only, measured 2026-05-16,
> 200-game rating estimate Δ Elo **+6.95 [−39.89, +54.05]** vs the
> long-standing 2641 anchor → ≈ **2648 [2601, 2695]**; statistically
> **flat within noise** vs M6.A's ≈2678 [2639, 2718] — the two are
> independent HEAD-vs-Stockfish estimates with heavily overlapping CIs,
> a ~30-pt point-estimate move well inside ±50 noise, **not** a
> regression). The in-engine mixed-TC SPRT — the actual landing gate and
> the authoritative *relative* measure — measured M6.B CONN-only
> **+45.42 [+19.68, +71.67] Elo vs `M6.A`**
> (`continue`@400-cap; lands by the M5.F/M5.G-v2 positive-CI
> outcome-ladder precedent — see ADR-0032 §7), itself stacked on M6.A's
> **+250.57 [+197.15, +316.66] vs `M5.H1`** (H1-accept). `UCI_LimitStrength`
> is a non-linear scale at 2200–2700, so chained clawfish-vs-clawfish
> logistic-Elo gains compress heavily into UCI_Elo space and do **not**
> transfer 1:1; the M6.A +250 was outsized because `M5.H1` was MG-only
> with no endgame king PST. Caveat: CONN-only's gain is fast-TC-concentrated
> and reverses at `60+0.6` (the literature CONN table is over-scaled for
> deep search) — an M6.H joint-Texel watch-item; the aggregate mixed-TC
> game (the actual gate object) is firmly positive.
>
> **Per-phase SPRT chain, bench history, and diagnostic-suite numbers**
> for every milestone live in the milestone docs — they are no longer
> mirrored in full here:
> [`bench/m5.md`](bench/m5.md), [`bench/m6.md`](bench/m6.md),
> [`docs/milestones/`](docs/milestones/), and the per-campaign logs under
> [`bench/sprt/`](bench/sprt/). Apple M4 P-cores, single thread, no
> pondering, virtual clock on clawfish.
>
> **Current bench:** `bench: 1213649 nodes <NPS> nps` at default depth 7
> (M6.B end — CONN-only; the live connected-pawn term reshuffles root-move
> ordering, +11% node count vs M6.A's `1093365`; depth-4 `90591`). Node
> count is deterministic; NPS is wallclock-dependent.
> From M6.A onward the bench *number* is no longer a no-regression signal
> (tapered leaf scores reshuffle root-move ordering) — but it stays stable
> across reruns at each HEAD and anchors the next phase
> ([`docs/roadmap.md`](docs/roadmap.md) §"Bench-node-count regression
> policy"). Run `printf 'bench\nquit\n' | ./target/release/clawfish` to
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

**HEAD (M6.A):** WAC **271/300 (90.3%)**, STS **9620/15000 (64.1% → STS-Elo ~2613)**, measured at `--movetime 500` in the M6.A same-campaign RUN-ALONE re-baseline (M6.A vs a fresh `M5.H1` rebuild: WAC +1 within ±2 noise; STS **+834 credit / +248 STS-Elo**, decisive). Note the movetime differs from the legacy `--movetime 1000` figures in [`bench/epd-suites.md`](bench/epd-suites.md)'s 2026-05-09 snapshot table — that snapshot is stale and not directly comparable; the same-campaign M6.A row is the load-bearing measurement (per the roadmap's "Same-campaign re-baseline required" rule). Per-piggyback sub-gates: mop-up lifts the "King Activity" theme +95 (404→499); the bishop-pair term is flat on "Bishop vs Knight" (Texel-calibrated in M6.H). STS-Elo systematically underestimates game-playing strength (the ~2613 STS-Elo lags the ~2678 mixed-TC rating estimate); the relative ranking across tags is the load-bearing signal. Per-baseline backfill + per-theme breakdown: [`bench/epd-suites.md`](bench/epd-suites.md).

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
| 4 | Grow out of reach for the best humans | ~3200-3500 | targeting M10 (NNUE) |
| 5 | Compete on par with or beat the strongest engines | 3600+ | aspirational; would require parallel search (M9) + NNUE (M10) plus aggressive M5 pruning + a high-quality training pipeline. Not on the current roadmap as a hard target. |

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
