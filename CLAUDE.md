# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M4.A complete (TT landed); M4.B — killer moves — is next.** Architectural commitments settled (see `docs/decisions/`).

| Phase | Status | One-line summary |
|---|---|---|
| M1.A–M1.G | ✓ | Bitboards, FEN, magic attacks, Zobrist, make/unmake, legal-direct movegen, perft (119 Mnps bulk D4). |
| M2.A–M2.E | ✓ | UCI move encoding + parser + I/O loop (ADR-0011) + RandomMover + fastchess harness (compliance 40/40). |
| M3.A | ✓ | `eval` module + `GreedyMover` depth-1 search; PeSTO MG values + incremental `static_eval_white` (ADR-0014). |
| M3.B | ✓ | `Engine::game_history` + `is_repetition` + `is_fifty_move_draw` helpers (plumbing only). |
| M3.C | ✓ | `AlphaBetaMover` (fail-soft negamax + triangular PV + MVV-LVA + mate-distance pruning) per ADR-0016; 20-0 vs `baseline/material-greedy` at depth 4; 8.55 Mnps depth-8 startpos. |
| M3.D | ✓ | Quiescence search at the negamax horizon; `negate_window` helper closes a structural mutation gap; 10.87 Mnps depth-8 startpos (+27% vs M3.C). |
| M3.E | ✓ | Iterative deepening + time management per ADR-0017; `compute_caps` pure function; `MoveOverhead` UCI option; `prior_root_move` ordering hint at ply==0; `aborted_fallback_result` helper. |
| M3.F | ✓ | `bench` UCI command (signature `bench: 172312700 nodes 11489045 nps`); `scripts/sprt.sh sprt\|match\|rating-estimate` wrapper + `scripts/elo-iterate.sh` online iterator; SPRT vs RandomMover **148-0-0 H1 accepted**; rating estimate via online Elo iteration converged to **~2114 Elo at tc=10+0.1** (±35 Elo, P-core pinned, 120 games); `compute_bench_nps` helper closes 3 mutation gaps. Closes M3. |
| Tooling/fuzzing | ✓ | `cargo-fuzz` harnesses for FEN + UCI (ADR-0013); 3.17B execs aggregate across saturation + smoke campaigns, one real parser bug found and fixed. |

For per-phase detail (what landed, verification numbers, lessons), see [`docs/milestones/`](docs/milestones/). The forward-looking milestone plan lives in [`docs/roadmap.md`](docs/roadmap.md).

### What's next

**M4 — search basics.** Transposition table (Zobrist), move ordering refinements (PV move, killer moves, history heuristic), aspiration windows, PVS. Each addition gated by SPRT against `baseline/alpha-beta-no-tt` (the tag added at M3.F end). Detail in [`docs/roadmap.md`](docs/roadmap.md).

## How to pick up a new session

1. Read this file (auto-loaded).
2. Read `docs/architecture.md` for current architectural state.
3. Read `docs/roadmap.md` for milestone status and what's next.
4. Skim `docs/milestones/` for retrospectives on landed phases (when context on a recent landing matters).
5. Read `docs/workflow.md` for how we collaborate.
6. Skim `docs/decisions/` for the *why* behind specific commitments.
7. Skim `docs/prior-art.md` for reference landscape — grows over time as we research each component.
8. Check git log if a repo exists.

## Ground rules (load-bearing — do not relax)

### User profile
- Chess strength ~1000 Elo. Casual player, serious engineer.
- **Does not know Rust, by design.** The language choice is a self-imposed gatekeeper to keep the work vibe-coded. The user will not be inspecting code line-by-line; chat explanations are the primary signal. Be unusually rigorous in chat about explaining decisions and surfacing risks, since bugs cannot be caught by reading.

### Interaction conventions
- **When asking the user questions, use the `AskUserQuestion` tool.** Plain-text questions in chat are easy to miss and lack a structured-choice UI; the tool surfaces the question with its options as discrete picks. Applies to clarifying requirements, choosing between approaches, or any other decision-soliciting prompt — not to status updates or summaries.
- **Project-specific rules go in the project directory, not agent memory.** Agent memory at `~/.claude-personal/projects/<slug>/memory/` is per-machine and does not travel with the repo — checking the project out on another machine loses it. Anything that should apply to the project on any machine, to any agent session, belongs in `CLAUDE.md` / `docs/workflow.md` / `docs/architecture.md` / an ADR. Memory is reserved for transient session state.

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
- `docs/roadmap.md` — forward-looking milestone plan, per-phase status, what's next.
- `docs/milestones/` — per-phase retrospectives (what landed, highlights, verification numbers). Historical reference, not load-bearing.
- `docs/workflow.md` — collaboration loop, TDD scope, benchmarking conventions.
- `docs/prior-art.md` — reference landscape; per-feature research notes accumulate here.
- `docs/decisions/` — ADR-style records, one file per substantive decision.
- `docs/reference/` — vendored authoritative specs (FIDE Laws of Chess, UCI protocol).
- `docs/tooling-backlog.md` — prioritized list of tooling/QA items not yet adopted. Pull from the top when a tooling slot opens.

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
