# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M0 complete; M1 planning.** Architectural commitments settled (see `docs/decisions/`). Cargo project initialized; `cargo run` produces a hello-world binary. Stockfish 18 installed via Homebrew per `decisions/0006`. M1 prior-art research complete (three reports in `docs/research/`, synthesized into `docs/prior-art.md`). M1 is decomposed into seven sub-phases M1.A–M1.G in `docs/roadmap.md`; each goes through plan mode with the blind-review loop per `docs/workflow.md`. ADRs 0007/0008/0009 are pre-decided (material in prior-art and research files) but written per-phase as each binds — none binds on M1.A. Next: enter plan mode for M1.A.

## How to pick up a new session

1. Read this file (auto-loaded).
2. Read `docs/architecture.md` for current architectural state.
3. Read `docs/roadmap.md` for milestone status and what's next.
4. Read `docs/workflow.md` for how we collaborate.
5. Skim `docs/decisions/` for the *why* behind specific commitments.
6. Skim `docs/prior-art.md` for reference landscape — grows over time as we research each component.
7. Check git log if a repo exists.

## Ground rules (load-bearing — do not relax)

### User profile
- Chess strength ~1000 Elo. Casual player, serious engineer.
- **Does not know Rust, by design.** The language choice is a self-imposed gatekeeper to keep the work vibe-coded. The user will not be inspecting code line-by-line; chat explanations are the primary signal. Be unusually rigorous in chat about explaining decisions and surfacing risks, since bugs cannot be caught by reading.

### Domain code restrictions
- **No third-party chess-domain code.** Move generation, search, eval, NNUE inference, opening-book parsers — all written from scratch.
- **No reading chess-themed source code online.** Even for inspiration. The user does not want me influenced by existing engine implementations. Browsing `github.com/.../engine/src/` is out, *even when wiki articles link to it*. The Chess Programming Wiki itself, papers, blog posts, TalkChess discussions, and articles containing illustrative code snippets are all fine — the prohibition is on browsing the *source repos* of existing engines (Stockfish, Fairy-Stockfish, Leela, any open-source Rust engine, etc.).
- **General-purpose libraries are fine**: data structures, test harnesses, parallelism primitives, CLI parsers, profiling, serialization.
- **Public chess data is encouraged**: perft suites, opening books (data files), Syzygy endgame tablebases, PGN game databases, eval test suites (STS, Bratko-Kopec, WAC).

### Workflow loop (per feature or major component)
Loop: **research → discuss → plan → tests → implement → final review → benchmark.** Each of the plan, the test suite, and the final code+tests goes through a **blind-review loop** with a fresh subagent (continued via `SendMessage` across iterations). Plans must identify parallelization opportunities for coding agents. Implementation may proceed in parallel across coding agents per the plan. See `docs/workflow.md` for the full per-feature loop, the dimensions checked at each review, and the rationale for each step.

Skipping research/discussion or any review loop strips the user of his only architectural review channels. He does not read code. When in doubt, propose before implementing.

### Architectural commitments (settled — see `docs/decisions/`)
- Rust.
- Single-variant codebase: standard chess only. No abstraction overhead for variants.
- Bitboards, `u64`. Magic bitboards for sliding pieces.
- Classical eval first; NNUE is a planned milestone, not a "maybe." `make_move` / `unmake_move` structured as single function calls with a clean interception point so an incremental accumulator can be added without surgery.
- **Configurable strength reduction is a planned milestone.** Same hook as NNUE — eval and move selection as discrete function calls — costs nothing to preserve.
- Parallelism not from day one, but design must accommodate it.
- UCI protocol.
- **Primary platform: Apple Silicon (ARM64) macOS.** Mobile is a downstream toy target — slower/weaker is acceptable there.

### Verification standards
- TDD with perft for the rules layer.
- Property tests for search invariants.
- SPRT for any change claiming a strength impact, once the engine plays games.
- Benchmark and profile every feature.

## Documentation map

- `CLAUDE.md` (this file) — index, rules, current status. Auto-loaded.
- `docs/architecture.md` — current architectural state.
- `docs/roadmap.md` — milestone plan, current progress.
- `docs/workflow.md` — collaboration loop, TDD scope, benchmarking conventions.
- `docs/prior-art.md` — reference landscape; per-feature research notes accumulate here.
- `docs/decisions/` — ADR-style records, one file per substantive decision.
- `docs/reference/` — vendored authoritative specs (FIDE Laws of Chess, UCI protocol).

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
