# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**M6.F landed 2026-05-19 (Tier-1 HCE infra — outposts + rook-on-file + endgame draw-scaling, score-neutral inert — all weights → M6.H).** Production HEAD = `M6.F` (eval byte-identical to `M6.E`). M6.F ships `eval::tier1`: (1) `outpost_term_white` iterating the `pub(crate) outpost_squares(pos,side)` seam — knight/bishop on an enemy-pawn-unchallengeable hole (`!*_attack_front_spans(enemy_pawns)`) ∩ own-pawn-defended, gated to the enemy-half `(3..=5)` relative-rank band (**the gate is correctness-load-bearing**: the front-span-complement is a valid hole test *only* in the enemy half — the coder must not "simplify" it away); (2) `rook_file_term_white` (`file_fill` projection, own-pawn-on-rook-file via `(file_fill(own_pawns) & from_square(rook)).any()`, open > semi-open precedence); (3) `endgame_scale` returning a numerator over the **structural** `EG_SCALE_DEN`, applied `blended * scale / EG_SCALE_DEN` *before* `+ mop_up` — `is_ocb_with_pawns` (exactly-1-bishop-each opposite complexes + no Q/R + ≥1 pawn) + `is_pawnless_drawish` (the narrow KNNvK/balanced-≤1-minor accept-list; KBNvK/KBBvK explicitly excluded as forced wins) + 50-move ramp. All computed live in `evaluate_core`, never cached (the ADR-0033 §6 live-term class). **Shipped score-neutral inert** — all additive weights zeroed + every scale tunable == `EG_SCALE_DEN` (scale ≡ identity), `TIER1_IN_EVAL=true`, term math live (M6.H-ready — the M6.B–E `*_IN_EVAL` precedent). The endgame-scaling **inert-vs-live open ADR question was resolved inert-per-precedent** (ADR-0034 §4): the roadmap-stated default is inert; the reinforcing optimizer-tractability argument is conditional on the still-open M6.H optimizer ADR and is fully honorable at M6.H without M6.F landing live (the scale coefficients can be fixed + excluded from the tunable vector then; `EG_SCALE_DEN`/`FIFTY_MOVE_TAPER_FROM` are already declared structural); the dominated correctness-timing axis costs **zero measurable strength** (no rated play in the M6.F→M6.G→M6.H gap); owned, not papered (§4 honestly concedes live-with-fixed-coefficient dominates on 2 of 3 axes). **Landing gate (the M6.C/M6.D/M6.E inert-landing precedent): zeroed/identity ⇒ outpost/rook-file ≡ (0,0), `endgame_scale ≡ EG_SCALE_DEN`, `blended·D/D == blended` byte-exact ⇒ `evaluate` byte-identical to `M6.E` ⇒ bench `1213649`/depth-4 `90591` byte-for-byte (deterministic ×2, orchestrator-re-verified) ⇒ provably inert ⇒ no confirmation SPRT, no SPRT screen ladder; roadmap line-296 satisfied vacuously.** Unlike M6.E there is **no diagnostic-screen-skip novelty to own** (M6.F carries no SPRT gate by roadmap construction; the M6.H outpost brief already exists — the measured −40 STS theme #3 gap). All three review loops (plan/test-suite/final) converged; mutation 30 caught/16 missed/1 timeout (0 real gaps — zero missed in `tier1.rs` + the scale wiring; the 16 are the M6.B–E-precedent blend-numerator `+`-on-zeroed-operands tool-scope artifact, documentation-not-exclusion / M6.H-revalidating). ADR-0034 (binds on M6.F per roadmap §M6). M6.E landed 2026-05-19 (king-safety infra, score-neutral, == `M6.D` eval, ADR-0033; NO SPRT screen ladder — the M6.C/M6.D divergence owned); M6.D landed 2026-05-17 (piece-mobility infra, score-neutral, == `M6.C` eval, no ADR — roadmap-committed); M6.C landed 2026-05-17 (passed-pawn infra, score-neutral, == `M6.B` eval, ADR-0032 §8); M6.B landed 2026-05-16 (CONN-only, Δ Elo +45.42 vs `M6.A`, ADR-0032 §7); M6.A landed 2026-05-15 (Δ Elo +250.57 vs M5.H1, ADR-0031).

**Next phase: M6.G (Corpus construction)** — a reusable labeled-position data-infra phase (CCRL + band-filtered Lichess + diversified-opening-book clawfish self-play + label-verified Zurichess quiet set; clock-loss/time-forfeit exclusion + TC-class filter; quiet-position extraction with the quiet *definition* pinned by M6.H's tuner qsearch; opening-ply skip; FEN dedup/per-FEN caps; **ADR-0003 label-provenance audit — game-result labels ONLY**; a held-out deployment-distributed self-play validation set; reproducible snapshot + manifest + RNG seeds + re-run script vendored in `bench/`) gated on **data-quality checks, NOT SPRT** (the M5.E correctness-only-gate precedent applied to data) — its own sub-milestone because the corpus is reusable foundational infra consumed by M6.H, the tuning-backlog "PST co-tuning" Arm B, future SPSA campaigns, and M10 NNUE data-prep; acquisition/filtering is largely independent of the M6.F eval features (now landed) and the quiet-definition / held-out set / stratified objective are pinned by the M6.H tuner contract at M6.G plan time (interface-first). **Then M6.H (Texel tuning)** — the single joint-Texel pass over the now-cumulative A–F parameter surface, consuming the *frozen* M6.G corpus artifact (game-result-labeled); M6.H's SPRT baseline is the `M6.F` tag (not `M6.G`). See [`docs/roadmap.md`](docs/roadmap.md) §M6 (M6.F + M6.G + M6.H scope detail + verdict ladder), [`docs/milestones/m6.f.md`](docs/milestones/m6.f.md) for the M6.F retrospective (the *inert-vs-live open ADR question resolved inert-per-precedent on a conditional-premise + zero-strength-cost argument, owned* / *the `outpost_squares` `pub(crate)` seam is the only correctness signal for a score-neutral predicate beyond the bench gate — an inline reconstruction pins nothing* / *don't mischaracterize an M6.H-dormant gate-check as a live discriminator* lessons) and [`docs/milestones/m6.e.md`](docs/milestones/m6.e.md) for the M6.E retrospective (the *co-calibrated-elsewhere ≠ transfers here law matured from empirical to predictive* / *a `pub(crate)` seam turns "tests a copy" into a real pin* / *structural M6.H-revalidation is the property* / *ADR supersession belongs in the structural `## Supersedes` section* lessons), and [`bench/m6.md`](bench/m6.md). **M6.H obligation (extended again — one joint pass): re-derive, against our PeSTO PSTs, the M6.B ISO/DBL/BWD re-introduction + CONN rescale, the M6.C passed-pawn rank-table/king-distance reshape, the M6.D N/B/R/Q mobility per-kind reshape, the M6.E king-safety full reshape (S-curve table + per-kind attacker weights + shield + open-file + MG/EG split; the §7 pawn-shield × PeSTO-king-PST double-count is the load-bearing input), AND the M6.F outpost + rook-file + endgame-scaling weights.** Until M6.H, M6.B–F's zeroed pawn/mobility/king-safety/Tier-1 terms stay inert by design. ADR-0032 §7/§8 + ADR-0033 §7/§8 + ADR-0034 §2/§4/§8 + the roadmap §M6 + `docs/milestones/m6.f.md`.

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
