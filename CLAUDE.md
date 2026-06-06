# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Production HEAD = `M6.J` (eval) + `M5.K` (search) — adaptive aspiration, shipped 2026-06-06** (bench d4 `112020` / d7 `1326598`; search-layer change, eval untouched; commit `55c8fae`). **M5.K** flips `Aspiration_Adaptive` ON by default — the ungated delta-baseline adaptive first-try window `half = clamp(K·|score(d-1)−score(d-2)|, MIN, MAX)` (K=2 / MIN=25 / MAX=250, depth band `[6,64]`); the fixed-±50 path stays reachable via `setoption Aspiration_Adaptive value false`. **SPRT vs `M5.F.1`: Δ Elo +13.03 [−3.78, +29.91]**, 2-seed-confirmed (seed1 +20, seed2 +6) — **rung-2 "ship-with-note"** per the ADR-0037 ladder (mean +13 ≥ 5, CI-lower −3.78 > −10; strict CI-lower>0 not met), user-approved. It is the **confirmed CEILING** for the mechanism: both improvement levers were explored and **failed** — SPSA-tune of K/MIN/MAX (empirically low-signal: aspiration is a node-efficiency knob, ~zero per-game gradient — 2026-06-06) and a `[8,12]` **depth-gate** (refuted, −34.9 Elo over 800 games, both seeds CI-negative — the regression landed in the 20+0.2 bucket it was meant to protect; `b03c942`). **Lesson: gating an aspiration-width mechanism by depth backfired — the adaptive benefit is not cleanly depth-localizable.** The delta-baseline mechanism + runtime `Aspiration_*` UCI options landed via the SPSA-harness build ([`docs/plans/spsa-harness.md`](docs/plans/spsa-harness.md), Unit 1 `fd1be1a`); the SPSA loop itself (Unit 2 `9d734d9`) is a sound reusable harness for higher-signal future targets. For details read [`docs/milestones/m5.k.md`](docs/milestones/m5.k.md) + [`docs/plans/aspiration-depth-gate.md`](docs/plans/aspiration-depth-gate.md) + [`bench/sprt/2026-06-06-depth-gate-aspiration-vs-m5f1.md`](bench/sprt/2026-06-06-depth-gate-aspiration-vs-m5f1.md). The prior search base **`M5.F.1`** (qsearch-Exact tuning ship, shipped 2026-05-31, bench d7 `1354640`) is now superseded on the search layer by M5.K; it stored `Exact` at completed-loop qsearch nodes (`alpha_entry < best < beta`), relaxing the M5.F Stockfish-45e5e65 Lower/Upper-only rule — **sound** because negamax delegates to qsearch at depth 0 before its TT-cutoff probe, so a depth-0 qsearch-Exact is only consumed by qsearch's own (window-independent) re-probe. SPRT vs `M6.J`: **Δ Elo +37.49 [+15.71, +59.58]** (mixed-TC + virtual-clock, 400-game cap, rung-1 ship by CI), depth-amplifying (strongest at 60+0.6, per the ELOH.D mandate). **Landed via the 2026-05-31 overnight qsearch-TT tuning campaign** ([`docs/plans/tuning-m5f-m5g-overnight.md`](docs/plans/tuning-m5f-m5g-overnight.md)): a sibling Path-A-store-suppression change (M5.F.3) *also* SPRT-validated +37.49 alone but **did not compose** with M5.F.1 (combined −58 Elo at 40+0.4, seed-confirmed) and was **deferred, not shipped**; an `SE_MARGIN=2` probe (M5.G) was flat and reverted. **Lesson: two independently-SPRT-validated changes to the same subsystem (qsearch TT) are not additive — always combined-confirm same-subsystem multi-ships.** The prior eval-layer production base is unchanged — **`M6.J`** — full-N-M meta-tuned eval, **shipped 2026-05-29** (bench `1357063` / d4 `112497`; mixed-TC + virtual-clock SPRT vs `M6.I` reached `verdict=continue` at the 400-game cap, **pentanomial CI Δ Elo +41.01 [+18.26, +64.12]** — rung-1 ship by CI per ADR-0037 §9; STS same-campaign **+102 credit / +30 STS-Elo** ≫ noise; rating-estimate vs Stockfish-1320 **+919.54 [+729.89, +inf]** ≈ 2240 at mixed-TC). **M6 complete** (M6.I closed M6 structurally; M6.J is the post-factum-named meta-tune ship on top). **M6.I shipped 2026-05-25** (bench `1411314` / d4 `111498`; SPRT **H1-accept Δ Elo +93.86 [+68.97, +119.72]** vs `M6.F`, mixed-TC + virtual-clock, 618 games — Texel-tuned classical eval, activated the deferred M6.B–F terms: N/B/R/Q mobility + passed/outpost/rook-file/king-shield; king-safety attacker S-curve excluded [structural]; the gain is **depth-amplifying** (regresses at 10+0.1, dominant at 60+0.6) vindicating the ELOH.D mixed-TC mandate). **M6.J landed 2026-05-28 → SHIPPED 2026-05-29** — full Nelder–Mead meta-tuner (`b1c4b74`: reflect-only stall fix → reflect/expand/contract/shrink + softmax in R³ + Kelley restart, ADR-0037 §8 tactical update, 13 mixture tests, blind-review approved, CI-green) + cold-start retune on the M6.H2 16M-position corpus (terminated iter 8/30 via `EPS_F = 1e-6`; chose mix `[0.32, 0.15, 0.23, 0.31]` at val_loss 0.142749; `texel-tune apply` `f390b46`); SPRT process lesson — log-content Monitors must enumerate every terminal state — see `docs/workflow.md` §"Chaining unattended step-12 gates". **M6.G landed 2026-05-20** (corpus data-infra, no SPRT, no Elo claim; amended 2026-05-21 with the four-source taxonomy — `Source::{SelfPlayOnBook, SelfPlayOffBook, Ccrl, LichessOpen}` + `OpeningMode`-per-campaign, ADR-0035 §10). **M6.H landed 2026-05-22** (robust on-demand Lichess/CCRL ingestion — `corpus fetch` + `corpus::fetch`, behind the non-default `corpus-fetch` Cargo feature; functional gate, no SPRT, no engine touch ⇒ bench unchanged; ADR-0036). **M6.H2 landed 2026-05-23** (corpus-lanes refactor — four flat build-ready `lane.bin` lanes; ADR-0035 §12 / ADR-0036 §7). Some Texel-fit terms have counterintuitive SPRT-validated shapes (e.g. `ISO_MG=+5`, negative early-rank `PASSED_MG`); a sign/monotonicity-constrained retune is backlogged. **M6.K CLOSED NEGATIVE 2026-05-29 — the king-safety attacker S-curve was REMOVED after both SPRT probes regressed; production HEAD stays `M6.J` (eval byte-identical, bench `1357063` / d4 `112497`).** M6.K tried activating the attacker-count S-curve (the one M6 term the quiet-corpus Texel pipeline structurally could not tune; ADR-0038), but both mixed-TC + virtual-clock SPRTs vs `M6.J` lost: **stage 1 (g=1 literature) Δ Elo −44.54 [−72.13, −17.50]**, **stage 2 (g=0.5 deflate) ~−48** (same regression class — halving the magnitude didn't help). The M6.E HIGH-transfer-risk **double-count vs the PeSTO MG king PST is confirmed real** and persists at half magnitude (optimum g≈0), so the S-curve was deleted (eval-neutral vs `M6.J`, slightly faster; shield + open-file king-safety from M6.I untouched). Same-campaign STS agreed with the SPRT (**King Activity −33, AKPC −27**); the candidate-land "+43 King Activity / +279 STS" was a **cross-campaign-noise artifact** (movetime-EPD swung the `M6.J` baseline 10205→10646 for the same binary) — lesson: only within-campaign EPD deltas are valid, and STS is diagnostic-only, never the gate. King Activity / AKPC remain the largest residual classical-eval gaps, now deferred to NNUE (M11). For details read [`docs/roadmap.md`](docs/roadmap.md) §M6 + the milestone retrospectives in [`docs/milestones/`](docs/milestones/) (esp. [`m6.k.md`](docs/milestones/m6.k.md) for the removed S-curve experiment, [`m6.j.md`](docs/milestones/m6.j.md) for the production HEAD, [`m6.i.md`](docs/milestones/m6.i.md) for the structural M6 close).

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
