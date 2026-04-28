# Roadmap

Milestone plan. Update as we complete or revise.

## Status

**M1 complete; M2 complete; M3.A complete; M3.B next — game-history + draw-detection plumbing.**

### M3.A — what landed (2026-04-28)

The first playing-engine phase: depth-1 best-eval replaces uniform-random as production search.

- **`src/eval.rs`** — `evaluate(&Position) -> i32` (side-to-move-relative centipawns); insufficient-material returns 0 (KvK / KvN / KvB).
- **`src/eval/data.rs`** (split file) — vendored PeSTO MG values + six `MG_*_PST` arrays. File-level cargo-mutants exclusion mirrors `src/magic/constants.rs` precedent.
- **`PSQT[2][6][64]` compile-time table** built via `const fn build_psqt()`; `s ^ 56` flip converts our LERF index to PeSTO's a8-origin layout.
- **Incremental `static_eval_white: i32` field on `Position`** maintained by `make_move` / `unmake_move`. Mirrors M1.E's Zobrist pattern (debug round-trip assert, release perf sentinel). `Undo` grows 16 → 24 B.
- **`update_static_eval_after_make` private helper** — order-agnostic; uses `mover.color` parameter, not `pos.side_to_move()`. Six flag-arm deltas; same shape as `update_zobrist_after_make`.
- **`GreedyMover` Search impl** — depth-1 best-eval with reservoir-sampled tie-break (one SplitMix64 step per tied move; uniform over the tied set). Single `pos_clone` mutated in place via make/unmake. `info` line emitted BEFORE wait loop with real `score cp` (not M2.D's placeholder 0).
- **`RandomMover` deleted** — struct, impl, M2.D-only tests, `find_*` helpers all gone. `splitmix64_next` survives under GreedyMover ownership; D1 + D11 tests stay verbatim.
- **`Random_Seed` UCI option preserved** — now drives GreedyMover's tie-break PRNG.
- **E36 deleted; E38 added** — pinned at seed 0, ply 116, last bestmove `f2f3`.
- **`scripts/match.sh`** — header strings updated; `-draw` adjudication still omitted (depth-1 scores too noisy for draw heuristic).
- **ADR-0014** codifies the eval composition.

### M3.A — implementation highlights

- **PSQT layout: precomputed table, not on-the-fly composition.** `PSQT[color][kind][square]` is a `[[[i32; 64]; 6]; 2]` const built at compile time. White lookup `+(MATERIAL[kind] + MG_PST[kind][sq ^ 56])`; Black `-(MATERIAL[kind] + MG_PST[kind][sq])`. Eliminates per-eval branches on color and rank-flip.
- **Combined material+PST single field, not separate.** One delta-update site, one source of truth — same argument as M2.D's "no seed field on Engine."
- **Insufficient-material short-circuit lives in `evaluate`** — relies on `validate_post_parse` 2-king invariant; total_count == 3 then implies one non-king piece. KBvKB same-color deferred to M6 alongside the bishop-pair term.
- **Mutation testing PST-data exclusion via split file.** Plan-review pass 1 surfaced ~185 `delete -` mutants on PeSTO PST literals. The B1 PSQT-symmetry test is structurally invariant under sign flips that affect both color sides; A1 is also color-symmetric. Final-review pass 1 verified the gap empirically. Resolution: split vendored data into `src/eval/data.rs` + file-level `exclude_globs`. Final mutation result on M3.A diff (82 mutants in scope, down from 265): **0 missed; 71 caught; 1 timeout (effective catch); 10 unviable.**
- **`info` emitted before wait loop.** Real depth-1 score should be visible immediately; M2.D's post-wait pattern was acceptable when score was always 0.
- **D17/F1 use KvK midboard** (`8/8/8/8/4K3/8/8/4k3`) — first plan claim of "8+ ties at startpos" was empirically false (PeSTO PSTs make startpos moves nearly all-distinct). KvK with all 8 white-king moves leaving KvK genuinely produces 8 tied scores.
- **Anchor tests pin combined material+PST values, not bare material.** A8: `PSQT[White][Pawn][A2] == 47` (= 82 + MG_PAWN[48] = -35); A9: `-47`; A10: `PSQT[White][King][E1] == 8` (king material = 0; PST is the entire term).
- **Two engine tests rewritten for KvK** — original tests asserted "different seeds → different bestmoves from startpos", but PeSTO eval makes `g1f3` (Nf3) the unique startpos best at depth 1. KvK exposes the reservoir-sampling code path on the orchestrator side too.
- **`make_move_no_from_scratch_in_release` perf sentinel** retained (single test); docstring expanded to cover both invariants (zobrist + eval). 100 ns/cycle threshold catches accidental from-scratch reintroduction on either path.

### M3.A — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 615 lib + 5 uci-integration + 24 other = **644 fast + 4 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo llvm-cov --summary-only --lib` | Total **96.08% region**. `eval.rs` 87.80% region (gap is `cargo-llvm-cov` not instrumenting the const-fn `build_psqt`; PSQT data verified by A8/A9/A10 + B1). `mov.rs` 96.85%, `search.rs` 98.56%, `position.rs` 98.68%. |
| `cargo mutants --in-diff` | 82 mutants generated (185 PST-data mutants excluded via `src/eval/data.rs` file-level rule); **71 caught + 1 timeout (effective catch on `delete ! in GreedyMover::go`) + 10 unviable; 0 missed.** |
| `scripts/match.sh compliance` | 40/40 fastchess `--compliance` steps pass. |
| `scripts/match.sh self-play` | 2 games at 20 ply each, both `[Termination "adjudication"]`. 0 illegal, 0 stalled. |
| `scripts/match.sh vs-stockfish` | 2 games vs Stockfish 18 capped at UCI_Elo=1320: game 1 [adjudication] 37 ply; game 2 [normal] 41 ply (sf18 mates). 0 illegal, 0 stalled. |
| Smoke (uci) | `info depth 1 score cp 36 nodes 20 time 0 pv g1f3` then `bestmove g1f3` from startpos — real `score cp`, not M2.D's `cp 0` placeholder. |
| Benchmark | **Skipped** — UCI dispatch + depth-1 evaluate are not search-hot-path. M3.C alpha-beta will be the first phase with a meaningful nps figure. |

All review loops converged: plan-review 3 passes, test-suite review 2 passes, final-review 3 passes (the third closed the must-fix on PST-data mutation exclusion via split-file refactor). Plan archived at `docs/plans/m3.a.md`. ADR-0014 codifies eval composition. `bench/m3.md` carries the per-phase milestone summary.

### M2.E — what landed

The external validation layer that closes M2:

- **`scripts/install-fastchess.sh`** — idempotent SHA256-pinned installer. Three constants (`EXPECTED_RELEASE_TAG`, `EXPECTED_VERSION_LINE`, `EXPECTED_SHA256`); platform + curl pre-flights; pre-flight version-line gate short-circuits on already-installed runs. Bumping the pinned release is a three-line edit.
- **`scripts/match.sh`** — three-subcommand wrapper: `compliance`, `self-play`, `vs-stockfish`. Locator: `vendor/fastchess/fastchess` first, `command -v fastchess` fallback; either way the resolved binary is gated against `EXPECTED_VERSION_LINE`. `cargo build --release` runs before each subcommand. Adjudication: `-maxmoves 300 -resign movecount=3 score=600`; `-draw` deliberately omitted. `ulimit -n 4096 || true` (best-effort) to dodge the macOS-256-fd default.
- **ADR-0012** — codifies fastchess as the runner, `vendor/fastchess/` as the install path, the engine-registry-via-shell-wrapper convention (no `engines.json`), output-path layout for smoke and SPRT, the M2 smoke contract, and the `RandomMover::go` info-line requirement.
- **`RandomMover::go` info-line emission** (~6 LOC in `src/search.rs`) — emits `info depth 0 score cp 0 nodes 1 time <ms> pv <move-or-0000>` via `info_sink` after the wait loop. `score cp 0` is empirically required by fastchess `--compliance` Step 12. `pv 0000` placeholder for mate / stalemate / empty searchmoves filter.
- **E37 + E33 amendment** — `tests/uci_integration.rs::integration_unknown_command_silently_ignored` (new) pins the silence-on-unknown contract via `assert_eq!(lines, vec!["readyok"])`. E33 amended with `assert!(lines.iter().any(|l| l.starts_with("info depth ")))` to pin the §11 emission. Both reuse existing helpers; no new test file.
- **`docs/workflow.md`** — new "Running a match" section pointing at `scripts/match.sh` subcommands.
- **`bench/m2.md`** — milestone summary per ADR-0010 / ADR-0012.

### M2.E — implementation highlights

- **`fastchess --compliance` Step 12 was the load-bearing discovery.** Plan-review pass-1 reviewer empirically ran the compliance check against the M2.D binary and found Steps 1–11 pass but Step 12 ("Check if engine prints an info line") fails. Plan §11 added the info-line emission to close it. After the fix, fastchess 1.8.0-alpha runs all 40 compliance steps to completion ("Engine passed all compliance checks.") — Steps 13–40 were previously gated behind the failing Step 12.
- **`score cp 0` is required.** The plan's first-draft committed on "no `score cp`" with the rationale "would be a lie." Empirical probe overturned that — `info depth N nodes K time M pv MV` without `score` fails Step 12 regardless of other fields. `info depth N score cp 0 nodes K time M pv MV` passes. fastchess defaults missing scores to 0 in adjudication anyway, so emitting `0` explicitly is the same value, just visible.
- **`-rounds 1 -repeat` not `-rounds 2 -repeat`.** Plan-review pass-1 SF4: with deterministic seeds (Random_Seed=1 vs =2) and no opening book, `-rounds 2 -repeat` produces 4 PGNs that are 2 identical trajectories duplicated. Cutting to `-rounds 1 -repeat` yields 2 honest games.
- **Adjudication knobs surprised on the upside.** Plan anticipated `-maxmoves 300` would fire on most random-vs-random games. Empirically, both self-play games naturally reached insufficient material (343 ply / 450 ply; both `[Termination "normal"]`) — random play captures pieces fast enough that lone-king endgames arrive before the 600-ply cap. vs-Stockfish smoke also natural mate at 56 ply / 25 ply. The `-resign` knob was a no-op for these 4 games — kept as a no-cost hedge for future runs.
- **No new test file.** First plan draft proposed `tests/uci_smoke.rs`; pass-1 must-fix #2 surfaced that S1–S4 of the proposed tests duplicated existing E33–E35 with weaker assertions. Resolution: drop the new file, add only E37 (the genuinely novel silent-on-unknown test) to `tests/uci_integration.rs` reusing existing helpers.
- **`ulimit -n 4096`** baked into `match.sh` as a best-effort prep step. macOS default of 256 was too low even at `-concurrency 1`.

### M2.E — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 591 lib + 5 uci-integration + 24 other = **620 fast + 6 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo llvm-cov --summary-only --lib` | `search.rs`: 78.58% region / 78.69% line (pre-existing gap on the two `#[ignore]`-gated documentation helpers + Search trait no-op defaults; the new info-line emission path is fully covered). `engine.rs`: 97.41% region / 96.92% line. Total: 95.34% region / 95.43% line. |
| `cargo mutants --in-diff` | 1 mutant generated; **1 caught; 0 missed; 0 timeouts; 0 unviable**. The single mutant (replace `RandomMover::go` body with `Default::default()`) is caught by E33's `info depth ` assertion + existing M2.D anchors. |
| `scripts/match.sh compliance` | All 40 steps pass per fastchess `--compliance` ("Engine passed all compliance checks."). |
| `scripts/match.sh self-play` | 2 games at 343 / 450 plies; both `[Termination "normal"]` (Draw by insufficient mating material). C3=C4=C5=0. |
| `scripts/match.sh vs-stockfish` | 2 games at 56 / 25 plies; both `[Termination "normal"]` (sf18 mated). C3=C4=C5=0. |

All review loops converged. Plan archived at `docs/plans/m2.e.md`. ADR-0012 codifies the harness layout. Plan went through 3 reviewer passes; test-suite review through 2; final review through 2.

### M2.D — what landed

The `RandomMover` SplitMix64-seeded `Search` impl + the engine's first real UCI option:

- **`RandomMover` in `src/search.rs`** — picks uniformly at random from the legal-move list (post-`searchmoves` filter) via one SplitMix64 step per `go`. Honors `infinite` / `movetime` / `ponder` by polling `should_abort` (1 ms cadence). Same race-free always-compute-candidate-first invariant as `Stub`.
- **`splitmix64_next`** — public-domain reference vendored verbatim. ~10 lines, no new dep. Modulo bias for legal-move counts ≤ 218 is < 4×10⁻¹⁸; no rejection sampling.
- **`Search` trait extended** with `set_seed(&mut self, _seed: u64) {}` and `reset(&mut self) {}` — both default-no-op. `RandomMover` overrides; M3+ alpha-beta and `InfoEmittingFake` inherit no-op.
- **`Random_Seed` UCI option** — `option name Random_Seed type spin default 0 min 0 max 2147483647`. Case-insensitive name match. Strict value validation against `MAX_RANDOM_SEED = 2_147_483_647` (= `i32::MAX`, the protocol-declared `max`). Silent on success even under `debug on`; debug-on echo on bad value.
- **PRNG semantics** — continuous state across `go` calls; `setoption` resets state immediately to the new seed (not deferred to `ucinewgame`); `ucinewgame` resets state to the current seed.
- **`Engine::join_in_flight_worker`** — new private helper consolidating the "signal stop + join" idiom previously inlined in `handle_go`'s back-to-back path. Now also called by `handle_ucinewgame` and `handle_setoption`'s `Random_Seed` success arm before they acquire the search mutex — closes a deadlock that would have hung the engine if a GUI sent `ucinewgame` or `setoption` while a `go infinite` worker was holding the mutex.
- **End-to-end self-play test E36** — drives the binary via `position startpos moves <accumulated>` + `go movetime 10` until terminal. Validates every bestmove with `Move::from_uci` against a parallel `Position`. Pinned at `SELF_PLAY_SEED = 8`, terminates at ply 106 with `bestmove d3d1`.

### M2.D — implementation highlights

- **No new fields on `Engine`.** The seed lives solely in `RandomMover.seed` to eliminate two-source-of-truth drift.
- **The plan's claim that "non-terminating random self-play is a strong signal of a movegen bug" was empirically wrong.** Random play between two random movers in a draw-rule-less engine can shuffle pieces back and forth indefinitely; we don't implement 50-move / threefold yet (explicit non-goal). Seeds 0–7 cycled past 300 ply; seed 8 was the first to terminate. The `find_terminating_seed_for_e36` `#[ignore]` test documents the search.
- **Strict O3 resolution on `Random_Seed`** — values in `(MAX_RANDOM_SEED, u32::MAX]` (valid `u32` but above declared `max`) are rejected. Stockfish 18 silently accepts these; we honor the declared protocol contract instead.

### M2.D — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 598 lib + 4 uci-integration + 24 other = **626 fast + 6 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` | `engine.rs`: 97.41% region / 96.92% line / 91.18% function. `search.rs`: 77.15% region / 78.69% line — production `splitmix64_next` and `RandomMover` at 100%; cosmetic gap is the two `#[ignore]` documentation helpers and the trait's no-op defaults (overridden by `RandomMover`, never called on `InfoEmittingFake`). |
| `cargo mutants --in-diff` | 23 mutants generated; **23 caught; 0 missed; 0 timeouts; 0 unviable**. |
| Smoke | `printf 'uci\nquit\n' | target/release/chess` produces 4 lines including `option name Random_Seed type spin default 0 min 0 max 2147483647`; exits ≤ 2 s. |
| Benchmark | **Skipped** — UCI dispatch is per-line; `RandomMover::go`'s per-`go` cost is one SplitMix64 step (~0.6 ns). Same precedent as M2.A / M2.B / M2.C. |

All three review loops converged. Plan archived at `docs/plans/m2.d.md`. The plan went 5 reviewer passes; the test suite 2; the final review 2 (the second pass's mutation rerun caught the new `join_in_flight_worker` helper at 100%).

### M2.C — what landed

The `engine` and `search` modules — the UCI I/O loop, command dispatch, and `Search` trait scaffolding:

- **`Engine<W, S>`** generic over stdout writer + Search impl. Holds `Position`, `debug` flag, `Arc<AtomicBool>` cancellation, `Arc<Mutex<W>>` stdout, `Arc<Mutex<S>>` search, and an `Option<JoinHandle>` for the in-flight worker.
- **Reader thread + mpsc** — `reader_loop(impl BufRead, Sender<Command>)`; EOF synthesizes `Quit`; reader exits on any `Quit` (parsed or synthesized). No double-Quit on `quit\n` + EOF.
- **Per-`go` worker thread** — `handle_go` joins the previous worker, clears the flag, builds `SearchContext`, spawns a new worker. Worker locks the search mutex, calls `Search::go`, writes `bestmove` directly to stdout under the shared mutex.
- **`Stub` Search impl** — deterministic lex-first legal move; honors `infinite` / `movetime` / `ponder` by polling `should_abort` (1 ms cadence) until cancelled / deadline expires. Always computes the candidate before checking cancellation — race-free under quit-immediately-after-go.
- **`run_stdio()` -> !** — production wrapper: spawns reader thread on `io::stdin()`, builds `Engine` with `io::stdout()` and `Stub`, drives `run`, then `process::exit(0)`.

### M2.C — implementation highlights

- **Threading model codified by ADR-0011** — reader thread → mpsc → main-as-orchestrator + per-`go` worker + `Arc<AtomicBool>` cancellation. Same primitive scales unchanged through M3 alpha-beta and M8 lazy-SMP.
- **`handle_quit` joins the worker** (bounded by cancellation polling cadence) so `bestmove` is in stdout before `run` returns. Required for testability outside `run_stdio`'s `process::exit` safety net; documented as a v3 amendment to ADR-0011.
- **`position` reset-on-error**, asymmetric:
  - FEN-parse-error → keep prior position (no safe base to fall back to).
  - Move-error → reset to spec base (parsed `startpos` or successfully-parsed FEN), no moves applied.
  - Both emit `info string position rejected: …` unconditionally (protocol-legal; silent rejection in tournament play would be the worst failure mode).
- **`searchmoves` filtering** silently drops bad entries (parse error or illegal-for-position). All-bad list yields `bestmove 0000`.
- **`handle_debug` is silent** — only toggles `self.debug`. `setoption` / `register` / `ponderhit` / `Unknown` are silent when debug=off; emit `info string … received: …` when debug=on.
- **Always-spawn handle_go** — back-to-back `go` joins the previous worker before spawning the new one (matches Stockfish). Implicit-stop semantics.
- **Generics over trait objects** — `Engine<W: Write + Send + 'static, S: Search + Send + 'static>` for cleaner stack traces and zero virtual-call overhead. `search: Arc<Mutex<S>>` lets `handle_go` clone the Arc into the spawned worker (Search::go takes `&mut self`).
- **`Stub` always computes the candidate before checking cancellation** — eliminates a race where `quit` arriving immediately after `go` could flip the flag before the worker thread was scheduled, causing `bestmove 0000` instead of the legitimate lex-first move.

### M2.C — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 583 lib + 3 uci-integration + 24 other = **610 fast + 4 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` | `engine.rs`: 97.30% region / 97.09% line / 93.44% function. `search.rs`: 98.29% region / 97.58% line. Remaining uncovered: `unreachable!()` on rx disconnect, `Err(_)` reader break (untestable with Cursor), `run_stdio` body (covered via integration tests through the binary). |
| `cargo mutants --in-diff` | 30 mutants generated; **25 caught + 2 timeouts (effective catches via test hangs) + 3 unviable; 0 missed**. |
| Smoke | `printf 'uci\nquit\n' | target/release/chess` → `id name chess 0.1.0 / id author Alex Feldgendler / uciok` and exits within 2 s. |
| Benchmark | **Skipped** — UCI dispatch is per-line, never on a search hot path. Same precedent as M2.A / M2.B. |

All three review loops converged. Plan archived at `docs/plans/m2.c.md`. ADR-0011 codifies the threading model.

### M2.C — workflow notes (stop-loss trigger on plan-reviewer tier)

Per `docs/workflow.md` "Stop-loss" runbook — recording the trigger so the rollback isn't silently forgotten:

- **Trigger.** Sonnet plan-reviewer (the originally-tiered model) missed two must-fix items on the M2.C v1 plan that Opus plan-reviewer caught when run in parallel during the calibration pass. Both fall squarely in the "correctness" dimension that plan-review is supposed to cover:
  - `Box<dyn Search>` is not `Clone` and cannot be moved into `thread::spawn`, so the orchestrator's `handle_go` could not have spawned a per-`go` worker as planned. Would have stalled Coder-B at implementation time.
  - The reader-loop's "EOF synthesizes `Quit`" rule made the orchestrator's "channel-disconnect" defensive branch unreachable — a latent dead-code path and mutation-test survivor.
- **Action taken.** Reverted plan-reviewer from Sonnet to Opus (`.claude/agents/plan-reviewer.md`, table row in `docs/workflow.md`, calibration-log entry in `docs/workflow.md`). Sonnet for test-suite-reviewer / coder / research stays — those tiers haven't fired the stop-loss.
- **Re-eval condition.** If a future milestone presents a substantively different artifact shape (e.g. simpler plans, novel domain), re-run the calibration pass before re-attempting Sonnet for plan-review.

### M2.B — what landed

The `uci` module — pure-function parser for UCI 2006 GUI→engine commands:

- **`parse_uci_line(&str) -> Command`** — sole public entry point. Total over input: every string yields a `Command`, with `Command::Unknown` for unrecognized or malformed input.
- **`Command` enum** + helper types: `DebugMode`, `Register` (with `Identify { name, code }` invariant variant), `PositionSpec`, `GoParams`.
- **5 private sub-parsers** (`parse_debug`/`parse_setoption`/`parse_register`/`parse_position`/`parse_go`) — tested through the public surface, not directly.
- **`SEARCHMOVES_TERMINATORS` const** — the 11 `go` body keywords other than `searchmoves` itself; pinned by data-invariant test against drift between source / test / plan §6.a.

### M2.B — implementation highlights

- **Per-command leniency model is asymmetric**, grounded in 12 empirical Stockfish 18 probes (see `docs/plans/m2.b.md` §3.1):
  - No-arg commands ignore trailing junk (`isready xyzzybanana` → `IsReady`).
  - `debug` is strict-exact-2-tokens (`debug on garbage` → `Unknown`).
  - `position` is lenient-stop after the position spec (junk between `startpos` and `moves` discards the `moves` clause).
  - `go` is lenient-skip on unknown body tokens (silently dropped, parsing continues).
- **Strict-first-token rule chosen over the spec's literal `joho debug on → debug on` rule** — Stockfish 18 doesn't honor that rule either, and silently swallowing GUI-side typos is anti-debuggability.
- **Move strings + FEN strings collected raw**, not parsed at this layer. M2.C parses moves via M2.A's `from_uci` and FEN via the existing FEN parser.
- **Numeric type widths grounded in spec**: `wtime`/`btime`/`movetime` `i64` (clock can go negative); `winc`/`binc` `u64` (spec "if x > 0"); `nodes` `u64`; `depth`/`movestogo`/`mate` `u32`. Negative or out-of-range inputs fail to parse and yield `None` per §6.b.
- **Total function — no `Result`, no panics.** All numeric parsing via `str::parse::<T>()` returning `Result`; tokenization via `split_whitespace` (panic-free); the only `unwrap` is a `peek-then-next` pattern provably safe by `Iterator` contract.

### M2.B — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 542 fast + 4 ignored (446 prior + 96 new M2.B). All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` (`uci.rs`) | 99.90% region / 100.00% line / 100.00% function. The 1 uncovered region is the `iter.next().unwrap()` panic branch in `searchmoves` collection (peek-then-next pattern; unreachable by Iterator contract). |
| `cargo mutants --in-diff` | 55 mutants generated; **49 caught, 6 unviable** (compile failures); 0 missed. |
| Benchmark | **Skipped** — UCI command parsing is per command (line-at-a-time, never inside a search loop). Microbenchmark would measure noise. Same precedent as M2.A. |

All three review loops converged: plan after 5 reviewer passes; test suite after 3; final code+tests on first pass. Plan archived at `docs/plans/m2.b.md`. No new ADR.

### M2.A — what landed

The two functions bridging our internal `Move` type and the UCI long-algebraic wire format:

- **`Move::to_uci(self) -> String`** — thin wrapper around the existing `Display` impl; both produce identical bytes. Self-documenting at M2 protocol call sites.
- **`Move::from_uci(s: &str, pos: &Position) -> Result<Move, UciMoveError>`** — generate-and-match: enumerates `generate_moves(pos)`, finds the unique move matching the parsed `(from, to, promotion_kind)`. Defers legality entirely to movegen (consistent with ADR-0007).
- **`UciMoveError`** — four-variant enum: `Malformed` / `IllegalPromotionPiece` / `NullMove` / `IllegalForPosition`. Re-exported from `src/lib.rs`.
- **38 new tests** in `src/mov.rs::tests`: 12 `to_uci` anchors (one per flag, including all four PromoCapture kinds) + 10 `from_uci` positive anchors (including a check-evasion case) + table-driven negative-parse + 10 position-dependent rejection tests + round-trip on the curated `CASES` corpus + round-trip on D1 enumeration of `UCI_SEED_FENS` (canonical 6 + EP-horizontal-pin + EP-double-check + mate + stalemate) + round-trip proptest on D2-reachable positions + `(from, to, promo)` uniqueness invariant proptest.

### M2.A — implementation highlights

- **Generate-and-match strategy.** Avoids ~150 lines of hand-rolled disambiguation logic (king-step vs. castle, push vs. EP, single vs. double, promo-capture inference) by reusing `generate_moves`'s legality filter. Cost is irrelevant — UCI move parsing is per-`position` command, not on the search hot path.
- **`(from, to, promotion_kind)` uniqueness invariant** — derived from chess-rules first principles in the plan. `from_uci` includes a debug-only `debug_assert!` on the invariant in addition to the property test, so a future regression in `generate_moves` fires loudly at the consumer site.
- **Strict lowercase input.** Files a–h and promo letter n/b/r/q lowercase only. Relax cost (if a real GUI ever sends uppercase) is a one-line `match bytes[4] | 0x20` change.
- **Null move `0000`** rejected as `UciMoveError::NullMove`. We have no `Move::NULL` sentinel today (deferred to null-move pruning, M5).
- **ASCII guard** before any byte slicing — without it, `&s[0..2]` / `&s[2..4]` would panic on a non-char-boundary index for inputs like `"e2e°"` (5 bytes, `&s[2..4]` ends mid-codepoint).

### M2.A — verification

| Metric | Result |
|---|---|
| Tests | 484 fast + 4 ignored. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov --summary-only --lib` (`mov.rs`) | 96.55% region / 95.75% line / 95.20% function. Uncovered lines = pre-existing `#[ignore]`-gated bench + pre-existing `unreachable!()` arms + `unwrap_or_else` panic branches in tests that don't fire when tests pass. |
| `cargo mutants --in-diff` | 19 mutants generated; **18 caught, 1 unviable** (`from_uci -> Ok(Default::default())` — `Move` has no `Default`); 0 missed. |
| Benchmark | **Skipped** — UCI move parsing is per-`position` command, not on the search hot path; a microbenchmark would measure noise. |

All three review loops (plan, test-suite, final code+tests) converged. Plan archived at `docs/plans/m2.a.md`. No ADR binds on this phase.

### M1 — prior milestone

### M1.G — what landed

The `perft` module — the engine's move-generation validation + measurement layer:

- **`src/perft.rs`** with `perft`, `perft_bulk` (CPW depth-1 leaf-skip), `divide` (UCI-sorted), `perft_categorized` (internal-only category counts per ADR-0006), and the `PerftCounts` struct.
- **Stockfish-regenerated fixtures** at `tests/fixtures/perft_canonical6.txt` (canonical 6 D1–D6) + `tests/fixtures/perft_whittington.epd` (174 positions D1–D4). Per ADR-0006 Stockfish 18 is the sole oracle.
- **`scripts/regen-perft-fixtures.sh`** — idempotent fixture-regeneration script; spawns one Stockfish per (fen, depth) pair, parses `Nodes searched:` lines.
- **Test partition** matching the plan's §"Fast-suite wall-clock budget": D1–D3 + light-D4 default-fast (sub-second); heavy-D4 + D5 + D6 + Whittington D4 `#[ignore]`-gated.
- **`criterion 0.7` benchmark harness** at `benches/perft.rs` + `benches/movegen.rs`; first baseline committed at `bench/m1.g.md` per **ADR-0010** (this phase's binding ADR).
- **M1.F smoke benchmark deleted** — replaced by the criterion harness.

### M1.G — verification

| Metric | Result |
|---|---|
| Tests | 446 fast + 9 ignored (4 perft-integration ignored slow + 1 perft-unit Kiwipete D4 + 4 prior). All ignored slow tests verified to pass: D4-heavy 0.23s; D5 + Whittington 2.47s combined; **D6 117.93s**. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo audit` + `cargo deny check` | clean (criterion + transitives MIT/Apache-2.0) |
| `cargo llvm-cov --summary-only --lib` | `perft.rs`: 98.32% region / 98.25% line / 93.75% function. Crate total 95.19%. |
| Headline perft throughput (Apple M4, release) | starting D4 plain **33 Mnps**; starting D4 bulk **119 Mnps**; Kiwipete D3 bulk **168 Mnps** — meeting the M1 ≥100 Mnps exit criterion on the bulk path. |
| Headline movegen throughput | 81–277 ns/call across canonical-6, ~200 ns/call typical. |
| D6 end-to-end | ~22B nodes / 117.93s ≈ ~187 Mnps (consistent with bench numbers). |

All four review loops (plan, test-suite, final code+tests, plus the post-final benchmark capture) converged. Plan archived at `docs/plans/m1.g.md`. ADR-0010 codifies the bench format.

### M1.F — what landed

The `movegen` module — the engine's legal-move-enumeration surface:

- **`MoveList`** — stack-allocated `[MaybeUninit<Move>; 256]` + `len: u16`. `push` and `clear` are `pub(crate)` so the soundness contract stays inside the crate; `as_slice` exposes `&[Move]` via a single justified `unsafe` block.
- **`generate_moves(pos, &mut MoveList)`** — legal-direct emission, mask-based, with check-evasion specialization (single check → king + capture-the-checker + block; double check → king-only).
- **`in_check(pos)`** — public side-to-move checker.
- **Per-call `MaskInfo`** — `checkers`, `pinned`, `capture_mask`, `push_mask`, `king_danger` (computed against `occupancy ^ king_bb` per the king-flee gotcha), `pin_rays[64]`. No cache on `Position`.
- **EP horizontal-pin filter** AND symmetric **diagonal discovery filter** at emission time (covers Position-3 trap and the diagonal counterpart).
- **Castling** — only when not in check; transit + destination squares not in `king_danger`. Mailbox `debug_assert!` on king/rook starting squares (release-trusted; the FEN parser validates at the boundary).
- **`validate_post_parse` extended** — rejects FENs whose castling rights don't match the king/rook mailbox; new `FenError::InconsistentCastlingRights`.

### M1.F — implementation highlights

- **Const-fn pre-computed leaf attack tables** (`PAWN_ATTACKS`, `KNIGHT_ATTACKS`, `KING_ATTACKS`) — built at compile time; no `LazyLock` overhead on the hot path.
- **Defensive-checks-debug-only convention** added to `docs/workflow.md` "Final review loop" → Code quality. Codified by the castling §13 invariant: validation at FEN parse, `debug_assert!` at consumers, release trusts.
- **`prop_no_legal_move_leaves_us_in_check`** — proptest with deterministic SplitMix64 random walk over §6 edge fixtures + canonical 6 seeds. Pins the legal-direct invariant against any future regression.
- **EP-double-check `count == 2` assertion** — in-crate test using crate-private `checkers_of` to verify §6.3 taxonomy.

### M1.F — verification

| Metric | Result |
|---|---|
| Tests | **401 passing** (372 lib + 12 movegen integration + 9 zobrist-vector + 3 fen + 2 magic + 3 make_unmake) |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test --release --test movegen -- --ignored` (smoke throughput) | starting 68 ns/call, Kiwipete 114, Pos-3 37, Pos-4 43, Pos-5 129, Pos-6 99 — all 50× under the 5 µs/call threshold |
| Plan, test-suite, final review loops | converged after 3 / 3 / 2 rounds |

Plan archived at `docs/plans/m1.f.md`. ADR-0007 codifies legal-direct + mask-based + check-evasion specialization.

### M1.E — prior milestone

## Prior status

### M1.E — what landed

The `mov` module — the engine's first mutating layer:

- **`Move`** — 16-bit packed: bits 0–5 from / 6–11 to / 12–15 flag.
- **`MoveFlag`** — 14 valid codes (6 and 7 deliberately absent).
- **`Undo`** (~16 B) — captured piece + prior aux state + prior zobrist.
- **`make_move(&mut Position, Move) -> Undo`** and **`unmake_move(&mut Position, Move, Undo)`** — free functions per ADR-0004; ergonomic `Position::make_move` / `Position::unmake_move` delegates.

All special cases: quiet, double-push, capture, en passant, kingside/queenside castle, four `*Promo` and four `*PromoCapture`.

### M1.E — implementation highlights

- **Castling-rights update** via 64-entry `CASTLING_MASK` table (from/to indexed) — handles the rook-captured-on-corner Kiwipete depth-4 trap.
- **Incremental Zobrist** with debug-build round-trip assert against `from_scratch`.
- **Always-on release-build perf sentinel** at 100 ns/cycle threshold guards against accidental from-scratch reintroduction.
- **`Position` extensions:** `clear_square`, `refresh_zobrist_from`, method delegates, `BitAnd` impl on `CastlingRights`.

### M1.E — verification

| Metric | Result |
|---|---|
| Tests | **303 passing** + 3 ignored benches (286 lib + 3 fen + 2 magic + 3 make/unmake integ + 9 zobrist-vector) |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov` on `mov.rs` | 95.65% region / 94.71% line (gaps: const-fn at compile time + `unreachable!()` arms + `debug_assert` formatters) |
| `cargo mutants --in-diff` | **0 survivors** on 123 mutants (107 caught, 16 unviable) |
| Throughput (Apple Silicon, release) | quiet 27 ns/cycle, capture 38, EP 36, castle 42, promo 22 — under <50 ns/cycle target |

All three review loops (plan, test-suite, final) converged. Plan archived at `docs/plans/m1.e.md`.

### What's next

**M1.G — perft + benchmarks.** Recursive perft driver with bulk-counting at depth 1, canonical-6 fixtures via Stockfish, EPD-corpus regression (Whittington `perft.epd`, Stockfish-generated counts), `criterion` benchmark harness with baseline saving.

### M1.D ✓ — Polyglot Zobrist hashing

- Vendored the Polyglot 781-key set verbatim from `docs/reference/polyglot-book-format.md`.
- Implemented the EP-only-when-pseudo-legal hashing rule, asymmetric turn key (XORed iff WHITE-to-move), four-key castling encoding.
- `Position::zobrist` field with `refresh_zobrist()` setter; 9 published Polyglot test vectors as gold-standard interop check.
- ADR-0009 landed in the same commit.

| Metric | Result |
|---|---|
| Tests | **238 passing** + 1 ignored bench (224 lib + 3 fen + 2 magic + 9 zobrist-vector) |
| `cargo mutants --in-diff` | 0 survivors on 1067-line diff |
| Throughput | `from_scratch(starting_position)` ≈ 50.8 ns/op; `ep_file_to_hash` ≈ 0.72 ns/op on no-EP early-exit |

## Milestones

### M0 — Scope & architecture ✓
Resolve foundational architectural questions.

**Outcome:**
- Variant chess: out of scope (`decisions/0001`).
- Target platform: Apple Silicon macOS primary, mobile downstream (`decisions/0002`).
- Source-code research restriction: no engine source code (`decisions/0003`).
- NNUE-readiness: `make_move`/`unmake_move` as interceptable function calls (`decisions/0004`).
- Implied: `u64` bitboards, magic bitboards, 16-bit move encoding, single-variant codebase.

### M1 — Move generator + perft (standard chess)
Bitboards, all rules of standard chess, no search, no eval. Validated against perft fixtures generated by Stockfish 18 (sole oracle — see `decisions/0006-stockfish-as-perft-oracle.md`) on the canonical 6 test positions (starting position, Kiwipete, Position 3, Position 4, Position 5, Position 6) to depth 6 or 7. A larger EPD-format regression suite (also Stockfish-generated) is an optional bulk-confidence layer.

**Exit criteria:** perft suite passes; benchmark harness records nodes/sec for move generation. Target ≥100 Mnps single-threaded on Apple Silicon M4 (≥200 Mnps would be excellent).

**TDD applicability:** maximum. Perft gives exact integer answers from known positions.

**Status — prior-art research complete (2026-04-27).** Three parallel research reports in `docs/research/`: `m1-engine-architecture.md`, `m1-magic-bitboards.md`, `m1-perft-and-rust.md`. Synthesis recorded in `docs/prior-art.md`.

**ADRs to write per-phase, as each binds.** None of these gates M1.A. Material for all three is already captured in `docs/prior-art.md` (headline calls) and `docs/research/m1-engine-architecture.md` + `docs/research/m1-magic-bitboards.md` (full reasoning). Each ADR lands just before the phase that depends on it, so the ADR can be refined by anything we learn during plan-mode for that phase:

- **ADR-0008** — Magic bitboards: fancy variant with variable shift; magic constants generated and committed via a separate `magicgen` binary; attack tables built at runtime startup; slow ray-walker kept as permanent differential-test oracle. **Binds on M1.C.**
- **ADR-0009** — Polyglot Zobrist key set + refined EP-file hashing rule (hash EP file only when an EP capture is actually pseudo-legally possible). **Binds on M1.D.**
- **ADR-0007** — Legal-direct move generation (vs. pseudo-legal-and-filter). **Binds on M1.F.**

**Sub-phases.** M1 is decomposed into seven plan-and-execute cycles. Each phase gets its own plan-mode pass with the self-review loop (see `workflow.md`), executes independently, lands its own commit(s), runs its own tests before we move to the next phase.

| Phase | Scope | Approx size |
|---|---|---|
| **M1.A** ✓ — Skeleton + primitives | `lib.rs` split, `Cargo.toml` release profile (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`), module skeleton, `Square` type, `Bitboard` type and primitive operations, unit tests for primitives | ~500–800 lines (actual: ~720 lines, 50 tests) |
| **M1.B** ✓ — Position + FEN | `Position` struct (6+2 bitboards + mailbox + cached king squares + auxiliary state), `Color` / `PieceKind` / `Piece` / `CastlingRights` types, FEN parse/format per Edwards 1994 §16.1 (strict syntactic + structural sanity checks), position-equality tests | ~600–900 lines (actual: ~2175 lines including ~40 negative-parse tests, 139 unit + 3 integration tests) |
| **M1.C** ✓ — Sliding-piece attacks | Slow ray-walker as permanent `slow_attacks` oracle module, `src/bin/magicgen.rs` (search + validation + codegen), generated magic constants source file, fancy-magic attack lookups, differential tests over all ~108k (square, occupancy) pairs | ~800–1200 lines (actual: ~1934 lines including ~145-line generated constants file and ~68-line ADR; 50 new unit + 2 integration tests) |
| **M1.D** ✓ — Zobrist | Polyglot 781-key table vendored from in-tree spec, EP-only-when-pseudo-legal hashing rule, side-to-move asymmetric turn key, `Position::zobrist` field + `refresh_zobrist()` setter; 9 published test vectors as gold-standard interop check. M1.E will add the incremental hash update + debug round-trip assert in make/unmake. | ~250–350 lines (actual: ~787 lines including 121-line vendored data file and ~120-line ADR; 30 unit + 5 property + 3 position + 9 integration tests) |
| **M1.E** ✓ — Make/unmake | `Move` (16-bit), `MoveFlag` (14 valid), `Undo` (~16 B), free functions `make_move`/`unmake_move` per ADR-0004, all special cases (castling, EP, all 8 promotion variants, double-push), incremental Zobrist with debug round-trip assert + release-build perf sentinel, round-trip property tests, ergonomic `Position::make_move`/`Position::unmake_move` delegates | ~600–900 lines (actual: ~2050 lines including ~700-line plan; 65 new tests) |
| **M1.F** ✓ — Legal move generation | `movegen` module with `MoveList` + `generate_moves` + `in_check`; per-call `MaskInfo` (checkers, pinned, capture/push masks, king_danger, pin_rays); per-piece emit fns; EP horizontal-pin + symmetric diagonal-pin filters; castling with mailbox `debug_assert!`s; `validate_post_parse` extended for castling consistency; defensive-checks-debug-only convention codified | ~1500–2000 lines (actual: ~3700 lines including ~700-line plan, ~110-line ADR; 91 new tests) |
| **M1.G** ✓ — Perft + benchmarks | Recursive perft (plain + bulk-count + divide + categorized) with 174-position Whittington EPD regression suite (Stockfish-regenerated counts) and canonical-6 D1–D6 fixtures, `criterion` benchmark harness with baseline saving (ADR-0010), 119 Mnps bulk on starting D4 (meets M1 exit criterion) | ~500–800 lines (actual: ~1300 lines incl. ~700-line plan, ~80-line ADR; 21 perft unit tests + 14 integration tests, plus the fixture parser) |

Phases A→F are foundational and largely sequential. G is the validation layer.

### M2 — Random-mover engine speaking UCI
Plays legal random moves through a tournament harness (fastchess; see ADR-0012 below). No search depth. Establishes the UCI skeleton, time-management harness, and tournament tooling before any search complexity.

**Exit criteria:** plays a complete game through `fastchess` against itself or another engine without protocol errors or illegal moves.

**TDD applicability:** very high. UCI is a text-in/text-out protocol — parsers and command dispatch are pure functions. Random move selection is deterministic given a seed. The end-to-end game loop is the only piece needing process-spawning integration tests.

**ADRs to write per-phase, as each binds.**

- **ADR-0011** — UCI I/O threading model. **Binds on M2.C.**
  - Constraint: UCI mandates stdin readable during search (`isready` → `readyok` mid-search; `stop` aborts `go infinite`).
  - Choice space: dedicated reader thread + cancellation channel to a search worker, vs. single thread polling stdin between search iterations.
  - Research: `research/m2-uci-threading.md` recommends reader thread + mpsc + per-`go` worker + `Arc<AtomicBool>` cancellation.
- **ADR-0012** — Tournament-harness conventions. **Binds on M2.E.**
  - Runner: `fastchess` (Cute Chess 1.4.0 ships zero macOS assets; fastchess ships pre-built `mac-arm64` binaries and is what Stockfish/Fishtest migrated to in 2024).
  - Engine config: `scripts/match.sh` wrapper. fastchess has no `engines.json` registry.
  - Output layout: raw PGN/log → `target/matches/` (gitignored). Milestone summaries → `bench/m2.md` per ADR-0010.
  - Smoke contract: 4 self-play + 4 vs Stockfish at `tc=10+0.1`, all legally terminated, no protocol errors, fastchess UCI-compliance checker silent.
  - SPRT (per-change strength gating): deferred to M3.
  - Research: `research/m2-tournament-harness.md`.

**Sub-phases.** M2 is decomposed into five plan-and-execute cycles. Each phase gets its own plan-mode pass with the self-review loop (see `workflow.md`), executes independently, lands its own commit(s), runs its own tests before we move to the next phase.

| Phase | Scope | Approx size |
|---|---|---|
| **M2.A** ✓ — UCI move encoding | `Move::to_uci` (Display delegate) + `Move::from_uci` (generate-and-match against `generate_moves`) + `UciMoveError` (4 variants). Long algebraic per UCI spec; null move `0000` rejected; strict lowercase input. Defers legality entirely to movegen (consistent with ADR-0007). Debug-only `debug_assert!` on `(from, to, promo)` uniqueness inside `from_uci` (defense-in-depth alongside property test) | ~400–600 lines (actual: ~860 lines incl. 38 new tests) |
| **M2.B** ✓ — UCI command parser | `uci` module: `Command` enum + helper types + `parse_uci_line(&str) -> Command`. Per-command leniency rules grounded in 12 empirical Stockfish 18 probes (strict-first-token, `debug` strict-exact-2-tokens, `position` lenient-stop, `go` lenient-skip). Move strings + FEN strings collected raw — M2.C parses. Total function: no `Result`, no panics. 96 tests; 99.9% line coverage; 0 missed mutants on `--in-diff` (49 caught, 6 unviable). | ~600–900 lines (actual: ~1430 lines incl. ~700 lines of impl + ~700 lines of 96 tests) |
| **M2.C** ✓ — Engine I/O loop + Search trait | `Engine<W, S>` orchestrator + reader thread + per-`go` worker + `Arc<AtomicBool>` cancellation per ADR-0011. Handlers for every UCI 2006 GUI→engine command. `position` reset-on-error (FEN err keeps prior; move err resets to base; both info-string-rejected unconditionally). `searchmoves` silently drops bad entries. `Search` trait + `SearchContext` + `Stub` impl (lex-first legal move; honors `infinite`/`movetime`/`ponder`). `handle_quit` joins worker (bounded by polling cadence) so `bestmove` is in stdout before `run` returns. Always-compute-candidate-before-cancellation eliminates quit-vs-go race. 7 new tests added in final-review pass closed all 3 missed mutants. | ~800–1200 lines (actual: ~1300 lines impl + tests + ~330 lines plan + ~140 lines ADR-0011) |
| **M2.D** ✓ — Random search + `Random_Seed` option | `RandomMover` SplitMix64-seeded `Search` impl replaces `Stub` (lex-first). `Search` trait extended with default-no-op `set_seed` / `reset` lifecycle hooks. `Random_Seed` UCI option (`type spin default 0 min 0 max 2147483647`); strict validation rejects values above declared `max`. PRNG state continuous across `go`; `setoption` and `ucinewgame` both reset state. `Engine::join_in_flight_worker` consolidates the stop+join idiom — also called by `handle_ucinewgame` and `handle_setoption` to close a deadlock under in-flight `go infinite`. End-to-end self-play test E36 drives the binary through a full game with seed 8 → terminates ply 106 → `bestmove d3d1`. 23 mutants / 23 caught; 100% coverage on production `splitmix64_next` + `RandomMover`. | ~700–1000 lines (actual: ~1700 lines impl + tests + ~700-line plan; +24 net new tests) |
| **M2.E** ✓ — Tournament harness + fastchess | `scripts/install-fastchess.sh` SHA256-pinned installer + `scripts/match.sh` three-subcommand wrapper (`compliance`/`self-play`/`vs-stockfish`). ADR-0012 codifies layout; `docs/workflow.md` "Running a match" runbook. `RandomMover::go` info-line emission (`info depth 0 score cp 0 nodes 1 time <ms> pv <move>`) added to close `--compliance` Step 12. E37 (`integration_unknown_command_silently_ignored`) + E33 amendment in `tests/uci_integration.rs` reusing existing helpers — no new test file. Self-play (2 games) + vs-Stockfish (2 games) + compliance all pass C1–C7 with C3=C4=0; bench/m2.md captured. Closes M2: complete game played through fastchess against itself and Stockfish without protocol errors. | ~400–700 lines (actual: ~510 lines incl. ~140-line ADR-0012, ~100-line bench/m2.md, ~110-line match.sh, ~80-line install script, ~50-line E37 + E33 amend, ~20-line workflow runbook section, plus the ~6-LOC src/search.rs info-line emission) |

Phases A → D are foundational and largely sequential; A and B are independent of each other and could be planned in either order, but C depends on both. E is the validation layer (analogous to M1.G).

### M3 — Alpha-beta + material eval
First playing engine. Negamax with iterative deepening, quiescence search, simple material + piece-square table eval. No transposition table yet.

**Exit criteria:** beats the random mover ~100% via SPRT; estimated rating from self-play and a known-strength reference.

**Status — prior-art research complete (2026-04-28).** Three research reports in `docs/research/`: `m3-search-basics.md` (+ `m3-search-basics.opus.md` calibration parallel pass), `m3-eval-material-pst.md`, `m3-time-management.md`. Synthesis recorded in `docs/prior-art.md`. chess-researcher Sonnet tier confirmed via Sonnet+Opus calibration on the search-basics brief.

**ADRs likely to bind per-phase, as each lands.** Material in `docs/research/` and `docs/prior-art.md`.

- **ADR-0013** — Search structure: fail-soft negamax + triangular PV + ply-adjusted mate scores + mate-distance pruning. **Binds on M3.C.**
- **ADR-0014** ✓ — Eval composition: material + PST single-phase, vendored PeSTO MG values, incremental delta in `Undo`. **Landed with M3.A.**
- **ADR-0015** — Time management: `compute_caps` formula + soft/hard cap discipline + `MoveOverhead` UCI option. **Binds on M3.E.**

(Numbers tentative for M3.C and M3.E; ADRs land just before the phase that depends on them.)

**Sub-phases.** M3 is decomposed into six plan-and-execute cycles. Each phase gets its own plan-mode pass with the self-review loop (see `workflow.md`), executes independently, lands its own commit(s), runs its own tests before we move to the next phase. Sized for ~300–800 LOC each per the workflow's typical-unit target.

| Phase | Scope | Approx size |
|---|---|---|
| **M3.A** ✓ — Material + PST eval + `GreedyMover` (depth-1 production search) | `eval` module + `src/eval/data.rs` (vendored PeSTO MG values + six PST arrays; file-level cargo-mutants exclusion mirrors `src/magic/constants.rs`). `evaluate(&Position) -> i32` side-to-move-relative; insufficient-material override (KvK / KvN / KvB → 0). `Position::static_eval_white: i32` field maintained incrementally by `make_move` / `unmake_move` per ADR-0014; `Undo::prior_static_eval` slot; debug round-trip assert + release perf sentinel `make_move_no_from_scratch_in_release` (single test covers both zobrist and eval invariants). `GreedyMover` Search impl — depth-1 max-eval with reservoir-sampled tie-break (one SplitMix64 step per tied move). One `pos_clone` mutated in place via make/unmake. `info` line emitted BEFORE wait loop with real `score cp`. `RandomMover` deleted; `splitmix64_next` retained under GreedyMover ownership. `Random_Seed` UCI option preserved. E36 deleted; E38 pinned at seed 0 / ply 116 / `f2f3`. ADR-0014 codifies the design. 0 missed mutants on the M3.A diff after PST-data file split (82 in scope, 71 caught + 1 timeout + 10 unviable). | ~1200 (~700 plan + ADR + research; ~700 impl + tests; final unit including the data-file split + tests stub round-trip) |
| **M3.B** — Game-history + draw-detection plumbing | `Engine::game_history: Vec<u64>` is the full Zobrist trajectory **including the current position**; invariant `history.last() == pos.zobrist()` holds at all times. `Engine::new()` initializes `vec![pos.zobrist()]`; `handle_position` clears then re-pushes the spec base's Zobrist before walking the moves clause (push-after-make for each successful move); `handle_ucinewgame` resets pos to startpos AND resets history to `[startpos_zobrist]` to maintain the invariant. Error-path discipline: FEN parse error keeps prior position and prior history; move-clause error resets position to spec base AND resets history to `[spec_base_zobrist]`. `SearchContext::history` is an owned `Vec<u64>` cloned from `Engine::game_history` at `go`-start (mutable for search-stack push/pop, which M3.C wires into negamax). Helper functions in `src/search.rs`: `is_repetition(history: &[u64], halfmove_clock: u8) -> bool` walks `history[..len-1]` backward by 2-ply (only same-side-to-move positions can repeat), capped at `halfmove_clock` plies (irreversible-move stop); first match returns true per CPW single-occurrence-in-search rule. `is_fifty_move_draw(halfmove_clock: u8) -> bool` returns true at `halfmove_clock >= 100` (FIDE 50-move-claimable threshold; 75-move auto-draw at 150 not separately handled — once a draw is claimable, treat as drawn for engine-reasoning purposes). No GreedyMover changes (depth-1 doesn't recurse; no consumer of these helpers until M3.C). No new ADR — pure plumbing for already-decided architectural goals (search owns repetition + 50-move; eval owns insufficient-material per ADR-0014). | ~300 |
| **M3.C** — Negamax alpha-beta core | Fail-soft negamax with `i32` scores. Mate scoring `MATE - ply` / `-(MATE - ply)`; UCI emit `score mate N` with full-moves conversion. Mate-distance pruning. Triangular PV table (~4 KB at MAX_PLY=64). MVV-LVA capture ordering; quiet moves in movegen order. Fixed depth via `go depth N` only (no ID, no time mgmt yet — single iteration). Calls `evaluate` at leaves (horizon effect accepted; qsearch lands in M3.D). Replaces `GreedyMover` as the production `Search` impl. Honors M3.B repetition/50-move helpers (called at ply > 0; root still searches and picks a move). ADR-0013 lands here. | ~700 |
| **M3.D** — Quiescence search | Stand-pat baseline (forbidden when in check). Captures + queen promotions extended at leaves. In-check all-evasions. Terminal detection in qsearch. No delta pruning, no non-capture checks, no underpromotions (M4+ refinements). Replaces M3.C's leaf eval call. | ~400 |
| **M3.E** — Iterative deepening + time management | `compute_caps(GoParams, Color, latency) -> (soft, hard)` pure function with mocked-clock unit tests. Soft cap = `remaining/20 + increment/2`; hard cap = `min(3 × soft, remaining - latency)`. ID outer loop with abort-between-iterations discipline; mid-iteration aborts discard partial result. Prior-iteration root PV move tried first (the only ordering hint available without TT). New `MoveOverhead` UCI option (default 50 ms). ADR-0015 lands here. | ~600 |
| **M3.F** — `bench` command + SPRT validation | UCI `bench` command running a fixed position set and reporting deterministic node count (regression baseline per CPW convention). SPRT match vs RandomMover via the M2.E fastchess harness — expected to cross the upper bound in ~5–10 games. First rating estimate from self-play + Stockfish-with-Elo-cap calibration matches. `bench/m3.md` milestone summary. Closes M3. | ~300 |

A and B are independent and can be planned/executed in parallel. C–F are sequential.

### M4 — Search basics
Transposition table (Zobrist), move ordering (PV move, MVV-LVA, killer moves, history heuristic), aspiration windows.

**Exit criteria:** each addition justified by SPRT win.

### M5 — Search advanced
Null-move pruning, late move reductions, futility pruning, singular extensions. Each gated by SPRT.

### M6 — Eval improvements
Tapered eval, pawn structure, king safety, mobility, passed pawn evaluation. Texel-tuned where possible.

### M7 — Skill dial (basic strength reduction)
Configurable strength reduction: UCI's standard `UCI_LimitStrength` and `UCI_Elo` options, plus a granular "skill level" knob. Mechanisms: depth/node-count caps, eval noise injection, top-N randomized move selection. Each mechanism is a pure function (TDD-able); their composition's actual Elo at each setting is calibrated empirically via self-play matches.

**Exit criteria:** at advertised Elo settings, calibrated self-play matches confirm strength within a stated margin (e.g. ±50 Elo). Engine plays "interestingly bad" at low settings rather than randomly bad.

**Schedulable earlier.** This milestone has no hard dependency past M3 — if the user wants to play against the engine before M7, we can pull a basic version forward. Full calibration only makes sense once eval is reasonably stable (post-M6).

See `decisions/0005-strength-dial-as-planned-milestone.md`.

### M8 — Parallelism
Lazy SMP or equivalent. Lockless TT.

### M9 — NNUE
Train and integrate. Requires a data pipeline and training infrastructure separate from the engine. Replaces classical eval as the primary scoring function. Re-uses make/unmake hooks from `decisions/0004`.

### M10 — Android app
Wrap engine as a mobile app. Toy target — performance regressions vs. macOS expected and acceptable. The skill dial (M7) is the primary mechanism for making the mobile engine a plausible opponent for the user.

### M11 — Tournament play
Enter CCRL Blitz, CEGT, or open TalkChess events to obtain external Elo.

### M12 — Human-like play (optional, post-NNUE)
A separate model (Maia-style — trained to predict human moves at a target rating band) plugged in via the same eval/policy hook used by NNUE. Distinct from M7's "skill dial," which is an engine playing weakly; this is an engine playing *like a human* of a target rating. Open question: whether worth the training infrastructure investment, given M7 may be sufficient for the app use case.

## Long-term strength target

GM-level (~2700+) is the design ceiling. Classical eval (M3–M6) targets the high-amateur to weak-master range; NNUE (M9) is what carries us into GM territory. The skill dial (M7) and optional human-like play (M12) provide the inverse — making the engine a calibrated opponent at lower levels.

## Notes

- Each milestone produces benchmarks recorded somewhere persistent (TBD — likely `bench/` directory with timestamped results).
- Each milestone updates this file with completion notes and rating estimate.
- SPRT-driven changes apply from M3 onward.
