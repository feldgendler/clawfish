# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M2.C complete; M2.D next.** Architectural commitments settled (see `docs/decisions/`).

- M1: complete through M1.G (perft + criterion benchmark harness; 119 Mnps bulk on starting D4).
- M2.A: complete — `Move::to_uci` + `Move::from_uci` + `UciMoveError`. Generate-and-match parsing strategy; null move `0000` rejected as `NullMove`; strict lowercase input. No new ADR.
- M2.B: complete — `uci` module: `Command` enum + `parse_uci_line(&str) -> Command`. Pure string→AST function, no I/O, no engine state. Per-command leniency rules grounded in empirical Stockfish 18 probes. No new ADR.
- M2.C: complete — `engine` + `search` modules: `Engine<W, S>` orchestrator + reader thread + per-`go` worker + `Arc<AtomicBool>` cancellation per **ADR-0011**. `Search` trait + `SearchContext` + `Stub` impl. End-to-end exercisable: `printf 'uci\nquit\n' | target/release/chess` produces `id name chess 0.1.0 / id author Alex Feldgendler / uciok`.
- M2: D–E remain. Pre-M2 research note `docs/research/m2-tournament-harness.md` binds ADR-0012 on M2.E.

### What M2.C landed

The `engine` and `search` modules — the UCI I/O layer that ties M2.A and M2.B together with a working command loop:

- **`Engine<W, S>` in `src/engine.rs`** — generic orchestrator over stdout writer + Search impl. Holds `Position`, `debug` flag, `Arc<AtomicBool>` cancellation, `Arc<Mutex<W>>` stdout, `Arc<Mutex<S>>` search, `Option<JoinHandle>` for the in-flight worker.
- **Reader thread + mpsc** — `reader_loop(impl BufRead, Sender<Command>)`. EOF synthesizes `Quit`; reader exits on any `Quit` (parsed or synthesized) — no double-Quit.
- **Per-`go` worker thread** — `handle_go` joins the previous worker, clears the cancellation flag, builds `SearchContext`, spawns a new worker. The worker locks the search mutex, calls `Search::go`, writes `bestmove` directly to stdout under the shared mutex.
- **`Search` trait + `SearchContext` + `SearchLimits` + `SearchResult` in `src/search.rs`** — committed at M2.C so M2.D / M3 / M8 plug in without trait churn.
- **`Stub` Search impl** — deterministic lex-first legal move; honors `infinite` / `movetime` / `ponder` by polling `should_abort` (1 ms cadence) until cancelled. Always computes the candidate before checking cancellation — race-free under quit-immediately-after-go.
- **`run_stdio()` -> !** — production wrapper: spawns reader thread on `io::stdin()`, builds Engine with `io::stdout()` and `Stub`, drives `run`, then `process::exit(0)`. `src/main.rs` is now `fn main() { chess::run_stdio(); }`.

### M2.C — implementation highlights

- **ADR-0011 codifies the threading model** — reader thread → mpsc → main-as-orchestrator + per-`go` worker + `Arc<AtomicBool>` cancellation. Same primitive scales unchanged through M3 alpha-beta and M8 lazy-SMP.
- **`handle_quit` joins the worker** (bounded by cancellation polling cadence ≤ 1 ms) so `bestmove` is in stdout before `run` returns. Required for testability outside `run_stdio`'s `process::exit` safety net. ADR-0011 was amended to v3 to document this.
- **`position` reset-on-error** — asymmetric: FEN-parse-error keeps prior position (no safe base); move-error resets to spec base (parsed `startpos` or successfully-parsed FEN), no moves applied. Both emit `info string position rejected: …` *unconditionally* (protocol-legal; silent rejection in tournament play would be the worst failure mode).
- **`searchmoves` filtering** silently drops bad entries. All-bad list yields `bestmove 0000`.
- **`handle_debug` is silent** — only toggles `self.debug`. `setoption` / `register` / `ponderhit` / `Unknown` are silent when debug=off; emit `info string … received: …` when debug=on.
- **Generics over trait objects** — `Engine<W: Write + Send + 'static, S: Search + Send + 'static>` for cleaner stack traces and zero virtual-call overhead. `search: Arc<Mutex<S>>` is the chosen idiom (Search::go takes `&mut self`; back-to-back `go` joins before spawning so the mutex is uncontended in normal flow).
- **`Stub` always computes the candidate before checking cancellation** — eliminates a race where `quit` arriving immediately after `go` could flip the flag before the worker thread was scheduled, causing `bestmove 0000` instead of the legitimate lex-first move. Spec-aligned: bestmove is the best legal move available; `0000` only when no legal moves exist (mate / stalemate / empty `searchmoves` filter).

