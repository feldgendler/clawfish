# clawfish

A Rust chess engine, written from scratch and grown incrementally toward
GM-level standard-chess strength.

Every line of it was written by Claude Code. I directed the work and made the
judgment calls; I don't write Rust. See [How this was built](#how-this-was-built)
if that is the part you came for.

## Goals and status

Standard chess only. Variant chess is explicitly out of scope.

**Current strength: ~2648 Elo** on the Stockfish 18 `UCI_LimitStrength` scale
at mixed time control, single-threaded on Apple Silicon (200-game estimate at
the M6.B measurement point). The engine implements a hand-crafted classical
evaluation; NNUE is a planned future milestone, not yet present.

**Roadmap: M6 complete.** Classical-eval infrastructure (M6.A–F) and corpus
data-infra (M6.G/H/H2) landed; the M6.I Texel tune jointly re-derived the
cumulative deferred-term weights on the 8M-position corpus, and the tuned eval
**shipped 2026-05-25** after a mixed-TC + virtual-clock SPRT vs `M6.F` returned
**H1-accept, Δ Elo +93.86 [+68.97, +119.72]** (618 games). The gain is
depth-amplifying — it regresses at ultra-fast TC but dominates at slow TC, which
is why the SPRT is run over a mix of time controls. Production HEAD = `M6.I`
(bench `1411314`). Parallel search (M11) and NNUE evaluation (M12) follow. See
[`docs/roadmap.md`](docs/roadmap.md) for the full plan.

### Tier goals

Informal markers for "done enough for now" at each stage:

| # | Goal | Target CCRL Blitz | Status |
|---|---|---|---|
| 1 | Play correct chess at all (legal moves, no protocol bugs) | n/a | **achieved at M2** |
| 2 | Beat the project's owner reliably | ~2000–2400 | **achieved at M3** (~2114 at the M3 anchor) |
| 3 | Beat grandmasters reliably | ~2600–2900 | targeting end-M5 / mid-M6 |
| 4 | Grow out of reach for the best humans | ~3200–3500 | targeting M12 (NNUE) |
| 5 | Compete with the strongest engines | 3600+ | aspirational; requires parallel search + NNUE + a strong training pipeline |

Goals 3–5 use approximate Elo proxies since real GMs and elite engines aren't
directly accessible for routine matches. Methodology and caveats live under
`bench/sprt/` per matching milestone.

## How this was built

The engine is the artifact; the method is the point. clawfish exists to answer
a narrower question than "can an agent write code" — **what does the apparatus
around an agent have to look like before it can grow a codebase across 274
commits without its owner reading every line?**

Everything below is visible in this repository: the workflow is written down in
[`docs/workflow.md`](docs/workflow.md), the model-tier decisions are backed by
dated evidence in [`docs/model-calibration-log.md`](docs/model-calibration-log.md),
and every substantive decision has an ADR under [`docs/decisions/`](docs/decisions/).

### The loop

Each unit of work runs the same cycle, designed to execute **unattended** from
prompt to commit: prior-art research → plan → *blind plan review* → test suite →
*blind test-suite review* → implementation → *blind final review* → mechanical
checks → strength gates → commit.

The three blind-review loops are the primary quality control. They replace the
per-step human approval gate that interactive agent work usually leans on —
which is what makes an unattended trajectory tolerable in the first place. The
agent stops only when genuinely **stuck**: an ambiguous spec that research can't
resolve, a hard tool failure, an architectural fork that contradicts an existing
ADR. *Uncertain* is not stuck — when uncertain it takes the most defensible path,
records the alternatives, and keeps going.

Each role is a separate subagent with its own prompt and model tier:
[`chess-researcher`](.claude/agents/chess-researcher.md),
[`plan-reviewer`](.claude/agents/plan-reviewer.md),
[`chess-coder`](.claude/agents/chess-coder.md),
[`test-suite-reviewer`](.claude/agents/test-suite-reviewer.md),
[`final-reviewer`](.claude/agents/final-reviewer.md).

### Verification is the real deliverable

A chess engine is an unusually honest subject. You cannot look at a diff and
know whether it made the engine stronger, and neither can the agent that wrote
it. Every strength claim therefore has to survive a statistical test rather than
an argument — which forces the verification apparatus to carry the weight that
code review carries elsewhere.

**Correctness gates**

- **perft** against Stockfish 18 as the sole oracle (ADR-0006): the canonical
  six positions at D1–D6 plus a 174-position corpus at D1–D4, regenerable via
  `scripts/regen-perft-fixtures.sh`. Movegen-adjacent units run an extended
  perft gate before review.
- **Property tests and fuzzing** alongside unit tests, with TDD applied broadly
  because the engine is overwhelmingly deterministic.
- **Mutation testing** (`cargo mutants`) per unit on the diff, with periodic
  full-suite backstops. Surviving mutants are treated as a defect in the code's
  shape, not as noise — see below.
- **Coverage, clippy `-D warnings`, and `cargo fmt`**, enforced continuously by
  a pre-commit hook rather than by good intentions.

**Strength gates** (only after correctness passes)

- **WAC and STS** diagnostic suites for tactical and strategic regression.
- **SPRT** against the current baseline tag over mixed time controls — baselines
  taken from historical commits, not feature flags.
- **Elo calibration vs Stockfish** at every milestone close.

All four are CPU-contention-sensitive and run sequentially; a change that claims
a strength delta does not land until the SPRT accepts it.

### What actually needed steering

The interesting failures were not "the agent wrote a bug." They were judgment
gaps, and the log records them:

- **Precedent applied literally instead of generally.** A cheaper-tier coder
  didn't generalize an existing mutation-exclusion precedent to a new data file;
  roughly 150 mutants would have shipped unexamined. Caught by final review, and
  the workflow gained explicit criteria for when to flag a slice to the stronger
  tier.
- **Untestable code shapes, discovered through mutation survivors.** Twice, a
  surviving mutant turned out to be structurally unreachable from any realistic
  fixture. The fix was to change the code — extracting `negate_window` and
  `aborted_fallback_result` as named helpers with direct unit tests — rather
  than to suppress the finding.
- **A suppression I rejected.** The proposal was to silence one gap with a
  line-number-anchored `exclude_re`; any unrelated edit would shift the line and
  the rule would fail silently. Refactoring was the right answer.
- **Tooling surprised us.** A worktree-isolated A/B silently materialized at the
  parent commit, so both arms rewrote work from scratch and the comparison
  measured more than it intended. Recorded rather than quietly discarded.
- **Model tiers are aliases, and aliases float.** Every calibration entry names
  what a tier resolved to on a given date. They are evidence with an expiry —
  the protocol is to re-run the A/B, not to extrapolate from an old entry.

### One deliberate constraint

We do not read other chess engines' source code, even for prior-art research,
even when a wiki article links to a specific line (ADR-0003). Papers, wiki
articles, forum threads and blog posts are in bounds; any engine's `src/` is
not. This costs real time whenever prose is ambiguous where a reference
implementation would settle it in seconds. It buys first-principles
understanding and provenance that is clean by construction.

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

- [`docs/workflow.md`](docs/workflow.md) — how the agent loop, reviews and gates actually run
- [`docs/model-calibration-log.md`](docs/model-calibration-log.md) — dated A/B evidence behind each role's model tier
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
