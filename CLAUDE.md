# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Production HEAD = `M6.J`** — full-N-M meta-tuned eval, **shipped 2026-05-29** (bench `1357063` / d4 `112497`; mixed-TC + virtual-clock SPRT vs `M6.I` reached `verdict=continue` at the 400-game cap, **pentanomial CI Δ Elo +41.01 [+18.26, +64.12]** — rung-1 ship by CI per ADR-0037 §9; STS same-campaign **+102 credit / +30 STS-Elo** ≫ noise; rating-estimate vs Stockfish-1320 **+919.54 [+729.89, +inf]** ≈ 2240 at mixed-TC). **M6 complete** (M6.I closed M6 structurally; M6.J is the post-factum-named meta-tune ship on top). **M6.I shipped 2026-05-25** (bench `1411314` / d4 `111498`; SPRT **H1-accept Δ Elo +93.86 [+68.97, +119.72]** vs `M6.F`, mixed-TC + virtual-clock, 618 games — Texel-tuned classical eval, activated the deferred M6.B–F terms: N/B/R/Q mobility + passed/outpost/rook-file/king-shield; king-safety attacker S-curve excluded [structural]; the gain is **depth-amplifying** (regresses at 10+0.1, dominant at 60+0.6) vindicating the ELOH.D mixed-TC mandate). **M6.J landed 2026-05-28 → SHIPPED 2026-05-29** — full Nelder–Mead meta-tuner (`b1c4b74`: reflect-only stall fix → reflect/expand/contract/shrink + softmax in R³ + Kelley restart, ADR-0037 §8 tactical update, 13 mixture tests, blind-review approved, CI-green) + cold-start retune on the M6.H2 16M-position corpus (terminated iter 8/30 via `EPS_F = 1e-6`; chose mix `[0.32, 0.15, 0.23, 0.31]` at val_loss 0.142749; `texel-tune apply` `f390b46`); SPRT process lesson — log-content Monitors must enumerate every terminal state — see `docs/workflow.md` §"Chaining unattended step-12 gates". **M6.G landed 2026-05-20** (corpus data-infra, no SPRT, no Elo claim; amended 2026-05-21 with the four-source taxonomy — `Source::{SelfPlayOnBook, SelfPlayOffBook, Ccrl, LichessOpen}` + `OpeningMode`-per-campaign, ADR-0035 §10). **M6.H landed 2026-05-22** (robust on-demand Lichess/CCRL ingestion — `corpus fetch` + `corpus::fetch`, behind the non-default `corpus-fetch` Cargo feature; functional gate, no SPRT, no engine touch ⇒ bench unchanged; ADR-0036). **M6.H2 landed 2026-05-23** (corpus-lanes refactor — four flat build-ready `lane.bin` lanes; ADR-0035 §12 / ADR-0036 §7). Some Texel-fit terms have counterintuitive SPRT-validated shapes (e.g. `ISO_MG=+5`, negative early-rank `PASSED_MG`); a sign/monotonicity-constrained retune is backlogged. The king-safety attacker-count S-curve remains structurally excluded (`tuning-backlog.md`); STS King Activity 555 / AKPC 628 are the largest residual eval gaps — both lifted modestly from M6.I (King Activity +38, AKPC +38) but a non-zero-multipliers SPSA campaign is the proper fix. For details read [`docs/roadmap.md`](docs/roadmap.md) §M6 + the milestone retrospectives in [`docs/milestones/`](docs/milestones/) (esp. [`m6.j.md`](docs/milestones/m6.j.md) for the production HEAD, [`m6.i.md`](docs/milestones/m6.i.md) for the structural M6 close).

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
