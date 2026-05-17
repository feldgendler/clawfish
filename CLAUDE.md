# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**M6.C landed 2026-05-17 (passed-pawn infra, score-neutral — all weights → M6.F).** Production HEAD = `M6.C`. M6.C ships `passed_pawn_term_white` (per-passer rank bonus + EG three-state path discriminator + EG king-tropism, reading M6.B's cached `passed[2]`, computed live in `evaluate_core` — never cached, ADR-0032 §3) with **every passed weight zeroed** in `eval::data` (`PASSED_MG/EG/FREE_EG_DELTA/KDIST_*` = 0; `PASSED_KDIST_CAP=5` kept as structural clamp; term math live at zero weight, M6.F-ready — the M6.B `PAWN_STRUCTURE_IN_EVAL` precedent). A three-config screen ladder vs `M6.B` (canonical mixed-TC, elo0=0/elo1=5, RUN ALONE) proved the literature defaults have a **scale-invariant structural mismatch** with this engine: all-three **−21.74** (60+0.6 W15-L57 KDIST slow-TC collapse — R2); {RANK+PATH} **+4.34** (10+0.1 W30-L59 RANK+PATH fast-TC over-magnitude vs our PeSTO EG pawn PST — R1); {RANK+PATH}/2 **−0.87** (failure *migrates* to 20+0.2 — the M6.B `(ISO+CONN)/2` plateau reproduced; a global scalar can't make a wrong-shaped term non-negative across the TC profile). No positive interaction-immune subset (contrast M6.B CONN-only +103 H1) ⇒ entire weight set deferred to M6.F. **Landing gate (M6.B inert-landing precedent): zeroed weights ⇒ term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.B` ⇒ bench `1213649`/depth-4 `90591` byte-for-byte ⇒ provably inert ⇒ no confirmation SPRT.** Live-term same-campaign WAC/STS (+151 STS, #9 "Advancement a/b/c" +58) is a *rejected-config* diagnostic — the term is directionally correct, mis-scaled not mis-designed. ADR-0032 §8. M6.B landed 2026-05-16 (CONN-only, Δ Elo +45.42 vs `M6.A`); M6.A landed 2026-05-15 (Δ Elo +250.57 vs M5.H1).

**Next phase: M6.D (piece mobility)** — N/B/R/Q pseudolegal-attack-square count ∩ mobility area; no code dependency on M6.C. See [`docs/roadmap.md`](docs/roadmap.md) §M6, [`docs/milestones/m6.c.md`](docs/milestones/m6.c.md) for the M6.C retrospective (the *co-calibrated-elsewhere ≠ transfers here* / TC-localized-collapse-is-the-over-scaled-EG-fingerprint / co-scale-probe-says-reshape-not-rescale lessons + the illegal-fixture-detector finding), and [`bench/m6.md`](bench/m6.md). **M6.F obligation (extended): the joint-Texel pass now also re-derives the passed-pawn rank table against our PeSTO EG pawn PST and *reshapes* (not rescales) king-distance — jointly with the M6.B ISO/DBL/BWD re-introduction + CONN rescale** (one joint pass; until then M6.B–E's zeroed pawn terms stay inert). ADR-0032 §7/§8.

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