### M2.C — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 583 lib + 3 uci-integration + 24 other = **610 fast + 4 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` | `engine.rs`: 97.30% region / 97.09% line / 93.44% function. `search.rs`: 98.29% region / 97.58% line. Remaining uncovered: `unreachable!()` on rx disconnect (by-design), `Err(_)` reader break (untestable with `Cursor`), `run_stdio` body (covered via integration tests through the binary path). |
| `cargo mutants --in-diff` | 30 mutants generated; **25 caught + 2 timeouts (effective catches via test hangs) + 3 unviable; 0 missed**. |
| Smoke | `printf 'uci\nquit\n' | target/release/chess` produces `id name chess 0.1.0 / id author Alex Feldgendler / uciok` and exits within 2 s. |
| Benchmark | **Skipped** — UCI dispatch is per-line, never on a search hot path. Same precedent as M2.A / M2.B. |

All three review loops converged. Plan archived at `docs/plans/m2.c.md`; ADR-0011 codifies the threading model. The plan went through 3 reviewer passes (Sonnet+Opus calibration on v1, then Opus-only on v2/v3); the test suite through 4; the final review through 2 (the second pass closed 3 missed mutants and a coverage gap on `searchmoves`).

### What M2.B landed

The `uci` module — pure-function parser for UCI 2006 GUI→engine commands:

- **`src/uci.rs`** with `Command` enum + helper types (`DebugMode`, `Register`, `PositionSpec`, `GoParams`) + `parse_uci_line(&str) -> Command` + 5 private sub-parsers.
- **Per-command leniency rules** (Stockfish-empirically grounded): strict-first-token (no leading-skip — `joho uci` → `Unknown`); `debug` strict-exact-2-tokens; `position` lenient-stop after the position spec; `go` lenient-skip on unknown body tokens.
- **Move strings collected raw**, not parsed — `Position { … moves: Vec<String> }` and `GoParams::searchmoves: Option<Vec<String>>` stay strings; M2.C parses via M2.A's `from_uci`.
- **FEN strings collected raw** — strict-6-token collection after `fen`, joined with single spaces; M2.C parses via FEN parser.
- **`SEARCHMOVES_TERMINATORS`** — private const enumerating the 11 `go` body keywords other than `searchmoves` itself; pinned by data-invariant test.

### M2.B — implementation highlights

- **Strict-first-token rule** chosen over spec-literal "skip leading unknown tokens." User flagged the spec rule as silly ("hides GUI-side typos"). Stockfish 18 probes confirmed the de facto reference doesn't implement leading-skip either. Documented in `docs/plans/m2.b.md` §3.1 with 12 empirical probe rows.
- **Per-command leniency asymmetry** is real (not a uniform rule): `debug` is strict-exact-2-tokens (`debug on garbage` → `Unknown`); `position` is lenient-stop (junk between `startpos` and `moves` discards the `moves` clause); `go` is lenient-skip (unknown body tokens silently dropped, parsing continues). All three confirmed empirically against Stockfish.
- **Numeric type widths grounded in spec**: `wtime`/`btime`/`movetime` are `i64` (clock can go negative under time-trouble); `winc`/`binc` are `u64` (spec literally says "if x > 0", so non-negative); `nodes` is `u64`; `depth`/`movestogo`/`mate` are `u32`.
- **Total function** — every input maps to a `Command`; no `Result`, no panics. No-arg commands ignore trailing junk (`isready xyzzybanana` → `IsReady`). Parser body uses no panicking primitives beyond a `peek-then-next.unwrap()` pattern that's safe by construction.

### M2.B — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 542 fast + 4 ignored (446 prior + 96 new M2.B; integration + doctests as before). All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` (`uci.rs`) | 99.90% region / 100.00% line / 100.00% function. The 1 uncovered region is the safety-net `iter.next().unwrap()` panic branch in the searchmoves collection loop (peek-then-next pattern; the panic side is unreachable by Iterator contract). |
| `cargo mutants --in-diff` | 55 mutants generated; **49 caught, 6 unviable** (the 6 unviable are mutants that fail to compile — typically `Default::default()` substitutions on types without `Default` impls, etc.); 0 missed. |
| Benchmark | **Skipped** — UCI command parsing is per command (line-at-a-time, never inside a search loop). A microbenchmark would measure noise. Same precedent as M2.A. |

All three review loops (plan, test-suite, final code+tests) converged. Plan archived at `docs/plans/m2.b.md`. The plan went through 5 reviewer passes; the test suite through 3; the final review converged on first pass.

### What M2.A landed

