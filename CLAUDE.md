# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M1.E complete; M1.F next.** Architectural commitments settled (see `docs/decisions/`).

### What M1.E landed

The `mov` module — the engine's first mutating layer:

- **`Move`** — 16-bit packed encoding: bits 0–5 from-square, bits 6–11 to-square, bits 12–15 flag nibble.
- **`MoveFlag`** — 14 valid codes (codes 6 and 7 deliberately absent; no public path constructs them).
- **`Undo`** (~16 B) — carries captured piece, prior castling/EP/halfmove, and prior zobrist.
- **`make_move(&mut Position, Move) -> Undo`** and **`unmake_move(&mut Position, Move, Undo)`** — free functions per ADR-0004; ergonomic `Position::make_move`/`Position::unmake_move` method delegates also present.

All special cases handled: quiet, double-push, capture, en passant, kingside/queenside castle, four `*Promo` and four `*PromoCapture` flags.

### Implementation highlights

- **Castling-rights update** via a 64-entry `CASTLING_MASK` table indexed by from/to. Handles the rook-captured-on-corner edge case (the Kiwipete depth-4 perft trap).
- **Incremental Zobrist update** with a debug-build round-trip assert against `from_scratch`.
- **Always-on release-build perf sentinel** (`make_move_no_from_scratch_in_release`) at 100 ns/cycle threshold — guards against accidental from-scratch reintroduction on the hot path.
- **`Position` extensions:** `clear_square`, `refresh_zobrist_from`, ergonomic method delegates, `BitAnd` impl on `CastlingRights`.

### Verification (Apple Silicon, dev machine)

| Metric | Result |
|---|---|
| Tests | **303 passing** + 3 ignored benches (286 lib + 3 fen + 2 magic + 3 make/unmake integration + 9 zobrist-vector) |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo build --release` | clean |
| `cargo llvm-cov` on `mov.rs` | 95.65% region / 94.71% line (gaps are const-fn evaluated at compile time + provably-unreachable `unreachable!()` arms + debug_assert formatters) |
| `cargo mutants --in-diff` | **0 survivors** on 123 mutants (107 caught, 16 unviable) — full-suite per-milestone backstop |
| Throughput (release, ns/cycle) | quiet 27, capture 38, EP 36, castle 42, promo 22 — all under the <50 ns/cycle target |

### Process

All three review loops (plan, test-suite, final) converged. Plan archived at `docs/plans/m1.e.md`.

### What's next

**M1.F — legal move generation.** Per-piece-type generation, pin computation, check-evasion specialization (single-check → king/capture/block; double-check → king only), legal-direct emission. ADR-0007 (legal-direct movegen) binds here. M1 prior-art research is in `docs/research/`, synthesized into `docs/prior-art.md`.

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

Keep these files current as the project evolves. When a session ends with new commitments or learnings, update the relevant doc before stopping.
