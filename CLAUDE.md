# Chess Engine Project

A Rust chess engine, vibe-coded by chat. Designed to grow incrementally toward GM-level standard chess strength via classical eval first, then NNUE.

Variant chess is **explicitly out of scope** for this project — it will be a future fork.

## Current status

**Phase: M3.A complete; M3.B next — game-history + draw-detection plumbing.** Architectural commitments settled (see `docs/decisions/`).

- M1: complete through M1.G (perft + criterion benchmark harness; 119 Mnps bulk on starting D4).
- M2.A: complete — `Move::to_uci` + `Move::from_uci` + `UciMoveError`. Generate-and-match parsing strategy; null move `0000` rejected as `NullMove`; strict lowercase input. No new ADR.
- M2.B: complete — `uci` module: `Command` enum + `parse_uci_line(&str) -> Command`. Pure string→AST function, no I/O, no engine state. Per-command leniency rules grounded in empirical Stockfish 18 probes. No new ADR.
- M2.C: complete — `engine` + `search` modules: `Engine<W, S>` orchestrator + reader thread + per-`go` worker + `Arc<AtomicBool>` cancellation per **ADR-0011**. `Search` trait + `SearchContext` + `Stub` impl. End-to-end exercisable: `printf 'uci\nquit\n' | target/release/chess` produces `id name chess 0.1.0 / id author Alex Feldgendler / uciok`.
- M2.D: complete — `RandomMover` SplitMix64-seeded random-mover replaces `Stub`. First real UCI option: `Random_Seed` (`type spin default 0 min 0 max 2147483647`). `Search` trait extended with `set_seed` / `reset` lifecycle hooks. New `Engine::join_in_flight_worker` helper unifies the stop+join sequence used by `handle_go`, `handle_ucinewgame`, and `handle_setoption`'s success arm — closes a deadlock under in-flight `go infinite`. End-to-end self-play game terminates legally with the embedded seed (E36). No new ADR.
- M2.E: complete — `scripts/install-fastchess.sh` + `scripts/match.sh` (three subcommands: `compliance` / `self-play` / `vs-stockfish`) + **ADR-0012**. `RandomMover::go` info-line emission added (`info depth 0 score cp 0 nodes 1 time <ms> pv <move>`) to close `--compliance` Step 12. E37 + E33 amendment in `tests/uci_integration.rs`. fastchess `--compliance` 40/40 pass; 4 smoke games (2 self-play + 2 vs-Stockfish) all `[Termination "normal"]`, zero illegal moves, zero stalled connections. M2 exit criterion met.
- M3.A: complete — `eval` module + `GreedyMover` Search impl. PeSTO middlegame material + PST tables vendored verbatim (split into `src/eval/data.rs` for cargo-mutants exclusion). `Position::static_eval_white: i32` field maintained incrementally by `make_move` / `unmake_move` per **ADR-0014**, mirroring the M1.E Zobrist pattern (debug round-trip assert + release perf sentinel). Insufficient-material draw detection (KvK / KvN / KvB → 0) lives in `evaluate`. `RandomMover` deleted; `GreedyMover` becomes production search. `score cp` now real (e.g. `info depth 1 score cp 36 ... pv g1f3` from startpos), not M2.D's placeholder 0. Self-play smoke shifts from random shuffles (343/450 ply) to material-greedy blunders (20/20 ply by resign adjudication). vs-Stockfish-1320: chess loses both at 37 / 41 ply. fastchess `--compliance` 40/40, 0 illegal moves, 0 stalled connections. 615 lib tests, 644 total. mutation 0 missed.

### What M3.A landed

The first `Search` impl that *evaluates* — depth-1 best-eval replaces uniform random as production search:

