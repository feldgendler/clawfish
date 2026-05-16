# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**M6.B landed 2026-05-16 (CONN-only, Δ Elo +45.42 vs `M6.A`).** Production HEAD = `M6.B`. M6.B ships the pawn-structure infrastructure (pawn hash, pawn-only Zobrist substream, isolated/doubled/backward/connected predicates, passed-pawn detection — `passed[]` cached for M6.C) with **only the connected-pawn term active** (`PAWN_STRUCTURE_IN_EVAL = true`; `ISO/DBL/BWD` weights zeroed in `eval::data`, CONN at literature default). The original all-four literature-default SPRT vs `M6.A` was **H0 −99.88**; a per-term screen + confirmation falsified "one bad term" — every term is individually positive-to-neutral (ISO +82.7, DBL +31.4, BWD −7.5, CONN +103.1) but every multi-term subset collapses via a **catastrophic ISO×CONN double-count of the connectivity axis** (ISO+CONN alone = −197.94; the literature weights cohere only after the joint Texel co-calibration ADR-0032 §6 always assigned to M6.F). CONN-only is the largest confirmed gain and structurally interaction-immune. Landing-gate SPRT (canonical mixed-TC 4-bucket, elo0=0/elo1=5, seed `…014`): **verdict=continue@400-cap, Δ Elo +45.42 [+19.68, +71.67]** — lands by the M5.F/M5.G-v2 `continue@cap`-positive-CI outcome-ladder precedent (CI lower ≫ floors; stronger than both landed cases). **Caveat:** per-TC depth reversal (10+0.1 strong+, 60+0.6 negative) — CONN's raw magnitude over-helps shallow ordering; an M6.F watch-item. bench `1213649` / depth-4 `90591`. ADR-0032 §7. M6.A landed 2026-05-15 (Δ Elo +250.57 vs M5.H1).

**Next phase: M6.C (passed pawns)** — reads `passed[]` from M6.B's pawn-hash cache. See [`docs/roadmap.md`](docs/roadmap.md) §M6, [`docs/milestones/m6.b.md`](docs/milestones/m6.b.md) for the M6.B retrospective (the marginals-don't-predict-combinations / connectivity-axis-double-count lesson + the env-mask subset-screen method), and [`bench/m6.md`](bench/m6.md). **M6.F obligation (revised — no longer a gate-flip; the gate is live): re-introduce ISO/DBL/BWD via joint Texel against the shipped CONN-only baseline, and rescale CONN to fix the slow-TC reversal** — without it M6.B–E's zeroed pawn terms stay inert.

## How to pick up a new session

1. Read this file (auto-loaded).
2. Read `docs/architecture.md` for current architectural state.
3. Read `docs/roadmap.md` for milestone status and what's next.
4. Skim `docs/milestones/README.md` for the phase index, then individual retrospectives when context on a recent landing matters.
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
- **Times and dates use the user's local time zone** — not UTC, not the agent's host time, not relative phrasing like "3 hours ago." When the agent records or reports a timestamp, it's the user's wall clock that matters; the user can't translate UTC at read time without effort.
- **ETAs use absolute clock time, not "how long is left."** Chat messages do not carry timestamps; a "~25 min remaining" figure is meaningless when read later — the reader doesn't know when the message was written. Phrase ETAs as a clock time the user can compare against their own wall clock (e.g., "expected by ~16:50 local"). Same applies to "in 30 min" / "in a few hours" — convert to absolute time before sending.

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

- `CLAUDE.md` (this file) — index, rules, current-status pointer. Auto-loaded.
- `docs/architecture.md` — current architectural state.
- `docs/roadmap.md` — forward-looking milestone plan, per-phase status, what's next.
- `docs/milestones/` — per-phase retrospectives (what landed, highlights, verification numbers). [`README.md`](docs/milestones/README.md) is the phase index.
- `docs/workflow.md` — collaboration loop, TDD scope, benchmarking conventions.
- `docs/prior-art.md` — reference landscape; per-feature research notes accumulate here.
- `docs/decisions/` — ADR-style records, one file per substantive decision.
- `docs/reference/` — vendored authoritative specs (FIDE Laws of Chess, UCI protocol).
- `docs/tooling-backlog.md` — prioritized list of tooling/QA items not yet adopted. Pull from the top when a tooling slot opens.
- `docs/tuning-backlog.md` — prioritized list of parameter-tuning campaigns (SPRT/SPSA) deferred from feature landings. Pull from the top when a tuning slot opens.

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
