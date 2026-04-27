# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M2.A complete; M2.B next.** Architectural commitments settled (see `docs/decisions/`).

- M1: complete through M1.G (perft + criterion benchmark harness; 119 Mnps bulk on starting D4).
- M2.A: complete — `Move::to_uci` + `Move::from_uci` + `UciMoveError`. Generate-and-match parsing strategy; null move `0000` rejected as `NullMove`; strict lowercase input. No new ADR.
- M2: B–E remain. Pre-M2 research complete: `docs/research/m2-uci-threading.md` (binds ADR-0011 on M2.C) and `docs/research/m2-tournament-harness.md` (binds ADR-0012 on M2.E).

### What M2.A landed

- **`Move::to_uci(self) -> String`** in `src/mov.rs`. Thin wrapper around the existing `Display` impl (canonical writer); both produce identical bytes. Self-documenting at M2 protocol call sites.
- **`Move::from_uci(s: &str, pos: &Position) -> Result<Move, UciMoveError>`** in `src/mov.rs`. Generate-and-match: enumerates `generate_moves(pos)`, finds the unique move matching the parsed `(from, to, promotion_kind)`. Defers legality entirely to movegen (consistent with ADR-0007).
- **`UciMoveError`** enum: `Malformed` / `IllegalPromotionPiece` / `NullMove` / `IllegalForPosition`. Implements `Display` + `std::error::Error`. Re-exported from `src/lib.rs`.
- **Tests** — 38 new test fns in `src/mov.rs::tests`: 12 `to_uci` anchors + 10 `from_uci` positive anchors (including check-evasion) + table-driven negative-parse + 10 position-dependent rejection tests + round-trip on `CASES` + round-trip on D1 enumeration of `UCI_SEED_FENS` (canonical 6 + EP-horizontal-pin + EP-double-check + mate + stalemate) + two proptests (round-trip on D2-reachable positions; `(from, to, promo)` uniqueness invariant).

### M2.A — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 484 fast + 4 ignored (446 lib + 38 new M2.A; integration + doctests as before). All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` (`mov.rs`) | 96.55% region / 95.75% line / 95.20% function. Uncovered lines = pre-existing `#[ignore]`-gated bench + pre-existing `unreachable!()` arms + `unwrap_or_else` panic branches in tests that don't fire when tests pass. No M2.A-specific gaps. |
| `cargo mutants --in-diff` | 19 mutants generated; **18 caught, 1 unviable** (`from_uci -> Ok(Default::default())` — `Move` has no `Default`); 0 missed. |
| Benchmark | **Skipped** — UCI move parsing is per-`position` command, not on a search hot path; a microbenchmark would measure noise. |

All three review loops (plan, test-suite, final code+tests) converged. Plan archived at `docs/plans/m2.a.md`.

### What M1.G landed

The `perft` module — move-generation validation + measurement:

- **`src/perft.rs`** with `perft`, `perft_bulk` (CPW depth-1 leaf-skip), `divide` (UCI-sorted), `perft_categorized` (internal-only category counts per ADR-0006), and the `PerftCounts` struct.
- **Recursion driver** `perft_inner<const BULK: bool>` monomorphized over plain/bulk; the leaf-skip branch is dead-code-eliminated from the plain path.
- **Stockfish-regenerated fixtures** — canonical 6 D1–D6 + 174-position Whittington corpus D1–D4. Per ADR-0006 Stockfish 18 is the sole oracle; `scripts/regen-perft-fixtures.sh` reproduces them.
- **Test partition** — D1–D3 (all 6 positions, plain + bulk parity) + light-D4 fast (`cargo test`); heavy-D4, D5, D6, Whittington D4 `#[ignore]`-gated. Default fast suite finishes in 0.02s release.
- **`criterion 0.7` benchmark harness** — `benches/perft.rs` + `benches/movegen.rs`; first baseline at `bench/m1.g.md`. Format ratified by **ADR-0010**.
- **M1.F smoke benchmark deleted** — criterion supersedes it.

