# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M1.F complete; M1.G next.** Architectural commitments settled (see `docs/decisions/`).

### What M1.F landed

The `movegen` module — the engine's legal-move enumeration surface:

- **`MoveList`** — 256-slot stack-allocated `[MaybeUninit<Move>; 256]` + `len: u16`. `push`/`clear` are `pub(crate)`; `as_slice` exposes `&[Move]` via one justified `unsafe` block. The 218-move legal ceiling makes over-push unreachable in practice.
- **`generate_moves(&Position, &mut MoveList)`** — legal-direct emission, mask-based, with check-evasion specialization (no check / single check → king + capture-the-checker + block / double check → king-only).
- **`in_check(&Position) -> bool`** — public helper; thin wrapper around `checkers_of`.
- **Per-call `MaskInfo`** — `checkers`, `pinned`, `capture_mask`, `push_mask`, `king_danger` (computed against `occupancy ^ king_bb` per the king-flee gotcha), `pin_rays[64]`. Recomputed each `generate_moves` call; not cached on `Position`.
- **EP horizontal-pin filter** AND symmetric **diagonal-discovery filter** — both at emission time. The symmetric diagonal case is needed because the standard pin filter doesn't catch it (the capturing pawn isn't on the king-pinner diagonal).
- **Castling** — gated on no-check + transit/destination not in `king_danger`. Mailbox `debug_assert!` on king/rook starting squares (release-trusted).
- **`validate_post_parse` extended** — rejects FENs whose castling rights don't match the king/rook mailbox; new `FenError::InconsistentCastlingRights`.
- **Defensive-checks-debug-only convention** — added to `docs/workflow.md` "Final review loop" → Code quality. Validation goes at the boundary that creates the invariant; consumers `debug_assert!`; release trusts.

### Implementation highlights

- **`const fn` pre-computed leaf attack tables** (`PAWN_ATTACKS`, `KNIGHT_ATTACKS`, `KING_ATTACKS`) — built at compile time; no `LazyLock` per-call atomic on the hot path.
- **`prop_no_legal_move_leaves_us_in_check`** — proptest with deterministic SplitMix64 random walk over §6 edge fixtures + canonical 6 seeds. Pins the legal-direct invariant.
- **In-crate `count == 2` assertion** for the EP-double-check fixture (uses crate-private `checkers_of`) — pins §6.3 taxonomy point.

### Verification (Apple Silicon, dev machine)

| Metric | Result |
|---|---|
| Tests | **401 passing** + 4 ignored benches (372 lib + 12 movegen integration + 9 zobrist-vector + 3 fen + 2 magic + 3 make_unmake) |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo build --release` | clean |
| Smoke throughput (release, ns/call) | starting 68, Kiwipete 114, Pos-3 37, Pos-4 43, Pos-5 129, Pos-6 99 — all 50× under the 5 µs/call regression-tripwire threshold |

### Process

All three review loops (plan, test-suite, final) converged after 3 / 3 / 2 rounds. Plan archived at `docs/plans/m1.f.md`. ADR-0007 codifies the binding architectural choices.

### What's next

**M1.G — perft + benchmarks.** Recursive perft driver with bulk-counting at depth 1, Stockfish-generated fixtures on the canonical 6 to depth 6/7, EPD-corpus regression (Whittington `perft.epd`), `criterion` benchmark harness with baseline saving. Target ≥100 Mnps single-threaded perft on Apple Silicon (≥200 Mnps would be excellent).

## How to pick up a new session

1. Read this file (auto-loaded).
2. Read `docs/architecture.md` for current architectural state.
3. Read `docs/roadmap.md` for milestone status and what's next.
4. Read `docs/workflow.md` for how we collaborate.
5. Skim `docs/decisions/` for the *why* behind specific commitments.
6. Skim `docs/prior-art.md` for reference landscape — grows over time as we research each component.
7. Check git log if a repo exists.

## Ground rules (load-bearing — do not relax)

### User profile
- Chess strength ~1000 Elo. Casual player, serious engineer.
- **Does not know Rust, by design.** The language choice is a self-imposed gatekeeper to keep the work vibe-coded. The user will not be inspecting code line-by-line; chat explanations are the primary signal. Be unusually rigorous in chat about explaining decisions and surfacing risks, since bugs cannot be caught by reading.

### Interaction conventions
- **When asking the user questions, use the `AskUserQuestion` tool.** Plain-text questions in chat are easy to miss and lack a structured-choice UI; the tool surfaces the question with its options as discrete picks. Applies to clarifying requirements, choosing between approaches, or any other decision-soliciting prompt — not to status updates or summaries.

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
- `docs/roadmap.md` — milestone plan, current progress.
- `docs/workflow.md` — collaboration loop, TDD scope, benchmarking conventions.
- `docs/prior-art.md` — reference landscape; per-feature research notes accumulate here.
- `docs/decisions/` — ADR-style records, one file per substantive decision.
- `docs/reference/` — vendored authoritative specs (FIDE Laws of Chess, UCI protocol).
- `docs/tooling-backlog.md` — prioritized list of tooling/QA items not yet adopted. Pull from the top when a tooling slot opens.

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