- **`Move::to_uci(self) -> String`** in `src/mov.rs`. Thin wrapper around the existing `Display` impl (canonical writer); both produce identical bytes. Self-documenting at M2 protocol call sites.
- **`Move::from_uci(s: &str, pos: &Position) -> Result<Move, UciMoveError>`** in `src/mov.rs`. Generate-and-match: enumerates `generate_moves(pos)`, finds the unique move matching the parsed `(from, to, promotion_kind)`. Defers legality entirely to movegen (consistent with ADR-0007).
- **`UciMoveError`** enum: `Malformed` / `IllegalPromotionPiece` / `NullMove` / `IllegalForPosition`. Implements `Display` + `std::error::Error`. Re-exported from `src/lib.rs`.
- **Tests** — 38 new test fns in `src/mov.rs::tests`: 12 `to_uci` anchors + 10 `from_uci` positive anchors (including check-evasion) + table-driven negative-parse + 10 position-dependent rejection tests + round-trip on `CASES` + round-trip on D1 enumeration of `UCI_SEED_FENS` (canonical 6 + EP-horizontal-pin + EP-double-check + mate + stalemate) + two proptests (round-trip on D2-reachable positions; `(from, to, promo)` uniqueness invariant).

### M2.A — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 484 fast + 4 ignored (446 lib + 38 new M2.A; integration + doctests as before). All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` (`mov.rs`) | 96.55% region / 95.75% line / 95.20% function. Uncovered lines = pre-existing `#[ignore]`-gated bench + pre-existing `unreachable!()` arms + `unwrap_or_else` panic branches in tests that don't fire when tests pass. No M2.A-specific gaps. |
| `cargo mutants --in-diff` | 19 mutants generated; **18 caught, 1 unviable** (`from_uci -> Ok(Default::default())` — `Move` has no `Default`); 0 missed. |
| Benchmark | **Skipped** — UCI move parsing is per-`position` command, not on a search hot path; a microbenchmark would measure noise. |

All three review loops (plan, test-suite, final code+tests) converged. Plan archived at `docs/plans/m2.a.md`.

### What M1.G landed

The `perft` module — move-generation validation + measurement:

- **`src/perft.rs`** with `perft`, `perft_bulk` (CPW depth-1 leaf-skip), `divide` (UCI-sorted), `perft_categorized` (internal-only category counts per ADR-0006), and the `PerftCounts` struct.
- **Recursion driver** `perft_inner<const BULK: bool>` monomorphized over plain/bulk; the leaf-skip branch is dead-code-eliminated from the plain path.
- **Stockfish-regenerated fixtures** — canonical 6 D1–D6 + 174-position Whittington corpus D1–D4. Per ADR-0006 Stockfish 18 is the sole oracle; `scripts/regen-perft-fixtures.sh` reproduces them.
- **Test partition** — D1–D3 (all 6 positions, plain + bulk parity) + light-D4 fast (`cargo test`); heavy-D4, D5, D6, Whittington D4 `#[ignore]`-gated. Default fast suite finishes in 0.02s release.
- **`criterion 0.7` benchmark harness** — `benches/perft.rs` + `benches/movegen.rs`; first baseline at `bench/m1.g.md`. Format ratified by **ADR-0010**.
- **M1.F smoke benchmark deleted** — criterion supersedes it.

### M1.G — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 446 fast + 9 ignored. All ignored verified to pass: D4-heavy 0.23s; D5 + Whittington 2.47s combined; **D6 117.93s** (~22B nodes). |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo audit` + `cargo deny check` | clean |
| `cargo llvm-cov` (`perft.rs`) | 98.32% region / 98.25% line / 93.75% function. Crate total 95.19%. |
| **Headline throughput (`bench/m1.g.md`)** | starting D4 plain **33 Mnps**; starting D4 bulk **119 Mnps**; Kiwipete D3 bulk **168 Mnps**. Meets M1 ≥100 Mnps exit criterion on bulk path. |

All four loops (plan, test-suite, final code+tests, benchmark capture) converged. Plan archived at `docs/plans/m1.g.md`. ADR-0010 codifies the bench-baseline format.

### What's next

**M2.D — Random-mover Search impl + `bestmove` reproducibility.** Replaces `Stub` with a SplitMix64-seeded random-mover. Adds a custom `Random_Seed` UCI option (the engine's first real `setoption`) so games are reproducible. Honors `go movetime <ms>` exactly (sleeps then emits — already plumbed by `Stub`), `go infinite`, `searchmoves` filtering. End-to-end test: drive a full game from `position startpos` through repeated `go movetime 10` until terminal.

- No new ADR.
- The plug-in point is `Search::go`'s body; orchestrator unchanged.
- A14 (`handle_setoption_silent_no_output_when_debug_off`) will need revising when `Random_Seed` lands — its forward-compat caveat is documented in M2.C's tests.
- Approx size: 700–1000 lines.

Full M2 plan and the remaining sub-phase M2.E is in `docs/roadmap.md`. Exit criteria for M2 as a whole: complete game through `fastchess` against itself or another engine without protocol errors or illegal moves.

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