- **`src/eval.rs`** — `pub fn evaluate(&Position) -> i32` (side-to-move-relative centipawn score). Insufficient-material override returns 0 for KvK / KvN / KvB; otherwise reads `Position::static_eval_white()` and flips for Black. `pub(crate) fn eval_white_from_scratch(&Position) -> i32` for from-scratch recomputation (debug round-trip assert + parse-time initialization).
- **`src/eval/data.rs`** (NEW separate file) — vendored PeSTO middlegame data: `MATERIAL[6] = [82, 337, 365, 477, 1025, 0]` plus six `MG_*_PST: [i32; 64]` arrays. File-level cargo-mutants exclusion mirrors the `src/magic/constants.rs` precedent for vendored constants.
- **`PSQT[2][6][64]`** — compile-time const built via `const fn build_psqt()` in `src/eval.rs`. Indexed by `[color_idx][kind_idx][sq_idx]`. White lookup `+(MATERIAL[kind] + MG_PST[kind][sq ^ 56])`; Black lookup `-(MATERIAL[kind] + MG_PST[kind][sq])`. The `s ^ 56` flip converts our LERF index (a1 = 0) to PeSTO's a8-origin array index. Logic-side mutations on `build_psqt` and `evaluate` are exercised by mutation testing; the data tables are excluded.
- **`Position::static_eval_white: i32` field** — combined material + PST score from White's perspective. Maintained incrementally by `make_move` / `unmake_move`. Initialized via `refresh_static_eval()` (analog of `refresh_zobrist`) in `Position::starting_position()` and after FEN parse. `pub(crate)` accessor; field is private. `Copy + PartialEq + Eq` derives stay valid (i32 is Copy; field is invariant-derived).
- **`Undo::prior_static_eval: i32`** — captured by `make_move` before any mutation; restored by `unmake_move` directly via `refresh_static_eval_from`. `Undo` grows from 16 B → 24 B with alignment.
- **`update_static_eval_after_make` private helper** — order-agnostic (takes `mover: Piece` by value; uses `mover.color`, not `pos.side_to_move()`). Six flag-arm deltas mirror the `update_zobrist_after_make` shape: Quiet/DoublePush, Capture, EnPassant (uses `capture_sq`, not `to`), KingCastle/QueenCastle (king + rook), four `*Promo` (uses `promo_kind`), four `*PromoCapture`.
- **Debug round-trip assert** — both `make_move` and `unmake_move` debug-assert `pos.static_eval_white() == eval::eval_white_from_scratch(pos)` after the delta. Release builds trust the delta; `make_move_no_from_scratch_in_release` perf sentinel covers both invariants (zobrist + eval) at ≤100 ns/cycle.
- **`GreedyMover` in `src/search.rs`** — replaces `RandomMover`. Algorithm per ADR-0014 / plan §10.1: enumerate legal moves, score each via `-evaluate(post_make)` (negate for mover's perspective), pick max via reservoir sampling (one SplitMix64 step per tied move; uniform over the tied set). ONE `pos_clone` mutated in place via make/unmake (single clone, then unmade per iteration; debug-build round-trip asserts run on each step). Honors `infinite` / `movetime` / `ponder` via 1 ms-cadence wait loop. Inherits `set_seed` / `reset` lifecycle from M2.D.
- **`info` line emitted BEFORE wait loop** — at the moment depth-1 search completes, not after cancellation. Real `score cp` (e.g. `info depth 1 score cp 36 nodes 20 time 0 pv g1f3` from startpos) replaces M2.D's placeholder `cp 0`. Empty `searchmoves` filter / mate / stalemate emit `info depth 0 score cp 0 nodes 0 ... pv 0000` and return `bestmove 0000`.
- **`RandomMover` deleted entirely** — struct, impl, M2.D-only tests (D2–D10, D12), `find_seed_pair_for_b2e` / `find_terminating_seed_for_e36` helpers all gone. `splitmix64_next` survives under `GreedyMover` ownership; D1 SplitMix64 reference test + D11 `should_abort` test stay verbatim.
- **`Random_Seed` UCI option preserved** — same min/max/default/case-insensitive parse. Now drives `GreedyMover`'s tie-break PRNG.
- **Engine plumbing** — `run_stdio()` builds `Engine::new(io::stdout(), GreedyMover::new(0))`. Two `engine.rs::tests` (`handle_setoption_random_seed_changes_future_bestmoves_but_not_past_ones` and `..._case_insensitive_and_boundary`) had to be rewritten to use a KvK-with-ties FEN — the original tests asserted "different seeds give different bestmoves from startpos", but PeSTO eval makes `g1f3` the unique best move from startpos. KvK exposes the reservoir-sampling code path (all 8 white-king moves tie at 0).
- **E36 deleted; E38 added** to `tests/uci_integration.rs` — pinned at `SELF_PLAY_SEED = 0`, `EXPECTED_FINAL_PLY = 116`, `EXPECTED_LAST_BESTMOVE = "f2f3"`. GreedyMover terminates faster than RandomMover (active material capture, not random shuffles).
- **`scripts/match.sh` header strings** updated RandomMover → GreedyMover. `-draw` adjudication still deliberately omitted (depth-1 scores too noisy to base draw heuristic on; future M3.C alpha-beta with iterative deepening will revisit).

### M3.A — implementation highlights

- **PSQT layout split decision.** Plan committed to a precomputed `PSQT[color][kind][square]` table built at compile time via `const fn build_psqt()`, eliminating per-eval branches on color and on the LERF↔PeSTO rank-flip. Logic stays in `src/eval.rs`; vendored data lives in `src/eval/data.rs`.
- **Combined material+PST single field, not separate.** Per plan §5.1: one delta-update site, one source of truth, eliminates dual-source-of-truth drift risk. M1.E's Zobrist field is precedent.
- **Combined static-eval as the NNUE hook (ADR-0004 extension).** When NNUE arrives at M9, the PSQT-based incremental update slots out for the accumulator update; signature stays. Exactly the discrete-function shape the make/unmake API was designed for.
- **`evaluate`'s insufficient-material short-circuit** is local — relies on `validate_post_parse` having ensured exactly 2 kings; total_count == 3 then implies exactly one non-king piece, so the `knight_count == 1 || bishop_count == 1` test is sufficient. Same-color KBvKB deferred to M6.
- **Mutation-testing data exclusion.** First plan-review pass surfaced that ~185 `delete -` mutants would survive on PeSTO PST literals (B1 PSQT-symmetry test is structurally invariant under sign flips that affect both color sides). Final-review pass 1 confirmed empirically. Resolution: split the vendored data into `src/eval/data.rs` + file-level `exclude_globs` (matching `src/magic/constants.rs`). Final-review pass 3 verified: 82 mutants in scope (down from 265), 0 missed, 71 caught, 1 timeout (effective catch on `delete ! in GreedyMover::go`'s wait-loop guard), 10 unviable (compile-failure mutants).
- **`info` line emitted BEFORE the wait loop.** M2.D emitted post-wait because the score was always 0. With real depth-1 evaluation, GUIs watching `info score cp` to track engine eval should see it the moment search decides — not after cancellation under `go infinite`.
- **D17/F1 use KvK midboard FEN** (`8/8/8/8/4K3/8/8/4k3 w - - 0 1`) — all 8 white-king moves leave KvK; all 8 candidate scores tie. The first plan draft claimed startpos produces 8+ ties, but PeSTO PST values make startpos moves nearly all-distinct (e2e3 → +18, d2d3 → +13, etc.) — empirically wrong; reservoir sampling wouldn't be exercised. KvK genuinely produces 8 ties.
- **PST anchor tests pin combined values, not bare material.** A8 = `PSQT[White][Pawn][A2] == 47` (= material 82 + MG_PAWN[E2 ^ 56 = 48] = -35); A9 = -47; A10 = `PSQT[White][King][E1] == 8` (king material is 0; PST is the entire term). These three concrete anchors plus B1 (exhaustive `PSQT[White][k][s] == -PSQT[Black][k][s ^ 56]`) plus A1 (startpos symmetric → 0) triangulate eval correctness without needing a `mirror_position` helper.
- **Two engine tests rewritten for KvK.** With PeSTO eval making `g1f3` the unique startpos best, "seed 42 vs seed 0 gives different bestmoves" cannot fire. Switched to KvK on the orchestrator side too — same reservoir-sampling test, exposes the same PRNG-driven divergence under genuine ties.
- **E38 final-ply / last-bestmove** captured during impl by running with placeholder values, reading the assertion failure, committing the captured constants. Same M2.D protocol as `find_terminating_seed_for_e36`.
- **`make_move_no_from_scratch_in_release` perf sentinel** retained (single test); its docstring expanded to call out both invariants (zobrist `from_scratch` ~50 ns/op AND `eval_white_from_scratch` ~30–40 ns/op). Either reintroduction trips the same 100 ns/cycle threshold; no need for two parallel sentinels.
- **`update_static_eval_after_make` uses `mover.color` parameter, not `pos.side_to_move()`.** Order-agnostic at the call boundary — moving the call before `set_aux_state` would silently break a `pos.side_to_move()`-derived implementation. Mirrors `update_zobrist_after_make` discipline.

### M3.A — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 615 lib + 5 uci-integration + 24 other = **644 fast + 4 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo llvm-cov --summary-only --lib` | Total **96.08% region / 96.18% line / 96.35% function**. `eval.rs`: 87.80% region / 84.92% line — gap is `cargo-llvm-cov` not instrumenting the const-fn `build_psqt` body (compile-time evaluation); the resulting PSQT data is verified by anchor tests A8/A9/A10 + symmetry B1. `mov.rs` 96.85%, `search.rs` 98.56%, `position.rs` 98.68% — no real gaps. |
| `cargo mutants --in-diff` | 82 mutants tested (185 PST-data `delete -` mutants excluded via `src/eval/data.rs` file-level rule, mirroring `src/magic/constants.rs` precedent); **71 caught + 1 timeout (effective catch via test hang on `delete ! in GreedyMover::go`) + 10 unviable (compile-failure); 0 missed.** |
| `scripts/match.sh compliance` | 40/40 fastchess `--compliance` steps pass ("Engine passed all compliance checks."). |
| `scripts/match.sh self-play` | 2 games at 20 ply each, both `[Termination "adjudication"]` via `-resign movecount=3 score=600` (depth-1 blunders compound fast). 0 illegal, 0 stalled. |
| `scripts/match.sh vs-stockfish` | 2 games vs Stockfish 18 capped at UCI_Elo=1320: game 1 `[Termination "adjudication"]` 37 ply; game 2 `[Termination "normal"]` 41 ply (sf18 mates). 0 illegal, 0 stalled. |
| Smoke (uci) | `printf 'uci\nposition startpos\ngo movetime 50\nquit\n' \| target/release/chess` produces `info depth 1 score cp 36 nodes 20 time 0 pv g1f3` followed by `bestmove g1f3`. The `cp 36` is the real depth-1 evaluation, not M2.D's placeholder 0. |
| Benchmark | **Skipped** — UCI dispatch is per-line; depth-1 evaluate over 30 legal moves is ~500 ns total. M3.C alpha-beta will be the first phase with a meaningful nps figure. Same precedent as M2.A–M2.E. |

All three review loops converged. Plan archived at `docs/plans/m3.a.md` (3 plan-review passes). ADR-0014 codifies eval composition. Test-suite review converged in 2 passes; final review in 3 (the third pass closed the must-fix on PST-data mutation exclusion via the split-file refactor).

### What M2.E landed

The external validation layer that closes M2 — proves the engine speaks UCI to a real tournament runner, not just to itself:

- **`scripts/install-fastchess.sh`** — idempotent SHA256-pinned installer. Three constants (`EXPECTED_RELEASE_TAG="v1.8.0-alpha"`, `EXPECTED_VERSION_LINE="alpha 1.8.0"`, `EXPECTED_SHA256=5f5a313b…`); platform + curl pre-flights; pre-flight version-line gate short-circuits already-installed runs to a no-op. Bumping the pinned release is a three-line edit.
- **`scripts/match.sh`** — three-subcommand wrapper. Locator: `vendor/fastchess/fastchess` first, `command -v fastchess` PATH fallback; either way the resolved binary is gated against `EXPECTED_VERSION_LINE` (catches stale on-PATH installs). `cargo build --release` runs before each subcommand. Adjudication: `-maxmoves 300 -resign movecount=3 score=600`; `-draw` deliberately omitted (random mover emits no `score cp` so any score-threshold draw heuristic would fire trivially after `movenumber` plies). `ulimit -n 4096 || true` baked in (best-effort) to dodge the macOS-256-fd default.
- **ADR-0012** — codifies fastchess as the runner, `vendor/fastchess/` as the install path, the engine-registry-via-shell-wrapper convention (no `engines.json`), `target/matches/{smoke,sprt}/` for raw output, the M2 smoke contract (2 self-play + 2 vs Stockfish + compliance), the `RandomMover::go` info-line requirement, and the fresh-clone bootstrap sequence.
- **`RandomMover::go` info-line emission** (~6 LOC in `src/search.rs`) — emits `info depth 0 score cp 0 nodes 1 time <ms> pv <move-or-0000>` via `info_sink` after the wait loop. `score cp 0` is **empirically required** by fastchess `--compliance` Step 12 (probed three variants — `info depth N nodes K time M pv MV` without a `score` field fails; with `score cp 0` passes). `pv 0000` placeholder when `candidate.is_none()` (mate / stalemate / empty searchmoves filter).
- **E37 + E33 amendment** in `tests/uci_integration.rs` (no new test file). E37 (`integration_unknown_command_silently_ignored`) pipes `joho garbage\nisready\nquit\n` and `assert_eq!(lines, vec!["readyok".to_string()])` — fully pins the silence-on-unknown contract. E33 amended with `assert!(lines.iter().any(|l| l.starts_with("info depth ")))` to pin the §11 emission. Both reuse existing `spawn_engine` / `drain_stdout` / `collect_lines` / `wait_for_exit` helpers.
- **`docs/workflow.md`** — new "Running a match" section pointing at `scripts/match.sh` subcommands.
- **`bench/m2.md`** — milestone summary per ADR-0010 / ADR-0012. Captures: 2 self-play games (343 / 450 plies; both natural insufficient-material draw), 2 vs-Stockfish games (56 / 25 plies; both natural mate by sf18), `--compliance` 40/40, all C3=C4=C5=0.

### M2.E — implementation highlights

- **The `--compliance` Step 12 failure was the load-bearing discovery.** Plan-review pass-1 reviewer empirically ran `vendor/fastchess/fastchess --compliance target/release/chess` against the M2.D binary — Steps 1–11 pass, Step 12 ("Check if engine prints an info line") fails. Plan §11 added the info-line emission; after Phase 1 lands, all 40 fastchess compliance steps run to completion. Steps 13–40 had been gated behind the failing Step 12.
- **`score cp 0` is required, not optional.** First plan draft committed on "no `score cp`" with rationale "would be a lie." Empirical probe overturned that — Step 12 rejects info lines without a `score` field regardless of other fields. fastchess's `-draw` adjudication defaults missing scores to 0 anyway, so emitting `0` explicitly is the same value, just visible. M3+ alpha-beta will replace `0` with an honest evaluation.
- **`-rounds 1 -repeat` (2 games), not `-rounds 2 -repeat` (4 games).** Plan-review pass-1 SF4: with deterministic seeds and no opening book, `-rounds 2 -repeat` produces 4 PGNs that are 2 identical trajectories duplicated. Cutting to 2 honest games closed the false-coverage claim.
- **Adjudication knobs were unused on the actual runs.** Plan anticipated `-maxmoves 300` would fire on most random-vs-random games (per M2.D's 300-ply E36 cap). Empirically both self-play games naturally reached insufficient material before the cap (343 ply / 450 ply); vs-Stockfish ended in natural mate (56 / 25 ply). `-resign` no-op for these 4 games — kept as a no-cost hedge.
- **No new test file.** Plan first draft proposed `tests/uci_smoke.rs` with five tests; pass-1 must-fix #2 surfaced that S1–S4 duplicated existing E33–E35 with weaker assertions. Resolution: drop the new file; add only E37 (the genuinely novel silent-on-unknown test) to `tests/uci_integration.rs`.
- **Stricter assertions.** Test-suite review pass-1 pushed E37 from `(any readyok) && (no info-string)` to a stricter `assert_eq!(lines, vec!["readyok"])` — fully pins the silence contract; would catch a regression that echoed the garbage line or emitted a stray `option`/`id` line. E33's amended assertion tightened from `starts_with("info ")` to `starts_with("info depth ")` to eliminate the latent ambiguity with `info string`.
- **`ulimit -n` gotcha.** macOS default 256 fd limit was too low even at `-concurrency 1`. Discovered during Phase 5 smoke runs; final-review SF1 pushed the `ulimit -n 4096 || true` (best-effort) into `match.sh` so the operator no longer needs to know about it.

### M2.E — verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| Tests | 591 lib + 5 uci-integration + 24 other = **620 fast + 6 ignored**. All passing. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo llvm-cov --summary-only --lib` | `search.rs`: 78.58% region (pre-existing gap on the two `#[ignore]`-gated documentation helpers + Search trait no-op defaults; the new info-line emission path is fully covered — ~16k hits on emit, 3 hits on the `pv 0000` empty-moves branch). `engine.rs`: 97.41% region. Total: 95.34% region. |
| `cargo mutants --in-diff` | 1 mutant generated; **1 caught; 0 missed; 0 timeouts; 0 unviable**. The single mutant (replace `RandomMover::go` body with `Default::default()`) is caught by E33's `info depth ` assertion + existing M2.D anchors. |
| `scripts/match.sh compliance` | All 40 steps pass per fastchess `--compliance` ("Engine passed all compliance checks."). |
| `scripts/match.sh self-play` | 2 games at 343 / 450 plies; both `[Termination "normal"]` (Draw by insufficient mating material). C3=C4=C5=0. |
| `scripts/match.sh vs-stockfish` | 2 games at 56 / 25 plies; both `[Termination "normal"]` (sf18 mated). C3=C4=C5=0. |
| Smoke | `printf 'uci\nposition startpos\ngo movetime 50\nquit\n' | target/release/chess` produces `info depth 0 score cp 0 nodes 1 time <ms> pv <move>` followed by `bestmove <move>`. |
| Benchmark | **Skipped** — UCI dispatch is per-line; the new info-line emission is one `format!` + one closure call per `go` (~µs). Same precedent as M2.A / M2.B / M2.C / M2.D. |

All review loops converged. Plan archived at `docs/plans/m2.e.md`; ADR-0012 codifies the harness. Plan went through 3 reviewer passes; test-suite review through 2; final review through 2 (the second pass closed a `cargo fmt` violation, the `ulimit` runbook gap, and minor nits).

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

**M3.B — game-history + draw-detection plumbing.** `Engine::game_history: Vec<u64>` populated by `handle_position` (Polyglot Zobrist after every applied move) and cleared by `handle_ucinewgame`. `SearchContext::history` reference plumbed through. Helper functions `is_repetition_in_search` (single prior occurrence in search-stack counts as draw, per CPW) and `is_50_move_draw` (`halfmove_clock >= 100`). Still GreedyMover; no consumer of these helpers until M3.C alpha-beta lands.

After M3.B: M3.C (negamax + alpha-beta core) is the first phase to use the new fastchess SPRT harness for change acceptance.

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
