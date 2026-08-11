# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Production HEAD = `M6.J` (eval) + `M8.A.1` (search).** Bench d4 `45788` / d7 `662085`. The current rating estimate lives in [`README.md`](README.md)'s Status block; per-milestone numbers in [`bench/`](bench/).

**M8.A.1 — depth-conditioned PVS, shipped 2026-06-26.** Principal Variation Search with the scout gated on a **smooth monotonic ramp over the ID root depth**: a non-first move at move-ordering rank `cur_i` is scouted iff `cur_i >= pvs_scout_start(root_depth)`, which is `MAX` (no scout at all) at root_depth ≤ `D0=12` and falls to `1` (full PVS) at ≥ `D1=16` (`BASE=16`, `SLOPE=4` ⇒ d13→12, d14→8, d15→4). Below d13 the ramp is inert, so the engine is **byte-identical to `M7.B.2`** there — fast-TC protection by construction rather than by measurement. 2-seed SHIP vs `M7.B.2`: **+30.48 [+9.63, +51.55]** and **+26.98 [+3.32, +50.89]**, both CI-lower > 0, with the gain concentrated at 60+0.6 (76-26-82 = 63.6%, ≈ +97). Resolves M8 (Search refinements I).
ADR-0044 · [plan](docs/plans/m8.a.1.md) · [research](docs/research/m8.a.1-depth-conditioned-pvs.md) · [SPRT](bench/sprt/2026-06-26-m8a1-depth-conditioned-pvs-vs-m7b2.md) · [retrospective](docs/milestones/m8.a.1.md)

**Shelved on unmerged branches — do not delete:**

- `m8a-pvs` — M8.A, unconditioned PVS. Net ≈ −20 Elo, depth-amplifying the wrong way; superseded by M8.A.1's ramp. ADR-0043.
- `m7c-see-capture-futility` — M7.C, SEE capture-futility / delta pruning. Both variants failed SPRT (v1 ≈ −1.7 Elo, v2 −49 with a 60+0.6 collapse). ADR-0042.

Earlier phases — M5.F.1, M5.K, M6.I/J/K, M7.A/B/B.2/C, M8.A — have retrospectives in [`docs/milestones/`](docs/milestones/) ([index](docs/milestones/README.md)) and status in [`docs/roadmap.md`](docs/roadmap.md); their SPRT records are in [`bench/sprt/`](bench/sprt/). Open levers: [`docs/tuning-backlog.md`](docs/tuning-backlog.md), [`docs/tooling-backlog.md`](docs/tooling-backlog.md).

### Standing lessons (earned, load-bearing)

- **Depth-conditioned mechanisms get a smooth monotonic ramp, never a hard band.** M5.K's `[8,12]` aspiration depth-gate was refuted at **−34.9 Elo over 800 games**, and the regression landed in the 20+0.2 bucket the gate was meant to protect. M7.B.2 and M8.A.1 both work because the ramp is smooth and its off-regime is byte-identical to the proven-safe baseline.
- **Two independently-SPRT-validated changes to the same subsystem are not additive.** M5.F.1 and M5.F.3 each measured +37.49 alone; combined they were −58 at 40+0.4. Always combined-confirm same-subsystem multi-ships.
- **SEE's Elo in this engine is in *pruning*, not ordering.** The qsearch prune converted (M7.B/M7.B.2, +92–124); ordering-by-SEE (M7.A) and negamax-frontier capture-futility (M7.C) both measured flat-to-negative.
- **Only within-campaign EPD deltas are valid.** M6.K's apparent "+43 King Activity" was cross-campaign noise — the same binary's baseline swung 10205 → 10646 between runs. STS is diagnostic-only, never the gate.
- **Read the per-TC profile, not just the aggregate.** Several shipped gains regress at 10+0.1 and dominate at 60+0.6; M7.B did the inverse. Catching this is what the mixed-TC mandate (ELOH.D) exists for.

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
- **Long-running work gets an ETA up front and hourly progress/ETA updates.** Whenever the agent kicks off anything long-running (SPRT runs, multi-hour mutation sweeps, corpus builds, overnight campaigns, etc.), surface an absolute-clock ETA **at the start**, then surface a progress + refreshed-ETA update **every hour** while it remains in flight (absolute clock time per the rule above). Implement the hourly cadence with an hourly self-wakeup (`ScheduleWakeup`, 3600 s) scoped to active long-running work — stop scheduling once it completes or the user pauses. Per-task completion notifications are separate from (not a substitute for) the hourly heartbeat.

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
