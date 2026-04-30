# Tooling and QA backlog

Industry-best-practice items surfaced during the 2026-04-27 workflow review but not yet adopted. **Listed in recommended implementation order** — pick from the top when the next slot opens for tooling work.

### ~~Custom in-process Elo-iteration harness~~ — Done (ELOH.B, 2026-04-30)

Landed as `src/bin/elo-iterate.rs` on branch `tooling/elo-harness`.

- **ELOH.A** (harness foundation): persistent subprocesses, UCI driver, native adjudication, per-side clock management, color-paired match loop, PGN/summary output. ~2300 LOC + 139 tests.
- **ELOH.B** (statistical layer): Robbins-Monro K-update at single-game cadence, σ-based stopping, N-parallel-pair concurrency via `std::thread` + `mpsc`, threshold adjudication (resign/draw/max-moves), convergence-progress output. Replaced `scripts/elo-iterate.sh` (now a thin wrapper) and `scripts/sprt.sh rating-estimate` arm (now routes through the binary).

Usage: `cargo run --release --bin elo-iterate -- --engine <path> --opponent <path> --tc <TC> --max-games <N> --initial-elo <E> [...]`. See `--help` for all flags. `scripts/elo-iterate.sh` preserves the prior bash CLI surface as a wrapper.

### ELOH controller boundary testing — mock `EngineHandle` test harness

**Purpose.** Close the integration-only gap left by ELOH.B's mutation-test triage. The synthetic-worker tests in `mod controller::tests` exercise happy-path dispatch + W/L/D aggregation + termination, but precise boundary distinctions in `controller::run_iteration`'s drain-loop dispatch gating (`pairs_dispatched < total_pairs`, `t < args.max_games`, in-flight-pair tracking math) are not covered. Surfaced during ELOH.B's pre-review mutation-survivor analysis (six `< → <=` and `+= → *=` mutants in `run_iteration` deferred via `.cargo/mutants.toml`).

**Scope.** ~150 LOC of test scaffolding: a fake `EngineHandle` that records the sequence of `send_line` calls, a fake `recv_until_bestmove` that emits canned `bestmove` responses driven by a per-game scripted outcome. Wires into the existing controller-test seam (`spawn_workers_with_fn`) so `production_worker_fn` can be unit-tested without real subprocesses.

**Coverage closes:**
- The 6 surviving boundary mutants in `controller::run_iteration` (per `.cargo/mutants.toml` ELOH.B exclusion block).
- Plan §6.6's `controller_setoption_before_ucinewgame_pins_order` test (currently pinned only by inline comment in `production_worker_fn`).
- The `pgn::format_pgn` move-pair-spacing boundary mutants when paired with a parametric move-count fixture sweep.

**When to land.** Earliest is alongside ELOH.C (which adds `--go-nodes` mode and would benefit from the same mock infrastructure for nodes-mode verification). Latest is before any future controller refactor that would invalidate the deferred-detection commitments documented in `.cargo/mutants.toml`.