### M1.G — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 446 fast + 9 ignored. All ignored verified to pass: D4-heavy 0.23s; D5 + Whittington 2.47s combined; **D6 117.93s** (~22B nodes). |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo audit` + `cargo deny check` | clean |
| `cargo llvm-cov` (`perft.rs`) | 98.32% region / 98.25% line / 93.75% function. Crate total 95.19%. |
| **Headline throughput (`bench/m1.g.md`)** | starting D4 plain **33 Mnps**; starting D4 bulk **119 Mnps**; Kiwipete D3 bulk **168 Mnps**. Meets M1 ≥100 Mnps exit criterion on bulk path. |

All four loops (plan, test-suite, final code+tests, benchmark capture) converged. Plan archived at `docs/plans/m1.g.md`. ADR-0010 codifies the bench-baseline format.

### What's next

**M2.B — UCI command parser.** New `uci` module: `Command` enum + `parse_uci_line(&str) -> Command` covering the GUI→engine command set (`uci`, `isready`, `setoption`, `ucinewgame`, `position`, `go`, `stop`, `quit`, etc. per UCI spec). Returns `Command::Unknown` for unrecognized tokens (per spec "ignore unknown command or token"). Property tests for whitespace tolerance and grammar coverage.

- No ADR binds on this phase.
- Approx size: 600–900 lines.
- M2.A's `from_uci` is a dependency for parsing the `position … moves m1 m2 …` command's move list.

Full M2 plan and the three remaining sub-phases (M2.C–E) are in `docs/roadmap.md`. Exit criteria for M2 as a whole: complete game through `fastchess` against itself or another engine without protocol errors or illegal moves.

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

### Interaction conventions
- **When asking the user questions, use the `AskUserQuestion` tool.** Plain-text questions in chat are easy to miss and lack a structured-choice UI; the tool surfaces the question with its options as discrete picks. Applies to clarifying requirements, choosing between approaches, or any other decision-soliciting prompt — not to status updates or summaries.

### Domain code restrictions
- **No third-party chess-domain code.** Move generation, search, eval, NNUE inference, opening-book parsers — all written from scratch.
- **No reading chess-themed source code online.** Even for inspiration. The user does not want me influenced by existing engine implementations. Browsing `github.com/.../engine/src/` is out, *even when wiki articles link to it*. The Chess Programming Wiki itself, papers, blog posts, TalkChess discussions, and articles containing illustrative code snippets are all fine — the prohibition is on browsing the *source repos* of existing engines (Stockfish, Fairy-Stockfish, Leela, any open-source Rust engine, etc.).
- **General-purpose libraries are fine**: data structures, test harnesses, parallelism primitives, CLI parsers, profiling, serialization.
- **Public chess data is encouraged**: perft suites, opening books (data files), Syzygy endgame tablebases, PGN game databases, eval test suites (STS, Bratko-Kopec, WAC).

### Workflow loop (per feature or major component)
Loop: **research → choose approach → plan → tests → implement → final review → benchmark → commit.** Runs **unattended by default** — a session goes end-to-end on a single prompt ("plan and implement M1.X") without proactive pauses. Each of the plan, test suite, and final code+tests goes through a **blind-review loop** with a fresh subagent (continued via `SendMessage` across iterations); reviewer convergence is the gate, not user approval. Plans must identify parallelization opportunities; implementation runs in parallel across coding agents per the plan. Reviewer concerns surface in chat as informational — the user can override by interjecting; absent intervention the agent proceeds.

If the agent gets genuinely stuck (ambiguous spec, hard tool failure, architectural fork contradicting ADRs), it surfaces the issue and pauses. "Uncertain" is not stuck — pick the most defensible path, note alternatives in chat, continue. See `docs/workflow.md` for the full structure and reviewer dimensions.

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
- `docs/tooling-backlog.md` — prioritized list of tooling/QA items not yet adopted. Pull from the top when a tooling slot opens.

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
