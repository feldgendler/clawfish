# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M4 complete — aspiration windows landed at M4.D; ELOH tooling milestone landed in parallel (in-process Elo-iteration harness with `VirtualClock` UCI option); M5 (search-advanced: NMP / LMR / futility / staged movegen) is next.** Architectural commitments settled (see `docs/decisions/`). M4.D's mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history` validated 2026-04-30: 138-90-172 in 400 games / 200 pairs, Δ Elo **+41.89 [+18.18, +65.61]** (pentanomial 95% CI), `Ptnml(0-2) = [5, 40, 78, 56, 21]`. The `baseline/alpha-beta-tt-killer-history-aspiration` tag (at `2d0decd`, M4.D threshold-6 follow-up) is the M5 SPRT reference.

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
| M4.A | ✓ | Zobrist-keyed transposition table (`UnsafeCell<Vec<TtEntry>>` + 16-byte entry; depth-preferred + age-bias replacement; mate-score depth-adjustment); `Hash` UCI option; `Engine::reset_for_new_game` consolidated. ADR-0018. Bench: `bench: 39964046 nodes <NPS> nps` (-77% vs M3.F). SPRT vs `baseline/alpha-beta-no-tt`: +281.89 ± 55.64 Elo. |
| M4.B | ✓ | Two killer slots per ply (`[[Move; 2]; MAX_PLY]` with `Move::default()` sentinel); `is_quiet`/`update_killers`/`negamax_move_order_score`/`order_moves`/`clear_killers` helpers extracted per the M3.D `negate_window` precedent; killers updated on quiet beta cutoffs and folded into negamax move ordering between captures and remaining quiets. Bench: `bench: 22237579 nodes <NPS> nps` (-44% vs M4.A). SPRT vs `baseline/alpha-beta-tt`: 148-8-2 (94.30%), Δ Elo **+487.58 ± 125.42** in 158 games / 12m20s. |
| M4.C | ✓ | Butterfly history table `[side][from][to]` i16 (`MAX_HISTORY = 16384`, literature standard); `+= depth*depth` bonus on quiet beta-cutoff with explicit clamp; matching `-= depth*depth` malus on prior quiets in `quiets_searched`. Layered into `negamax_move_order_score` as the comparator score for non-killer quiets in `[-MAX_HISTORY, MAX_HISTORY]`. To preserve the captures > killers > history-quiets hierarchy, M4.B's `KILLER0_SCORE = 200` and `KILLER1_SCORE = 100` were bumped to `100_001` / `100_000`, and the comparator's non-quiet path adds `CAPTURE_OFFSET = 1_000_000` to `mvv_lva_score`. ADR-0019. Bench: `bench: 17650332 nodes <NPS> nps` (-20.6% vs M4.B). SPRT vs `baseline/alpha-beta-tt-killer` @ tc=60+0.6: 164-123-113 (55.12%), Δ Elo **+35.74 ± 27.51**, LOS 99.49% in 400 games (load-bearing); fast-TC @ tc=10+0.1 inconclusive (+3.47 ± 5.88). |
| M4.D | ✓ | Aspiration windows over the M3.E ID outer loop. `aspiration_window` + `widen_after_fail` + `extract_bestmove_or_tt_fallback` pure helpers in `src/search.rs`; two-tier asymmetric widening at ±50 cp first try, fail-soft proved-bound preserved on the unfailed side; **depth ≥ 6 threshold** (raised from initial 4 after empirical tc=10+0.1 SPRT showed threshold=4 regressed −22.62 ± 21.70 Elo while threshold=6 gained **+65.92 ± 26.52 Elo (LOS 100%)** vs `baseline/alpha-beta-tt-killer-history`); killers persist across re-searches within an iteration; empty-PV-after-fail-high recovered via TT fallback (ADR-0018 §7's best-move-on-overwrite preservation). `Move::from_bits` constructor added. `info string aspiration_re_search` test instrumentation. Bench: `bench: 15863206 nodes <NPS> nps` (-10.1% vs M4.C). Mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history` (first ELOH.D consumer, 4-bucket uniform `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`): 138-90-172 in 400 games / 200 pairs, Δ Elo **+41.89 [+18.18, +65.61]** (pentanomial 95% CI). |
| ELOH.A | ✓ | In-process Elo-iteration harness foundation: persistent subprocesses, UCI driver, native adjudication, per-side wallclock TC, color-paired match loop, PGN/summary output. ~2300 LOC + 51 tests. ADR-0020. |
| ELOH.B | ✓ | Statistical layer: Robbins-Monro K-update, σ-stopping, N-parallel concurrency, threshold adjudication, convergence-progress output. Replaced `scripts/elo-iterate.sh`. ~5917 LOC total binary + 139 tests. |
| ELOH.C | ✓ | `VirtualClock` UCI option + harness `--virtual-clock` flag + handshake negotiation. Engine worker uses `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` when active; `SearchClock` is worker-local (per-thread invariant). `--go-nodes N` dropped (implementation-coupled). ADR-0021. ~190 prod + ~140 test LOC. Back-test gate Part 1 deferred to post-merge manual run. |
| ELOH.D | ✓ | Per-pair TC sampling (`--tc-sample <SPEC>` + `--seed N`) for mixed-TC SPRT and Δ(TC) regression. SplitMix64 master stream pre-materialises all per-pair TCs at run start (`Vec<(TimeControl, TimeControl)>` indexed by pair_index) so sampler advance is deterministic regardless of subprocess scheduling under N>1 concurrency. PGN `TimeControl` tag + per-game `tc=` summary field + `summary-by-tc:` aggregate (input-spec order) record sampled TCs. Mutually exclusive with `--tc` at parse time. K-update math unchanged. ~245 LOC. Back-test Part 1 (chi-squared sampler) in-tree; Part 2 (degenerate single-TC self-back-test) deferred to post-merge manual run. **Closes ELOH milestone.** |
| Tooling/fuzzing | ✓ | `cargo-fuzz` harnesses for FEN + UCI (ADR-0013); 3.17B execs aggregate across saturation + smoke campaigns, one real parser bug found and fixed. |

For per-phase detail (what landed, verification numbers, lessons), see [`docs/milestones/`](docs/milestones/). The forward-looking milestone plan lives in [`docs/roadmap.md`](docs/roadmap.md).

### What's next

**M5 — search-advanced** (NMP, LMR, futility, singular extensions, staged movegen, qsearch-in-TT, third aspiration tier). Each gated by its own SPRT against the prior phase's baseline tag (`baseline/alpha-beta-tt-killer-history-aspiration`, now tagged at the M4.D-shipping tip). Detail in [`docs/roadmap.md`](docs/roadmap.md).

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
