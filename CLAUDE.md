# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**M6.E landed 2026-05-19 (king-safety infra, score-neutral — all weights → M6.F).** Production HEAD = `M6.E` (eval byte-identical to `M6.D`). M6.E ships `king_safety::king_safety_term_white` (king-zone attacker S-curve gated `<2 attackers ∨ no queen` + castled pawn-shield + MG-only open/semi-open-file penalty; zone via the single-source `pub(crate) king_zone(side,ksq)` = `ring | (fwd & !from_square(ksq))`, king square excluded, 11 central / 8 back-rank / 5 corner; computed live in `evaluate_core` — never cached, not pawn-only — **ADR-0033 §6 supersedes ADR-0032 §3**'s pawn-shield-cache reservation as a correctness hazard; pawn-storm omitted, ADR-0033 §3 documented gap) with **every king-safety weight zeroed** in `eval::data` (`KING_SAFETY_IN_EVAL=true`, term math live at zero weight — the M6.B–D `*_IN_EVAL` precedent). **Unlike M6.B–D, NO SPRT screen ladder** (ADR-0033 §8 — the M6.C/M6.D divergence, owned, not papered as precedent): research transfer-risk verdict HIGH (PeSTO MG king PST already prices ~30–50 cp of castled-king safety — the dominant double-count axis, structurally the M6.D PST-double-count finding but stronger) + the M6.B→C→D three-phase law + king-safety's universal SPRT-noisiness (mixed-TC screens mandatory, ~5× M6.B's single-TC cost) + components coupled-by-design (no M6.B-style interaction-immune subset to find) ⇒ the screen's only outcome is "defer," at the highest screen cost of any M6 term ⇒ negative expected value. Roadmap §M6 line-296 satisfied **vacuously** (inert ⇒ no Elo claim ⇒ SPRT measures zero by construction — the M6.C/M6.D disposition). **Landing gate (M6.C/M6.D inert-landing precedent): zeroed ⇒ term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.D` ⇒ bench `1213649`/depth-4 `90591` byte-for-byte ⇒ provably inert ⇒ no confirmation SPRT.** ADR-0033 (binds on M6.E per roadmap §M6; supersedes ADR-0032 §3). M6.D landed 2026-05-17 (piece-mobility infra, score-neutral, == `M6.C` eval, no ADR — roadmap-committed; the all-four/per-kind/×0.5-co-scale screen proved a scale-invariant structural mismatch, dominant offender the slider-EG magnitude not the predicted N/B PST double-count); M6.C landed 2026-05-17 (passed-pawn infra, score-neutral, == `M6.B` eval, ADR-0032 §8); M6.B landed 2026-05-16 (CONN-only, Δ Elo +45.42 vs `M6.A`, ADR-0032 §7); M6.A landed 2026-05-15 (Δ Elo +250.57 vs M5.H1, ADR-0031).

**Next phase: M6.F (Texel tuning)** — the single joint-Texel pass over the cumulative A–E parameter surface against a game-result-labeled corpus. See [`docs/roadmap.md`](docs/roadmap.md) §M6 (M6.F scope detail + verdict ladder), [`docs/milestones/m6.e.md`](docs/milestones/m6.e.md) for the M6.E retrospective (the *co-calibrated-elsewhere ≠ transfers here law matured from empirical to predictive — M6.E skips the screen on a prediction, owned* / *a `pub(crate)` seam turns "tests a copy" into a real pin* / *for a score-neutral term, "symbolic tests fail vs the (0,0) stub" is the wrong TDD-red expectation; structural M6.F-revalidation is the property* / *ADR supersession belongs in the structural `## Supersedes` section* lessons), and [`bench/m6.md`](bench/m6.md). **M6.F obligation (extended again — one joint pass): re-derive, against our PeSTO PSTs, the M6.B ISO/DBL/BWD re-introduction + CONN rescale, the M6.C passed-pawn rank-table/king-distance reshape, the M6.D N/B/R/Q mobility per-kind reshape, AND the M6.E king-safety full reshape (S-curve table + per-kind attacker weights + shield + open-file + MG/EG split; the §7 pawn-shield × PeSTO-king-PST double-count is the load-bearing input).** Until M6.F, M6.B–E's zeroed pawn/mobility/king-safety terms stay inert by design. ADR-0032 §7/§8 + ADR-0033 §7/§8 + the roadmap §M6 + `docs/milestones/m6.e.md`.

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
