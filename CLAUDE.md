# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M5.C late move reductions landed 2026-05-05 with mixed-TC SPRT vs `baseline/m5b-rfp` (H1 accepted in 144 games / 72 pairs; pentanomial Δ Elo **+145.47 [+100.12, +196.09]**; all four TC buckets positive with strong slow-TC amplification — 60+0.6: 78.9%, 40+0.4: 81.9%) and Stockfish UCI_LimitStrength rating estimate (**2657.44 ± 16.49** mixed-TC, **+5 Elo over M5.B's 2652.43**). Bench `bench: 1651610 nodes <NPS> nps` (**-50.8%** vs M5.B's 3355270). Mutation testing campaign: 94 mutants, 86/92 catch rate (93.5%), 6 survivors triaged in landing commit. M5.B reverse futility pruning landed 2026-05-02 with mixed-TC SPRT (W-L-D 94/32/74 in 200 games / 100 pairs; score 65.5%; logistic Elo +111; H1 not formally crossed at `elo1=5` due to too-narrow bound, but signal unambiguous — adopted on score-based decisioning) and Stockfish UCI_LimitStrength rating estimate (2652.43 ± 14.55 mixed-TC, fast-TC-weighted, **+51 Elo over M5.A**); M5.A NMP landed 2026-05-01; M4 fully closed (aspiration at M4.D); ELOH tooling milestone closed at ELOH.E.** Architectural commitments settled (see `docs/decisions/`). The `baseline/m5c-lmr` tag (at `0f9bd88`, M5.C landing) is the next milestone's SPRT reference. The `baseline/m5<letter>-<feature>` shorter convention is the M5 milestone's adopted form.

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
| M5.A | ✓ | Null-move pruning. `make_null_move` / `unmake_null_move` + `NullUndo` (no `prior_fullmove` — derived from STM per `unmake_move:803` precedent) in `src/mov.rs`; `null_move_reduction(depth) = 2 + depth/6` and `has_non_pawn_material` helpers in `src/search.rs`; NMP block in `negamax` at step 9 (renumbered at M5.B; was step 8) with seven-condition gate (`ply > 0` defense-in-depth + `allow_null` + `!is_pv` + `depth >= 3` + `!in_check` + `has_non_pawn_material` + `static_eval >= beta`). Mate-cap on cutoff (`null_score >= MATE_IN_MAX_PLY → beta`); TT store as `Bound::Lower` at current depth with `best_move = 0` and mate-capped score. `negamax` signature gains `allow_null: bool` after `is_pv` (~45 call sites mechanically migrated). `#[cfg(test)] nmp_firings` counter for direct stacked-null kill. ADR-0023. Bench: `bench: 5345534 nodes <NPS> nps` (-66.3% vs M4.D). Mixed-TC SPRT vs `baseline/alpha-beta-tt-killer-history-aspiration` (4-bucket uniform `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`): **H1 accepted in 22 games / 11 pairs**, Δ Elo **+400.00 [+285.52, +677.51]** (pentanomial 95% CI), pentanomial `[0, 0, 0, 4, 7]` (zero losses), 18-0-4, all four TC buckets positive. ~2 min wallclock — extreme signal triggered minimum-viable SPRT termination. |
| M5.B | ✓ | Reverse futility pruning. New step 8 in `negamax` (before NMP step 9). `reverse_futility_margin(depth) = 100 * depth` helper + `RFP_MAX_DEPTH = 6` / `RFP_MARGIN_PER_DEPTH = 100` constants in `src/search.rs`. RFP block gate: `ply > 0 && !is_pv && !in_check && depth <= 6 && beta.abs() < MATE_IN_MAX_PLY`; on `static_eval - margin >= beta`, returns `static_eval - margin` (fail-soft proved lower bound). No TT store. Lazy-dup `static_eval` read (not shared with NMP — preserves M5.A semantics verbatim; SPRT signal attributable to RFP alone). `#[cfg(test)] rfp_firings` counter. ADR-0024. Bench: `bench: 3355270 nodes <NPS> nps` (-37.2% vs M5.A). Mixed-TC SPRT vs `baseline/m5a-nmp` (4-bucket uniform `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`): W-L-D **94/32/74** in 200 games / 100 pairs, score 65.5%, logistic Elo **+111**. Pentanomial-GSPRT did not formally cross `elo1=5` H1 bound (too narrow for actual signal magnitude); H1 accepted on score-based decisioning. Mixed-TC rating estimate vs Stockfish UCI_LimitStrength: **2652.43 ± 14.55 Elo** (+51 over M5.A's 2601.45). |
| M5.C | ✓ | Late move reductions. Quiet-only LMR in the `negamax` move loop with five constants (`LMR_MIN_DEPTH = 3`, `LMR_MIN_QUIET_INDEX = 2`, `LMR_BASE_OFFSET = 0.99`, `LMR_LOG_DIVISOR = 3.14`, `LMR_HIGH_HISTORY_THRESHOLD = 4096`); pure helpers `late_move_reduction`, `is_lmr_eligible_quiet`, `lmr_needs_full_research`, `tt_bound_for_completed_node`, `best_is_full_depth_after_score`, `update_history_on_quiet_cutoff`. Move-loop refactor: `search_child` helper centralises make/recurse/unmake plumbing. Node gate: `ply > 0 && !is_pv && depth >= LMR_MIN_DEPTH && !in_check`. Quiet gate: `quiet_index >= 2`, not killer, history < THRESHOLD; TT move implicit-exempt via step-12 reorder + quiet_index = 1. Reduction `R = floor(0.99 + ln(depth)·ln(quiet_index)/3.14)` clamped to `0..=(depth-2)`; `r == 0` skips the LMR path entirely. Reduced search at `depth - 1 - R`; full-depth re-search at `depth - 1` iff `reduced_score > alpha`. **TT-store suppression** when `best` is reduced-only (load-bearing correctness; ADR-0025 §6 — Lower/Exact unreachable as reduced-only by §4.4 re-search rule, Upper would be false advertising). Reduced-only quiets excluded from `quiets_searched`. ADR-0025. Bench: `bench: 1651610 nodes <NPS> nps` (-50.8% vs M5.B). Mixed-TC SPRT vs `baseline/m5b-rfp` (seed `0xC1ABF15AE10DD004`): **H1 accepted in 144 games / 72 pairs**, pentanomial Δ Elo **+145.47 [+100.12, +196.09]**, all four TC buckets positive (60+0.6: 78.9%, 40+0.4: 81.9%, 20+0.2: 58.75%, 10+0.1: 58.3%). Mixed-TC rating estimate: **2657.44 ± 16.49 Elo** (+5 over M5.B). Mutation campaign: 86/92 catch (93.5%); 6 survivors triaged in landing commit (3 catchable closed by added assertions, 3 equivalent / structurally undetectable with `exclude_re` + equivalence proofs). Workflow: this unit was originally implemented in a single-shot pass that skipped the blind-review loops; loops were run retroactively (plan 2-pass / test-suite 2-pass / final 2-pass), all converged. |
| ELOH.A | ✓ | In-process Elo-iteration harness foundation: persistent subprocesses, UCI driver, native adjudication, per-side wallclock TC, color-paired match loop, PGN/summary output. ~2300 LOC + 51 tests. ADR-0020. |
| ELOH.B | ✓ | Statistical layer: Robbins-Monro K-update, σ-stopping, N-parallel concurrency, threshold adjudication, convergence-progress output. Replaced `scripts/elo-iterate.sh`. ~5917 LOC total binary + 139 tests. |
| ELOH.C | ✓ | `VirtualClock` UCI option + harness `--virtual-clock` flag + handshake negotiation. Engine worker uses `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` when active; `SearchClock` is worker-local (per-thread invariant). `--go-nodes N` dropped (implementation-coupled). ADR-0021. ~190 prod + ~140 test LOC. Back-test gate Part 1 deferred to post-merge manual run. |
| ELOH.D | ✓ | Per-pair TC sampling (`--tc-sample <SPEC>` + `--seed N`) for mixed-TC SPRT and Δ(TC) regression. SplitMix64 master stream pre-materialises all per-pair TCs at run start (`Vec<(TimeControl, TimeControl)>` indexed by pair_index) so sampler advance is deterministic regardless of subprocess scheduling under N>1 concurrency. PGN `TimeControl` tag + per-game `tc=` summary field + `summary-by-tc:` aggregate (input-spec order) record sampled TCs. Mutually exclusive with `--tc` at parse time. K-update math unchanged. ~245 LOC. Back-test Part 1 (chi-squared sampler) in-tree; Part 2 (degenerate single-TC self-back-test) deferred to post-merge manual run. |
| ELOH.E | ✓ | In-process pentanomial-GSPRT + fixed-games match + smoke flows. New `mod sprt` (LLR + Wald bounds + pentanomial CI) in `src/bin/elo-iterate.rs`; `--sprt-elo0/elo1/alpha/beta` CLI flags + parse-time mutex against K-update / σ-stopping; per-worker `pair_score_buffers: HashMap<u32, Vec<f64>>` keyed by `worker_id` (load-bearing under concurrency >1); `WorkerReport::GameComplete` extended with `worker_id`; new `StopReason::SprtAcceptH0/H1`; run-end `sprt: verdict=…` and `ci: elo=…` summary lines; `match.pgn` concatenation step. `scripts/sprt.sh sprt|match` and `scripts/match.sh self-play|vs-stockfish` rewritten as harness wrappers; only `scripts/match.sh compliance` still uses fastchess. ADR-0022. ADR-0012 amended. ~280 prod + ~225 test LOC. Back-test Part 1 (synthetic Bernoulli + draw-heavy streams) in-tree; Part 2 (M4.D mixed-TC SPRT statistical-equivalence replay) deferred to post-merge manual run. **Closes ELOH milestone.** |
| Tooling/fuzzing | ✓ | `cargo-fuzz` harnesses for FEN + UCI (ADR-0013); 3.17B execs aggregate across saturation + smoke campaigns, one real parser bug found and fixed. |

For per-phase detail (what landed, verification numbers, lessons), see [`docs/milestones/`](docs/milestones/). The forward-looking milestone plan lives in [`docs/roadmap.md`](docs/roadmap.md).

### What's next

**M5.D — frontier futility pruning** is next. Skips quiet moves at `depth ≤ 2` when `static_eval + margin·depth < α`; per-depth margin table tuned (CPW: 100/150/250 cp at d=1/2/3). Layered into the per-quiet-move decision next to M5.C's reduce/skip choice. Reference baseline: `baseline/m5c-lmr` at commit `0f9bd88`. Detail in [`docs/roadmap.md`](docs/roadmap.md).

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
