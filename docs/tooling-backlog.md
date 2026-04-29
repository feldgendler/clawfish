# Tooling and QA backlog

Industry-best-practice items surfaced during the 2026-04-27 workflow review but not yet adopted. **Listed in recommended implementation order** — pick from the top when the next slot opens for tooling work.

### Custom in-process Elo-iteration harness (replaces fastchess for rating estimation)

**Purpose.** Replace `fastchess` for the rating-estimation use case with a custom in-process Rust harness. Surfaced during the M3.F rating estimate when the bash + fastchess iteration scheme proved acceptable but suboptimal: per-batch fastchess spawn + UCI handshake costs ~1-2 s overhead, and the bash + awk PGN-parsing chain adds further latency. A custom harness eliminates both.

**Use case.** Online Elo iteration against a calibrated opponent (typically Stockfish at variable UCI_Elo): play a small batch (one color-pair or more) at the current Elo hypothesis, parse results, update via Robbins-Monro `K_t = K_0 / (1 + t/τ)` schedule, re-configure the opponent, repeat. Currently implemented in bash at `scripts/elo-iterate.sh`.

**Requirements.**

- Spawn clawfish once per run via `std::process::Command` with stdin/stdout pipes; keep the process alive across all batches. Same for Stockfish (or equivalent calibrated opponent).
- Drive the UCI protocol from the harness side: send `uci`, `isready`, `setoption`, `position`, `go wtime/btime`, parse `bestmove`. Reuse clawfish's own `parse_uci_line` is fine for *parsing* opponent output, but the harness drives the FROM-engine direction with hand-built command strings.
- Reuse `Position`, `make_move`, `unmake_move`, `generate_moves`, `in_check`, `is_repetition`, `is_fifty_move_draw` for the harness-side board state — these already exist in the crate.
- Implement game-end adjudication: mate (no legal moves + check), stalemate (no legal moves + not in check), 50-move rule, 3-fold repetition, insufficient-material, plus optional resign-threshold and draw-threshold adjudication matching `scripts/match.sh`'s `-resign movecount=3 score=600 -draw movenumber=34 movecount=8 score=20 -maxmoves 200`.
- Implement clock management per side: `wtime/btime/winc/binc` per UCI; track elapsed time per move; declare time-forfeit on overflow.
- Re-configure the opponent's UCI options between games via `setoption name UCI_Elo value <new>` — single UCI command, no process restart.
- Concurrency: support N parallel games via Tokio or `std::thread`; each game owns its own pair of engine processes. Match `fastchess`'s `-concurrency` semantics.
- Per-game color-swap (a la fastchess `-repeat`); per-batch K-update against averaged score; final estimate at run end.
- Output: per-batch summary line (compatible with `scripts/elo-iterate.sh`'s current format) plus a per-game PGN file for archival.

**Advantages over the fastchess + bash approach.**

1. **Zero per-batch spawn overhead** — engine processes persist across the whole run. ~20% wallclock reduction at our current 10+0.1 TC; larger relative win at faster TCs.
2. **Single-game updates** practical — without spawn overhead, updating after every individual game (instead of color-pairs) becomes feasible. Finer-grained Robbins-Monro schedule, faster convergence per game.
3. **Adaptive K** based on running variance, not just the `1 + t/τ` schedule. The harness has the full estimator state in memory; can switch K dynamically.
4. **Stopping criteria** — terminate when last 20 estimates have σ < threshold. Currently the bash version runs a fixed batch count.
5. **Asymmetric time controls** trivially expressed: clawfish at 10+0.1, Stockfish at "effectively unlimited" (3600+0). Fastchess accepts asymmetric `tc=` per `-engine` block but the bash wrapper has to thread it explicitly.
6. **Better instrumentation** — the harness can record per-game: clawfish's reported depth, score, time used, plus Stockfish's reported depth (when available). Surfacing this enables per-anchor diagnostics ("does clawfish reach depth 8 within budget at all anchors?") that fastchess's PGN format doesn't expose.
7. **Reuses crate code** — `Position`, movegen, draw helpers all already exist and are exhaustively tested. The harness inherits the project's correctness guarantees on the board-side.

**Estimated size.** ~250-350 LOC of Rust at `src/bin/elo-iterate.rs` (or as a separate sub-crate if test isolation is preferred). Plus ~100 LOC of unit tests covering the adjudication logic and the K-update math.

**When to land.** Surfaces as worthwhile when M4+ runs more rating-estimation matches (each new milestone's baseline-tag SPRT closes with a rating-estimate run; the cumulative wallclock saved by a custom harness pays back the dev cost over ~10 runs). Reasonable to land at M4.A close, alongside the first TT-vs-no-TT rating delta measurement.

### Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension

**Purpose.** Decouple SPRT and rating-estimation results from hardware thermal state, background load, and scheduler decisions. Surfaced during M3.F when the user observed: "Clawfish might take more wall clock time to do the same amount of work e.g. if the CPU is hot, but its strength will be unaffected." Wallclock-based TC (the standard) couples results to whatever the OS happens to be doing; that's noise we shouldn't have to fight.

**Why not `go nodes <N>`** — both clawfish and Stockfish support it, and it's hardware-invariant. But `go nodes` ties the budget to the engine's *internal* node count, which is engine-version-coupled: a future change like a smarter-but-slower eval, or more aggressive pruning per node, shifts what "N nodes" means even at fixed hardware. Different engine versions at the same N nodes aren't directly comparable. So `go nodes` is suitable for *one specific engine version vs Stockfish at fixed UCI_Elo* (a single rating snapshot), but not for cross-version SPRT (the project's primary use case from M4 onward). Skip it.

**The right metric is CPU cycle / instruction count** — invariant to hardware (cores, clocks, thermal state), invariant to OS scheduling, AND meaningful across engine versions because each version got the same compute resource. CPU-time (`CLOCK_THREAD_CPUTIME_ID`) is a partial fix — it removes preemption and background-load noise but still varies with core type (P vs E core at different clock rates), in-core frequency scaling (boost vs throttled), and memory-bandwidth-induced stalls. Cycle / instruction counts via PMU are the rigorous answer.

**`VirtualClock` UCI extension** — a clawfish-private option that replaces wallclock with a hardware-invariant work metric inside `compute_caps`. Two implementation tiers:

**Tier B — preferred: PMU-based cycle / retired-instruction count.** Counting retired instructions gives a perfectly reproducible "work" metric. Cycles are noisier (vary with frequency scaling) but acceptable on hardware without aggressive DVFS. Per-thread instruction count solves all three residual variances that thread-CPU-time leaves: core-type asymmetry (work-per-cycle is roughly stable across cores at the same ISA; instruction count is even more invariant), frequency scaling, and memory-bandwidth stalls (a stall doesn't retire instructions, so it doesn't consume budget).

- macOS: `mach/processor_info.h` PMU access; possibly via `kperf` framework. Apple Silicon supports per-thread instruction-count via `kpc_get_thread_counters` (semi-private API); modern macOS may require entitlements for unprivileged access. Apple's `Instruments.app` uses these counters extensively.
- Linux: `perf_event_open(PERF_COUNT_HW_INSTRUCTIONS)` with `attr.inherit = 1` for thread-scoped counting; available unprivileged with `kernel.perf_event_paranoid <= 2` (default on most distros).
- Implementation surface: ~150 LOC platform-specific code under `src/search/cycles.rs` with `cfg(target_os)` arms, plus the same UCI option plumbing as Tier A.
- Test: deterministic instruction-count fixture — same negamax invocation should produce identical instruction count across two runs on the same binary.

**Tier A — fallback: thread CPU time.** Substitute `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (POSIX) / `mach_thread_info(THREAD_BASIC_INFO)` (macOS) for `Instant::now()` in `compute_caps` and the deadline checks. Faster to ship (~50 LOC); removes the dominant noise (preemption + background load) but leaves core-type and frequency-scaling variance. Useful as a stepping stone or as the option enabled by default if PMU access proves restricted on the dev box.

**Single UCI option** `option name VirtualClock type combo default off var off var cputime var cycles var instructions`. Default `off` = standard `Instant::now()` wallclock. Options surface both tiers; `cputime` is Tier A; `cycles` / `instructions` are Tier B variants.

- **Limitation: only works for clawfish-vs-clawfish (internal SPRT).** Stock Stockfish doesn't support this option; for external matches, fall back to wallclock TC and accept the variance.
- The custom in-process Elo-iteration harness above must propagate `setoption name VirtualClock value <variant>` to clawfish at session start.

**Synthesis: core-type invariance.** With hardware-invariant TC (`go nodes` or `VirtualClock`), core speed only affects the test's wallclock duration, never the Elo comparison. Fast cores → test finishes sooner. Slow cores → identical Elo measurement, just takes longer. The entire QoS-pinning / P-vs-E-core scheduling question that motivates the M3.F harness setup disappears: any thread can run anywhere on any core, and the rating result is reproducible to within sample noise. This is the load-bearing reason the two mechanisms together are *more* than just "less wallclock variance" — they make the measurement environment-independent.

**When to land.** The custom harness (above) is the prerequisite — both mechanisms layer cleanly into it. `go nodes` mode is the cheap follow-up (~50 LOC of harness logic). The `VirtualClock` extension is a separate clawfish change that lands when M4+ SPRT reveals wallclock variance is the dominant noise in marginal-Elo signals (typically `elo1=3` or `elo0=-3` matches at fast TC, where ±10 Elo of wallclock noise can flip the SPRT verdict).

---

*Active queue ends here. New items append above this line.*

---

## Deferred — gated on a specific later trigger, not on prioritization

- **SPRT infrastructure** (`cutechess-cli` / `fastchess`) — premature until M3; nothing has strength to test yet.
- **`unsafe` audit policy** — defer until the engine first uses `unsafe` (likely a hot-path `get_unchecked` in magic-bitboard lookups, possibly M1.C or later). At that point write an ADR for when `unsafe` is allowed and how it's reviewed.

---

## Done since the 2026-04-27 review

- **Doc-coverage lint (`#![deny(missing_docs)]`)** — completed 2026-04-28. Added at `src/lib.rs` crate root; ~235 doc comments backfilled across the public surface (`bitboard`, `engine`, `eval`, `fen`, `magic`, `mov`, `movegen`, `perft`, `piece`, `position`, `search`, `slow_attacks`, `square`, `uci`, `zobrist`). Module-level `//!` headers added where missing (`bitboard.rs`, `square.rs`). Originally deferred as "low ROI for an engine the user isn't reading" — revisited once the project went public on GitHub and external readers became plausible. Five chess-coder agents in parallel did the bulk of the additions; final-reviewer caught one must-fix (`Position::ep_target` doc contradicted phantom-EP sanitization) plus three should-fix and several nits, all addressed before commit.
- **LICENSE files** — completed 2026-04-28 (commit `3c6fd8e`). Dual MIT + Apache-2.0 (`LICENSE-MIT` + `LICENSE-APACHE` at repo root) — standard Rust ecosystem convention. `README.md`'s License section points to both. `Cargo.toml` is still `publish = false`.
- **CI (GitHub Actions)** — completed 2026-04-28. `.github/workflows/ci.yml` runs on push to `main`, on PRs, and on manual dispatch. Jobs: `test` matrix on `macos-14` (primary, Apple Silicon) + `ubuntu-latest` (portability) running `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets` + `cargo test --doc`; `fuzz-check` on the separate fuzz workspace; `cargo deny` via the EmbarkStudios action (covers RustSec advisories + license + source-allowlist policy from `deny.toml` — `cargo audit` skipped as redundant); `coverage` via `cargo-llvm-cov --summary-only` (informational; no enforced threshold). All jobs cached via `Swatinem/rust-cache@v2`. Closes backlog item #1.
- **Fuzzing (`cargo-fuzz`)** — completed 2026-04-28 per ADR-0013 (`docs/decisions/0013-fuzzing-strategy.md`). `fuzz/` workspace (independent — `libfuzzer-sys` stays out of root `cargo audit` / `cargo deny` scope). Two `Arbitrary<String>` harnesses: `fuzz_fen` and `fuzz_uci`. Hand-curated seed corpora (35 fen, 30 uci) under `fuzz/corpus/<target>/`. `--sanitizer none` (no fuzzed parser path reaches `unsafe`). Cadence + commands documented under `docs/workflow.md` "Fuzzing". The first 2-hour campaigns per target are deferred to a later session — operator runs `cd fuzz && cargo fuzz run --sanitizer none <target> -- -max_len=200 -max_total_time=7200` once nightly is installed; campaign-result tables go to `docs/research/tooling-fuzzing-results.md`.

## Done in the 2026-04-27 review

- **Benchmark baseline format** — ratified at M1.G's plan-mode pass; ADR-0010 (`docs/decisions/0010-benchmark-baseline-format.md`) committed alongside the M1.G `bench/m1.g.md` first baseline. `criterion 0.7` + `--save-baseline` on per-machine `target/criterion/` (gitignored) + committed human-readable table.
- `cargo fmt --check` enforcement (commits `5ca6c86` style + `eaf9d37` workflow).
- Pre-commit hook at `.claude/hooks/pre-commit-check.sh`, wired via `.claude/settings.json`.
- `cargo audit` + `cargo deny` with policy in `deny.toml`; `Cargo.toml` marked `publish = false`.
- Documentation in `docs/workflow.md` under "Static analysis and dependency hygiene".
- **Mutation testing (`cargo-mutants`)** — backfilled across M1.A + M1.B + M1.C. Configuration in `.cargo/mutants.toml` (with `exclude_re` rules documenting the equivalent mutants). Integrated into the final-review loop per `docs/workflow.md`. Baseline run against committed code: 333 caught + 7 timeout (caught) + 47 unviable + 0 unaddressed survivors, after adding seven targeted tests (idempotency for `Bitboard::with` / `CastlingRights::with`, `Square` Debug-format, `Position::debug_assert_consistent` panic-on-broken-state, `slow_attacks::ray_attack` per-axis step pinning) and excluding eleven equivalent-mutant patterns.
- **Property-based testing (`proptest`)** — `proptest = "1.11"` wired as a dev-dependency. Eight property tests backfill M1.A/M1.B primitives: `Square` index/file/rank round-trip + out-of-range rejection (`src/square.rs`), `Bitboard` set algebra (commutativity, associativity, identity, idempotence, De Morgan, absorption) + membership/`pop_lsb`-ordering invariants (`src/bitboard.rs`), `CastlingRights` bit packing + multi-flag `has` semantics (`src/position.rs`), and `Position` ↔ FEN round-trip across randomly generated valid positions (`src/fen.rs`). Existing anchor unit tests are kept (proptest samples rather than enumerates; anchors document specific mutants killed). Test-suite review loop converged after one revision (stronger idempotence form on `with`/`without`, LSB-ascending order pin on `pop_lsb`, positive-only scope made explicit on the FEN generator).