**Estimated cost vs. payoff.** The current deferral is acceptable because (a) the ELOH.B back-validation gate (Part 1: 120-game online run vs M3.F's ~2114 ± 2σ) catches dispatch-level structural bugs end-to-end; (b) a future controller refactor with real bugs is also catchable by ELOH.C's self-play stress under `--go-nodes`. The mock harness pays back when running mutation tests against the controller surface becomes cheap enough that catching boundary mutants surfaces small bugs early.

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

### ML-tuned aspiration window sizing

**Purpose.** Replace M4.D's fixed `±25 → ±100 → full` widening schedule with a learned policy that predicts the optimal first-try window from cheap features, maximizing expected node savings.

**Motivation.** Aspiration is a bet on score continuity across ID iterations: a successful narrow-window search saves nodes (up to ~2× under perfect ordering), a failed one costs a re-search. The optimal window width depends on position-specific volatility, which is observable from cheap features but not captured by a fixed schedule. Surfaced during the M4.D walkthrough (2026-04-30 chat) as a deferred follow-up if the fixed schedule under-performs or saturates after SPSA tuning.

**Approach.**

1. **Feature set** (all cheap, all available without extra search work):
   - Iteration depth and depth threshold delta.
   - Prior score and prior score delta (`score(d-1) − score(d-2)`); the delta is the single strongest feature.
   - EWMA of past `K` score deltas.
   - Game-tree ply number.
   - Total material on each side (proxy for game phase; once M6's tapered eval lands, use the proper phase tag instead).
   - Branching factor at the root (move-list size).
   - First-move-stability flag from the prior iteration.
   - TT hit rate at root or near-root.
2. **Architecture.** Single hidden layer MLP, ~16-32 units, ReLU. Total params low thousands. Output: continuous window width (regression) or softmax over a bucket grid (`{25, 50, 100, 200, full}`). Inference ~ns; called once per ID iteration.
3. **Training data.** Per-position label generated by running the search with a grid of candidate windows and taking the argmin node count. Corpus: ~100K positions sampled from CCRL games + tactics suites (the latter to cover failure modes that self-play under-represents). Cost estimate: ~50-200 CPU-hours at d≤12.

**Cheap baseline first.** Before training a model, validate that the dominant signal isn't already captured by a one-line heuristic:

```
window = clamp(k * |score(d-1) − score(d-2)|, MIN, MAX)
```

Three SPSA-tunable params. Empirically this baseline captures 60-80% of the ML model's gain in published work on similar problems. If the baseline + SPSA SPRTs neutral or marginal vs M4.D's fixed schedule, the ML approach is unlikely to clear `elo1=5`.

**TC-adaptive (or depth-adaptive) intermediate tier.** Between the cheap delta-baseline and a full MLP, a parameterized adaptive width that scales with search depth or estimated time-per-move is a natural stepping stone (surfaced during the M4.D walkthrough on 2026-04-30 alongside the mixed-TC SPRT discussion). Functional shape: `half_width(depth) = base · max(min_factor, 1 - α·(depth - threshold))` — two parameters (`base`, `α`) plus the existing `threshold`. Depth-adaptive is cleaner than time-adaptive (depth is a discrete observable in the ID outer loop; no coupling to `compute_caps` time-management state). Validation: mixed-TC SPRT against M4.D's fixed-width baseline (the same campaign methodology M4.D establishes), with adaptive variants entering the per-TC regression curve to surface where adaptation helps. Expected gain: small (+2-4 Elo over fixed-width) but cheap to implement (~30 LOC + parameter tune); validates whether the depth/TC interaction motif is real before investing in delta-EWMA or MLP variants.

**Why not now.**

- M4.D's gate is the fixed schedule passing mixed-TC SPRT; building ML training infra is a multi-week scope expansion to a phase that should land in days.
- Prerequisites missing: tapered eval for proper phase signal (M6), SPSA / CLOP harness for the cheap baseline, position corpus, offline label-generation pipeline.
- Elo headroom small at this layer: ~+3-5 Elo realistic ceiling for ML over hand-tuned, vs +50-80 Elo for NMP, +80-100 Elo for LMR, +400+ Elo for NNUE. Opportunity cost is poor.
- NNUE (M9) re-trains the eval from scratch; an ML aspiration model trained on PeSTO eval would need re-training post-NNUE.

**When to land.** Four-tier escalation, each tier proceeding only if the previous shows ≥ +5 Elo of remaining headroom worth chasing:

1. **Fixed schedule (M4.D)** — ±50 default, width-tuned via mixed-TC SPRT. Ships at M4.D close.
2. **TC- or depth-adaptive parametric (post-M5, pre-M9)** — `base · max(min_factor, 1 - α·(depth - threshold))`. SPSA-tuned. Cheapest validation of the depth/TC interaction motif.
3. **Cheap delta-baseline (post-M9 or earlier if (2) saturates)** — `window = clamp(k · |score(d-1) − score(d-2)|, MIN, MAX)`. Three SPSA-tunable params.
4. **MLP (post-M9, only if (2)+(3) saturate)** — full feature set + hidden layer.

**Estimated size.** Adaptive parametric: ~30-50 LOC + SPSA harness reuse. Cheap delta-baseline: ~50 LOC + SPSA harness reuse. ML model: ~500-800 LOC (feature extraction + inference + offline-training pipeline as a separate Python or Rust binary), plus the corpus + label-generation infrastructure (potentially shared with Texel tuning or NNUE data prep).

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
