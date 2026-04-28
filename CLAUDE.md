# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M2.D complete; M2.E next.** Architectural commitments settled (see `docs/decisions/`).

- M1: complete through M1.G (perft + criterion benchmark harness; 119 Mnps bulk on starting D4).
- M2.A: complete — `Move::to_uci` + `Move::from_uci` + `UciMoveError`. Generate-and-match parsing strategy; null move `0000` rejected as `NullMove`; strict lowercase input. No new ADR.
- M2.B: complete — `uci` module: `Command` enum + `parse_uci_line(&str) -> Command`. Pure string→AST function, no I/O, no engine state. Per-command leniency rules grounded in empirical Stockfish 18 probes. No new ADR.
- M2.C: complete — `engine` + `search` modules: `Engine<W, S>` orchestrator + reader thread + per-`go` worker + `Arc<AtomicBool>` cancellation per **ADR-0011**. `Search` trait + `SearchContext` + `Stub` impl. End-to-end exercisable: `printf 'uci\nquit\n' | target/release/chess` produces `id name chess 0.1.0 / id author Alex Feldgendler / uciok`.
- M2.D: complete — `RandomMover` SplitMix64-seeded random-mover replaces `Stub`. First real UCI option: `Random_Seed` (`type spin default 0 min 0 max 2147483647`). `Search` trait extended with `set_seed` / `reset` lifecycle hooks. New `Engine::join_in_flight_worker` helper unifies the stop+join sequence used by `handle_go`, `handle_ucinewgame`, and `handle_setoption`'s success arm — closes a deadlock under in-flight `go infinite`. End-to-end self-play game terminates legally with the embedded seed (E36). No new ADR.
- M2: E remains. Pre-M2 research note `docs/research/m2-tournament-harness.md` binds ADR-0012 on M2.E.

### What M2.D landed

The first real `Search` impl + the engine's first real UCI option:

- **`RandomMover` in `src/search.rs`** — `pub(crate) struct RandomMover { seed: u64, state: u64 }`. Picks uniformly at random from the legal-move list (post-`searchmoves` filter) via one SplitMix64 step per `go`. Honors `infinite` / `movetime` / `ponder` by polling `should_abort` on a 1 ms cadence, same as M2.C's `Stub`. Always computes the candidate before checking cancellation — same race-free invariant `Stub` shipped with.
- **`splitmix64_next(&mut u64) -> u64`** — public-domain reference implementation, vendored verbatim with constants `0x9E3779B97F4A7C15`, `0xBF58476D1CE4E5B9`, `0x94D049BB133111EB` and shifts 30/27/31. ~10 lines, no new dep. Modulo bias against N ≤ 218 legal moves is < 4×10⁻¹⁸ (research §3.4) — `splitmix_output % n_moves` is correct without rejection sampling.
- **`Search` trait extended** with `fn set_seed(&mut self, _seed: u64) {}` and `fn reset(&mut self) {}` — both default-no-op. `RandomMover` overrides them; `InfoEmittingFake` and any future M3+ alpha-beta inherit the no-op defaults.
- **`Random_Seed` UCI option** — emitted by `handle_uci` in the slot between `id author` and `uciok`. Parsed and validated in `handle_setoption` via case-insensitive name match (`eq_ignore_ascii_case("random_seed")`); value parsed as `u32` then validated against `MAX_RANDOM_SEED = 2_147_483_647` (= `i32::MAX`, the protocol-declared `max`). Strict acceptance: anything outside `[0, MAX_RANDOM_SEED]` is rejected (silent debug-off; `info string Random_Seed: rejected …` debug-on).
- **PRNG semantics** — continuous state across `go` calls; `setoption name Random_Seed value N` resets state immediately to `N` (not deferred to `ucinewgame`); `ucinewgame` resets state to the current `seed`. Net: a sequence of `go` calls with a fixed seed is fully reproducible.
- **`Engine::join_in_flight_worker`** — new private helper consolidating the "signal stop + join the worker" idiom previously inlined in `handle_go`'s back-to-back path. Now also called by `handle_ucinewgame` (before `Position::starting_position()` + `search.lock().reset()`) and `handle_setoption`'s `Random_Seed` success arm (before `search.lock().set_seed(n)`). Closes a deadlock that would have hung the engine if a GUI sent `ucinewgame` or `setoption` while a `go infinite` worker was holding the search mutex.
- **End-to-end self-play test E36** — drives the binary through `position startpos moves <accumulated>` + `go movetime 10` until terminal. Uses a parallel `Position` + `Move::from_uci` + `make_move` to validate every bestmove and detect mate/stalemate. Pinned at `SELF_PLAY_SEED = 8` (seeds 0–7 cycle past 300-ply MAX without terminating — random self-play in a draw-rule-less engine can absolutely cycle, not a bug). Seed 8 terminates at ply 106 with `bestmove d3d1`.
- **`run_stdio()`** now constructs `Engine::new(io::stdout(), RandomMover::new(0))` instead of `Stub`. Default seed `0` matches the protocol-declared `default 0`.

### M2.D — implementation highlights

- **No new fields on `Engine`.** Earlier draft of the plan added `seed_value: u64` to `Engine`; final design eliminated the field as a dead store. The seed lives solely in `RandomMover.seed`, mutated via `Search::set_seed` and read via `Search::reset`. No two-source-of-truth drift risk.
- **`splitmix64_next` constants vendored verbatim** from prng.di.unimi.it (public-domain). Same provenance as the Polyglot Zobrist table. Pinned by D1, which compares the first 8 outputs from seed 0 against a hand-computed reference table.
- **Phase 1 implemented `splitmix64_next` and `RandomMover::go` early** so the seed-pair pre-computation for B.2.e (which needed working `RandomMover`s) could run before Phase 2's tests went in. Phase 4 Coder-A's slice was thereby narrowed to the trivial `set_seed` / `reset` one-liners — eventually rolled into Phase 1 as well, leaving Coder-A vacuous.
- **The plan's claim that "non-terminating random self-play is a strong signal of a movegen bug" was overly aggressive** — confirmed empirically by Phase 4. Random play between two random movers in a draw-rule-less engine can shuffle pieces back and forth indefinitely without ever reaching mate or stalemate. We don't implement 50-move / threefold yet (explicit non-goal). Seed 8 was the first that terminated within 300 ply; the `find_terminating_seed_for_e36` `#[ignore]`-gated test documents the search.
- **Strict O3 resolution on `Random_Seed` value validation** — values in `(MAX_RANDOM_SEED, u32::MAX]` (i.e. valid `u32` but above declared `max`) are rejected. Stockfish 18 silently accepts these per research §2.2; we honor the declared protocol contract instead.
- **`handle_setoption` mid-`go` no longer deadlocks** — the `join_in_flight_worker` consolidation aborts the in-flight search before acquiring the search mutex. Spec says `setoption` arrives between searches; if a defective GUI sends one mid-search, we now degrade gracefully instead of hanging.

### M2.D — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 598 lib + 4 uci-integration + 24 other = **626 fast + 6 ignored**. All passing. The 6 ignored: 4 pre-existing benches/doctests + `find_seed_pair_for_b2e` (documentation helper) + `find_terminating_seed_for_e36` (documentation helper). |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` | `engine.rs`: 97.41% region / 96.92% line / 91.18% function. `search.rs`: 77.15% region / 78.69% line — production `splitmix64_next` and `RandomMover` are at 100%; the cosmetic gap is the two `#[ignore]`-gated documentation helpers and the `Search` trait's no-op defaults (overridden by `RandomMover`, never called on `InfoEmittingFake`). |
| `cargo mutants --in-diff` | 23 mutants generated; **23 caught; 0 missed; 0 timeouts; 0 unviable**. |
| Smoke | `printf 'uci\nquit\n' | target/release/chess` produces 4 lines including `option name Random_Seed type spin default 0 min 0 max 2147483647` and exits within 2 s. |
| Benchmark | **Skipped** — UCI dispatch is per-line, never on a search hot path; `RandomMover::go`'s per-`go` cost is one SplitMix64 step (~0.6 ns). Same precedent as M2.A / M2.B / M2.C. |

All three review loops converged. Plan archived at `docs/plans/m2.d.md`; no new ADR. The plan went through 5 reviewer passes; the test suite through 2; the final review through 2 (the second pass's mutation rerun caught the new `join_in_flight_worker` helper at 100%).

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

**M2.E — Tournament harness + fastchess.** `scripts/match.sh` wrapper around `fastchess`; documentation for downloading the `mac-arm64` release and reading PGN/log output. Integration test that spawns the release binary, drives it through a complete self-game via piped stdin/stdout, asserts the game terminates legally (checkmate / stalemate / 50-move / threefold / insufficient material per FIDE) and that the PGN parses. ADR-0012 codifies the harness layout. `docs/workflow.md` gains a "running a match" runbook.

- ADR-0012 binds (per pre-M2 research note `docs/research/m2-tournament-harness.md`).
- Approx size: 400–700 lines.
- Closes M2 as a whole: complete game through `fastchess` against itself or another engine without protocol errors or illegal moves.

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
